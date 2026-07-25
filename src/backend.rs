//! The [`Backend`] and [`StatementBackend`] traits every driver implements,
//! plus the shared `SQLGetInfo` helpers (`common_get_info_raw`,
//! `default_get_info`).

use odbc_sys::CDataType;

use crate::errors::OdbcError;
use crate::types::{
    CatalogResultColumnWidths, ColumnDescriptor, ColumnValue, ConnectParams, ExecuteOutcome,
    FetchResult, IdentifierType, InfoValue, Nullable, Scope, TypeInfoRow,
};

/// Core abstraction for database-specific logic.
/// Everything in stackable-odbc-core is generic over B: Backend.
/// `Sized` is implicit (all traits require it by default), listed for symmetry with `StatementBackend` and to make the full contract visible in one place.
pub trait Backend: Sized + Send + Sync + 'static {
    type Connection: Send + Sync;
    type Statement: StatementBackend;
    type Error: Into<OdbcError>;

    /// Establishes a new connection using the given [`ConnectParams`].
    ///
    /// Called by `SQLDriverConnectW` / `SQLConnectW`. Returns the backend-specific
    /// connection handle on success.
    fn connect(params: &ConnectParams) -> Result<Self::Connection, Self::Error>;

    /// Closes an existing connection and releases associated resources.
    ///
    /// Called by `SQLDisconnectW`.
    fn disconnect(conn: &mut Self::Connection) -> Result<(), Self::Error>;

    /// Executes a SQL statement directly without preparation.
    ///
    /// Called by `SQLExecDirectW`. Returns a statement that can be used to iterate results
    /// via [`StatementBackend`].
    fn exec_direct(conn: &Self::Connection, sql: &str) -> Result<Self::Statement, Self::Error>;

    /// Prepares a SQL statement for later execution.
    ///
    /// Called by `SQLPrepareW`. Returns a prepared statement object (`Self::Statement`)
    /// that can be executed via [`Backend::execute`].
    fn prepare(conn: &Self::Connection, sql: &str) -> Result<Self::Statement, Self::Error>;

    /// Executes a previously prepared statement with the given parameter values.
    ///
    /// Called by `SQLExecuteW`. `params` contains one [`ColumnValue`] per bound
    /// parameter, in bind order (the assembled *input* values).
    ///
    /// Returns an [`ExecuteOutcome`]. Backends without output-parameter support
    /// return `Ok(ExecuteOutcome::default())` (the common case). A backend that
    /// produces `SQL_PARAM_OUTPUT` / `SQL_PARAM_INPUT_OUTPUT` values populates
    /// [`ExecuteOutcome::output_params`]; `stackable-odbc-core` then writes each value back
    /// into the application's bound parameter buffer, the symmetric counterpart
    /// of the `params` input above.
    fn execute(
        conn: &Self::Connection,
        stmt: &mut Self::Statement,
        params: &[ColumnValue],
    ) -> Result<ExecuteOutcome, Self::Error>;

    /// Switch the connection between autocommit and manual-commit mode.
    ///
    /// Called by `SQLSetConnectAttr(SQL_ATTR_AUTOCOMMIT)`. In manual-commit
    /// mode the backend must hold changes until [`Backend::end_tran`] commits
    /// or rolls them back.
    ///
    /// The default implementation reports `HYC00` for manual-commit mode,
    /// which is correct for a backend that reports `SQL_TC_NONE` for
    /// `SQL_TXN_CAPABLE`. A backend that advertises transaction support **must**
    /// override this: accepting the attribute without honouring it would let
    /// an application believe a rollback is available when it is not.
    fn set_autocommit(_conn: &Self::Connection, enabled: bool) -> Result<(), OdbcError> {
        if enabled {
            // Autocommit is the default mode; nothing to do.
            Ok(())
        } else {
            Err(OdbcError::NotImplemented {
                feature: "SQL_ATTR_AUTOCOMMIT=SQL_AUTOCOMMIT_OFF (manual-commit mode)".into(),
            })
        }
    }

    /// Returns driver or data source information for the given `InfoType`.
    ///
    /// Called by `SQLGetInfoW`. See [`default_get_info`] for values that are shared across
    /// all drivers; backends should delegate to it before handling driver-specific types.
    fn get_info(
        conn: &Self::Connection,
        info_type: crate::types::InfoType,
    ) -> Result<InfoValue, Self::Error>;

    /// Return driver-level info that does not require an active connection.
    ///
    /// The Windows Driver Manager calls `SQLGetInfoW` for types like
    /// `SQL_DRIVER_ODBC_VER` *before* the connection is established.
    /// Backends should override this to handle those pre-connect info types.
    /// The default returns `NotImplemented`.
    fn get_info_pre_connect(_info_type: crate::types::InfoType) -> Result<InfoValue, OdbcError> {
        Err(OdbcError::NotImplemented {
            feature: "get_info_pre_connect".into(),
        })
    }

    /// Handle an info type by its raw `u16` value, before the typed `InfoType`
    /// dispatch in [`Backend::get_info`] / [`default_get_info`] runs.
    ///
    /// Return `Some(Ok(value))` to respond, `Some(Err(e))` to report an error,
    /// or `None` (the default) to fall through to the Driver-Manager-safe
    /// default in `info_type_default_response` (`stackable-odbc-core/src/ffi/info.rs`).
    ///
    /// This is the *only* place two different kinds of info type get a value:
    /// - Info types genuinely absent from `odbc_sys::InfoType` (e.g.
    ///   `SQL_CURSOR_ROLLBACK_BEHAVIOR`) have no `InfoType` variant to match on
    ///   anywhere else.
    /// - Info types that **are** real `InfoType` variants (e.g.
    ///   `SQL_AGGREGATE_FUNCTIONS`, `SQL_FILE_USAGE`) but have no arm in
    ///   [`default_get_info`] or in the backend's own typed `get_info` still
    ///   need a value; those reach here as raw `u16`s after the typed call
    ///   returns `NotImplemented`. See `info_type_default_response`'s
    ///   "load-bearing ordering" doc for why this must be checked before the
    ///   generic numeric-range defaults.
    ///
    /// Backends should match their own driver-specific info types first,
    /// then delegate to [`common_get_info_raw`] as the fallback (`_ =>`
    /// arm) for the small set of values that are identical across every
    /// driver. That way a driver's own answer always wins over the shared
    /// default for any info type both would otherwise handle.
    fn get_info_raw(
        _conn: &Self::Connection,
        _info_type: u16,
    ) -> Option<Result<InfoValue, Self::Error>> {
        None
    }

    /// Returns the list of ODBC functions supported by this driver.
    ///
    /// Called by `SQLGetFunctionsW`. The returned slice must contain one
    /// [`FunctionId`](crate::function_id::FunctionId) entry
    /// per exported FFI function. `stackable-odbc-core` maps 3.x IDs to their 2.x equivalents
    /// automatically for the legacy function array.
    fn get_functions() -> &'static [crate::function_id::FunctionId];

    /// Returns static type information rows describing the SQL types supported by this driver.
    ///
    /// Called by `SQLGetTypeInfoW`. The returned slice should include both ANSI and Unicode
    /// type variants so that ODBC applications can match on `SQL_VARCHAR` as well as
    /// `SQL_WVARCHAR`.
    fn get_type_info() -> &'static [TypeInfoRow];

    /// Returns a result set describing tables matching the given filter criteria.
    ///
    /// Called by `SQLTablesW`. All filter parameters are optional; `None` means no filter
    /// on that dimension.
    fn tables(
        conn: &Self::Connection,
        catalog: Option<&str>,
        schema: Option<&str>,
        table: Option<&str>,
        table_type: Option<&str>,
    ) -> Result<Self::Statement, Self::Error>;

    /// Returns a result set describing columns matching the given filter criteria.
    ///
    /// Called by `SQLColumnsW`. All filter parameters are optional; `None` means no filter
    /// on that dimension.
    fn columns(
        conn: &Self::Connection,
        catalog: Option<&str>,
        schema: Option<&str>,
        table: Option<&str>,
        column: Option<&str>,
    ) -> Result<Self::Statement, Self::Error>;

    /// Return the primary key columns for the given table.
    ///
    /// Called by `SQLPrimaryKeysW`. Backends that do not support this can leave the
    /// default implementation which returns `NotImplemented`.
    fn primary_keys(
        _conn: &Self::Connection,
        _catalog: Option<&str>,
        _schema: Option<&str>,
        _table: Option<&str>,
    ) -> Result<Self::Statement, OdbcError> {
        Err(OdbcError::NotImplemented {
            feature: "primary_keys".into(),
        })
    }

    /// Return foreign key relationships.
    ///
    /// Called by `SQLForeignKeysW`. Either `pk_table` or `fk_table` (or both) may be supplied.
    /// Backends that do not support this can leave the default implementation which returns
    /// `NotImplemented`.
    fn foreign_keys(
        _conn: &Self::Connection,
        _pk_catalog: Option<&str>,
        _pk_schema: Option<&str>,
        _pk_table: Option<&str>,
        _fk_catalog: Option<&str>,
        _fk_schema: Option<&str>,
        _fk_table: Option<&str>,
    ) -> Result<Self::Statement, OdbcError> {
        Err(OdbcError::NotImplemented {
            feature: "foreign_keys".into(),
        })
    }

    /// Return index statistics for a single table.
    ///
    /// Called by `SQLStatisticsW`. `unique_only` reflects `SQL_INDEX_UNIQUE`
    /// (true) vs `SQL_INDEX_ALL` (false). Backends that do not expose index
    /// metadata leave the default; the FFI layer then returns a spec-legitimate
    /// empty result set (a table with no indexes is a valid empty response).
    fn statistics(
        _conn: &Self::Connection,
        _catalog: Option<&str>,
        _schema: Option<&str>,
        _table: Option<&str>,
        _unique_only: bool,
    ) -> Result<Self::Statement, OdbcError> {
        Err(OdbcError::NotImplemented {
            feature: "statistics".into(),
        })
    }

    /// Return the optimal row-identifier (`SQL_BEST_ROWID`) or row-version
    /// (`SQL_ROWVER`) columns for a single table.
    ///
    /// Called by `SQLSpecialColumnsW`. The default returns `NotImplemented`,
    /// which the FFI layer converts to an empty result set, the spec's defined
    /// response when no such columns exist.
    fn special_columns(
        _conn: &Self::Connection,
        _identifier_type: IdentifierType,
        _catalog: Option<&str>,
        _schema: Option<&str>,
        _table: Option<&str>,
        _scope: Scope,
        _nullable: Nullable,
    ) -> Result<Self::Statement, OdbcError> {
        Err(OdbcError::NotImplemented {
            feature: "special_columns".into(),
        })
    }

    /// Cancels an in-progress statement.
    ///
    /// Called by `SQLCancelW`. Takes `&mut` because implementations must clear
    /// streaming state (e.g. `next_uri`) after a server-side cancel to prevent
    /// `close_cursor`/`Drop` from trying to drain a cancelled query, which
    /// would fail and leave the connection pool's TCP socket dirty.
    ///
    /// Returns `OdbcError` directly (not `Self::Error`) to allow a default
    /// implementation. The default returns `NotImplemented`.
    fn cancel(_stmt: &mut Self::Statement) -> Result<(), OdbcError> {
        Err(OdbcError::NotImplemented {
            feature: "cancel".into(),
        })
    }

    /// Commit or roll back the current transaction on a connection.
    ///
    /// Called by `SQLEndTran`. If `commit` is `true`, commit; otherwise roll back.
    /// The default implementation returns `NotImplemented`; backends that support
    /// explicit transactions should override this.
    fn end_tran(_conn: &Self::Connection, _commit: bool) -> Result<(), OdbcError> {
        Err(OdbcError::NotImplemented {
            feature: "end_tran".into(),
        })
    }

    /// What `SQLEndTran(SQL_COMMIT)` does to the open cursors on a connection.
    ///
    /// This value is authoritative in two places at once: `sql_end_tran`
    /// applies it to the connection's statements, and [`default_get_info`]
    /// reports it for `SQL_CURSOR_COMMIT_BEHAVIOR`. Overriding this method
    /// therefore changes both together, which is the point — before this hook
    /// existed, core advertised `SQL_CB_DELETE` and implemented nothing.
    ///
    /// The default is [`CursorBehavior::Preserve`]: the least destructive
    /// value, and the one both psqlODBC and MySQL Connector/ODBC report for
    /// commit. A backend whose data source drops cursors on commit **must**
    /// override this.
    ///
    /// A backend that answers `SQL_CURSOR_COMMIT_BEHAVIOR` from its own
    /// `get_info` match instead of delegating to [`default_get_info`] bypasses
    /// this hook and must keep the two in sync itself.
    fn cursor_commit_behavior() -> crate::types::CursorBehavior {
        crate::types::CursorBehavior::Preserve
    }

    /// What `SQLEndTran(SQL_ROLLBACK)` does to the open cursors on a connection.
    ///
    /// Separate from [`Backend::cursor_commit_behavior`] because the two
    /// legitimately differ: psqlODBC reports `SQL_CB_PRESERVE` for commit but
    /// `SQL_CB_CLOSE` for rollback when `use_declarefetch` is enabled.
    ///
    /// Reported for `SQL_CURSOR_ROLLBACK_BEHAVIOR` by
    /// [`common_get_info_raw`]; see [`Backend::cursor_commit_behavior`] for the
    /// rest of the contract.
    fn cursor_rollback_behavior() -> crate::types::CursorBehavior {
        crate::types::CursorBehavior::Preserve
    }

    /// Returns the connection string attribute names required by this driver.
    ///
    /// Used by `SQLBrowseConnectW` to determine which attributes are still
    /// missing and must be supplied by the application. Keys should be
    /// lowercase to match `ConnectParams` storage convention.
    ///
    /// The default returns an empty slice (all attributes are optional).
    fn browse_connect_attrs() -> &'static [&'static str] {
        &[]
    }

    /// The escape-translation dialect for this backend (`{fn}` name map,
    /// identifier quotes, date-literal rendering). Called by the generic
    /// `SQLExecDirect`/`SQLPrepare`/`SQLNativeSql` translation. The default is a
    /// neutral ANSI dialect.
    fn escape_dialect() -> crate::escape::EscapeDialect {
        crate::escape::EscapeDialect::ansi_default()
    }

    /// The data-source-dependent widths of this driver's catalog result-set
    /// columns, and the SQL type its character columns report.
    ///
    /// Every catalog result set the driver can produce derives from this one
    /// value -- `SQLTables`, `SQLColumns`, `SQLPrimaryKeys`, `SQLForeignKeys`,
    /// `SQLStatistics`, `SQLSpecialColumns`, `SQLProcedures`,
    /// `SQLProcedureColumns`, `SQLColumnPrivileges`, `SQLTablePrivileges` and
    /// `SQLGetTypeInfo` -- so they cannot describe the same column two ways.
    ///
    /// The default suits a data source with no identifier length limit. A
    /// driver for a source that *does* impose one -- PostgreSQL's 63-character
    /// `NAMEDATALEN - 1`, say -- overrides this, and both its catalog result
    /// sets and its `SQL_MAX_*_NAME_LEN` answers follow from the one override.
    fn catalog_result_column_widths() -> CatalogResultColumnWidths {
        CatalogResultColumnWidths::default()
    }
}

