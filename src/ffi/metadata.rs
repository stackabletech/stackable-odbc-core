//! FFI implementations for ODBC metadata functions:
//! SQLTablesW, SQLColumnsW, SQLStatisticsW, SQLSpecialColumnsW,
//! SQLPrimaryKeysW, SQLForeignKeysW, SQLDescribeColW, SQLColAttributeW,
//! SQLProceduresW, SQLProcedureColumnsW, SQLColumnPrivilegesW,
//! SQLTablePrivilegesW.

use std::ffi::c_void;

use crate::backend::{Backend, StatementBackend};
use crate::errors::{IntoOdbc, OdbcError};
use crate::handles::scope::HandleScope;
use crate::handles::{StatementData, StatementHandle};
use crate::panic::panic_safe;
use crate::synthetic::SyntheticStatement;
use crate::types::col_attr::{ColAttrValue, get_column_attribute};
use crate::types::{
    CatalogResultColumnWidths, ColumnDescriptor, Desc, Nullable, PRIVILEGE_LEN, SQL_INDEX_UNIQUE,
    SqlReturn, SqlState, ULen, YES_NO_LEN, character, identifier, identifier_type_from_raw,
    integer, nullable_from_raw, scope_from_raw, smallint, special_columns_columns,
    statistics_columns,
};
use crate::utf16::{utf16_to_string, write_utf16};

/// Parse a UTF-16 filter parameter. Returns `None` if the pointer is null,
/// otherwise parses the string and returns `Some`.
///
/// # Safety
///
/// `ptr` must be valid for `len` u16 elements (or null-terminated if len is SQL_NTS).
unsafe fn parse_filter_param(ptr: *const u16, len: i16) -> Result<Option<String>, OdbcError> {
    if ptr.is_null() {
        return Ok(None);
    }
    // SAFETY: ptr is non-null (checked above) and points to a valid sequence of `len` UTF-16
    // code units, or is null-terminated if len == SQL_NTS; caller upholds this invariant per
    // the function's own safety contract.
    let s = unsafe { utf16_to_string(ptr, len.into())? };
    Ok(Some(s))
}

