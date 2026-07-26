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
    /// applies it to the connection's statements, and `SQLGetInfoW` reports it
    /// for `SQL_CURSOR_COMMIT_BEHAVIOR`. Overriding this method therefore
    /// changes both together, which is the point — before this hook existed,
    /// core advertised `SQL_CB_DELETE` and implemented nothing.
    ///
    /// The default is [`crate::types::CursorBehavior::Preserve`]: the least destructive
    /// value, and the one both psqlODBC and MySQL Connector/ODBC report for
    /// commit. A backend whose data source drops cursors on commit **must**
    /// override this.
    ///
    /// # Reporting path
    ///
    /// [`default_get_info`] derives `SQL_CURSOR_COMMIT_BEHAVIOR` from this
    /// hook, and so does core's own DM-safe fallback
    /// (`info_type_default_response` in `src/ffi/info.rs`), so a backend that
    /// answers the info type *nowhere* still reports the declared value. The
    /// one remaining way to bypass the hook is to answer
    /// `SQL_CURSOR_COMMIT_BEHAVIOR` deliberately — from the backend's own
    /// typed `get_info` match, or from [`Backend::get_info_raw`], which is
    /// consulted before the fallback. A backend that does either must keep the
    /// reported value and this hook in sync itself.
    ///
    /// # `SQL_CB_CLOSE` requires `close_cursor`
    ///
    /// Under [`crate::types::CursorBehavior::Close`], `sql_end_tran` closes each
    /// statement's cursor through [`StatementBackend::close_cursor`] and leaves
    /// the statement itself prepared (the transition table's footnote `[2]`).
    /// `close_cursor` defaults to a no-op, so a backend declaring `Close`
    /// **must** implement it or no cursor is actually closed.
    /// [`crate::types::CursorBehavior::Delete`] needs no such implementation:
    /// core drops the backend statement outright.
    fn cursor_commit_behavior() -> crate::types::CursorBehavior {
        crate::types::CursorBehavior::Preserve
    }

    /// What `SQLEndTran(SQL_ROLLBACK)` does to the open cursors on a connection.
    ///
    /// Separate from [`Backend::cursor_commit_behavior`] because the two
    /// legitimately differ: psqlODBC reports `SQL_CB_PRESERVE` for commit but
    /// `SQL_CB_CLOSE` for rollback when `use_declarefetch` is enabled.
    ///
    /// Reported for `SQL_CURSOR_ROLLBACK_BEHAVIOR` by [`common_get_info_raw`]
    /// and, for a backend that answers the info type nowhere, by core's own
    /// DM-safe fallback; see [`Backend::cursor_commit_behavior`] for the rest
    /// of the contract, including the requirement that a backend declaring
    /// [`crate::types::CursorBehavior::Close`] implement
    /// [`StatementBackend::close_cursor`].
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

    /// Whether this data source exposes ODBC **catalogs**.
    ///
    /// The `SQLGetInfo` spec defines a whole group of info types in terms of
    /// this one fact, and mandates an empty string or zero for every one of
    /// them when the answer is no:
    ///
    /// | Info type | Value when `false` |
    /// |---|---|
    /// | `SQL_CATALOG_NAME` | `"N"` |
    /// | `SQL_CATALOG_TERM` | `""` |
    /// | `SQL_CATALOG_NAME_SEPARATOR` | `""` |
    /// | `SQL_CATALOG_LOCATION` | `0` |
    /// | `SQL_CATALOG_USAGE` | `0` |
    ///
    /// [`default_get_info`] derives all five from this method, so a backend
    /// cannot report `SQL_CATALOG_NAME = "N"` and name its catalogs in the
    /// same breath.
    ///
    /// Deliberately **required**: a defaulted `true` would silently reproduce
    /// that contradiction in the next catalog-less backend, and the fact is
    /// one every backend author already knows.
    ///
    /// Note that when the answer is `true`, `SQL_CATALOG_LOCATION` and
    /// `SQL_CATALOG_USAGE` become genuinely data-source-specific; core returns
    /// `None` for them rather than inventing a value, so a backend with
    /// catalogs answers those two itself.
    fn supports_catalogs() -> bool;

    /// Whether this data source exposes ODBC **schemas**.
    ///
    /// The schema half of [`Backend::supports_catalogs`]: the spec mandates
    /// `SQL_SCHEMA_TERM = ""` and `SQL_SCHEMA_USAGE = 0` when the answer is
    /// no, and both are derived from this method by [`default_get_info`].
    /// When the answer is `true`, `SQL_SCHEMA_USAGE` is data-source-specific
    /// and left to the backend.
    ///
    /// There is no `SQL_SCHEMA_NAME` info type; the spec directs applications
    /// to `SQL_CATALOG_NAME` for both questions, so this hook is the only
    /// place the schema fact is stated.
    fn supports_schemas() -> bool;

    /// The `SQL_ALTER_TABLE` (86) capability bitmask — an OR of the
    /// [`SQL_AT_*`](crate::types::SQL_AT_ADD_COLUMN_SINGLE) constants.
    ///
    /// Required rather than defaulted for the reason a capability bitmap
    /// always should be: `0` means "this data source cannot `ALTER TABLE` at
    /// all", which is a claim, not an absence of one. A backend author is
    /// unlikely to notice a capability they never wrote code for, so the
    /// compiler asks instead. Return `0` only if that is genuinely true.
    fn alter_table_support() -> u32;

    /// The `SQL_OJ_CAPABILITIES` (115) bitmask — an OR of the
    /// [`SQL_OJ_*`](crate::types::SQL_OJ_LEFT) constants.
    ///
    /// Required for the same reason as [`Backend::alter_table_support`]: `0`
    /// asserts that the data source supports no outer joins whatsoever.
    fn outer_join_capabilities() -> u32;

    /// The `SQL_DEFAULT_TXN_ISOLATION` (26) level this data source runs at
    /// when the application has not set one — a single
    /// [`SQL_TXN_*`](crate::types::SQL_TXN_SERIALIZABLE) constant, or `0` if
    /// the data source does not support transactions.
    ///
    /// This is also what `SQLGetConnectAttr(SQL_ATTR_TXN_ISOLATION)` reports
    /// on a connection where the application has not set the attribute. Both
    /// answers come from here so they cannot disagree; core previously
    /// hard-coded `SQL_TXN_READ_COMMITTED` for the connection attribute while
    /// the backend reported something else for the info type.
    fn default_txn_isolation() -> u32;

    /// The `SQL_TXN_ISOLATION_OPTION` (72) bitmask: every isolation level this
    /// data source can actually run at, as an OR of the
    /// [`SQL_TXN_*`](crate::types::SQL_TXN_SERIALIZABLE) constants. `0` if the
    /// data source does not support transactions.
    ///
    /// `SQLSetConnectAttr(SQL_ATTR_TXN_ISOLATION)` validates against this and
    /// rejects anything outside it with `HY024`, so a level reported here is a
    /// promise the backend must be able to keep — see
    /// [`Backend::set_txn_isolation`], which a backend declaring more than one
    /// level is required to implement.
    ///
    /// Must include [`Backend::default_txn_isolation`] whenever that is
    /// non-zero.
    fn txn_isolation_options() -> u32;

    /// Apply an isolation level to an open connection.
    ///
    /// Called by `SQLSetConnectAttr(SQL_ATTR_TXN_ISOLATION)` after `level` has
    /// been validated against [`Backend::txn_isolation_options`], and by
    /// `SQLDriverConnect`/`SQLConnect` for a level the application set before
    /// connecting. `level` is always exactly one `SQL_TXN_*` bit.
    ///
    /// The default handles the common case of a data source with exactly one
    /// isolation level: there is nothing to switch to, so applying the only
    /// supported level succeeds without the backend writing any code. A
    /// backend that declares **more than one** level in
    /// `txn_isolation_options` must override this — otherwise the default
    /// reports `NotImplemented` rather than accepting a level it would then
    /// silently fail to apply.
    fn set_txn_isolation(_conn: &Self::Connection, level: u32) -> Result<(), OdbcError> {
        if Self::txn_isolation_options() == level {
            // The only level this data source has; it is already in effect.
            Ok(())
        } else {
            Err(OdbcError::NotImplemented {
                feature: "set_txn_isolation".into(),
            })
        }
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
    /// Called by `SQLEndTran` for a backend that declares
    /// [`crate::types::CursorBehavior::Close`] from
    /// [`Backend::cursor_commit_behavior`] / [`Backend::cursor_rollback_behavior`].
    /// That is its only caller: `SQLCloseCursorW` and `SQLFreeStmt(SQL_CLOSE)`
    /// discard the whole backend statement instead, so there is nothing left
    /// for this method to close.
    /// The statement handle remains valid and may be re-executed.
    ///
    /// The default is a no-op, which is why a backend declaring `Close`
    /// **must** override it: `SQLEndTran` deliberately keeps the backend
    /// statement alive under `SQL_CB_CLOSE` (the transition table leaves a
    /// prepared-but-unexecuted statement unchanged), so this method is the only
    /// thing that actually closes the cursor.
    fn close_cursor(&mut self) {}
}