/// Separate trait for statement/cursor operations.
///
/// All methods have default implementations that return `NotImplemented` errors,
/// allowing backends to implement only the methods they support. Override methods
/// as you implement real functionality.
pub trait StatementBackend: Send + Sync {
    /// Advances the cursor to the next row.
    ///
    /// Called by `SQLFetchW`. Returns [`FetchResult::Row`] if a row is available,
    /// [`FetchResult::NoData`] when the result set is exhausted.
    fn fetch(&mut self) -> Result<FetchResult, OdbcError> {
        Err(OdbcError::NotImplemented {
            feature: "fetch".into(),
        })
    }

    /// Retrieves the value of column `col` (1-based) from the current row.
    ///
    /// Called by `SQLGetDataW`. The value is converted to `target_type` as requested by
    /// the application.
    ///
    /// Returns a [`Cow`](std::borrow::Cow) so that backends which cache rows in memory can hand
    /// back a borrow (`Cow::Borrowed`) without cloning, while backends that
    /// need to construct a value on the fly can still return `Cow::Owned`.
    fn get_data(
        &mut self,
        _col: u16,
        _target_type: CDataType,
    ) -> Result<std::borrow::Cow<'_, ColumnValue>, OdbcError> {
        Err(OdbcError::NotImplemented {
            feature: "get_data".into(),
        })
    }

    /// Returns the number of columns in the result set.
    ///
    /// Called by `SQLNumResultColsW`. Returns 0 if no result set is active.
    fn column_count(&self) -> u16 {
        0
    }

    /// Returns metadata for column `col` (1-based).
    ///
    /// Called by `SQLDescribeColW`.
    fn describe_col(&self, _col: u16) -> Result<ColumnDescriptor, OdbcError> {
        Err(OdbcError::NotImplemented {
            feature: "describe_col".into(),
        })
    }

    /// Returns the number of rows affected by the last DML statement.
    ///
    /// Called by `SQLRowCountW`. Returns `None` if not applicable (e.g. for SELECT
    /// statements or when no statement has been executed).
    fn row_count(&self) -> Option<usize> {
        None
    }

    /// Closes the cursor and discards any pending results.
    ///
    /// Called by `SQLCloseCursorW`. The statement handle remains valid and may be
    /// re-executed.
    fn close_cursor(&mut self) {}
}

