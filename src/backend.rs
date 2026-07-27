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

    /// The one error type every [`Backend`] method returns.
    ///
    /// Both conversion directions are required, and they do different jobs:
    ///
    /// - `Into<OdbcError>` lets core turn a backend failure into a diagnostic
    ///   record. This is what the generic FFI entry points call.
    /// - `From<OdbcError>` lets a *defaulted* method body in this trait
    ///   construct an error and still name `Self::Error` — without it, no
    ///   default could report `NotImplemented`.
    ///
    /// `std::error::Error` is what makes the causal chain usable: core attaches
    /// a backend error as [`OdbcError::with_source`] and walks `source()` when
    /// building the diagnostic message. It also lets core log an error it would
    /// otherwise have to swallow, such as a `disconnect` that fails while
    /// unwinding a half-open connection.
    ///
    /// `Send + Sync + 'static` matches the handles the error travels inside;
    /// a `#[derive(Debug, Snafu)]` error type satisfies all of this already.
    type Error: Into<OdbcError> + From<OdbcError> + std::error::Error + Send + Sync + 'static;

    /// Establishes a new connection using the given [`ConnectParams`].
    ///
    /// Called by `SQLDriverConnectW` / `SQLConnectW`. Returns the backend-specific
    /// connection handle on success.
    fn connect(params: &ConnectParams) -> Result<Self::Connection, Self::Error>;

    /// Connection-string keywords whose values must never be logged.
    ///
    /// The backend owns its connection-string vocabulary, so it is the only
    /// party that can name its own secrets: core sees `WalletLocation`,
    /// `OAuthAssertion` or `KeyStorePin` as ordinary keywords and has no way to
    /// know better. Every `ConnectParams` the generic FFI entry points build is
    /// told this list, so declaring a keyword here is all a driver has to do.
    ///
    /// Matched case-insensitively against the whole keyword name. Aliases must
    /// be listed individually.
    ///
    /// Defaulted to empty rather than required, because core keeps a substring
    /// heuristic (`password`, `pwd`, `secret`, `token`, `apikey`, …) in force
    /// underneath: a backend that declares nothing is still covered for the
    /// common shapes, so the default understates rather than leaks. Declaring a
    /// keyword here only ever adds redaction — it can never un-redact one the
    /// heuristic already catches.
    fn sensitive_connect_keywords() -> &'static [&'static str] {
        &[]
    }

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
    fn set_autocommit(_conn: &Self::Connection, enabled: bool) -> Result<(), Self::Error> {
        if enabled {
            // Autocommit is the default mode; nothing to do.
            Ok(())
        } else {
            Err(OdbcError::NotImplemented {
                feature: "SQL_ATTR_AUTOCOMMIT=SQL_AUTOCOMMIT_OFF (manual-commit mode)".into(),
            }
            .into())
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
    fn get_info_pre_connect(_info_type: crate::types::InfoType) -> Result<InfoValue, Self::Error> {
        Err(OdbcError::NotImplemented {
            feature: "get_info_pre_connect".into(),
        }
        .into())
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
    ) -> Result<Self::Statement, Self::Error> {
        Err(OdbcError::NotImplemented {
            feature: "primary_keys".into(),
        }
        .into())
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
    ) -> Result<Self::Statement, Self::Error> {
        Err(OdbcError::NotImplemented {
            feature: "foreign_keys".into(),
        }
        .into())
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
    ) -> Result<Self::Statement, Self::Error> {
        Err(OdbcError::NotImplemented {
            feature: "statistics".into(),
        }
        .into())
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
    ) -> Result<Self::Statement, Self::Error> {
        Err(OdbcError::NotImplemented {
            feature: "special_columns".into(),
        }
        .into())
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
    fn cancel(_stmt: &mut Self::Statement) -> Result<(), Self::Error> {
        Err(OdbcError::NotImplemented {
            feature: "cancel".into(),
        }
        .into())
    }

    /// Commit or roll back the current transaction on a connection.
    ///
    /// Called by `SQLEndTran`. If `commit` is `true`, commit; otherwise roll back.
    /// The default implementation returns `NotImplemented`; backends that support
    /// explicit transactions should override this.
    fn end_tran(_conn: &Self::Connection, _commit: bool) -> Result<(), Self::Error> {
        Err(OdbcError::NotImplemented {
            feature: "end_tran".into(),
        }
        .into())
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

    /// The `SQL_GROUP_BY` (88) relationship between the columns in a
    /// `GROUP BY` clause and the non-aggregated columns in the select list —
    /// one of the [`SQL_GB_*`](crate::types::SQL_GB_NO_RELATION) values.
    ///
    /// Required because every value here is a claim, `0`
    /// (`SQL_GB_NOT_SUPPORTED`, "GROUP BY is not supported") included. Core
    /// used to answer `SQL_GB_NO_RELATION`, which is both a claim no backend
    /// made and one the spec says an entry-level driver does not return: "a
    /// SQL-92 Entry level-conformant driver will always return the
    /// SQL_GB_GROUP_BY_EQUALS_SELECT option as supported."
    fn group_by() -> u16;

    /// The `SQL_NULL_COLLATION` (85) position of NULLs in a sorted result set
    /// — one of the [`SQL_NC_*`](crate::types::SQL_NC_END) values.
    ///
    /// Required because `0` is [`SQL_NC_HIGH`](crate::types::SQL_NC_HIGH), a
    /// substantive answer ("NULLs sort high, depending on ASC/DESC") rather
    /// than an absence of one — so the shape default silently claimed it for
    /// every backend.
    fn null_collation() -> u16;

    /// The `SQL_CORRELATION_NAME` (74) support level — one of the
    /// [`SQL_CN_*`](crate::types::SQL_CN_ANY) values.
    ///
    /// Required because `0` is [`SQL_CN_NONE`](crate::types::SQL_CN_NONE),
    /// "correlation names are not supported". The spec also ties this to
    /// [`Backend::sql_conformance`]: "a SQL-92 Entry level-conformant driver
    /// will always return SQL_CN_ANY."
    fn correlation_name() -> u16;

    /// The `SQL_NON_NULLABLE_COLUMNS` (75) answer to whether the data source
    /// supports `NOT NULL` — [`SQL_NNC_NULL`](crate::types::SQL_NNC_NULL) or
    /// [`SQL_NNC_NON_NULL`](crate::types::SQL_NNC_NON_NULL).
    ///
    /// Required because `0` is `SQL_NNC_NULL`, "all columns must be nullable".
    /// The spec ties this to [`Backend::sql_conformance`] too: "a SQL-92 Entry
    /// level-conformant driver will return SQL_NNC_NON_NULL."
    fn non_nullable_columns() -> u16;

    /// Whether the data source supports expressions (not just column names) in
    /// an `ORDER BY` list — `SQL_EXPRESSIONS_IN_ORDERBY` (27), reported as
    /// `"Y"` or `"N"`.
    ///
    /// Required rather than defaulted because it is a capability an
    /// application acts on: a tool deciding whether to push `ORDER BY
    /// lower(name)` down to the data source reads this, and both a wrong `"N"`
    /// and the `""` core used to fall back to read as "no".
    fn expressions_in_order_by() -> bool;

    /// The `SQL_SQL_CONFORMANCE` (118) level — one of the
    /// [`SQL_SC_*`](crate::types::SQL_SC_SQL92_ENTRY) values.
    ///
    /// Required because core cannot know it, and hard-coding it made core
    /// contradict itself: it claimed `SQL_SC_SQL92_ENTRY` while separately
    /// supplying `SQL_GROUP_BY`, `SQL_CORRELATION_NAME` and
    /// `SQL_NON_NULLABLE_COLUMNS` values the spec says an entry-level driver
    /// never returns. Declaring a level here is a promise about those three
    /// hooks.
    fn sql_conformance() -> u32;

    /// The `SQL_SUBQUERIES` (95) bitmask: which predicates accept a subquery,
    /// as an OR of the [`SQL_SQ_*`](crate::types::SQL_SQ_EXISTS) constants.
    ///
    /// Constrained by [`Backend::sql_conformance`]: "a SQL-92 Entry
    /// level-conformant driver will always return a bitmask with all of these
    /// bits set." Core used to hard-code exactly that, which is why a backend
    /// declaring no conformance level was still told it supported correlated
    /// subqueries — the claim a BI tool acts on when it decides to push one
    /// down.
    fn subqueries() -> u32;

    /// Whether the data source supports column aliases (`SELECT x AS y`) —
    /// `SQL_COLUMN_ALIAS` (87), reported as `"Y"` or `"N"`.
    ///
    /// Constrained by [`Backend::sql_conformance`]: "a SQL-92 Entry
    /// level-conformant driver will always return 'Y'."
    fn column_alias() -> bool;

    /// How the data source concatenates a NULL character column with a
    /// non-NULL one — `SQL_CONCAT_NULL_BEHAVIOR` (22), either
    /// [`SQL_CB_NULL`](crate::types::SQL_CB_NULL) or
    /// [`SQL_CB_NON_NULL`](crate::types::SQL_CB_NON_NULL).
    ///
    /// Required because `0` is `SQL_CB_NULL`, a substantive answer. Also
    /// constrained by [`Backend::sql_conformance`]: "a SQL-92 Entry
    /// level-conformant driver will always return SQL_CB_NULL."
    fn concat_null_behavior() -> u16;

    /// The `SQL_UNION` (96) bitmask: which of `UNION` and `UNION ALL` the data
    /// source supports, as an OR of
    /// [`SQL_U_UNION`](crate::types::SQL_U_UNION) and
    /// [`SQL_U_UNION_ALL`](crate::types::SQL_U_UNION_ALL).
    fn union_support() -> u32;

    /// The `SQL_CONVERT_FUNCTIONS` (48) bitmask: which of the ODBC conversion
    /// functions the driver supports, as an OR of
    /// [`SQL_FN_CVT_CAST`](crate::types::SQL_FN_CVT_CAST) and
    /// [`SQL_FN_CVT_CONVERT`](crate::types::SQL_FN_CVT_CONVERT).
    ///
    /// Note this is about `CAST` / `CONVERT` themselves. Which *type pairs*
    /// each can convert between is the separate `SQL_CONVERT_*` family, which
    /// a backend answers through [`Backend::get_info_raw`].
    fn convert_functions() -> u32;

    /// Whether a column named in `ORDER BY` must also appear in the select
    /// list — `SQL_ORDER_BY_COLUMNS_IN_SELECT` (90), reported as `"Y"` or
    /// `"N"`.
    ///
    /// `false` is the *permissive* answer, so it is a claim rather than an
    /// absence of one: it tells an application it may order by a column it did
    /// not select.
    fn order_by_columns_in_select() -> bool;

    /// Whether the connected user is guaranteed `SELECT` on **every** table
    /// `SQLTables` returns — `SQL_ACCESSIBLE_TABLES` (19).
    ///
    /// `true` is a guarantee core cannot make on a backend's behalf, and one
    /// that depends on the connected principal rather than the driver. Return
    /// `false` unless the data source genuinely filters its catalog by
    /// privilege.
    fn accessible_tables() -> bool;

    /// Whether the data source is read-only — `SQL_DATA_SOURCE_READ_ONLY`
    /// (25), reported as `"Y"` or `"N"`.
    fn data_source_read_only() -> bool;

    /// The `SQL_SEARCH_PATTERN_ESCAPE` (14) character: what escapes `%` and
    /// `_` in the pattern arguments of the catalog functions, so they match
    /// literally.
    ///
    /// Applies only to catalog-function patterns, not to the `LIKE` predicate
    /// (that is `SQL_LIKE_ESCAPE_CLAUSE`). Return `""` if the data source has
    /// no escape character — the spec's answer for that case, and one core
    /// cannot distinguish from a backend that simply never set it.
    fn search_pattern_escape() -> &'static str;

    /// The data source's own reserved words, unfiltered.
    ///
    /// Core reports `SQL_KEYWORDS` (89) from this, after removing everything
    /// ODBC already reserves — the spec defines the info type as the data
    /// source's keywords *excluding* its own ("This list does not contain
    /// keywords specific to ODBC or keywords used by both the data source and
    /// ODBC"), so a backend states the raw fact and core applies the rule once
    /// against [`ODBC_RESERVED_KEYWORDS`](crate::types::ODBC_RESERVED_KEYWORDS).
    ///
    /// Required, not defaulted: an empty list is the claim that this data
    /// source reserves nothing beyond ODBC, which applications act on when
    /// deciding what to quote.
    ///
    /// Return the raw names in any order and in any case; core filters
    /// case-insensitively, sorts, and joins. Note that it does so on **every**
    /// call rather than caching: a `static` cannot be generic over `B`, and
    /// `SQLGetInfo(SQL_KEYWORDS)` is not a hot path. A backend whose list is
    /// expensive to produce — read out of a linked library, say — should cache
    /// behind its own `OnceLock` and return the cached slice from here.
    fn keywords() -> &'static [&'static str];

    /// The `SQL_TIMEDATE_ADD_INTERVALS` (109) bitmask: the interval units the
    /// `TIMESTAMPADD` scalar function accepts, as an OR of the
    /// [`SQL_FN_TSI_*`](crate::types::SQL_FN_TSI_SECOND) constants.
    ///
    /// Coupled to `SQL_TIMEDATE_FUNCTIONS`: a backend claiming
    /// `SQL_FN_TD_TIMESTAMPADD` there and `0` here would be saying the function
    /// exists but accepts no units. Required so that contradiction cannot be
    /// inherited silently. Return `0` only if `TIMESTAMPADD` is genuinely not
    /// supported.
    fn timedate_add_intervals() -> u32;

    /// The `SQL_TIMEDATE_DIFF_INTERVALS` (110) bitmask: the interval units the
    /// `TIMESTAMPDIFF` scalar function accepts.
    ///
    /// Separate from [`Backend::timedate_add_intervals`] because a data source
    /// may accept different units for each; see that method for the coupling
    /// to `SQL_TIMEDATE_FUNCTIONS`.
    fn timedate_diff_intervals() -> u32;

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
    ///
    /// A backend with **no** transactions (`txn_isolation_options` of `0`,
    /// matching `SQL_TC_NONE`) never reaches this method at all: validation
    /// rejects every level before the call, because no level can be inside an
    /// empty set. The `NotImplemented` branch below is therefore unreachable
    /// for such a backend, and it needs no implementation.
    fn set_txn_isolation(_conn: &Self::Connection, level: u32) -> Result<(), Self::Error> {
        if Self::txn_isolation_options() == level {
            // The only level this data source has; it is already in effect.
            Ok(())
        } else {
            Err(OdbcError::NotImplemented {
                feature: "set_txn_isolation".into(),
            }
            .into())
        }
    }
}

/// Separate trait for statement/cursor operations.
///
/// All methods have default implementations that return `NotImplemented` errors,
/// allowing backends to implement only the methods they support. Override methods
/// as you implement real functionality.
pub trait StatementBackend: Send + Sync {
    /// The one error type every [`StatementBackend`] method returns.
    ///
    /// Same bounds and same reasoning as [`Backend::Error`]. A driver is free to
    /// use one type for both traits — nothing here requires them to differ.
    ///
    /// This exists so the fetch path keeps its causal chain. `fetch` and
    /// `get_data` are the hottest error path in the crate, and while they
    /// returned `OdbcError` directly a driver had to flatten its own error into
    /// a string at every call, which is exactly what `Backend::Error` was
    /// introduced to stop everywhere else.
    type Error: Into<OdbcError> + From<OdbcError> + std::error::Error + Send + Sync + 'static;

    /// Advances the cursor to the next row.
    ///
    /// Called by `SQLFetchW`. Returns [`FetchResult::Row`] if a row is available,
    /// [`FetchResult::NoData`] when the result set is exhausted.
    fn fetch(&mut self) -> Result<FetchResult, Self::Error> {
        Err(OdbcError::NotImplemented {
            feature: "fetch".into(),
        }
        .into())
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
    ) -> Result<std::borrow::Cow<'_, ColumnValue>, Self::Error> {
        Err(OdbcError::NotImplemented {
            feature: "get_data".into(),
        }
        .into())
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
    fn describe_col(&self, _col: u16) -> Result<ColumnDescriptor, Self::Error> {
        Err(OdbcError::NotImplemented {
            feature: "describe_col".into(),
        }
        .into())
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
        SQL_INSENSITIVE, SQL_MAX_CURSOR_NAME_LEN, SQL_OIC_CORE, SQL_SO_FORWARD_ONLY,
    };
    match info_type {
        // --- String types identical in all drivers ---
        InfoType::DriverOdbcVer => Some(InfoValue::String(SQL_DRIVER_ODBC_VER_STRING.into())),
        InfoType::SearchPatternEscape => Some(InfoValue::String(B::search_pattern_escape().into())),
        // Derived from the escape dialect, which already carries this fact and
        // is what the escape translator itself consults. Hard-coding `"` here
        // let a backend quote identifiers one way and tell the application
        // another. The spec's "if the data source does not support quoted
        // identifiers, a blank is returned" is the empty-dialect case.
        InfoType::IdentifierQuoteChar => Some(InfoValue::String(
            B::escape_dialect()
                .identifier_quotes
                .first()
                .map(|(open, _)| open.to_string())
                .unwrap_or_default(),
        )),
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
        // Each of these was core asserting an entry-level SQL-92 answer while
        // the conformance level itself is the backend's to declare, so a
        // backend claiming no level was still told it had all of them.
        InfoType::ColumnAlias => Some(InfoValue::String(
            if B::column_alias() { "Y" } else { "N" }.into(),
        )),
        InfoType::Subqueries => Some(InfoValue::U32(B::subqueries())),
        InfoType::ConcatNullBehavior => Some(InfoValue::U16(B::concat_null_behavior())),
        InfoType::OrderByColumnsInSelect => Some(InfoValue::String(
            if B::order_by_columns_in_select() {
                "Y"
            } else {
                "N"
            }
            .into(),
        )),
        InfoType::UnionStatement => Some(InfoValue::U32(B::union_support())),
        InfoType::DataSourceName => Some(InfoValue::String(String::new())),
        InfoType::ServerName => Some(InfoValue::String(String::new())),
        InfoType::UserName => Some(InfoValue::String(String::new())),
        InfoType::DataSourceReadOnly => Some(InfoValue::String(
            if B::data_source_read_only() { "Y" } else { "N" }.into(),
        )),
        InfoType::AccessibleTables => Some(InfoValue::String(
            if B::accessible_tables() { "Y" } else { "N" }.into(),
        )),
        InfoType::AccessibleProcedures => Some(InfoValue::String("N".into())),
        InfoType::Integrity => Some(InfoValue::String("N".into())),
        InfoType::SpecialCharacters => Some(InfoValue::String(String::new())),
        InfoType::XopenCliYear => Some(InfoValue::String("1995".into())),
        InfoType::CollationSeq => Some(InfoValue::String(String::new())),
        InfoType::DescribeParameter => Some(InfoValue::String("Y".into())),
        // Spec-declared "Y"/"N" strings that previously had no arm, so the
        // shape-aware fallback gave them `""` -- the right shape, but not a
        // value in any of their value lists.
        InfoType::MultResultSets => Some(InfoValue::String("N".into())),
        InfoType::MaxRowSizeIncludesLong => Some(InfoValue::String("N".into())),
        InfoType::NeedLongDataLen => Some(InfoValue::String("N".into())),
        // A capability, not a shared default: `""` and a wrong "N" both read as
        // "no" to a tool deciding whether to push an expression into ORDER BY.
        InfoType::ExpressionsInOrderBy => Some(InfoValue::String(
            if B::expressions_in_order_by() {
                "Y"
            } else {
                "N"
            }
            .into(),
        )),
        // --- U16 types identical in all drivers ---
        // Enum-valued info types where zero is a substantive answer
        // (SQL_GB_NOT_SUPPORTED, SQL_NC_HIGH, SQL_CN_NONE, SQL_NNC_NULL), not
        // "unknown" -- so the backend has to state them rather than inherit a
        // claim it never made. See the `Backend` docs for each.
        InfoType::GroupBy => Some(InfoValue::U16(B::group_by())),
        InfoType::NullCollation => Some(InfoValue::U16(B::null_collation())),
        InfoType::CorrelationName => Some(InfoValue::U16(B::correlation_name())),
        InfoType::NonNullableColumns => Some(InfoValue::U16(B::non_nullable_columns())),
        InfoType::MaxDriverConnections => Some(InfoValue::U16(0)),
        InfoType::MaxConcurrentActivities => Some(InfoValue::U16(0)),
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
        InfoType::ConvertFunctions => Some(InfoValue::U32(B::convert_functions())),
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
        // Core cannot know the conformance level, and hard-coding it made core
        // contradict its own SQL_GROUP_BY / SQL_CORRELATION_NAME /
        // SQL_NON_NULLABLE_COLUMNS answers.
        InfoType::SqlConformance => Some(InfoValue::U32(B::sql_conformance())),
        // The units TIMESTAMPADD / TIMESTAMPDIFF accept. Defaulting these to 0
        // while the backend claims SQL_FN_TD_TIMESTAMPADD in
        // SQL_TIMEDATE_FUNCTIONS is self-contradictory.
        InfoType::TimedateAddIntervals => Some(InfoValue::U32(B::timedate_add_intervals())),
        InfoType::TimedateDiffIntervals => Some(InfoValue::U32(B::timedate_diff_intervals())),
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
/// The `SQL_KEYWORDS` (89) value for `B`: [`Backend::keywords`] minus
/// everything ODBC itself reserves, sorted, comma-separated with no spaces.
///
/// The subtraction is what the spec defines the info type to be — "This list
/// does not contain keywords specific to ODBC or keywords used by both the
/// data source and ODBC" — so it lives here once rather than in every backend.
/// Comparison is ASCII-case-insensitive: the reserved list is upper-case and a
/// data source that spells its keywords in lower case still shares them.
///
/// Sorting is not required by the spec, but makes the value stable for
/// anything that diffs or caches it, whatever order the backend enumerates in.
///
/// Recomputed on every call. A `static` cache cannot be generic over `B`, and
/// this is a linear scan of a short list against a fixed one on a path
/// `SQLGetInfo` reaches at most a handful of times per connection; a backend
/// with an expensive list caches on its own side (see [`Backend::keywords`]).
fn data_source_specific_keywords<B: Backend>() -> String {
    let mut names: Vec<&'static str> = B::keywords()
        .iter()
        .copied()
        .filter(|name| {
            !crate::types::ODBC_RESERVED_KEYWORDS
                .iter()
                .any(|reserved| reserved.eq_ignore_ascii_case(name))
        })
        .collect();
    names.sort_unstable();
    names.join(",")
}

pub fn common_get_info_raw<B: Backend>(info_type: u16) -> Option<InfoValue> {
    use crate::types::{
        InfoValue, SQL_CURSOR_ROLLBACK_BEHAVIOR, SQL_DATABASE_NAME, SQL_FILE_USAGE,
        SQL_IC_SENSITIVE, SQL_KEYWORDS, SQL_MULTIPLE_ACTIVE_TXN, SQL_PROCEDURE_TERM,
        SQL_PROCEDURES, SQL_QUOTED_IDENTIFIER_CASE, SQL_ROW_UPDATES, SQL_TABLE_TERM,
    };
    match info_type {
        SQL_FILE_USAGE => Some(InfoValue::U16(0)),
        // See the matching arm in `default_get_info`.
        SQL_CURSOR_ROLLBACK_BEHAVIOR => {
            Some(InfoValue::U16(B::cursor_rollback_behavior().as_u16()))
        }
        SQL_QUOTED_IDENTIFIER_CASE => Some(InfoValue::U16(SQL_IC_SENSITIVE)),
        // Both are spec-defined "Y"/"N" character strings with no
        // `odbc_sys::InfoType` variant, so this raw path is the only place
        // they can be answered. Without these arms they reach the
        // unnamed-raw default `U32(0)` and an application reading them into a
        // character buffer gets four bytes of binary zero.
        //
        // "N" for both: core drives a forward-only cursor with no positioned
        // updates, and exports no procedure-invocation support of its own. A
        // backend that has either answers it before delegating here.
        SQL_ROW_UPDATES => Some(InfoValue::String("N".into())),
        SQL_PROCEDURES => Some(InfoValue::String("N".into())),
        // Core opens no transaction of its own, so it certainly cannot hold
        // two open at once. A backend that can answers before delegating.
        SQL_MULTIPLE_ACTIVE_TXN => Some(InfoValue::String("N".into())),
        // The data source's own reserved words, minus everything ODBC already
        // reserves -- the subtraction the spec defines for this info type,
        // applied once here rather than in each backend. The list itself is a
        // capability, so it comes from `Backend::keywords`; core only owns the
        // rule.
        SQL_KEYWORDS => Some(InfoValue::String(data_source_specific_keywords::<B>())),
        // The remaining character-string info types with no
        // `odbc_sys::InfoType` variant. Empty is a valid value for both:
        // there is no shared name for the current database, and -- given
        // SQL_PROCEDURES above answers "N" -- no procedures to have a vendor
        // term for. Every data source has tables, so SQL_TABLE_TERM gets the
        // generic term rather than "".
        SQL_DATABASE_NAME => Some(InfoValue::String(String::new())),
        SQL_PROCEDURE_TERM => Some(InfoValue::String(String::new())),
        SQL_TABLE_TERM => Some(InfoValue::String("table".into())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{MockBackend, MockNoCatalogBackend, MockTxnDeleteCloseBackend};
    use crate::types::{
        DEFAULT_IDENTIFIER_LEN, InfoType, InfoValue, SQL_AM_NONE, SQL_AT_ADD_COLUMN_SINGLE,
        SQL_AT_DROP_COLUMN_RESTRICT, SQL_CA1_NEXT, SQL_CB_PRESERVE, SQL_CN_ANY,
        SQL_DRIVER_ODBC_VER_STRING, SQL_FN_CVT_CAST, SQL_FN_TSI_DAY, SQL_FN_TSI_SECOND,
        SQL_FN_TSI_YEAR, SQL_GB_GROUP_BY_EQUALS_SELECT, SQL_INSENSITIVE, SQL_MAX_CURSOR_NAME_LEN,
        SQL_NC_END, SQL_NNC_NON_NULL, SQL_OIC_CORE, SQL_OJ_LEFT, SQL_OJ_NESTED, SQL_SC_SQL92_ENTRY,
        SQL_SO_FORWARD_ONLY, SQL_SQ_COMPARISON, SQL_SQ_CORRELATED_SUBQUERIES, SQL_SQ_EXISTS,
        SQL_SQ_IN, SQL_SQ_QUANTIFIED, SQL_TXN_SERIALIZABLE, SQL_U_UNION, SQL_U_UNION_ALL,
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
        // "Y" guarantees the user has SELECT on every table SQLTables returns.
        // Core cannot make that promise for a backend, and it depends on the
        // connected principal, so the mock declares the honest "N".
        (InfoType::AccessibleTables,              Expected::Str("N")),
        (InfoType::AccessibleProcedures,          Expected::Str("N")),
        (InfoType::Integrity,                     Expected::Str("N")),
        (InfoType::SpecialCharacters,             Expected::Str("")),
        (InfoType::XopenCliYear,                  Expected::Str("1995")),
        (InfoType::CollationSeq,                  Expected::Str("")),
        (InfoType::DescribeParameter,             Expected::Str("Y")),
        // Y/N strings that used to fall through to the shape default's "",
        // which is not in any of their value lists.
        (InfoType::MultResultSets,                Expected::Str("N")),
        (InfoType::MaxRowSizeIncludesLong,        Expected::Str("N")),
        (InfoType::NeedLongDataLen,               Expected::Str("N")),
        // Backend-stated capability; MockBackend declares true.
        (InfoType::ExpressionsInOrderBy,          Expected::Str("Y")),
        // --- U16 values ---
        // Enum values where 0 is a real answer, so they come from the backend.
        (InfoType::GroupBy,                       Expected::U16(SQL_GB_GROUP_BY_EQUALS_SELECT)),
        (InfoType::NullCollation,                 Expected::U16(SQL_NC_END)),
        (InfoType::CorrelationName,               Expected::U16(SQL_CN_ANY)),
        (InfoType::NonNullableColumns,            Expected::U16(SQL_NNC_NON_NULL)),
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
        (InfoType::TimedateAddIntervals,          Expected::U32(SQL_FN_TSI_SECOND | SQL_FN_TSI_DAY)),
        (InfoType::TimedateDiffIntervals,         Expected::U32(SQL_FN_TSI_SECOND | SQL_FN_TSI_YEAR)),
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

    /// `SQL_ROW_UPDATES` (11) and `SQL_PROCEDURES` (21) are spec-defined
    /// `"Y"`/`"N"` character strings with no `odbc_sys::InfoType` variant, so
    /// they can only be answered through the raw-`u16` path. Without an arm
    /// there they fall to the unnamed-raw default `U32(0)`, and an application
    /// passing a character buffer gets four bytes of binary zero with
    /// `StringLength = 4`.
    #[test]
    fn row_updates_and_procedures_are_yn_strings_not_u32() {
        use crate::types::{SQL_PROCEDURES, SQL_ROW_UPDATES};

        for info_type in [SQL_ROW_UPDATES, SQL_PROCEDURES] {
            let value = common_get_info_raw::<MockBackend>(info_type)
                .unwrap_or_else(|| panic!("common_get_info_raw returned None for {info_type}"));
            assert!(
                matches!(&value, InfoValue::String(s) if s == "N"),
                "info type {info_type} must be a Y/N string, got {value:?}"
            );
        }
    }

    /// Four info types the spec declares as `"Y"`/`"N"` strings had no arm at
    /// all, so the shape-aware fallback gave them `""` — the right *shape*,
    /// but not a value in any of their value lists.
    /// `SQL_EXPRESSIONS_IN_ORDERBY` is not here: it is a capability a backend
    /// states (see [`Backend::expressions_in_order_by`]), not a shared default.
    #[test]
    fn yn_info_types_default_to_a_value_in_their_value_list() {
        let widths = CatalogResultColumnWidths::default();
        for info_type in [
            InfoType::MultResultSets,
            InfoType::MaxRowSizeIncludesLong,
            InfoType::NeedLongDataLen,
        ] {
            assert_eq!(
                default_get_info::<MockBackend>(info_type, &widths),
                Some(InfoValue::String("N".into())),
                "{info_type:?} must be \"Y\" or \"N\", never the empty string"
            );
        }
    }

    /// The C5 failure mode in its purest form: for these four info types zero
    /// is a *substantive answer*, not "unknown", so the shape default handed
    /// out a real spec claim (`SQL_NC_HIGH`, `SQL_CN_NONE`, `SQL_NNC_NULL`,
    /// `SQL_GB_NOT_SUPPORTED`) that no backend ever made. They are now stated
    /// by the backend.
    #[test]
    fn enum_valued_info_types_come_from_the_backend() {
        let widths = CatalogResultColumnWidths::default();
        for (info_type, actual) in [
            (InfoType::NullCollation, MockBackend::null_collation()),
            (InfoType::CorrelationName, MockBackend::correlation_name()),
            (
                InfoType::NonNullableColumns,
                MockBackend::non_nullable_columns(),
            ),
            (InfoType::GroupBy, MockBackend::group_by()),
        ] {
            assert_eq!(
                default_get_info::<MockBackend>(info_type, &widths),
                Some(InfoValue::U16(actual)),
                "{info_type:?} ignored its Backend hook"
            );
        }
    }

    /// Core hard-coded `SQL_SQL_CONFORMANCE = SQL_SC_SQL92_ENTRY` while
    /// separately supplying `SQL_GROUP_BY`, `SQL_CORRELATION_NAME` and
    /// `SQL_NON_NULLABLE_COLUMNS` values the spec says an entry-level driver
    /// never returns. Every backend inherited that contradiction; the
    /// conformance claim is now the backend's too.
    ///
    /// Asserted across two backends declaring *different* levels, because a
    /// single backend cannot distinguish "core read the hook" from "core still
    /// hard-codes the value this backend happens to declare".
    #[test]
    fn sql_conformance_comes_from_the_backend() {
        let widths = CatalogResultColumnWidths::default();
        assert_eq!(
            default_get_info::<MockBackend>(InfoType::SqlConformance, &widths),
            Some(InfoValue::U32(SQL_SC_SQL92_ENTRY)),
            "SQL_SQL_CONFORMANCE ignored Backend::sql_conformance"
        );
        assert_eq!(
            default_get_info::<MockNoCatalogBackend>(InfoType::SqlConformance, &widths),
            Some(InfoValue::U32(0)),
            "SQL_SQL_CONFORMANCE is still pinned to SQL_SC_SQL92_ENTRY"
        );
        assert_ne!(
            MockBackend::sql_conformance(),
            MockNoCatalogBackend::sql_conformance(),
            "the two mocks must declare different levels or this test proves nothing"
        );
    }

    /// The contradiction item 4 is about: core claimed `SQL_SC_SQL92_ENTRY`
    /// while separately supplying `SQL_GROUP_BY`, `SQL_CORRELATION_NAME` and
    /// `SQL_NON_NULLABLE_COLUMNS` values the spec says an entry-level driver
    /// never returns. Now that all four come from the same backend, a backend
    /// declaring entry level reports the three values the spec names for it.
    #[test]
    fn entry_level_conformance_no_longer_contradicts_the_other_info_types() {
        use crate::types::{SQL_CN_ANY, SQL_GB_GROUP_BY_EQUALS_SELECT, SQL_NNC_NON_NULL};

        let widths = CatalogResultColumnWidths::default();
        assert_eq!(
            default_get_info::<MockBackend>(InfoType::SqlConformance, &widths),
            Some(InfoValue::U32(SQL_SC_SQL92_ENTRY)),
        );
        for (info_type, expected, spec) in [
            (
                InfoType::CorrelationName,
                SQL_CN_ANY,
                "will always return SQL_CN_ANY",
            ),
            (
                InfoType::NonNullableColumns,
                SQL_NNC_NON_NULL,
                "will return SQL_NNC_NON_NULL",
            ),
            (
                InfoType::GroupBy,
                SQL_GB_GROUP_BY_EQUALS_SELECT,
                "will always return the SQL_GB_GROUP_BY_EQUALS_SELECT option",
            ),
        ] {
            assert_eq!(
                default_get_info::<MockBackend>(info_type, &widths),
                Some(InfoValue::U16(expected)),
                "an entry-level-conformant driver {spec} for {info_type:?}"
            );
        }
    }

    /// `SQL_EXPRESSIONS_IN_ORDERBY` is a capability, and `""` reads as "no" to
    /// a tool deciding whether to push an expression into `ORDER BY`.
    #[test]
    fn expressions_in_order_by_comes_from_the_backend() {
        let widths = CatalogResultColumnWidths::default();
        assert_eq!(
            default_get_info::<MockBackend>(InfoType::ExpressionsInOrderBy, &widths),
            Some(InfoValue::String("Y".into())),
            "SQL_EXPRESSIONS_IN_ORDERBY ignored Backend::expressions_in_order_by"
        );
        assert_eq!(
            default_get_info::<MockNoCatalogBackend>(InfoType::ExpressionsInOrderBy, &widths),
            Some(InfoValue::String("N".into())),
            "a backend declaring no ORDER BY expressions must report \"N\", not \"\""
        );
    }

    /// The interval bitmaps are the units `TIMESTAMPADD` / `TIMESTAMPDIFF`
    /// accept. Defaulting them to 0 while a backend freely claims
    /// `SQL_FN_TD_TIMESTAMPADD` in `SQL_TIMEDATE_FUNCTIONS` is
    /// self-contradictory, so the backend states both. They are separate hooks
    /// because a data source can legitimately support different units for each.
    #[test]
    fn timedate_interval_bitmaps_come_from_the_backend() {
        let widths = CatalogResultColumnWidths::default();
        assert_eq!(
            default_get_info::<MockBackend>(InfoType::TimedateAddIntervals, &widths),
            Some(InfoValue::U32(MockBackend::timedate_add_intervals())),
            "SQL_TIMEDATE_ADD_INTERVALS ignored Backend::timedate_add_intervals"
        );
        assert_eq!(
            default_get_info::<MockBackend>(InfoType::TimedateDiffIntervals, &widths),
            Some(InfoValue::U32(MockBackend::timedate_diff_intervals())),
            "SQL_TIMEDATE_DIFF_INTERVALS ignored Backend::timedate_diff_intervals"
        );
        assert_ne!(
            MockBackend::timedate_add_intervals(),
            MockBackend::timedate_diff_intervals(),
            "the mock must declare different units for each, or one hook could \
             serve both and the test would not notice"
        );
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

    /// Info types `default_get_info` answers identically for **every** backend,
    /// each with the reason core is entitled to decide it.
    ///
    /// The entries fall into three kinds, and nothing else belongs here:
    ///
    /// - **Facts about core's own implementation.** Core's fetch really is
    ///   forward-only and the `Backend` trait really is synchronous, so these
    ///   are not claims about the data source at all. A hook would be worse:
    ///   it would let a backend contradict what core actually does.
    /// - **Limits where the spec defines `0` as "no limit or unknown".**
    ///   Asserting nothing.
    /// - **Driver-level identity** that has no per-backend answer.
    ///
    /// Anything else — any value that is a falsifiable statement about the
    /// *data source* — belongs on a `Backend` method instead. See AGENTS.md,
    /// "Deciding whether a new info type belongs here".
    #[rustfmt::skip]
    const CORE_FACTS: &[(InfoType, &str)] = &[
        // --- Facts about core's own implementation ---
        (InfoType::ScrollOptions,                "core's fetch is forward-only"),
        (InfoType::CursorSensitivity,            "forward-only cursors cannot see other transactions' changes"),
        (InfoType::ForwardOnlyCursorAttributes1, "the fetch operations core implements"),
        (InfoType::ForwardOnlyCursorAttributes2, "core implements none of these"),
        (InfoType::DynamicCursorAttributes1,     "core has no dynamic cursor"),
        (InfoType::DynamicCursorAttributes2,     "core has no dynamic cursor"),
        (InfoType::KeysetCursorAttributes1,      "core has no keyset cursor"),
        (InfoType::KeysetCursorAttributes2,      "core has no keyset cursor"),
        (InfoType::StaticCursorAttributes1,      "core has no static cursor"),
        (InfoType::StaticCursorAttributes2,      "core has no static cursor"),
        (InfoType::AsyncMode,                    "the Backend trait is synchronous"),
        (InfoType::AsyncDbcFunctions,            "the Backend trait is synchronous"),
        (InfoType::MultResultSets,               "sql_more_results always returns SQL_NO_DATA"),
        (InfoType::NeedLongDataLen,              "core's data-at-execution path never needs the length up front"),
        (InfoType::OdbcInterfaceConformance,     "describes the FFI surface core exports"),
        (InfoType::DriverOdbcVer,                "describes the FFI surface core exports"),
        (InfoType::XopenCliYear,                 "driver-level identity, not a data-source property"),
        (InfoType::MaxCursorNameLen,             "a cursor name is an ODBC-level convention core owns"),
        (InfoType::DescribeParameter,            "core's SQLDescribeParam always answers, generically"),
        (InfoType::MaxRowSizeIncludesLong,       "follows from SQL_MAX_ROW_SIZE being 'unknown'"),
        // --- Limits: the spec defines 0 as "no limit or unknown" ---
        (InfoType::MaxDriverConnections,         "0 = no limit"),
        (InfoType::MaxConcurrentActivities,      "0 = no limit"),
        (InfoType::ActiveEnvironments,           "0 = no limit"),
        (InfoType::MaxColumnsInGroupBy,          "0 = no limit or unknown"),
        (InfoType::MaxColumnsInIndex,            "0 = no limit or unknown"),
        (InfoType::MaxColumnsInOrderBy,          "0 = no limit or unknown"),
        (InfoType::MaxColumnsInSelect,           "0 = no limit or unknown"),
        (InfoType::MaxColumnsInTable,            "0 = no limit or unknown"),
        (InfoType::MaxTablesInSelect,            "0 = no limit or unknown"),
        (InfoType::MaxUserNameLen,               "0 = no limit or unknown"),
        (InfoType::MaxIndexSize,                 "0 = no limit or unknown"),
        (InfoType::MaxRowSize,                   "0 = no limit or unknown"),
        (InfoType::MaxStatementLen,              "0 = no limit or unknown"),
        // --- No per-backend answer to give ---
        (InfoType::DataSourceName,               "the DM supplies the DSN; core has none"),
        (InfoType::ServerName,                   "carried in the connection string, not known here"),
        (InfoType::UserName,                     "carried in the connection string, not known here"),
        (InfoType::SpecialCharacters,            "empty understates; a backend with any overrides"),
        (InfoType::CollationSeq,                 "unknown, and the spec allows empty"),
        (InfoType::AccessibleProcedures,         "core exports no procedure support of its own"),
        (InfoType::Integrity,                    "core implements no integrity-enhancement grammar"),
    ];

    /// Classifies every info type `default_get_info` answers by asking one
    /// question: **does the answer move when the backend does?**
    ///
    /// Two backends that share no capability declaration are compared. An info
    /// type answering the same for both is one *core* decided, and must appear
    /// in [`CORE_FACTS`] with the reason core is entitled to decide it. One
    /// that differs is backend-derived and needs no entry.
    ///
    /// This is what keeps the backend/`SQLGetInfo` split from drifting.
    /// Hard-coding a claim about the data source into `default_get_info` now
    /// fails a test naming the info type, instead of surviving review — which
    /// is how `SQL_GROUP_BY`, `SQL_CORRELATION_NAME`, `SQL_SUBQUERIES` and the
    /// rest got there in the first place.
    #[test]
    fn default_get_info_answers_are_backend_derived_or_declared_core_facts() {
        use crate::test_utils::MockAltBackend;

        // Each backend's own widths, exactly as `sql_get_info_w` calls this.
        // Passing one shared value would make the `SQL_MAX_*_NAME_LEN` group
        // look core-decided when it is derived from
        // `Backend::catalog_result_column_widths`.
        let mine_widths = MockBackend::catalog_result_column_widths();
        let alt_widths = MockAltBackend::catalog_result_column_widths();
        assert_ne!(
            mine_widths.identifier_len, alt_widths.identifier_len,
            "the two mocks must declare different identifier widths"
        );
        let mut undeclared = Vec::new();
        let mut stale = Vec::new();

        for info_type in crate::conformance::all_info_types() {
            let mine = default_get_info::<MockBackend>(info_type, &mine_widths);
            let theirs = default_get_info::<MockAltBackend>(info_type, &alt_widths);
            let declared = CORE_FACTS.iter().find(|(t, _)| *t == info_type);

            match (mine.is_some() && mine == theirs, declared) {
                // Core decides it, and said why. Fine.
                (true, Some(_)) => {}
                // Core decides it, and did not say why.
                (true, None) => undeclared.push(info_type),
                // Backend-derived (or unanswered) but still listed as a core
                // fact -- the entry outlived the hard-coded value it described.
                (false, Some(_)) => stale.push(info_type),
                (false, None) => {}
            }
        }

        assert!(
            undeclared.is_empty(),
            "these info types answer the same for two backends with nothing in \
             common, so core is deciding them. Either derive each from a \
             `Backend` method, or add it to CORE_FACTS with the reason core is \
             entitled to decide it: {undeclared:?}"
        );
        assert!(
            stale.is_empty(),
            "these are listed in CORE_FACTS but no longer answered identically \
             for every backend; drop the stale entries: {stale:?}"
        );
    }

    /// The classification above is only as strong as the two mocks differing.
    /// A hook added to `MockBackend` and copied verbatim into `MockAltBackend`
    /// would silently turn a backend-derived info type into a "core fact"
    /// without anyone noticing.
    #[test]
    fn the_two_classification_mocks_share_no_capability_declaration() {
        use crate::test_utils::MockAltBackend;

        macro_rules! differs {
            ($($hook:ident),+ $(,)?) => {$(
                assert_ne!(
                    MockBackend::$hook(), MockAltBackend::$hook(),
                    concat!(
                        "MockBackend and MockAltBackend declare the same ",
                        stringify!($hook),
                        ", which weakens the classification test",
                    )
                );
            )+};
        }
        differs!(
            supports_catalogs,
            supports_schemas,
            alter_table_support,
            outer_join_capabilities,
            default_txn_isolation,
            txn_isolation_options,
            group_by,
            null_collation,
            correlation_name,
            non_nullable_columns,
            expressions_in_order_by,
            sql_conformance,
            timedate_add_intervals,
            timedate_diff_intervals,
            subqueries,
            column_alias,
            concat_null_behavior,
            union_support,
            convert_functions,
            order_by_columns_in_select,
            accessible_tables,
            data_source_read_only,
            search_pattern_escape,
            keywords,
            cursor_commit_behavior,
            cursor_rollback_behavior,
        );
        assert_ne!(
            MockBackend::escape_dialect().identifier_quotes,
            MockAltBackend::escape_dialect().identifier_quotes,
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
