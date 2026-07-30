//! The [`Backend`] and [`StatementBackend`] traits every driver implements,
//! plus the shared `SQLGetInfo` helpers (`common_get_info_raw`,
//! `default_get_info`).

use std::borrow::Cow;

use odbc_sys::CDataType;

use crate::errors::OdbcError;
use crate::types::{
    CatalogResultColumnWidths, ColumnDescriptor, ColumnPrivilegeRow, ColumnRow, ColumnValue,
    ConnectParams, ExecuteOutcome, FetchResult, ForeignKeyRow, IdentifierType, InfoValue, Nullable,
    PrimaryKeyRow, ProcedureColumnRow, ProcedureRow, Scope, SpecialColumnRow, StatisticsRow,
    TablePrivilegeRow, TableRow, TypeInfoRow,
};

/// Core abstraction for database-specific logic.
/// Everything in stackable-odbc-core is generic over B: Backend.
/// `Sized` is implicit (all traits require it by default), listed for symmetry with `StatementBackend` and to make the full contract visible in one place.
///
/// # Re-entrancy
///
/// Every method here runs while core holds the target connection's group
/// lock (see `HandleScope` and `panic_safe` in `src/panic.rs`) — including
/// [`Backend::connect`], which runs under the lock the freshly allocated
/// connection handle already has, even though nothing yet depends on it. That
/// lock is not reentrant. A method implementation that calls back into any
/// `SQLxxx` entry point on the same connection — directly, or indirectly
/// through a callback into application code that does so — deadlocks the
/// calling thread permanently: there is no diagnostic and no `SqlReturn`,
/// because the thread never returns from the lock acquisition to produce
/// either. [`Backend::cancel`] is the one method exempt from this, and its
/// own doc comment explains why.
pub trait Backend: Sized + Send + Sync + 'static {
    /// The backend's live connection, whatever it needs to hold: a socket, a
    /// client-library handle, a file descriptor, a runtime plus a channel.
    ///
    /// Core stores one inside a `ConnectionHandle` and hands it back to every
    /// method by reference; it never inspects it. `Send + Sync` because an
    /// application may use one connection from several threads: core's own
    /// per-connection group lock serialises calls into a `Backend` method,
    /// but does not confine the connection to the thread that created it.
    type Connection: Send + Sync;

    /// The backend's executed or prepared statement, from which rows are read.
    ///
    /// Produced by `exec_direct`, `prepare` and the catalog methods, and driven
    /// through [`StatementBackend`].
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
    ///
    /// `Cow<'static, [Cow<'static, str>]>`, not `&'static [&'static str]`: this
    /// method takes no connection, so nothing about it varies at runtime, but
    /// every other list-returning method on this trait uses the same shape for
    /// a data source whose answer does vary — this one matches for uniformity
    /// of the trait's vocabulary rather than because it needs to.
    fn sensitive_connect_keywords() -> Cow<'static, [Cow<'static, str>]> {
        Cow::Borrowed(&[])
    }

    /// The driver's interactive prompter, if it has one.
    ///
    /// Core calls this while connecting and gates the result on
    /// `SQLDriverConnect`'s *DriverCompletion*: under `SQL_DRIVER_NOPROMPT` the
    /// backend receives `None` from
    /// [`ConnectParams::prompter`](crate::types::ConnectParams::prompter) and
    /// has nothing to call, so the spec's "do not prompt" cannot be forgotten
    /// at a call site. `SQLConnect` and `SQLBrowseConnect` have no such
    /// argument and always permit it.
    ///
    /// Read it from `ConnectParams` inside [`Backend::connect`], never by
    /// calling this method directly — this one is ungated and says only what
    /// the driver *could* do, not what this call is allowed to do.
    ///
    /// Static, like [`Backend::connect`] and
    /// [`Backend::sensitive_connect_keywords`]: there is no connection yet when
    /// core asks. A driver whose prompter varies per connection string can
    /// return one implementation that reads the difference at
    /// [`Prompter::present_url`](crate::prompt::Prompter::present_url) time.
    ///
    /// Defaulted, so a driver with no interactive authentication never has to
    /// think about it.
    fn prompter() -> Option<std::sync::Arc<dyn crate::prompt::Prompter>> {
        None
    }

    /// Closes an existing connection and releases associated resources.
    ///
    /// Called by `SQLDisconnect`.
    fn disconnect(conn: &mut Self::Connection) -> Result<(), Self::Error>;

    /// Executes a SQL statement directly without preparation.
    ///
    /// Called by `SQLExecDirectW`. Returns a statement that can be used to iterate results
    /// via [`StatementBackend`]. `cancel` is this statement's token; record whatever
    /// `Backend::cancel` will need to identify this work.
    fn exec_direct(
        conn: &Self::Connection,
        cancel: &Self::CancelToken,
        sql: &str,
    ) -> Result<Self::Statement, Self::Error>;

    /// Prepares a SQL statement for later execution.
    ///
    /// Called by `SQLPrepareW`. Returns a prepared statement object (`Self::Statement`)
    /// that can be executed via [`Backend::execute`]. `cancel` is this statement's token;
    /// record whatever `Backend::cancel` will need to identify this work.
    fn prepare(
        conn: &Self::Connection,
        cancel: &Self::CancelToken,
        sql: &str,
    ) -> Result<Self::Statement, Self::Error>;

    /// Executes a previously prepared statement with the given parameter values.
    ///
    /// Called by `SQLExecute`. `params` contains one [`ColumnValue`] per bound
    /// parameter, in bind order (the assembled *input* values).
    ///
    /// Returns an [`ExecuteOutcome`]. Backends without output-parameter support
    /// return `Ok(ExecuteOutcome::default())` (the common case). A backend that
    /// produces `SQL_PARAM_OUTPUT` / `SQL_PARAM_INPUT_OUTPUT` values populates
    /// [`ExecuteOutcome::output_params`]; `stackable-odbc-core` then writes each value back
    /// into the application's bound parameter buffer, the symmetric counterpart
    /// of the `params` input above. `cancel` is this statement's token; record
    /// whatever `Backend::cancel` will need to identify this work.
    fn execute(
        conn: &Self::Connection,
        cancel: &Self::CancelToken,
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

    /// The catalog the session is currently using, if the data source has a
    /// current catalog at all.
    ///
    /// Read by `SQLGetConnectAttr(SQL_ATTR_CURRENT_CATALOG)` and, because the
    /// spec makes them one value, by `SQLGetInfo(SQL_DATABASE_NAME)`. An
    /// application's own `SQLSetConnectAttr` value takes precedence, since that
    /// is what it asked for; this answers when it has set nothing.
    ///
    /// Defaults to `None`, meaning "this data source has no current catalog to
    /// report" — the empty string, which is what both readers then produce.
    ///
    /// A backend that returns `Some` should also implement
    /// [`Backend::set_current_catalog`], or an application can read the catalog
    /// and not change it.
    fn current_catalog(_conn: &Self::Connection) -> Option<Cow<'static, str>> {
        None
    }

    /// Switch the session to `catalog`.
    ///
    /// Called by `SQLSetConnectAttr(SQL_ATTR_CURRENT_CATALOG)`. The spec's
    /// example is a driver sending `USE database`; for a single-tier driver it
    /// may be changing a directory.
    ///
    /// The default reports `HYC00`, which is the honest answer for a backend
    /// that cannot switch catalogs: storing the value and returning
    /// `SQL_SUCCESS` would tell an application its unqualified names now
    /// resolve somewhere they do not. Core stores the value only once this
    /// returns `Ok`.
    ///
    /// **Map "no such catalog" to `3D000`**, via
    /// [`crate::types::SqlState::invalid_catalog_name`].
    /// `SQLSetConnectAttr`'s `3D000` row — "the *Attribute* argument was
    /// SQL_CURRENT_CATALOG, and the specified catalog name was invalid" —
    /// carries no `(DM)` marker, so the driver owes it, and this method is the
    /// only place it can come from: core has no way to know which catalogs a
    /// data source has. Core propagates whatever this returns unchanged, so a
    /// backend that reports a bad catalog name as a generic `HY000` is the only
    /// reason an application would not see `3D000`.
    ///
    /// Note where the error surfaces. The spec lists this attribute as settable
    /// either side of a connection and notes that interoperable applications set
    /// it *before* one, in which case core applies it during `SQLDriverConnectW`
    /// / `SQLConnectW` / `SQLBrowseConnectW` and a failure here fails the
    /// connect — `3D000` from a function whose own diagnostics table does not
    /// list it. That is deliberate: degrading it would tell the application its
    /// connection failed for some unrelated reason.
    fn set_current_catalog(_conn: &Self::Connection, _catalog: &str) -> Result<(), Self::Error> {
        Err(OdbcError::NotImplemented {
            feature: "SQL_ATTR_CURRENT_CATALOG".into(),
        }
        .into())
    }

    /// Ask the data source to return at most `rows` rows from a result set.
    ///
    /// Called by `SQLSetStmtAttr(SQL_ATTR_MAX_ROWS)`. `rows` is the spec's
    /// `SQLULEN` verbatim; it is never `0`, because `0` means "return all rows"
    /// and core handles that without asking.
    ///
    /// **Core deliberately does not emulate this, and neither should a driver.**
    /// The spec says so outright: "a driver should not emulate
    /// SQL_ATTR_MAX_ROWS behavior for `SQLFetch` or `SQLFetchScroll` (if result
    /// set size limitations cannot be implemented at the data source) if it
    /// cannot guarantee that SQL_ATTR_MAX_ROWS will be implemented properly."
    /// The reason is in the attribute's own purpose — "this attribute is
    /// intended to reduce network traffic" — which counting rows on the client
    /// and discarding the rest cannot achieve. Implement this only where the
    /// data source can genuinely cap the result set (a `LIMIT`, a fetch size, a
    /// server-side row cap).
    ///
    /// Note the spec's scope: it "applies to all result sets on the
    /// *Statement*, including those returned by catalog functions", and
    /// "conceptually, it is applied when the result set is created".
    ///
    /// Three answers, as for [`Backend::set_query_timeout`]:
    ///
    /// - `Ok(())` — the cap is in force. Core stores the value, so
    ///   `SQLGetStmtAttr` reports back what the application asked for.
    /// - `Err(NotImplemented)`, the default — core substitutes `0` and posts
    ///   `01S02`, which the spec's closed `01S02` list names this attribute
    ///   for, so the application learns it got no cap rather than believing in
    ///   one that will never apply.
    /// - Any other `Err` — a real failure, reported as-is rather than turned
    ///   into a substitution.
    fn set_max_rows(_conn: &Self::Connection, _rows: usize) -> Result<(), Self::Error> {
        Err(OdbcError::NotImplemented {
            feature: "SQL_ATTR_MAX_ROWS".into(),
        }
        .into())
    }

    /// Ask the data source to return at most `bytes` from a character or binary
    /// column.
    ///
    /// Called by `SQLSetStmtAttr(SQL_ATTR_MAX_LENGTH)`. `bytes` is the spec's
    /// `SQLULEN` verbatim; it is never `0`, which means "return all available
    /// data".
    ///
    /// **Core deliberately does not emulate this either.** The spec restricts
    /// it to the data source in as many words: "this attribute is intended to
    /// reduce network traffic and should be supported only when the data source
    /// (as opposed to the driver) in a multiple-tier driver can implement it",
    /// and it warns applications off using it as a truncation mechanism at all
    /// — "this mechanism should not be used by applications to truncate data;
    /// to truncate data received, an application should specify the maximum
    /// buffer length in the *BufferLength* argument in `SQLBindCol` or
    /// `SQLGetData`". Truncating in core would move bytes over the wire and
    /// then throw them away, which is the opposite of the point.
    ///
    /// Truncation performed by the data source under this attribute returns
    /// `SQL_SUCCESS`, not `01004`: "if *ValuePtr* is less than the length of
    /// the available data, `SQLFetch` or `SQLGetData` truncates the data and
    /// returns SQL_SUCCESS."
    ///
    /// Same three answers as [`Backend::set_max_rows`].
    fn set_max_length(_conn: &Self::Connection, _bytes: usize) -> Result<(), Self::Error> {
        Err(OdbcError::NotImplemented {
            feature: "SQL_ATTR_MAX_LENGTH".into(),
        }
        .into())
    }

    /// Tell the data source whether this connection needs to support updates.
    ///
    /// Called by `SQLSetConnectAttr(SQL_ATTR_ACCESS_MODE)` with `true` for
    /// `SQL_MODE_READ_ONLY` and `false` for `SQL_MODE_READ_WRITE` (the
    /// default). A value set before connecting is applied at connect, since the
    /// spec lists this attribute as settable either side of one.
    ///
    /// **The default is `Ok(())` — accepting and ignoring — and that is
    /// spec-compliant here**, which is what makes this hook different from
    /// [`Backend::set_current_catalog`] and [`Backend::set_autocommit`], whose
    /// defaults refuse. The spec says so directly: read-only "is used by the
    /// driver or data source as an indicator that the connection is not
    /// required to support SQL statements that cause updates to occur ... **the
    /// driver is not required to prevent such statements from being submitted
    /// to the data source**", and "the behavior of the driver and data source
    /// when asked to process SQL statements that are not read-only during a
    /// read-only connection is implementation-defined".
    ///
    /// So this is a *hint*, not a guarantee, and an application must not treat
    /// `SQL_MODE_READ_ONLY` as a safety interlock. Storing it without telling
    /// the data source therefore misleads nobody about correctness — it only
    /// forgoes the optimisation the spec offers: "this mode can be used to
    /// optimize locking strategies, transaction management, or other areas as
    /// appropriate to the driver or data source."
    ///
    /// Override it where the data source has a real read-only session mode
    /// worth entering. Returning an error is reasonable for a backend that
    /// cannot enter one *and* judges that silently ignoring the request would
    /// mislead its users; core reports whatever SQLSTATE the error maps to.
    fn set_access_mode(_conn: &Self::Connection, _read_only: bool) -> Result<(), Self::Error> {
        Ok(())
    }

    /// Whether the connection to the data source has been lost.
    ///
    /// Read by `SQLGetConnectAttr(SQL_ATTR_CONNECTION_DEAD)`, which the spec
    /// makes read-only: "Both SQL_ATTR_AUTO_IPD and SQL_ATTR_CONNECTION_DEAD
    /// connection attributes can be returned by a call to `SQLGetConnectAttr`
    /// but cannot be set by a call to `SQLSetConnectAttr`." `true` becomes
    /// `SQL_CD_TRUE`, `false` becomes `SQL_CD_FALSE`.
    ///
    /// **This is what a connection pool reads before handing a connection
    /// out.** Answering `false` for a connection whose socket closed an hour
    /// ago is how a pool serves a dead connection to the next caller, which
    /// then fails on its first query for no reason the application can see.
    ///
    /// **Answer from state you already have; do not make a round trip.** The
    /// spec's own note on this function is that "a driver can improve
    /// performance by minimizing the number of times that information is sent
    /// or requested from the server", and a pool may call this on every
    /// checkout. A flag the error-mapping function sets when it sees a
    /// connection-level failure, or whatever liveness the client library
    /// already tracks, is the intended source.
    ///
    /// The default is `false`, which is correct for a backend that cannot tell:
    /// `SQL_CD_TRUE` asserts the connection *has been lost*, and a backend with
    /// no liveness signal has not observed that. Note the asymmetry — `false`
    /// means "not known to be dead", not "known to be alive".
    fn connection_dead(_conn: &Self::Connection) -> bool {
        false
    }

    /// Ask the data source to stop a statement that runs longer than
    /// `seconds`.
    ///
    /// Called by `SQLSetStmtAttr(SQL_ATTR_QUERY_TIMEOUT)`. `seconds` is the
    /// spec's `SQLULEN` verbatim, hence `usize`; it is never `0`, because `0`
    /// means "no timeout" and core handles that without asking.
    ///
    /// **This is the server-side half of the timeout.** A data source that can
    /// enforce a deadline itself does it far better than core can: it knows
    /// when the query actually started, it stops the work rather than merely
    /// abandoning it, and it needs no [`Backend::cancel`] implementation.
    /// Implement this in preference to relying on core's timer.
    ///
    /// Four answers, and the difference is load-bearing:
    ///
    /// - [`Ok(DataSource)`](crate::types::QueryTimeout::DataSource) — the data source enforces the
    ///   deadline itself. Core arms no timer and stores the value, so
    ///   `SQLGetStmtAttr` reports back what the application asked for.
    /// - [`Ok(CoreCancels)`](crate::types::QueryTimeout::CoreCancels) — the backend cannot set a
    ///   server-side deadline but *can* be cancelled, so core arms a timer that
    ///   calls [`Backend::cancel`] when the deadline passes. Returning this
    ///   asserts that `cancel` really cancels; see
    ///   [`QueryTimeout::CoreCancels`](crate::types::QueryTimeout::CoreCancels).
    /// - `Err(NotImplemented)`, the default — this backend can do neither. Core
    ///   substitutes `0` and posts `01S02`, which is the spec's own answer for
    ///   `SQL_ATTR_QUERY_TIMEOUT` ("the statement attributes that can be changed
    ///   are: ... SQL_ATTR_QUERY_TIMEOUT"), so `SQLGetStmtAttr` reports `0` and
    ///   the application learns it got no timeout rather than believing in one
    ///   that will never fire.
    /// - Any other `Err` — a real failure talking to the data source, reported
    ///   as-is. It is *not* turned into a substitution: an application told
    ///   `01S02` concludes "this driver caps my timeout", which is a different
    ///   thing from "the connection is broken".
    ///
    /// The spec's own use of `01S02` here is clamping — "if the specified
    /// timeout exceeds the maximum timeout in the data source or is smaller
    /// than the minimum timeout, `SQLSetStmtAttr` substitutes that value and
    /// returns SQLSTATE 01S02". A backend that clamps should therefore *not*
    /// return an error: return `Ok` and core stores what was asked for.
    /// Reporting the clamped value instead needs core to know it, which this
    /// signature does not carry — see the note on the `SQL_ATTR_QUERY_TIMEOUT`
    /// arm in `ffi/stmt_attr.rs`.
    ///
    /// **Scope caveat.** `SQL_ATTR_QUERY_TIMEOUT` is a *statement* attribute
    /// but this hook receives only the connection, so a backend that applies it
    /// session-wide gives every statement on that connection the most recently
    /// set value. Core cannot fix that without changing the signature of the
    /// nine statement-producing methods, which would break every existing
    /// driver. A backend that can scope a deadline per statement should carry
    /// the value on its own connection state and apply it at execution time.
    fn set_query_timeout(
        _conn: &Self::Connection,
        _seconds: usize,
    ) -> Result<crate::types::QueryTimeout, Self::Error> {
        Err(OdbcError::NotImplemented {
            feature: "SQL_ATTR_QUERY_TIMEOUT".into(),
        }
        .into())
    }

    /// Return driver-level info that does not require an active connection.
    ///
    /// The Windows Driver Manager calls `SQLGetInfoW` for types like
    /// `SQL_DRIVER_ODBC_VER` *before* the connection is established, and reads
    /// the answer to decide whether the driver is ODBC 3.x — a wrong or missing
    /// one costs the connection its 3.x features.
    ///
    /// **Overriding this is no longer needed for that group.** `NotImplemented`
    /// falls through to [`default_get_info`], which answers `SQL_DRIVER_NAME`
    /// and `SQL_DRIVER_VER` from [`Backend::driver_name`] and
    /// [`Backend::driver_version`], and `SQL_DRIVER_ODBC_VER`,
    /// `SQL_ASYNC_DBC_FUNCTIONS` and `SQL_MAX_CONCURRENT_ACTIVITIES` from what
    /// core knows about itself. Those are the five the Driver Manager needs,
    /// and they are required declarations now rather than a checklist item.
    ///
    /// Override it for a *further* info type a backend can answer before
    /// connecting, which is rare: most of `SQLGetInfo` describes the data
    /// source, and there is no data source yet.
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
    /// Called by `SQLGetFunctions`. The returned slice must contain one
    /// [`FunctionId`](crate::function_id::FunctionId) entry
    /// per exported FFI function. `stackable-odbc-core` maps 3.x IDs to their 2.x equivalents
    /// automatically for the legacy function array.
    ///
    /// This method takes no connection, so nothing about it varies at
    /// runtime; it returns `Cow<'static, [FunctionId]>` rather than
    /// `&'static [FunctionId]` for uniformity with the trait's other
    /// list-returning methods, not because it needs to be computed.
    fn get_functions() -> Cow<'static, [crate::function_id::FunctionId]>;

    /// Returns type information rows describing the SQL types supported by this driver.
    ///
    /// Called by `SQLGetTypeInfoW`. The returned rows should include both ANSI and Unicode
    /// type variants so that ODBC applications can match on `SQL_VARCHAR` as well as
    /// `SQL_WVARCHAR`.
    ///
    /// Takes a connection because the type list can genuinely differ by data
    /// source (a server-version probe gating a type's availability, say), so
    /// the answer is computed rather than a `'static` borrow — hence
    /// `Cow<'static, [TypeInfoRow]>` rather than `&'static [TypeInfoRow]`.
    fn get_type_info(conn: &Self::Connection) -> Cow<'static, [TypeInfoRow]>;

    /// Returns the rows describing tables matching the given filter criteria.
    ///
    /// Called by `SQLTablesW`. All filter parameters are optional; `None` means
    /// no filter on that dimension. `cancel` is this statement's token; record
    /// whatever `Backend::cancel` will need to identify this work.
    ///
    /// **Return the rows in any order.** Core sorts them into the spec's order
    /// (TABLE_TYPE, TABLE_CAT, TABLE_SCHEM, TABLE_NAME) and builds the result
    /// set, so a backend does not need an ORDER BY for correctness.
    ///
    /// `table_types` is the parsed `TableType` value list — core has already
    /// split it on commas and stripped the optional single quotes, so a backend
    /// never parses it. An empty slice means no table-type filter.
    fn tables(
        conn: &Self::Connection,
        cancel: &Self::CancelToken,
        catalog: Option<&str>,
        schema: Option<&str>,
        table: Option<&str>,
        table_types: &[String],
    ) -> Result<Vec<TableRow>, Self::Error>;

    /// The table types this data source has, for `SQLTables`'
    /// `SQL_ALL_TABLE_TYPES` enumeration — e.g. `["TABLE", "VIEW"]`.
    ///
    /// Required, not defaulted, for the reason in AGENTS.md: an empty list is
    /// an *answer* ("this data source has no table types"), not "unknown", and
    /// there is no `supports_*` method to derive it from the way
    /// `SQL_ALL_CATALOGS` derives from [`Backend::supports_catalogs`]. Any
    /// value core invented here would be a claim about a data source it knows
    /// nothing about.
    ///
    /// Values should be upper case: the spec has applications specify table
    /// types in upper case and the driver map them to whatever the data source
    /// needs.
    ///
    /// Takes no `cancel` token because it is a static declaration rather than
    /// a query — unlike [`Backend::catalogs`] and [`Backend::schemas`].
    fn table_types(conn: &Self::Connection) -> Vec<Cow<'static, str>>;

    /// The catalog names on this data source, for `SQLTables`'
    /// `SQL_ALL_CATALOGS` enumeration.
    ///
    /// Runtime data rather than a static capability, so this takes `cancel`
    /// like every other backend call. Core only calls it when
    /// [`Backend::supports_catalogs`] already returned `true` — a backend that
    /// says it has no catalogs gets an empty result set without being asked —
    /// so a backend that claims catalogs and leaves this defaulted is a driver
    /// bug, which core reports as `HYC00` rather than silently returning
    /// nothing.
    ///
    /// **Return the names in any order.** Core sorts the result set.
    fn catalogs(
        _conn: &Self::Connection,
        _cancel: &Self::CancelToken,
    ) -> Result<Vec<String>, Self::Error> {
        Err(OdbcError::NotImplemented {
            feature: "catalogs".into(),
        }
        .into())
    }

    /// The schema names on this data source, for `SQLTables`'
    /// `SQL_ALL_SCHEMAS` enumeration. See [`Backend::catalogs`]; this is gated
    /// on [`Backend::supports_schemas`] the same way.
    fn schemas(
        _conn: &Self::Connection,
        _cancel: &Self::CancelToken,
    ) -> Result<Vec<String>, Self::Error> {
        Err(OdbcError::NotImplemented {
            feature: "schemas".into(),
        }
        .into())
    }

    /// Returns the rows describing columns matching the given filter criteria.
    ///
    /// Called by `SQLColumnsW`. All filter parameters are optional; `None` means no filter
    /// on that dimension. `cancel` is this statement's token; record whatever
    /// `Backend::cancel` will need to identify this work.
    ///
    /// **Return the rows in any order.** Core sorts them into the spec's order
    /// (TABLE_CAT, TABLE_SCHEM, TABLE_NAME, ORDINAL_POSITION) and builds the
    /// result set, so a backend does not need an ORDER BY for correctness.
    fn columns(
        conn: &Self::Connection,
        cancel: &Self::CancelToken,
        catalog: Option<&str>,
        schema: Option<&str>,
        table: Option<&str>,
        column: Option<&str>,
    ) -> Result<Vec<ColumnRow>, Self::Error>;

    /// Return the primary key columns for the given table.
    ///
    /// Called by `SQLPrimaryKeysW`. Backends that do not support this can leave the
    /// default implementation which returns `NotImplemented`. `cancel` is this
    /// statement's token; record whatever `Backend::cancel` will need to identify
    /// this work.
    ///
    /// **Return the rows in any order.** Core sorts them into the spec's order
    /// (TABLE_CAT, TABLE_SCHEM, TABLE_NAME, KEY_SEQ) and builds the result set,
    /// so a backend does not need an ORDER BY for correctness.
    fn primary_keys(
        _conn: &Self::Connection,
        _cancel: &Self::CancelToken,
        _catalog: Option<&str>,
        _schema: Option<&str>,
        _table: Option<&str>,
    ) -> Result<Vec<PrimaryKeyRow>, Self::Error> {
        Err(OdbcError::NotImplemented {
            feature: "primary_keys".into(),
        }
        .into())
    }

    /// Return foreign key relationships.
    ///
    /// Called by `SQLForeignKeysW`. Either `pk_table` or `fk_table` (or both) may be supplied.
    /// Backends that do not support this can leave the default implementation which returns
    /// `NotImplemented`. `cancel` is this statement's token; record whatever
    /// `Backend::cancel` will need to identify this work.
    ///
    /// **Return the rows in any order.** Core sorts them into the spec's order
    /// and builds the result set, so a backend does not need an ORDER BY for
    /// correctness. Which of the two orders the spec defines applies is
    /// decided by core from the arguments: FKTABLE_CAT, FKTABLE_SCHEM,
    /// FKTABLE_NAME, KEY_SEQ when `pk_table` was supplied, and PKTABLE_CAT,
    /// PKTABLE_SCHEM, PKTABLE_NAME, KEY_SEQ otherwise.
    #[allow(clippy::too_many_arguments)]
    fn foreign_keys(
        _conn: &Self::Connection,
        _cancel: &Self::CancelToken,
        _pk_catalog: Option<&str>,
        _pk_schema: Option<&str>,
        _pk_table: Option<&str>,
        _fk_catalog: Option<&str>,
        _fk_schema: Option<&str>,
        _fk_table: Option<&str>,
    ) -> Result<Vec<ForeignKeyRow>, Self::Error> {
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
    /// `cancel` is this statement's token; record whatever `Backend::cancel`
    /// will need to identify this work.
    ///
    /// **Return the rows in any order.** Core sorts them into the spec's order
    /// (NON_UNIQUE, TYPE, INDEX_QUALIFIER, INDEX_NAME, ORDINAL_POSITION) and
    /// builds the result set, so a backend does not need an ORDER BY for
    /// correctness.
    fn statistics(
        _conn: &Self::Connection,
        _cancel: &Self::CancelToken,
        _catalog: Option<&str>,
        _schema: Option<&str>,
        _table: Option<&str>,
        _unique_only: bool,
    ) -> Result<Vec<StatisticsRow>, Self::Error> {
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
    /// response when no such columns exist. `cancel` is this statement's
    /// token; record whatever `Backend::cancel` will need to identify this
    /// work.
    ///
    /// **Return the rows in any order.** Core sorts them into the spec's order
    /// (SCOPE) and builds the result set, so a backend does not need an ORDER
    /// BY for correctness.
    #[allow(clippy::too_many_arguments)]
    fn special_columns(
        _conn: &Self::Connection,
        _cancel: &Self::CancelToken,
        _identifier_type: IdentifierType,
        _catalog: Option<&str>,
        _schema: Option<&str>,
        _table: Option<&str>,
        _scope: Scope,
        _nullable: Nullable,
    ) -> Result<Vec<SpecialColumnRow>, Self::Error> {
        Err(OdbcError::NotImplemented {
            feature: "special_columns".into(),
        }
        .into())
    }

    /// Return the stored procedures matching the given filter criteria.
    ///
    /// Called by `SQLProceduresW`. All filter parameters are optional; `None`
    /// means no filter on that dimension. `cancel` is this statement's token;
    /// record whatever `Backend::cancel` will need to identify this work.
    ///
    /// The default returns no rows rather than `NotImplemented`, unlike
    /// [`Backend::primary_keys`] and its neighbours: a data source with no
    /// stored procedures has none to report, which is a legitimate answer, and
    /// this function returned an empty result set for every driver before the
    /// hook existed. Erroring instead would regress a working call.
    ///
    /// **Return the rows in any order.** Core sorts them into the spec's order
    /// (PROCEDURE_CAT, PROCEDURE_SCHEM, PROCEDURE_NAME) and builds the result
    /// set, so a backend does not need an ORDER BY for correctness.
    ///
    /// `SQL_ATTR_METADATA_ID` has already been applied: when it is `SQL_TRUE`
    /// core has stripped delimiters, case-folded and escaped each argument, so
    /// these are always ordinary pattern values here.
    fn procedures(
        _conn: &Self::Connection,
        _cancel: &Self::CancelToken,
        _catalog: Option<&str>,
        _schema: Option<&str>,
        _proc_name: Option<&str>,
    ) -> Result<Vec<ProcedureRow>, Self::Error> {
        Ok(Vec::new())
    }

    /// Return the parameters and result-set columns of the matching stored
    /// procedures.
    ///
    /// Called by `SQLProcedureColumnsW`. Defaulted to no rows for the same
    /// reason as [`Backend::procedures`], and `SQL_ATTR_METADATA_ID` has
    /// likewise already been applied to all four arguments.
    ///
    /// **Return the rows in any order.** Core sorts them into the spec's order
    /// (PROCEDURE_CAT, PROCEDURE_SCHEM, PROCEDURE_NAME, COLUMN_TYPE) and
    /// builds the result set, so a backend does not need an ORDER BY for
    /// correctness.
    fn procedure_columns(
        _conn: &Self::Connection,
        _cancel: &Self::CancelToken,
        _catalog: Option<&str>,
        _schema: Option<&str>,
        _proc_name: Option<&str>,
        _column: Option<&str>,
    ) -> Result<Vec<ProcedureColumnRow>, Self::Error> {
        Ok(Vec::new())
    }

    /// Return the column-level privileges on a single table.
    ///
    /// Called by `SQLColumnPrivilegesW`. Defaulted to no rows for the same
    /// reason as [`Backend::procedures`], and `SQL_ATTR_METADATA_ID` has
    /// likewise already been applied to all four arguments.
    ///
    /// **Return the rows in any order.** Core sorts them into the spec's order
    /// (TABLE_CAT, TABLE_SCHEM, TABLE_NAME, COLUMN_NAME, PRIVILEGE) and builds
    /// the result set, so a backend does not need an ORDER BY for correctness.
    fn column_privileges(
        _conn: &Self::Connection,
        _cancel: &Self::CancelToken,
        _catalog: Option<&str>,
        _schema: Option<&str>,
        _table: Option<&str>,
        _column: Option<&str>,
    ) -> Result<Vec<ColumnPrivilegeRow>, Self::Error> {
        Ok(Vec::new())
    }

    /// Return the table-level privileges on the matching tables.
    ///
    /// Called by `SQLTablePrivilegesW`. Defaulted to no rows for the same
    /// reason as [`Backend::procedures`], and `SQL_ATTR_METADATA_ID` has
    /// likewise already been applied to all three arguments.
    ///
    /// **Return the rows in any order.** Core sorts them into the spec's order
    /// (TABLE_CAT, TABLE_SCHEM, TABLE_NAME, PRIVILEGE, GRANTEE) and builds the
    /// result set, so a backend does not need an ORDER BY for correctness.
    /// Note that PRIVILEGE outranks GRANTEE, unlike `SQLColumnPrivileges`.
    fn table_privileges(
        _conn: &Self::Connection,
        _cancel: &Self::CancelToken,
        _catalog: Option<&str>,
        _schema: Option<&str>,
        _table: Option<&str>,
    ) -> Result<Vec<TablePrivilegeRow>, Self::Error> {
        Ok(Vec::new())
    }

    /// Everything needed to cancel work on a statement from a thread holding
    /// no lock on the connection, while another thread may be executing on it
    /// concurrently.
    ///
    /// Two shapes are legitimate, and the choice is the backend's:
    ///
    /// - **Standalone** — the token carries its own channel and shares
    ///   nothing with the connection. libpq's `PGcancel` (a snapshot of the
    ///   backend PID and secret key, used over a fresh socket) and MySQL's
    ///   second connection issuing `KILL QUERY` are both this shape. Prefer
    ///   it.
    /// - **Aliasing** — the token holds the connection itself, sound only
    ///   where the backend's own client library documents concurrent use.
    ///   `sqlite3_interrupt` is the example: SQLite guarantees it is safe to
    ///   call from a thread other than the one running the query.
    ///
    /// An aliasing token **must** keep its target alive through an `Arc`,
    /// never a raw handle: core clones the token out before doing anything
    /// else, so it has to survive a concurrent `SQLDisconnect`. SQLite's own
    /// documentation states the requirement directly — "it is not safe to
    /// call this routine with a database connection that is closed or might
    /// close before `sqlite3_interrupt()` returns" — and the same reasoning
    /// applies to any aliasing token, not only SQLite's.
    ///
    /// A backend that cannot cancel anything uses `()`.
    ///
    /// `'static` is required, not incidental: core stores the token as
    /// `Arc<dyn Any + Send + Sync>` in the registry so the registry itself
    /// can stay non-generic over `Backend`, and `Any` is implemented only for
    /// `'static` types.
    type CancelToken: Send + Sync + 'static;

    /// Build the cancel token for a connection.
    ///
    /// Core calls this immediately before the statement's first backend call
    /// site (`exec_direct`, `prepare`, a catalog function, ...), not at
    /// `SQLAllocHandle(SQL_HANDLE_STMT)`. A statement can be allocated on a
    /// connection that is not yet open (`SQLAllocHandle`'s 08003 for that
    /// case is Driver-Manager-owned, so core never checks it), which leaves
    /// no `&Self::Connection` to build a token from until a
    /// statement-producing call actually supplies one. Resolution can precede
    /// a fallible step (parameter collection, say) that then fails before any
    /// backend call happens; the token still gets built in that case, just
    /// unused — harmless, since it is cheap and the statement may yet make a
    /// real call later.
    ///
    /// A **new token is minted at every statement-producing call**, including
    /// a second `SQLExecute` on the same handle; the statement's stored token
    /// is replaced each time, and the cursor-consuming calls (`SQLFetch`,
    /// `SQLGetData`, ...) read that execution's token rather than minting one.
    /// An earlier revision built a single token per statement and never
    /// replaced it, which left a cancelled statement permanently unusable —
    /// `Backend::cancel` marks the token, and the next execution reused the
    /// marked one. The spec requires the opposite: "After the statement has
    /// been canceled, the application can call SQLExecute or SQLExecDirect
    /// again."
    ///
    /// The consequence a backend author should plan for is that a `SQLCancel`
    /// which cloned an *earlier* execution's token signals only that
    /// execution. That is the spec's own outcome for a cancel arriving when
    /// the work it named is already over: "a call to SQLCancel when no
    /// processing is being done on the statement ... has is \[sic\] no effect
    /// at all."
    ///
    /// Build the token with the connection's parameters in hand here —
    /// do not defer the real assembly to `cancel`. MariaDB's ODBC-401 is the
    /// failure this rule exists to prevent: its cancel channel was assembled
    /// lazily, after the original connection's TLS settings were no longer
    /// available, so cancelling an encrypted connection failed silently.
    ///
    /// A backend whose cancellation needs a value not known until execution
    /// (e.g. a query id) returns an empty shared slot here —
    /// `Arc<Mutex<Option<QueryId>>>` — and fills it from whichever of the nine
    /// statement-producing methods actually runs the work (a catalog method
    /// like `tables` is a real query with its own id on some backends, not
    /// only `exec_direct`/`prepare`/`execute`), each of which receives the
    /// same token.
    fn cancel_token(conn: &Self::Connection) -> Self::CancelToken;

    /// Cancel whatever the token refers to.
    ///
    /// Called by `SQLCancel`, possibly while another thread is executing on
    /// the same statement. It receives neither a connection nor a statement —
    /// deliberately: that signature keeps this call off the *guarded state*
    /// (there is no `&mut Self::Statement` or `&Self::Connection` reachable
    /// through it, so a backend cannot reach into a handle another thread may
    /// be mutating).
    ///
    /// That is **not** the same as "core never holds the connection's lock
    /// while calling this" — whether it does depends on which of
    /// `SQLCancel`'s two cases applies:
    ///
    /// - **Another thread holds the connection** (the cross-thread cancel the
    ///   spec singles out): core calls this holding **no lock at all**.
    /// - **The connection is idle** (a data-at-execution cancel, or "no
    ///   processing in progress"): core calls this **while holding the
    ///   connection's group lock**, because it needs that same scope
    ///   afterward to post the resulting diagnostic.
    ///
    /// A `cancel` implementation must therefore never block on anything that
    /// itself waits for this connection's lock — calling back into another
    /// `SQLxxx` entry point on the same connection, for instance — or the
    /// idle-path case deadlocks the calling thread. Blocking on something
    /// outside the connection (a network round-trip to the data source
    /// asking it to cancel a query, say) is fine on both paths; it just means
    /// `SQLCancel` itself does not return until that finishes.
    ///
    /// Any state a backend needs to clear after a cancellation (e.g.
    /// streaming/pagination state) has to live in the token itself, or behind
    /// synchronisation the backend owns.
    ///
    /// The default returns `NotImplemented`, which `SQLCancel` treats as
    /// success: there was nothing to cancel.
    fn cancel(_token: &Self::CancelToken) -> Result<(), Self::Error> {
        Err(OdbcError::NotImplemented {
            feature: "cancel".into(),
        }
        .into())
    }

    /// Whether this token has been signalled by [`Backend::cancel`].
    ///
    /// Core calls this **only after a backend call returned an error**, to
    /// decide whether that error was a cancellation. When it answers `true`,
    /// core discards the backend's SQLSTATE and reports `HY008` instead —
    /// the spec's "If the original function is canceled, it returns SQL_ERROR
    /// and SQLSTATE HY008 (Operation canceled)."
    ///
    /// Core never asks on the success path, and a backend must not expect it
    /// to. The spec explicitly allows a cancelled execution to finish anyway:
    /// "it is possible for the execution to succeed and return SQL_SUCCESS
    /// while the cancel is also successful." A successful call therefore stays
    /// successful whatever this would have answered.
    ///
    /// **Implement this whenever you implement [`Backend::cancel`].** The two
    /// are a pair: `cancel` signals, this observes. A backend that signals but
    /// cannot observe still cancels the work — it just reports the resulting
    /// failure with whatever SQLSTATE its own error mapping produced, usually
    /// `HY000`, which tells the application nothing about why it failed.
    ///
    /// The default is `false`, which is the correct answer for a backend that
    /// leaves `cancel` defaulted and therefore has no cancellation at all.
    ///
    /// Called with no lock held on the cross-thread path and with the
    /// connection's group lock held on the idle path, exactly as
    /// [`Backend::cancel`] is, so the same "never block on anything that waits
    /// for this connection's lock" rule applies.
    fn is_cancelled(_token: &Self::CancelToken) -> bool {
        false
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
    /// changes both together, which is the point: the value a driver reports
    /// and the value it applies have to be the same one, or it advertises a
    /// behaviour it does not implement.
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
    ///
    /// `Cow<'static, [Cow<'static, str>]>`, not `&'static [&'static str]`: this
    /// method takes no connection, so nothing about it varies at runtime, but
    /// it matches the shape of the trait's other list-returning methods for
    /// uniformity of vocabulary rather than because it needs to.
    fn browse_connect_attrs() -> Cow<'static, [Cow<'static, str>]> {
        Cow::Borrowed(&[])
    }

    /// The escape-translation dialect for this backend (`{fn}` name map,
    /// identifier quotes, date-literal rendering). Called by the generic
    /// `SQLExecDirect`/`SQLPrepare`/`SQLNativeSql` translation. The default is a
    /// neutral ANSI dialect.
    fn escape_dialect(_conn: &Self::Connection) -> crate::escape::EscapeDialect {
        crate::escape::EscapeDialect::ansi_default()
    }

    /// Describes the 1-based `parameter_number`-th parameter marker of `sql`,
    /// for `SQLDescribeParam`.
    ///
    /// `sql` is the prepared statement text as the backend received it (after
    /// escape translation). `Ok(None)` means "this backend cannot tell", which
    /// is the default and which leaves core reporting a generic
    /// `VARCHAR(SQL_DEFAULT_PARAM_SIZE)` — usable, but wrong for any parameter
    /// that is not a string, which is what leads a client that sizes its buffers
    /// from this to send a number as text and get a type error back from the
    /// data source.
    ///
    /// Override this wherever the data source can be asked. A backend that can
    /// describe *some* parameters should answer `Ok(None)` for the rest rather
    /// than guess: core's fallback is at least a documented, uniform guess,
    /// whereas a wrong specific type is indistinguishable from a real answer.
    ///
    /// Note that `SQLGetInfo(SQL_DESCRIBE_PARAMETER)` reports `"Y"` either way.
    /// That is not a claim about accuracy — the spec defines it as whether the
    /// driver supports the *call*, and core always answers it.
    fn describe_param(
        _conn: &Self::Connection,
        _sql: &str,
        _parameter_number: u16,
    ) -> Result<Option<crate::types::ParamDescriptor>, Self::Error> {
        Ok(None)
    }

    /// The data-source-dependent widths of this driver's catalog result-set
    /// columns, and the SQL type its character columns report.
    ///
    /// Every catalog result set the driver can produce derives from this one
    /// value — `SQLTables`, `SQLColumns`, `SQLPrimaryKeys`, `SQLForeignKeys`,
    /// `SQLStatistics`, `SQLSpecialColumns`, `SQLProcedures`,
    /// `SQLProcedureColumns`, `SQLColumnPrivileges`, `SQLTablePrivileges` and
    /// `SQLGetTypeInfo` — so they cannot describe the same column two ways.
    ///
    /// The default suits a data source with no identifier length limit. A
    /// driver for a source that *does* impose one — PostgreSQL's 63-character
    /// `NAMEDATALEN - 1`, say — overrides this, and both its catalog result
    /// sets and its `SQL_MAX_*_NAME_LEN` answers follow from the one override.
    ///
    /// Deliberately takes no connection, unlike the capability methods below.
    /// Four of the catalog functions it serves — `SQLProcedures`,
    /// `SQLProcedureColumns`, `SQLColumnPrivileges` and `SQLTablePrivileges` —
    /// return an empty result set without resolving one, and requiring a
    /// connection here would put a connection lookup, and an error path, into
    /// paths that have neither. A driver whose identifier limit varies by
    /// server version reports the widest form here and narrows the
    /// `SQL_MAX_*_NAME_LEN` answers through the per-connection hooks.
    fn catalog_result_column_widths() -> CatalogResultColumnWidths {
        CatalogResultColumnWidths::default()
    }

    // ------------------------------------------------------------------
    // Capability declarations
    //
    // Each takes `&Self::Connection`, because `SQLGetInfo` is a
    // *per-connection* call: what a data source can do is a property of the
    // connection, not of the driver binary. A backend whose answer depends on
    // the server version reads it from the connection here; one whose answer is
    // fixed ignores the argument. Modelling these as associated functions would
    // make a driver pick one answer for every server it ever talks to.
    //
    // Exactly three declarations take no connection, for two different
    // reasons.
    //
    // `cursor_commit_behavior` and `cursor_rollback_behavior`, because
    // `SQLGetInfo` must answer `SQL_CURSOR_COMMIT_BEHAVIOR` and
    // `SQL_CURSOR_ROLLBACK_BEHAVIOR` *before* a connection exists: the Windows
    // Driver Manager queries info types ahead of `SQLDriverConnectW`, and
    // falling through to the shape default there answers `SQL_CB_DELETE`,
    // which is the exact claim these two hooks exist to stop core from
    // inventing.
    //
    // `catalog_result_column_widths`, for an unrelated reason: it is not an
    // `SQLGetInfo` answer at all. Four catalog functions (`SQLProcedures`,
    // `SQLProcedureColumns`, `SQLColumnPrivileges`, `SQLTablePrivileges`)
    // return an empty result set without resolving a connection, and requiring
    // one here would put a connection lookup, and an error path, into paths
    // that have neither.
    //
    // The rule all three share is the general one: a declaration consumed on a
    // path that has no connection cannot require one. Every other capability
    // here is consumed only once a connection exists, so it takes one.
    // ------------------------------------------------------------------

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
    fn supports_catalogs(conn: &Self::Connection) -> bool;

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
    fn supports_schemas(conn: &Self::Connection) -> bool;

    /// The `SQL_ALTER_TABLE` (86) capability bitmask — an OR of the
    /// [`SQL_AT_*`](crate::types::SQL_AT_ADD_COLUMN_SINGLE) constants.
    ///
    /// Required rather than defaulted for the reason a capability bitmap
    /// always should be: `0` means "this data source cannot `ALTER TABLE` at
    /// all", which is a claim, not an absence of one. A backend author is
    /// unlikely to notice a capability they never wrote code for, so the
    /// compiler asks instead. Return `0` only if that is genuinely true.
    fn alter_table_support(conn: &Self::Connection) -> u32;

    /// The `SQL_OJ_CAPABILITIES` (115) bitmask — an OR of the
    /// [`SQL_OJ_*`](crate::types::SQL_OJ_LEFT) constants.
    ///
    /// Required for the same reason as [`Backend::alter_table_support`]: `0`
    /// asserts that the data source supports no outer joins whatsoever.
    fn outer_join_capabilities(conn: &Self::Connection) -> u32;

    /// The `SQL_GROUP_BY` (88) relationship between the columns in a
    /// `GROUP BY` clause and the non-aggregated columns in the select list —
    /// one of the [`SQL_GB_*`](crate::types::SQL_GB_NO_RELATION) values.
    ///
    /// Required because every value here is a claim, `0`
    /// (`SQL_GB_NOT_SUPPORTED`, "GROUP BY is not supported") included. There is
    /// none core could pick that would merely be permissive: even
    /// `SQL_GB_NO_RELATION` is one the spec says an entry-level driver does not
    /// return — "a SQL-92 Entry level-conformant driver will always return the
    /// SQL_GB_GROUP_BY_EQUALS_SELECT option as supported."
    fn group_by(conn: &Self::Connection) -> u16;

    /// The `SQL_NULL_COLLATION` (85) position of NULLs in a sorted result set
    /// — one of the [`SQL_NC_*`](crate::types::SQL_NC_END) values.
    ///
    /// Required because `0` is [`SQL_NC_HIGH`](crate::types::SQL_NC_HIGH), a
    /// substantive answer ("NULLs sort high, depending on ASC/DESC") rather
    /// than an absence of one — so a shape-derived default would silently make
    /// that claim for every backend.
    fn null_collation(conn: &Self::Connection) -> u16;

    /// How the data source treats *unquoted* identifiers —
    /// `SQL_IDENTIFIER_CASE` (28), one of the
    /// [`SQL_IC_*`](crate::types::SQL_IC_UPPER) values.
    ///
    /// Required because the shape-aware default cannot produce a legal answer
    /// here: the spec defines `SQL_IC_UPPER` (1), `SQL_IC_LOWER` (2),
    /// `SQL_IC_SENSITIVE` (3) and `SQL_IC_MIXED` (4), and `0` is none of them.
    /// Unlike the capability methods where zero is a substantive claim, there
    /// is no value core could pick that is merely *understated* — every choice
    /// is a different assertion about how the data source folds identifiers,
    /// and an application uses it to decide how to quote generated SQL.
    ///
    /// Distinct from [`Backend::quoted_identifier_case`], which describes
    /// *quoted* identifiers.
    fn identifier_case(conn: &Self::Connection) -> u16;

    /// How the data source treats *quoted* identifiers —
    /// `SQL_QUOTED_IDENTIFIER_CASE` (93), one of the
    /// [`SQL_IC_*`](crate::types::SQL_IC_UPPER) values.
    ///
    /// The counterpart of [`Backend::identifier_case`] and required for the
    /// same reason: `0` is not one of the four legal values, so there is no
    /// understated answer to fall back on. Every choice is a different
    /// assertion about what the system catalog stores, and the two are
    /// genuinely independent — a data source that upper-cases unquoted
    /// identifiers commonly stores quoted ones verbatim.
    ///
    /// Not derivable from `identifier_case`: in the MySQL family the answer
    /// depends on server configuration rather than on the unquoted rule.
    fn quoted_identifier_case(conn: &Self::Connection) -> u16;

    /// The `SQL_TXN_CAPABLE` (46) level: what the data source allows inside a
    /// transaction, as one of the [`SQL_TC_*`](crate::types::SQL_TC_ALL)
    /// values.
    ///
    /// Required because `0` is
    /// [`SQL_TC_NONE`](crate::types::SQL_TC_NONE), "transactions not
    /// supported" — the strongest possible denial, and the one a shape-aware
    /// default would make on a backend's behalf. A backend that declares an
    /// isolation level in [`Backend::txn_isolation_options`] and implements
    /// [`Backend::end_tran`] would then be reported as having no transactions
    /// at all, which is the self-contradiction the constrained-info-type rule
    /// in AGENTS.md exists to prevent. A test pins the pair: `SQL_TC_NONE` if
    /// and only if `txn_isolation_options` is `0`.
    ///
    /// Return `u16`, not `u32`: the info type is `SQLUSMALLINT`, and the
    /// `SQL_TC_*` constants are typed `u32` for use in bitmask expressions.
    fn txn_capable(conn: &Self::Connection) -> u16;

    /// Whether the data source supports the Integrity Enhancement Facility —
    /// `SQL_INTEGRITY` (73), reported as `"Y"` or `"N"`.
    ///
    /// The IEF is referential-integrity DDL: `FOREIGN KEY`, `CHECK` and the
    /// rest. The spec's wording is "`"Y"` if **the data source** supports the
    /// Integrity Enhancement Facility", so this is a statement about the data
    /// source and not about the driver — which is why core cannot answer it
    /// from anything it knows about itself.
    fn integrity(conn: &Self::Connection) -> bool;

    /// Whether more than one transaction can be active at a time —
    /// `SQL_MULTIPLE_ACTIVE_TXN` (37), reported as `"Y"` or `"N"`.
    ///
    /// The spec: "`"Y"` if the driver supports more than one active
    /// transaction at the same time, `"N"` if only one transaction can be
    /// active at any time." A driver whose connections are independent answers
    /// `"Y"`, and `"N"` is a real restriction an application plans around, so
    /// neither is a safe default.
    fn multiple_active_txn(conn: &Self::Connection) -> bool;

    /// The characters other than `a`–`z`, `A`–`Z`, `0`–`9` and `_` that may
    /// appear in an identifier without delimiting it —
    /// `SQL_SPECIAL_CHARACTERS` (94).
    ///
    /// Required for the same reason as [`Backend::keywords`]: an empty list is
    /// an answer, not an absence. An application reads this to decide when an
    /// identifier must be quoted, so `""` asserts that nothing beyond the
    /// alphanumerics and underscore is legal unquoted — a claim about the data
    /// source that core has no way to make.
    fn special_characters(conn: &Self::Connection) -> Cow<'static, str>;

    /// Whether the connected user can execute every procedure `SQLProcedures`
    /// returns — `SQL_ACCESSIBLE_PROCEDURES` (20), reported as `"Y"` or `"N"`.
    ///
    /// The counterpart of [`Backend::accessible_tables`], and required for the
    /// same reason: `"Y"` is a guarantee about the connected principal's
    /// privileges that only the backend can make.
    fn accessible_procedures(conn: &Self::Connection) -> bool;

    /// The driver's own name — `SQL_DRIVER_NAME` (6).
    ///
    /// Takes no connection: the Windows Driver Manager asks for driver
    /// identity *before* `SQLDriverConnectW`, and an answer that needed a
    /// connection could not be given then.
    ///
    /// Required because core has no name to give and the empty string is not a
    /// usable one. With this and [`Backend::driver_version`] declared, core
    /// answers the whole pre-connect identity group itself, so a driver no
    /// longer has to remember to override
    /// [`Backend::get_info_pre_connect`] for the Driver Manager's benefit.
    fn driver_name() -> Cow<'static, str>;

    /// The driver's own version — `SQL_DRIVER_VER` (7), in the spec's
    /// `##.##.####` form, which [`crate::types::format_odbc_version`] produces.
    ///
    /// Takes no connection, for the reason given on [`Backend::driver_name`].
    fn driver_version() -> Cow<'static, str>;

    /// The name of the DBMS this connection reached — `SQL_DBMS_NAME` (17).
    ///
    /// Required because only the backend knows what it connected to, and the
    /// empty string a shape-aware default would produce is what tools display
    /// and log when they identify a data source.
    fn dbms_name(conn: &Self::Connection) -> Cow<'static, str>;

    /// The version of the DBMS this connection reached — `SQL_DBMS_VER` (18),
    /// in the spec's `##.##.####` form followed by any vendor-defined suffix.
    ///
    /// Per-connection rather than per-driver: it is the server's version, not
    /// the driver's, and a backend that gates capabilities on it already has
    /// the value (see [`crate::types::format_odbc_version`]).
    fn dbms_version(conn: &Self::Connection) -> Cow<'static, str>;

    /// The `SQL_CORRELATION_NAME` (74) support level — one of the
    /// [`SQL_CN_*`](crate::types::SQL_CN_ANY) values.
    ///
    /// Required because `0` is [`SQL_CN_NONE`](crate::types::SQL_CN_NONE),
    /// "correlation names are not supported". The spec also ties this to
    /// [`Backend::sql_conformance`]: "a SQL-92 Entry level-conformant driver
    /// will always return SQL_CN_ANY."
    fn correlation_name(conn: &Self::Connection) -> u16;

    /// The `SQL_NON_NULLABLE_COLUMNS` (75) answer to whether the data source
    /// supports `NOT NULL` — [`SQL_NNC_NULL`](crate::types::SQL_NNC_NULL) or
    /// [`SQL_NNC_NON_NULL`](crate::types::SQL_NNC_NON_NULL).
    ///
    /// Required because `0` is `SQL_NNC_NULL`, "all columns must be nullable".
    /// The spec ties this to [`Backend::sql_conformance`] too: "a SQL-92 Entry
    /// level-conformant driver will return SQL_NNC_NON_NULL."
    fn non_nullable_columns(conn: &Self::Connection) -> u16;

    /// Whether the data source supports expressions (not just column names) in
    /// an `ORDER BY` list — `SQL_EXPRESSIONS_IN_ORDERBY` (27), reported as
    /// `"Y"` or `"N"`.
    ///
    /// Required rather than defaulted because it is a capability an
    /// application acts on: a tool deciding whether to push `ORDER BY
    /// lower(name)` down to the data source reads this, and both a wrong `"N"`
    /// and the `""` a shape-derived default would produce read as "no".
    fn expressions_in_order_by(conn: &Self::Connection) -> bool;

    /// The `SQL_SQL_CONFORMANCE` (118) level — one of the
    /// [`SQL_SC_*`](crate::types::SQL_SC_SQL92_ENTRY) values.
    ///
    /// Required because core cannot know it, and hard-coding it made core
    /// contradict itself: it claimed `SQL_SC_SQL92_ENTRY` while separately
    /// supplying `SQL_GROUP_BY`, `SQL_CORRELATION_NAME` and
    /// `SQL_NON_NULLABLE_COLUMNS` values the spec says an entry-level driver
    /// never returns. Declaring a level here is a promise about those three
    /// hooks.
    fn sql_conformance(conn: &Self::Connection) -> u32;

    /// The `SQL_SUBQUERIES` (95) bitmask: which predicates accept a subquery,
    /// as an OR of the [`SQL_SQ_*`](crate::types::SQL_SQ_EXISTS) constants.
    ///
    /// Constrained by [`Backend::sql_conformance`]: "a SQL-92 Entry
    /// level-conformant driver will always return a bitmask with all of these
    /// bits set." Hard-coding that in core would tell a backend declaring no
    /// conformance level at all that it supports correlated subqueries — the
    /// claim a BI tool acts on when it decides to push one down.
    fn subqueries(conn: &Self::Connection) -> u32;

    /// Whether the data source supports column aliases (`SELECT x AS y`) —
    /// `SQL_COLUMN_ALIAS` (87), reported as `"Y"` or `"N"`.
    ///
    /// Constrained by [`Backend::sql_conformance`]: "a SQL-92 Entry
    /// level-conformant driver will always return 'Y'."
    fn column_alias(conn: &Self::Connection) -> bool;

    /// How the data source concatenates a NULL character column with a
    /// non-NULL one — `SQL_CONCAT_NULL_BEHAVIOR` (22), either
    /// [`SQL_CB_NULL`](crate::types::SQL_CB_NULL) or
    /// [`SQL_CB_NON_NULL`](crate::types::SQL_CB_NON_NULL).
    ///
    /// Required because `0` is `SQL_CB_NULL`, a substantive answer. Also
    /// constrained by [`Backend::sql_conformance`]: "a SQL-92 Entry
    /// level-conformant driver will always return SQL_CB_NULL."
    fn concat_null_behavior(conn: &Self::Connection) -> u16;

    /// The `SQL_UNION` (96) bitmask: which of `UNION` and `UNION ALL` the data
    /// source supports, as an OR of
    /// [`SQL_U_UNION`](crate::types::SQL_U_UNION) and
    /// [`SQL_U_UNION_ALL`](crate::types::SQL_U_UNION_ALL).
    fn union_support(conn: &Self::Connection) -> u32;

    /// The `SQL_CONVERT_FUNCTIONS` (48) bitmask: which of the ODBC conversion
    /// functions the driver supports, as an OR of
    /// [`SQL_FN_CVT_CAST`](crate::types::SQL_FN_CVT_CAST) and
    /// [`SQL_FN_CVT_CONVERT`](crate::types::SQL_FN_CVT_CONVERT).
    ///
    /// Note this is about `CAST` / `CONVERT` themselves. Which *type pairs*
    /// each can convert between is the separate `SQL_CONVERT_*` family, which
    /// a backend answers through [`Backend::get_info_raw`].
    fn convert_functions(conn: &Self::Connection) -> u32;

    /// Whether a column named in `ORDER BY` must also appear in the select
    /// list — `SQL_ORDER_BY_COLUMNS_IN_SELECT` (90), reported as `"Y"` or
    /// `"N"`.
    ///
    /// `false` is the *permissive* answer, so it is a claim rather than an
    /// absence of one: it tells an application it may order by a column it did
    /// not select.
    fn order_by_columns_in_select(conn: &Self::Connection) -> bool;

    /// Whether the connected user is guaranteed `SELECT` on **every** table
    /// `SQLTables` returns — `SQL_ACCESSIBLE_TABLES` (19).
    ///
    /// `true` is a guarantee core cannot make on a backend's behalf, and one
    /// that depends on the connected principal rather than the driver. Return
    /// `false` unless the data source genuinely filters its catalog by
    /// privilege.
    fn accessible_tables(conn: &Self::Connection) -> bool;

    /// Whether the data source is read-only — `SQL_DATA_SOURCE_READ_ONLY`
    /// (25), reported as `"Y"` or `"N"`.
    fn data_source_read_only(conn: &Self::Connection) -> bool;

    /// The `SQL_SEARCH_PATTERN_ESCAPE` (14) character: what escapes `%` and
    /// `_` in the pattern arguments of the catalog functions, so they match
    /// literally.
    ///
    /// Applies only to catalog-function patterns, not to the `LIKE` predicate
    /// (that is `SQL_LIKE_ESCAPE_CLAUSE`). Return `""` if the data source has
    /// no escape character — the spec's answer for that case, and one core
    /// cannot distinguish from a backend that simply never set it.
    ///
    /// Takes a connection because the escape character can genuinely differ by
    /// data source, so the answer is computed rather than a `'static` borrow —
    /// hence `Cow<'static, str>` rather than `&'static str`.
    fn search_pattern_escape(conn: &Self::Connection) -> Cow<'static, str>;

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
    ///
    /// Takes a connection because the keyword list can genuinely differ by
    /// data source, so the answer is computed rather than a `'static` borrow —
    /// hence `Cow<'static, [Cow<'static, str>]>` rather than
    /// `&'static [&'static str]`; the inner `Cow` lets each keyword itself be
    /// owned, not only the list.
    fn keywords(conn: &Self::Connection) -> Cow<'static, [Cow<'static, str>]>;

    /// The `SQL_TIMEDATE_ADD_INTERVALS` (109) bitmask: the interval units the
    /// `TIMESTAMPADD` scalar function accepts, as an OR of the
    /// [`SQL_FN_TSI_*`](crate::types::SQL_FN_TSI_SECOND) constants.
    ///
    /// Coupled to `SQL_TIMEDATE_FUNCTIONS`: a backend claiming
    /// `SQL_FN_TD_TIMESTAMPADD` there and `0` here would be saying the function
    /// exists but accepts no units. Required so that contradiction cannot be
    /// inherited silently. Return `0` only if `TIMESTAMPADD` is genuinely not
    /// supported.
    fn timedate_add_intervals(conn: &Self::Connection) -> u32;

    /// The `SQL_TIMEDATE_DIFF_INTERVALS` (110) bitmask: the interval units the
    /// `TIMESTAMPDIFF` scalar function accepts.
    ///
    /// Separate from [`Backend::timedate_add_intervals`] because a data source
    /// may accept different units for each; see that method for the coupling
    /// to `SQL_TIMEDATE_FUNCTIONS`.
    fn timedate_diff_intervals(conn: &Self::Connection) -> u32;

    /// The `SQL_DEFAULT_TXN_ISOLATION` (26) level this data source runs at
    /// when the application has not set one — a single
    /// [`SQL_TXN_*`](crate::types::SQL_TXN_SERIALIZABLE) constant, or `0` if
    /// the data source does not support transactions.
    ///
    /// This is also what `SQLGetConnectAttr(SQL_ATTR_TXN_ISOLATION)` reports
    /// on a connection where the application has not set the attribute. Both
    /// answers come from here so they cannot disagree. Answering the connection
    /// attribute from a constant instead would let a connection report one
    /// isolation level through `SQLGetConnectAttr` and another through
    /// `SQLGetInfo`, with nothing in either path to reconcile them.
    fn default_txn_isolation(conn: &Self::Connection) -> u32;

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
    fn txn_isolation_options(conn: &Self::Connection) -> u32;

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
    fn set_txn_isolation(conn: &Self::Connection, level: u32) -> Result<(), Self::Error> {
        if Self::txn_isolation_options(conn) == level {
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
    /// Called by `SQLFetch`. Returns [`FetchResult::Row`] if a row is available,
    /// [`FetchResult::NoData`] when the result set is exhausted.
    fn fetch(&mut self) -> Result<FetchResult, Self::Error> {
        Err(OdbcError::NotImplemented {
            feature: "fetch".into(),
        }
        .into())
    }

    /// Retrieves the value of column `col` (1-based) from the current row.
    ///
    /// Called by `SQLGetData`. The value is converted to `target_type` as requested by
    /// the application.
    ///
    /// Returns a [`Cow`] so that backends which cache rows in memory can hand
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
    /// Called by `SQLNumResultCols`. Returns 0 if no result set is active.
    ///
    /// `i16` because that is what the ABI is: `SQLNumResultCols` writes through
    /// a `SQLSMALLINT *`. Returning `u16` let a backend name a count the driver
    /// then could not report, and pushed a fallible narrowing into core for a
    /// value the backend already knew was out of range.
    fn column_count(&self) -> i16 {
        0
    }

    /// Returns metadata for column `col` (1-based).
    ///
    /// Called by `SQLDescribeColW`.
    ///
    /// Returns a [`Cow`] for the same reason
    /// [`StatementBackend::get_data`] does: a backend holding its column
    /// descriptors in memory — which most do, since the result set's shape is
    /// known once — can hand back a borrow instead of cloning a
    /// `ColumnDescriptor` and its two `String`s on every call. `SQLColAttribute`
    /// calls this once per column per attribute, so an application walking the
    /// metadata of a wide result set was paying for a clone each time.
    ///
    /// **Core range-checks `col` against [`StatementBackend::column_count`]
    /// before calling**, so an out-of-range column never reaches here and an
    /// error returned from this method is understood as a genuine failure. Its
    /// SQLSTATE is what the application sees — `08S01` for a link failure,
    /// `HY008` if the statement's cancel token reports signalled, `HY000`
    /// otherwise — so route it through the driver's central error-mapping
    /// function rather than hand-building an `OdbcError` here. Core used to
    /// overwrite every error from this method with `07009` "column number out
    /// of range", which is why this contract is worth stating.
    fn describe_col(
        &self,
        _col: u16,
    ) -> Result<std::borrow::Cow<'_, ColumnDescriptor>, Self::Error> {
        Err(OdbcError::NotImplemented {
            feature: "describe_col".into(),
        }
        .into())
    }

    /// Returns the number of rows affected by the last DML statement.
    ///
    /// Called by `SQLRowCount`. Returns `None` if not applicable (e.g. for SELECT
    /// statements or when no statement has been executed).
    ///
    /// `i64` because `SQLRowCount` writes through a `SQLLEN *`, which is signed
    /// and 64-bit on a 64-bit build. The signedness is load-bearing: the spec
    /// defines `SQL_NO_TOTAL` (`-1`) for "the driver cannot determine the count",
    /// which is a different answer from `None`'s "not applicable to this
    /// statement", and `usize` could express neither.
    fn row_count(&self) -> Option<i64> {
        None
    }

    /// Closes the open cursor, discarding any unread rows, and leaves the
    /// statement handle valid and re-executable.
    ///
    /// Three callers, all of which mean "this cursor is closing now":
    ///
    /// - `SQLCloseCursor`, the most obvious place an application closes one.
    /// - `SQLFreeStmt(SQL_CLOSE)`, which the spec makes equivalent to it bar
    ///   the `24000`. Both then discard the backend statement, so a failure
    ///   here is reported and the discard happens anyway — the application must
    ///   not be left holding a cursor it cannot clear.
    /// - `SQLEndTran`, for a backend declaring
    ///   [`crate::types::CursorBehavior::Close`] from
    ///   [`Backend::cursor_commit_behavior`] /
    ///   [`Backend::cursor_rollback_behavior`].
    ///
    /// **The statement is dropped after this returns** in the first two cases,
    /// so an implementation must be safe to follow with `Drop`. `SQLEndTran`
    /// is the exception and keeps it alive: footnote \[2\] of the transition
    /// table leaves a prepared-but-unexecuted statement unchanged under
    /// `SQL_CB_CLOSE`, which is why a backend declaring `Close` **must**
    /// override this — there, this method is the only thing that closes the
    /// cursor at all.
    ///
    /// Fallible because for a networked data source this is a round trip that
    /// can fail: it is where a driver tells the server to drop a partially-read
    /// result set. That is also why core calls it rather than relying on
    /// `Drop`, which cannot report anything and may run without the runtime an
    /// async-bridged driver needs. The SQLSTATE the application sees is
    /// whatever the driver's central error mapping produced.
    ///
    /// The default is `Ok(())`, which is correct only for a backend whose
    /// cursors need no teardown. See [`Backend::cursor_commit_behavior`].
    fn close_cursor(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// Default values for `InfoType` variants that are **identical** across all drivers.
///
/// Backends should call this at the end of their `get_info` match, before the `_ =>` arm,
/// to avoid duplicating these ~60 arms. Returns `None` for anything driver-specific.
///
/// Generic over the calling backend so that the answers can be derived from its
/// own declarations — call it as `default_get_info::<Self>(info_type)`.
///
/// The catalog column widths come from [`Backend::catalog_result_column_widths`]
/// rather than from a parameter. They are already a `Backend` declaration, and
/// taking them separately would let a caller hand over widths that disagree with
/// the ones the same backend reports everywhere else — which is exactly what
/// the `SQL_MAX_*_NAME_LEN` group is derived from.
pub fn default_get_info<B: Backend>(
    conn: Option<&B::Connection>,
    info_type: crate::types::InfoType,
) -> Option<InfoValue> {
    use crate::types::{
        InfoType, InfoValue, SQL_AM_NONE, SQL_ASYNC_NOTIFICATION_NOT_CAPABLE, SQL_CA1_NEXT,
        SQL_DRIVER_ODBC_VER_STRING, SQL_GD_ANY_COLUMN, SQL_GD_ANY_ORDER, SQL_GD_BOUND,
        SQL_MAX_CURSOR_NAME_LEN, SQL_OIC_CORE, SQL_PARC_NO_BATCH, SQL_PAS_NO_SELECT,
        SQL_SO_FORWARD_ONLY, SQL_UNSPECIFIED,
    };
    match info_type {
        // --- String types identical in all drivers ---
        InfoType::DriverOdbcVer => Some(InfoValue::String(SQL_DRIVER_ODBC_VER_STRING.into())),
        InfoType::SearchPatternEscape => {
            Some(InfoValue::String(B::search_pattern_escape(conn?).into()))
        }
        // Derived from the escape dialect, which already carries this fact and
        // is what the escape translator itself consults. Hard-coding `"` here
        // let a backend quote identifiers one way and tell the application
        // another. The spec's "if the data source does not support quoted
        // identifiers, a blank is returned" is the empty-dialect case.
        InfoType::IdentifierQuoteChar => Some(InfoValue::String(
            B::escape_dialect(conn?)
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
            if B::supports_catalogs(conn?) {
                "catalog"
            } else {
                ""
            }
            .into(),
        )),
        InfoType::SchemaTerm => Some(InfoValue::String(
            if B::supports_schemas(conn?) {
                "schema"
            } else {
                ""
            }
            .into(),
        )),
        InfoType::CatalogNameSeparator => Some(InfoValue::String(
            if B::supports_catalogs(conn?) { "." } else { "" }.into(),
        )),
        InfoType::CatalogName => Some(InfoValue::String(
            if B::supports_catalogs(conn?) {
                "Y"
            } else {
                "N"
            }
            .into(),
        )),
        // Only the spec-mandated zero is asserted here. Once catalogs or
        // schemas exist, the position of the catalog in a qualified name and
        // the statements catalogs/schemas may appear in are genuinely
        // per-data-source, so core falls through and lets the backend answer
        // rather than overstating a capability it cannot know.
        InfoType::CatalogLocation if !B::supports_catalogs(conn?) => Some(InfoValue::U16(0)),
        InfoType::CatalogUsage if !B::supports_catalogs(conn?) => Some(InfoValue::U32(0)),
        InfoType::SchemaUsage if !B::supports_schemas(conn?) => Some(InfoValue::U32(0)),
        // Each of these was core asserting an entry-level SQL-92 answer while
        // the conformance level itself is the backend's to declare, so a
        // backend claiming no level was still told it had all of them.
        InfoType::ColumnAlias => Some(InfoValue::String(
            if B::column_alias(conn?) { "Y" } else { "N" }.into(),
        )),
        InfoType::Subqueries => Some(InfoValue::U32(B::subqueries(conn?))),
        InfoType::ConcatNullBehavior => Some(InfoValue::U16(B::concat_null_behavior(conn?))),
        InfoType::OrderByColumnsInSelect => Some(InfoValue::String(
            if B::order_by_columns_in_select(conn?) {
                "Y"
            } else {
                "N"
            }
            .into(),
        )),
        InfoType::UnionStatement => Some(InfoValue::U32(B::union_support(conn?))),
        InfoType::DataSourceName => Some(InfoValue::String(String::new())),
        InfoType::ServerName => Some(InfoValue::String(String::new())),
        InfoType::UserName => Some(InfoValue::String(String::new())),
        InfoType::DataSourceReadOnly => Some(InfoValue::String(
            if B::data_source_read_only(conn?) {
                "Y"
            } else {
                "N"
            }
            .into(),
        )),
        InfoType::AccessibleTables => Some(InfoValue::String(
            if B::accessible_tables(conn?) {
                "Y"
            } else {
                "N"
            }
            .into(),
        )),
        InfoType::AccessibleProcedures => Some(InfoValue::String(
            if B::accessible_procedures(conn?) {
                "Y"
            } else {
                "N"
            }
            .into(),
        )),
        InfoType::Integrity => Some(InfoValue::String(
            if B::integrity(conn?) { "Y" } else { "N" }.into(),
        )),
        InfoType::SpecialCharacters => Some(InfoValue::String(B::special_characters(conn?).into())),
        // Identity. `SQL_DRIVER_NAME` and `SQL_DRIVER_VER` describe the driver
        // rather than the data source, so they answer without a connection —
        // which is what the Windows Driver Manager asks for before one exists.
        InfoType::DriverName => Some(InfoValue::String(B::driver_name().into())),
        InfoType::DriverVer => Some(InfoValue::String(B::driver_version().into())),
        InfoType::DbmsName => Some(InfoValue::String(B::dbms_name(conn?).into())),
        InfoType::DbmsVer => Some(InfoValue::String(B::dbms_version(conn?).into())),
        // Zero here is `SQL_TC_NONE`, "transactions not supported", which
        // contradicts any backend declaring an isolation level. See
        // `Backend::txn_capable`.
        InfoType::TransactionCapable => Some(InfoValue::U16(B::txn_capable(conn?))),
        InfoType::XopenCliYear => Some(InfoValue::String("1995".into())),
        InfoType::CollationSeq => Some(InfoValue::String(String::new())),
        InfoType::DescribeParameter => Some(InfoValue::String("Y".into())),
        // Spec-declared "Y"/"N" strings, which need an arm each: the
        // shape-aware fallback would give them `""` — the right shape, but not
        // a value in any of their value lists.
        InfoType::MultResultSets => Some(InfoValue::String("N".into())),
        InfoType::MaxRowSizeIncludesLong => Some(InfoValue::String("N".into())),
        InfoType::NeedLongDataLen => Some(InfoValue::String("N".into())),
        // A capability, not a shared default: `""` and a wrong "N" both read as
        // "no" to a tool deciding whether to push an expression into ORDER BY.
        InfoType::ExpressionsInOrderBy => Some(InfoValue::String(
            if B::expressions_in_order_by(conn?) {
                "Y"
            } else {
                "N"
            }
            .into(),
        )),
        // --- U16 types identical in all drivers ---
        // Enum-valued info types where zero is a substantive answer
        // (SQL_GB_NOT_SUPPORTED, SQL_NC_HIGH, SQL_CN_NONE, SQL_NNC_NULL), not
        // "unknown" — so the backend has to state them rather than inherit a
        // claim it never made. See the `Backend` docs for each.
        InfoType::GroupBy => Some(InfoValue::U16(B::group_by(conn?))),
        InfoType::NullCollation => Some(InfoValue::U16(B::null_collation(conn?))),
        InfoType::CorrelationName => Some(InfoValue::U16(B::correlation_name(conn?))),
        InfoType::NonNullableColumns => Some(InfoValue::U16(B::non_nullable_columns(conn?))),
        InfoType::MaxDriverConnections => Some(InfoValue::U16(0)),
        InfoType::MaxConcurrentActivities => Some(InfoValue::U16(0)),
        // Derived from the backend hook so the value reported here and the
        // behaviour `sql_end_tran` applies cannot disagree.
        InfoType::CursorCommitBehaviour => {
            Some(InfoValue::U16(B::cursor_commit_behavior().as_u16()))
        }
        InfoType::MaxColumnNameLen => Some(InfoValue::U16(
            B::catalog_result_column_widths().identifier_len,
        )),
        // Deliberately not `widths.identifier_len` — a cursor name is an
        // ODBC-level convention the application invents, not a data-source
        // identifier the backend's catalog stores. See
        // `SQL_MAX_CURSOR_NAME_LEN`'s doc comment for the full rationale.
        InfoType::MaxCursorNameLen => Some(InfoValue::U16(SQL_MAX_CURSOR_NAME_LEN)),
        InfoType::MaxSchemaNameLen => Some(InfoValue::U16(
            B::catalog_result_column_widths().identifier_len,
        )),
        InfoType::MaxCatalogNameLen => Some(InfoValue::U16(
            B::catalog_result_column_widths().identifier_len,
        )),
        InfoType::MaxTableNameLen => Some(InfoValue::U16(
            B::catalog_result_column_widths().identifier_len,
        )),
        InfoType::MaxColumnsInGroupBy => Some(InfoValue::U16(0)),
        InfoType::MaxColumnsInIndex => Some(InfoValue::U16(0)),
        InfoType::MaxColumnsInOrderBy => Some(InfoValue::U16(0)),
        InfoType::MaxColumnsInSelect => Some(InfoValue::U16(0)),
        InfoType::MaxColumnsInTable => Some(InfoValue::U16(0)),
        InfoType::MaxTablesInSelect => Some(InfoValue::U16(0)),
        InfoType::MaxUserNameLen => Some(InfoValue::U16(0)),
        InfoType::ActiveEnvironments => Some(InfoValue::U16(0)),
        // SQL_CURSOR_SENSITIVITY is `An SQLUINTEGER value` per the SQLGetInfo
        // spec, not SQLUSMALLINT — found by the info-type conformance test
        // (`stackable-odbc-core::conformance`), which enumerates every InfoType's
        // declared shape rather than relying on a hand-picked subset. `U16`
        // here would hand a numeric type expecting 4 bytes only 2, leaving
        // the upper 2 bytes as whatever the caller's buffer already held.
        // `SQL_UNSPECIFIED`, not `SQL_INSENSITIVE`. Insensitivity is a promise
        // that no other cursor's changes become visible, and core's fetch
        // streams rows from the backend as the application asks for them — it
        // makes no such promise about rows it has not read yet. The spec puts
        // the two on either side of a conformance line ("a SQL-92 Entry
        // level-conformant driver will always return the SQL_UNSPECIFIED
        // option as supported. A SQL-92 Full level-conformant driver will
        // always return the SQL_INSENSITIVE option"), but this describes core's
        // cursor rather than the backend's SQL grammar, so it does not follow
        // `Backend::sql_conformance`. `SQLGetStmtAttr(SQL_ATTR_CURSOR_
        // SENSITIVITY)` reports the same value.
        InfoType::CursorSensitivity => Some(InfoValue::U32(u32::from(SQL_UNSPECIFIED))),
        InfoType::MaxIdentifierLen => Some(InfoValue::U16(
            B::catalog_result_column_widths().identifier_len,
        )),
        // --- U32 types identical in all drivers ---
        InfoType::ScrollOptions => Some(InfoValue::U32(SQL_SO_FORWARD_ONLY)),
        InfoType::ConvertFunctions => Some(InfoValue::U32(B::convert_functions(conn?))),
        // Capability bitmaps, not limits: a `0` here is the claim "this data
        // source cannot do this at all", so the backend has to state it.
        InfoType::AlterTable => Some(InfoValue::U32(B::alter_table_support(conn?))),
        InfoType::OuterJoinCapabilities => Some(InfoValue::U32(B::outer_join_capabilities(conn?))),
        // Limits, where the spec defines 0 as "no specified limit or the limit
        // is unknown" — correct as a shared default, unlike the two above.
        InfoType::MaxIndexSize => Some(InfoValue::U32(0)),
        InfoType::MaxRowSize => Some(InfoValue::U32(0)),
        InfoType::MaxStatementLen => Some(InfoValue::U32(0)),
        // Derived from the backend hooks so that SQL_DEFAULT_TXN_ISOLATION and
        // SQLGetConnectAttr(SQL_ATTR_TXN_ISOLATION) cannot report two
        // different levels for the same connection.
        InfoType::DefaultTxnIsolation => Some(InfoValue::U32(B::default_txn_isolation(conn?))),
        InfoType::TransactionIsolationProtocol => {
            Some(InfoValue::U32(B::txn_isolation_options(conn?)))
        }
        // Core cannot know the conformance level, and hard-coding it made core
        // contradict its own SQL_GROUP_BY / SQL_CORRELATION_NAME /
        // SQL_NON_NULLABLE_COLUMNS answers.
        InfoType::SqlConformance => Some(InfoValue::U32(B::sql_conformance(conn?))),
        // The units TIMESTAMPADD / TIMESTAMPDIFF accept. Defaulting these to 0
        // while the backend claims SQL_FN_TD_TIMESTAMPADD in
        // SQL_TIMEDATE_FUNCTIONS is self-contradictory.
        InfoType::TimedateAddIntervals => Some(InfoValue::U32(B::timedate_add_intervals(conn?))),
        InfoType::TimedateDiffIntervals => Some(InfoValue::U32(B::timedate_diff_intervals(conn?))),
        InfoType::IdentifierCase => Some(InfoValue::U16(B::identifier_case(conn?))),
        InfoType::OdbcInterfaceConformance => Some(InfoValue::U32(SQL_OIC_CORE)),
        InfoType::AsyncMode => Some(InfoValue::U32(SQL_AM_NONE)),
        InfoType::AsyncDbcFunctions => Some(InfoValue::U32(0)),

        // --- Facts about core's own implementation ---
        //
        // `SQLGetData` here can read any column, in any order, bound or not:
        // `sql_get_data` checks neither column order nor binding state.
        // `SQL_GD_BLOCK` is deliberately absent — it describes reading a row
        // from a block cursor, and `sql_set_stmt_attr_w` substitutes 1 back for
        // any `SQL_ATTR_ROW_ARRAY_SIZE`, so no multi-row rowset can exist.
        InfoType::GetDataExtensions => Some(InfoValue::U32(
            SQL_GD_ANY_COLUMN | SQL_GD_ANY_ORDER | SQL_GD_BOUND,
        )),
        // `escape.rs` implements the `{escape}` sequence, and its translation
        // is what an application is asking about here.
        InfoType::LikeEscapeClause => Some(InfoValue::String("Y".into())),
        // Core executes one parameter set and one statement per execute:
        // `sql_set_stmt_attr_w` refuses any `SQL_ATTR_PARAMSET_SIZE` other
        // than 1, so no batch or parameter-array rowset can be produced.
        InfoType::BatchSupport => Some(InfoValue::U32(0)),
        InfoType::BatchRowCount => Some(InfoValue::U32(0)),
        InfoType::ParamArrayRowCounts => Some(InfoValue::U32(SQL_PARC_NO_BATCH)),
        InfoType::ParamArraySelects => Some(InfoValue::U32(SQL_PAS_NO_SELECT)),
        // The `Backend` trait is synchronous, so there is no asynchronous
        // execution to describe and nothing to notify about.
        InfoType::MaxAsyncConcurrentStatements => Some(InfoValue::U32(0)),
        InfoType::AsyncNotification => Some(InfoValue::U32(SQL_ASYNC_NOTIFICATION_NOT_CAPABLE)),
        // Core owns no connection pool.
        InfoType::DriverAwarePoolingSupported => Some(InfoValue::U32(0)),
        // Whether the data source supports outer joins at all is already stated
        // by `outer_join_capabilities`; deriving it here keeps the two from
        // contradicting each other.
        InfoType::OuterJoins => Some(InfoValue::String(
            if B::outer_join_capabilities(conn?) != 0 {
                "Y"
            } else {
                "N"
            }
            .into(),
        )),
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
fn data_source_specific_keywords<B: Backend>(conn: &B::Connection) -> String {
    let keywords = B::keywords(conn);
    let mut names: Vec<&str> = keywords
        .iter()
        .map(Cow::as_ref)
        .filter(|name| {
            !crate::types::ODBC_RESERVED_KEYWORDS
                .iter()
                .any(|reserved| reserved.eq_ignore_ascii_case(name))
        })
        .collect();
    names.sort_unstable();
    names.join(",")
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
/// derived from [`Backend::cursor_rollback_behavior`] — call it as
/// `common_get_info_raw::<Self>(info_type)`.
pub fn common_get_info_raw<B: Backend>(
    conn: Option<&B::Connection>,
    info_type: u16,
) -> Option<InfoValue> {
    use crate::types::{
        InfoValue, SQL_CURSOR_ROLLBACK_BEHAVIOR, SQL_FILE_USAGE, SQL_KEYWORDS,
        SQL_MULTIPLE_ACTIVE_TXN, SQL_PROCEDURE_TERM, SQL_PROCEDURES, SQL_QUOTED_IDENTIFIER_CASE,
        SQL_ROW_UPDATES, SQL_TABLE_TERM,
    };
    match info_type {
        SQL_FILE_USAGE => Some(InfoValue::U16(0)),
        // See the matching arm in `default_get_info`.
        SQL_CURSOR_ROLLBACK_BEHAVIOR => {
            Some(InfoValue::U16(B::cursor_rollback_behavior().as_u16()))
        }
        // The quoted counterpart of `SQL_IDENTIFIER_CASE`, and like it a
        // four-valued statement about the system catalog with no legal zero.
        SQL_QUOTED_IDENTIFIER_CASE => Some(InfoValue::U16(B::quoted_identifier_case(conn?))),
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
        // Whether two transactions can be live at once is a property of the
        // driver's connections, which only the backend knows.
        SQL_MULTIPLE_ACTIVE_TXN => Some(InfoValue::String(
            if B::multiple_active_txn(conn?) {
                "Y"
            } else {
                "N"
            }
            .into(),
        )),
        // The data source's own reserved words, minus everything ODBC already
        // reserves — the subtraction the spec defines for this info type,
        // applied once here rather than in each backend. The list itself is a
        // capability, so it comes from `Backend::keywords`; core only owns the
        // rule.
        SQL_KEYWORDS => Some(InfoValue::String(data_source_specific_keywords::<B>(conn?))),
        // `SQL_DATABASE_NAME` is deliberately absent: the spec equates it with
        // `SQLGetConnectAttr(SQL_ATTR_CURRENT_CATALOG)`, which lives on the
        // connection *handle* rather than on `B::Connection`, so `sql_get_info_w`
        // answers it from there. Answering `""` here as well would give the
        // same info type two sources and let them disagree.
        //
        // Empty is a valid value for `SQL_PROCEDURE_TERM`: given
        // `SQL_PROCEDURES` above answers "N", there are no procedures to have a
        // vendor term for. Every data source has tables, so `SQL_TABLE_TERM`
        // gets the generic term rather than "".
        SQL_PROCEDURE_TERM => Some(InfoValue::String(String::new())),
        SQL_TABLE_TERM => Some(InfoValue::String("table".into())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{
        MockBackend, MockConnection, MockNoCatalogBackend, MockTxnConnection,
        MockTxnDeleteCloseBackend,
    };
    use crate::types::{
        DEFAULT_IDENTIFIER_LEN, InfoType, InfoValue, SQL_AM_NONE, SQL_AT_ADD_COLUMN_SINGLE,
        SQL_AT_DROP_COLUMN_RESTRICT, SQL_CA1_NEXT, SQL_CB_PRESERVE, SQL_CN_ANY,
        SQL_DRIVER_ODBC_VER_STRING, SQL_FN_CVT_CAST, SQL_FN_TSI_DAY, SQL_FN_TSI_SECOND,
        SQL_FN_TSI_YEAR, SQL_GB_GROUP_BY_EQUALS_SELECT, SQL_MAX_CURSOR_NAME_LEN, SQL_NC_END,
        SQL_NNC_NON_NULL, SQL_OIC_CORE, SQL_OJ_LEFT, SQL_OJ_NESTED, SQL_SC_SQL92_ENTRY,
        SQL_SO_FORWARD_ONLY, SQL_SQ_COMPARISON, SQL_SQ_CORRELATED_SUBQUERIES, SQL_SQ_EXISTS,
        SQL_SQ_IN, SQL_SQ_QUANTIFIED, SQL_TC_ALL, SQL_TXN_SERIALIZABLE, SQL_U_UNION,
        SQL_U_UNION_ALL, SQL_UNSPECIFIED,
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
        (InfoType::SearchPatternEscape,           Expected::Str("\\")),
        (InfoType::IdentifierQuoteChar,           Expected::Str("\"")),
        (InfoType::CatalogTerm,                   Expected::Str("catalog")),
        (InfoType::SchemaTerm,                    Expected::Str("schema")),
        (InfoType::CatalogNameSeparator,          Expected::Str(".")),
        (InfoType::ColumnAlias,                   Expected::Str("Y")),
        (InfoType::OrderByColumnsInSelect,        Expected::Str("N")),
        (InfoType::DataSourceName,                Expected::Str("")),
        (InfoType::ServerName,                    Expected::Str("")),
        (InfoType::UserName,                      Expected::Str("")),
        (InfoType::DataSourceReadOnly,            Expected::Str("N")),
        // "Y" guarantees the user has SELECT on every table SQLTables returns.
        // Core cannot make that promise for a backend, and it depends on the
        // connected principal, so the mock declares the honest "N".
        (InfoType::AccessibleTables,              Expected::Str("N")),
        // Backend-stated capabilities; MockBackend declares each of these.
        (InfoType::AccessibleProcedures,          Expected::Str("Y")),
        (InfoType::Integrity,                     Expected::Str("Y")),
        (InfoType::SpecialCharacters,             Expected::Str("$#")),
        // Identity. The two driver-level ones answer without a connection,
        // which is what the Windows Driver Manager asks for before one exists.
        (InfoType::DriverName,                    Expected::Str("Mock ODBC Driver")),
        (InfoType::DriverVer,                     Expected::Str("01.00.0000")),
        (InfoType::DbmsName,                      Expected::Str("MockDB")),
        (InfoType::DbmsVer,                       Expected::Str("01.02.0003")),
        (InfoType::XopenCliYear,                  Expected::Str("1995")),
        (InfoType::CollationSeq,                  Expected::Str("")),
        (InfoType::DescribeParameter,             Expected::Str("Y")),
        // Y/N strings, which need arms of their own: the shape default's "" is
        // not in any of their value lists.
        (InfoType::MultResultSets,                Expected::Str("N")),
        (InfoType::MaxRowSizeIncludesLong,        Expected::Str("N")),
        (InfoType::NeedLongDataLen,               Expected::Str("N")),
        // Backend-stated capability; MockBackend declares true.
        (InfoType::ExpressionsInOrderBy,          Expected::Str("Y")),
        // --- U16 values ---
        // Enum values where 0 is a real answer, so they come from the backend.
        // Non-`SQL_TC_NONE`, as MockBackend declares an isolation level.
        (InfoType::TransactionCapable,            Expected::U16(SQL_TC_ALL as u16)),
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
        // CursorSensitivity is SQLUINTEGER per spec, not SQLUSMALLINT — see
        // the matching comment on its arm in `default_get_info`.
        (InfoType::CursorSensitivity,             Expected::U32(SQL_UNSPECIFIED as u32)),
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
            let actual = default_get_info::<MockBackend>(Some(&MockConnection), *info_type)
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
                Some(&MockTxnConnection {
                    end_tran_fails: false
                }),
                InfoType::CursorCommitBehaviour
            ),
            Some(InfoValue::U16(SQL_CB_DELETE)),
            "SQL_CURSOR_COMMIT_BEHAVIOR ignored Backend::cursor_commit_behavior"
        );
        assert_eq!(
            common_get_info_raw::<MockTxnDeleteCloseBackend>(
                Some(&MockTxnConnection {
                    end_tran_fails: false
                }),
                SQL_CURSOR_ROLLBACK_BEHAVIOR
            ),
            Some(InfoValue::U16(SQL_CB_CLOSE)),
            "SQL_CURSOR_ROLLBACK_BEHAVIOR ignored Backend::cursor_rollback_behavior"
        );
    }

    #[test]
    fn advertised_cursor_behavior_defaults_to_preserve() {
        use crate::test_utils::MockBackend;
        use crate::types::{SQL_CB_PRESERVE, SQL_CURSOR_ROLLBACK_BEHAVIOR};

        assert_eq!(
            default_get_info::<MockBackend>(Some(&MockConnection), InfoType::CursorCommitBehaviour),
            Some(InfoValue::U16(SQL_CB_PRESERVE))
        );
        assert_eq!(
            common_get_info_raw::<MockBackend>(Some(&MockConnection), SQL_CURSOR_ROLLBACK_BEHAVIOR),
            Some(InfoValue::U16(SQL_CB_PRESERVE))
        );
    }

    /// `SQL_CATALOG_TERM`, `SQL_CATALOG_NAME_SEPARATOR`, `SQL_CATALOG_NAME`,
    /// `SQL_CATALOG_LOCATION` and `SQL_CATALOG_USAGE` are all defined by the
    /// `SQLGetInfo` spec in terms of one fact — whether the data source has
    /// catalogs at all — so a backend that says it has none must not be handed
    /// a name for them. Answering "catalog" and "." unconditionally would let a
    /// driver report `SQL_CATALOG_NAME = "N"` and name its catalogs in the same
    /// breath.
    #[test]
    fn catalog_less_backend_reports_the_spec_mandated_empty_catalog_group() {
        use crate::test_utils::MockNoCatalogBackend;
        assert_eq!(
            default_get_info::<MockNoCatalogBackend>(Some(&MockConnection), InfoType::CatalogTerm),
            Some(InfoValue::String(String::new())),
            "SQL_CATALOG_TERM must be empty when the data source has no catalogs"
        );
        assert_eq!(
            default_get_info::<MockNoCatalogBackend>(
                Some(&MockConnection),
                InfoType::CatalogNameSeparator
            ),
            Some(InfoValue::String(String::new())),
            "SQL_CATALOG_NAME_SEPARATOR must be empty when the data source has no catalogs"
        );
        assert_eq!(
            default_get_info::<MockNoCatalogBackend>(Some(&MockConnection), InfoType::CatalogName),
            Some(InfoValue::String("N".into())),
            "SQL_CATALOG_NAME must be \"N\" when the data source has no catalogs"
        );
        assert_eq!(
            default_get_info::<MockNoCatalogBackend>(
                Some(&MockConnection),
                InfoType::CatalogLocation
            ),
            Some(InfoValue::U16(0)),
            "SQL_CATALOG_LOCATION must be 0 when the data source has no catalogs"
        );
        assert_eq!(
            default_get_info::<MockNoCatalogBackend>(Some(&MockConnection), InfoType::CatalogUsage),
            Some(InfoValue::U32(0)),
            "SQL_CATALOG_USAGE must be 0 when the data source has no catalogs"
        );
    }

    /// The schema half of the same rule: `SQL_SCHEMA_TERM` and
    /// `SQL_SCHEMA_USAGE` are defined in terms of whether schemas exist.
    #[test]
    fn schema_less_backend_reports_the_spec_mandated_empty_schema_group() {
        use crate::test_utils::MockNoCatalogBackend;
        assert_eq!(
            default_get_info::<MockNoCatalogBackend>(Some(&MockConnection), InfoType::SchemaTerm),
            Some(InfoValue::String(String::new())),
            "SQL_SCHEMA_TERM must be empty when the data source has no schemas"
        );
        assert_eq!(
            default_get_info::<MockNoCatalogBackend>(Some(&MockConnection), InfoType::SchemaUsage),
            Some(InfoValue::U32(0)),
            "SQL_SCHEMA_USAGE must be 0 when the data source has no schemas"
        );
    }

    /// A backend that *does* have catalogs and schemas gets the SQL-92 Full
    /// level terms the spec names. The sibling test above pins the empty group
    /// for a backend without them; this one is its other half, so that
    /// suppressing the group for one backend cannot blank it for both.
    #[test]
    fn catalog_supporting_backend_keeps_the_sql92_full_terms() {
        for (info_type, expected) in [
            (InfoType::CatalogTerm, "catalog"),
            (InfoType::SchemaTerm, "schema"),
            (InfoType::CatalogNameSeparator, "."),
            (InfoType::CatalogName, "Y"),
        ] {
            assert_eq!(
                default_get_info::<MockBackend>(Some(&MockConnection), info_type),
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
        for info_type in [
            InfoType::CatalogLocation,
            InfoType::CatalogUsage,
            InfoType::SchemaUsage,
        ] {
            assert_eq!(
                default_get_info::<MockBackend>(Some(&MockConnection), info_type),
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
        assert_eq!(
            default_get_info::<MockBackend>(Some(&MockConnection), InfoType::AlterTable),
            Some(InfoValue::U32(MockBackend::alter_table_support(
                &MockConnection
            ))),
            "SQL_ALTER_TABLE ignored Backend::alter_table_support"
        );
        assert_eq!(
            default_get_info::<MockBackend>(Some(&MockConnection), InfoType::OuterJoinCapabilities),
            Some(InfoValue::U32(MockBackend::outer_join_capabilities(
                &MockConnection
            ))),
            "SQL_OJ_CAPABILITIES ignored Backend::outer_join_capabilities"
        );
        // A non-zero declaration is what proves the value is read rather than
        // hard-coded: the previous implementation returned 0 for both.
        assert_ne!(MockBackend::alter_table_support(&MockConnection), 0);
        assert_ne!(MockBackend::outer_join_capabilities(&MockConnection), 0);
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
            let value = common_get_info_raw::<MockBackend>(Some(&MockConnection), info_type)
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
        for info_type in [
            InfoType::MultResultSets,
            InfoType::MaxRowSizeIncludesLong,
            InfoType::NeedLongDataLen,
        ] {
            assert_eq!(
                default_get_info::<MockBackend>(Some(&MockConnection), info_type),
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
        for (info_type, actual) in [
            (
                InfoType::NullCollation,
                MockBackend::null_collation(&MockConnection),
            ),
            (
                InfoType::CorrelationName,
                MockBackend::correlation_name(&MockConnection),
            ),
            (
                InfoType::NonNullableColumns,
                MockBackend::non_nullable_columns(&MockConnection),
            ),
            (InfoType::GroupBy, MockBackend::group_by(&MockConnection)),
        ] {
            assert_eq!(
                default_get_info::<MockBackend>(Some(&MockConnection), info_type),
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
        assert_eq!(
            default_get_info::<MockBackend>(Some(&MockConnection), InfoType::SqlConformance),
            Some(InfoValue::U32(SQL_SC_SQL92_ENTRY)),
            "SQL_SQL_CONFORMANCE ignored Backend::sql_conformance"
        );
        assert_eq!(
            default_get_info::<MockNoCatalogBackend>(
                Some(&MockConnection),
                InfoType::SqlConformance
            ),
            Some(InfoValue::U32(0)),
            "SQL_SQL_CONFORMANCE is still pinned to SQL_SC_SQL92_ENTRY"
        );
        assert_ne!(
            MockBackend::sql_conformance(&MockConnection),
            MockNoCatalogBackend::sql_conformance(&MockConnection),
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
        assert_eq!(
            default_get_info::<MockBackend>(Some(&MockConnection), InfoType::SqlConformance),
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
                default_get_info::<MockBackend>(Some(&MockConnection), info_type),
                Some(InfoValue::U16(expected)),
                "an entry-level-conformant driver {spec} for {info_type:?}"
            );
        }
    }

    /// `SQL_EXPRESSIONS_IN_ORDERBY` is a capability, and `""` reads as "no" to
    /// a tool deciding whether to push an expression into `ORDER BY`.
    #[test]
    fn expressions_in_order_by_comes_from_the_backend() {
        assert_eq!(
            default_get_info::<MockBackend>(Some(&MockConnection), InfoType::ExpressionsInOrderBy),
            Some(InfoValue::String("Y".into())),
            "SQL_EXPRESSIONS_IN_ORDERBY ignored Backend::expressions_in_order_by"
        );
        assert_eq!(
            default_get_info::<MockNoCatalogBackend>(
                Some(&MockConnection),
                InfoType::ExpressionsInOrderBy
            ),
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
        assert_eq!(
            default_get_info::<MockBackend>(Some(&MockConnection), InfoType::TimedateAddIntervals),
            Some(InfoValue::U32(MockBackend::timedate_add_intervals(
                &MockConnection
            ))),
            "SQL_TIMEDATE_ADD_INTERVALS ignored Backend::timedate_add_intervals"
        );
        assert_eq!(
            default_get_info::<MockBackend>(Some(&MockConnection), InfoType::TimedateDiffIntervals),
            Some(InfoValue::U32(MockBackend::timedate_diff_intervals(
                &MockConnection
            ))),
            "SQL_TIMEDATE_DIFF_INTERVALS ignored Backend::timedate_diff_intervals"
        );
        assert_ne!(
            MockBackend::timedate_add_intervals(&MockConnection),
            MockBackend::timedate_diff_intervals(&MockConnection),
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
        assert_eq!(
            default_get_info::<MockBackend>(Some(&MockConnection), InfoType::DefaultTxnIsolation),
            Some(InfoValue::U32(SQL_TXN_SERIALIZABLE)),
            "SQL_DEFAULT_TXN_ISOLATION ignored Backend::default_txn_isolation"
        );
        assert_eq!(
            default_get_info::<MockBackend>(
                Some(&MockConnection),
                InfoType::TransactionIsolationProtocol
            ),
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
        (
            InfoType::GetDataExtensions,
            "sql_get_data checks neither column order nor binding state, and no block cursor can exist",
        ),
        (
            InfoType::LikeEscapeClause,
            "escape.rs implements the {escape} sequence core is being asked about",
        ),
        (
            InfoType::BatchSupport,
            "core executes one statement per execute",
        ),
        (
            InfoType::BatchRowCount,
            "core executes one statement per execute",
        ),
        (
            InfoType::ParamArrayRowCounts,
            "sql_set_stmt_attr_w refuses any SQL_ATTR_PARAMSET_SIZE but 1",
        ),
        (
            InfoType::ParamArraySelects,
            "sql_set_stmt_attr_w refuses any SQL_ATTR_PARAMSET_SIZE but 1",
        ),
        (
            InfoType::MaxAsyncConcurrentStatements,
            "the Backend trait is synchronous",
        ),
        (
            InfoType::AsyncNotification,
            "the Backend trait is synchronous",
        ),
        (
            InfoType::DriverAwarePoolingSupported,
            "core owns no connection pool",
        ),
        (InfoType::ScrollOptions, "core's fetch is forward-only"),
        (
            InfoType::CursorSensitivity,
            "core's fetch streams rows and promises nothing about another cursor's changes",
        ),
        (
            InfoType::ForwardOnlyCursorAttributes1,
            "the fetch operations core implements",
        ),
        (
            InfoType::ForwardOnlyCursorAttributes2,
            "core implements none of these",
        ),
        (
            InfoType::DynamicCursorAttributes1,
            "core has no dynamic cursor",
        ),
        (
            InfoType::DynamicCursorAttributes2,
            "core has no dynamic cursor",
        ),
        (
            InfoType::KeysetCursorAttributes1,
            "core has no keyset cursor",
        ),
        (
            InfoType::KeysetCursorAttributes2,
            "core has no keyset cursor",
        ),
        (
            InfoType::StaticCursorAttributes1,
            "core has no static cursor",
        ),
        (
            InfoType::StaticCursorAttributes2,
            "core has no static cursor",
        ),
        (InfoType::AsyncMode, "the Backend trait is synchronous"),
        (
            InfoType::AsyncDbcFunctions,
            "the Backend trait is synchronous",
        ),
        (
            InfoType::MultResultSets,
            "sql_more_results always returns SQL_NO_DATA",
        ),
        (
            InfoType::NeedLongDataLen,
            "core's data-at-execution path never needs the length up front",
        ),
        (
            InfoType::OdbcInterfaceConformance,
            "describes the FFI surface core exports",
        ),
        (
            InfoType::DriverOdbcVer,
            "describes the FFI surface core exports",
        ),
        (
            InfoType::XopenCliYear,
            "driver-level identity, not a data-source property",
        ),
        (
            InfoType::MaxCursorNameLen,
            "a cursor name is an ODBC-level convention core owns",
        ),
        (
            InfoType::DescribeParameter,
            "core's SQLDescribeParam always answers, generically",
        ),
        (
            InfoType::MaxRowSizeIncludesLong,
            "follows from SQL_MAX_ROW_SIZE being 'unknown'",
        ),
        // --- Limits: the spec defines 0 as "no limit or unknown" ---
        (InfoType::MaxDriverConnections, "0 = no limit"),
        (InfoType::MaxConcurrentActivities, "0 = no limit"),
        (InfoType::ActiveEnvironments, "0 = no limit"),
        (InfoType::MaxColumnsInGroupBy, "0 = no limit or unknown"),
        (InfoType::MaxColumnsInIndex, "0 = no limit or unknown"),
        (InfoType::MaxColumnsInOrderBy, "0 = no limit or unknown"),
        (InfoType::MaxColumnsInSelect, "0 = no limit or unknown"),
        (InfoType::MaxColumnsInTable, "0 = no limit or unknown"),
        (InfoType::MaxTablesInSelect, "0 = no limit or unknown"),
        (InfoType::MaxUserNameLen, "0 = no limit or unknown"),
        (InfoType::MaxIndexSize, "0 = no limit or unknown"),
        (InfoType::MaxRowSize, "0 = no limit or unknown"),
        (InfoType::MaxStatementLen, "0 = no limit or unknown"),
        // --- No per-backend answer to give ---
        (
            InfoType::DataSourceName,
            "the DM supplies the DSN; core has none",
        ),
        (
            InfoType::ServerName,
            "carried in the connection string, not known here",
        ),
        (
            InfoType::UserName,
            "carried in the connection string, not known here",
        ),
        (InfoType::CollationSeq, "unknown, and the spec allows empty"),
    ];

    /// Info types with no arm in [`default_get_info`], where reaching the
    /// shape-aware default in `info_type_default_response` is the intended
    /// outcome rather than an oversight.
    ///
    /// Two kinds qualify, and neither is "core decided the data source's
    /// answer":
    ///
    /// - **Driver identity.** Core has no name or version to give; only the
    ///   driver does, and the Windows DM path documented in AGENTS.md requires
    ///   it to supply them through `get_info_pre_connect`.
    /// - **Capability bitmaps a backend must claim for itself.** `0` reads as
    ///   "supports none of these", which is the only honest answer core can
    ///   give for a backend that has said nothing — overstating here is what
    ///   makes a BI tool push down SQL the data source then rejects. Several
    ///   are also genuinely *per-connection* (a server version can gate them),
    ///   which a static capability method could not express; those belong in
    ///   `Backend::get_info`, which runs first and takes a connection.
    #[rustfmt::skip]
    const SHAPE_DEFAULT_IS_THE_ANSWER: &[(InfoType, &str)] = &[
        // --- Driver and data source identity ---

        // --- Claims about the data source, understated rather than invented ---
        (InfoType::SqlFileUsage,               "whether the driver is single-tier and file-based is a driver fact"),
        (InfoType::SqlQuotedIdentifierCase,    "answered by common_get_info_raw for backends that delegate to it"),

        // --- Scalar-function bitmaps: claiming one core cannot honour makes a
        //     BI tool emit a function the data source rejects ---
        (InfoType::NumericFunctions,           "the backend owns its scalar function set"),
        (InfoType::StringFunctions,            "the backend owns its scalar function set"),
        (InfoType::SystemFunctions,            "the backend owns its scalar function set"),
        (InfoType::TimedateFunctions,          "the backend owns its scalar function set"),
        (InfoType::AggregateFunctions,         "the backend owns its aggregate function set"),

        // --- SQL-92 grammar bitmaps. Several are per-connection in practice
        //     (Trino gates MATCH/UNIQUE on the coordinator version), so they
        //     belong in Backend::get_info, which takes a connection ---
        (InfoType::Sql92Predicates,            "per-connection in at least one driver; belongs in get_info"),
        (InfoType::Sql92RelationalJoinOperators, "per-connection in at least one driver; belongs in get_info"),
        (InfoType::Sql92DatetimeFunctions,     "the backend owns its SQL-92 grammar support"),
        (InfoType::Sql92NumericValueFunctions, "the backend owns its SQL-92 grammar support"),
        (InfoType::Sql92StringFunctions,       "the backend owns its SQL-92 grammar support"),
        (InfoType::Sql92ValueExpressions,      "the backend owns its SQL-92 grammar support"),
        (InfoType::Sql92RowValueConstructor,   "the backend owns its SQL-92 grammar support"),
        (InfoType::Sql92ForeignKeyDeleteRule,  "referential actions are a data source fact"),
        (InfoType::Sql92ForeignKeyUpdateRule,  "referential actions are a data source fact"),
        (InfoType::Sql92Grant,                 "the backend owns its privilege model"),
        (InfoType::Sql92Revoke,                "the backend owns its privilege model"),
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
        let mut unanswered = Vec::new();

        for info_type in crate::conformance::all_info_types() {
            let mine = default_get_info::<MockBackend>(Some(&MockConnection), info_type);
            let theirs = default_get_info::<MockAltBackend>(Some(&MockConnection), info_type);
            let declared = CORE_FACTS.iter().find(|(t, _)| *t == info_type);

            match (mine.is_some() && mine == theirs, declared) {
                // Core decides it, and said why. Fine.
                (true, Some(_)) => {}
                // Core decides it, and did not say why.
                (true, None) => undeclared.push(info_type),
                // Backend-derived (or unanswered) but still listed as a core
                // fact — the entry outlived the hard-coded value it described.
                (false, Some(_)) => stale.push(info_type),
                (false, None) => {
                    // Neither backend answers it at all. `default_get_info`
                    // returns `None`, so `sql_get_info_w` falls through to the
                    // shape-aware default — `0` or `""` — which for many info
                    // types is a substantive claim about the data source that
                    // core is in no position to make. Only the comparison above
                    // sees arms that *exist*, so without this a type with no arm
                    // bypasses the whole "capability must be declared" design.
                    if mine.is_none() && theirs.is_none() {
                        unanswered.push(info_type);
                    }
                }
            }
        }

        assert!(
            undeclared.is_empty(),
            "these info types answer the same for two backends with nothing in \
             common, so core is deciding them. Either derive each from a \
             `Backend` method, or add it to CORE_FACTS with the reason core is \
             entitled to decide it: {undeclared:?}"
        );
        let undeclared_gap: Vec<_> = unanswered
            .iter()
            .filter(|t| !SHAPE_DEFAULT_IS_THE_ANSWER.iter().any(|(d, _)| d == *t))
            .collect();
        assert!(
            undeclared_gap.is_empty(),
            "these info types have no arm at all, so they reach the shape-aware \
             default (`0` / `\"\"`) with nothing naming that as the intended \
             answer. Either give each an arm — derived from a `Backend` method \
             if it is a claim about the data source — or list it in \
             SHAPE_DEFAULT_IS_THE_ANSWER with the reason the default is \
             correct: {undeclared_gap:?}"
        );
        let stale_gap: Vec<_> = SHAPE_DEFAULT_IS_THE_ANSWER
            .iter()
            .map(|(t, _)| *t)
            .filter(|t| !unanswered.contains(t))
            .collect();
        assert!(
            stale_gap.is_empty(),
            "these are listed in SHAPE_DEFAULT_IS_THE_ANSWER but have an arm; \
             drop the stale entries: {stale_gap:?}"
        );
        assert!(
            stale.is_empty(),
            "these are listed in CORE_FACTS but are not answered identically \
             for every backend; drop the stale entries: {stale:?}"
        );
    }

    /// Info types [`common_get_info_raw`] answers identically for **every**
    /// backend, each with the reason core is entitled to decide it. The
    /// raw-path sibling of [`CORE_FACTS`], and held to the same standard.
    #[rustfmt::skip]
    const RAW_PATH_CORE_FACTS: &[(u16, &str)] = &[
        (
            crate::types::SQL_ROW_UPDATES,
            "describes a keyset-driven or mixed cursor's row-version detection, and core drives neither",
        ),
        (
            crate::types::SQL_PROCEDURES,
            "the spec's conjunction requires the driver to support ODBC procedure-invocation syntax, and escape.rs rejects {call} with HYC00",
        ),
        (
            crate::types::SQL_PROCEDURE_TERM,
            "the spec mandates the empty string while SQL_PROCEDURES is \"N\"",
        ),
        (
            crate::types::SQL_TABLE_TERM,
            "ODBC's generic word for the one object every data source has; a vendor that calls it something else overrides the info type in get_info",
        ),
        (
            crate::types::SQL_FILE_USAGE,
            "core builds a driver that is not single-tier and file-based",
        ),
    ];

    /// Info types [`common_get_info_raw`] answers identically for every backend
    /// where that answer is a claim about the *data source*, which core is not
    /// in a position to make.
    ///
    /// Every entry is outstanding work, not a justification: each needs either
    /// a `Backend` method to state it or a derivation from one that already
    /// exists. The list exists so the count can only go down — a new hard-coded
    /// data-source claim on this path fails the test rather than joining them
    /// silently.
    #[rustfmt::skip]
    const RAW_PATH_GAPS: &[(u16, &str)] = &[
    ];

    /// The raw-path sibling of
    /// [`default_get_info_answers_are_backend_derived_or_declared_core_facts`],
    /// asking the same question of the same two mocks: **does the answer move
    /// when the backend does?**
    ///
    /// `common_get_info_raw` is the only place several info types are ever
    /// answered — `SQL_CURSOR_ROLLBACK_BEHAVIOR` has no `odbc_sys::InfoType`
    /// variant at all, and `SQL_TABLE_TERM`, `SQL_PROCEDURES` and their
    /// neighbours have one but no arm in `default_get_info`. Without this test
    /// that path is unpoliced, and a hard-coded claim about the data source can
    /// live there indefinitely.
    ///
    /// The whole `u16` range is scanned rather than a hand-picked set: what is
    /// answered here is exactly what the function's `match` decides, and a list
    /// maintained beside it would be a second statement of the same fact.
    ///
    /// That scan is 131 072 evaluations, which costs nothing on stable (the
    /// whole suite runs in 0.2s) and 43.7s under Miri — a quarter of the job on
    /// its own. `common_get_info_raw` contains no `unsafe`, so Miri has nothing
    /// to check here and pays that purely to re-run a classification stable
    /// already made. Same reasoning as
    /// `escape::tests::pathological_nesting_returns_an_error_rather_than_killing_the_process`.
    #[cfg_attr(
        miri,
        ignore = "131k-evaluation scan; the function under test has no unsafe for Miri to check"
    )]
    #[test]
    fn common_get_info_raw_answers_are_backend_derived_or_declared() {
        use crate::test_utils::MockAltBackend;

        let mut undeclared = Vec::new();
        let mut stale = Vec::new();

        for raw in 0..=u16::MAX {
            let mine = common_get_info_raw::<MockBackend>(Some(&MockConnection), raw);
            let theirs = common_get_info_raw::<MockAltBackend>(Some(&MockConnection), raw);
            let declared = RAW_PATH_CORE_FACTS.iter().any(|(t, _)| *t == raw)
                || RAW_PATH_GAPS.iter().any(|(t, _)| *t == raw);

            match (mine.is_some() && mine == theirs, declared) {
                // Core decides it, and said why — or said it is a known gap.
                (true, true) => {}
                // Core decides it, and neither is recorded.
                (true, false) => undeclared.push(raw),
                // Backend-derived (or unanswered) but still listed — the entry
                // outlived the hard-coded value it described.
                (false, true) => stale.push(raw),
                (false, false) => {}
            }
        }

        assert!(
            undeclared.is_empty(),
            "these raw info types answer the same for two backends with nothing \
             in common, so core is deciding them. Either derive each from a \
             `Backend` method, or record it in RAW_PATH_CORE_FACTS with the \
             reason core is entitled to decide it — or in RAW_PATH_GAPS if it is \
             a claim about the data source that still needs a hook: {undeclared:?}"
        );
        assert!(
            stale.is_empty(),
            "these are recorded in RAW_PATH_CORE_FACTS or RAW_PATH_GAPS but are \
             not answered identically for every backend; drop the stale \
             entries: {stale:?}"
        );
    }

    /// `SQL_TXN_CAPABLE` and `SQL_TXN_ISOLATION_OPTION` constrain each other:
    /// `SQL_TC_NONE` says the data source has no transactions, and a non-zero
    /// isolation bitmask says it has at least one level to run them at. A
    /// backend that declares both is making one claim, and this is the pairing
    /// the AGENTS.md "info types that constrain each other" rule is about.
    ///
    /// Asserted over both classification mocks, which between them cover the
    /// two combinations that matter: `SQL_TC_ALL` with `SQL_TXN_SERIALIZABLE`
    /// and `SQL_TC_DML` with `SQL_TXN_READ_UNCOMMITTED`.
    #[test]
    fn txn_capable_and_the_isolation_options_agree_about_transactions() {
        use crate::test_utils::MockAltBackend;

        fn check<B: Backend>(conn: &B::Connection, name: &str) {
            let capable = B::txn_capable(conn);
            let options = B::txn_isolation_options(conn);
            assert_eq!(
                capable == crate::types::SQL_TC_NONE as u16,
                options == 0,
                "{name} reports SQL_TXN_CAPABLE = {capable} and \
                 SQL_TXN_ISOLATION_OPTION = {options:#x}; a data source with no \
                 transactions has no isolation levels, and one with levels is \
                 not SQL_TC_NONE"
            );
        }

        check::<MockBackend>(&MockConnection, "MockBackend");
        check::<MockAltBackend>(&MockConnection, "MockAltBackend");
    }

    /// The Windows Driver Manager queries driver identity *before*
    /// `SQLDriverConnectW`, so these answers cannot depend on a connection.
    /// With [`Backend::driver_name`] and [`Backend::driver_version`] declared,
    /// core answers them itself and a driver needs no
    /// [`Backend::get_info_pre_connect`] override to satisfy the Driver
    /// Manager.
    #[test]
    fn driver_identity_answers_without_a_connection() {
        assert_eq!(
            default_get_info::<MockBackend>(None, InfoType::DriverName),
            Some(InfoValue::String("Mock ODBC Driver".into())),
        );
        assert_eq!(
            default_get_info::<MockBackend>(None, InfoType::DriverVer),
            Some(InfoValue::String("01.00.0000".into())),
        );
        // The rest of the group the Windows DM asks for, for the same reason.
        assert_eq!(
            default_get_info::<MockBackend>(None, InfoType::DriverOdbcVer),
            Some(InfoValue::String(SQL_DRIVER_ODBC_VER_STRING.into())),
        );
        assert_eq!(
            default_get_info::<MockBackend>(None, InfoType::AsyncDbcFunctions),
            Some(InfoValue::U32(0)),
        );
        assert_eq!(
            default_get_info::<MockBackend>(None, InfoType::MaxConcurrentActivities),
            Some(InfoValue::U16(0)),
        );
        // The data-source half of identity is not answerable without one.
        assert_eq!(
            default_get_info::<MockBackend>(None, InfoType::DbmsName),
            None,
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
                    MockBackend::$hook(&MockConnection),
                    MockAltBackend::$hook(&MockConnection),
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
            identifier_case,
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
            quoted_identifier_case,
            txn_capable,
            integrity,
            multiple_active_txn,
            special_characters,
            accessible_procedures,
            dbms_name,
            dbms_version,
        );
        // Asserted separately for the same reason as the cursor-behaviour pair
        // below: driver identity answers before a connection exists, because
        // that is when the Windows Driver Manager asks for it.
        assert_ne!(MockBackend::driver_name(), MockAltBackend::driver_name());
        assert_ne!(
            MockBackend::driver_version(),
            MockAltBackend::driver_version(),
        );
        // Asserted separately: these three do not take a connection, because
        // `SQLGetInfo` has to answer the two cursor-behaviour info types before
        // one exists.
        assert_ne!(
            MockBackend::cursor_commit_behavior(),
            MockAltBackend::cursor_commit_behavior(),
        );
        assert_ne!(
            MockBackend::cursor_rollback_behavior(),
            MockAltBackend::cursor_rollback_behavior(),
        );
        assert_ne!(
            MockBackend::escape_dialect(&MockConnection).identifier_quotes,
            MockAltBackend::escape_dialect(&MockConnection).identifier_quotes,
        );
    }

    /// The five identifier-length info types follow the backend's declared
    /// catalog widths, not a baked-in 128, so a driver cannot report 63 in its
    /// catalog result sets and 128 here — two different answers about one
    /// limit.
    #[test]
    fn max_name_len_info_types_follow_the_backends_declared_widths() {
        use crate::test_utils::MockAltBackend;
        // `MockAltBackend` declares 63; the shared default is 128.
        assert_eq!(
            MockAltBackend::catalog_result_column_widths().identifier_len,
            63
        );
        assert_eq!(
            CatalogResultColumnWidths::default().identifier_len,
            128,
            "if the shared default were also 63 this test could not tell them apart"
        );
        for info_type in [
            InfoType::MaxColumnNameLen,
            InfoType::MaxSchemaNameLen,
            InfoType::MaxCatalogNameLen,
            InfoType::MaxTableNameLen,
            InfoType::MaxIdentifierLen,
        ] {
            assert_eq!(
                default_get_info::<MockAltBackend>(Some(&MockConnection), info_type),
                Some(InfoValue::U16(63)),
                "{info_type:?} ignored the backend's declared identifier_len"
            );
        }
    }
}
