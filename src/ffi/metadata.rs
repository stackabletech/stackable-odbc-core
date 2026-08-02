//! FFI implementations for ODBC metadata functions:
//! SQLTablesW, SQLColumnsW, SQLStatisticsW, SQLSpecialColumnsW,
//! SQLPrimaryKeysW, SQLForeignKeysW, SQLDescribeColW, SQLColAttributeW,
//! SQLProceduresW, SQLProcedureColumnsW, SQLColumnPrivilegesW,
//! SQLTablePrivilegesW.

use std::borrow::Cow;
use std::ffi::c_void;

use crate::backend::{Backend, StatementBackend};
use crate::cancel::reclassify_cancelled_opt;
use crate::errors::{IntoOdbc, OdbcError};
use crate::handles::{StatementData, StatementHandle};
use crate::panic::panic_safe;
use crate::synthetic::SyntheticStatement;
use crate::types::col_attr::{ColAttrValue, get_column_attribute};
use crate::types::{
    CatalogResultColumnWidths, ColumnDescriptor, ColumnPrivilegeRow, ColumnRow, ColumnValue,
    ColumnsResultCol, Desc, ForeignKeyRow, ForeignKeysResultCol, Nullable, PRIVILEGE_LEN,
    PrimaryKeyRow, PrimaryKeysResultCol, ProcedureColumnRow, ProcedureRow, SQL_ALL_CATALOGS,
    SQL_ALL_SCHEMAS, SQL_ALL_TABLE_TYPES, SQL_FALSE, SQL_INDEX_UNIQUE, SQL_TRUE, SpecialColumnRow,
    SqlReturn, SqlState, StatementAttribute, StatisticsRow, TablePrivilegeRow, TableRow,
    TablesResultCol, ULen, YES_NO_LEN, character, identifier, identifier_type_from_raw, integer,
    nullable_from_raw, scope_from_raw, smallint, special_columns_columns, statistics_columns,
};
use crate::utf16::write_utf16;

/// Parse a UTF-16 filter parameter. Returns `None` if the pointer is null,
/// otherwise parses the string and returns `Some`.
///
/// `arg` is the ODBC spec's own name for the argument (`"CatalogName"`,
/// `"PKTableName"`, …), and reaches the application in the `HY090` a
/// terminator-less `SQL_NTS` argument produces. `SQLForeignKeys` takes six of
/// these and `SQLColumns` four, so "a string argument was too long" would name
/// none of them.
///
/// # Safety
///
/// `ptr` must be valid for `len` u16 elements (or null-terminated if len is SQL_NTS).
unsafe fn parse_filter_param(
    ptr: *const u16,
    len: i16,
    arg: &str,
) -> Result<Option<String>, OdbcError> {
    if ptr.is_null() {
        return Ok(None);
    }
    // SAFETY: ptr is non-null (checked above) and points to a valid sequence of `len` UTF-16
    // code units, or is null-terminated if len == SQL_NTS; caller upholds this invariant per
    // the function's own safety contract.
    let s = unsafe { crate::utf16::utf16_to_string_named(ptr, len.into(), arg)? };
    Ok(Some(s))
}

/// Whether `SQL_ATTR_METADATA_ID` is `SQL_TRUE` on this statement.
///
/// The spec's initial value is `SQL_FALSE`, which is what an absent entry
/// means; `SQLSetStmtAttr` stores the raw value under the attribute's own
/// identifier (see `ffi::stmt_attr`).
fn metadata_id_enabled<B: Backend>(stmt: &StatementHandle<B>) -> bool {
    stmt.attrs
        .get(&(StatementAttribute::MetadataId as i32))
        .copied()
        .unwrap_or(SQL_FALSE as usize)
        == SQL_TRUE as usize
}

/// The one `HY009` clause every catalog function's diagnostics table states
/// **without** a `(DM)` marker, and therefore the driver's to return:
/// `SQL_ATTR_METADATA_ID` is `SQL_TRUE`, the catalog argument is a null
/// pointer, and `SQL_CATALOG_NAME` reports that catalog names are supported.
///
/// All three conjuncts matter. A data source with no catalogs has nothing for
/// a catalog identifier to name, so a null pointer is the only sensible thing
/// an application can pass there — which is why the check is conditional on
/// [`Backend::supports_catalogs`], the same fact core already answers
/// `SQL_CATALOG_NAME` from.
///
/// The neighbouring `SchemaName`/`TableName` clauses of the same `HY009` row
/// *are* `(DM)`-marked, and are deliberately not checked here.
fn check_metadata_id_null_catalog<B: Backend>(
    connection: &B::Connection,
    catalog_name: *const u16,
    metadata_id: bool,
    function: &str,
) -> Result<(), OdbcError> {
    if metadata_id && catalog_name.is_null() && B::supports_catalogs(connection) {
        return Err(OdbcError::general(
            format!(
                "{function}: CatalogName must not be a null pointer when \
                 SQL_ATTR_METADATA_ID is SQL_TRUE and catalog names are supported"
            ),
            SqlState::invalid_use_of_null_pointer(),
        ));
    }
    Ok(())
}

/// Spec `HY009`, "The TableName argument was a null pointer" — for
/// `SQLStatistics`, `SQLSpecialColumns` and `SQLColumnPrivileges` **only**.
///
/// Those three are the only catalog functions whose diagnostics table states
/// that sentence without a `(DM)` marker. `SQLPrimaryKeys`, `SQLForeignKeys`
/// and `SQLTablePrivileges` carry the same sentence *with* the marker, so the
/// Driver Manager returns it and the driver must not — do not extend this to
/// them. `SQLProcedures` and `SQLProcedureColumns` state no such sentence about
/// their `ProcName` at all.
fn check_null_table_name(table_name: *const u16, function: &str) -> Result<(), OdbcError> {
    if table_name.is_null() {
        return Err(OdbcError::general(
            format!("{function}: TableName must not be a null pointer"),
            SqlState::invalid_use_of_null_pointer(),
        ));
    }
    Ok(())
}

/// Apply `SQL_ATTR_METADATA_ID` normalisation to one identifier-valued catalog
/// argument.
///
/// Returns the value unchanged when `METADATA_ID` is `SQL_FALSE`: the spec then
/// classifies these as ordinary or pattern-value arguments, which are passed
/// through literally. Under `SQL_TRUE` they become identifiers, and core turns
/// each into a pattern matching exactly the one name it denotes — so a backend
/// needs no code for the feature at all.
///
/// Call it on exactly the arguments the spec's "Arguments in Catalog Functions"
/// table classifies as `ID` under `SQL_TRUE`. `SQLTables`' `TableType` is not
/// one of them: it is a value list under both settings.
fn normalise_catalog_arg<B: Backend>(
    connection: &B::Connection,
    value: Option<String>,
    metadata_id: bool,
) -> Option<String> {
    if !metadata_id {
        return value;
    }
    // `escape_dialect` returns an owned `EscapeDialect`, so it is bound before
    // its quotes are borrowed — borrowing from the temporary would not compile.
    let dialect = B::escape_dialect(connection);
    let escape = B::search_pattern_escape(connection);
    value.map(|v| {
        crate::catalog_ident::normalise_identifier(
            &v,
            B::identifier_case(connection),
            dialect.identifier_quotes(),
            &escape,
        )
    })
}

/// `SQLTables`' spec sort order — "ordered by TABLE_TYPE, TABLE_CAT,
/// TABLE_SCHEM, and TABLE_NAME", as zero-based column indices.
const TABLES_SORT_KEYS: [usize; 4] = [3, 0, 1, 2];

/// Which `SQLTables` enumeration an argument combination selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TableEnumeration {
    /// `SQL_ALL_CATALOGS` — the list of catalogs.
    Catalogs,
    /// `SQL_ALL_SCHEMAS` — the list of schemas.
    Schemas,
    /// `SQL_ALL_TABLE_TYPES` — the list of table types.
    TableTypes,
}

/// Classify `SQLTables`' four arguments as one of the three enumerations, or
/// as an ordinary query.
///
/// All three `SQL_ALL_*` sentinels are the same string, `"%"`, so an
/// enumeration is identified by which argument carries it **while the others
/// are empty strings** — never by the `"%"` on its own. `SQLTables("%", "%",
/// "%")` is an ordinary match-everything query, and a detector keyed on `"%"`
/// alone would answer it with a catalog list.
///
/// A null argument (`None`) is not an empty string and does not satisfy the
/// trigger: the spec spells the other arguments out as empty strings.
fn table_enumeration(
    catalog: &Option<String>,
    schema: &Option<String>,
    table: &Option<String>,
    table_type: &Option<String>,
) -> Option<TableEnumeration> {
    let is_empty = |arg: &Option<String>| arg.as_deref() == Some("");
    if catalog.as_deref() == Some(SQL_ALL_CATALOGS) && is_empty(schema) && is_empty(table) {
        Some(TableEnumeration::Catalogs)
    } else if schema.as_deref() == Some(SQL_ALL_SCHEMAS) && is_empty(catalog) && is_empty(table) {
        Some(TableEnumeration::Schemas)
    } else if table_type.as_deref() == Some(SQL_ALL_TABLE_TYPES)
        && is_empty(catalog)
        && is_empty(schema)
        && is_empty(table)
    {
        Some(TableEnumeration::TableTypes)
    } else {
        None
    }
}

/// Generic implementation of SQLTablesW.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqltables-function>
///
/// Queries the backend for table metadata and stores the result set in the
/// statement handle.
///
/// Three argument combinations are *enumerations* rather than queries — a
/// `"%"` in `catalog_name`, `schema_name` or `table_type` while every other
/// name argument is the empty string. Core serves those itself from
/// [`Backend::catalogs`], [`Backend::schemas`] and [`Backend::table_types`],
/// with every column but the enumerated one NULL. `Backend::tables` is not
/// called for them.
///
/// # Parameters
///
/// - `statement_handle`: Statement handle for retrieved results.
/// - `catalog_name` / `name_length1`: Catalog name filter (NULL = no filter).
/// - `schema_name` / `name_length2`: Schema name search pattern (NULL = no filter).
/// - `table_name` / `name_length3`: Table name search pattern (NULL = no filter).
/// - `table_type` / `name_length4`: Comma-separated list of table types to match (NULL = all
///   types).
///
/// # Spec compliance
///
/// - 01000: General warning (driver-specific informational message); not returned here.
/// - 08S01: Communication link failure; propagated from the backend when its
///   client library reports the link to the data source failed.
/// - 24000: Invalid cursor state — returned if a cursor is already open on this statement.
/// - 40001: Serialization failure — propagated from the backend unchanged, as `08S01` is.
///   Core degrades nothing to `HY000`; that state appears only when the backend's own error
///   mapping produced no more specific one.
/// - 40003: Statement completion unknown — propagated from the backend unchanged.
/// - HY000: General error; returned for any unexpected backend error.
/// - HY001: Memory allocation error; returned if allocation fails.
/// - HY008: Operation canceled. The row's first clause — asynchronous processing, then the
///   function called again — is not applicable: core implements no asynchronous execution and
///   never returns `SQL_STILL_EXECUTING`. The second clause, `SQLCancel` called on the
///   statement "from a different thread in a multithread application", **is returned by this
///   driver**: the row carries no `(DM)` marker, and when a backend call fails with
///   `Backend::is_cancelled` reporting its token signalled, core reports `HY008` in place of
///   the backend's own SQLSTATE.
/// - HY009: Invalid use of null pointer. **Returned by this driver** for the one clause the
///   spec's diagnostics table states *without* a `(DM)` marker: `SQL_ATTR_METADATA_ID` was
///   `SQL_TRUE`, `CatalogName` was a null pointer, and `SQL_CATALOG_NAME` reports that
///   catalog names are supported. The `SchemaName`/`TableName` clause beside it *is*
///   `(DM)`-marked and is deliberately not checked here.
/// - HY010: Function sequence error — returned if connection is not open (HY010). DM cases
///   (async, etc.) are driver-manager-handled; not returned here.
/// - HY013: Memory management error; returned if underlying allocation fails.
/// - HY090: Invalid string or buffer length — the row has two sentences and only the first
///   carries `(DM)`: a name length argument less than 0 but not equal to `SQL_NTS`. The
///   second is the driver's — a name length exceeding "the maximum length value for the
///   corresponding name" — and it cannot arise here, because core declares no maximum name
///   lengths. `SQL_MAX_CATALOG_NAME_LEN`, `SQL_MAX_SCHEMA_NAME_LEN`,
///   `SQL_MAX_TABLE_NAME_LEN` and `SQL_MAX_COLUMN_NAME_LEN` all answer `0`, which the
///   `SQLGetInfo` page defines as "no specified limit or the limit is unknown". A driver
///   that answers a real maximum for any of those has to add the check.
///
///   **Also returned here**, for a condition this row does not itself state: **any** of this
///   function's name arguments, passed as `SQL_NTS`, whose null terminator is not within
///   `MAX_NTS_SCAN` (32 767) code units. Every one of them is resolved by the same helper
///   (`parse_filter_param`), so the limit applies to all of them alike and the diagnostic
///   names which one overran. It is a length the driver cannot determine, which is what
///   `HY090` names; `utf16_to_string` used to hand back the 32 767-unit prefix instead, so
///   the call filtered on a truncated pattern and returned a result set that was wrong
///   rather than absent. An **explicitly measured** length is not limited by this, at any
///   size. See `tables_refuses_an_nts_filter_that_runs_to_the_scan_cap` and
///   `foreign_keys_names_the_argument_whose_nts_scan_overran`.
/// - HY117: Connection suspended (DM) (driver-manager-handled; not returned here).
/// - HYC00: Optional feature not implemented; propagated from backend if catalogs/schemas are
///   unsupported.
/// - HYT00: Timeout expired — **returned by this driver**, not merely propagated. A backend
///   that answered `QueryTimeout::CoreCancels` gets core's own timer (`crate::query_timer`),
///   armed over this call, and `QueryTimer::reclassify` relabels the failing call `HYT00`
///   when the deadline fired. A backend enforcing its own timeout has its `HYT00` propagated
///   unchanged.
/// - HYT01: Connection timeout expired — propagated from the backend unchanged.
/// - IM001: Driver does not support this function (DM) (driver-manager-handled; not returned here).
/// - IM017: Polling disabled; not returned here (the asynchronous notification model is not
///   supported — not DM-annotated in the spec).
/// - IM018: SQLCompleteAsync not called; not returned here (the asynchronous notification model
///   is not supported — not DM-annotated in the spec).
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
#[allow(clippy::too_many_arguments)]
pub unsafe fn sql_tables_w<B: Backend>(
    statement_handle: *mut c_void,
    catalog_name: *const u16,
    name_length1: i16,
    schema_name: *const u16,
    name_length2: i16,
    table_name: *const u16,
    name_length3: i16,
    table_type: *const u16,
    name_length4: i16,
) -> SqlReturn {
    tracing::trace!("SQLTablesW(stmt={:?})", statement_handle);
    // SAFETY: statement_handle is null or a valid StatementHandle<B>; kind and group
    // validated by scope.stmt_with_parent inside the closure.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, |scope| {
            let (stmt, conn) = scope.stmt_with_parent::<B>(statement_handle)?;
            stmt.diagnostics.clear();

            let catalog = parse_filter_param(catalog_name, name_length1, "CatalogName")?;
            let schema = parse_filter_param(schema_name, name_length2, "SchemaName")?;
            let table = parse_filter_param(table_name, name_length3, "TableName")?;
            let tt = parse_filter_param(table_type, name_length4, "TableType")?;
            tracing::debug!(
                "SQLTablesW(stmt={:?}, catalog={:?}, schema={:?}, table={:?}, table_type={:?})",
                statement_handle,
                catalog,
                schema,
                table,
                tt,
            );

            // Spec 24000: Cursor already open.
            if stmt.cursor_open {
                return Err(OdbcError::general(
                    "A cursor is already open on this statement",
                    SqlState::invalid_cursor_state(),
                ));
            }

            // Spec HY010: Connection must be open.
            // Manual-commit mode: this call opens a transaction (or extends
            // the open one), which is what SQL_ATTR_TXN_ISOLATION's HY011 is
            // about. Recorded before the backend call, not after it succeeds:
            // a call that fails partway may still have opened one.
            conn.note_work_started();

            let Some(ref connection) = conn.connection else {
                return Err(OdbcError::general(
                    "Connection is not open",
                    SqlState::function_sequence_error(),
                ));
            };

            // Spec HY009 (not (DM)): METADATA_ID plus a null CatalogName on a
            // data source that has catalogs.
            let metadata_id = metadata_id_enabled(stmt);
            check_metadata_id_null_catalog::<B>(
                connection,
                catalog_name,
                metadata_id,
                "SQLTablesW",
            )?;

            // The token exists once this statement makes its first
            // backend call; created here on demand, then reused for every
            // later call on the same statement (see `mint_cancel_token`).
            let cancel_token = crate::handles::mint_cancel_token::<B>(statement_handle, connection);
            let cancel = crate::handles::cancel_as::<B>(&cancel_token)?;
            // Core-enforced deadline, if the backend asked core to own one.
            // Disarmed by `Drop` the moment this scope ends, so a fast call
            // leaves no thread behind.
            let timer =
                crate::query_timer::QueryTimer::arm::<B>(stmt.core_query_timeout, &cancel_token);

            // Enumeration detection runs on the *raw* arguments, before any
            // normalisation: a normalised `"%"` would no longer be the
            // sentinel, and the enumeration would silently become an ordinary
            // query. Anything that rewrites these arguments must stay behind
            // this block.
            if let Some(kind) = table_enumeration(&catalog, &schema, &table, &tt) {
                tracing::debug!("SQLTablesW: serving the {:?} enumeration", kind);
                let names: Vec<String> = match kind {
                    TableEnumeration::Catalogs => {
                        if B::supports_catalogs(connection) {
                            B::catalogs(connection, cancel).into_odbc()?
                        } else {
                            // Spec-correct without asking the backend: a data
                            // source with no catalogs has no catalogs to list.
                            Vec::new()
                        }
                    }
                    TableEnumeration::Schemas => {
                        if B::supports_schemas(connection) {
                            B::schemas(connection, cancel).into_odbc()?
                        } else {
                            Vec::new()
                        }
                    }
                    TableEnumeration::TableTypes => B::table_types(connection)
                        .into_iter()
                        .map(Cow::into_owned)
                        .collect(),
                };

                // Spec: "All columns except the <enumerated> column contain
                // NULLs."
                let rows: Vec<TableRow> = names
                    .into_iter()
                    .map(|name| match kind {
                        TableEnumeration::Catalogs => TableRow {
                            catalog: Some(name),
                            ..TableRow::default()
                        },
                        TableEnumeration::Schemas => TableRow {
                            schema: Some(name),
                            ..TableRow::default()
                        },
                        TableEnumeration::TableTypes => TableRow {
                            table_type: Some(name),
                            ..TableRow::default()
                        },
                    })
                    .collect();
                let mut values: Vec<Vec<ColumnValue>> =
                    rows.iter().map(TableRow::to_values).collect();
                crate::catalog_sort::sort_rows(
                    &mut values,
                    &TABLES_SORT_KEYS,
                    B::null_collation(connection),
                );
                stmt.set_result_set(StatementData::Synthetic(SyntheticStatement::new(
                    TablesResultCol::all_descriptors(&B::catalog_result_column_widths()),
                    values,
                )));
                return Ok(SqlReturn::SUCCESS);
            }

            // Past the enumeration block, so the sentinels above were compared
            // before anything rewrote them. `tt` is deliberately absent: the
            // spec says "the SQL_ATTR_METADATA_ID statement attribute has no
            // effect upon the TableType argument. TableType is a value list
            // argument, regardless of the setting of SQL_ATTR_METADATA_ID."
            let catalog = normalise_catalog_arg::<B>(connection, catalog, metadata_id);
            let schema = normalise_catalog_arg::<B>(connection, schema, metadata_id);
            let table = normalise_catalog_arg::<B>(connection, table, metadata_id);

            // The spec defines `TableType` as a value list, not a pattern, and
            // `SQL_ATTR_METADATA_ID` never applies to it. Parsed here so that
            // every driver does not have to.
            let table_types = tt
                .as_deref()
                .map(crate::catalog_ident::parse_table_type_list)
                .unwrap_or_default();

            let query = crate::types::TablesQuery::default()
                .with_catalog(catalog.as_deref())
                .with_schema(schema.as_deref())
                .with_table(table.as_deref())
                .with_table_types(table_types.as_slice());
            let rows = B::tables(connection, cancel, &query);
            let rows = timer.check::<B, _, _>(rows, cancel)?;

            let mut values: Vec<Vec<ColumnValue>> = rows.iter().map(TableRow::to_values).collect();
            crate::catalog_sort::sort_rows(
                &mut values,
                &TABLES_SORT_KEYS,
                B::null_collation(connection),
            );
            stmt.set_result_set(StatementData::Synthetic(SyntheticStatement::new(
                TablesResultCol::all_descriptors(&B::catalog_result_column_widths()),
                values,
            )));
            Ok(SqlReturn::SUCCESS)
        })
    };
    tracing::debug!("SQLTablesW -> {:?}", ret);
    ret
}

/// Generic implementation of SQLColumnsW.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlcolumns-function>
///
/// Queries the backend for column metadata and stores the result set in the
/// statement handle.
///
/// # Parameters
///
/// - `statement_handle`: Statement handle.
/// - `catalog_name` / `name_length1`: Catalog name filter (NULL = no filter; no search
///   patterns).
/// - `schema_name` / `name_length2`: Schema name search pattern (NULL = no filter).
/// - `table_name` / `name_length3`: Table name search pattern (NULL = no filter).
/// - `column_name` / `name_length4`: Column name search pattern (NULL = no filter).
///
/// # Spec compliance
///
/// - 01000: General warning (driver-specific informational message); not returned here.
/// - 08S01: Communication link failure; propagated from the backend when its
///   client library reports the link to the data source failed.
/// - 24000: Invalid cursor state — returned if a cursor is already open on this statement.
/// - 40001: Serialization failure — propagated from the backend unchanged, as `08S01` is.
///   Core degrades nothing to `HY000`; that state appears only when the backend's own error
///   mapping produced no more specific one.
/// - 40003: Statement completion unknown — propagated from the backend unchanged.
/// - HY000: General error; returned for any unexpected backend error.
/// - HY001: Memory allocation error; returned if allocation fails.
/// - HY008: Operation canceled. The row's first clause — asynchronous processing, then the
///   function called again — is not applicable: core implements no asynchronous execution and
///   never returns `SQL_STILL_EXECUTING`. The second clause, `SQLCancel` called on the
///   statement "from a different thread in a multithread application", **is returned by this
///   driver**: the row carries no `(DM)` marker, and when a backend call fails with
///   `Backend::is_cancelled` reporting its token signalled, core reports `HY008` in place of
///   the backend's own SQLSTATE.
/// - HY009: Invalid use of null pointer. **Returned by this driver** for the one clause the
///   spec's diagnostics table states *without* a `(DM)` marker: `SQL_ATTR_METADATA_ID` was
///   `SQL_TRUE`, `CatalogName` was a null pointer, and `SQL_CATALOG_NAME` reports that
///   catalog names are supported. The `SchemaName`/`TableName` clause beside it *is*
///   `(DM)`-marked and is deliberately not checked here.
/// - HY010: Function sequence error — returned if connection is not open (HY010). DM cases
///   (async, etc.) are driver-manager-handled; not returned here.
/// - HY013: Memory management error; returned if underlying allocation fails.
/// - HY090: Invalid string or buffer length — every clause of this row is `(DM)`
///   (driver-manager-handled), so none of the row's own clauses is returned here. Unlike
///   several of its siblings' rows, this one has only the name-length-below-zero sentence
///   and no maximum-length sentence, so nothing in it is the driver's.
///
///   **Also returned here**, for a condition this row does not itself state: **any** of this
///   function's name arguments, passed as `SQL_NTS`, whose null terminator is not within
///   `MAX_NTS_SCAN` (32 767) code units. Every one of them is resolved by the same helper
///   (`parse_filter_param`), so the limit applies to all of them alike and the diagnostic
///   names which one overran. It is a length the driver cannot determine, which is what
///   `HY090` names; `utf16_to_string` used to hand back the 32 767-unit prefix instead, so
///   the call filtered on a truncated pattern and returned a result set that was wrong
///   rather than absent. An **explicitly measured** length is not limited by this, at any
///   size. See `tables_refuses_an_nts_filter_that_runs_to_the_scan_cap` and
///   `foreign_keys_names_the_argument_whose_nts_scan_overran`.
/// - HY117: Connection suspended (DM) (driver-manager-handled; not returned here).
/// - HYC00: Optional feature not implemented; propagated from backend if catalogs/schemas are
///   unsupported.
/// - HYT00: Timeout expired — **returned by this driver**, not merely propagated. A backend
///   that answered `QueryTimeout::CoreCancels` gets core's own timer (`crate::query_timer`),
///   armed over this call, and `QueryTimer::reclassify` relabels the failing call `HYT00`
///   when the deadline fired. A backend enforcing its own timeout has its `HYT00` propagated
///   unchanged.
/// - HYT01: Connection timeout expired — propagated from the backend unchanged.
/// - IM001: Driver does not support this function (DM) (driver-manager-handled; not returned here).
/// - IM017: Polling disabled; not returned here (the asynchronous notification model is not
///   supported — not DM-annotated in the spec).
/// - IM018: SQLCompleteAsync not called; not returned here (the asynchronous notification model
///   is not supported — not DM-annotated in the spec).
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
#[allow(clippy::too_many_arguments)]
pub unsafe fn sql_columns_w<B: Backend>(
    statement_handle: *mut c_void,
    catalog_name: *const u16,
    name_length1: i16,
    schema_name: *const u16,
    name_length2: i16,
    table_name: *const u16,
    name_length3: i16,
    column_name: *const u16,
    name_length4: i16,
) -> SqlReturn {
    tracing::trace!("SQLColumnsW(stmt={:?})", statement_handle);
    // SAFETY: statement_handle is null or a valid StatementHandle<B>; kind and group
    // validated by scope.stmt_with_parent inside the closure.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, |scope| {
            let (stmt, conn) = scope.stmt_with_parent::<B>(statement_handle)?;
            stmt.diagnostics.clear();

            let catalog = parse_filter_param(catalog_name, name_length1, "CatalogName")?;
            let schema = parse_filter_param(schema_name, name_length2, "SchemaName")?;
            let table = parse_filter_param(table_name, name_length3, "TableName")?;
            let column = parse_filter_param(column_name, name_length4, "ColumnName")?;
            tracing::debug!(
                "SQLColumnsW(stmt={:?}, catalog={:?}, schema={:?}, table={:?}, column={:?})",
                statement_handle,
                catalog,
                schema,
                table,
                column,
            );

            // Spec 24000: Cursor already open.
            if stmt.cursor_open {
                return Err(OdbcError::general(
                    "A cursor is already open on this statement",
                    SqlState::invalid_cursor_state(),
                ));
            }

            // Spec HY010: Connection must be open.
            // Manual-commit mode: this call opens a transaction (or extends
            // the open one), which is what SQL_ATTR_TXN_ISOLATION's HY011 is
            // about. Recorded before the backend call, not after it succeeds:
            // a call that fails partway may still have opened one.
            conn.note_work_started();

            let Some(ref connection) = conn.connection else {
                return Err(OdbcError::general(
                    "Connection is not open",
                    SqlState::function_sequence_error(),
                ));
            };

            // The token exists once this statement makes its first
            // backend call; created here on demand, then reused for every
            // later call on the same statement (see `mint_cancel_token`).
            let cancel_token = crate::handles::mint_cancel_token::<B>(statement_handle, connection);
            let cancel = crate::handles::cancel_as::<B>(&cancel_token)?;
            // Core-enforced deadline, if the backend asked core to own one.
            // Disarmed by `Drop` the moment this scope ends, so a fast call
            // leaves no thread behind.
            let timer =
                crate::query_timer::QueryTimer::arm::<B>(stmt.core_query_timeout, &cancel_token);

            // Spec HY009 (not (DM)): METADATA_ID plus a null CatalogName on a
            // data source that has catalogs.
            let metadata_id = metadata_id_enabled(stmt);
            check_metadata_id_null_catalog::<B>(
                connection,
                catalog_name,
                metadata_id,
                "SQLColumnsW",
            )?;

            // All four are identifiers under METADATA_ID — `ColumnName`
            // included, which no other catalog function has.
            let catalog = normalise_catalog_arg::<B>(connection, catalog, metadata_id);
            let schema = normalise_catalog_arg::<B>(connection, schema, metadata_id);
            let table = normalise_catalog_arg::<B>(connection, table, metadata_id);
            let column = normalise_catalog_arg::<B>(connection, column, metadata_id);

            let query = crate::types::ColumnsQuery::default()
                .with_catalog(catalog.as_deref())
                .with_schema(schema.as_deref())
                .with_table(table.as_deref())
                .with_column(column.as_deref());
            let rows = B::columns(connection, cancel, &query);
            let rows = timer.check::<B, _, _>(rows, cancel)?;

            let mut values: Vec<Vec<ColumnValue>> = rows.iter().map(ColumnRow::to_values).collect();
            // Spec: ordered by TABLE_CAT, TABLE_SCHEM, TABLE_NAME,
            // ORDINAL_POSITION — zero-based column indices 0, 1, 2, 16.
            crate::catalog_sort::sort_rows(
                &mut values,
                &[0, 1, 2, 16],
                B::null_collation(connection),
            );
            stmt.set_result_set(StatementData::Synthetic(SyntheticStatement::new(
                ColumnsResultCol::all_descriptors(&B::catalog_result_column_widths()),
                values,
            )));
            Ok(SqlReturn::SUCCESS)
        })
    };
    tracing::debug!("SQLColumnsW -> {:?}", ret);
    ret
}