/// Generic implementation of SQLTablesW.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqltables-function>
///
/// Queries the backend for table metadata and stores the result set in the
/// statement handle.
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
/// - 40001: Serialization failure; propagated from backend as `HY000`.
/// - 40003: Statement completion unknown; propagated from backend as `HY000`.
/// - HY000: General error; returned for any unexpected backend error.
/// - HY001: Memory allocation error; returned if allocation fails.
/// - HY008: Operation canceled (DM) (driver-manager-handled; not returned here).
/// - HY009: Invalid use of null pointer (DM) (driver-manager-handled; not returned by this
///   driver).
/// - HY010: Function sequence error — returned if connection is not open (HY010). DM cases
///   (async, etc.) are driver-manager-handled; not returned here.
/// - HY013: Memory management error; returned if underlying allocation fails.
/// - HY090: Invalid string or buffer length (DM) (driver-manager-handled; not returned by this
///   driver).
/// - HY117: Connection suspended (DM) (driver-manager-handled; not returned here).
/// - HYC00: Optional feature not implemented; propagated from backend if catalogs/schemas are
///   unsupported.
/// - HYT00: Timeout expired; propagated from backend as `HY000`.
/// - HYT01: Connection timeout expired; propagated from backend as `HY000`.
/// - IM001: Driver does not support this function (DM) (driver-manager-handled; not returned here).
/// - IM017: Polling disabled (DM) (driver-manager-handled; not returned here).
/// - IM018: SQLCompleteAsync not called (DM) (driver-manager-handled; not returned here).
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

            let catalog = parse_filter_param(catalog_name, name_length1)?;
            let schema = parse_filter_param(schema_name, name_length2)?;
            let table = parse_filter_param(table_name, name_length3)?;
            let tt = parse_filter_param(table_type, name_length4)?;
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
            let Some(ref connection) = conn.connection else {
                return Err(OdbcError::general(
                    "Connection is not open",
                    SqlState::function_sequence_error(),
                ));
            };

            // The token exists once this statement makes its first
            // backend call; created here on demand, then reused for every
            // later call on the same statement (see `resolve_cancel_token`).
            let cancel_token =
                crate::handles::resolve_cancel_token::<B>(statement_handle, connection);
            let cancel = crate::handles::cancel_as::<B>(&cancel_token)?;

            let result = B::tables(
                connection,
                cancel,
                catalog.as_deref(),
                schema.as_deref(),
                table.as_deref(),
                tt.as_deref(),
            )
            .into_odbc()?;

            stmt.set_result_set(StatementData::Backend(result));
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
/// - 40001: Serialization failure; propagated from backend as `HY000`.
/// - 40003: Statement completion unknown; propagated from backend as `HY000`.
/// - HY000: General error; returned for any unexpected backend error.
/// - HY001: Memory allocation error; returned if allocation fails.
/// - HY008: Operation canceled (DM) (driver-manager-handled; not returned here).
/// - HY009: Invalid use of null pointer (DM) (driver-manager-handled; not returned by this
///   driver).
/// - HY010: Function sequence error — returned if connection is not open (HY010). DM cases
///   (async, etc.) are driver-manager-handled; not returned here.
/// - HY013: Memory management error; returned if underlying allocation fails.
/// - HY090: Invalid string or buffer length (DM) (driver-manager-handled; not returned by this
///   driver).
/// - HY117: Connection suspended (DM) (driver-manager-handled; not returned here).
/// - HYC00: Optional feature not implemented; propagated from backend if catalogs/schemas are
///   unsupported.
/// - HYT00: Timeout expired; propagated from backend as `HY000`.
/// - HYT01: Connection timeout expired; propagated from backend as `HY000`.
/// - IM001: Driver does not support this function (DM) (driver-manager-handled; not returned here).
/// - IM017: Polling disabled (DM) (driver-manager-handled; not returned here).
/// - IM018: SQLCompleteAsync not called (DM) (driver-manager-handled; not returned here).
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

            let catalog = parse_filter_param(catalog_name, name_length1)?;
            let schema = parse_filter_param(schema_name, name_length2)?;
            let table = parse_filter_param(table_name, name_length3)?;
            let column = parse_filter_param(column_name, name_length4)?;
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
            let Some(ref connection) = conn.connection else {
                return Err(OdbcError::general(
                    "Connection is not open",
                    SqlState::function_sequence_error(),
                ));
            };

            // The token exists once this statement makes its first
            // backend call; created here on demand, then reused for every
            // later call on the same statement (see `resolve_cancel_token`).
            let cancel_token =
                crate::handles::resolve_cancel_token::<B>(statement_handle, connection);
            let cancel = crate::handles::cancel_as::<B>(&cancel_token)?;

            let result = B::columns(
                connection,
                cancel,
                catalog.as_deref(),
                schema.as_deref(),
                table.as_deref(),
                column.as_deref(),
            )
            .into_odbc()?;

            stmt.set_result_set(StatementData::Backend(result));
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
/// - 24000: Invalid cursor state — returned if a cursor is already open on this statement.
///   Note: the spec marks this (DM) for some subcases; the driver also returns it directly.
/// - 40001: Serialization failure; propagated from backend as `HY000`.
/// - 40003: Statement completion unknown; propagated from backend as `HY000`.
/// - HY000: General error; returned for any unexpected backend error.
/// - HY001: Memory allocation error; returned if allocation fails.
/// - HY008: Operation canceled (DM) (driver-manager-handled; not returned here).
/// - HY009: Invalid use of null pointer (DM) (driver-manager-handled; not returned by this
///   driver). Note: the spec requires `TableName` to not be NULL, but that check is DM-only.
/// - HY010: Function sequence error — returned if connection is not open. DM cases (async, etc.)
///   are driver-manager-handled; not returned here.
/// - HY013: Memory management error; returned if underlying allocation fails.
/// - HY090: Invalid string or buffer length (DM) (driver-manager-handled; not returned by this
///   driver).
/// - HY117: Connection suspended (DM) (driver-manager-handled; not returned here).
/// - HYC00: Optional feature not implemented; propagated from backend if catalogs/schemas are
///   unsupported.
/// - HYT00: Timeout expired; propagated from backend as `HY000`.
/// - HYT01: Connection timeout expired; propagated from backend as `HY000`.
/// - IM001: Driver does not support this function (DM) (driver-manager-handled; not returned here).
/// - IM017: Polling disabled (DM) (driver-manager-handled; not returned here).
/// - IM018: SQLCompleteAsync not called (DM) (driver-manager-handled; not returned here).
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
    tracing::debug!("SQLPrimaryKeysW(stmt={:?})", statement_handle);
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

            let Some(ref connection) = conn.connection else {
                return Err(OdbcError::general(
                    "Connection is not open",
                    SqlState::function_sequence_error(),
                ));
            };

            let catalog = parse_filter_param(catalog_name, name_length1)?;
            let schema = parse_filter_param(schema_name, name_length2)?;
            let table = parse_filter_param(table_name, name_length3)?;

            // The token exists once this statement makes its first
            // backend call; created here on demand, then reused for every
            // later call on the same statement (see `resolve_cancel_token`).
            let cancel_token =
                crate::handles::resolve_cancel_token::<B>(statement_handle, connection);
            let cancel = crate::handles::cancel_as::<B>(&cancel_token)?;

            let result = B::primary_keys(
                connection,
                cancel,
                catalog.as_deref(),
                schema.as_deref(),
                table.as_deref(),
            )
            .into_odbc()?;

            stmt.set_result_set(StatementData::Backend(result));
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
/// - 40001: Serialization failure; propagated from backend as `HY000`.
/// - 40003: Statement completion unknown; propagated from backend as `HY000`.
/// - HY000: General error; returned for any unexpected backend error.
/// - HY001: Memory allocation error; returned if allocation fails.
/// - HY008: Operation canceled (DM) (driver-manager-handled; not returned here).
/// - HY009: Invalid use of null pointer (DM) (driver-manager-handled; not returned by this
///   driver). Note: spec requires `PKTableName` and `FKTableName` to not both be NULL — this is
///   a DM-only check.
/// - HY010: Function sequence error — returned if connection is not open. DM cases (async, etc.)
///   are driver-manager-handled; not returned here.
/// - HY013: Memory management error; returned if underlying allocation fails.
/// - HY090: Invalid string or buffer length (DM) (driver-manager-handled; not returned by this
///   driver).
/// - HY117: Connection suspended (DM) (driver-manager-handled; not returned here).
/// - HYC00: Optional feature not implemented; propagated from backend if catalogs/schemas are
///   unsupported.
/// - HYT00: Timeout expired; propagated from backend as `HY000`.
/// - HYT01: Connection timeout expired; propagated from backend as `HY000`.
/// - IM001: Driver does not support this function (DM) (driver-manager-handled; not returned here).
/// - IM017: Polling disabled (DM) (driver-manager-handled; not returned here).
/// - IM018: SQLCompleteAsync not called (DM) (driver-manager-handled; not returned here).
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
    tracing::debug!("SQLForeignKeysW(stmt={:?})", statement_handle);
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

            let Some(ref connection) = conn.connection else {
                return Err(OdbcError::general(
                    "Connection is not open",
                    SqlState::function_sequence_error(),
                ));
            };

            let pk_catalog = parse_filter_param(pk_catalog_name, name_length1)?;
            let pk_schema = parse_filter_param(pk_schema_name, name_length2)?;
            let pk_table = parse_filter_param(pk_table_name, name_length3)?;
            let fk_catalog = parse_filter_param(fk_catalog_name, name_length4)?;
            let fk_schema = parse_filter_param(fk_schema_name, name_length5)?;
            let fk_table = parse_filter_param(fk_table_name, name_length6)?;

            // The token exists once this statement makes its first
            // backend call; created here on demand, then reused for every
            // later call on the same statement (see `resolve_cancel_token`).
            let cancel_token =
                crate::handles::resolve_cancel_token::<B>(statement_handle, connection);
            let cancel = crate::handles::cancel_as::<B>(&cancel_token)?;

            let result = B::foreign_keys(
                connection,
                cancel,
                pk_catalog.as_deref(),
                pk_schema.as_deref(),
                pk_table.as_deref(),
                fk_catalog.as_deref(),
                fk_schema.as_deref(),
                fk_table.as_deref(),
            )
            .into_odbc()?;

            stmt.set_result_set(StatementData::Backend(result));
            Ok(SqlReturn::SUCCESS)
        })
    };
    tracing::debug!("SQLForeignKeysW -> {:?}", ret);
    ret
}