/// Default values for `InfoType` variants that are **identical** across all drivers.
///
/// Backends should call this at the end of their `get_info` match, before the `_ =>` arm,
/// to avoid duplicating these ~60 arms. Returns `None` for anything driver-specific.
pub fn default_get_info(
    info_type: crate::types::InfoType,
    widths: &CatalogResultColumnWidths,
) -> Option<InfoValue> {
    use crate::types::{
        InfoType, InfoValue, SQL_AM_NONE, SQL_CA1_NEXT, SQL_DRIVER_ODBC_VER_STRING,
        SQL_FN_CVT_CAST, SQL_GB_NO_RELATION, SQL_INSENSITIVE, SQL_MAX_CURSOR_NAME_LEN,
        SQL_OIC_CORE, SQL_SC_SQL92_ENTRY, SQL_SO_FORWARD_ONLY, SQL_SQ_COMPARISON,
        SQL_SQ_CORRELATED_SUBQUERIES, SQL_SQ_EXISTS, SQL_SQ_IN, SQL_SQ_QUANTIFIED, SQL_U_UNION,
        SQL_U_UNION_ALL,
    };
    match info_type {
        // --- String types identical in all drivers ---
        InfoType::DriverOdbcVer => Some(InfoValue::String(SQL_DRIVER_ODBC_VER_STRING.into())),
        InfoType::SearchPatternEscape => Some(InfoValue::String("\\".into())),
        InfoType::IdentifierQuoteChar => Some(InfoValue::String("\"".into())),
        InfoType::CatalogTerm => Some(InfoValue::String("catalog".into())),
        InfoType::SchemaTerm => Some(InfoValue::String("schema".into())),
        InfoType::CatalogNameSeparator => Some(InfoValue::String(".".into())),
        InfoType::ColumnAlias => Some(InfoValue::String("Y".into())),
        InfoType::OrderByColumnsInSelect => Some(InfoValue::String("N".into())),
        InfoType::Subqueries => Some(InfoValue::U32(
            SQL_SQ_COMPARISON
                | SQL_SQ_EXISTS
                | SQL_SQ_IN
                | SQL_SQ_QUANTIFIED
                | SQL_SQ_CORRELATED_SUBQUERIES,
        )),
        InfoType::UnionStatement => Some(InfoValue::U32(SQL_U_UNION | SQL_U_UNION_ALL)),
        InfoType::DataSourceName => Some(InfoValue::String(String::new())),
        InfoType::ServerName => Some(InfoValue::String(String::new())),
        InfoType::UserName => Some(InfoValue::String(String::new())),
        InfoType::DataSourceReadOnly => Some(InfoValue::String("N".into())),
        InfoType::AccessibleTables => Some(InfoValue::String("Y".into())),
        InfoType::AccessibleProcedures => Some(InfoValue::String("N".into())),
        InfoType::Integrity => Some(InfoValue::String("N".into())),
        InfoType::SpecialCharacters => Some(InfoValue::String(String::new())),
        InfoType::XopenCliYear => Some(InfoValue::String("1995".into())),
        InfoType::CollationSeq => Some(InfoValue::String(String::new())),
        InfoType::DescribeParameter => Some(InfoValue::String("Y".into())),
        // --- U16 types identical in all drivers ---
        InfoType::GroupBy => Some(InfoValue::U16(SQL_GB_NO_RELATION)),
        InfoType::MaxDriverConnections => Some(InfoValue::U16(0)),
        InfoType::MaxConcurrentActivities => Some(InfoValue::U16(0)),
        InfoType::ConcatNullBehavior => Some(InfoValue::U16(0)),
        InfoType::CursorCommitBehaviour => Some(InfoValue::U16(0)),
        InfoType::MaxColumnNameLen => Some(InfoValue::U16(widths.identifier_len)),
        // Deliberately not `widths.identifier_len` -- a cursor name is an
        // ODBC-level convention the application invents, not a data-source
        // identifier the backend's catalog stores. See
        // `SQL_MAX_CURSOR_NAME_LEN`'s doc comment for the full rationale.
        InfoType::MaxCursorNameLen => Some(InfoValue::U16(SQL_MAX_CURSOR_NAME_LEN)),
        InfoType::MaxSchemaNameLen => Some(InfoValue::U16(widths.identifier_len)),
        InfoType::MaxCatalogNameLen => Some(InfoValue::U16(widths.identifier_len)),
        InfoType::MaxTableNameLen => Some(InfoValue::U16(widths.identifier_len)),
        InfoType::MaxColumnsInGroupBy => Some(InfoValue::U16(0)),
        InfoType::MaxColumnsInIndex => Some(InfoValue::U16(0)),
        InfoType::MaxColumnsInOrderBy => Some(InfoValue::U16(0)),
        InfoType::MaxColumnsInSelect => Some(InfoValue::U16(0)),
        InfoType::MaxColumnsInTable => Some(InfoValue::U16(0)),
        InfoType::MaxTablesInSelect => Some(InfoValue::U16(0)),
        InfoType::MaxUserNameLen => Some(InfoValue::U16(0)),
        InfoType::ActiveEnvironments => Some(InfoValue::U16(0)),
        // SQL_CURSOR_SENSITIVITY is `An SQLUINTEGER value` per the SQLGetInfo
        // spec, not SQLUSMALLINT -- found by the info-type conformance test
        // (`stackable-odbc-core::conformance`), which enumerates every InfoType's
        // declared shape rather than relying on a hand-picked subset. `U16`
        // here would hand a numeric type expecting 4 bytes only 2, leaving
        // the upper 2 bytes as whatever the caller's buffer already held.
        InfoType::CursorSensitivity => Some(InfoValue::U32(u32::from(SQL_INSENSITIVE))),
        InfoType::MaxIdentifierLen => Some(InfoValue::U16(widths.identifier_len)),
        // --- U32 types identical in all drivers ---
        InfoType::ScrollOptions => Some(InfoValue::U32(SQL_SO_FORWARD_ONLY)),
        InfoType::ConvertFunctions => Some(InfoValue::U32(SQL_FN_CVT_CAST)),
        InfoType::AlterTable => Some(InfoValue::U32(0)),
        InfoType::MaxIndexSize => Some(InfoValue::U32(0)),
        InfoType::MaxRowSize => Some(InfoValue::U32(0)),
        InfoType::MaxStatementLen => Some(InfoValue::U32(0)),
        InfoType::OuterJoinCapabilities => Some(InfoValue::U32(0)),
        InfoType::SqlConformance => Some(InfoValue::U32(SQL_SC_SQL92_ENTRY)),
        InfoType::OdbcInterfaceConformance => Some(InfoValue::U32(SQL_OIC_CORE)),
        InfoType::AsyncMode => Some(InfoValue::U32(SQL_AM_NONE)),
        InfoType::AsyncDbcFunctions => Some(InfoValue::U32(0)),
        // --- Cursor attributes (all zero except ForwardOnly1) ---
        InfoType::DynamicCursorAttributes1 => Some(InfoValue::U32(0)),
        InfoType::DynamicCursorAttributes2 => Some(InfoValue::U32(0)),
        InfoType::ForwardOnlyCursorAttributes1 => Some(InfoValue::U32(SQL_CA1_NEXT)),
        InfoType::ForwardOnlyCursorAttributes2 => Some(InfoValue::U32(0)),
        InfoType::KeysetCursorAttributes1 => Some(InfoValue::U32(0)),
        InfoType::KeysetCursorAttributes2 => Some(InfoValue::U32(0)),
        InfoType::StaticCursorAttributes1 => Some(InfoValue::U32(0)),
        InfoType::StaticCursorAttributes2 => Some(InfoValue::U32(0)),
        _ => None,
    }
}