/// Generic implementation of SQLPrimaryKeysW.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlprimarykeys-function>
///
/// Returns the primary key columns for the specified table as a result set.
///
/// # Parameters
///
/// - `statement_handle`: Statement handle.
/// - `catalog_name` / `name_length1`: Catalog name (NULL = no filter; no search patterns).
/// - `schema_name` / `name_length2`: Schema name (NULL = no filter; no search patterns).
/// - `table_name` / `name_length3`: Table name (required; cannot be NULL; no search patterns).
///
/// # Spec compliance
///
/// - 01000: General warning (driver-specific informational message); not returned here.
/// - 08S01: Communication link failure; propagated from the backend when its
///   client library reports the link to the data source failed.
/// - 24000: Invalid cursor state — **returned by this driver** if a cursor is already open on
///   this statement. Alone among the twelve catalog functions, this row is `(DM)`-marked, and
///   only on its first sentence: a cursor open where `SQLFetch` or `SQLFetchScroll` had been
///   called. The second carries no marker and is the driver's — a cursor open but where
///   `SQLFetch` or `SQLFetchScroll` had not been called — and it is the one core enforces.
///   Note: the spec marks this (DM) for some subcases; the driver also returns it directly.
/// - 40001: Serialization failure — propagated from the backend unchanged, as `08S01` is.
///   Core degrades nothing to `HY000`; that state appears only when the backend's own error
///   mapping produced no more specific one.
/// - 40003: Statement completion unknown — propagated from the backend unchanged.
/// - HY000: General error; returned for any unexpected backend error.
/// - HY001: Memory allocation error; returned if allocation fails.
/// - HY008: Operation canceled. The row's first clause — asynchronous processing, then the
///   function called again — is not applicable: core implements no asynchronous execution and
///   never returns `SQL_STILL_EXECUTING`. The second clause, `SQLCancel` called on the
///   statement "from a different thread in a multithread application", **is returned by this
///   driver**: the row carries no `(DM)` marker, and when a backend call fails with
///   `Backend::is_cancelled` reporting its token signalled, core reports `HY008` in place of
///   the backend's own SQLSTATE.
/// - HY009: Invalid use of null pointer. **Returned by this driver** for the one clause the
///   spec's diagnostics table states *without* a `(DM)` marker: `SQL_ATTR_METADATA_ID` was
///   `SQL_TRUE`, `CatalogName` was a null pointer, and `SQL_CATALOG_NAME` reports that
///   catalog names are supported. The `SchemaName`/`TableName` clause beside it *is*
///   `(DM)`-marked and is deliberately not checked here.
///
///   The spec's other `HY009` sentence here, "(DM) The `TableName` argument was a null
///   pointer", **is** `(DM)`-marked, so the Driver Manager returns it and this driver
///   deliberately does **not**. `SQLStatistics` and `SQLSpecialColumns` carry that same
///   sentence *without* the marker and do check it — the difference between the three
///   functions is intentional, not an omission here. See
///   `primary_keys_does_not_check_null_table_name`.
/// - HY010: Function sequence error — returned if connection is not open. DM cases (async, etc.)
///   are driver-manager-handled; not returned here.
/// - HY013: Memory management error; returned if underlying allocation fails.
/// - HY090: Invalid string or buffer length — the row has two sentences and only the first
///   carries `(DM)`: a name length argument less than 0 but not equal to `SQL_NTS`. The
///   second is the driver's — a name length exceeding "the maximum length value for the
///   corresponding name" — and it cannot arise here, because core declares no maximum name
///   lengths. `SQL_MAX_CATALOG_NAME_LEN`, `SQL_MAX_SCHEMA_NAME_LEN`,
///   `SQL_MAX_TABLE_NAME_LEN` and `SQL_MAX_COLUMN_NAME_LEN` all answer `0`, which the
///   `SQLGetInfo` page defines as "no specified limit or the limit is unknown". A driver
///   that answers a real maximum for any of those has to add the check.
///
///   **Also returned here**, for a condition this row does not itself state: **any** of this
///   function's name arguments, passed as `SQL_NTS`, whose null terminator is not within
///   `MAX_NTS_SCAN` (32 767) code units. Every one of them is resolved by the same helper
///   (`parse_filter_param`), so the limit applies to all of them alike and the diagnostic
///   names which one overran. It is a length the driver cannot determine, which is what
///   `HY090` names; `utf16_to_string` used to hand back the 32 767-unit prefix instead, so
///   the call filtered on a truncated pattern and returned a result set that was wrong
///   rather than absent. An **explicitly measured** length is not limited by this, at any
///   size. See `tables_refuses_an_nts_filter_that_runs_to_the_scan_cap` and
///   `foreign_keys_names_the_argument_whose_nts_scan_overran`.
/// - HY117: Connection suspended (DM) (driver-manager-handled; not returned here).
/// - HYC00: Optional feature not implemented; propagated from backend if catalogs/schemas are
///   unsupported.
/// - HYT00: Timeout expired — **returned by this driver**, not merely propagated. A backend
///   that answered `QueryTimeout::CoreCancels` gets core's own timer (`crate::query_timer`),
///   armed over this call, and `QueryTimer::reclassify` relabels the failing call `HYT00`
///   when the deadline fired. A backend enforcing its own timeout has its `HYT00` propagated
///   unchanged.
/// - HYT01: Connection timeout expired — propagated from the backend unchanged.
/// - IM001: Driver does not support this function (DM) (driver-manager-handled; not returned here).
/// - IM017: Polling disabled; not returned here (the asynchronous notification model is not
///   supported — not DM-annotated in the spec).
/// - IM018: SQLCompleteAsync not called; not returned here (the asynchronous notification model
///   is not supported — not DM-annotated in the spec).
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
pub unsafe fn sql_primary_keys_w<B: Backend>(
    statement_handle: *mut c_void,
    catalog_name: *const u16,
    name_length1: i16,
    schema_name: *const u16,
    name_length2: i16,
    table_name: *const u16,
    name_length3: i16,
) -> SqlReturn {
    tracing::trace!("SQLPrimaryKeysW(stmt={:?})", statement_handle);
    // SAFETY: statement_handle is null or a valid StatementHandle<B>; kind and group
    // validated by scope.stmt_with_parent inside the closure.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, |scope| {
            let (stmt, conn) = scope.stmt_with_parent::<B>(statement_handle)?;
            stmt.diagnostics.clear();

            if stmt.cursor_open {
                return Err(OdbcError::general(
                    "A cursor is already open on this statement",
                    SqlState::invalid_cursor_state(),
                ));
            }

            // Manual-commit mode: this call opens a transaction (or extends
            // the open one), which is what SQL_ATTR_TXN_ISOLATION's HY011 is
            // about. Recorded before the backend call, not after it succeeds:
            // a call that fails partway may still have opened one.
            conn.note_work_started();

            let Some(ref connection) = conn.connection else {
                return Err(OdbcError::general(
                    "Connection is not open",
                    SqlState::function_sequence_error(),
                ));
            };

            let catalog = parse_filter_param(catalog_name, name_length1, "CatalogName")?;
            let schema = parse_filter_param(schema_name, name_length2, "SchemaName")?;
            let table = parse_filter_param(table_name, name_length3, "TableName")?;
            tracing::debug!(
                "SQLPrimaryKeysW(stmt={:?}, catalog={:?}, schema={:?}, table={:?})",
                statement_handle,
                catalog,
                schema,
                table,
            );

            // The token exists once this statement makes its first
            // backend call; created here on demand, then reused for every
            // later call on the same statement (see `mint_cancel_token`).
            let cancel_token = crate::handles::mint_cancel_token::<B>(statement_handle, connection);
            let cancel = crate::handles::cancel_as::<B>(&cancel_token)?;
            // Core-enforced deadline, if the backend asked core to own one.
            // Disarmed by `Drop` the moment this scope ends, so a fast call
            // leaves no thread behind.
            let timer =
                crate::query_timer::QueryTimer::arm::<B>(stmt.core_query_timeout, &cancel_token);

            // Spec HY009 (not (DM)): METADATA_ID plus a null CatalogName on a
            // data source that has catalogs. The neighbouring "TableName was a
            // null pointer" clause *is* (DM)-marked here — see the doc comment
            // — so there is deliberately no check for it.
            let metadata_id = metadata_id_enabled(stmt);
            check_metadata_id_null_catalog::<B>(
                connection,
                catalog_name,
                metadata_id,
                "SQLPrimaryKeysW",
            )?;

            let catalog = normalise_catalog_arg::<B>(connection, catalog, metadata_id);
            let schema = normalise_catalog_arg::<B>(connection, schema, metadata_id);
            let table = normalise_catalog_arg::<B>(connection, table, metadata_id);

            let query = crate::types::PrimaryKeysQuery::default()
                .with_catalog(catalog.as_deref())
                .with_schema(schema.as_deref())
                .with_table(table.as_deref());
            let rows = B::primary_keys(connection, cancel, &query);
            let rows = timer.check::<B, _, _>(rows, cancel)?;

            let mut values: Vec<Vec<ColumnValue>> =
                rows.iter().map(PrimaryKeyRow::to_values).collect();
            // Spec: ordered by TABLE_CAT, TABLE_SCHEM, TABLE_NAME, KEY_SEQ
            // — zero-based column indices 0, 1, 2, 4.
            crate::catalog_sort::sort_rows(
                &mut values,
                &[0, 1, 2, 4],
                B::null_collation(connection),
            );
            stmt.set_result_set(StatementData::Synthetic(SyntheticStatement::new(
                PrimaryKeysResultCol::all_descriptors(&B::catalog_result_column_widths()),
                values,
            )));
            Ok(SqlReturn::SUCCESS)
        })
    };
    tracing::debug!("SQLPrimaryKeysW -> {:?}", ret);
    ret
}

/// Generic implementation of SQLForeignKeysW.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlforeignkeys-function>
///
/// Returns foreign key relationships. Either `pk_table` or `fk_table` (or both) may be
/// specified. Returns foreign keys in the FK table that point to the PK table's primary key,
/// foreign keys in other tables referencing the PK table, or both.
///
/// # Parameters
///
/// - `statement_handle`: Statement handle.
/// - `pk_catalog_name` / `name_length1`: Primary key table catalog name (NULL = no filter; no
///   search patterns).
/// - `pk_schema_name` / `name_length2`: Primary key table schema name (NULL = no filter; no
///   search patterns).
/// - `pk_table_name` / `name_length3`: Primary key table name (NULL = no filter; no search
///   patterns).
/// - `fk_catalog_name` / `name_length4`: Foreign key table catalog name (NULL = no filter; no
///   search patterns).
/// - `fk_schema_name` / `name_length5`: Foreign key table schema name (NULL = no filter; no
///   search patterns).
/// - `fk_table_name` / `name_length6`: Foreign key table name (NULL = no filter; no search
///   patterns).
///
/// # Spec compliance
///
/// - 01000: General warning (driver-specific informational message); not returned here.
/// - 08S01: Communication link failure; propagated from the backend when its
///   client library reports the link to the data source failed.
/// - 24000: Invalid cursor state — returned if a cursor is already open on this statement.
/// - 40001: Serialization failure — propagated from the backend unchanged, as `08S01` is.
///   Core degrades nothing to `HY000`; that state appears only when the backend's own error
///   mapping produced no more specific one.
/// - 40003: Statement completion unknown — propagated from the backend unchanged.
/// - HY000: General error; returned for any unexpected backend error.
/// - HY001: Memory allocation error; returned if allocation fails.
/// - HY008: Operation canceled. The row's first clause — asynchronous processing, then the
///   function called again — is not applicable: core implements no asynchronous execution and
///   never returns `SQL_STILL_EXECUTING`. The second clause, `SQLCancel` called on the
///   statement "from a different thread in a multithread application", **is returned by this
///   driver**: the row carries no `(DM)` marker, and when a backend call fails with
///   `Backend::is_cancelled` reporting its token signalled, core reports `HY008` in place of
///   the backend's own SQLSTATE.
/// - HY009: Invalid use of null pointer. **Returned by this driver** for the one clause the
///   spec's diagnostics table states *without* a `(DM)` marker: `SQL_ATTR_METADATA_ID` was
///   `SQL_TRUE`, a catalog argument was a null pointer, and `SQL_CATALOG_NAME` reports that
///   catalog names are supported — checked for `PKCatalogName` and `FKCatalogName` alike. The
///   `SchemaName`/`TableName` clause beside it *is* `(DM)`-marked and is deliberately not
///   checked here.
///
///   The spec's other `HY009` sentence here, "(DM) The `PKTableName` and `FKTableName`
///   arguments were both null pointers", **is** `(DM)`-marked, so the Driver Manager returns
///   it and this driver deliberately does **not** — unlike `SQLStatistics` and
///   `SQLSpecialColumns`, whose equivalent sentence is unmarked. See
///   `foreign_keys_does_not_check_null_table_names`.
/// - HY010: Function sequence error — returned if connection is not open. DM cases (async, etc.)
///   are driver-manager-handled; not returned here.
/// - HY013: Memory management error; returned if underlying allocation fails.
/// - HY090: Invalid string or buffer length — every clause of this row is `(DM)`
///   (driver-manager-handled), so none of the row's own clauses is returned here. Unlike
///   several of its siblings' rows, this one has only the name-length-below-zero sentence
///   and no maximum-length sentence, so nothing in it is the driver's.
///
///   **Also returned here**, for a condition this row does not itself state: **any** of this
///   function's name arguments, passed as `SQL_NTS`, whose null terminator is not within
///   `MAX_NTS_SCAN` (32 767) code units. Every one of them is resolved by the same helper
///   (`parse_filter_param`), so the limit applies to all of them alike and the diagnostic
///   names which one overran. It is a length the driver cannot determine, which is what
///   `HY090` names; `utf16_to_string` used to hand back the 32 767-unit prefix instead, so
///   the call filtered on a truncated pattern and returned a result set that was wrong
///   rather than absent. An **explicitly measured** length is not limited by this, at any
///   size. See `tables_refuses_an_nts_filter_that_runs_to_the_scan_cap` and
///   `foreign_keys_names_the_argument_whose_nts_scan_overran`.
/// - HY117: Connection suspended (DM) (driver-manager-handled; not returned here).
/// - HYC00: Optional feature not implemented; propagated from backend if catalogs/schemas are
///   unsupported.
/// - HYT00: Timeout expired — **returned by this driver**, not merely propagated. A backend
///   that answered `QueryTimeout::CoreCancels` gets core's own timer (`crate::query_timer`),
///   armed over this call, and `QueryTimer::reclassify` relabels the failing call `HYT00`
///   when the deadline fired. A backend enforcing its own timeout has its `HYT00` propagated
///   unchanged.
/// - HYT01: Connection timeout expired — propagated from the backend unchanged.
/// - IM001: Driver does not support this function (DM) (driver-manager-handled; not returned here).
/// - IM017: Polling disabled; not returned here (the asynchronous notification model is not
///   supported — not DM-annotated in the spec).
/// - IM018: SQLCompleteAsync not called; not returned here (the asynchronous notification model
///   is not supported — not DM-annotated in the spec).
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
#[allow(clippy::too_many_arguments)]
pub unsafe fn sql_foreign_keys_w<B: Backend>(
    statement_handle: *mut c_void,
    pk_catalog_name: *const u16,
    name_length1: i16,
    pk_schema_name: *const u16,
    name_length2: i16,
    pk_table_name: *const u16,
    name_length3: i16,
    fk_catalog_name: *const u16,
    name_length4: i16,
    fk_schema_name: *const u16,
    name_length5: i16,
    fk_table_name: *const u16,
    name_length6: i16,
) -> SqlReturn {
    tracing::trace!("SQLForeignKeysW(stmt={:?})", statement_handle);
    // SAFETY: statement_handle is null or a valid StatementHandle<B>; kind and group
    // validated by scope.stmt_with_parent inside the closure.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, |scope| {
            let (stmt, conn) = scope.stmt_with_parent::<B>(statement_handle)?;
            stmt.diagnostics.clear();

            if stmt.cursor_open {
                return Err(OdbcError::general(
                    "A cursor is already open on this statement",
                    SqlState::invalid_cursor_state(),
                ));
            }

            // Manual-commit mode: this call opens a transaction (or extends
            // the open one), which is what SQL_ATTR_TXN_ISOLATION's HY011 is
            // about. Recorded before the backend call, not after it succeeds:
            // a call that fails partway may still have opened one.
            conn.note_work_started();

            let Some(ref connection) = conn.connection else {
                return Err(OdbcError::general(
                    "Connection is not open",
                    SqlState::function_sequence_error(),
                ));
            };

            let pk_catalog = parse_filter_param(pk_catalog_name, name_length1, "PKCatalogName")?;
            let pk_schema = parse_filter_param(pk_schema_name, name_length2, "PKSchemaName")?;
            let pk_table = parse_filter_param(pk_table_name, name_length3, "PKTableName")?;
            let fk_catalog = parse_filter_param(fk_catalog_name, name_length4, "FKCatalogName")?;
            let fk_schema = parse_filter_param(fk_schema_name, name_length5, "FKSchemaName")?;
            let fk_table = parse_filter_param(fk_table_name, name_length6, "FKTableName")?;
            tracing::debug!(
                "SQLForeignKeysW(stmt={:?}, pk_catalog={:?}, pk_schema={:?}, pk_table={:?}, \
                 fk_catalog={:?}, fk_schema={:?}, fk_table={:?})",
                statement_handle,
                pk_catalog,
                pk_schema,
                pk_table,
                fk_catalog,
                fk_schema,
                fk_table,
            );

            // The token exists once this statement makes its first
            // backend call; created here on demand, then reused for every
            // later call on the same statement (see `mint_cancel_token`).
            let cancel_token = crate::handles::mint_cancel_token::<B>(statement_handle, connection);
            let cancel = crate::handles::cancel_as::<B>(&cancel_token)?;
            // Core-enforced deadline, if the backend asked core to own one.
            // Disarmed by `Drop` the moment this scope ends, so a fast call
            // leaves no thread behind.
            let timer =
                crate::query_timer::QueryTimer::arm::<B>(stmt.core_query_timeout, &cancel_token);

            // Spec HY009 (not (DM)): METADATA_ID plus a null catalog argument
            // on a data source that has catalogs — and this function has two
            // catalog arguments, so both are checked. The "PKTableName and
            // FKTableName were both null pointers" clause *is* (DM)-marked —
            // see the doc comment — so there is deliberately no check for it.
            let metadata_id = metadata_id_enabled(stmt);
            check_metadata_id_null_catalog::<B>(
                connection,
                pk_catalog_name,
                metadata_id,
                "SQLForeignKeysW (PKCatalogName)",
            )?;
            check_metadata_id_null_catalog::<B>(
                connection,
                fk_catalog_name,
                metadata_id,
                "SQLForeignKeysW (FKCatalogName)",
            )?;

            // Both trios are identifiers under METADATA_ID, not just the PK one.
            let pk_catalog = normalise_catalog_arg::<B>(connection, pk_catalog, metadata_id);
            let pk_schema = normalise_catalog_arg::<B>(connection, pk_schema, metadata_id);
            let pk_table = normalise_catalog_arg::<B>(connection, pk_table, metadata_id);
            let fk_catalog = normalise_catalog_arg::<B>(connection, fk_catalog, metadata_id);
            let fk_schema = normalise_catalog_arg::<B>(connection, fk_schema, metadata_id);
            let fk_table = normalise_catalog_arg::<B>(connection, fk_table, metadata_id);

            let query = crate::types::ForeignKeysQuery::default()
                .with_pk_catalog(pk_catalog.as_deref())
                .with_pk_schema(pk_schema.as_deref())
                .with_pk_table(pk_table.as_deref())
                .with_fk_catalog(fk_catalog.as_deref())
                .with_fk_schema(fk_schema.as_deref())
                .with_fk_table(fk_table.as_deref());
            let rows = B::foreign_keys(connection, cancel, &query);
            let rows = timer.check::<B, _, _>(rows, cancel)?;

            let mut values: Vec<Vec<ColumnValue>> =
                rows.iter().map(ForeignKeyRow::to_values).collect();
            // Spec: "If the foreign keys associated with a primary key are
            // requested, the result set is ordered by FKTABLE_CAT,
            // FKTABLE_SCHEM, FKTABLE_NAME, and KEY_SEQ. If the primary keys
            // associated with a foreign key are requested, the result set is
            // ordered by PKTABLE_CAT, PKTABLE_SCHEM, PKTABLE_NAME, and
            // KEY_SEQ."
            //
            // TODO(spec): when BOTH pk_table and fk_table are supplied the
            // spec states neither order. The FK order is used, because that
            // case "should be one key at most" per the same page, making the
            // choice unobservable for a conforming data source.
            let keys: &[usize] = if pk_table.is_some() {
                &[4, 5, 6, 8] // FKTABLE_CAT, FKTABLE_SCHEM, FKTABLE_NAME, KEY_SEQ
            } else {
                &[0, 1, 2, 8] // PKTABLE_CAT, PKTABLE_SCHEM, PKTABLE_NAME, KEY_SEQ
            };
            crate::catalog_sort::sort_rows(&mut values, keys, B::null_collation(connection));
            stmt.set_result_set(StatementData::Synthetic(SyntheticStatement::new(
                ForeignKeysResultCol::all_descriptors(&B::catalog_result_column_widths()),
                values,
            )));
            Ok(SqlReturn::SUCCESS)
        })
    };
    tracing::debug!("SQLForeignKeysW -> {:?}", ret);
    ret
}

/// Generic implementation of SQLStatisticsW.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlstatistics-function>
///
/// Queries the backend for index statistics on the given table and stores the result set in
/// the statement handle. Backends that do not expose index metadata return `NotImplemented`,
/// which falls back to an empty result set with the standard 13-column schema; a table with
/// no indexes is a spec-legitimate empty response.
///
/// # Parameters
///
/// - `statement_handle`: Statement handle.
/// - `catalog_name` / `name_length1`: Catalog name (NULL = no filter; no search patterns).
/// - `schema_name` / `name_length2`: Schema name (NULL = no filter; no search patterns).
/// - `table_name` / `name_length3`: Table name (NULL = no filter; no search patterns).
/// - `unique`: `SQL_INDEX_UNIQUE` or `SQL_INDEX_ALL`.
/// - `_reserved`: `SQL_ENSURE` or `SQL_QUICK` (ignored — this driver always performs the
///   equivalent of `SQL_QUICK` behavior).
///
/// # Spec compliance
///
/// - 01000: General warning (driver-specific informational message); not returned here.
/// - 08S01: Communication link failure; propagated from the backend when its
///   client library reports the link to the data source failed.
/// - 24000: Invalid cursor state — returned if a cursor is already open on this statement.
/// - 40001: Serialization failure — propagated from the backend unchanged, as `08S01` is.
///   Core degrades nothing to `HY000`; that state appears only when the backend's own error
///   mapping produced no more specific one.
/// - 40003: Statement completion unknown — propagated from the backend unchanged.
/// - HY000: General error; returned for any unexpected backend error.
/// - HY001: Memory allocation error; returned if allocation fails.
/// - HY008: Operation canceled. The row's first clause — asynchronous processing, then the
///   function called again — is not applicable: core implements no asynchronous execution and
///   never returns `SQL_STILL_EXECUTING`. The second clause, `SQLCancel` called on the
///   statement "from a different thread in a multithread application", **is returned by this
///   driver**: the row carries no `(DM)` marker, and when a backend call fails with
///   `Backend::is_cancelled` reporting its token signalled, core reports `HY008` in place of
///   the backend's own SQLSTATE.
/// - HY009: Invalid use of null pointer. **Both** of the spec's clauses are unmarked here, so
///   both are returned by this driver: (1) the `TableName` argument was a null pointer —
///   note that `SQLPrimaryKeys` and `SQLForeignKeys` carry this same sentence *with* a `(DM)`
///   marker and therefore must not check it; and (2) `SQL_ATTR_METADATA_ID` was `SQL_TRUE`,
///   `CatalogName` was a null pointer, and `SQL_CATALOG_NAME` reports that catalog names are
///   supported. The `SchemaName` half of clause (2) *is* `(DM)`-marked and is not checked.
/// - HY010: Function sequence error — returned if connection is not open. DM cases (async, etc.)
///   are driver-manager-handled; not returned here.
/// - HY013: Memory management error; returned if underlying allocation fails.
/// - HY090: Invalid string or buffer length — the row has two sentences and only the first
///   carries `(DM)`: a name length argument less than 0 but not equal to `SQL_NTS`. The
///   second is the driver's — a name length exceeding "the maximum length value for the
///   corresponding name" — and it cannot arise here, because core declares no maximum name
///   lengths. `SQL_MAX_CATALOG_NAME_LEN`, `SQL_MAX_SCHEMA_NAME_LEN`,
///   `SQL_MAX_TABLE_NAME_LEN` and `SQL_MAX_COLUMN_NAME_LEN` all answer `0`, which the
///   `SQLGetInfo` page defines as "no specified limit or the limit is unknown". A driver
///   that answers a real maximum for any of those has to add the check.
///
///   **Also returned here**, for a condition this row does not itself state: **any** of this
///   function's name arguments, passed as `SQL_NTS`, whose null terminator is not within
///   `MAX_NTS_SCAN` (32 767) code units. Every one of them is resolved by the same helper
///   (`parse_filter_param`), so the limit applies to all of them alike and the diagnostic
///   names which one overran. It is a length the driver cannot determine, which is what
///   `HY090` names; `utf16_to_string` used to hand back the 32 767-unit prefix instead, so
///   the call filtered on a truncated pattern and returned a result set that was wrong
///   rather than absent. An **explicitly measured** length is not limited by this, at any
///   size. See `tables_refuses_an_nts_filter_that_runs_to_the_scan_cap` and
///   `foreign_keys_names_the_argument_whose_nts_scan_overran`.
/// - HY100: Uniqueness option type out of range (DM) (driver-manager-handled; not returned here).
/// - HY101: Accuracy option type out of range (DM) (driver-manager-handled; not returned here).
/// - HY117: Connection suspended (DM) (driver-manager-handled; not returned here).
/// - HYC00: Optional feature not implemented; propagated from backend if index statistics are
///   unsupported for some reason other than the `NotImplemented` fallback (which instead yields
///   an empty result set).
/// - HYT00: Timeout expired — **returned by this driver**, not merely propagated. A backend
///   that answered `QueryTimeout::CoreCancels` gets core's own timer (`crate::query_timer`),
///   armed over this call, and `QueryTimer::reclassify` relabels the failing call `HYT00`
///   when the deadline fired. A backend enforcing its own timeout has its `HYT00` propagated
///   unchanged.
/// - HYT01: Connection timeout expired — propagated from the backend unchanged.
/// - IM001: Driver does not support this function (DM) (driver-manager-handled; not returned here).
/// - IM017: Polling disabled; not returned here (the asynchronous notification model is not
///   supported — not DM-annotated in the spec).
/// - IM018: SQLCompleteAsync not called; not returned here (the asynchronous notification model
///   is not supported — not DM-annotated in the spec).
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
#[allow(clippy::too_many_arguments)]
pub unsafe fn sql_statistics_w<B: Backend>(
    statement_handle: *mut c_void,
    catalog_name: *const u16,
    name_length1: i16,
    schema_name: *const u16,
    name_length2: i16,
    table_name: *const u16,
    name_length3: i16,
    unique: u16,
    _reserved: u16,
) -> SqlReturn {
    tracing::trace!(
        "SQLStatisticsW(stmt={:?}, unique={})",
        statement_handle,
        unique
    );
    let unique_only = unique == SQL_INDEX_UNIQUE;
    tracing::debug!("SQLStatisticsW: unique_only={}", unique_only);
    // SAFETY: statement_handle is null or a valid StatementHandle<B>; kind and group
    // validated by scope.stmt_with_parent inside the closure.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, |scope| {
            let (stmt, conn) = scope.stmt_with_parent::<B>(statement_handle)?;
            stmt.diagnostics.clear();

            if stmt.cursor_open {
                return Err(OdbcError::general(
                    "A cursor is already open on this statement",
                    SqlState::invalid_cursor_state(),
                ));
            }

            // Manual-commit mode: this call opens a transaction (or extends
            // the open one), which is what SQL_ATTR_TXN_ISOLATION's HY011 is
            // about. Recorded before the backend call, not after it succeeds:
            // a call that fails partway may still have opened one.
            conn.note_work_started();

            let Some(ref connection) = conn.connection else {
                return Err(OdbcError::general(
                    "Connection is not open",
                    SqlState::function_sequence_error(),
                ));
            };

            // Spec HY009, both clauses driver-side here: `SQLStatistics` is one
            // of only three catalog functions whose "TableName argument was a
            // null pointer" sentence carries no (DM) marker — the others being
            // `SQLSpecialColumns` and `SQLColumnPrivileges`.
            check_null_table_name(table_name, "SQLStatisticsW")?;
            let metadata_id = metadata_id_enabled(stmt);
            check_metadata_id_null_catalog::<B>(
                connection,
                catalog_name,
                metadata_id,
                "SQLStatisticsW",
            )?;

            let catalog = parse_filter_param(catalog_name, name_length1, "CatalogName")?;
            let schema = parse_filter_param(schema_name, name_length2, "SchemaName")?;
            let table = parse_filter_param(table_name, name_length3, "TableName")?;
            tracing::debug!(
                "SQLStatisticsW(stmt={:?}, catalog={:?}, schema={:?}, table={:?})",
                statement_handle,
                catalog,
                schema,
                table,
            );

            // `unique` and `_reserved` are not strings and are never normalised.
            let catalog = normalise_catalog_arg::<B>(connection, catalog, metadata_id);
            let schema = normalise_catalog_arg::<B>(connection, schema, metadata_id);
            let table = normalise_catalog_arg::<B>(connection, table, metadata_id);

            // The token exists once this statement makes its first
            // backend call; created here on demand, then reused for every
            // later call on the same statement (see `mint_cancel_token`).
            let cancel_token = crate::handles::mint_cancel_token::<B>(statement_handle, connection);
            let cancel = crate::handles::cancel_as::<B>(&cancel_token)?;
            // Core-enforced deadline, if the backend asked core to own one.
            // Disarmed by `Drop` the moment this scope ends, so a fast call
            // leaves no thread behind.
            let timer =
                crate::query_timer::QueryTimer::arm::<B>(stmt.core_query_timeout, &cancel_token);

            let query = crate::types::StatisticsQuery::new(unique_only)
                .with_catalog(catalog.as_deref())
                .with_schema(schema.as_deref())
                .with_table(table.as_deref());
            match B::statistics(connection, cancel, &query)
                // Converted before matching, so the `NotImplemented` arm below can
                // still recognise core's own variant inside the backend's error.
                .into_odbc()
            {
                Ok(rows) => {
                    let mut values: Vec<Vec<ColumnValue>> =
                        rows.iter().map(StatisticsRow::to_values).collect();
                    // Spec: ordered by NON_UNIQUE, TYPE, INDEX_QUALIFIER,
                    // INDEX_NAME, ORDINAL_POSITION — zero-based column
                    // indices 3, 6, 4, 5, 7.
                    crate::catalog_sort::sort_rows(
                        &mut values,
                        &[3, 6, 4, 5, 7],
                        B::null_collation(connection),
                    );
                    stmt.set_result_set(StatementData::Synthetic(SyntheticStatement::new(
                        statistics_columns(&B::catalog_result_column_widths()),
                        values,
                    )));
                    Ok(SqlReturn::SUCCESS)
                }
                Err(OdbcError::NotImplemented { .. }) => {
                    stmt.set_result_set(StatementData::Synthetic(SyntheticStatement::new(
                        statistics_columns(&B::catalog_result_column_widths()),
                        vec![],
                    )));
                    Ok(SqlReturn::SUCCESS)
                }
                // Only this arm is reclassified. The `NotImplemented` arm above is
                // not a failure at all — it is how a backend says it exposes no such
                // metadata, and the spec's answer to that is an empty result set,
                // not `HY008`.
                Err(e) => timer.check::<B, _, _>(Err(e), cancel),
            }
        })
    };
    tracing::debug!("SQLStatisticsW -> {:?}", ret);
    ret
}