/// Helper: validate that a statement handle is connected, then store a synthetic empty result set.
///
/// Used by catalog functions that return an empty result set (statistics, special columns) instead
/// of querying the database.
///
/// Called from within a `panic_safe` closure, which already holds the
/// group `scope` protects; this function borrows through that same scope
/// rather than acquiring anything of its own.
fn set_empty_result<B: Backend>(
    scope: &mut HandleScope<'_>,
    statement_handle: *mut c_void,
    columns: Vec<ColumnDescriptor>,
) -> Result<SqlReturn, OdbcError> {
    let (stmt, conn) = scope.stmt_with_parent::<B>(statement_handle)?;
    stmt.diagnostics.clear();

    if stmt.cursor_open {
        return Err(OdbcError::general(
            "A cursor is already open on this statement",
            SqlState::invalid_cursor_state(),
        ));
    }

    if conn.connection.is_none() {
        return Err(OdbcError::general(
            "Connection is not open",
            SqlState::function_sequence_error(),
        ));
    }

    stmt.set_result_set(StatementData::Synthetic(SyntheticStatement::new(
        columns,
        vec![],
    )));
    Ok(SqlReturn::SUCCESS)
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
/// - 40001: Serialization failure; propagated from backend as `HY000`.
/// - 40003: Statement completion unknown; propagated from backend as `HY000`.
/// - HY000: General error; returned for any unexpected backend error.
/// - HY001: Memory allocation error; returned if allocation fails.
/// - HY008: Operation canceled (DM) (driver-manager-handled; not returned here).
/// - HY009: Invalid use of null pointer (DM) (driver-manager-handled; not returned by this
///   driver). Note: the spec requires `TableName` to not be NULL, but that check is DM-only.
/// - HY010: Function sequence error — returned if connection is not open. DM cases (async, etc.)
///   are driver-manager-handled; not returned here.
/// - HY013: Memory management error; returned if underlying allocation fails.
/// - HY090: Invalid string or buffer length (DM) (driver-manager-handled; not returned here).
/// - HY100: Uniqueness option type out of range (DM) (driver-manager-handled; not returned here).
/// - HY101: Accuracy option type out of range (DM) (driver-manager-handled; not returned here).
/// - HY117: Connection suspended (DM) (driver-manager-handled; not returned here).
/// - HYC00: Optional feature not implemented; propagated from backend if index statistics are
///   unsupported for some reason other than the `NotImplemented` fallback (which instead yields
///   an empty result set).
/// - HYT00: Timeout expired; propagated from backend as `HY000`.
/// - HYT01: Connection timeout expired; propagated from backend as `HY000`.
/// - IM001: Driver does not support this function (DM) (driver-manager-handled; not returned here).
/// - IM017: Polling disabled (DM) (driver-manager-handled; not returned here).
/// - IM018: SQLCompleteAsync not called (DM) (driver-manager-handled; not returned here).
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

            let Some(ref connection) = conn.connection else {
                return Err(OdbcError::general(
                    "Connection is not open",
                    SqlState::function_sequence_error(),
                ));
            };

            let catalog = parse_filter_param(catalog_name, name_length1)?;
            let schema = parse_filter_param(schema_name, name_length2)?;
            let table = parse_filter_param(table_name, name_length3)?;

            // The token exists once this statement makes its first
            // backend call; created here on demand, then reused for every
            // later call on the same statement (see `resolve_cancel_token`).
            let cancel_token =
                crate::handles::resolve_cancel_token::<B>(statement_handle, connection);
            let cancel = crate::handles::cancel_as::<B>(&cancel_token)?;

            match B::statistics(
                connection,
                cancel,
                catalog.as_deref(),
                schema.as_deref(),
                table.as_deref(),
                unique_only,
            )
            // Converted before matching, so the `NotImplemented` arm below can
            // still recognise core's own variant inside the backend's error.
            .into_odbc()
            {
                Ok(result) => {
                    stmt.set_result_set(StatementData::Backend(result));
                    Ok(SqlReturn::SUCCESS)
                }
                Err(OdbcError::NotImplemented { .. }) => {
                    stmt.set_result_set(StatementData::Synthetic(SyntheticStatement::new(
                        statistics_columns(&B::catalog_result_column_widths()),
                        vec![],
                    )));
                    Ok(SqlReturn::SUCCESS)
                }
                Err(e) => Err(e),
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
/// - 40001: Serialization failure; propagated from backend as `HY000`.
/// - 40003: Statement completion unknown; propagated from backend as `HY000`.
/// - HY000: General error; returned for any unexpected backend error.
/// - HY001: Memory allocation error; returned if allocation fails.
/// - HY008: Operation canceled (DM) (driver-manager-handled; not returned here).
/// - HY009: Invalid use of null pointer (DM) (driver-manager-handled; not returned by this
///   driver). Note: the spec requires `TableName` to not be NULL, but that check is DM-only.
/// - HY010: Function sequence error — returned if connection is not open. DM cases (async, etc.)
///   are driver-manager-handled; not returned here.
/// - HY013: Memory management error; returned if underlying allocation fails.
/// - HY090: Invalid string or buffer length (DM) (driver-manager-handled; not returned by this
///   driver).
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
/// - HYT00: Timeout expired; propagated from backend as `HY000`.
/// - HYT01: Connection timeout expired; propagated from backend as `HY000`.
/// - IM001: Driver does not support this function (DM) (driver-manager-handled; not returned here).
/// - IM017: Polling disabled (DM) (driver-manager-handled; not returned here).
/// - IM018: SQLCompleteAsync not called (DM) (driver-manager-handled; not returned here).
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

            let Some(ref connection) = conn.connection else {
                return Err(OdbcError::general(
                    "Connection is not open",
                    SqlState::function_sequence_error(),
                ));
            };

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

            let catalog = parse_filter_param(catalog_name, name_length1)?;
            let schema = parse_filter_param(schema_name, name_length2)?;
            let table = parse_filter_param(table_name, name_length3)?;

            // The token exists once this statement makes its first
            // backend call; created here on demand, then reused for every
            // later call on the same statement (see `resolve_cancel_token`).
            let cancel_token =
                crate::handles::resolve_cancel_token::<B>(statement_handle, connection);
            let cancel = crate::handles::cancel_as::<B>(&cancel_token)?;

            match B::special_columns(
                connection,
                cancel,
                identifier_type,
                catalog.as_deref(),
                schema.as_deref(),
                table.as_deref(),
                scope,
                nullable,
            )
            // Converted before matching, so the `NotImplemented` arm below can
            // still recognise core's own variant inside the backend's error.
            .into_odbc()
            {
                Ok(result) => {
                    stmt.set_result_set(StatementData::Backend(result));
                    Ok(SqlReturn::SUCCESS)
                }
                Err(OdbcError::NotImplemented { .. }) => empty(stmt),
                Err(e) => Err(e),
            }
        })
    };
    tracing::debug!("SQLSpecialColumnsW -> {:?}", ret);
    ret
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
///   supported) or greater than the number of columns.
/// - 08S01: Communication link failure; not applicable (no backend query at describe time).
/// - HY000: General error; returned for any unexpected internal error.
/// - HY001: Memory allocation error; returned if allocation fails.
/// - HY008: Operation canceled (DM) (driver-manager-handled; not returned here).
/// - HY010: Function sequence error (DM) (driver-manager-handled; not returned here).
/// - HY013: Memory management error; returned if underlying allocation fails.
/// - HY090: Invalid string or buffer length (DM) (driver-manager-handled; not returned by this
///   driver).
/// - HY117: Connection suspended (DM) (driver-manager-handled; not returned here).
/// - HYT01: Connection timeout expired; not applicable.
/// - IM001: Driver does not support this function (DM) (driver-manager-handled; not returned here).
/// - IM017: Polling disabled (DM) (driver-manager-handled; not returned here).
/// - IM018: SQLCompleteAsync not called (DM) (driver-manager-handled; not returned here).
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

            // Spec 07009: Column number out of range.
            let desc = statement_data.describe_col(column_number).map_err(|_| {
                OdbcError::general(
                    format!(
                        "Column number {} out of range (have {} columns)",
                        column_number,
                        statement_data.column_count()
                    ),
                    SqlState::invalid_descriptor_index(),
                )
            })?;

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
                // `resolve_precision_ulen` substitutes `SQL_NO_TOTAL` for a
                // backend's "undeterminable length" sentinel (see its doc
                // comment); every other value is the widening u32 -> usize
                // cast this always was, which cannot truncate on 32- or
                // 64-bit targets.
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
                // determine was announced as `SQL_NO_NULLS` -- telling the
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
///   when `column_number` is greater than the number of result set columns (not DM-annotated).
/// - HY000: General error; returned for any unexpected internal error.
/// - HY001: Memory allocation error; returned if allocation fails.
/// - HY008: Operation canceled; not returned here (the `Backend` trait is synchronous, so
///   asynchronous cancellation never applies — not DM-annotated in the spec).
/// - HY010: Function sequence error (DM) (driver-manager-handled; not returned here).
/// - HY013: Memory management error; returned if underlying allocation fails.
/// - HY090: Invalid string or buffer length (DM) (driver-manager-handled; not returned here).
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

            let desc = statement_data.describe_col(column_number).map_err(|_| {
                OdbcError::general(
                    format!(
                        "Column number {} out of range (have {} columns)",
                        column_number, column_count
                    ),
                    SqlState::invalid_descriptor_index(),
                )
            })?;

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
/// Returns an empty result set with the standard 8-column schema. The shared framework
/// surfaces no stored-procedure metadata, so this satisfies the protocol contract (valid
/// column descriptors, zero rows) while allowing BI tools to proceed without error.
///
/// # Parameters
///
/// - `statement_handle`: Statement handle.
/// - `_catalog_name` / `_name_length1`: Catalog name (ignored — returns empty result set).
/// - `_schema_name` / `_name_length2`: Schema name (ignored — returns empty result set).
/// - `_proc_name` / `_name_length3`: Procedure name (ignored — returns empty result set).
///
/// # Spec compliance
///
/// - 01000: General warning (driver-specific informational message); not returned here.
/// - 08S01: Communication link failure; not applicable (no backend query).
/// - 24000: Invalid cursor state — returned if a cursor is already open on this statement.
/// - 40001: Serialization failure; not applicable (no backend query).
/// - 40003: Statement completion unknown; not applicable (no backend query).
/// - HY000: General error; returned for any unexpected internal error.
/// - HY001: Memory allocation error; returned if allocation fails.
/// - HY008: Operation canceled (DM) (driver-manager-handled; not returned here).
/// - HY009: Invalid use of null pointer (DM) (driver-manager-handled; not returned by this
///   driver).
/// - HY010: Function sequence error — returned if connection is not open. DM cases (async, etc.)
///   are driver-manager-handled; not returned here.
/// - HY013: Memory management error; returned if underlying allocation fails.
/// - HY090: Invalid string or buffer length (DM) (driver-manager-handled; not returned here).
/// - HY117: Connection suspended (DM) (driver-manager-handled; not returned here).
/// - HYC00: Optional feature not implemented; not applicable (empty result set always returned).
/// - HYT00: Timeout expired; not applicable (no backend query).
/// - HYT01: Connection timeout expired; not applicable (no backend query).
/// - IM001: Driver does not support this function (DM) (driver-manager-handled; not returned here).
/// - IM017: Polling disabled (DM) (driver-manager-handled; not returned here).
/// - IM018: SQLCompleteAsync not called (DM) (driver-manager-handled; not returned here).
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
pub unsafe fn sql_procedures_w<B: Backend>(
    statement_handle: *mut c_void,
    _catalog_name: *const u16,
    _name_length1: i16,
    _schema_name: *const u16,
    _name_length2: i16,
    _proc_name: *const u16,
    _name_length3: i16,
) -> SqlReturn {
    tracing::debug!("SQLProceduresW(stmt={:?})", statement_handle);
    // SAFETY: statement_handle is null or a valid StatementHandle<B>; kind and group
    // validated by scope.stmt_with_parent inside set_empty_result.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, |scope| {
            set_empty_result::<B>(
                scope,
                statement_handle,
                procedures_columns(&B::catalog_result_column_widths()),
            )
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
/// Returns an empty result set with the standard 19-column schema. The shared framework
/// surfaces no stored-procedure metadata, so this satisfies the protocol contract (valid
/// column descriptors, zero rows) while allowing BI tools to proceed without error.
///
/// # Parameters
///
/// - `statement_handle`: Statement handle.
/// - `_catalog_name` / `_name_length1`: Catalog name (ignored — returns empty result set).
/// - `_schema_name` / `_name_length2`: Schema name (ignored — returns empty result set).
/// - `_proc_name` / `_name_length3`: Procedure name (ignored — returns empty result set).
/// - `_column_name` / `_name_length4`: Column name (ignored — returns empty result set).
///
/// # Spec compliance
///
/// - 01000: General warning (driver-specific informational message); not returned here.
/// - 08S01: Communication link failure; not applicable (no backend query).
/// - 24000: Invalid cursor state — returned if a cursor is already open on this statement.
/// - 40001: Serialization failure; not applicable (no backend query).
/// - 40003: Statement completion unknown; not applicable (no backend query).
/// - HY000: General error; returned for any unexpected internal error.
/// - HY001: Memory allocation error; returned if allocation fails.
/// - HY008: Operation canceled (DM) (driver-manager-handled; not returned here).
/// - HY009: Invalid use of null pointer (DM) (driver-manager-handled; not returned by this
///   driver).
/// - HY010: Function sequence error — returned if connection is not open. DM cases (async, etc.)
///   are driver-manager-handled; not returned here.
/// - HY013: Memory management error; returned if underlying allocation fails.
/// - HY090: Invalid string or buffer length (DM) (driver-manager-handled; not returned here).
/// - HY117: Connection suspended (DM) (driver-manager-handled; not returned here).
/// - HYC00: Optional feature not implemented; not applicable (empty result set always returned).
/// - HYT00: Timeout expired; not applicable (no backend query).
/// - HYT01: Connection timeout expired; not applicable (no backend query).
/// - IM001: Driver does not support this function (DM) (driver-manager-handled; not returned here).
/// - IM017: Polling disabled (DM) (driver-manager-handled; not returned here).
/// - IM018: SQLCompleteAsync not called (DM) (driver-manager-handled; not returned here).
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
#[allow(clippy::too_many_arguments)]
pub unsafe fn sql_procedure_columns_w<B: Backend>(
    statement_handle: *mut c_void,
    _catalog_name: *const u16,
    _name_length1: i16,
    _schema_name: *const u16,
    _name_length2: i16,
    _proc_name: *const u16,
    _name_length3: i16,
    _column_name: *const u16,
    _name_length4: i16,
) -> SqlReturn {
    tracing::debug!("SQLProcedureColumnsW(stmt={:?})", statement_handle);
    // SAFETY: statement_handle is null or a valid StatementHandle<B>; kind and group
    // validated by scope.stmt_with_parent inside set_empty_result.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, |scope| {
            set_empty_result::<B>(
                scope,
                statement_handle,
                procedure_columns_columns(&B::catalog_result_column_widths()),
            )
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
/// Returns an empty result set with the standard 8-column schema. The shared framework
/// surfaces no column-level privilege information, so this satisfies the protocol contract
/// (valid column descriptors, zero rows) while allowing BI tools to proceed without error.
///
/// # Parameters
///
/// - `statement_handle`: Statement handle.
/// - `_catalog_name` / `_name_length1`: Catalog name (ignored — returns empty result set).
/// - `_schema_name` / `_name_length2`: Schema name (ignored — returns empty result set).
/// - `_table_name` / `_name_length3`: Table name (ignored — returns empty result set).
/// - `_column_name` / `_name_length4`: Column name pattern (ignored — returns empty result set).
///
/// # Spec compliance
///
/// - 01000: General warning (driver-specific informational message); not returned here.
/// - 08S01: Communication link failure; not applicable (no backend query).
/// - 24000: Invalid cursor state — returned if a cursor is already open on this statement.
/// - 40001: Serialization failure; not applicable (no backend query).
/// - 40003: Statement completion unknown; not applicable (no backend query).
/// - HY000: General error; returned for any unexpected internal error.
/// - HY001: Memory allocation error; returned if allocation fails.
/// - HY008: Operation canceled (DM) (driver-manager-handled; not returned here).
/// - HY009: Invalid use of null pointer (DM) (driver-manager-handled; not returned by this
///   driver).
/// - HY010: Function sequence error — returned if connection is not open. DM cases (async, etc.)
///   are driver-manager-handled; not returned here.
/// - HY013: Memory management error; returned if underlying allocation fails.
/// - HY090: Invalid string or buffer length (DM) (driver-manager-handled; not returned here).
/// - HY117: Connection suspended (DM) (driver-manager-handled; not returned here).
/// - HYC00: Optional feature not implemented; not applicable (empty result set always returned).
/// - HYT00: Timeout expired; not applicable (no backend query).
/// - HYT01: Connection timeout expired; not applicable (no backend query).
/// - IM001: Driver does not support this function (DM) (driver-manager-handled; not returned here).
/// - IM017: Polling disabled (DM) (driver-manager-handled; not returned here).
/// - IM018: SQLCompleteAsync not called (DM) (driver-manager-handled; not returned here).
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
#[allow(clippy::too_many_arguments)]
pub unsafe fn sql_column_privileges_w<B: Backend>(
    statement_handle: *mut c_void,
    _catalog_name: *const u16,
    _name_length1: i16,
    _schema_name: *const u16,
    _name_length2: i16,
    _table_name: *const u16,
    _name_length3: i16,
    _column_name: *const u16,
    _name_length4: i16,
) -> SqlReturn {
    tracing::debug!("SQLColumnPrivilegesW(stmt={:?})", statement_handle);
    // SAFETY: statement_handle is null or a valid StatementHandle<B>; kind and group
    // validated by scope.stmt_with_parent inside set_empty_result.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, |scope| {
            set_empty_result::<B>(
                scope,
                statement_handle,
                column_privileges_columns(&B::catalog_result_column_widths()),
            )
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
/// Returns an empty result set with the standard 7-column schema. The shared framework
/// surfaces no table-level privilege information, so this satisfies the protocol contract
/// (valid column descriptors, zero rows) while allowing BI tools to proceed without error.
///
/// # Parameters
///
/// - `statement_handle`: Statement handle.
/// - `_catalog_name` / `_name_length1`: Catalog name (ignored — returns empty result set).
/// - `_schema_name` / `_name_length2`: Schema name (ignored — returns empty result set).
/// - `_table_name` / `_name_length3`: Table name pattern (ignored — returns empty result set).
///
/// # Spec compliance
///
/// - 01000: General warning (driver-specific informational message); not returned here.
/// - 08S01: Communication link failure; not applicable (no backend query).
/// - 24000: Invalid cursor state — returned if a cursor is already open on this statement.
/// - 40001: Serialization failure; not applicable (no backend query).
/// - 40003: Statement completion unknown; not applicable (no backend query).
/// - HY000: General error; returned for any unexpected internal error.
/// - HY001: Memory allocation error; returned if allocation fails.
/// - HY008: Operation canceled (DM) (driver-manager-handled; not returned here).
/// - HY009: Invalid use of null pointer (DM) (driver-manager-handled; not returned by this
///   driver).
/// - HY010: Function sequence error — returned if connection is not open. DM cases (async, etc.)
///   are driver-manager-handled; not returned here.
/// - HY013: Memory management error; returned if underlying allocation fails.
/// - HY090: Invalid string or buffer length (DM) (driver-manager-handled; not returned here).
/// - HY117: Connection suspended (DM) (driver-manager-handled; not returned here).
/// - HYC00: Optional feature not implemented; not applicable (empty result set always returned).
/// - HYT00: Timeout expired; not applicable (no backend query).
/// - HYT01: Connection timeout expired; not applicable (no backend query).
/// - IM001: Driver does not support this function (DM) (driver-manager-handled; not returned here).
/// - IM017: Polling disabled (DM) (driver-manager-handled; not returned here).
/// - IM018: SQLCompleteAsync not called (DM) (driver-manager-handled; not returned here).
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
pub unsafe fn sql_table_privileges_w<B: Backend>(
    statement_handle: *mut c_void,
    _catalog_name: *const u16,
    _name_length1: i16,
    _schema_name: *const u16,
    _name_length2: i16,
    _table_name: *const u16,
    _name_length3: i16,
) -> SqlReturn {
    tracing::debug!("SQLTablePrivilegesW(stmt={:?})", statement_handle);
    // SAFETY: statement_handle is null or a valid StatementHandle<B>; kind and group
    // validated by scope.stmt_with_parent inside set_empty_result.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, |scope| {
            set_empty_result::<B>(
                scope,
                statement_handle,
                table_privileges_columns(&B::catalog_result_column_widths()),
            )
        })
    };
    tracing::debug!("SQLTablePrivilegesW -> {:?}", ret);
    ret
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffi::handle::sql_free_handle;
    use crate::ffi::info::type_info_columns;
    use crate::handles::ConnectionHandle;
    use crate::test_utils::{MockBackend, MockConnection, alloc_env_conn_stmt, with_handle};
    use crate::types::{
        ColumnsResultCol, Desc, ForeignKeysResultCol, Nullable, PrimaryKeysResultCol,
        SQL_BEST_ROWID, SQL_INDEX_ALL, SQL_SCOPE_CURROW, SqlDataType, TablesResultCol,
    };
    use odbc_sys::HandleType;

    /// Every catalog result set the driver can produce, named for assertion
    /// messages. This is the list that must grow when a new catalog function
    /// gains a result set -- the tests below iterate it, so a result set added
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
                "{result_set} has no identifier column -- the name list in \
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

            let ret = sql_column_privileges_w::<MockBackend>(
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
}