/// Default values for `InfoType` variants that are **identical** across all drivers.
///
/// Backends should call this at the end of their `get_info` match, before the `_ =>` arm,
/// to avoid duplicating these ~60 arms. Returns `None` for anything driver-specific.
///
/// Generic over the calling backend so that `SQL_CURSOR_COMMIT_BEHAVIOR` can be
/// derived from [`Backend::cursor_commit_behavior`] -- call it as
/// `default_get_info::<Self>(info_type, widths)`.
pub fn default_get_info<B: Backend>(
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
        // --- Catalog / schema group: all derived from the two backend hooks ---
        // The spec defines each of these in terms of whether the data source
        // has catalogs (resp. schemas) at all, and mandates the empty string
        // or zero when it does not. See `Backend::supports_catalogs`.
        InfoType::CatalogTerm => Some(InfoValue::String(
            if B::supports_catalogs() {
                "catalog"
            } else {
                ""
            }
            .into(),
        )),
        InfoType::SchemaTerm => Some(InfoValue::String(
            if B::supports_schemas() { "schema" } else { "" }.into(),
        )),
        InfoType::CatalogNameSeparator => Some(InfoValue::String(
            if B::supports_catalogs() { "." } else { "" }.into(),
        )),
        InfoType::CatalogName => Some(InfoValue::String(
            if B::supports_catalogs() { "Y" } else { "N" }.into(),
        )),
        // Only the spec-mandated zero is asserted here. Once catalogs or
        // schemas exist, the position of the catalog in a qualified name and
        // the statements catalogs/schemas may appear in are genuinely
        // per-data-source, so core falls through and lets the backend answer
        // rather than overstating a capability it cannot know.
        InfoType::CatalogLocation if !B::supports_catalogs() => Some(InfoValue::U16(0)),
        InfoType::CatalogUsage if !B::supports_catalogs() => Some(InfoValue::U32(0)),
        InfoType::SchemaUsage if !B::supports_schemas() => Some(InfoValue::U32(0)),
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
        // Derived from the backend hook so the value reported here and the
        // behaviour `sql_end_tran` applies cannot disagree.
        InfoType::CursorCommitBehaviour => {
            Some(InfoValue::U16(B::cursor_commit_behavior().as_u16()))
        }
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
        // Capability bitmaps, not limits: a `0` here is the claim "this data
        // source cannot do this at all", so the backend has to state it.
        InfoType::AlterTable => Some(InfoValue::U32(B::alter_table_support())),
        InfoType::OuterJoinCapabilities => Some(InfoValue::U32(B::outer_join_capabilities())),
        // Limits, where the spec defines 0 as "no specified limit or the limit
        // is unknown" -- correct as a shared default, unlike the two above.
        InfoType::MaxIndexSize => Some(InfoValue::U32(0)),
        InfoType::MaxRowSize => Some(InfoValue::U32(0)),
        InfoType::MaxStatementLen => Some(InfoValue::U32(0)),
        // Derived from the backend hooks so that SQL_DEFAULT_TXN_ISOLATION and
        // SQLGetConnectAttr(SQL_ATTR_TXN_ISOLATION) cannot report two
        // different levels for the same connection.
        InfoType::DefaultTxnIsolation => Some(InfoValue::U32(B::default_txn_isolation())),
        InfoType::TransactionIsolationProtocol => Some(InfoValue::U32(B::txn_isolation_options())),
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
///
/// Generic over the calling backend so that `SQL_CURSOR_ROLLBACK_BEHAVIOR` can be
/// derived from [`Backend::cursor_rollback_behavior`] -- call it as
/// `common_get_info_raw::<Self>(info_type)`.
pub fn common_get_info_raw<B: Backend>(info_type: u16) -> Option<InfoValue> {
    use crate::types::{
        InfoValue, SQL_CURSOR_ROLLBACK_BEHAVIOR, SQL_FILE_USAGE, SQL_IC_SENSITIVE,
        SQL_QUOTED_IDENTIFIER_CASE,
    };
    match info_type {
        SQL_FILE_USAGE => Some(InfoValue::U16(0)),
        // See the matching arm in `default_get_info`.
        SQL_CURSOR_ROLLBACK_BEHAVIOR => {
            Some(InfoValue::U16(B::cursor_rollback_behavior().as_u16()))
        }
        SQL_QUOTED_IDENTIFIER_CASE => Some(InfoValue::U16(SQL_IC_SENSITIVE)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{MockBackend, MockTxnDeleteCloseBackend};
    use crate::types::{
        DEFAULT_IDENTIFIER_LEN, InfoType, InfoValue, SQL_AM_NONE, SQL_AT_ADD_COLUMN_SINGLE,
        SQL_AT_DROP_COLUMN_RESTRICT, SQL_CA1_NEXT, SQL_CB_PRESERVE, SQL_DRIVER_ODBC_VER_STRING,
        SQL_FN_CVT_CAST, SQL_GB_NO_RELATION, SQL_INSENSITIVE, SQL_MAX_CURSOR_NAME_LEN,
        SQL_OIC_CORE, SQL_OJ_LEFT, SQL_OJ_NESTED, SQL_SC_SQL92_ENTRY, SQL_SO_FORWARD_ONLY,
        SQL_SQ_COMPARISON, SQL_SQ_CORRELATED_SUBQUERIES, SQL_SQ_EXISTS, SQL_SQ_IN,
        SQL_SQ_QUANTIFIED, SQL_TXN_SERIALIZABLE, SQL_U_UNION, SQL_U_UNION_ALL,
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
        (InfoType::CursorCommitBehaviour,         Expected::U16(SQL_CB_PRESERVE)),
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
        // Capability bitmaps: MockBackend's declared values, not a shared 0.
        (InfoType::AlterTable,                    Expected::U32(SQL_AT_ADD_COLUMN_SINGLE | SQL_AT_DROP_COLUMN_RESTRICT)),
        (InfoType::OuterJoinCapabilities,         Expected::U32(SQL_OJ_LEFT | SQL_OJ_NESTED)),
        // Limits, where the spec defines 0 as "no limit or unknown".
        (InfoType::MaxIndexSize,                  Expected::U32(0)),
        (InfoType::MaxRowSize,                    Expected::U32(0)),
        (InfoType::MaxStatementLen,               Expected::U32(0)),
        (InfoType::DefaultTxnIsolation,           Expected::U32(SQL_TXN_SERIALIZABLE)),
        (InfoType::TransactionIsolationProtocol,  Expected::U32(SQL_TXN_SERIALIZABLE)),
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
            let actual =
                default_get_info::<MockBackend>(*info_type, &CatalogResultColumnWidths::default())
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

    #[test]
    fn advertised_cursor_behavior_tracks_the_backend_hooks() {
        use crate::types::{SQL_CB_CLOSE, SQL_CB_DELETE, SQL_CURSOR_ROLLBACK_BEHAVIOR};

        assert_eq!(
            default_get_info::<MockTxnDeleteCloseBackend>(
                InfoType::CursorCommitBehaviour,
                &CatalogResultColumnWidths::default(),
            ),
            Some(InfoValue::U16(SQL_CB_DELETE)),
            "SQL_CURSOR_COMMIT_BEHAVIOR ignored Backend::cursor_commit_behavior"
        );
        assert_eq!(
            common_get_info_raw::<MockTxnDeleteCloseBackend>(SQL_CURSOR_ROLLBACK_BEHAVIOR),
            Some(InfoValue::U16(SQL_CB_CLOSE)),
            "SQL_CURSOR_ROLLBACK_BEHAVIOR ignored Backend::cursor_rollback_behavior"
        );
    }

    #[test]
    fn advertised_cursor_behavior_defaults_to_preserve() {
        use crate::test_utils::MockBackend;
        use crate::types::{SQL_CB_PRESERVE, SQL_CURSOR_ROLLBACK_BEHAVIOR};

        assert_eq!(
            default_get_info::<MockBackend>(
                InfoType::CursorCommitBehaviour,
                &CatalogResultColumnWidths::default(),
            ),
            Some(InfoValue::U16(SQL_CB_PRESERVE))
        );
        assert_eq!(
            common_get_info_raw::<MockBackend>(SQL_CURSOR_ROLLBACK_BEHAVIOR),
            Some(InfoValue::U16(SQL_CB_PRESERVE))
        );
    }

    /// `SQL_CATALOG_TERM`, `SQL_CATALOG_NAME_SEPARATOR`, `SQL_CATALOG_NAME`,
    /// `SQL_CATALOG_LOCATION` and `SQL_CATALOG_USAGE` are all defined by the
    /// `SQLGetInfo` spec in terms of one fact — whether the data source has
    /// catalogs at all — so a backend that says it has none must not be handed
    /// a name for them. Core used to answer "catalog" and "." unconditionally,
    /// which let a driver report `SQL_CATALOG_NAME = "N"` and name its
    /// catalogs in the same breath.
    #[test]
    fn catalog_less_backend_reports_the_spec_mandated_empty_catalog_group() {
        use crate::test_utils::MockNoCatalogBackend;

        let widths = CatalogResultColumnWidths::default();
        assert_eq!(
            default_get_info::<MockNoCatalogBackend>(InfoType::CatalogTerm, &widths),
            Some(InfoValue::String(String::new())),
            "SQL_CATALOG_TERM must be empty when the data source has no catalogs"
        );
        assert_eq!(
            default_get_info::<MockNoCatalogBackend>(InfoType::CatalogNameSeparator, &widths),
            Some(InfoValue::String(String::new())),
            "SQL_CATALOG_NAME_SEPARATOR must be empty when the data source has no catalogs"
        );
        assert_eq!(
            default_get_info::<MockNoCatalogBackend>(InfoType::CatalogName, &widths),
            Some(InfoValue::String("N".into())),
            "SQL_CATALOG_NAME must be \"N\" when the data source has no catalogs"
        );
        assert_eq!(
            default_get_info::<MockNoCatalogBackend>(InfoType::CatalogLocation, &widths),
            Some(InfoValue::U16(0)),
            "SQL_CATALOG_LOCATION must be 0 when the data source has no catalogs"
        );
        assert_eq!(
            default_get_info::<MockNoCatalogBackend>(InfoType::CatalogUsage, &widths),
            Some(InfoValue::U32(0)),
            "SQL_CATALOG_USAGE must be 0 when the data source has no catalogs"
        );
    }

    /// The schema half of the same rule: `SQL_SCHEMA_TERM` and
    /// `SQL_SCHEMA_USAGE` are defined in terms of whether schemas exist.
    #[test]
    fn schema_less_backend_reports_the_spec_mandated_empty_schema_group() {
        use crate::test_utils::MockNoCatalogBackend;

        let widths = CatalogResultColumnWidths::default();
        assert_eq!(
            default_get_info::<MockNoCatalogBackend>(InfoType::SchemaTerm, &widths),
            Some(InfoValue::String(String::new())),
            "SQL_SCHEMA_TERM must be empty when the data source has no schemas"
        );
        assert_eq!(
            default_get_info::<MockNoCatalogBackend>(InfoType::SchemaUsage, &widths),
            Some(InfoValue::U32(0)),
            "SQL_SCHEMA_USAGE must be 0 when the data source has no schemas"
        );
    }

    /// A backend that *does* have catalogs and schemas still gets the SQL-92
    /// Full level terms the spec names, so the fix does not quietly blank the
    /// values for the drivers that were already right.
    #[test]
    fn catalog_supporting_backend_keeps_the_sql92_full_terms() {
        let widths = CatalogResultColumnWidths::default();
        for (info_type, expected) in [
            (InfoType::CatalogTerm, "catalog"),
            (InfoType::SchemaTerm, "schema"),
            (InfoType::CatalogNameSeparator, "."),
            (InfoType::CatalogName, "Y"),
        ] {
            assert_eq!(
                default_get_info::<MockBackend>(info_type, &widths),
                Some(InfoValue::String(expected.into())),
                "{info_type:?} changed for a catalog-supporting backend"
            );
        }
    }

    /// Where the spec only mandates the *zero*, core must not invent the
    /// non-zero. `SQL_CATALOG_LOCATION` (start vs end), `SQL_CATALOG_USAGE`
    /// and `SQL_SCHEMA_USAGE` are genuinely per-data-source once catalogs or
    /// schemas exist, so core returns `None` and leaves them to the backend
    /// rather than overstating a capability it cannot know.
    #[test]
    fn catalog_supporting_backend_leaves_location_and_usage_to_the_backend() {
        let widths = CatalogResultColumnWidths::default();
        for info_type in [
            InfoType::CatalogLocation,
            InfoType::CatalogUsage,
            InfoType::SchemaUsage,
        ] {
            assert_eq!(
                default_get_info::<MockBackend>(info_type, &widths),
                None,
                "{info_type:?} must be left to the backend when catalogs/schemas exist"
            );
        }
    }

    /// C5: `SQL_ALTER_TABLE` and `SQL_OJ_CAPABILITIES` are capability bitmaps,
    /// where a defaulted `0` claims "this data source cannot do this at all".
    /// They now come from required `Backend` methods, so a backend author has
    /// to state the fact instead of inheriting a silent understatement.
    #[test]
    fn alter_table_and_outer_join_capabilities_come_from_the_backend() {
        let widths = CatalogResultColumnWidths::default();
        assert_eq!(
            default_get_info::<MockBackend>(InfoType::AlterTable, &widths),
            Some(InfoValue::U32(MockBackend::alter_table_support())),
            "SQL_ALTER_TABLE ignored Backend::alter_table_support"
        );
        assert_eq!(
            default_get_info::<MockBackend>(InfoType::OuterJoinCapabilities, &widths),
            Some(InfoValue::U32(MockBackend::outer_join_capabilities())),
            "SQL_OJ_CAPABILITIES ignored Backend::outer_join_capabilities"
        );
        // A non-zero declaration is what proves the value is read rather than
        // hard-coded: the previous implementation returned 0 for both.
        assert_ne!(MockBackend::alter_table_support(), 0);
        assert_ne!(MockBackend::outer_join_capabilities(), 0);
    }

    /// C2: `SQL_DEFAULT_TXN_ISOLATION` and `SQL_TXN_ISOLATION_OPTION` are
    /// derived from the same two hooks that
    /// `SQLGetConnectAttr(SQL_ATTR_TXN_ISOLATION)` reads, so the two cannot
    /// disagree on one connection.
    #[test]
    fn txn_isolation_info_types_come_from_the_backend() {
        use crate::types::SQL_TXN_SERIALIZABLE;

        let widths = CatalogResultColumnWidths::default();
        assert_eq!(
            default_get_info::<MockBackend>(InfoType::DefaultTxnIsolation, &widths),
            Some(InfoValue::U32(SQL_TXN_SERIALIZABLE)),
            "SQL_DEFAULT_TXN_ISOLATION ignored Backend::default_txn_isolation"
        );
        assert_eq!(
            default_get_info::<MockBackend>(InfoType::TransactionIsolationProtocol, &widths),
            Some(InfoValue::U32(SQL_TXN_SERIALIZABLE)),
            "SQL_TXN_ISOLATION_OPTION ignored Backend::txn_isolation_options"
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
                default_get_info::<MockBackend>(info_type, &widths),
                Some(InfoValue::U16(63)),
                "{info_type:?} ignored the supplied identifier_len"
            );
        }
    }
}