/// Generic implementation of SQLSpecialColumnsW.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlspecialcolumns-function>
///
/// Queries the backend for the optimal row identifier or row-version columns of the given
/// table and stores the result set in the statement handle. Backends that do not expose
/// pseudo-columns or auto-updated row-version columns in the ODBC sense return
/// `NotImplemented`, which falls back to an empty result set with the standard 8-column
/// schema.
///
/// # Parameters
///
/// - `statement_handle`: Statement handle.
/// - `identifier_type_raw`: `SQL_BEST_ROWID` or `SQL_ROWVER`.
/// - `catalog_name` / `name_length1`: Catalog name (NULL = no filter; no search patterns).
/// - `schema_name` / `name_length2`: Schema name (NULL = no filter; no search patterns).
/// - `table_name` / `name_length3`: Table name (NULL = no filter; no search patterns).
/// - `scope_raw`: Minimum required scope (`SQL_SCOPE_CURROW`, `SQL_SCOPE_TRANSACTION`,
///   `SQL_SCOPE_SESSION`).
/// - `nullable_raw`: `SQL_NO_NULLS` or `SQL_NULLABLE`.
///
/// # Spec compliance
///
/// - 01000: General warning (driver-specific informational message); not returned here.
/// - 08S01: Communication link failure; propagated from the backend when its
///   client library reports the link to the data source failed.
/// - 24000: Invalid cursor state — returned if a cursor is already open on this statement.
/// - 40001: Serialization failure — propagated from the backend unchanged, as `08S01` is.
///   Core degrades nothing to `HY000`; that state appears only when the backend's own error
///   mapping produced no more specific one.
/// - 40003: Statement completion unknown — propagated from the backend unchanged.
/// - HY000: General error; returned for any unexpected backend error.
/// - HY001: Memory allocation error; returned if allocation fails.
/// - HY008: Operation canceled. The row's first clause — asynchronous processing, then the
///   function called again — is not applicable: core implements no asynchronous execution and
///   never returns `SQL_STILL_EXECUTING`. The second clause, `SQLCancel` called on the
///   statement "from a different thread in a multithread application", **is returned by this
///   driver**: the row carries no `(DM)` marker, and when a backend call fails with
///   `Backend::is_cancelled` reporting its token signalled, core reports `HY008` in place of
///   the backend's own SQLSTATE.
/// - HY009: Invalid use of null pointer. **Both** of the spec's clauses are unmarked here, so
///   both are returned by this driver: (1) the `TableName` argument was a null pointer —
///   note that `SQLPrimaryKeys` and `SQLForeignKeys` carry this same sentence *with* a `(DM)`
///   marker and therefore must not check it; and (2) `SQL_ATTR_METADATA_ID` was `SQL_TRUE`,
///   `CatalogName` was a null pointer, and `SQL_CATALOG_NAME` reports that catalog names are
///   supported. The `SchemaName` half of clause (2) *is* `(DM)`-marked and is not checked.
/// - HY010: Function sequence error — returned if connection is not open. DM cases (async, etc.)
///   are driver-manager-handled; not returned here.
/// - HY013: Memory management error; returned if underlying allocation fails.
/// - HY090: Invalid string or buffer length — the row has two sentences and only the first
///   carries `(DM)`: a name length argument less than 0 but not equal to `SQL_NTS`. The
///   second is the driver's — a name length exceeding "the maximum length value for the
///   corresponding name" — and it cannot arise here, because core declares no maximum name
///   lengths. `SQL_MAX_CATALOG_NAME_LEN`, `SQL_MAX_SCHEMA_NAME_LEN`,
///   `SQL_MAX_TABLE_NAME_LEN` and `SQL_MAX_COLUMN_NAME_LEN` all answer `0`, which the
///   `SQLGetInfo` page defines as "no specified limit or the limit is unknown". A driver
///   that answers a real maximum for any of those has to add the check.
///
///   **Also returned here**, for a condition this row does not itself state: **any** of this
///   function's name arguments, passed as `SQL_NTS`, whose null terminator is not within
///   `MAX_NTS_SCAN` (32 767) code units. Every one of them is resolved by the same helper
///   (`parse_filter_param`), so the limit applies to all of them alike and the diagnostic
///   names which one overran. It is a length the driver cannot determine, which is what
///   `HY090` names; `utf16_to_string` used to hand back the 32 767-unit prefix instead, so
///   the call filtered on a truncated pattern and returned a result set that was wrong
///   rather than absent. An **explicitly measured** length is not limited by this, at any
///   size. See `tables_refuses_an_nts_filter_that_runs_to_the_scan_cap` and
///   `foreign_keys_names_the_argument_whose_nts_scan_overran`.
/// - HY097: Column type out of range (DM) (driver-manager-handled; not returned here). Should
///   not occur, since the Driver Manager validates `IdentifierType`; if an unrecognized value
///   somehow reaches the driver, it is treated as an unsupported characteristic and returns an
///   empty result set rather than an error.
/// - HY098: Scope type out of range (DM) (driver-manager-handled; not returned here). Same
///   unrecognized-value handling as HY097 applies to `Scope`.
/// - HY099: Nullable type out of range (DM) (driver-manager-handled; not returned by this
///   driver). Same unrecognized-value handling as HY097 applies to `Nullable`.
/// - HY117: Connection suspended (DM) (driver-manager-handled; not returned here).
/// - HYC00: Optional feature not implemented; propagated from backend if this characteristic is
///   unsupported for some reason other than the `NotImplemented` fallback (which instead yields
///   an empty result set).
/// - HYT00: Timeout expired — **returned by this driver**, not merely propagated. A backend
///   that answered `QueryTimeout::CoreCancels` gets core's own timer (`crate::query_timer`),
///   armed over this call, and `QueryTimer::reclassify` relabels the failing call `HYT00`
///   when the deadline fired. A backend enforcing its own timeout has its `HYT00` propagated
///   unchanged.
/// - HYT01: Connection timeout expired — propagated from the backend unchanged.
/// - IM001: Driver does not support this function (DM) (driver-manager-handled; not returned here).
/// - IM017: Polling disabled; not returned here (the asynchronous notification model is not
///   supported — not DM-annotated in the spec).
/// - IM018: SQLCompleteAsync not called; not returned here (the asynchronous notification model
///   is not supported — not DM-annotated in the spec).
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
#[allow(clippy::too_many_arguments)]
pub unsafe fn sql_special_columns_w<B: Backend>(
    statement_handle: *mut c_void,
    identifier_type_raw: u16,
    catalog_name: *const u16,
    name_length1: i16,
    schema_name: *const u16,
    name_length2: i16,
    table_name: *const u16,
    name_length3: i16,
    scope_raw: u16,
    nullable_raw: u16,
) -> SqlReturn {
    tracing::trace!(
        "SQLSpecialColumnsW(stmt={:?}, id_type={}, scope={}, nullable={})",
        statement_handle,
        identifier_type_raw,
        scope_raw,
        nullable_raw
    );
    // SAFETY: statement_handle is null or a valid StatementHandle<B>; kind and group
    // validated by scope.stmt_with_parent inside the closure.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, |scope| {
            let (stmt, conn) = scope.stmt_with_parent::<B>(statement_handle)?;
            stmt.diagnostics.clear();

            if stmt.cursor_open {
                return Err(OdbcError::general(
                    "A cursor is already open on this statement",
                    SqlState::invalid_cursor_state(),
                ));
            }

            // Manual-commit mode: this call opens a transaction (or extends
            // the open one), which is what SQL_ATTR_TXN_ISOLATION's HY011 is
            // about. Recorded before the backend call, not after it succeeds:
            // a call that fails partway may still have opened one.
            conn.note_work_started();

            let Some(ref connection) = conn.connection else {
                return Err(OdbcError::general(
                    "Connection is not open",
                    SqlState::function_sequence_error(),
                ));
            };

            // Spec HY009, both clauses driver-side here: `SQLSpecialColumns` is
            // one of three catalog functions whose "TableName argument was a
            // null pointer" sentence carries no (DM) marker — the others being
            // `SQLStatistics` and `SQLColumnPrivileges`. Checked before the
            // IdentifierType/Scope/Nullable arm below, so a null TableName is a
            // diagnosed error rather than being masked by the empty result set
            // that an unsupported characteristic produces.
            check_null_table_name(table_name, "SQLSpecialColumnsW")?;
            let metadata_id = metadata_id_enabled(stmt);
            check_metadata_id_null_catalog::<B>(
                connection,
                catalog_name,
                metadata_id,
                "SQLSpecialColumnsW",
            )?;

            let empty = |stmt: &mut StatementHandle<B>| {
                stmt.set_result_set(StatementData::Synthetic(SyntheticStatement::new(
                    special_columns_columns(&B::catalog_result_column_widths()),
                    vec![],
                )));
                Ok(SqlReturn::SUCCESS)
            };

            let (Some(identifier_type), Some(scope), Some(nullable)) = (
                identifier_type_from_raw(identifier_type_raw),
                scope_from_raw(scope_raw),
                nullable_from_raw(nullable_raw),
            ) else {
                tracing::warn!(
                    "SQLSpecialColumnsW: unrecognized argument (id_type={}, scope={}, nullable={}); returning empty result set",
                    identifier_type_raw,
                    scope_raw,
                    nullable_raw
                );
                return empty(stmt);
            };
            tracing::debug!(
                "SQLSpecialColumnsW: id_type={:?}, scope={:?}, nullable={:?}",
                identifier_type,
                scope,
                nullable
            );

            let catalog = parse_filter_param(catalog_name, name_length1, "CatalogName")?;
            let schema = parse_filter_param(schema_name, name_length2, "SchemaName")?;
            let table = parse_filter_param(table_name, name_length3, "TableName")?;
            tracing::debug!(
                "SQLSpecialColumnsW(stmt={:?}, catalog={:?}, schema={:?}, table={:?})",
                statement_handle,
                catalog,
                schema,
                table,
            );

            // `identifier_type`, `scope` and `nullable` are not strings and are
            // never normalised.
            let catalog = normalise_catalog_arg::<B>(connection, catalog, metadata_id);
            let schema = normalise_catalog_arg::<B>(connection, schema, metadata_id);
            let table = normalise_catalog_arg::<B>(connection, table, metadata_id);

            // The token exists once this statement makes its first
            // backend call; created here on demand, then reused for every
            // later call on the same statement (see `mint_cancel_token`).
            let cancel_token = crate::handles::mint_cancel_token::<B>(statement_handle, connection);
            let cancel = crate::handles::cancel_as::<B>(&cancel_token)?;
            // Core-enforced deadline, if the backend asked core to own one.
            // Disarmed by `Drop` the moment this scope ends, so a fast call
            // leaves no thread behind.
            let timer =
                crate::query_timer::QueryTimer::arm::<B>(stmt.core_query_timeout, &cancel_token);

            let query = crate::types::SpecialColumnsQuery::new(identifier_type, scope, nullable)
                .with_catalog(catalog.as_deref())
                .with_schema(schema.as_deref())
                .with_table(table.as_deref());
            match B::special_columns(connection, cancel, &query)
                // Converted before matching, so the `NotImplemented` arm below can
                // still recognise core's own variant inside the backend's error.
                .into_odbc()
            {
                Ok(rows) => {
                    let mut values: Vec<Vec<ColumnValue>> =
                        rows.iter().map(SpecialColumnRow::to_values).collect();
                    // Spec: ordered by SCOPE — zero-based column index 0.
                    crate::catalog_sort::sort_rows(
                        &mut values,
                        &[0],
                        B::null_collation(connection),
                    );
                    stmt.set_result_set(StatementData::Synthetic(SyntheticStatement::new(
                        special_columns_columns(&B::catalog_result_column_widths()),
                        values,
                    )));
                    Ok(SqlReturn::SUCCESS)
                }
                Err(OdbcError::NotImplemented { .. }) => empty(stmt),
                // Only this arm is reclassified. The `NotImplemented` arm above is
                // not a failure at all — it is how a backend says it exposes no such
                // metadata, and the spec's answer to that is an empty result set,
                // not `HY008`.
                Err(e) => timer.check::<B, _, _>(Err(e), cancel),
            }
        })
    };
    tracing::debug!("SQLSpecialColumnsW -> {:?}", ret);
    ret
}

/// Narrows a backend-reported column count to the `u16` the 07009 range
/// check in [`sql_describe_col_w`] and [`sql_col_attribute_w`] compares
/// `column_number` against, saturating up rather than down when it does not
/// fit.
///
/// `StatementBackend::column_count` returns `i16` — the `SQLNumResultCols`
/// ABI type — so a *positive* count always fits `u16`: its max, 32 767, is
/// below `u16::MAX`. The only way `u16::try_from` fails here is a **negative**
/// count from a backend, not an oversized one — the shape a naive
/// `unwrap_or(0)` invites is unreachable for "too many columns" given this
/// signature, but a misbehaving `StatementBackend` impl can still return a
/// negative value nothing here validates against.
///
/// `unwrap_or(0)` used to collapse that case to 0, and the `column_number > 0`
/// comparison that follows then rejected every column as 07009, including
/// column 1 — the reported count could not be trusted, and the driver
/// answered by trusting it least. Saturating up to `u16::MAX` instead makes
/// the comparison permissive whenever the count can't be represented,
/// deferring to `describe_col`'s own answer rather than manufacturing a
/// range-check failure the backend never reported.
///
/// Covered by
/// `tests::describe_col_succeeds_when_backend_column_count_is_negative` and
/// `tests::col_attribute_succeeds_when_backend_column_count_is_negative`.
fn column_count_upper_bound(column_count: i16) -> u16 {
    u16::try_from(column_count).unwrap_or_else(|_| {
        tracing::warn!(
            "column_count_upper_bound: backend reported a column count ({column_count}) that \
             does not fit u16; treating the 07009 range check as unbounded rather than \
             rejecting every column"
        );
        u16::MAX
    })
}

/// Generic implementation of SQLDescribeColW.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqldescribecol-function>
///
/// Returns descriptor information (name, type, column size, decimal digits, nullability) for
/// one column in the current result set.
///
/// # Parameters
///
/// - `statement_handle`: Statement handle.
/// - `column_number`: Column number (1-based). Column 0 (bookmark) returns 07009 since bookmarks
///   are not supported (`SQL_ATTR_USE_BOOKMARKS` is `SQL_UB_OFF`).
/// - `column_name` / `buffer_length` / `name_length_ptr`: Output buffer for the column name.
///   If `column_name` is NULL, `name_length_ptr` still returns the available length.
/// - `data_type_ptr`: Output for the SQL data type. May be NULL.
/// - `column_size_ptr`: Output for the column size. May be NULL.
/// - `decimal_digits_ptr`: Output for decimal digits. May be NULL.
/// - `nullable_ptr`: Output for nullability (`SQL_NO_NULLS`, `SQL_NULLABLE`,
///   `SQL_NULLABLE_UNKNOWN`). May be NULL.
///
/// # Spec compliance
///
/// - 01000: General warning (driver-specific informational message); not returned here.
/// - 01004: String data right truncated — returned via `write_utf16` if `column_name` buffer is
///   too small (returns `SQL_SUCCESS_WITH_INFO`).
/// - 07005: Prepared statement not a cursor-specification — returned if no result set is open.
/// - 07009: Invalid descriptor index — returned if `column_number` is 0 (bookmarks not
///   supported) or greater than the number of columns. The spec puts `(DM)` on the bookmark
///   clause only, leaving the out-of-range clause to the driver, and core checks it itself
///   against `StatementBackend::column_count` **before** calling `describe_col`. That ordering
///   is what lets every other SQLSTATE below be real: this state is now returned only for the
///   case its message describes.
/// - 08S01: Communication link failure; **returned by this driver**, propagated unchanged when
///   `StatementBackend::describe_col` fails and the driver's error mapping classified it that
///   way. Until core owned the range check, every such failure was overwritten with `07009`.
/// - HY000: General error; returned for any unexpected internal error, and propagated from
///   `describe_col` when the backend's mapping produced no more specific state.
/// - HY001: Memory allocation error; returned if allocation fails.
/// - HY008: Operation canceled. The asynchronous clause is inapplicable — core never returns
///   `SQL_STILL_EXECUTING` — but the second clause, `SQLCancel` called on the statement "from a
///   different thread in a multithread application", **is returned by this driver**: the row
///   carries no `(DM)` marker, and a `describe_col` failure whose token reports signalled is
///   reclassified `HY008` in place of the backend's own SQLSTATE.
/// - HY010: Function sequence error (DM) (driver-manager-handled; not returned here).
/// - HY013: Memory management error; returned if underlying allocation fails.
/// - HY090: Invalid string or buffer length — every clause of this row is `(DM)`
///   (driver-manager-handled; not returned here). Unlike several of its siblings' rows,
///   this one has only the name-length-below-zero sentence and no maximum-length
///   sentence, so nothing in it is the driver's.
/// - HY117: Connection suspended (DM) (driver-manager-handled; not returned here).
/// - HYT01: Connection timeout expired; not applicable.
/// - IM001: Driver does not support this function (DM) (driver-manager-handled; not returned here).
/// - IM017: Polling disabled; not returned here (the asynchronous notification model is not
///   supported — not DM-annotated in the spec).
/// - IM018: SQLCompleteAsync not called; not returned here (the asynchronous notification model
///   is not supported — not DM-annotated in the spec).
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
/// Output pointers must be valid or null.
#[allow(clippy::too_many_arguments)]
pub unsafe fn sql_describe_col_w<B: Backend>(
    statement_handle: *mut c_void,
    column_number: u16,
    column_name: *mut u16,
    buffer_length: i16,
    name_length_ptr: *mut i16,
    data_type_ptr: *mut i16,
    column_size_ptr: *mut ULen,
    decimal_digits_ptr: *mut i16,
    nullable_ptr: *mut i16,
) -> SqlReturn {
    tracing::debug!(
        "SQLDescribeColW(stmt={:?}, col={})",
        statement_handle,
        column_number
    );
    // SAFETY: statement_handle is null or a valid StatementHandle<B>; kind and group
    // validated by scope.get inside the closure. Output pointers (data_type_ptr,
    // column_size_ptr, etc.) are checked for null before writing; caller guarantees they point
    // to writable locations if non-null.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, |scope| {
            let stmt = scope.get::<StatementHandle<B>>(statement_handle)?;
            stmt.diagnostics.clear();

            // This describes a result set an earlier execution produced, so it
            // observes *that* execution's token rather than minting one.
            // Resolved off the registry, which needs no borrow of `stmt`, and
            // taken here so it precedes the borrow below.
            let cancel_token = crate::handles::current_cancel_token(statement_handle);
            let cancel = cancel_token
                .as_ref()
                .map(crate::handles::cancel_as::<B>)
                .transpose()?;

            // Spec 07005: No result set.
            let Some(ref statement_data) = stmt.statement else {
                return Err(OdbcError::general(
                    "No result set available (statement not executed)",
                    SqlState::prepared_statement_not_cursor_spec(),
                ));
            };

            // 07009: Column 0 (bookmark) is normally DM-handled, but we reject it
            // defensively since this driver does not support bookmarks.
            if column_number == 0 {
                return Err(OdbcError::general(
                    "Column number 0 is not supported (bookmarks not implemented)",
                    SqlState::invalid_descriptor_index(),
                ));
            }

            // Spec 07009: Column number out of range. Core's own check, not the
            // backend's: the spec's `(DM)` marker covers only this row's
            // bookmark clause, leaving "greater than the number of columns in
            // the result set" to the driver, and core already knows the count.
            //
            // Doing it here is what lets the backend's error survive below. The
            // previous shape was `describe_col(...).map_err(|_| 07009)`, which
            // discarded the backend's error entirely and told the application
            // its column number was wrong whatever had actually failed — a
            // communication failure, a cancellation, anything.
            let column_count = statement_data.column_count();
            if column_number > column_count_upper_bound(column_count) {
                return Err(OdbcError::general(
                    format!(
                        "Column number {column_number} out of range (have {column_count} columns)"
                    ),
                    SqlState::invalid_descriptor_index(),
                ));
            }

            // Anything from here is a real failure, reported as the backend's
            // central error mapping classified it — `08S01` for a link failure,
            // `HY000` otherwise, both of which this function's diagnostics table
            // lists. `HY008` wins over either when the token says the statement
            // was cancelled; `_opt` because this reads a cursor an earlier
            // execution opened rather than minting a token, and `None` there
            // means no backend call has run yet.
            let desc = reclassify_cancelled_opt::<B, _, _>(
                statement_data.describe_col(column_number),
                cancel,
            )?;

            // Write data_type_ptr if pointer is non-null.
            //
            // SAFETY for the four output writes below: each pointer is non-null (checked) and the
            // caller guarantees it is a valid, writable output parameter, but alignment is not
            // guaranteed (row-wise binding may place it at an arbitrary offset), so use unaligned
            // writes. Already inside the enclosing `unsafe { panic_safe(...) }` block.
            if !data_type_ptr.is_null() {
                std::ptr::write_unaligned(data_type_ptr, desc.sql_type.0);
            }

            // Write column_size_ptr if pointer is non-null.
            if !column_size_ptr.is_null() {
                // `resolve_precision_ulen` reports 0 for a backend's
                // "undeterminable length" sentinel, which is what this
                // parameter's spec text requires (see its doc comment for why
                // this differs from SQL_DESC_LENGTH); every other value is the
                // widening u32 -> usize cast this always was, which cannot
                // truncate on 32- or 64-bit targets.
                std::ptr::write_unaligned(
                    column_size_ptr,
                    crate::types::resolve_precision_ulen(desc.precision),
                );
            }

            // Write decimal_digits_ptr if pointer is non-null.
            if !decimal_digits_ptr.is_null() {
                std::ptr::write_unaligned(decimal_digits_ptr, desc.scale);
            }

            // Write nullable_ptr if pointer is non-null.
            if !nullable_ptr.is_null() {
                // Written through as-is. While the descriptor carried a
                // `bool`, `SQL_NULLABLE_UNKNOWN` could not be reported at all
                // and a column whose nullability the backend could not
                // determine was announced as `SQL_NO_NULLS` — telling the
                // application it could skip a NULL check it actually needs.
                std::ptr::write_unaligned(nullable_ptr, desc.nullable as i16);
            }

            // Write column name via write_utf16.
            let name = desc.name.clone();
            Ok(crate::utf16::note_truncation(
                write_utf16(&name, column_name, buffer_length, name_length_ptr),
                &mut stmt.diagnostics,
            ))
        })
    };
    tracing::debug!("SQLDescribeColW -> {:?}", ret);
    ret
}