/// Returns a value for the few info types that must be dispatched through
/// [`Backend::get_info_raw`] (rather than the typed `InfoType` path; see
/// that method's doc for why) but are **identical** across all drivers.
///
/// Only `SQL_CURSOR_ROLLBACK_BEHAVIOR` is genuinely absent from
/// `odbc_sys::InfoType`; `SQL_FILE_USAGE` and `SQL_QUOTED_IDENTIFIER_CASE` are
/// real `InfoType` variants (`SqlFileUsage`, `SqlQuotedIdentifierCase`) that
/// simply have no arm in [`default_get_info`], so they still need a raw-`u16`
/// answer here.
///
/// Backends should call this from `get_info_raw` before checking driver-specific values.
/// Returns `None` if the info type is not handled here.
pub fn common_get_info_raw(info_type: u16) -> Option<InfoValue> {
    use crate::types::{
        InfoValue, SQL_CURSOR_ROLLBACK_BEHAVIOR, SQL_FILE_USAGE, SQL_IC_SENSITIVE,
        SQL_QUOTED_IDENTIFIER_CASE,
    };
    match info_type {
        SQL_FILE_USAGE => Some(InfoValue::U16(0)),
        SQL_CURSOR_ROLLBACK_BEHAVIOR => Some(InfoValue::U16(0)),
        SQL_QUOTED_IDENTIFIER_CASE => Some(InfoValue::U16(SQL_IC_SENSITIVE)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        DEFAULT_IDENTIFIER_LEN, InfoType, InfoValue, SQL_AM_NONE, SQL_CA1_NEXT,
        SQL_DRIVER_ODBC_VER_STRING, SQL_FN_CVT_CAST, SQL_GB_NO_RELATION, SQL_INSENSITIVE,
        SQL_MAX_CURSOR_NAME_LEN, SQL_OIC_CORE, SQL_SC_SQL92_ENTRY, SQL_SO_FORWARD_ONLY,
        SQL_SQ_COMPARISON, SQL_SQ_CORRELATED_SUBQUERIES, SQL_SQ_EXISTS, SQL_SQ_IN,
        SQL_SQ_QUANTIFIED, SQL_U_UNION, SQL_U_UNION_ALL,
    };

    enum Expected {
        Str(&'static str),
        U16(u16),
        U32(u32),
    }

    #[rustfmt::skip]
    const EXPECTED: &[(InfoType, Expected)] = &[
        // --- String values ---
        (InfoType::DriverOdbcVer,                 Expected::Str(SQL_DRIVER_ODBC_VER_STRING)),
        (InfoType::SearchPatternEscape,            Expected::Str("\\")),
        (InfoType::IdentifierQuoteChar,            Expected::Str("\"")),
        (InfoType::CatalogTerm,                   Expected::Str("catalog")),
        (InfoType::SchemaTerm,                    Expected::Str("schema")),
        (InfoType::CatalogNameSeparator,           Expected::Str(".")),
        (InfoType::ColumnAlias,                   Expected::Str("Y")),
        (InfoType::OrderByColumnsInSelect,         Expected::Str("N")),
        (InfoType::DataSourceName,                Expected::Str("")),
        (InfoType::ServerName,                    Expected::Str("")),
        (InfoType::UserName,                      Expected::Str("")),
        (InfoType::DataSourceReadOnly,             Expected::Str("N")),
        (InfoType::AccessibleTables,              Expected::Str("Y")),
        (InfoType::AccessibleProcedures,          Expected::Str("N")),
        (InfoType::Integrity,                     Expected::Str("N")),
        (InfoType::SpecialCharacters,             Expected::Str("")),
        (InfoType::XopenCliYear,                  Expected::Str("1995")),
        (InfoType::CollationSeq,                  Expected::Str("")),
        (InfoType::DescribeParameter,             Expected::Str("Y")),
        // --- U16 values ---
        (InfoType::GroupBy,                       Expected::U16(SQL_GB_NO_RELATION)),
        (InfoType::MaxDriverConnections,          Expected::U16(0)),
        (InfoType::MaxConcurrentActivities,       Expected::U16(0)),
        (InfoType::ConcatNullBehavior,            Expected::U16(0)),
        (InfoType::CursorCommitBehaviour,         Expected::U16(0)),
        (InfoType::MaxColumnNameLen,              Expected::U16(DEFAULT_IDENTIFIER_LEN)),
        (InfoType::MaxCursorNameLen,              Expected::U16(SQL_MAX_CURSOR_NAME_LEN)),
        (InfoType::MaxSchemaNameLen,              Expected::U16(DEFAULT_IDENTIFIER_LEN)),
        (InfoType::MaxCatalogNameLen,             Expected::U16(DEFAULT_IDENTIFIER_LEN)),
        (InfoType::MaxTableNameLen,               Expected::U16(DEFAULT_IDENTIFIER_LEN)),
        (InfoType::MaxColumnsInGroupBy,           Expected::U16(0)),
        (InfoType::MaxColumnsInIndex,             Expected::U16(0)),
        (InfoType::MaxColumnsInOrderBy,           Expected::U16(0)),
        (InfoType::MaxColumnsInSelect,            Expected::U16(0)),
        (InfoType::MaxColumnsInTable,             Expected::U16(0)),
        (InfoType::MaxTablesInSelect,             Expected::U16(0)),
        (InfoType::MaxUserNameLen,                Expected::U16(0)),
        (InfoType::ActiveEnvironments,            Expected::U16(0)),
        (InfoType::MaxIdentifierLen,              Expected::U16(DEFAULT_IDENTIFIER_LEN)),
        // --- U32 values ---
        // CursorSensitivity is SQLUINTEGER per spec, not SQLUSMALLINT -- see
        // the matching comment on its arm in `default_get_info`.
        (InfoType::CursorSensitivity,             Expected::U32(SQL_INSENSITIVE as u32)),
        (InfoType::Subqueries,                    Expected::U32(SQL_SQ_COMPARISON | SQL_SQ_EXISTS | SQL_SQ_IN | SQL_SQ_QUANTIFIED | SQL_SQ_CORRELATED_SUBQUERIES)),
        (InfoType::UnionStatement,                Expected::U32(SQL_U_UNION | SQL_U_UNION_ALL)),
        (InfoType::ScrollOptions,                 Expected::U32(SQL_SO_FORWARD_ONLY)),
        (InfoType::ConvertFunctions,              Expected::U32(SQL_FN_CVT_CAST)),
        (InfoType::AlterTable,                    Expected::U32(0)),
        (InfoType::MaxIndexSize,                  Expected::U32(0)),
        (InfoType::MaxRowSize,                    Expected::U32(0)),
        (InfoType::MaxStatementLen,               Expected::U32(0)),
        (InfoType::OuterJoinCapabilities,         Expected::U32(0)),
        (InfoType::SqlConformance,                Expected::U32(SQL_SC_SQL92_ENTRY)),
        (InfoType::OdbcInterfaceConformance,      Expected::U32(SQL_OIC_CORE)),
        (InfoType::AsyncMode,                     Expected::U32(SQL_AM_NONE)),
        (InfoType::AsyncDbcFunctions,             Expected::U32(0)),
        (InfoType::DynamicCursorAttributes1,      Expected::U32(0)),
        (InfoType::DynamicCursorAttributes2,      Expected::U32(0)),
        (InfoType::ForwardOnlyCursorAttributes1,  Expected::U32(SQL_CA1_NEXT)),
        (InfoType::ForwardOnlyCursorAttributes2,  Expected::U32(0)),
        (InfoType::KeysetCursorAttributes1,       Expected::U32(0)),
        (InfoType::KeysetCursorAttributes2,       Expected::U32(0)),
        (InfoType::StaticCursorAttributes1,       Expected::U32(0)),
        (InfoType::StaticCursorAttributes2,       Expected::U32(0)),
    ];

    #[test]
    fn default_get_info_snapshot() {
        for (info_type, expected) in EXPECTED {
            let actual = default_get_info(*info_type, &CatalogResultColumnWidths::default())
                .unwrap_or_else(|| panic!("default_get_info returned None for {info_type:?}"));
            match (expected, &actual) {
                (Expected::Str(s), InfoValue::String(v)) => {
                    assert_eq!(v.as_str(), *s, "wrong value for {info_type:?}")
                }
                (Expected::U16(n), InfoValue::U16(v)) => {
                    assert_eq!(v, n, "wrong value for {info_type:?}")
                }
                (Expected::U32(n), InfoValue::U32(v)) => {
                    assert_eq!(v, n, "wrong value for {info_type:?}")
                }
                _ => panic!("type mismatch for {info_type:?}"),
            }
        }
    }

    #[test]
    fn cursor_behavior_hooks_default_to_preserve() {
        use crate::test_utils::MockBackend;
        use crate::types::CursorBehavior;

        assert_eq!(
            MockBackend::cursor_commit_behavior(),
            CursorBehavior::Preserve
        );
        assert_eq!(
            MockBackend::cursor_rollback_behavior(),
            CursorBehavior::Preserve
        );
    }

    /// The five identifier-length info types must follow the supplied widths,
    /// not a baked-in 128. Before this was plumbed, a driver could report 63
    /// in its catalog result sets and 128 here, telling an application two
    /// different things about the same limit.
    #[test]
    fn max_name_len_info_types_follow_the_supplied_widths() {
        let widths = CatalogResultColumnWidths {
            identifier_len: 63,
            ..CatalogResultColumnWidths::default()
        };
        for info_type in [
            InfoType::MaxColumnNameLen,
            InfoType::MaxSchemaNameLen,
            InfoType::MaxCatalogNameLen,
            InfoType::MaxTableNameLen,
            InfoType::MaxIdentifierLen,
        ] {
            assert_eq!(
                default_get_info(info_type, &widths),
                Some(InfoValue::U16(63)),
                "{info_type:?} ignored the supplied identifier_len"
            );
        }
    }
}