/// Generic implementation of SQLColAttributeW.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlcolattribute-function>
///
/// Returns a single descriptor field (attribute) for a column in the current result set.
/// Integer attributes are written to `numeric_attribute_ptr`; string attributes are written
/// to `character_attribute_ptr`.
///
/// # Parameters
///
/// - `statement_handle`: Statement handle.
/// - `column_number`: Column number (1-based for record fields). Column 0 is only valid for
///   header fields (e.g. `SQL_DESC_COUNT`); all other fields with column 0 return 07009.
/// - `field_identifier`: The descriptor field to retrieve (e.g. `SQL_DESC_NAME`,
///   `SQL_DESC_TYPE`, `SQL_DESC_COUNT`).
/// - `character_attribute_ptr` / `buffer_length` / `string_length_ptr`: Output buffer for
///   string attributes. `buffer_length` and `string_length_ptr` are both measured in bytes (the
///   spec requires `buffer_length` to be an even number for the W variant). If
///   `character_attribute_ptr` is NULL, `string_length_ptr` still returns the available length
///   in bytes.
/// - `numeric_attribute_ptr`: Output for numeric attributes. May be NULL. Applications should
///   initialize to 0 before calling — some drivers only write the low 32 bits.
///
/// # Spec compliance
///
/// - 01000: General warning (driver-specific informational message); not returned here.
/// - 01004: String data right truncated — returned via `write_utf16` if the character attribute
///   buffer is too small (returns `SQL_SUCCESS_WITH_INFO`).
/// - 07005: Prepared statement not a cursor-specification — returned if no result set is open
///   and `field_identifier` is not `SQL_DESC_COUNT`.
/// - 07009: Invalid descriptor index — spec marks the `column_number == 0` sub-case (DM); this
///   driver also checks it defensively since bookmarks are not supported. Returned by the driver
///   when `column_number` is greater than the number of columns in the result set, which the
///   row states without a marker,
///   which core checks against `StatementBackend::column_count` **before** calling
///   `describe_col`, so this state is returned only for the case its message describes.
/// - 08S01: Communication link failure — **absent from this function's diagnostics table**, yet
///   reachable and not filtered out. The page states that when called after `SQLPrepare` and
///   before `SQLExecute` this function "can return any SQLSTATE that can be returned by
///   SQLPrepare or SQLExecute", both of which list `08S01`. A `describe_col` failure the
///   driver's error mapping classified that way is therefore propagated unchanged, which is far
///   more use to an application than the `07009` it used to be overwritten with.
/// - HY000: General error; returned for any unexpected internal error, and propagated from
///   `describe_col` when the backend's mapping produced no more specific state.
/// - HY001: Memory allocation error; returned if allocation fails.
/// - HY008: Operation canceled. The asynchronous clause is inapplicable — core never returns
///   `SQL_STILL_EXECUTING` — but the second clause, `SQLCancel` called on the statement "from a
///   different thread in a multithread application", **is returned by this driver**: the row
///   carries no `(DM)` marker, and a `describe_col` failure whose token reports signalled is
///   reclassified `HY008` in place of the backend's own SQLSTATE.
/// - HY010: Function sequence error (DM) (driver-manager-handled; not returned here).
/// - HY013: Memory management error; returned if underlying allocation fails.
/// - HY090: Invalid string or buffer length — every clause of this row is `(DM)`
///   (driver-manager-handled; not returned here). Unlike several of its siblings' rows,
///   this one has only the name-length-below-zero sentence and no maximum-length
///   sentence, so nothing in it is the driver's.
/// - HY091: Invalid descriptor field identifier — `HYC00` ("driver not capable") is returned
///   instead, treating unrecognised field identifiers as unsupported extensions rather than invalid IDs.
/// - HY117: Connection suspended (DM) (driver-manager-handled; not returned here).
/// - HYC00: Driver not capable — returned for unrecognized `field_identifier` values.
/// - HYT01: Connection timeout expired; not applicable.
/// - IM001: Driver does not support this function (DM) (driver-manager-handled; not returned here).
/// - IM017: Polling disabled; not returned here (the asynchronous notification model is not
///   supported — not DM-annotated in the spec).
/// - IM018: SQLCompleteAsync not called; not returned here (the asynchronous notification model
///   is not supported — not DM-annotated in the spec).
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
/// Output pointers must be valid or null.
#[allow(clippy::too_many_arguments)]
pub unsafe fn sql_col_attribute_w<B: Backend>(
    statement_handle: *mut c_void,
    column_number: u16,
    field_identifier: u16,
    character_attribute_ptr: *mut c_void,
    buffer_length: i16,
    string_length_ptr: *mut i16,
    numeric_attribute_ptr: *mut isize,
) -> SqlReturn {
    // SAFETY: statement_handle is null or a valid StatementHandle<B>; kind and group
    // validated by scope.get inside the closure. Output pointers
    // (numeric_attribute_ptr, string_length_ptr, character_attribute_ptr) are checked for null
    // before writing; caller guarantees they point to writable locations if non-null.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, |scope| {
            let stmt = scope.get::<StatementHandle<B>>(statement_handle)?;
            stmt.diagnostics.clear();

            // As in `sql_describe_col_w`: this describes a result set an earlier
            // execution produced, so it observes that execution's token, taken
            // off the registry before any borrow of `stmt.statement`.
            let cancel_token = crate::handles::current_cancel_token(statement_handle);
            let cancel = cancel_token
                .as_ref()
                .map(crate::handles::cancel_as::<B>)
                .transpose()?;

            tracing::trace!(
                "SQLColAttributeW(stmt={:?}, col={}, field_identifier={})",
                statement_handle,
                column_number,
                field_identifier,
            );
            // Convert raw u16 to the strongly-typed Desc enum at the FFI boundary.
            let field = crate::types::desc_from_raw(field_identifier);
            tracing::debug!(
                "SQLColAttributeW(stmt={:?}, col={}, field_id={} ({:?}))",
                statement_handle,
                column_number,
                field_identifier,
                field
            );
            let field = field.ok_or_else(|| {
                OdbcError::general(
                    format!("Unknown descriptor field identifier: {field_identifier}"),
                    SqlState::optional_feature_not_implemented(),
                )
            })?;

            // SQL_DESC_COUNT is a header field describing the whole result set, not a
            // record field: column_number is ignored and it is answered even when no
            // result set is open (it reports 0 columns rather than erroring), so it must
            // be handled before the 07005 guard below.
            if field == Desc::Count {
                let column_count = stmt.statement.as_ref().map_or(0, |s| s.column_count());
                if !numeric_attribute_ptr.is_null() {
                    // SAFETY: numeric_attribute_ptr is non-null (checked above); caller
                    // guarantees it is a valid, writable isize output parameter, but
                    // alignment is not guaranteed (row-wise binding may place it at an
                    // arbitrary offset). Already inside the enclosing `unsafe` block.
                    std::ptr::write_unaligned(numeric_attribute_ptr, column_count as isize);
                }
                return Ok(SqlReturn::SUCCESS);
            }

            // Spec 07005: No result set (for record fields, which require a cursor).
            let Some(ref statement_data) = stmt.statement else {
                return Err(OdbcError::general(
                    "No result set available (statement not executed)",
                    SqlState::prepared_statement_not_cursor_spec(),
                ));
            };

            let column_count = statement_data.column_count();

            // Spec 07009: Column number must be >= 1 for record fields.
            if column_number == 0 {
                return Err(OdbcError::general(
                    "Column number 0 is not supported (bookmarks not implemented)",
                    SqlState::invalid_descriptor_index(),
                ));
            }

            // Spec 07009, core's own check — see `sql_describe_col_w`, which
            // carries the same pair of comments and the reasoning behind them.
            if column_number > column_count_upper_bound(column_count) {
                return Err(OdbcError::general(
                    format!(
                        "Column number {column_number} out of range (have {column_count} columns)"
                    ),
                    SqlState::invalid_descriptor_index(),
                ));
            }

            // The backend's own SQLSTATE from here, or `HY008` if the statement
            // was cancelled. This function's diagnostics table has no `08S01`
            // row, but its page states it "can return any SQLSTATE that can be
            // returned by SQLPrepare or SQLExecute" when called between the two,
            // so a link failure passing through is legal — and far more use to
            // an application than "column number out of range".
            let desc = reclassify_cancelled_opt::<B, _, _>(
                statement_data.describe_col(column_number),
                cancel,
            )?;

            let attr = get_column_attribute(&desc, column_count, field)?;

            match attr {
                ColAttrValue::String(s) => {
                    // Spec: BufferLength is in bytes and must be even for the W
                    // variant; StringLengthPtr is likewise "the total number of bytes".
                    // write_utf16 reports UTF-16 code units, so convert on the way out.
                    let buf_len_u16 = buffer_length / 2;
                    let mut units: i16 = 0;
                    let ret = crate::utf16::note_truncation(
                        write_utf16(
                            &s,
                            character_attribute_ptr as *mut u16,
                            buf_len_u16,
                            &mut units,
                        ),
                        &mut stmt.diagnostics,
                    );
                    if !string_length_ptr.is_null() {
                        let bytes = i16::try_from(i32::from(units) * 2).unwrap_or_else(|_| {
                            tracing::warn!(
                                "sql_col_attribute_w: byte length for {} code units overflows i16, saturating to i16::MAX",
                                units
                            );
                            i16::MAX
                        });
                        // SAFETY: string_length_ptr is non-null (checked above); caller
                        // guarantees it is a valid, writable i16 output parameter, but
                        // alignment is not guaranteed (row-wise binding may place it at
                        // an arbitrary offset), so use an unaligned write. Already inside
                        // the enclosing `unsafe { panic_safe(...) }` block.
                        std::ptr::write_unaligned(string_length_ptr, bytes);
                    }
                    Ok(ret)
                }
                ColAttrValue::Numeric(n) => {
                    if !numeric_attribute_ptr.is_null() {
                        // SAFETY: numeric_attribute_ptr is non-null (checked above); caller
                        // guarantees it is a valid, writable isize output parameter, but
                        // alignment is not guaranteed (row-wise binding may place it at an
                        // arbitrary offset). Already inside the enclosing `unsafe` block.
                        std::ptr::write_unaligned(numeric_attribute_ptr, n);
                    }
                    Ok(SqlReturn::SUCCESS)
                }
            }
        })
    };
    tracing::debug!("SQLColAttributeW -> {:?}", ret);
    ret
}

/// Column descriptors for the SQLProcedures result set (8 columns per spec).
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlprocedures-function>
///
/// `PROCEDURE_NAME` (3) is the only column the spec marks "not NULL". Columns
/// 4-6 are listed with data type "N/A" ("reserved for future use"); they are
/// reported as `SMALLINT`, which is what the ODBC 2.0 layout used and what
/// applications binding by column number expect.
/// `SQLProcedures`' spec sort order, as zero-based column indices.
///
/// The page reads "ordered by PROCEDURE_CAT, PROCEDURE_SCHEMA, and
/// PROCEDURE_NAME", but no `PROCEDURE_SCHEMA` column exists — the same page's
/// result-column table names column 2 `PROCEDURE_SCHEM`. The sentence has a
/// typo; column 2 is the key. Do not "correct" this into a lookup for a column
/// that does not exist.
const PROCEDURES_SORT_KEYS: [usize; 3] = [0, 1, 2];

pub(crate) fn procedures_columns(widths: &CatalogResultColumnWidths) -> Vec<ColumnDescriptor> {
    vec![
        identifier("PROCEDURE_CAT", widths, Nullable::SqlNullable),
        identifier("PROCEDURE_SCHEM", widths, Nullable::SqlNullable),
        identifier("PROCEDURE_NAME", widths, Nullable::SqlNoNulls),
        smallint("NUM_INPUT_PARAMS", Nullable::SqlNullable),
        smallint("NUM_OUTPUT_PARAMS", Nullable::SqlNullable),
        smallint("NUM_RESULT_SETS", Nullable::SqlNullable),
        character("REMARKS", widths.remarks_len, widths, Nullable::SqlNullable),
        smallint("PROCEDURE_TYPE", Nullable::SqlNullable),
    ]
}

/// Generic implementation of SQLProceduresW.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlprocedures-function>
///
/// Queries the backend for stored-procedure metadata and stores the result set
/// in the statement handle. A backend that leaves [`Backend::procedures`]
/// defaulted returns no rows, which is the spec's response for a data source
/// with no stored procedures.
///
/// # Parameters
///
/// - `statement_handle`: Statement handle.
/// - `catalog_name` / `name_length1`: Catalog name (NULL = no filter; no search patterns).
/// - `schema_name` / `name_length2`: Schema name search pattern (NULL = no filter).
/// - `proc_name` / `name_length3`: Procedure name search pattern (NULL = no filter).
///
/// # Spec compliance
///
/// - 01000: General warning (driver-specific informational message); not returned here.
/// - 08S01: Communication link failure; propagated from the backend when its
///   client library reports the link to the data source failed.
/// - 24000: Invalid cursor state — returned if a cursor is already open on this statement.
/// - 40001: Serialization failure — propagated from the backend unchanged, as `08S01` is.
///   Core degrades nothing to `HY000`; that state appears only when the backend's own error
///   mapping produced no more specific one.
/// - 40003: Statement completion unknown — propagated from the backend unchanged.
/// - HY000: General error; returned for any unexpected backend error.
/// - HY001: Memory allocation error; returned if allocation fails.
/// - HY008: Operation canceled. The row's first clause — asynchronous processing, then the
///   function called again — is not applicable: core implements no asynchronous execution and
///   never returns `SQL_STILL_EXECUTING`. The second clause, `SQLCancel` called on the
///   statement "from a different thread in a multithread application", **is returned by this
///   driver**: the row carries no `(DM)` marker, and when a backend call fails with
///   `Backend::is_cancelled` reporting its token signalled, core reports `HY008` in place of
///   the backend's own SQLSTATE.
/// - HY009: Invalid use of null pointer. **Returned by this driver** for the one clause the
///   spec's diagnostics table states *without* a `(DM)` marker: `SQL_ATTR_METADATA_ID` was
///   `SQL_TRUE`, `CatalogName` was a null pointer, and `SQL_CATALOG_NAME` reports that
///   catalog names are supported. The `SchemaName`/`ProcName` clause beside it *is*
///   `(DM)`-marked and is deliberately not checked here.
///
///   This page states **no** unconditional null-argument clause, so a null `ProcName` is
///   accepted — unlike `SQLColumnPrivileges`, whose `TableName` sentence carries no marker
///   and no `METADATA_ID` condition and *is* checked. See
///   `the_procedure_functions_do_not_check_null_name_arguments`.
/// - HY010: Function sequence error — returned if connection is not open. DM cases (async, etc.)
///   are driver-manager-handled; not returned here.
/// - HY013: Memory management error; returned if underlying allocation fails.
/// - HY090: Invalid string or buffer length — the row has two sentences and only the first
///   carries `(DM)`: a name length argument less than 0 but not equal to `SQL_NTS`. The
///   second is the driver's — a name length exceeding "the maximum length value for the
///   corresponding name" — and it cannot arise here, because core declares no maximum name
///   lengths. `SQL_MAX_CATALOG_NAME_LEN`, `SQL_MAX_SCHEMA_NAME_LEN`,
///   `SQL_MAX_TABLE_NAME_LEN` and `SQL_MAX_COLUMN_NAME_LEN` all answer `0`, which the
///   `SQLGetInfo` page defines as "no specified limit or the limit is unknown". A driver
///   that answers a real maximum for any of those has to add the check.
///
///   **Also returned here**, for a condition this row does not itself state: **any** of this
///   function's name arguments, passed as `SQL_NTS`, whose null terminator is not within
///   `MAX_NTS_SCAN` (32 767) code units. Every one of them is resolved by the same helper
///   (`parse_filter_param`), so the limit applies to all of them alike and the diagnostic
///   names which one overran. It is a length the driver cannot determine, which is what
///   `HY090` names; `utf16_to_string` used to hand back the 32 767-unit prefix instead, so
///   the call filtered on a truncated pattern and returned a result set that was wrong
///   rather than absent. An **explicitly measured** length is not limited by this, at any
///   size. See `tables_refuses_an_nts_filter_that_runs_to_the_scan_cap` and
///   `foreign_keys_names_the_argument_whose_nts_scan_overran`.
/// - HY117: Connection suspended (DM) (driver-manager-handled; not returned here).
/// - HYC00: Optional feature not implemented; propagated from backend if catalogs/schemas or
///   search patterns are unsupported.
/// - HYT00: Timeout expired — **returned by this driver**, not merely propagated. A backend
///   that answered `QueryTimeout::CoreCancels` gets core's own timer (`crate::query_timer`),
///   armed over this call, and `QueryTimer::reclassify` relabels the failing call `HYT00`
///   when the deadline fired. A backend enforcing its own timeout has its `HYT00` propagated
///   unchanged.
/// - HYT01: Connection timeout expired — propagated from the backend unchanged.
/// - IM001: Driver does not support this function (DM) (driver-manager-handled; not returned here).
/// - IM017: Polling disabled; not returned here (the asynchronous notification model is not
///   supported — not DM-annotated in the spec).
/// - IM018: SQLCompleteAsync not called; not returned here (the asynchronous notification model
///   is not supported — not DM-annotated in the spec).
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
pub unsafe fn sql_procedures_w<B: Backend>(
    statement_handle: *mut c_void,
    catalog_name: *const u16,
    name_length1: i16,
    schema_name: *const u16,
    name_length2: i16,
    proc_name: *const u16,
    name_length3: i16,
) -> SqlReturn {
    tracing::trace!("SQLProceduresW(stmt={:?})", statement_handle);
    // SAFETY: statement_handle is null or a valid StatementHandle<B>; kind and group
    // validated by scope.stmt_with_parent inside the closure.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, |scope| {
            let (stmt, conn) = scope.stmt_with_parent::<B>(statement_handle)?;
            stmt.diagnostics.clear();

            if stmt.cursor_open {
                return Err(OdbcError::general(
                    "A cursor is already open on this statement",
                    SqlState::invalid_cursor_state(),
                ));
            }

            // Manual-commit mode: this call opens a transaction (or extends
            // the open one), which is what SQL_ATTR_TXN_ISOLATION's HY011 is
            // about. Recorded before the backend call, not after it succeeds:
            // a call that fails partway may still have opened one.
            conn.note_work_started();

            let Some(ref connection) = conn.connection else {
                return Err(OdbcError::general(
                    "Connection is not open",
                    SqlState::function_sequence_error(),
                ));
            };

            let catalog = parse_filter_param(catalog_name, name_length1, "CatalogName")?;
            let schema = parse_filter_param(schema_name, name_length2, "SchemaName")?;
            let proc = parse_filter_param(proc_name, name_length3, "ProcName")?;
            tracing::debug!(
                "SQLProceduresW(stmt={:?}, catalog={:?}, schema={:?}, proc={:?})",
                statement_handle,
                catalog,
                schema,
                proc,
            );

            // Spec HY009 (not (DM)): METADATA_ID plus a null CatalogName on a
            // data source that has catalogs. The neighbouring
            // SchemaName/ProcName clause *is* (DM)-marked, and this page states
            // no unconditional null-argument clause — see the doc comment.
            let metadata_id = metadata_id_enabled(stmt);
            check_metadata_id_null_catalog::<B>(
                connection,
                catalog_name,
                metadata_id,
                "SQLProceduresW",
            )?;

            // The token exists once this statement makes its first
            // backend call; created here on demand, then reused for every
            // later call on the same statement (see `mint_cancel_token`).
            let cancel_token = crate::handles::mint_cancel_token::<B>(statement_handle, connection);
            let cancel = crate::handles::cancel_as::<B>(&cancel_token)?;
            // Core-enforced deadline, if the backend asked core to own one.
            // Disarmed by `Drop` the moment this scope ends, so a fast call
            // leaves no thread behind.
            let timer =
                crate::query_timer::QueryTimer::arm::<B>(stmt.core_query_timeout, &cancel_token);

            // All three arguments are identifiers under METADATA_ID; this
            // family has no `TableType`-style exemption.
            let catalog = normalise_catalog_arg::<B>(connection, catalog, metadata_id);
            let schema = normalise_catalog_arg::<B>(connection, schema, metadata_id);
            let proc = normalise_catalog_arg::<B>(connection, proc, metadata_id);

            let query = crate::types::ProceduresQuery::default()
                .with_catalog(catalog.as_deref())
                .with_schema(schema.as_deref())
                .with_proc_name(proc.as_deref());
            let rows = B::procedures(connection, cancel, &query);
            let rows = timer.check::<B, _, _>(rows, cancel)?;

            let mut values: Vec<Vec<ColumnValue>> =
                rows.iter().map(ProcedureRow::to_values).collect();
            crate::catalog_sort::sort_rows(
                &mut values,
                &PROCEDURES_SORT_KEYS,
                B::null_collation(connection),
            );
            stmt.set_result_set(StatementData::Synthetic(SyntheticStatement::new(
                procedures_columns(&B::catalog_result_column_widths()),
                values,
            )));
            Ok(SqlReturn::SUCCESS)
        })
    };
    tracing::debug!("SQLProceduresW -> {:?}", ret);
    ret
}

/// Column descriptors for the SQLProcedureColumns result set (19 columns per spec).
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlprocedurecolumns-function>
///
/// "not NULL" per the spec's column table: `PROCEDURE_NAME` (3),
/// `COLUMN_NAME` (4), `COLUMN_TYPE` (5), `DATA_TYPE` (6), `TYPE_NAME` (7),
/// `NULLABLE` (12), `SQL_DATA_TYPE` (15) and `ORDINAL_POSITION` (18).
/// `SQLProcedureColumns`' spec sort order — "ordered by PROCEDURE_CAT,
/// PROCEDURE_SCHEM, PROCEDURE_NAME, and COLUMN_TYPE", as zero-based column
/// indices.
const PROCEDURE_COLUMNS_SORT_KEYS: [usize; 4] = [0, 1, 2, 4];

pub(crate) fn procedure_columns_columns(
    widths: &CatalogResultColumnWidths,
) -> Vec<ColumnDescriptor> {
    vec![
        identifier("PROCEDURE_CAT", widths, Nullable::SqlNullable),
        identifier("PROCEDURE_SCHEM", widths, Nullable::SqlNullable),
        identifier("PROCEDURE_NAME", widths, Nullable::SqlNoNulls),
        identifier("COLUMN_NAME", widths, Nullable::SqlNoNulls),
        smallint("COLUMN_TYPE", Nullable::SqlNoNulls),
        smallint("DATA_TYPE", Nullable::SqlNoNulls),
        identifier("TYPE_NAME", widths, Nullable::SqlNoNulls),
        integer("COLUMN_SIZE", Nullable::SqlNullable),
        integer("BUFFER_LENGTH", Nullable::SqlNullable),
        smallint("DECIMAL_DIGITS", Nullable::SqlNullable),
        smallint("NUM_PREC_RADIX", Nullable::SqlNullable),
        smallint("NULLABLE", Nullable::SqlNoNulls),
        character("REMARKS", widths.remarks_len, widths, Nullable::SqlNullable),
        character(
            "COLUMN_DEF",
            widths.remarks_len,
            widths,
            Nullable::SqlNullable,
        ),
        smallint("SQL_DATA_TYPE", Nullable::SqlNoNulls),
        smallint("SQL_DATETIME_SUB", Nullable::SqlNullable),
        integer("CHAR_OCTET_LENGTH", Nullable::SqlNullable),
        integer("ORDINAL_POSITION", Nullable::SqlNoNulls),
        character("IS_NULLABLE", YES_NO_LEN, widths, Nullable::SqlNullable),
    ]
}

/// Generic implementation of SQLProcedureColumnsW.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlprocedurecolumns-function>
///
/// Queries the backend for stored-procedure parameter and result-column
/// metadata and stores the result set in the statement handle. A backend that
/// leaves [`Backend::procedure_columns`] defaulted returns no rows.
///
/// # Parameters
///
/// - `statement_handle`: Statement handle.
/// - `catalog_name` / `name_length1`: Catalog name (NULL = no filter; no search patterns).
/// - `schema_name` / `name_length2`: Schema name search pattern (NULL = no filter).
/// - `proc_name` / `name_length3`: Procedure name search pattern (NULL = no filter).
/// - `column_name` / `name_length4`: Column name search pattern (NULL = no filter).
///
/// # Spec compliance
///
/// - 01000: General warning (driver-specific informational message); not returned here.
/// - 08S01: Communication link failure; propagated from the backend when its
///   client library reports the link to the data source failed.
/// - 24000: Invalid cursor state — returned if a cursor is already open on this statement.
/// - 40001: Serialization failure — propagated from the backend unchanged, as `08S01` is.
///   Core degrades nothing to `HY000`; that state appears only when the backend's own error
///   mapping produced no more specific one.
/// - 40003: Statement completion unknown — propagated from the backend unchanged.
/// - HY000: General error; returned for any unexpected backend error.
/// - HY001: Memory allocation error; returned if allocation fails.
/// - HY008: Operation canceled. The row's first clause — asynchronous processing, then the
///   function called again — is not applicable: core implements no asynchronous execution and
///   never returns `SQL_STILL_EXECUTING`. The second clause, `SQLCancel` called on the
///   statement "from a different thread in a multithread application", **is returned by this
///   driver**: the row carries no `(DM)` marker, and when a backend call fails with
///   `Backend::is_cancelled` reporting its token signalled, core reports `HY008` in place of
///   the backend's own SQLSTATE.
/// - HY009: Invalid use of null pointer. **Returned by this driver** for the one clause the
///   spec's diagnostics table states *without* a `(DM)` marker: `SQL_ATTR_METADATA_ID` was
///   `SQL_TRUE`, `CatalogName` was a null pointer, and `SQL_CATALOG_NAME` reports that
///   catalog names are supported. The `SchemaName`/`ProcName`/`ColumnName` clause beside it
///   *is* `(DM)`-marked and is deliberately not checked here.
///
///   This page states **no** unconditional null-argument clause, so a null `ProcName` or
///   `ColumnName` is accepted — unlike `SQLColumnPrivileges`, whose `TableName` sentence
///   carries no marker and no `METADATA_ID` condition and *is* checked. See
///   `the_procedure_functions_do_not_check_null_name_arguments`.
/// - HY010: Function sequence error — returned if connection is not open. DM cases (async, etc.)
///   are driver-manager-handled; not returned here.
/// - HY013: Memory management error — **absent from this function's diagnostics table**,
///   which is the one difference from its eleven siblings' tables. Core would report it the
///   same way regardless, if an underlying allocation failed.
/// - HY090: Invalid string or buffer length — the row has two sentences and only the first
///   carries `(DM)`: a name length argument less than 0 but not equal to `SQL_NTS`. The
///   second is the driver's — a name length exceeding "the maximum length value for the
///   corresponding name" — and it cannot arise here, because core declares no maximum name
///   lengths. `SQL_MAX_CATALOG_NAME_LEN`, `SQL_MAX_SCHEMA_NAME_LEN`,
///   `SQL_MAX_TABLE_NAME_LEN` and `SQL_MAX_COLUMN_NAME_LEN` all answer `0`, which the
///   `SQLGetInfo` page defines as "no specified limit or the limit is unknown". A driver
///   that answers a real maximum for any of those has to add the check.
///
///   **Also returned here**, for a condition this row does not itself state: **any** of this
///   function's name arguments, passed as `SQL_NTS`, whose null terminator is not within
///   `MAX_NTS_SCAN` (32 767) code units. Every one of them is resolved by the same helper
///   (`parse_filter_param`), so the limit applies to all of them alike and the diagnostic
///   names which one overran. It is a length the driver cannot determine, which is what
///   `HY090` names; `utf16_to_string` used to hand back the 32 767-unit prefix instead, so
///   the call filtered on a truncated pattern and returned a result set that was wrong
///   rather than absent. An **explicitly measured** length is not limited by this, at any
///   size. See `tables_refuses_an_nts_filter_that_runs_to_the_scan_cap` and
///   `foreign_keys_names_the_argument_whose_nts_scan_overran`.
/// - HY117: Connection suspended (DM) (driver-manager-handled; not returned here).
/// - HYC00: Optional feature not implemented; propagated from backend if catalogs/schemas or
///   search patterns are unsupported.
/// - HYT00: Timeout expired — **returned by this driver**, not merely propagated. A backend
///   that answered `QueryTimeout::CoreCancels` gets core's own timer (`crate::query_timer`),
///   armed over this call, and `QueryTimer::reclassify` relabels the failing call `HYT00`
///   when the deadline fired. A backend enforcing its own timeout has its `HYT00` propagated
///   unchanged.
/// - HYT01: Connection timeout expired — propagated from the backend unchanged.
/// - IM001: Driver does not support this function (DM) (driver-manager-handled; not returned here).
/// - IM017: Polling disabled; not returned here (the asynchronous notification model is not
///   supported — not DM-annotated in the spec).
/// - IM018: SQLCompleteAsync not called; not returned here (the asynchronous notification model
///   is not supported — not DM-annotated in the spec).
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
#[allow(clippy::too_many_arguments)]
pub unsafe fn sql_procedure_columns_w<B: Backend>(
    statement_handle: *mut c_void,
    catalog_name: *const u16,
    name_length1: i16,
    schema_name: *const u16,
    name_length2: i16,
    proc_name: *const u16,
    name_length3: i16,
    column_name: *const u16,
    name_length4: i16,
) -> SqlReturn {
    tracing::trace!("SQLProcedureColumnsW(stmt={:?})", statement_handle);
    // SAFETY: statement_handle is null or a valid StatementHandle<B>; kind and group
    // validated by scope.stmt_with_parent inside the closure.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, |scope| {
            let (stmt, conn) = scope.stmt_with_parent::<B>(statement_handle)?;
            stmt.diagnostics.clear();

            if stmt.cursor_open {
                return Err(OdbcError::general(
                    "A cursor is already open on this statement",
                    SqlState::invalid_cursor_state(),
                ));
            }

            // Manual-commit mode: this call opens a transaction (or extends
            // the open one), which is what SQL_ATTR_TXN_ISOLATION's HY011 is
            // about. Recorded before the backend call, not after it succeeds:
            // a call that fails partway may still have opened one.
            conn.note_work_started();

            let Some(ref connection) = conn.connection else {
                return Err(OdbcError::general(
                    "Connection is not open",
                    SqlState::function_sequence_error(),
                ));
            };

            let catalog = parse_filter_param(catalog_name, name_length1, "CatalogName")?;
            let schema = parse_filter_param(schema_name, name_length2, "SchemaName")?;
            let proc = parse_filter_param(proc_name, name_length3, "ProcName")?;
            let column = parse_filter_param(column_name, name_length4, "ColumnName")?;
            tracing::debug!(
                "SQLProcedureColumnsW(stmt={:?}, catalog={:?}, schema={:?}, proc={:?}, \
                 column={:?})",
                statement_handle,
                catalog,
                schema,
                proc,
                column,
            );

            // Spec HY009 (not (DM)): METADATA_ID plus a null CatalogName on a
            // data source that has catalogs. The neighbouring
            // SchemaName/ProcName/ColumnName clause *is* (DM)-marked, and this
            // page states no unconditional null-argument clause — see the doc
            // comment.
            let metadata_id = metadata_id_enabled(stmt);
            check_metadata_id_null_catalog::<B>(
                connection,
                catalog_name,
                metadata_id,
                "SQLProcedureColumnsW",
            )?;

            let cancel_token = crate::handles::mint_cancel_token::<B>(statement_handle, connection);
            let cancel = crate::handles::cancel_as::<B>(&cancel_token)?;
            // Core-enforced deadline, if the backend asked core to own one.
            // Disarmed by `Drop` the moment this scope ends, so a fast call
            // leaves no thread behind.
            let timer =
                crate::query_timer::QueryTimer::arm::<B>(stmt.core_query_timeout, &cancel_token);

            // All four arguments are identifiers under METADATA_ID.
            let catalog = normalise_catalog_arg::<B>(connection, catalog, metadata_id);
            let schema = normalise_catalog_arg::<B>(connection, schema, metadata_id);
            let proc = normalise_catalog_arg::<B>(connection, proc, metadata_id);
            let column = normalise_catalog_arg::<B>(connection, column, metadata_id);

            let query = crate::types::ProcedureColumnsQuery::default()
                .with_catalog(catalog.as_deref())
                .with_schema(schema.as_deref())
                .with_proc_name(proc.as_deref())
                .with_column(column.as_deref());
            let rows = B::procedure_columns(connection, cancel, &query);
            let rows = timer.check::<B, _, _>(rows, cancel)?;

            let mut values: Vec<Vec<ColumnValue>> =
                rows.iter().map(ProcedureColumnRow::to_values).collect();
            crate::catalog_sort::sort_rows(
                &mut values,
                &PROCEDURE_COLUMNS_SORT_KEYS,
                B::null_collation(connection),
            );
            stmt.set_result_set(StatementData::Synthetic(SyntheticStatement::new(
                procedure_columns_columns(&B::catalog_result_column_widths()),
                values,
            )));
            Ok(SqlReturn::SUCCESS)
        })
    };
    tracing::debug!("SQLProcedureColumnsW -> {:?}", ret);
    ret
}

/// Column descriptors for the SQLColumnPrivileges result set (8 columns per spec).
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlcolumnprivileges-function>
///
/// "not NULL" per the spec's column table: `TABLE_NAME` (3), `COLUMN_NAME`
/// (4), `GRANTEE` (6) and `PRIVILEGE` (7).
/// `SQLColumnPrivileges`' spec sort order — "ordered by TABLE_CAT,
/// TABLE_SCHEM, TABLE_NAME, COLUMN_NAME, and PRIVILEGE", as zero-based column
/// indices.
const COLUMN_PRIVILEGES_SORT_KEYS: [usize; 5] = [0, 1, 2, 3, 6];

pub(crate) fn column_privileges_columns(
    widths: &CatalogResultColumnWidths,
) -> Vec<ColumnDescriptor> {
    vec![
        identifier("TABLE_CAT", widths, Nullable::SqlNullable),
        identifier("TABLE_SCHEM", widths, Nullable::SqlNullable),
        identifier("TABLE_NAME", widths, Nullable::SqlNoNulls),
        identifier("COLUMN_NAME", widths, Nullable::SqlNoNulls),
        identifier("GRANTOR", widths, Nullable::SqlNullable),
        identifier("GRANTEE", widths, Nullable::SqlNoNulls),
        character("PRIVILEGE", PRIVILEGE_LEN, widths, Nullable::SqlNoNulls),
        character("IS_GRANTABLE", YES_NO_LEN, widths, Nullable::SqlNullable),
    ]
}

/// Generic implementation of SQLColumnPrivilegesW.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlcolumnprivileges-function>
///
/// Queries the backend for column-level privileges and stores the result set in
/// the statement handle. A backend that leaves [`Backend::column_privileges`]
/// defaulted returns no rows.
///
/// # Parameters
///
/// - `statement_handle`: Statement handle.
/// - `catalog_name` / `name_length1`: Catalog name (NULL = no filter; no search patterns).
/// - `schema_name` / `name_length2`: Schema name (NULL = no filter; no search patterns).
/// - `table_name` / `name_length3`: Table name (required; cannot be NULL; no search patterns).
/// - `column_name` / `name_length4`: Column name search pattern (NULL = no filter).
///
/// # Spec compliance
///
/// - 01000: General warning (driver-specific informational message); not returned here.
/// - 08S01: Communication link failure; propagated from the backend when its
///   client library reports the link to the data source failed.
/// - 24000: Invalid cursor state — returned if a cursor is already open on this statement.
/// - 40001: Serialization failure — propagated from the backend unchanged, as `08S01` is.
///   Core degrades nothing to `HY000`; that state appears only when the backend's own error
///   mapping produced no more specific one.
/// - 40003: Statement completion unknown — propagated from the backend unchanged.
/// - HY000: General error; returned for any unexpected backend error.
/// - HY001: Memory allocation error; returned if allocation fails.
/// - HY008: Operation canceled. The row's first clause — asynchronous processing, then the
///   function called again — is not applicable: core implements no asynchronous execution and
///   never returns `SQL_STILL_EXECUTING`. The second clause, `SQLCancel` called on the
///   statement "from a different thread in a multithread application", **is returned by this
///   driver**: the row carries no `(DM)` marker, and when a backend call fails with
///   `Backend::is_cancelled` reporting its token signalled, core reports `HY008` in place of
///   the backend's own SQLSTATE.
/// - HY009: Invalid use of null pointer. **Returned by this driver** for *both* clauses the
///   spec's diagnostics table states without a `(DM)` marker — this is the only one of the
///   four functions in this family with two:
///
///   1. "The `TableName` argument was a null pointer", **unconditionally**. The sentence
///      carries no marker and no `METADATA_ID` condition, and the page's argument
///      description agrees that `TableName` "cannot be a null pointer".
///      `SQLTablePrivileges`, `SQLProcedures` and `SQLProcedureColumns` state no such
///      unmarked sentence and deliberately do not check it — see
///      `table_privileges_does_not_check_null_table_name` and
///      `the_procedure_functions_do_not_check_null_name_arguments`.
///   2. `SQL_ATTR_METADATA_ID` was `SQL_TRUE`, `CatalogName` was a null pointer, and
///      `SQL_CATALOG_NAME` reports that catalog names are supported.
///
///   The `SchemaName`/`ColumnName` clause beside them *is* `(DM)`-marked and is not checked.
/// - HY010: Function sequence error — returned if connection is not open. DM cases (async, etc.)
///   are driver-manager-handled; not returned here.
/// - HY013: Memory management error; returned if underlying allocation fails.
/// - HY090: Invalid string or buffer length — every clause of this row is `(DM)`
///   (driver-manager-handled), so none of the row's own clauses is returned here. Unlike
///   several of its siblings' rows, this one has only the name-length-below-zero sentence
///   and no maximum-length sentence, so nothing in it is the driver's.
///
///   **Also returned here**, for a condition this row does not itself state: **any** of this
///   function's name arguments, passed as `SQL_NTS`, whose null terminator is not within
///   `MAX_NTS_SCAN` (32 767) code units. Every one of them is resolved by the same helper
///   (`parse_filter_param`), so the limit applies to all of them alike and the diagnostic
///   names which one overran. It is a length the driver cannot determine, which is what
///   `HY090` names; `utf16_to_string` used to hand back the 32 767-unit prefix instead, so
///   the call filtered on a truncated pattern and returned a result set that was wrong
///   rather than absent. An **explicitly measured** length is not limited by this, at any
///   size. See `tables_refuses_an_nts_filter_that_runs_to_the_scan_cap` and
///   `foreign_keys_names_the_argument_whose_nts_scan_overran`.
/// - HY117: Connection suspended (DM) (driver-manager-handled; not returned here).
/// - HYC00: Optional feature not implemented; propagated from backend if catalogs/schemas or
///   search patterns are unsupported.
/// - HYT00: Timeout expired — **returned by this driver**, not merely propagated. A backend
///   that answered `QueryTimeout::CoreCancels` gets core's own timer (`crate::query_timer`),
///   armed over this call, and `QueryTimer::reclassify` relabels the failing call `HYT00`
///   when the deadline fired. A backend enforcing its own timeout has its `HYT00` propagated
///   unchanged.
/// - HYT01: Connection timeout expired — propagated from the backend unchanged.
/// - IM001: Driver does not support this function (DM) (driver-manager-handled; not returned here).
/// - IM017: Polling disabled; not returned here (the asynchronous notification model is not
///   supported — not DM-annotated in the spec).
/// - IM018: SQLCompleteAsync not called; not returned here (the asynchronous notification model
///   is not supported — not DM-annotated in the spec).
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
#[allow(clippy::too_many_arguments)]
pub unsafe fn sql_column_privileges_w<B: Backend>(
    statement_handle: *mut c_void,
    catalog_name: *const u16,
    name_length1: i16,
    schema_name: *const u16,
    name_length2: i16,
    table_name: *const u16,
    name_length3: i16,
    column_name: *const u16,
    name_length4: i16,
) -> SqlReturn {
    tracing::trace!("SQLColumnPrivilegesW(stmt={:?})", statement_handle);
    // SAFETY: statement_handle is null or a valid StatementHandle<B>; kind and group
    // validated by scope.stmt_with_parent inside the closure.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, |scope| {
            let (stmt, conn) = scope.stmt_with_parent::<B>(statement_handle)?;
            stmt.diagnostics.clear();

            if stmt.cursor_open {
                return Err(OdbcError::general(
                    "A cursor is already open on this statement",
                    SqlState::invalid_cursor_state(),
                ));
            }

            // Manual-commit mode: this call opens a transaction (or extends
            // the open one), which is what SQL_ATTR_TXN_ISOLATION's HY011 is
            // about. Recorded before the backend call, not after it succeeds:
            // a call that fails partway may still have opened one.
            conn.note_work_started();

            let Some(ref connection) = conn.connection else {
                return Err(OdbcError::general(
                    "Connection is not open",
                    SqlState::function_sequence_error(),
                ));
            };

            // Spec HY009 (not (DM), and not conditional on METADATA_ID) — the
            // clause only this one of the four states. Checked before the
            // arguments are parsed, because a null pointer is exactly what
            // parsing would turn into a `None` and lose. See the doc comment.
            check_null_table_name(table_name, "SQLColumnPrivilegesW")?;

            let catalog = parse_filter_param(catalog_name, name_length1, "CatalogName")?;
            let schema = parse_filter_param(schema_name, name_length2, "SchemaName")?;
            let table = parse_filter_param(table_name, name_length3, "TableName")?;
            let column = parse_filter_param(column_name, name_length4, "ColumnName")?;
            tracing::debug!(
                "SQLColumnPrivilegesW(stmt={:?}, catalog={:?}, schema={:?}, table={:?}, \
                 column={:?})",
                statement_handle,
                catalog,
                schema,
                table,
                column,
            );

            // Spec HY009 (not (DM)): METADATA_ID plus a null CatalogName on a
            // data source that has catalogs. The neighbouring
            // SchemaName/ColumnName clause *is* (DM)-marked.
            let metadata_id = metadata_id_enabled(stmt);
            check_metadata_id_null_catalog::<B>(
                connection,
                catalog_name,
                metadata_id,
                "SQLColumnPrivilegesW",
            )?;

            let cancel_token = crate::handles::mint_cancel_token::<B>(statement_handle, connection);
            let cancel = crate::handles::cancel_as::<B>(&cancel_token)?;
            // Core-enforced deadline, if the backend asked core to own one.
            // Disarmed by `Drop` the moment this scope ends, so a fast call
            // leaves no thread behind.
            let timer =
                crate::query_timer::QueryTimer::arm::<B>(stmt.core_query_timeout, &cancel_token);

            // All four arguments are identifiers under METADATA_ID.
            let catalog = normalise_catalog_arg::<B>(connection, catalog, metadata_id);
            let schema = normalise_catalog_arg::<B>(connection, schema, metadata_id);
            let table = normalise_catalog_arg::<B>(connection, table, metadata_id);
            let column = normalise_catalog_arg::<B>(connection, column, metadata_id);

            let query = crate::types::ColumnPrivilegesQuery::default()
                .with_catalog(catalog.as_deref())
                .with_schema(schema.as_deref())
                .with_table(table.as_deref())
                .with_column(column.as_deref());
            let rows = B::column_privileges(connection, cancel, &query);
            let rows = timer.check::<B, _, _>(rows, cancel)?;

            let mut values: Vec<Vec<ColumnValue>> =
                rows.iter().map(ColumnPrivilegeRow::to_values).collect();
            crate::catalog_sort::sort_rows(
                &mut values,
                &COLUMN_PRIVILEGES_SORT_KEYS,
                B::null_collation(connection),
            );
            stmt.set_result_set(StatementData::Synthetic(SyntheticStatement::new(
                column_privileges_columns(&B::catalog_result_column_widths()),
                values,
            )));
            Ok(SqlReturn::SUCCESS)
        })
    };
    tracing::debug!("SQLColumnPrivilegesW -> {:?}", ret);
    ret
}

/// Column descriptors for the SQLTablePrivileges result set (7 columns per spec).
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqltableprivileges-function>
///
/// "not NULL" per the spec's column table: `TABLE_NAME` (3), `GRANTEE` (5)
/// and `PRIVILEGE` (6).
/// `SQLTablePrivileges`' spec sort order — "ordered by TABLE_CAT,
/// TABLE_SCHEM, TABLE_NAME, PRIVILEGE, and GRANTEE", as zero-based column
/// indices.
///
/// The last two are **not** in ascending index order: `PRIVILEGE` is column 6
/// and `GRANTEE` column 5, and the spec sorts by `PRIVILEGE` first. That is
/// this function's order, and the opposite of `SQLColumnPrivileges`', which
/// ends `COLUMN_NAME, PRIVILEGE` with no `GRANTEE` key at all.
const TABLE_PRIVILEGES_SORT_KEYS: [usize; 5] = [0, 1, 2, 5, 4];

pub(crate) fn table_privileges_columns(
    widths: &CatalogResultColumnWidths,
) -> Vec<ColumnDescriptor> {
    vec![
        identifier("TABLE_CAT", widths, Nullable::SqlNullable),
        identifier("TABLE_SCHEM", widths, Nullable::SqlNullable),
        identifier("TABLE_NAME", widths, Nullable::SqlNoNulls),
        identifier("GRANTOR", widths, Nullable::SqlNullable),
        identifier("GRANTEE", widths, Nullable::SqlNoNulls),
        character("PRIVILEGE", PRIVILEGE_LEN, widths, Nullable::SqlNoNulls),
        character("IS_GRANTABLE", YES_NO_LEN, widths, Nullable::SqlNullable),
    ]
}

/// Generic implementation of SQLTablePrivilegesW.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqltableprivileges-function>
///
/// Queries the backend for table-level privileges and stores the result set in
/// the statement handle. A backend that leaves [`Backend::table_privileges`]
/// defaulted returns no rows.
///
/// # Parameters
///
/// - `statement_handle`: Statement handle.
/// - `catalog_name` / `name_length1`: Catalog name (NULL = no filter; no search patterns).
/// - `schema_name` / `name_length2`: Schema name search pattern (NULL = no filter).
/// - `table_name` / `name_length3`: Table name search pattern (NULL = no filter).
///
/// # Spec compliance
///
/// - 01000: General warning (driver-specific informational message); not returned here.
/// - 08S01: Communication link failure; propagated from the backend when its
///   client library reports the link to the data source failed.
/// - 24000: Invalid cursor state — returned if a cursor is already open on this statement.
/// - 40001: Serialization failure — propagated from the backend unchanged, as `08S01` is.
///   Core degrades nothing to `HY000`; that state appears only when the backend's own error
///   mapping produced no more specific one.
/// - 40003: Statement completion unknown — propagated from the backend unchanged.
/// - HY000: General error; returned for any unexpected backend error.
/// - HY001: Memory allocation error; returned if allocation fails.
/// - HY008: Operation canceled. The row's first clause — asynchronous processing, then the
///   function called again — is not applicable: core implements no asynchronous execution and
///   never returns `SQL_STILL_EXECUTING`. The second clause, `SQLCancel` called on the
///   statement "from a different thread in a multithread application", **is returned by this
///   driver**: the row carries no `(DM)` marker, and when a backend call fails with
///   `Backend::is_cancelled` reporting its token signalled, core reports `HY008` in place of
///   the backend's own SQLSTATE.
/// - HY009: Invalid use of null pointer. **Returned by this driver** for the one clause the
///   spec's diagnostics table states *without* a `(DM)` marker: `SQL_ATTR_METADATA_ID` was
///   `SQL_TRUE`, `CatalogName` was a null pointer, and `SQL_CATALOG_NAME` reports that
///   catalog names are supported.
///
///   The `SchemaName`/`TableName` clause beside it *is* `(DM)`-marked, so the Driver Manager
///   returns it and this driver deliberately does **not** — including for a null `TableName`,
///   which this page states only under that marked, `METADATA_ID`-conditional sentence.
///   `SQLColumnPrivileges` carries an *unmarked and unconditional* null-`TableName` sentence
///   and does check it; the difference between the two is intentional, not an omission here.
///   See `table_privileges_does_not_check_null_table_name`.
/// - HY010: Function sequence error — returned if connection is not open. DM cases (async, etc.)
///   are driver-manager-handled; not returned here.
/// - HY013: Memory management error; returned if underlying allocation fails.
/// - HY090: Invalid string or buffer length — the row has two sentences and only the first
///   carries `(DM)`: a name length argument less than 0 but not equal to `SQL_NTS`. The
///   second is the driver's — a name length exceeding "the maximum length value for the
///   corresponding name" — and it cannot arise here, because core declares no maximum name
///   lengths. `SQL_MAX_CATALOG_NAME_LEN`, `SQL_MAX_SCHEMA_NAME_LEN`,
///   `SQL_MAX_TABLE_NAME_LEN` and `SQL_MAX_COLUMN_NAME_LEN` all answer `0`, which the
///   `SQLGetInfo` page defines as "no specified limit or the limit is unknown". A driver
///   that answers a real maximum for any of those has to add the check.
///
///   **Also returned here**, for a condition this row does not itself state: **any** of this
///   function's name arguments, passed as `SQL_NTS`, whose null terminator is not within
///   `MAX_NTS_SCAN` (32 767) code units. Every one of them is resolved by the same helper
///   (`parse_filter_param`), so the limit applies to all of them alike and the diagnostic
///   names which one overran. It is a length the driver cannot determine, which is what
///   `HY090` names; `utf16_to_string` used to hand back the 32 767-unit prefix instead, so
///   the call filtered on a truncated pattern and returned a result set that was wrong
///   rather than absent. An **explicitly measured** length is not limited by this, at any
///   size. See `tables_refuses_an_nts_filter_that_runs_to_the_scan_cap` and
///   `foreign_keys_names_the_argument_whose_nts_scan_overran`.
/// - HY117: Connection suspended (DM) (driver-manager-handled; not returned here).
/// - HYC00: Optional feature not implemented; propagated from backend if catalogs/schemas or
///   search patterns are unsupported.
/// - HYT00: Timeout expired — **returned by this driver**, not merely propagated. A backend
///   that answered `QueryTimeout::CoreCancels` gets core's own timer (`crate::query_timer`),
///   armed over this call, and `QueryTimer::reclassify` relabels the failing call `HYT00`
///   when the deadline fired. A backend enforcing its own timeout has its `HYT00` propagated
///   unchanged.
/// - HYT01: Connection timeout expired — propagated from the backend unchanged.
/// - IM001: Driver does not support this function (DM) (driver-manager-handled; not returned here).
/// - IM017: Polling disabled; not returned here (the asynchronous notification model is not
///   supported — not DM-annotated in the spec).
/// - IM018: SQLCompleteAsync not called; not returned here (the asynchronous notification model
///   is not supported — not DM-annotated in the spec).
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
pub unsafe fn sql_table_privileges_w<B: Backend>(
    statement_handle: *mut c_void,
    catalog_name: *const u16,
    name_length1: i16,
    schema_name: *const u16,
    name_length2: i16,
    table_name: *const u16,
    name_length3: i16,
) -> SqlReturn {
    tracing::trace!("SQLTablePrivilegesW(stmt={:?})", statement_handle);
    // SAFETY: statement_handle is null or a valid StatementHandle<B>; kind and group
    // validated by scope.stmt_with_parent inside the closure.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, |scope| {
            let (stmt, conn) = scope.stmt_with_parent::<B>(statement_handle)?;
            stmt.diagnostics.clear();

            if stmt.cursor_open {
                return Err(OdbcError::general(
                    "A cursor is already open on this statement",
                    SqlState::invalid_cursor_state(),
                ));
            }

            // Manual-commit mode: this call opens a transaction (or extends
            // the open one), which is what SQL_ATTR_TXN_ISOLATION's HY011 is
            // about. Recorded before the backend call, not after it succeeds:
            // a call that fails partway may still have opened one.
            conn.note_work_started();

            let Some(ref connection) = conn.connection else {
                return Err(OdbcError::general(
                    "Connection is not open",
                    SqlState::function_sequence_error(),
                ));
            };

            let catalog = parse_filter_param(catalog_name, name_length1, "CatalogName")?;
            let schema = parse_filter_param(schema_name, name_length2, "SchemaName")?;
            let table = parse_filter_param(table_name, name_length3, "TableName")?;
            tracing::debug!(
                "SQLTablePrivilegesW(stmt={:?}, catalog={:?}, schema={:?}, table={:?})",
                statement_handle,
                catalog,
                schema,
                table,
            );

            // Spec HY009 (not (DM)): METADATA_ID plus a null CatalogName on a
            // data source that has catalogs. This page's null-`TableName`
            // clause *is* (DM)-marked, unlike `SQLColumnPrivileges`', so there
            // is deliberately no unconditional check here — see the doc comment
            // and `table_privileges_does_not_check_null_table_name`.
            let metadata_id = metadata_id_enabled(stmt);
            check_metadata_id_null_catalog::<B>(
                connection,
                catalog_name,
                metadata_id,
                "SQLTablePrivilegesW",
            )?;

            let cancel_token = crate::handles::mint_cancel_token::<B>(statement_handle, connection);
            let cancel = crate::handles::cancel_as::<B>(&cancel_token)?;
            // Core-enforced deadline, if the backend asked core to own one.
            // Disarmed by `Drop` the moment this scope ends, so a fast call
            // leaves no thread behind.
            let timer =
                crate::query_timer::QueryTimer::arm::<B>(stmt.core_query_timeout, &cancel_token);

            // All three arguments are identifiers under METADATA_ID.
            let catalog = normalise_catalog_arg::<B>(connection, catalog, metadata_id);
            let schema = normalise_catalog_arg::<B>(connection, schema, metadata_id);
            let table = normalise_catalog_arg::<B>(connection, table, metadata_id);

            let query = crate::types::TablePrivilegesQuery::default()
                .with_catalog(catalog.as_deref())
                .with_schema(schema.as_deref())
                .with_table(table.as_deref());
            let rows = B::table_privileges(connection, cancel, &query);
            let rows = timer.check::<B, _, _>(rows, cancel)?;

            let mut values: Vec<Vec<ColumnValue>> =
                rows.iter().map(TablePrivilegeRow::to_values).collect();
            crate::catalog_sort::sort_rows(
                &mut values,
                &TABLE_PRIVILEGES_SORT_KEYS,
                B::null_collation(connection),
            );
            stmt.set_result_set(StatementData::Synthetic(SyntheticStatement::new(
                table_privileges_columns(&B::catalog_result_column_widths()),
                values,
            )));
            Ok(SqlReturn::SUCCESS)
        })
    };
    tracing::debug!("SQLTablePrivilegesW -> {:?}", ret);
    ret
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffi::handle::{sql_alloc_handle, sql_free_handle};
    use crate::ffi::info::type_info_columns;
    use crate::handles::ConnectionHandle;
    use crate::test_utils::{
        MockBackend, MockCancelAwareBackend, MockCatalogArgsBackend, MockCatalogBackend,
        MockConnection, MockFailingDescribeBackend, MockNegativeColumnCountBackend,
        MockNoCatalogBackend, alloc_env_conn_stmt, with_handle,
    };
    use crate::types::{
        CDataType, ColumnsResultCol, Desc, ForeignKeysResultCol, Nullable, PrimaryKeysResultCol,
        SQL_BEST_ROWID, SQL_INDEX_ALL, SQL_NTS, SQL_NULL_DATA, SQL_QUICK, SQL_SCOPE_CURROW,
        SQL_TRUE, SqlDataType, StatementAttribute,
    };
    use odbc_sys::HandleType;

    /// Every catalog result set the driver can produce, named for assertion
    /// messages. This is the list that must grow when a new catalog function
    /// gains a result set — the tests below iterate it, so a result set added
    /// outside `CatalogResultColumnWidths` fails here.
    fn every_catalog_result_set(
        widths: &CatalogResultColumnWidths,
    ) -> Vec<(&'static str, Vec<ColumnDescriptor>)> {
        vec![
            ("SQLTables", TablesResultCol::all_descriptors(widths)),
            ("SQLColumns", ColumnsResultCol::all_descriptors(widths)),
            (
                "SQLPrimaryKeys",
                PrimaryKeysResultCol::all_descriptors(widths),
            ),
            (
                "SQLForeignKeys",
                ForeignKeysResultCol::all_descriptors(widths),
            ),
            ("SQLStatistics", statistics_columns(widths)),
            ("SQLSpecialColumns", special_columns_columns(widths)),
            ("SQLProcedures", procedures_columns(widths)),
            ("SQLProcedureColumns", procedure_columns_columns(widths)),
            ("SQLColumnPrivileges", column_privileges_columns(widths)),
            ("SQLTablePrivileges", table_privileges_columns(widths)),
            ("SQLGetTypeInfo", type_info_columns(widths)),
        ]
    }

    /// Column names whose width is the data source's identifier limit. A
    /// driver that reports 63 here and 128 in `SQL_MAX_IDENTIFIER_LEN` is
    /// telling the application two different things about one limit.
    fn is_identifier_column(name: &str) -> bool {
        matches!(
            name,
            "TABLE_CAT"
                | "TABLE_SCHEM"
                | "TABLE_NAME"
                | "COLUMN_NAME"
                | "TYPE_NAME"
                | "LOCAL_TYPE_NAME"
                | "PK_NAME"
                | "FK_NAME"
                | "PKTABLE_CAT"
                | "PKTABLE_SCHEM"
                | "PKTABLE_NAME"
                | "PKCOLUMN_NAME"
                | "FKTABLE_CAT"
                | "FKTABLE_SCHEM"
                | "FKTABLE_NAME"
                | "FKCOLUMN_NAME"
                | "INDEX_QUALIFIER"
                | "INDEX_NAME"
                | "PROCEDURE_CAT"
                | "PROCEDURE_SCHEM"
                | "PROCEDURE_NAME"
                | "GRANTOR"
                | "GRANTEE"
        )
    }

    /// The completeness guard. A PostgreSQL-shaped override (identifiers cap
    /// at `NAMEDATALEN - 1`) must reach *every* catalog result set, not just
    /// the four that were consolidated first. A result set built with literal
    /// widths instead of `CatalogResultColumnWidths` fails here.
    #[test]
    fn every_catalog_result_set_follows_the_overridden_identifier_width() {
        const POSTGRES_NAMEDATALEN_MINUS_ONE: u16 = 63;
        let widths = CatalogResultColumnWidths {
            identifier_len: POSTGRES_NAMEDATALEN_MINUS_ONE,
            ..CatalogResultColumnWidths::default()
        };
        for (result_set, descs) in every_catalog_result_set(&widths) {
            let mut seen = 0;
            for desc in descs {
                if is_identifier_column(&desc.name) {
                    seen += 1;
                    assert_eq!(
                        desc.precision,
                        u32::from(POSTGRES_NAMEDATALEN_MINUS_ONE),
                        "{result_set}.{} ignored the overridden identifier_len",
                        desc.name
                    );
                }
            }
            assert!(
                seen > 0,
                "{result_set} has no identifier column — the name list in \
                 is_identifier_column is probably stale"
            );
        }
    }

    /// Every catalog result set is delivered through the W-suffix entry
    /// points, so its character columns must report the Unicode SQL types.
    /// Reporting `SQL_VARCHAR` for `SQLStatistics.TABLE_NAME` while
    /// `SQLTables.TABLE_NAME` says `SQL_WVARCHAR` is the same column described
    /// two ways.
    #[test]
    fn every_catalog_result_set_uses_one_set_of_sql_types() {
        let widths = CatalogResultColumnWidths::default();
        for (result_set, descs) in every_catalog_result_set(&widths) {
            for desc in descs {
                assert!(
                    matches!(
                        desc.sql_type,
                        SqlDataType::EXT_W_VARCHAR
                            | SqlDataType::EXT_W_CHAR
                            | SqlDataType::SMALLINT
                            | SqlDataType::INTEGER
                    ),
                    "{result_set}.{} has unexpected type {:?}",
                    desc.name,
                    desc.sql_type
                );
            }
        }
    }

    /// The spec fixes each result set's column count; anything else means a
    /// column was dropped or duplicated during consolidation.
    #[test]
    fn every_catalog_result_set_has_its_spec_column_count() {
        let widths = CatalogResultColumnWidths::default();
        let expected = [
            ("SQLTables", 5),
            ("SQLColumns", 18),
            ("SQLPrimaryKeys", 6),
            ("SQLForeignKeys", 14),
            ("SQLStatistics", 13),
            ("SQLSpecialColumns", 8),
            ("SQLProcedures", 8),
            ("SQLProcedureColumns", 19),
            ("SQLColumnPrivileges", 8),
            ("SQLTablePrivileges", 7),
            ("SQLGetTypeInfo", 19),
        ];
        let actual = every_catalog_result_set(&widths);
        assert_eq!(actual.len(), expected.len());
        for ((result_set, descs), (name, count)) in actual.into_iter().zip(expected) {
            assert_eq!(result_set, name);
            assert_eq!(descs.len(), count, "{result_set} column count");
        }
    }

    unsafe fn cleanup(env: *mut c_void, conn: *mut c_void, stmt: *mut c_void) {
        unsafe {
            let _ = sql_free_handle::<MockBackend>(HandleType::Stmt as i16, stmt);
            let _ = crate::ffi::connect::sql_disconnect::<MockBackend>(conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    /// Generic counterpart of `alloc_env_conn_stmt` + `cleanup` above, for a
    /// test that needs a backend other than `MockBackend`. The catalog
    /// functions all require an open connection, so this connects too.
    unsafe fn alloc_env_conn_stmt_for<B: Backend>() -> (*mut c_void, *mut c_void, *mut c_void) {
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<B>(HandleType::Env as i16, std::ptr::null_mut(), &mut env);
            let mut conn: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<B>(HandleType::Dbc as i16, env, &mut conn);
            let wide: Vec<u16> = "Host=localhost;Database=test".encode_utf16().collect();
            assert_eq!(
                crate::ffi::connect::sql_driver_connect_w::<B>(
                    conn,
                    std::ptr::null_mut(),
                    wide.as_ptr(),
                    i16::try_from(wide.len()).expect("connection string fits in i16"),
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    0,
                ),
                SqlReturn::SUCCESS,
            );
            let mut stmt: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<B>(HandleType::Stmt as i16, conn, &mut stmt);
            (env, conn, stmt)
        }
    }

    unsafe fn cleanup_for<B: Backend>(env: *mut c_void, conn: *mut c_void, stmt: *mut c_void) {
        unsafe {
            let _ = sql_free_handle::<B>(HandleType::Stmt as i16, stmt);
            // A connected handle cannot be freed.
            let _ = crate::ffi::connect::sql_disconnect::<B>(conn);
            let _ = sql_free_handle::<B>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<B>(HandleType::Env as i16, env);
        }
    }

    /// Fetch every row of the statement's open cursor, collecting one column
    /// as a string. A NULL value is collected as the empty string; no test
    /// here distinguishes the two.
    unsafe fn fetch_column_as_strings<B: Backend>(stmt: *mut c_void, column: u16) -> Vec<String> {
        let mut out = Vec::new();
        unsafe {
            loop {
                let fetched = crate::ffi::fetch::sql_fetch::<B>(stmt);
                if fetched == SqlReturn::NO_DATA {
                    break;
                }
                assert_eq!(fetched, SqlReturn::SUCCESS, "SQLFetch failed");
                let mut buf = [0u8; 128];
                let mut ind: isize = 0;
                let ret = crate::ffi::fetch::sql_get_data::<B>(
                    stmt,
                    column,
                    CDataType::Char as i16,
                    buf.as_mut_ptr().cast::<c_void>(),
                    buf.len() as isize,
                    &mut ind,
                );
                assert_eq!(ret, SqlReturn::SUCCESS, "SQLGetData failed");
                let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
                out.push(String::from_utf8_lossy(&buf[..end]).into_owned());
                assert!(out.len() <= 100, "fetch loop does not terminate");
            }
        }
        out
    }

    /// A UTF-16 argument for a `*W` catalog function, null-terminated so it
    /// can be passed with [`SQL_NTS_I16`]. The terminator is not optional: an
    /// empty `Vec<u16>`'s `as_ptr()` is dangling, and scanning it for a
    /// terminator would be undefined behaviour.
    fn utf16_of(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// [`SQL_NTS`] at the width of these functions' length parameters.
    /// `i16::try_from` is not usable in a `const` initialiser and `-3` cannot
    /// truncate.
    const SQL_NTS_I16: i16 = SQL_NTS as i16;

    /// The `SQLTables` result columns, in spec order.
    const TABLE_CAT_COLUMN: u16 = 1;
    const TABLE_SCHEM_COLUMN: u16 = 2;
    const TABLE_NAME_COLUMN: u16 = 3;
    const TABLE_TYPE_COLUMN: u16 = 4;
    const REMARKS_COLUMN: u16 = 5;
    /// `SQLProcedures`' discriminating result column.
    const PROCEDURE_NAME_COLUMN: u16 = 3;

    const ALL_TABLES_COLUMNS: [u16; 5] = [
        TABLE_CAT_COLUMN,
        TABLE_SCHEM_COLUMN,
        TABLE_NAME_COLUMN,
        TABLE_TYPE_COLUMN,
        REMARKS_COLUMN,
    ];

    /// Fetch every row of the statement's open cursor, collecting
    /// `(value, indicator)` for each of `columns`.
    ///
    /// One pass over the cursor, unlike `fetch_column_as_strings`: the cursor
    /// is forward-only, so a test needing two columns cannot simply call that
    /// helper twice — the second call would see `SQL_NO_DATA` immediately and
    /// assert over an empty vector.
    unsafe fn fetch_columns_with_indicators<B: Backend>(
        stmt: *mut c_void,
        columns: &[u16],
    ) -> Vec<Vec<(String, isize)>> {
        let mut out: Vec<Vec<(String, isize)>> = Vec::new();
        unsafe {
            loop {
                let fetched = crate::ffi::fetch::sql_fetch::<B>(stmt);
                if fetched == SqlReturn::NO_DATA {
                    break;
                }
                assert_eq!(fetched, SqlReturn::SUCCESS, "SQLFetch failed");
                let mut row = Vec::with_capacity(columns.len());
                for &column in columns {
                    let mut buf = [0u8; 128];
                    let mut ind: isize = 0;
                    let ret = crate::ffi::fetch::sql_get_data::<B>(
                        stmt,
                        column,
                        CDataType::Char as i16,
                        buf.as_mut_ptr().cast::<c_void>(),
                        buf.len() as isize,
                        &mut ind,
                    );
                    assert_eq!(ret, SqlReturn::SUCCESS, "SQLGetData failed");
                    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
                    row.push((String::from_utf8_lossy(&buf[..end]).into_owned(), ind));
                }
                out.push(row);
                assert!(out.len() <= 100, "fetch loop does not terminate");
            }
        }
        out
    }

    /// Drain a `SQLTables` enumeration result set: assert that every column
    /// except `populated` is genuinely NULL — the spec says those columns
    /// "contain NULLs", and an empty string would pass a string comparison
    /// while being wrong — and return the populated column's values in row
    /// order.
    unsafe fn enumeration_values<B: Backend>(stmt: *mut c_void, populated: u16) -> Vec<String> {
        let rows = unsafe { fetch_columns_with_indicators::<B>(stmt, &ALL_TABLES_COLUMNS) };
        for row in &rows {
            for (column, (value, indicator)) in ALL_TABLES_COLUMNS.iter().zip(row) {
                if *column == populated {
                    continue;
                }
                assert_eq!(
                    *indicator, SQL_NULL_DATA,
                    "column {column} must be NULL, got {value:?}"
                );
            }
        }
        let index = usize::from(populated - 1);
        rows.into_iter().map(|row| row[index].0.clone()).collect()
    }

    /// Spec: "If CatalogName is SQL_ALL_CATALOGS and SchemaName and TableName
    /// are empty strings, the result set contains a list of valid catalogs for
    /// the data source. (All columns except the TABLE_CAT column contain
    /// NULLs.)"
    #[test]
    fn all_catalogs_enumeration_returns_catalog_names_only() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockCatalogBackend>();
            let catalog = utf16_of(SQL_ALL_CATALOGS);
            let empty = utf16_of("");
            let ret = sql_tables_w::<MockCatalogBackend>(
                stmt,
                catalog.as_ptr(),
                SQL_NTS_I16,
                empty.as_ptr(),
                SQL_NTS_I16,
                empty.as_ptr(),
                SQL_NTS_I16,
                std::ptr::null(),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(
                enumeration_values::<MockCatalogBackend>(stmt, TABLE_CAT_COLUMN),
                vec!["cat_a", "cat_b"],
            );
            cleanup_for::<MockCatalogBackend>(env, conn, stmt);
        }
    }

    /// Spec: "If SchemaName is SQL_ALL_SCHEMAS and CatalogName and TableName
    /// are empty strings, the result set contains a list of valid schemas for
    /// the data source. (All columns except the TABLE_SCHEM column contain
    /// NULLs.)"
    #[test]
    fn all_schemas_enumeration_returns_schema_names_only() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockCatalogBackend>();
            let schema = utf16_of(SQL_ALL_SCHEMAS);
            let empty = utf16_of("");
            let ret = sql_tables_w::<MockCatalogBackend>(
                stmt,
                empty.as_ptr(),
                SQL_NTS_I16,
                schema.as_ptr(),
                SQL_NTS_I16,
                empty.as_ptr(),
                SQL_NTS_I16,
                std::ptr::null(),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(
                enumeration_values::<MockCatalogBackend>(stmt, TABLE_SCHEM_COLUMN),
                vec!["sch_a", "sch_b"],
            );
            cleanup_for::<MockCatalogBackend>(env, conn, stmt);
        }
    }

    /// Spec: "If TableType is SQL_ALL_TABLE_TYPES and CatalogName, SchemaName,
    /// and TableName are empty strings, the result set contains a list of
    /// valid table types for the data source. (All columns except the
    /// TABLE_TYPE column contain NULLs.)"
    #[test]
    fn all_table_types_enumeration_returns_table_types_only() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockCatalogBackend>();
            let table_type = utf16_of(SQL_ALL_TABLE_TYPES);
            let empty = utf16_of("");
            let ret = sql_tables_w::<MockCatalogBackend>(
                stmt,
                empty.as_ptr(),
                SQL_NTS_I16,
                empty.as_ptr(),
                SQL_NTS_I16,
                empty.as_ptr(),
                SQL_NTS_I16,
                table_type.as_ptr(),
                SQL_NTS_I16,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(
                enumeration_values::<MockCatalogBackend>(stmt, TABLE_TYPE_COLUMN),
                vec!["TABLE", "VIEW"],
            );
            cleanup_for::<MockCatalogBackend>(env, conn, stmt);
        }
    }

    /// The regression this pins: all three `SQL_ALL_*` sentinels are the
    /// literal string `"%"`, so a detector keyed on `"%"` alone would replace
    /// an ordinary match-everything query with a catalog list.
    #[test]
    fn a_pattern_in_every_argument_is_not_an_enumeration() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockCatalogBackend>();
            let pattern = utf16_of("%");
            let ret = sql_tables_w::<MockCatalogBackend>(
                stmt,
                pattern.as_ptr(),
                SQL_NTS_I16,
                pattern.as_ptr(),
                SQL_NTS_I16,
                pattern.as_ptr(),
                SQL_NTS_I16,
                std::ptr::null(),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(
                fetch_column_as_strings::<MockCatalogBackend>(stmt, TABLE_NAME_COLUMN),
                vec!["b_table", "z_table", "a_view"],
                "must be the ordinary table list, not a catalog enumeration"
            );
            cleanup_for::<MockCatalogBackend>(env, conn, stmt);
        }
    }

    /// `supports_catalogs` is a required capability method, so core answers an
    /// `SQL_ALL_CATALOGS` enumeration with an empty result set for a backend
    /// that has no catalogs, without calling the backend at all —
    /// `MockNoCatalogBackend::tables` returns an error, so reaching the
    /// ordinary path would surface as `SQL_ERROR`.
    #[test]
    fn all_catalogs_is_empty_when_the_backend_has_no_catalogs() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockNoCatalogBackend>();
            let catalog = utf16_of(SQL_ALL_CATALOGS);
            let empty = utf16_of("");
            let ret = sql_tables_w::<MockNoCatalogBackend>(
                stmt,
                catalog.as_ptr(),
                SQL_NTS_I16,
                empty.as_ptr(),
                SQL_NTS_I16,
                empty.as_ptr(),
                SQL_NTS_I16,
                std::ptr::null(),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert!(
                fetch_column_as_strings::<MockNoCatalogBackend>(stmt, TABLE_CAT_COLUMN).is_empty()
            );
            cleanup_for::<MockNoCatalogBackend>(env, conn, stmt);
        }
    }

    /// The schema half of the check above: `supports_schemas` is likewise
    /// required, so a schema enumeration is empty without a backend call.
    #[test]
    fn all_schemas_is_empty_when_the_backend_has_no_schemas() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockNoCatalogBackend>();
            let schema = utf16_of(SQL_ALL_SCHEMAS);
            let empty = utf16_of("");
            let ret = sql_tables_w::<MockNoCatalogBackend>(
                stmt,
                empty.as_ptr(),
                SQL_NTS_I16,
                schema.as_ptr(),
                SQL_NTS_I16,
                empty.as_ptr(),
                SQL_NTS_I16,
                std::ptr::null(),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert!(
                fetch_column_as_strings::<MockNoCatalogBackend>(stmt, TABLE_SCHEM_COLUMN)
                    .is_empty()
            );
            cleanup_for::<MockNoCatalogBackend>(env, conn, stmt);
        }
    }

    /// Spec: "SQLTables returns the results as a standard result set, ordered
    /// by TABLE_TYPE, TABLE_CAT, TABLE_SCHEM, and TABLE_NAME." TABLE_TYPE
    /// dominates, so a VIEW sorts after every TABLE regardless of name.
    #[test]
    fn tables_result_is_sorted_by_spec_keys() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockCatalogBackend>();
            let ret = sql_tables_w::<MockCatalogBackend>(
                stmt,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let names = fetch_column_as_strings::<MockCatalogBackend>(stmt, TABLE_NAME_COLUMN);
            assert_eq!(
                names,
                vec!["b_table", "z_table", "a_view"],
                "TABLE_TYPE must dominate TABLE_NAME"
            );

            cleanup_for::<MockCatalogBackend>(env, conn, stmt);
        }
    }

    /// Spec: "SQLColumns returns the results as a standard result set, ordered
    /// by TABLE_CAT, TABLE_SCHEM, TABLE_NAME, and ORDINAL_POSITION." The
    /// ordinals within `t_one` are 10, 2, 1 — compared as text they would sort
    /// 1, 10, 2.
    #[test]
    fn columns_result_is_sorted_by_spec_keys() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockCatalogBackend>();
            let ret = sql_columns_w::<MockCatalogBackend>(
                stmt,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            const COLUMN_NAME_COLUMN: u16 = 4;
            let names = fetch_column_as_strings::<MockCatalogBackend>(stmt, COLUMN_NAME_COLUMN);
            assert_eq!(
                names,
                vec!["z_first", "a", "b", "j"],
                "TABLE_NAME must dominate, and ORDINAL_POSITION must compare numerically"
            );

            cleanup_for::<MockCatalogBackend>(env, conn, stmt);
        }
    }

    /// Spec: "SQLPrimaryKeys returns the results as a standard result set,
    /// ordered by TABLE_CAT, TABLE_SCHEM, TABLE_NAME, and KEY_SEQ."
    #[test]
    fn primary_keys_result_is_sorted_by_spec_keys() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockCatalogBackend>();
            let ret = sql_primary_keys_w::<MockCatalogBackend>(
                stmt,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            const COLUMN_NAME_COLUMN: u16 = 4;
            let names = fetch_column_as_strings::<MockCatalogBackend>(stmt, COLUMN_NAME_COLUMN);
            assert_eq!(
                names,
                vec!["a", "b", "c", "x"],
                "TABLE_NAME must dominate KEY_SEQ"
            );

            cleanup_for::<MockCatalogBackend>(env, conn, stmt);
        }
    }

    /// Spec: "If the foreign keys associated with a primary key are requested,
    /// the result set is ordered by FKTABLE_CAT, FKTABLE_SCHEM, FKTABLE_NAME,
    /// and KEY_SEQ." That is the `PKTableName`-supplied case.
    #[test]
    fn foreign_keys_result_is_fk_ordered_when_pk_table_is_supplied() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockCatalogBackend>();
            let pk_table: Vec<u16> = "p_a".encode_utf16().collect();
            let ret = sql_foreign_keys_w::<MockCatalogBackend>(
                stmt,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                pk_table.as_ptr(),
                i16::try_from(pk_table.len()).expect("table name fits in i16"),
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            const FK_NAME_COLUMN: u16 = 12;
            let names = fetch_column_as_strings::<MockCatalogBackend>(stmt, FK_NAME_COLUMN);
            assert_eq!(
                names,
                vec!["third", "second", "first"],
                "FKTABLE_NAME must order the result set"
            );

            cleanup_for::<MockCatalogBackend>(env, conn, stmt);
        }
    }

    /// Spec: "If the primary keys associated with a foreign key are requested,
    /// the result set is ordered by PKTABLE_CAT, PKTABLE_SCHEM, PKTABLE_NAME,
    /// and KEY_SEQ." That is the `FKTableName`-only case.
    #[test]
    fn foreign_keys_result_is_pk_ordered_when_only_fk_table_is_supplied() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockCatalogBackend>();
            let fk_table: Vec<u16> = "f_a".encode_utf16().collect();
            let ret = sql_foreign_keys_w::<MockCatalogBackend>(
                stmt,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                fk_table.as_ptr(),
                i16::try_from(fk_table.len()).expect("table name fits in i16"),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            const FK_NAME_COLUMN: u16 = 12;
            let names = fetch_column_as_strings::<MockCatalogBackend>(stmt, FK_NAME_COLUMN);
            assert_eq!(
                names,
                vec!["second", "first", "third"],
                "PKTABLE_NAME must order the result set"
            );

            cleanup_for::<MockCatalogBackend>(env, conn, stmt);
        }
    }

    /// Spec: "SQLStatistics returns the results as a standard result set,
    /// ordered by NON_UNIQUE, TYPE, INDEX_QUALIFIER, INDEX_NAME, and
    /// ORDINAL_POSITION."
    #[test]
    fn statistics_result_is_sorted_by_spec_keys() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockCatalogBackend>();
            // A real TableName: `SQLStatistics` is one of the two catalog
            // functions whose null-`TableName` `HY009` is the driver's.
            let table = utf16_of("t");
            let ret = sql_statistics_w::<MockCatalogBackend>(
                stmt,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                table.as_ptr(),
                SQL_NTS_I16,
                SQL_INDEX_ALL,
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            const COLUMN_NAME_COLUMN: u16 = 9;
            let names = fetch_column_as_strings::<MockCatalogBackend>(stmt, COLUMN_NAME_COLUMN);
            assert_eq!(
                names,
                vec!["b", "a", "c", "d"],
                "NON_UNIQUE must dominate, then INDEX_NAME, then ORDINAL_POSITION"
            );

            cleanup_for::<MockCatalogBackend>(env, conn, stmt);
        }
    }

    /// Spec: "SQLSpecialColumns returns the results as a standard result set,
    /// ordered by SCOPE."
    #[test]
    fn special_columns_result_is_sorted_by_spec_keys() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockCatalogBackend>();
            // A real TableName: `SQLSpecialColumns` is the other catalog
            // function whose null-`TableName` `HY009` is the driver's.
            let table = utf16_of("t");
            let ret = sql_special_columns_w::<MockCatalogBackend>(
                stmt,
                SQL_BEST_ROWID,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                table.as_ptr(),
                SQL_NTS_I16,
                SQL_SCOPE_CURROW,
                Nullable::SqlNullable as u16,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            const COLUMN_NAME_COLUMN: u16 = 2;
            let names = fetch_column_as_strings::<MockCatalogBackend>(stmt, COLUMN_NAME_COLUMN);
            assert_eq!(
                names,
                vec!["a", "b", "c"],
                "SCOPE must order the result set"
            );

            cleanup_for::<MockCatalogBackend>(env, conn, stmt);
        }
    }

    /// Spec: "SQLProcedures returns the results as a standard result set,
    /// ordered by PROCEDURE_CAT, PROCEDURE_SCHEMA, and PROCEDURE_NAME." The
    /// page writes `PROCEDURE_SCHEMA`, but its own result-column table names
    /// column 2 `PROCEDURE_SCHEM`; the sort is by column 2 either way.
    #[test]
    fn procedures_result_is_sorted_by_spec_keys() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockCatalogBackend>();
            let ret = sql_procedures_w::<MockCatalogBackend>(
                stmt,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let names = fetch_column_as_strings::<MockCatalogBackend>(stmt, PROCEDURE_NAME_COLUMN);
            assert_eq!(names, vec!["a_proc", "m_proc", "z_proc"]);

            cleanup_for::<MockCatalogBackend>(env, conn, stmt);
        }
    }

    /// Spec: "SQLProcedureColumns returns the results as a standard result
    /// set, ordered by PROCEDURE_CAT, PROCEDURE_SCHEM, PROCEDURE_NAME, and
    /// COLUMN_TYPE."
    #[test]
    fn procedure_columns_result_is_sorted_by_spec_keys() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockCatalogBackend>();
            let ret = sql_procedure_columns_w::<MockCatalogBackend>(
                stmt,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            const COLUMN_NAME_COLUMN: u16 = 4;
            let names = fetch_column_as_strings::<MockCatalogBackend>(stmt, COLUMN_NAME_COLUMN);
            assert_eq!(
                names,
                vec!["a", "b", "c"],
                "COLUMN_TYPE orders the rows, not COLUMN_NAME"
            );

            cleanup_for::<MockCatalogBackend>(env, conn, stmt);
        }
    }

    /// Spec: "SQLColumnPrivileges returns the results as a standard result set,
    /// ordered by TABLE_CAT, TABLE_SCHEM, TABLE_NAME, COLUMN_NAME, and
    /// PRIVILEGE."
    #[test]
    fn column_privileges_result_is_sorted_by_spec_keys() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockCatalogBackend>();
            // `TableName` must not be null: the spec makes that HY009 here.
            let table = utf16_of("t");
            let ret = sql_column_privileges_w::<MockCatalogBackend>(
                stmt,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                table.as_ptr(),
                SQL_NTS_I16,
                std::ptr::null(),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            const IS_GRANTABLE_COLUMN: u16 = 8;
            let labels = fetch_column_as_strings::<MockCatalogBackend>(stmt, IS_GRANTABLE_COLUMN);
            assert_eq!(
                labels,
                vec!["first", "second", "third"],
                "COLUMN_NAME dominates, and PRIVILEGE breaks the tie within a column"
            );

            cleanup_for::<MockCatalogBackend>(env, conn, stmt);
        }
    }

    /// Spec: "SQLTablePrivileges returns the results as a standard result set,
    /// ordered by TABLE_CAT, TABLE_SCHEM, TABLE_NAME, PRIVILEGE, and GRANTEE."
    /// PRIVILEGE comes *before* GRANTEE, so the sort keys are not in ascending
    /// column order — that is the spec, not a transcription slip.
    #[test]
    fn table_privileges_result_is_sorted_by_spec_keys() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockCatalogBackend>();
            let ret = sql_table_privileges_w::<MockCatalogBackend>(
                stmt,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            const IS_GRANTABLE_COLUMN: u16 = 7;
            let labels = fetch_column_as_strings::<MockCatalogBackend>(stmt, IS_GRANTABLE_COLUMN);
            assert_eq!(
                labels,
                vec!["first", "second", "third"],
                "PRIVILEGE dominates GRANTEE"
            );

            cleanup_for::<MockCatalogBackend>(env, conn, stmt);
        }
    }

    /// Spec, `SQLColumnPrivileges` `HY009`: "The TableName argument was a null
    /// pointer." No `(DM)` marker, and not conditional on `METADATA_ID` —
    /// unlike the `SchemaName`/`ColumnName` sentence beside it, which is
    /// `(DM)`. The page's argument description agrees: `TableName` "cannot be a
    /// null pointer". This is the only one of the four functions in this family
    /// carrying such a clause.
    #[test]
    fn column_privileges_checks_null_table_name_unconditionally() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockCatalogBackend>();
            let ret = sql_column_privileges_w::<MockCatalogBackend>(
                stmt,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(), // null TableName, METADATA_ID not set
                0,
                std::ptr::null(),
                0,
            );
            assert_eq!(ret, SqlReturn::ERROR);
            with_handle::<MockCatalogBackend, StatementHandle<MockCatalogBackend>, _>(stmt, |h| {
                let rec = h.diagnostics.get(0).expect("record 1 exists");
                assert_eq!(rec.sqlstate.as_str(), "HY009");
            });
            cleanup_for::<MockCatalogBackend>(env, conn, stmt);
        }
    }

    /// Spec, `SQLTablePrivileges` `HY009`: its null-`TableName` clause appears
    /// only under "(DM) ... SQL_ATTR_METADATA_ID was set to SQL_TRUE". There is
    /// no unconditional driver-side check here, unlike `SQLColumnPrivileges`.
    /// Do not "fix" this into consistency.
    #[test]
    fn table_privileges_does_not_check_null_table_name() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockCatalogBackend>();
            let ret = sql_table_privileges_w::<MockCatalogBackend>(
                stmt,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            cleanup_for::<MockCatalogBackend>(env, conn, stmt);
        }
    }

    /// The same for `SQLProcedures` and `SQLProcedureColumns`: neither page
    /// states an unmarked null-argument clause, so a null `ProcName` is
    /// accepted. Both are exercised because the two pages were transcribed
    /// separately, and only one of them being right is the realistic slip.
    #[test]
    fn the_procedure_functions_do_not_check_null_name_arguments() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockCatalogBackend>();
            assert_eq!(
                sql_procedures_w::<MockCatalogBackend>(
                    stmt,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    0,
                ),
                SqlReturn::SUCCESS,
            );
            // The first call left a cursor open; a second on the same
            // statement would fail 24000 for that reason rather than HY009.
            let _ = crate::ffi::cursor::sql_close_cursor::<MockCatalogBackend>(stmt);
            assert_eq!(
                sql_procedure_columns_w::<MockCatalogBackend>(
                    stmt,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    0,
                ),
                SqlReturn::SUCCESS,
            );
            cleanup_for::<MockCatalogBackend>(env, conn, stmt);
        }
    }

    /// Spec `HY009`: "The SQL_ATTR_METADATA_ID statement attribute was set to
    /// SQL_TRUE, the CatalogName argument was a null pointer, and the
    /// SQL_CATALOG_NAME InfoType returns that catalog names are supported."
    /// That sentence carries no `(DM)` marker on any of these four pages, so
    /// all four check it — the half that *is* uniform across the family.
    #[test]
    fn all_four_check_metadata_id_with_a_null_catalog() {
        unsafe {
            let expect_hy009 = |stmt: *mut c_void, ret: SqlReturn, function: &str| {
                assert_eq!(ret, SqlReturn::ERROR, "{function} accepted a null catalog");
                with_handle::<MockCatalogBackend, StatementHandle<MockCatalogBackend>, _>(
                    stmt,
                    |h| {
                        let rec = h.diagnostics.get(0).expect("record 1 exists");
                        assert_eq!(rec.sqlstate.as_str(), "HY009", "{function}");
                    },
                );
            };
            let empty = utf16_of("");

            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockCatalogBackend>();
            set_metadata_id_true::<MockCatalogBackend>(stmt);
            expect_hy009(
                stmt,
                sql_procedures_w::<MockCatalogBackend>(
                    stmt,
                    std::ptr::null(),
                    0,
                    empty.as_ptr(),
                    SQL_NTS_I16,
                    empty.as_ptr(),
                    SQL_NTS_I16,
                ),
                "SQLProceduresW",
            );
            expect_hy009(
                stmt,
                sql_procedure_columns_w::<MockCatalogBackend>(
                    stmt,
                    std::ptr::null(),
                    0,
                    empty.as_ptr(),
                    SQL_NTS_I16,
                    empty.as_ptr(),
                    SQL_NTS_I16,
                    empty.as_ptr(),
                    SQL_NTS_I16,
                ),
                "SQLProcedureColumnsW",
            );
            expect_hy009(
                stmt,
                sql_column_privileges_w::<MockCatalogBackend>(
                    stmt,
                    std::ptr::null(),
                    0,
                    empty.as_ptr(),
                    SQL_NTS_I16,
                    // Non-null, so the unconditional TableName check cannot be
                    // what produces the HY009 this asserts.
                    empty.as_ptr(),
                    SQL_NTS_I16,
                    empty.as_ptr(),
                    SQL_NTS_I16,
                ),
                "SQLColumnPrivilegesW",
            );
            expect_hy009(
                stmt,
                sql_table_privileges_w::<MockCatalogBackend>(
                    stmt,
                    std::ptr::null(),
                    0,
                    empty.as_ptr(),
                    SQL_NTS_I16,
                    empty.as_ptr(),
                    SQL_NTS_I16,
                ),
                "SQLTablePrivilegesW",
            );
            cleanup_for::<MockCatalogBackend>(env, conn, stmt);
        }
    }

    /// Spec, "Arguments in Catalog Functions": under `METADATA_ID = SQL_TRUE`
    /// **every** string argument of all four becomes an identifier — there is
    /// no `TableType`-style exemption anywhere in this family. Core folds and
    /// escapes each one before the backend sees it.
    #[test]
    fn metadata_id_normalises_every_argument_of_the_four() {
        unsafe {
            let catalog = utf16_of("my_cat");
            let schema = utf16_of("my_sch");
            let name = utf16_of("my_name");
            let column = utf16_of("my_col");

            // Each function gets its own statement: a call leaves a cursor
            // open, and a second on the same statement would fail 24000.
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockCatalogArgsBackend>();
            set_metadata_id_true::<MockCatalogArgsBackend>(stmt);
            assert_eq!(
                sql_procedures_w::<MockCatalogArgsBackend>(
                    stmt,
                    catalog.as_ptr(),
                    SQL_NTS_I16,
                    schema.as_ptr(),
                    SQL_NTS_I16,
                    name.as_ptr(),
                    SQL_NTS_I16,
                ),
                SqlReturn::SUCCESS,
            );
            let args = MockCatalogArgsBackend::recorded().expect("Backend::procedures was called");
            assert_eq!(args.catalog.as_deref(), Some("MY\\_CAT"));
            assert_eq!(args.schema.as_deref(), Some("MY\\_SCH"));
            assert_eq!(args.proc.as_deref(), Some("MY\\_NAME"));
            cleanup_for::<MockCatalogArgsBackend>(env, conn, stmt);

            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockCatalogArgsBackend>();
            set_metadata_id_true::<MockCatalogArgsBackend>(stmt);
            assert_eq!(
                sql_procedure_columns_w::<MockCatalogArgsBackend>(
                    stmt,
                    catalog.as_ptr(),
                    SQL_NTS_I16,
                    schema.as_ptr(),
                    SQL_NTS_I16,
                    name.as_ptr(),
                    SQL_NTS_I16,
                    column.as_ptr(),
                    SQL_NTS_I16,
                ),
                SqlReturn::SUCCESS,
            );
            let args =
                MockCatalogArgsBackend::recorded().expect("Backend::procedure_columns was called");
            assert_eq!(args.catalog.as_deref(), Some("MY\\_CAT"));
            assert_eq!(args.schema.as_deref(), Some("MY\\_SCH"));
            assert_eq!(args.proc.as_deref(), Some("MY\\_NAME"));
            assert_eq!(args.column.as_deref(), Some("MY\\_COL"));
            cleanup_for::<MockCatalogArgsBackend>(env, conn, stmt);

            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockCatalogArgsBackend>();
            set_metadata_id_true::<MockCatalogArgsBackend>(stmt);
            assert_eq!(
                sql_column_privileges_w::<MockCatalogArgsBackend>(
                    stmt,
                    catalog.as_ptr(),
                    SQL_NTS_I16,
                    schema.as_ptr(),
                    SQL_NTS_I16,
                    name.as_ptr(),
                    SQL_NTS_I16,
                    column.as_ptr(),
                    SQL_NTS_I16,
                ),
                SqlReturn::SUCCESS,
            );
            let args =
                MockCatalogArgsBackend::recorded().expect("Backend::column_privileges was called");
            assert_eq!(args.catalog.as_deref(), Some("MY\\_CAT"));
            assert_eq!(args.schema.as_deref(), Some("MY\\_SCH"));
            assert_eq!(args.table.as_deref(), Some("MY\\_NAME"));
            assert_eq!(args.column.as_deref(), Some("MY\\_COL"));
            cleanup_for::<MockCatalogArgsBackend>(env, conn, stmt);

            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockCatalogArgsBackend>();
            set_metadata_id_true::<MockCatalogArgsBackend>(stmt);
            assert_eq!(
                sql_table_privileges_w::<MockCatalogArgsBackend>(
                    stmt,
                    catalog.as_ptr(),
                    SQL_NTS_I16,
                    schema.as_ptr(),
                    SQL_NTS_I16,
                    name.as_ptr(),
                    SQL_NTS_I16,
                ),
                SqlReturn::SUCCESS,
            );
            let args =
                MockCatalogArgsBackend::recorded().expect("Backend::table_privileges was called");
            assert_eq!(args.catalog.as_deref(), Some("MY\\_CAT"));
            assert_eq!(args.schema.as_deref(), Some("MY\\_SCH"));
            assert_eq!(args.table.as_deref(), Some("MY\\_NAME"));
            cleanup_for::<MockCatalogArgsBackend>(env, conn, stmt);
        }
    }

    /// Spec, `SQLTables` `HY008`, second clause: the function "was called, and
    /// before it completed execution, `SQLCancel` … was called on the
    /// `StatementHandle` from a different thread in a multithread
    /// application." No `(DM)` marker, so this driver returns it. A catalog
    /// function is an ordinary backend call and is cancellable like any other.
    #[test]
    fn a_cancelled_catalog_call_reports_hy008() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockCancelAwareBackend>();
            MockCancelAwareBackend::fail_next_execution();
            MockCancelAwareBackend::cancel_before_returning();

            let ret = sql_tables_w::<MockCancelAwareBackend>(
                stmt,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
            );
            assert_eq!(ret, SqlReturn::ERROR);
            with_handle::<MockCancelAwareBackend, StatementHandle<MockCancelAwareBackend>, _>(
                stmt,
                |h| {
                    let rec = h.diagnostics.get(0).expect("record 1 exists");
                    assert_eq!(rec.sqlstate.as_str(), "HY008");
                },
            );
            cleanup_for::<MockCancelAwareBackend>(env, conn, stmt);
        }
    }

    /// The counterpart, and the subtlety of the two `match`-shaped catalog
    /// sites: `SQLStatistics` and `SQLSpecialColumns` convert the backend's
    /// error *before* matching, so a `NotImplemented` can be recognised and
    /// turned into the spec's empty result set.
    ///
    /// `NotImplemented` there means "this backend exposes no index metadata" —
    /// a legitimate empty answer, not a failure — so it must survive even when
    /// the token happens to be signalled. Reclassifying the whole `Result`
    /// rather than only its genuine-error arm would turn a spec-mandated empty
    /// result set into `HY008`.
    #[test]
    fn an_unimplemented_catalog_method_is_not_reclassified_as_cancelled() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockCancelAwareBackend>();
            // The mock's `statistics` signals its own token and then answers
            // `NotImplemented`, which is the exact collision this pins.
            MockCancelAwareBackend::cancel_before_returning();

            let table = utf16_of("t");
            let ret = sql_statistics_w::<MockCancelAwareBackend>(
                stmt,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                table.as_ptr(),
                SQL_NTS_I16,
                SQL_INDEX_ALL,
                SQL_QUICK,
            );
            assert_eq!(
                ret,
                SqlReturn::SUCCESS,
                "an unimplemented catalog method still yields the spec's empty result set"
            );
            cleanup_for::<MockCancelAwareBackend>(env, conn, stmt);
        }
    }

    /// Set `SQL_ATTR_METADATA_ID` to `SQL_TRUE` through the real entry point,
    /// so these tests exercise the same storage `SQLSetStmtAttr` writes rather
    /// than reaching into `stmt.attrs`.
    unsafe fn set_metadata_id_true<B: Backend>(stmt: *mut c_void) {
        let ret = unsafe {
            crate::ffi::stmt_attr::sql_set_stmt_attr_w::<B>(
                stmt,
                StatementAttribute::MetadataId as i32,
                SQL_TRUE as usize as *mut c_void,
                0,
            )
        };
        assert_eq!(
            ret,
            SqlReturn::SUCCESS,
            "setting SQL_ATTR_METADATA_ID failed"
        );
    }

    /// Spec, `SQLTables` `TableName`: under `SQL_ATTR_METADATA_ID = SQL_TRUE`
    /// the argument "is treated as an identifier argument" — its case is not
    /// significant, and it is not a pattern. Core normalises it before the
    /// backend sees it, so the backend receives a pattern matching exactly one
    /// name: folded per `SQL_IDENTIFIER_CASE` and with `_` escaped per
    /// `SQL_SEARCH_PATTERN_ESCAPE`.
    #[test]
    fn metadata_id_folds_and_escapes_the_table_argument() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockCatalogArgsBackend>();
            set_metadata_id_true::<MockCatalogArgsBackend>(stmt);

            let empty = utf16_of("");
            let name = utf16_of("my_table");
            let ret = sql_tables_w::<MockCatalogArgsBackend>(
                stmt,
                empty.as_ptr(),
                SQL_NTS_I16,
                empty.as_ptr(),
                SQL_NTS_I16,
                name.as_ptr(),
                SQL_NTS_I16,
                std::ptr::null(),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let args = MockCatalogArgsBackend::recorded().expect("Backend::tables was called");
            assert_eq!(
                args.table.as_deref(),
                Some("MY\\_TABLE"),
                "folded to upper case (SQL_IC_UPPER) and the _ escaped"
            );
            cleanup_for::<MockCatalogArgsBackend>(env, conn, stmt);
        }
    }

    /// The other half of the same rule: with `SQL_ATTR_METADATA_ID` at its
    /// default `SQL_FALSE`, `TableName` is a pattern-value argument and must
    /// reach the backend byte for byte. Normalising unconditionally would turn
    /// every `LIKE` pattern an application ever passed into a literal.
    #[test]
    fn without_metadata_id_the_table_argument_is_passed_through_verbatim() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockCatalogArgsBackend>();

            let empty = utf16_of("");
            let name = utf16_of("my_table");
            let ret = sql_tables_w::<MockCatalogArgsBackend>(
                stmt,
                empty.as_ptr(),
                SQL_NTS_I16,
                empty.as_ptr(),
                SQL_NTS_I16,
                name.as_ptr(),
                SQL_NTS_I16,
                std::ptr::null(),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let args = MockCatalogArgsBackend::recorded().expect("Backend::tables was called");
            assert_eq!(args.table.as_deref(), Some("my_table"));
            cleanup_for::<MockCatalogArgsBackend>(env, conn, stmt);
        }
    }

    /// Core fills each `TablesQuery` field from the `SQLTables` argument of the
    /// same name.
    ///
    /// The two `METADATA_ID` tests above supply an empty catalog and schema and
    /// assert only on the table, so transposing any two of the three was
    /// invisible to the entire suite. `TablesQuery` makes that mistake
    /// unwritable inside a backend; this pins the one place that still fills
    /// the fields in positionally, where it remains writable.
    #[test]
    fn sql_tables_fills_each_query_field_from_its_own_argument() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockCatalogArgsBackend>();

            let catalog = utf16_of("cat");
            let schema = utf16_of("sch");
            let table = utf16_of("tbl");
            let table_type = utf16_of("VIEW");
            let ret = sql_tables_w::<MockCatalogArgsBackend>(
                stmt,
                catalog.as_ptr(),
                SQL_NTS_I16,
                schema.as_ptr(),
                SQL_NTS_I16,
                table.as_ptr(),
                SQL_NTS_I16,
                table_type.as_ptr(),
                SQL_NTS_I16,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let args = MockCatalogArgsBackend::recorded().expect("Backend::tables was called");
            assert_eq!(args.catalog.as_deref(), Some("cat"));
            assert_eq!(args.schema.as_deref(), Some("sch"));
            assert_eq!(args.table.as_deref(), Some("tbl"));
            assert_eq!(args.table_types, vec![String::from("VIEW")]);
            cleanup_for::<MockCatalogArgsBackend>(env, conn, stmt);
        }
    }

    /// Set `SQL_ATTR_METADATA_ID` to `SQL_TRUE` on a **connection** through the
    /// real entry point.
    unsafe fn set_connection_metadata_id_true<B: Backend>(conn: *mut c_void) {
        let ret = unsafe {
            crate::ffi::connect_attr::sql_set_connect_attr_w::<B>(
                conn,
                crate::types::ConnectionAttribute::METADATA_ID.0,
                SQL_TRUE as usize as *mut c_void,
                0,
            )
        };
        assert_eq!(
            ret,
            SqlReturn::SUCCESS,
            "setting SQL_ATTR_METADATA_ID on the connection failed"
        );
    }

    unsafe fn alloc_stmt_on<B: Backend>(conn: *mut c_void) -> *mut c_void {
        let mut stmt: *mut c_void = std::ptr::null_mut();
        let ret = unsafe { sql_alloc_handle::<B>(HandleType::Stmt as i16, conn, &mut stmt) };
        assert_eq!(ret, SqlReturn::SUCCESS, "allocating a statement failed");
        stmt
    }

    /// Spec, `SQLSetStmtAttr` Comments: "ODBC 3.x statement attributes cannot
    /// be set at the connection level, with the exception of the
    /// SQL_ATTR_METADATA_ID and SQL_ATTR_ASYNC_ENABLE attributes, which are
    /// both connection attributes and statement attributes, and can be set at
    /// either the connection level or the statement level."
    ///
    /// The connection-level route is therefore legal, and a statement
    /// allocated after it treats its catalog arguments as identifiers.
    #[test]
    fn metadata_id_set_on_the_connection_reaches_a_statement_allocated_afterwards() {
        unsafe {
            let (env, conn, first) = alloc_env_conn_stmt_for::<MockCatalogArgsBackend>();
            set_connection_metadata_id_true::<MockCatalogArgsBackend>(conn);
            let stmt = alloc_stmt_on::<MockCatalogArgsBackend>(conn);

            let empty = utf16_of("");
            let name = utf16_of("my_table");
            let ret = sql_tables_w::<MockCatalogArgsBackend>(
                stmt,
                empty.as_ptr(),
                SQL_NTS_I16,
                empty.as_ptr(),
                SQL_NTS_I16,
                name.as_ptr(),
                SQL_NTS_I16,
                std::ptr::null(),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let args = MockCatalogArgsBackend::recorded().expect("Backend::tables was called");
            assert_eq!(
                args.table.as_deref(),
                Some("MY\\_TABLE"),
                "a statement allocated after the connection-level set must inherit it"
            );

            let _ = sql_free_handle::<MockCatalogArgsBackend>(HandleType::Stmt as i16, stmt);
            cleanup_for::<MockCatalogArgsBackend>(env, conn, first);
        }
    }

    /// The ODBC 2.x rule the connection-level route inherits: the value is the
    /// default for statements allocated *afterwards*, and does not reach back
    /// to statements that already exist. A statement the application has
    /// already configured keeps the argument treatment it was configured with.
    #[test]
    fn metadata_id_set_on_the_connection_leaves_an_existing_statement_alone() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockCatalogArgsBackend>();
            set_connection_metadata_id_true::<MockCatalogArgsBackend>(conn);

            let empty = utf16_of("");
            let name = utf16_of("my_table");
            let ret = sql_tables_w::<MockCatalogArgsBackend>(
                stmt,
                empty.as_ptr(),
                SQL_NTS_I16,
                empty.as_ptr(),
                SQL_NTS_I16,
                name.as_ptr(),
                SQL_NTS_I16,
                std::ptr::null(),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let args = MockCatalogArgsBackend::recorded().expect("Backend::tables was called");
            assert_eq!(
                args.table.as_deref(),
                Some("my_table"),
                "the statement predates the connection-level set and keeps SQL_FALSE"
            );
            cleanup_for::<MockCatalogArgsBackend>(env, conn, stmt);
        }
    }

    /// Inheritance seeds the statement's own value; it does not pin it. A
    /// statement-level `SQL_FALSE` wins over an inherited `SQL_TRUE`, so an
    /// application can turn the treatment off for one statement.
    #[test]
    fn a_statement_level_metadata_id_overrides_the_inherited_connection_value() {
        unsafe {
            let (env, conn, first) = alloc_env_conn_stmt_for::<MockCatalogArgsBackend>();
            set_connection_metadata_id_true::<MockCatalogArgsBackend>(conn);
            let stmt = alloc_stmt_on::<MockCatalogArgsBackend>(conn);

            let ret = crate::ffi::stmt_attr::sql_set_stmt_attr_w::<MockCatalogArgsBackend>(
                stmt,
                StatementAttribute::MetadataId as i32,
                SQL_FALSE as usize as *mut c_void,
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let empty = utf16_of("");
            let name = utf16_of("my_table");
            let ret = sql_tables_w::<MockCatalogArgsBackend>(
                stmt,
                empty.as_ptr(),
                SQL_NTS_I16,
                empty.as_ptr(),
                SQL_NTS_I16,
                name.as_ptr(),
                SQL_NTS_I16,
                std::ptr::null(),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let args = MockCatalogArgsBackend::recorded().expect("Backend::tables was called");
            assert_eq!(
                args.table.as_deref(),
                Some("my_table"),
                "the statement-level SQL_FALSE must override the inherited SQL_TRUE"
            );

            let _ = sql_free_handle::<MockCatalogArgsBackend>(HandleType::Stmt as i16, stmt);
            cleanup_for::<MockCatalogArgsBackend>(env, conn, first);
        }
    }

    /// Spec, `SQLTables` `TableType`: "the SQL_ATTR_METADATA_ID statement
    /// attribute has no effect upon the TableType argument. TableType is a
    /// value list argument, regardless of the setting of
    /// SQL_ATTR_METADATA_ID."
    #[test]
    fn metadata_id_does_not_touch_the_table_type_argument() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockCatalogArgsBackend>();
            set_metadata_id_true::<MockCatalogArgsBackend>(stmt);

            let empty = utf16_of("");
            // Lower case and carrying a `_`, so normalisation would visibly
            // change it. `'TABLE','VIEW'` is already upper case and has no
            // pattern metacharacter, so normalising it is a no-op and the
            // assertion would pass either way.
            let table_type = utf16_of("'base_table','view'");
            let ret = sql_tables_w::<MockCatalogArgsBackend>(
                stmt,
                empty.as_ptr(),
                SQL_NTS_I16,
                empty.as_ptr(),
                SQL_NTS_I16,
                empty.as_ptr(),
                SQL_NTS_I16,
                table_type.as_ptr(),
                SQL_NTS_I16,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let args = MockCatalogArgsBackend::recorded().expect("Backend::tables was called");
            assert_eq!(
                args.table_types,
                vec!["base_table".to_string(), "view".to_string()],
                "TableType is a value list, so only the list syntax is stripped — \
                 no case folding and no pattern escaping"
            );
            cleanup_for::<MockCatalogArgsBackend>(env, conn, stmt);
        }
    }

    /// Spec, `SQLTables` `TableType`: "a list of comma-separated values for the
    /// types of interest; each value can be enclosed in single quotation marks
    /// (') or unquoted, for example, 'TABLE', 'VIEW' or TABLE, VIEW." Core
    /// parses it once so that every driver does not.
    #[test]
    fn table_type_list_reaches_the_backend_parsed() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockCatalogArgsBackend>();
            // Mixes both spellings the spec's example gives, and pads with
            // whitespace, so a parser that handles only one of them fails here.
            let table_type = utf16_of("'TABLE', VIEW");
            let ret = sql_tables_w::<MockCatalogArgsBackend>(
                stmt,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                table_type.as_ptr(),
                SQL_NTS_I16,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let args = MockCatalogArgsBackend::recorded().expect("Backend::tables was called");
            assert_eq!(
                args.table_types,
                vec!["TABLE".to_string(), "VIEW".to_string()],
                "quoted and unquoted values both arrive parsed and trimmed"
            );
            cleanup_for::<MockCatalogArgsBackend>(env, conn, stmt);
        }
    }

    /// An absent `TableType` is "no table-type filter", which the previous
    /// signature spelled `None`. The empty slice carries the same meaning and
    /// must not be confused with a filter that matches nothing.
    #[test]
    fn an_absent_table_type_argument_is_an_empty_slice() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockCatalogArgsBackend>();
            let ret = sql_tables_w::<MockCatalogArgsBackend>(
                stmt,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let args = MockCatalogArgsBackend::recorded().expect("Backend::tables was called");
            assert!(args.table_types.is_empty());
            cleanup_for::<MockCatalogArgsBackend>(env, conn, stmt);
        }
    }

    /// Spec, "Arguments in Catalog Functions": under `METADATA_ID = SQL_TRUE`
    /// all four of `SQLColumns`' string arguments are identifiers —
    /// `ColumnName` included, which is the one an implementation is most
    /// likely to forget because it is the only argument no other catalog
    /// function has.
    #[test]
    fn metadata_id_normalises_the_column_argument_too() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockCatalogArgsBackend>();
            set_metadata_id_true::<MockCatalogArgsBackend>(stmt);

            let empty = utf16_of("");
            let table = utf16_of("my_table");
            let column = utf16_of("col_1");
            let ret = sql_columns_w::<MockCatalogArgsBackend>(
                stmt,
                empty.as_ptr(),
                SQL_NTS_I16,
                empty.as_ptr(),
                SQL_NTS_I16,
                table.as_ptr(),
                SQL_NTS_I16,
                column.as_ptr(),
                SQL_NTS_I16,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let args = MockCatalogArgsBackend::recorded().expect("Backend::columns was called");
            assert_eq!(args.table.as_deref(), Some("MY\\_TABLE"));
            assert_eq!(args.column.as_deref(), Some("COL\\_1"));
            cleanup_for::<MockCatalogArgsBackend>(env, conn, stmt);
        }
    }

    /// Core fills each `ColumnsQuery` field from the `SQLColumns` argument of
    /// the same name.
    ///
    /// The `METADATA_ID` test above pins the table and column arguments but
    /// passes an empty catalog and schema, so crossing those two was invisible
    /// to the suite. Same gap, same fix, as for `SQLTables`.
    #[test]
    fn sql_columns_fills_each_query_field_from_its_own_argument() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockCatalogArgsBackend>();

            let catalog = utf16_of("cat");
            let schema = utf16_of("sch");
            let table = utf16_of("tbl");
            let column = utf16_of("col");
            let ret = sql_columns_w::<MockCatalogArgsBackend>(
                stmt,
                catalog.as_ptr(),
                SQL_NTS_I16,
                schema.as_ptr(),
                SQL_NTS_I16,
                table.as_ptr(),
                SQL_NTS_I16,
                column.as_ptr(),
                SQL_NTS_I16,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let args = MockCatalogArgsBackend::recorded().expect("Backend::columns was called");
            assert_eq!(args.catalog.as_deref(), Some("cat"));
            assert_eq!(args.schema.as_deref(), Some("sch"));
            assert_eq!(args.table.as_deref(), Some("tbl"));
            assert_eq!(args.column.as_deref(), Some("col"));
            cleanup_for::<MockCatalogArgsBackend>(env, conn, stmt);
        }
    }

    /// `SQLForeignKeys` has two independent identifier trios, and the spec
    /// classifies all six as identifiers under `METADATA_ID`. Normalising only
    /// the PK trio is the plausible half-fix this pins against.
    #[test]
    fn metadata_id_normalises_both_foreign_key_trios() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockCatalogArgsBackend>();
            set_metadata_id_true::<MockCatalogArgsBackend>(stmt);

            let empty = utf16_of("");
            let pk_table = utf16_of("pk_t");
            let fk_table = utf16_of("fk_t");
            let ret = sql_foreign_keys_w::<MockCatalogArgsBackend>(
                stmt,
                empty.as_ptr(),
                SQL_NTS_I16,
                empty.as_ptr(),
                SQL_NTS_I16,
                pk_table.as_ptr(),
                SQL_NTS_I16,
                empty.as_ptr(),
                SQL_NTS_I16,
                empty.as_ptr(),
                SQL_NTS_I16,
                fk_table.as_ptr(),
                SQL_NTS_I16,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let args =
                MockCatalogArgsBackend::recorded().expect("Backend::foreign_keys was called");
            assert_eq!(args.table.as_deref(), Some("PK\\_T"));
            assert_eq!(args.fk_table.as_deref(), Some("FK\\_T"));
            cleanup_for::<MockCatalogArgsBackend>(env, conn, stmt);
        }
    }

    /// The ordering constraint between this task and the `SQL_ALL_*`
    /// enumerations: all three sentinels are the literal `"%"`, which
    /// normalisation would escape to `"\%"` — no longer the sentinel, so the
    /// enumeration would silently become an ordinary `Backend::tables` query.
    /// Enumeration detection therefore has to run on the raw arguments.
    #[test]
    fn an_enumeration_still_works_with_metadata_id_set() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockCatalogArgsBackend>();
            set_metadata_id_true::<MockCatalogArgsBackend>(stmt);

            let catalog = utf16_of(SQL_ALL_CATALOGS);
            let empty = utf16_of("");
            let ret = sql_tables_w::<MockCatalogArgsBackend>(
                stmt,
                catalog.as_ptr(),
                SQL_NTS_I16,
                empty.as_ptr(),
                SQL_NTS_I16,
                empty.as_ptr(),
                SQL_NTS_I16,
                std::ptr::null(),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(
                enumeration_values::<MockCatalogArgsBackend>(stmt, TABLE_CAT_COLUMN),
                vec!["cat_a"],
                "normalising before detecting the enumeration would have escaped the sentinel"
            );
            cleanup_for::<MockCatalogArgsBackend>(env, conn, stmt);
        }
    }

    /// Spec, `SQLTables` `HY009`: "The SQL_ATTR_METADATA_ID statement attribute
    /// was set to SQL_TRUE, the CatalogName argument was a null pointer, and
    /// the SQL_CATALOG_NAME InfoType returns that catalog names are supported."
    /// That sentence carries no `(DM)` marker, so it is the driver's to return
    /// — unlike the `SchemaName`/`TableName` sentence beside it, which is
    /// `(DM)`.
    #[test]
    fn metadata_id_with_null_catalog_returns_hy009() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockCatalogBackend>();
            set_metadata_id_true::<MockCatalogBackend>(stmt);

            let empty = utf16_of("");
            let ret = sql_tables_w::<MockCatalogBackend>(
                stmt,
                std::ptr::null(), // null CatalogName
                0,
                empty.as_ptr(),
                SQL_NTS_I16,
                empty.as_ptr(),
                SQL_NTS_I16,
                std::ptr::null(),
                0,
            );
            assert_eq!(ret, SqlReturn::ERROR);
            with_handle::<MockCatalogBackend, StatementHandle<MockCatalogBackend>, _>(stmt, |h| {
                let rec = h.diagnostics.get(0).expect("record 1 exists");
                assert_eq!(rec.sqlstate.as_str(), "HY009");
            });
            cleanup_for::<MockCatalogBackend>(env, conn, stmt);
        }
    }

    /// The same `HY009` clause's third conjunct: it applies only when
    /// `SQL_CATALOG_NAME` reports catalogs are supported. A data source with no
    /// catalogs has nothing for a catalog identifier to name, so a null pointer
    /// there is the only sensible thing an application can pass.
    /// `MockNoCatalogBackend` leaves `statistics` unimplemented, so core's
    /// empty-result-set fallback is the visible answer if no error is raised.
    #[test]
    fn metadata_id_null_catalog_is_accepted_without_catalog_support() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockNoCatalogBackend>();
            set_metadata_id_true::<MockNoCatalogBackend>(stmt);

            let table = utf16_of("t");
            let ret = sql_statistics_w::<MockNoCatalogBackend>(
                stmt,
                std::ptr::null(), // null CatalogName
                0,
                std::ptr::null(),
                0,
                table.as_ptr(),
                SQL_NTS_I16,
                SQL_INDEX_ALL,
                SQL_QUICK,
            );
            assert_eq!(
                ret,
                SqlReturn::SUCCESS,
                "the catalog HY009 clause is conditional on catalogs being supported"
            );
            cleanup_for::<MockNoCatalogBackend>(env, conn, stmt);
        }
    }

    /// Spec, `SQLPrimaryKeys` `HY009`: "(DM) The TableName argument was a null
    /// pointer." That marker means the Driver Manager returns it, so the driver
    /// must not — `SQLStatistics` and `SQLSpecialColumns` carry the same
    /// sentence *without* the marker, and only those two get a driver-side
    /// check.
    #[test]
    fn primary_keys_does_not_check_null_table_name() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockCatalogBackend>();
            let ret = sql_primary_keys_w::<MockCatalogBackend>(
                stmt,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(), // null TableName — the DM's job, not ours
                0,
            );
            assert_ne!(
                ret,
                SqlReturn::ERROR,
                "null TableName is (DM) for SQLPrimaryKeys; the driver must not reject it"
            );
            cleanup_for::<MockCatalogBackend>(env, conn, stmt);
        }
    }

    /// Spec, `SQLForeignKeys` `HY009`: "(DM) The PKTableName and FKTableName
    /// arguments were both null pointers." `(DM)` again, so both null is not
    /// the driver's to reject either.
    #[test]
    fn foreign_keys_does_not_check_null_table_names() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockCatalogBackend>();
            let ret = sql_foreign_keys_w::<MockCatalogBackend>(
                stmt,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(), // null PKTableName
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(), // and null FKTableName
                0,
            );
            assert_ne!(
                ret,
                SqlReturn::ERROR,
                "both table names null is (DM) for SQLForeignKeys; the driver must not reject it"
            );
            cleanup_for::<MockCatalogBackend>(env, conn, stmt);
        }
    }

    /// Spec, `SQLStatistics` `HY009`: "The TableName argument was a null
    /// pointer." No `(DM)` marker on this sentence, unlike `SQLPrimaryKeys`'
    /// identical one.
    #[test]
    fn statistics_does_check_null_table_name() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockCatalogBackend>();
            let ret = sql_statistics_w::<MockCatalogBackend>(
                stmt,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(), // null TableName — the driver's job here
                0,
                SQL_INDEX_ALL,
                SQL_QUICK,
            );
            assert_eq!(ret, SqlReturn::ERROR);
            with_handle::<MockCatalogBackend, StatementHandle<MockCatalogBackend>, _>(stmt, |h| {
                let rec = h.diagnostics.get(0).expect("record 1 exists");
                assert_eq!(rec.sqlstate.as_str(), "HY009");
            });
            cleanup_for::<MockCatalogBackend>(env, conn, stmt);
        }
    }

    /// Spec, `SQLSpecialColumns` `HY009`: "The TableName argument was a null
    /// pointer." Unmarked, like `SQLStatistics`'.
    #[test]
    fn special_columns_does_check_null_table_name() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockCatalogBackend>();
            let ret = sql_special_columns_w::<MockCatalogBackend>(
                stmt,
                SQL_BEST_ROWID,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(), // null TableName — the driver's job here
                0,
                SQL_SCOPE_CURROW,
                Nullable::SqlNullable as u16,
            );
            assert_eq!(ret, SqlReturn::ERROR);
            with_handle::<MockCatalogBackend, StatementHandle<MockCatalogBackend>, _>(stmt, |h| {
                let rec = h.diagnostics.get(0).expect("record 1 exists");
                assert_eq!(rec.sqlstate.as_str(), "HY009");
            });
            cleanup_for::<MockCatalogBackend>(env, conn, stmt);
        }
    }

    /// Install a result set with a single column named "abcde" (5 characters,
    /// 10 bytes in UTF-16) so buffer-length units are unambiguous.
    unsafe fn stmt_with_named_column(stmt: *mut c_void) {
        with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
            handle.set_result_set(StatementData::Synthetic(SyntheticStatement::new(
                vec![ColumnDescriptor {
                    name: "abcde".into(),
                    type_name: String::new(),
                    sql_type: SqlDataType::VARCHAR,
                    precision: 10,
                    scale: 0,
                    nullable: Nullable::SqlNullable,
                    ..Default::default()
                }],
                vec![],
            )));
        });
    }

    /// Install a result set with one column whose length the backend could not
    /// determine — the shape a `DESCRIBE`/`SHOW`/`EXPLAIN` result has, where
    /// every column is an unbounded VARCHAR with no catalog entry to read a
    /// declared length from.
    unsafe fn stmt_with_undeterminable_column(stmt: *mut c_void) {
        with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
            handle.set_result_set(StatementData::Synthetic(SyntheticStatement::new(
                vec![ColumnDescriptor {
                    name: "unbounded".into(),
                    type_name: String::new(),
                    sql_type: SqlDataType::VARCHAR,
                    precision: crate::types::PRECISION_UNDETERMINABLE,
                    scale: 0,
                    nullable: Nullable::SqlNullable,
                    ..Default::default()
                }],
                vec![],
            )));
        });
    }

    #[test]
    fn col_attribute_reports_string_length_in_bytes() {
        // Spec, SQLColAttribute StringLengthPtr: "the total number of bytes
        // (excluding the null-termination character for character data)".
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            stmt_with_named_column(stmt);
            let mut buf = [0u16; 32];
            let mut len: i16 = 0;
            let ret = sql_col_attribute_w::<MockBackend>(
                stmt,
                1,
                Desc::Name as u16,
                buf.as_mut_ptr().cast(),
                i16::try_from(buf.len() * 2).expect("buffer fits i16"),
                &mut len,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(len, 10, "5 characters is 10 bytes in UTF-16");
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn col_attribute_reports_byte_length_when_buffer_is_null() {
        // Spec: "If CharacterAttributePtr is NULL, StringLengthPtr will still
        // return the total number of bytes."
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            stmt_with_named_column(stmt);
            let mut len: i16 = 0;
            let ret = sql_col_attribute_w::<MockBackend>(
                stmt,
                1,
                Desc::Name as u16,
                std::ptr::null_mut(),
                0,
                &mut len,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(len, 10);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn describe_col_still_reports_characters() {
        // Spec, SQLDescribeCol NameLengthPtr: "the total number of characters".
        // This function is already correct and must not be changed.
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            stmt_with_named_column(stmt);
            let mut buf = [0u16; 32];
            let mut name_len: i16 = 0;
            let mut data_type: i16 = 0;
            let mut col_size: usize = 0;
            let mut decimal_digits: i16 = 0;
            let mut nullable: i16 = 0;
            let ret = sql_describe_col_w::<MockBackend>(
                stmt,
                1,
                buf.as_mut_ptr(),
                i16::try_from(buf.len()).expect("buffer fits i16"),
                &mut name_len,
                &mut data_type,
                &mut col_size,
                &mut decimal_digits,
                &mut nullable,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(name_len, 5, "5 characters, not 10 bytes");
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn describe_col_reports_zero_column_size_when_undeterminable() {
        // Spec, SQLDescribeCol ColumnSizePtr: "If the column size cannot be
        // determined, the driver returns 0."
        //
        // The regression this pins: `SQL_NO_TOTAL` is -4, and -4 widened into
        // the `SQLULEN` this parameter actually is reads back as
        // 18_446_744_073_709_551_612.
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            stmt_with_undeterminable_column(stmt);
            let mut buf = [0u16; 32];
            let mut name_len: i16 = 0;
            let mut data_type: i16 = 0;
            let mut col_size: usize = 12345; // poisoned, so a skipped write fails
            let mut decimal_digits: i16 = 0;
            let mut nullable: i16 = 0;
            let ret = sql_describe_col_w::<MockBackend>(
                stmt,
                1,
                buf.as_mut_ptr(),
                i16::try_from(buf.len()).expect("buffer fits i16"),
                &mut name_len,
                &mut data_type,
                &mut col_size,
                &mut decimal_digits,
                &mut nullable,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(
                col_size, 0,
                "spec says 0 when the column size cannot be determined"
            );
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn primary_keys_not_connected_returns_error() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = sql_primary_keys_w::<MockBackend>(
                stmt,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
            );
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn foreign_keys_not_connected_returns_error() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = sql_foreign_keys_w::<MockBackend>(
                stmt,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
            );
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn tables_not_connected_returns_error() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = sql_tables_w::<MockBackend>(
                stmt,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
            );
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn columns_not_connected_returns_error() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = sql_columns_w::<MockBackend>(
                stmt,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
            );
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn, stmt);
        }
    }

    // -----------------------------------------------------------------------
    // SQLStatisticsW / SQLSpecialColumnsW
    // -----------------------------------------------------------------------

    #[test]
    fn statistics_w_not_connected_returns_error() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = sql_statistics_w::<MockBackend>(
                stmt,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                0,
                0,
            );
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn special_columns_w_not_connected_returns_error() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = sql_special_columns_w::<MockBackend>(
                stmt,
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                0,
                0,
            );
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn statistics_falls_back_to_empty_result_set_when_unimplemented() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            // MockBackend uses the default statistics() -> NotImplemented.
            with_handle::<MockBackend, ConnectionHandle<MockBackend>, _>(conn, |conn_ref| {
                conn_ref.connection = Some(MockConnection);
            });

            let table = utf16_of("t");
            let ret = sql_statistics_w::<MockBackend>(
                stmt,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                table.as_ptr(),
                SQL_NTS_I16,
                SQL_INDEX_ALL,
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let mut cols: i16 = 0;
            assert_eq!(
                crate::ffi::cursor::sql_num_result_cols::<MockBackend>(stmt, &mut cols),
                SqlReturn::SUCCESS
            );
            assert_eq!(cols, 13);
            assert_eq!(
                crate::ffi::fetch::sql_fetch::<MockBackend>(stmt),
                SqlReturn::NO_DATA
            );
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn special_columns_falls_back_to_empty_result_set_when_unimplemented() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            with_handle::<MockBackend, ConnectionHandle<MockBackend>, _>(conn, |conn_ref| {
                conn_ref.connection = Some(MockConnection);
            });

            let table = utf16_of("t");
            let ret = sql_special_columns_w::<MockBackend>(
                stmt,
                SQL_BEST_ROWID,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                table.as_ptr(),
                SQL_NTS_I16,
                SQL_SCOPE_CURROW,
                Nullable::SqlNullable as u16,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let mut cols: i16 = 0;
            assert_eq!(
                crate::ffi::cursor::sql_num_result_cols::<MockBackend>(stmt, &mut cols),
                SqlReturn::SUCCESS
            );
            assert_eq!(cols, 8);
            assert_eq!(
                crate::ffi::fetch::sql_fetch::<MockBackend>(stmt),
                SqlReturn::NO_DATA
            );
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn statistics_not_connected_returns_error() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = sql_statistics_w::<MockBackend>(
                stmt,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                SQL_INDEX_ALL,
                0,
            );
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn special_columns_not_connected_returns_error() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = sql_special_columns_w::<MockBackend>(
                stmt,
                SQL_BEST_ROWID,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                SQL_SCOPE_CURROW,
                Nullable::SqlNullable as u16,
            );
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn describe_col_no_result_set_returns_error() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let mut name_buf = [0u16; 64];
            let mut name_len: i16 = 0;
            let mut data_type: i16 = 0;
            let mut size: ULen = 0;
            let mut decimal: i16 = 0;
            let mut nullable: i16 = 0;

            let ret = sql_describe_col_w::<MockBackend>(
                stmt,
                1,
                name_buf.as_mut_ptr(),
                64,
                &mut name_len,
                &mut data_type,
                &mut size,
                &mut decimal,
                &mut nullable,
            );
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn, stmt);
        }
    }

    /// A null table_name pointer is a valid ODBC wildcard ("all tables"). Verify the null-pointer
    /// path through parse_filter_param doesn't crash; MockBackend::columns returns Err so the
    /// overall call returns ERROR, but the error comes from the backend, not a null dereference.
    #[test]
    fn columns_null_table_name_connected_reaches_backend() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            // Connect so we pass the HY010 check.
            let cs: Vec<u16> = "Host=localhost;User=u;Password=p".encode_utf16().collect();
            let _ = crate::ffi::connect::sql_driver_connect_w::<MockBackend>(
                conn,
                std::ptr::null_mut(),
                cs.as_ptr(),
                cs.len() as i16,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                0,
            );
            // All name pointers null; null means wildcard per spec.
            let ret = sql_columns_w::<MockBackend>(
                stmt,
                std::ptr::null(), // catalog_name
                0,
                std::ptr::null(), // schema_name
                0,
                std::ptr::null(), // table_name — null is valid ("all tables")
                0,
                std::ptr::null(), // column_name
                0,
            );
            // MockBackend::columns returns Err, so we get ERROR from the backend, not a crash.
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn col_attribute_no_result_set_returns_error() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let mut num_attr: isize = 0;

            let ret = sql_col_attribute_w::<MockBackend>(
                stmt,
                1,
                Desc::Type as u16,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut num_attr,
            );
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn describe_col_column_zero_returns_07009() {
        // Spec 07009: Column 0 (bookmark) is not supported.
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let mut name_buf = [0u16; 64];
            let mut name_len: i16 = 0;
            let mut data_type: i16 = 0;
            let mut size: ULen = 0;
            let mut decimal: i16 = 0;
            let mut nullable: i16 = 0;

            // Even without a result set, column 0 should be rejected
            // before the result-set check (but currently the no-result-set check
            // comes first; either way, column 0 returns ERROR).
            let ret = sql_describe_col_w::<MockBackend>(
                stmt,
                0,
                name_buf.as_mut_ptr(),
                64,
                &mut name_len,
                &mut data_type,
                &mut size,
                &mut decimal,
                &mut nullable,
            );
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn, stmt);
        }
    }

    // -----------------------------------------------------------------------
    // SQLProceduresW / SQLProcedureColumnsW
    // -----------------------------------------------------------------------

    /// Helper: connect a connection handle using MockBackend.
    unsafe fn connect_handle(conn: *mut c_void) -> SqlReturn {
        let input = "Host=localhost;Port=8080;Database=test;User=me";
        let wide: Vec<u16> = input.encode_utf16().collect();
        unsafe {
            crate::ffi::connect::sql_driver_connect_w::<MockBackend>(
                conn,
                std::ptr::null_mut(),
                wide.as_ptr(),
                wide.len() as i16,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                0,
            )
        }
    }

    #[test]
    fn procedures_returns_success_with_connected_handle() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let cr = connect_handle(conn);
            assert_eq!(cr, SqlReturn::SUCCESS);

            let ret = sql_procedures_w::<MockBackend>(
                stmt,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            // Verify the synthetic result set has exactly 8 columns.
            let col_count =
                with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                    match &handle.statement {
                        Some(StatementData::Synthetic(s)) => s.column_count(),
                        _ => panic!("expected synthetic statement"),
                    }
                });
            assert_eq!(col_count, 8);

            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn procedures_returns_error_without_connection() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = sql_procedures_w::<MockBackend>(
                stmt,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
            );
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn procedure_columns_returns_success_with_connected_handle() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let cr = connect_handle(conn);
            assert_eq!(cr, SqlReturn::SUCCESS);

            let ret = sql_procedure_columns_w::<MockBackend>(
                stmt,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            // Verify the synthetic result set has exactly 19 columns.
            let col_count =
                with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                    match &handle.statement {
                        Some(StatementData::Synthetic(s)) => s.column_count(),
                        _ => panic!("expected synthetic statement"),
                    }
                });
            assert_eq!(col_count, 19);

            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn column_privileges_returns_success_with_connected_handle() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let cr = connect_handle(conn);
            assert_eq!(cr, SqlReturn::SUCCESS);

            // `TableName` must not be null here: the spec states that clause
            // without a `(DM)` marker, so the driver rejects it with HY009.
            let table = utf16_of("t");
            let ret = sql_column_privileges_w::<MockBackend>(
                stmt,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                table.as_ptr(),
                SQL_NTS_I16,
                std::ptr::null(),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            // Verify the synthetic result set has exactly 8 columns.
            let col_count =
                with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                    match &handle.statement {
                        Some(StatementData::Synthetic(s)) => s.column_count(),
                        _ => panic!("expected synthetic statement"),
                    }
                });
            assert_eq!(col_count, 8);

            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn table_privileges_returns_success_with_connected_handle() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let cr = connect_handle(conn);
            assert_eq!(cr, SqlReturn::SUCCESS);

            let ret = sql_table_privileges_w::<MockBackend>(
                stmt,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            // Verify the synthetic result set has exactly 7 columns.
            let col_count =
                with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                    match &handle.statement {
                        Some(StatementData::Synthetic(s)) => s.column_count(),
                        _ => panic!("expected synthetic statement"),
                    }
                });
            assert_eq!(col_count, 7);

            cleanup(env, conn, stmt);
        }
    }

    // -----------------------------------------------------------------------
    // A describe failure is the backend's error, not "column out of range"
    // -----------------------------------------------------------------------

    /// Env + connection + statement with a cursor open, for an arbitrary
    /// backend, so a describe reaches the backend at all.
    unsafe fn described_stmt_for<B: Backend>() -> (*mut c_void, *mut c_void, *mut c_void) {
        unsafe {
            let (env, conn, stmt) = crate::test_utils::alloc_connected_env_conn_stmt::<B>();
            let sql: Vec<u16> = "SELECT 1".encode_utf16().collect();
            assert_eq!(
                crate::ffi::execute::sql_exec_direct_w::<B>(
                    stmt,
                    sql.as_ptr(),
                    i32::try_from(sql.len()).expect("SQL fits in i32"),
                ),
                SqlReturn::SUCCESS,
                "precondition: a cursor is open, so describe_col is reachable",
            );
            (env, conn, stmt)
        }
    }

    /// Read the first diagnostic's SQLSTATE off a statement handle.
    fn first_sqlstate<B: Backend>(stmt: *mut c_void) -> String {
        with_handle::<B, StatementHandle<B>, _>(stmt, |h| {
            h.diagnostics
                .get(0)
                .expect("a diagnostic record")
                .sqlstate
                .as_str()
                .to_owned()
        })
    }

    /// The whole catalog family resolves its name arguments through one helper,
    /// `parse_filter_param`, so one `SQL_NTS` argument running to
    /// `MAX_NTS_SCAN` is `HY090` for all ten of them. A truncated *filter* is
    /// the quietest failure in the family: it returns a result set, just the
    /// wrong one, so an application sees a table list that is missing entries
    /// with no diagnostic to say why.
    ///
    /// `SQLTables` stands for the family here; `SQLForeignKeys` below covers
    /// the part a shared helper cannot — telling six name arguments apart.
    ///
    /// The buffer is exactly the cap, so an over-read is a heap overflow Miri
    /// sees rather than a longer filter.
    #[test]
    fn tables_refuses_an_nts_filter_that_runs_to_the_scan_cap() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockBackend>();
            let wide = vec![b'a' as u16; crate::utf16::MAX_NTS_SCAN];

            assert_eq!(
                sql_tables_w::<MockBackend>(
                    stmt,
                    wide.as_ptr(),
                    crate::types::SQL_NTS as i16,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    0,
                ),
                SqlReturn::ERROR,
            );
            assert_eq!(
                first_sqlstate::<MockBackend>(stmt),
                crate::types::sql_state::INVALID_STRING_OR_BUFFER_LENGTH
            );

            cleanup_for::<MockBackend>(env, conn, stmt);
        }
    }

    /// The diagnostic names *which* argument overran. `SQLForeignKeys` takes
    /// six name arguments, and "a string argument was too long" would identify
    /// none of them — which is why `parse_filter_param` carries the spec's own
    /// argument name rather than letting the shared helper phrase it.
    #[test]
    fn foreign_keys_names_the_argument_whose_nts_scan_overran() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockBackend>();
            let wide = vec![b'a' as u16; crate::utf16::MAX_NTS_SCAN];
            let nts = crate::types::SQL_NTS as i16;

            assert_eq!(
                sql_foreign_keys_w::<MockBackend>(
                    stmt,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    0,
                    wide.as_ptr(),
                    nts,
                ),
                SqlReturn::ERROR,
            );
            let message = with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |h| {
                h.diagnostics
                    .get(0)
                    .expect("a diagnostic record")
                    .message
                    .clone()
            });
            assert!(
                message.contains("FKTableName"),
                "the diagnostic must name the overrunning argument, got: {message}"
            );

            cleanup_for::<MockBackend>(env, conn, stmt);
        }
    }

    /// The accepting side for the family: a terminator in the last position the
    /// scan may read is a complete filter, not an error.
    #[test]
    fn tables_accepts_an_nts_filter_terminated_at_the_last_scannable_position() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockCatalogBackend>();
            let mut wide = vec![b'a' as u16; crate::utf16::MAX_NTS_SCAN];
            wide[crate::utf16::MAX_NTS_SCAN - 1] = 0;

            assert_eq!(
                sql_tables_w::<MockCatalogBackend>(
                    stmt,
                    wide.as_ptr(),
                    crate::types::SQL_NTS as i16,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    0,
                ),
                SqlReturn::SUCCESS,
            );

            cleanup_for::<MockCatalogBackend>(env, conn, stmt);
        }
    }

    /// The defect: `map_err(|_| ...)` threw the backend's error away and told
    /// the application the column number was out of range, whatever had
    /// actually gone wrong — a link failure, a cancellation, anything.
    ///
    /// Column 1 of a two-column result set, so the range check core now does
    /// first cannot be what produced the answer.
    #[test]
    fn describe_col_reports_the_backends_error_not_07009() {
        unsafe {
            let (env, conn, stmt) = described_stmt_for::<MockFailingDescribeBackend>();

            let mut data_type: i16 = 0;
            assert_eq!(
                sql_describe_col_w::<MockFailingDescribeBackend>(
                    stmt,
                    1,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    &mut data_type,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                ),
                SqlReturn::ERROR,
            );
            assert_eq!(
                first_sqlstate::<MockFailingDescribeBackend>(stmt),
                "08S01",
                "a link failure must not be reported as a bad column number",
            );

            crate::test_utils::cleanup_connected_env_conn_stmt::<MockFailingDescribeBackend>(
                env, conn, stmt,
            );
        }
    }

    /// The other half of the same claim: `07009` survives for the case its
    /// message actually describes. Run against the *same* mock as the test
    /// above, so the pair proves core tells the two apart rather than having
    /// swapped one blanket answer for another.
    #[test]
    fn describe_col_still_reports_07009_for_a_column_past_the_end() {
        unsafe {
            let (env, conn, stmt) = described_stmt_for::<MockFailingDescribeBackend>();

            let mut data_type: i16 = 0;
            assert_eq!(
                sql_describe_col_w::<MockFailingDescribeBackend>(
                    stmt,
                    3, // the mock reports 2 columns
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    &mut data_type,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                ),
                SqlReturn::ERROR,
            );
            assert_eq!(
                first_sqlstate::<MockFailingDescribeBackend>(stmt),
                "07009",
                "a column past the end is the one case 07009 is for",
            );

            crate::test_utils::cleanup_connected_env_conn_stmt::<MockFailingDescribeBackend>(
                env, conn, stmt,
            );
        }
    }

    /// `HY008` reaching `SQLDescribeColW` at all is what a blanket `map_err` here
    /// used to prevent: it overwrote the SQLSTATE unconditionally, so
    /// reclassifying a cancelled call was a no-op. `SQLCancel` from this thread
    /// signals the execution's token; the failing describe then reports the
    /// cancellation rather than the link failure that was its symptom.
    #[test]
    fn describe_col_reports_hy008_when_the_statement_was_cancelled() {
        unsafe {
            let (env, conn, stmt) = described_stmt_for::<MockFailingDescribeBackend>();

            assert_eq!(
                crate::ffi::cursor::sql_cancel::<MockFailingDescribeBackend>(stmt),
                SqlReturn::SUCCESS,
                "precondition: the execution's cancel token is signalled",
            );

            let mut data_type: i16 = 0;
            assert_eq!(
                sql_describe_col_w::<MockFailingDescribeBackend>(
                    stmt,
                    1,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    &mut data_type,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                ),
                SqlReturn::ERROR,
            );
            assert_eq!(
                first_sqlstate::<MockFailingDescribeBackend>(stmt),
                "HY008",
                "a cancelled describe reports the cancellation, not its symptom",
            );

            crate::test_utils::cleanup_connected_env_conn_stmt::<MockFailingDescribeBackend>(
                env, conn, stmt,
            );
        }
    }

    /// `SQLColAttributeW` carries the identical defect at its own call site.
    ///
    /// Its diagnostics table has no `08S01` row, but its page states that after
    /// `SQLPrepare` and before `SQLExecute` it "can return any SQLSTATE that can
    /// be returned by SQLPrepare or SQLExecute", so a backend's `08S01` passing
    /// through is legal — and far more useful than `07009`.
    #[test]
    fn col_attribute_reports_the_backends_error_not_07009() {
        unsafe {
            let (env, conn, stmt) = described_stmt_for::<MockFailingDescribeBackend>();

            let mut numeric: isize = 0;
            assert_eq!(
                sql_col_attribute_w::<MockFailingDescribeBackend>(
                    stmt,
                    1,
                    Desc::ConciseType as u16,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    &mut numeric,
                ),
                SqlReturn::ERROR,
            );
            assert_eq!(
                first_sqlstate::<MockFailingDescribeBackend>(stmt),
                "08S01",
                "a link failure must not be reported as a bad column number",
            );

            crate::test_utils::cleanup_connected_env_conn_stmt::<MockFailingDescribeBackend>(
                env, conn, stmt,
            );
        }
    }

    /// The `SQLColAttributeW` half of the range-check pair.
    #[test]
    fn col_attribute_still_reports_07009_for_a_column_past_the_end() {
        unsafe {
            let (env, conn, stmt) = described_stmt_for::<MockFailingDescribeBackend>();

            let mut numeric: isize = 0;
            assert_eq!(
                sql_col_attribute_w::<MockFailingDescribeBackend>(
                    stmt,
                    3, // the mock reports 2 columns
                    Desc::ConciseType as u16,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    &mut numeric,
                ),
                SqlReturn::ERROR,
            );
            assert_eq!(
                first_sqlstate::<MockFailingDescribeBackend>(stmt),
                "07009",
                "a column past the end is the one case 07009 is for",
            );

            crate::test_utils::cleanup_connected_env_conn_stmt::<MockFailingDescribeBackend>(
                env, conn, stmt,
            );
        }
    }

    /// The `SQLColAttributeW` half of the cancellation claim.
    #[test]
    fn col_attribute_reports_hy008_when_the_statement_was_cancelled() {
        unsafe {
            let (env, conn, stmt) = described_stmt_for::<MockFailingDescribeBackend>();

            assert_eq!(
                crate::ffi::cursor::sql_cancel::<MockFailingDescribeBackend>(stmt),
                SqlReturn::SUCCESS,
                "precondition: the execution's cancel token is signalled",
            );

            let mut numeric: isize = 0;
            assert_eq!(
                sql_col_attribute_w::<MockFailingDescribeBackend>(
                    stmt,
                    1,
                    Desc::ConciseType as u16,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    &mut numeric,
                ),
                SqlReturn::ERROR,
            );
            assert_eq!(
                first_sqlstate::<MockFailingDescribeBackend>(stmt),
                "HY008",
                "a cancelled describe reports the cancellation, not its symptom",
            );

            crate::test_utils::cleanup_connected_env_conn_stmt::<MockFailingDescribeBackend>(
                env, conn, stmt,
            );
        }
    }

    /// [`column_count_upper_bound`] in isolation, sidestepping the backend
    /// entirely: a negative count saturates up to `u16::MAX` rather than down
    /// to 0, while an ordinary positive count (including `i16::MAX`, the
    /// largest a backend can ever report) passes through unchanged.
    #[test]
    fn column_count_upper_bound_saturates_up_not_down() {
        assert_eq!(
            column_count_upper_bound(-1),
            u16::MAX,
            "a negative count must not collapse to 0, which would reject every column",
        );
        assert_eq!(
            column_count_upper_bound(i16::MIN),
            u16::MAX,
            "the most negative representable count still saturates up, not down",
        );
        assert_eq!(
            column_count_upper_bound(0),
            0,
            "an empty result set is 0 columns"
        );
        assert_eq!(
            column_count_upper_bound(2),
            2,
            "an ordinary positive count passes through unchanged",
        );
        assert_eq!(
            column_count_upper_bound(i16::MAX),
            u16::try_from(i16::MAX).expect("i16::MAX fits u16"),
            "the largest count a backend can report still fits u16 and passes through",
        );
    }

    /// Task 2.10's defect: `u16::try_from(column_count).unwrap_or(0)` collapses
    /// an unrepresentable count to 0, and the `column_number > 0` check that
    /// follows then rejects every column, including valid ones. A backend
    /// cannot report *more* than `u16::MAX` columns through
    /// `StatementBackend::column_count` — it returns `i16`, whose max already
    /// fits — so the only way to hit the failed conversion is a *negative*
    /// count, which `MockNegativeColumnCountBackend` reports.
    ///
    /// Column 1 must still reach `describe_col` and succeed: saturating the
    /// unrepresentable count up to `u16::MAX` makes the range check
    /// permissive rather than rejecting everything, leaving the backend's own
    /// answer as the real gate.
    #[test]
    fn describe_col_succeeds_when_backend_column_count_is_negative() {
        unsafe {
            let (env, conn, stmt) = described_stmt_for::<MockNegativeColumnCountBackend>();

            let mut data_type: i16 = 0;
            assert_eq!(
                sql_describe_col_w::<MockNegativeColumnCountBackend>(
                    stmt,
                    1,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    &mut data_type,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                ),
                SqlReturn::SUCCESS,
                "a negative backend column count must not reject every column as 07009",
            );

            crate::test_utils::cleanup_connected_env_conn_stmt::<MockNegativeColumnCountBackend>(
                env, conn, stmt,
            );
        }
    }

    /// The `SQLColAttributeW` half of the same claim — it carries an
    /// identical range check at its own call site.
    #[test]
    fn col_attribute_succeeds_when_backend_column_count_is_negative() {
        unsafe {
            let (env, conn, stmt) = described_stmt_for::<MockNegativeColumnCountBackend>();

            let mut numeric: isize = 0;
            assert_eq!(
                sql_col_attribute_w::<MockNegativeColumnCountBackend>(
                    stmt,
                    1,
                    Desc::ConciseType as u16,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    &mut numeric,
                ),
                SqlReturn::SUCCESS,
                "a negative backend column count must not reject every column as 07009",
            );

            crate::test_utils::cleanup_connected_env_conn_stmt::<MockNegativeColumnCountBackend>(
                env, conn, stmt,
            );
        }
    }
}
