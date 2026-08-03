//! Shared test infrastructure for stackable-odbc-core.
//!
//! `MockBackend` is the default: connect and disconnect succeed and everything else
//! answers `NotImplemented`. Beside it are purpose-built mocks for paths `MockBackend`
//! cannot reach — `MockAltBackend` declares a different value for every capability
//! method, `MockTypeInfoBackend` and `MockFunctionsBackend` declare real rows and a real
//! function list so a loop over them cannot pass vacuously, `MockFailingCloseBackend`
//! fails a cursor close, and the `mock_isolation_backend!` / `mock_txn_backend!` families
//! generate transaction-capability variants. Grep for `struct Mock` for the current set;
//! a list here would go stale, as an earlier mention of a `MockFailBackend` that was
//! never written did.

use std::borrow::Cow;
use std::ffi::c_void;

use odbc_sys::HandleType;

use crate::backend::{Backend, StatementBackend};
use crate::errors::OdbcError;
use crate::ffi::handle::{sql_alloc_handle, sql_free_handle};
use crate::types::{
    ColumnPrivilegeRow, ColumnRow, ConnectParams, ForeignKeyRow, InfoValue, Nullable, ParamType,
    PrimaryKeyRow, ProcedureColumnRow, ProcedureRow, SQL_FALSE, SQL_INDEX_OTHER, SQL_PC_NOT_PSEUDO,
    SQL_PT_PROCEDURE, SQL_SCOPE_CURROW, SQL_SCOPE_SESSION, SQL_SCOPE_TRANSACTION, SQL_TRUE,
    SpecialColumnRow, SqlDataType, SqlReturn, StatisticsRow, TablePrivilegeRow, TableRow,
    TypeInfoRow,
};

// ---------------------------------------------------------------------------
// Handle allocation helpers
// ---------------------------------------------------------------------------

/// Allocate an environment, a connection on it, and a statement on that
/// connection, all against [`MockBackend`].
///
/// Shared by every test module that needs a live env/conn/stmt triple rather
/// than building one by hand.
///
/// # Safety
///
/// Purely a test helper: it allocates real handles via `sql_alloc_handle` and
/// hands back their tokens, so the caller must free them (see
/// [`cleanup_env_conn_stmt`]) before the test ends.
pub(crate) unsafe fn alloc_env_conn_stmt() -> (*mut c_void, *mut c_void, *mut c_void) {
    let mut env: *mut c_void = std::ptr::null_mut();
    let _ = unsafe {
        sql_alloc_handle::<MockBackend>(HandleType::Env as i16, std::ptr::null_mut(), &mut env)
    };
    let mut conn: *mut c_void = std::ptr::null_mut();
    let _ = unsafe { sql_alloc_handle::<MockBackend>(HandleType::Dbc as i16, env, &mut conn) };
    let mut stmt: *mut c_void = std::ptr::null_mut();
    let _ = unsafe { sql_alloc_handle::<MockBackend>(HandleType::Stmt as i16, conn, &mut stmt) };
    (env, conn, stmt)
}

/// Free a statement, connection and environment allocated by
/// [`alloc_env_conn_stmt`], in child-before-parent order.
///
/// Any of the three may instead be `SQL_NULL_HANDLE` (null) for "already torn
/// down by other means, nothing to do here" — e.g. a test that disconnected
/// the connection itself, which frees its statements as a side effect. The
/// underlying `sql_free_handle` calls reject null without dereferencing it, so
/// this degrades to a no-op rather than freeing anything twice.
///
/// # Safety
///
/// Each of `env`, `conn` and `stmt` that is *not* null must be a live token
/// from `alloc_env_conn_stmt` (or an otherwise valid `MockBackend` handle)
/// that has not already been freed.
pub(crate) unsafe fn cleanup_env_conn_stmt(env: *mut c_void, conn: *mut c_void, stmt: *mut c_void) {
    unsafe {
        let _ = sql_free_handle::<MockBackend>(HandleType::Stmt as i16, stmt);
        let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
        let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
    }
}

/// [`alloc_env_conn_stmt`] for an arbitrary backend, with the connection
/// **actually connected**.
///
/// The difference from [`alloc_env_conn_stmt`] is the connect, and it is the
/// whole point: that helper leaves `ConnectionHandle::connection` as `None`, so
/// any code path guarded by "is there a connection to ask?" takes its
/// no-connection branch. A test written against it to prove a `Backend` hook is
/// consulted proves nothing — the hook is never reached and the test passes on
/// the fallback it was meant to rule out.
///
/// # Safety
///
/// As [`alloc_env_conn_stmt`]: the caller must free the three tokens with
/// [`cleanup_connected_env_conn_stmt`] before the test ends.
pub(crate) unsafe fn alloc_connected_env_conn_stmt<B: Backend>()
-> (*mut c_void, *mut c_void, *mut c_void) {
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
                i16::try_from(wide.len()).expect("the fixed test connection string is short"),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                0,
            ),
            SqlReturn::SUCCESS,
            "the helper's whole purpose is an open connection",
        );
        let mut stmt: *mut c_void = std::ptr::null_mut();
        let _ = sql_alloc_handle::<B>(HandleType::Stmt as i16, conn, &mut stmt);
        (env, conn, stmt)
    }
}

/// Tear down a triple from [`alloc_connected_env_conn_stmt`].
///
/// Disconnects first: a connected handle cannot be freed, so the plain
/// [`cleanup_env_conn_stmt`] would leak the connection and the environment and
/// turn the Miri job red.
///
/// # Safety
///
/// Each non-null token must be live and not already freed.
pub(crate) unsafe fn cleanup_connected_env_conn_stmt<B: Backend>(
    env: *mut c_void,
    conn: *mut c_void,
    stmt: *mut c_void,
) {
    unsafe {
        let _ = sql_free_handle::<B>(HandleType::Stmt as i16, stmt);
        let _ = crate::ffi::connect::sql_disconnect::<B>(conn);
        let _ = sql_free_handle::<B>(HandleType::Dbc as i16, conn);
        let _ = sql_free_handle::<B>(HandleType::Env as i16, env);
    }
}

/// Read or mutate a handle in a test, holding its group lock exactly as an FFI
/// entry point would.
///
/// Going through the same gate as production is what keeps a test from
/// asserting on a state the driver cannot actually observe.
pub(crate) fn with_handle<B: Backend, T: crate::handles::HasKind, R>(
    token: *mut c_void,
    f: impl FnOnce(&mut T) -> R,
) -> R {
    let mut out = None;
    let ret = unsafe {
        crate::panic::panic_safe::<B, _>(token, |scope| {
            out = Some(f(scope.get::<T>(token)?));
            Ok(SqlReturn::SUCCESS)
        })
    };
    assert_eq!(ret, SqlReturn::SUCCESS, "handle {token:?} was not valid");
    out.expect("closure ran")
}

/// [`with_handle`], for one of a statement's descriptors.
///
/// Goes through [`HandleScope::desc_of`] rather than naming a field of
/// [`StatementHandle`], so a test asserting on a record map keeps working as
/// which allocation a role resolves to changes.
///
/// [`HandleScope::desc_of`]: crate::handles::scope::HandleScope::desc_of
/// [`StatementHandle`]: crate::handles::StatementHandle
pub(crate) fn with_descriptor<B: Backend, R>(
    stmt: *mut c_void,
    role: crate::descriptor::DescriptorRole,
    f: impl FnOnce(&mut crate::handles::Descriptor) -> R,
) -> R {
    let mut out = None;
    let ret = unsafe {
        crate::panic::panic_safe::<B, _>(stmt, |scope| {
            out = Some(f(scope.desc_of::<B>(stmt, role)?));
            Ok(SqlReturn::SUCCESS)
        })
    };
    assert_eq!(ret, SqlReturn::SUCCESS, "statement {stmt:?} was not valid");
    out.expect("closure ran")
}

// ---------------------------------------------------------------------------
// Shared types
// ---------------------------------------------------------------------------

pub struct MockConnection;
pub struct MockStatement;

/// Records that `SQLCancel` reached the backend, so a test can assert the
/// signal arrived rather than merely that the call returned success.
///
/// `should_fail` is read only by [`MockFailingCloseBackend`]'s `cancel`,
/// which is the one mock whose `Error` is `OdbcError` directly rather than
/// `MockError` (`MockError` collapses to `NotImplemented` on the way back to
/// `OdbcError`, which `sql_cancel` treats as "nothing to cancel" rather than
/// a failure, so it cannot stand in for a backend's cancel actually erroring
/// out).
///
/// `saw_execution` is written only by [`MockRecordingBackend`]'s
/// `exec_direct`, which stores `true` into whatever token it receives, and
/// read only by the tests exercising it — proof that the token a
/// statement-producing call is handed is the same one `SQLCancel` would
/// later read back out of the registry, not merely some token.
///
/// `executed_params` is the same trick applied to [`Backend::execute`]'s
/// parameter slice: a test that must assert *what the backend received* needs
/// somewhere per-statement to put it, and the cancel token is the one piece of
/// backend-owned state a test can already read back out of the registry
/// (`Registry::cancel_of`). A `static` would be the alternative and would race,
/// because `cargo test` runs these in parallel threads.
#[derive(Debug, Default)]
pub struct MockCancelToken {
    pub cancelled: std::sync::atomic::AtomicBool,
    pub should_fail: std::sync::atomic::AtomicBool,
    pub saw_execution: std::sync::atomic::AtomicBool,
    /// The parameter values the last [`Backend::execute`] on this statement was
    /// handed, in parameter order.
    pub executed_params: crate::sync::Mutex<Vec<crate::types::ColumnValue>>,
}

#[derive(Debug)]
pub struct MockError;

impl std::fmt::Display for MockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "mock error")
    }
}

impl std::error::Error for MockError {}

impl From<MockError> for OdbcError {
    fn from(_: MockError) -> Self {
        // MockBackend/MockStatement methods return MockError to mean "this mock
        // does not implement this operation" (see the `StatementBackend` impl
        // below). Mapping to `NotImplemented` (rather than a generic `General`
        // error) lets tests exercise the "backend reports NotImplemented"
        // fallback paths (e.g. `SQLGetInfoW`'s DM-safe default) against a real
        // `Backend` impl instead of hand-constructing an `OdbcError`.
        OdbcError::NotImplemented {
            feature: "mock".into(),
        }
    }
}

impl From<OdbcError> for MockError {
    fn from(_: OdbcError) -> Self {
        // Required by the `Backend::Error` / `StatementBackend::Error` bounds so
        // that a defaulted trait body can construct an error and still name
        // `Self::Error`. The mock collapses every error to one value, which is
        // lossy — but the round trip back through `From<MockError> for OdbcError`
        // lands on `NotImplemented`, which is what the defaults produce anyway.
        MockError
    }
}

// Uses trait defaults (all return NotImplemented).
impl StatementBackend for MockStatement {
    type Error = MockError;
}

/// The required capability declarations, for a mock that stands for a minimal
/// data source and whose test does not care about them.
///
/// Every value is the *least* capable one the spec defines, which for four of
/// them happens to be `0`. That is deliberate: `0` is a real claim for these
/// info types, so a mock that means "minimal" should say so explicitly rather
/// than have core assume it — which is the whole point of the methods being
/// required. Expand this in a mock that is actually testing one of them.
///
/// `minimal_capability_decls!(keywords = <slice>)` keeps every other value
/// minimal but states a reserved-word list, for the mocks that exist only to
/// test the `SQL_KEYWORDS` subtraction; `table_types = <slice>` does the same
/// for the mock that exercises `SQLTables`' `SQL_ALL_TABLE_TYPES` enumeration.
///
/// `minimal_capability_decls!(identifier_case = <SQL_IC_*>, search_pattern_escape
/// = <str>)` states the two declarations `SQL_ATTR_METADATA_ID` normalisation
/// reads. The minimal pair — `SQL_IC_SENSITIVE` and no escape character — makes
/// normalisation a no-op, so a mock testing it has to say otherwise or every
/// assertion passes whether core normalised or not.
macro_rules! minimal_capability_decls {
    () => {
        minimal_capability_decls!(keywords = &[], table_types = &[]);
    };
    (keywords = $keywords:expr) => {
        minimal_capability_decls!(keywords = $keywords, table_types = &[]);
    };
    (table_types = $table_types:expr) => {
        minimal_capability_decls!(keywords = &[], table_types = $table_types);
    };
    (keywords = $keywords:expr, table_types = $table_types:expr) => {
        minimal_capability_decls!(
            keywords = $keywords,
            table_types = $table_types,
            identifier_case = crate::types::SQL_IC_SENSITIVE,
            search_pattern_escape = ""
        );
    };
    (table_types = $table_types:expr, identifier_case = $identifier_case:expr,
     search_pattern_escape = $search_pattern_escape:expr) => {
        minimal_capability_decls!(
            keywords = &[],
            table_types = $table_types,
            identifier_case = $identifier_case,
            search_pattern_escape = $search_pattern_escape
        );
    };
    (keywords = $keywords:expr, table_types = $table_types:expr,
     identifier_case = $identifier_case:expr,
     search_pattern_escape = $search_pattern_escape:expr) => {
        fn keywords(_conn: &Self::Connection) -> Cow<'static, [Cow<'static, str>]> {
            let list: &'static [&'static str] = $keywords;
            Cow::Owned(list.iter().map(|s| Cow::Borrowed(*s)).collect())
        }
        fn table_types(_conn: &Self::Connection) -> Vec<Cow<'static, str>> {
            let list: &'static [&'static str] = $table_types;
            list.iter().map(|s| Cow::Borrowed(*s)).collect()
        }
        fn group_by(_conn: &Self::Connection) -> u16 {
            crate::types::SQL_GB_NOT_SUPPORTED
        }
        fn null_collation(_conn: &Self::Connection) -> u16 {
            crate::types::SQL_NC_HIGH
        }
        // No "minimal" value exists: 0 is not a legal SQL_IDENTIFIER_CASE.
        fn identifier_case(_conn: &Self::Connection) -> u16 {
            $identifier_case
        }
        fn correlation_name(_conn: &Self::Connection) -> u16 {
            crate::types::SQL_CN_NONE
        }
        fn non_nullable_columns(_conn: &Self::Connection) -> u16 {
            crate::types::SQL_NNC_NULL
        }
        fn expressions_in_order_by(_conn: &Self::Connection) -> bool {
            false
        }
        // Conforms to no SQL-92 level, which is consistent with the values
        // above — an entry-level claim would contradict SQL_CN_NONE and
        // SQL_NNC_NULL.
        fn sql_conformance(_conn: &Self::Connection) -> u32 {
            0
        }
        fn timedate_add_intervals(_conn: &Self::Connection) -> u32 {
            0
        }
        fn timedate_diff_intervals(_conn: &Self::Connection) -> u32 {
            0
        }
        fn subqueries(_conn: &Self::Connection) -> u32 {
            0
        }
        fn column_alias(_conn: &Self::Connection) -> bool {
            false
        }
        fn concat_null_behavior(_conn: &Self::Connection) -> u16 {
            crate::types::SQL_CB_NULL
        }
        fn union_support(_conn: &Self::Connection) -> u32 {
            0
        }
        fn convert_functions(_conn: &Self::Connection) -> u32 {
            0
        }
        fn order_by_columns_in_select(_conn: &Self::Connection) -> bool {
            true
        }
        fn accessible_tables(_conn: &Self::Connection) -> bool {
            false
        }
        fn data_source_read_only(_conn: &Self::Connection) -> bool {
            false
        }
        fn search_pattern_escape(_conn: &Self::Connection) -> Cow<'static, str> {
            Cow::Borrowed($search_pattern_escape)
        }
        // No "minimal" value exists, for the same reason as identifier_case:
        // 0 is not a legal SQL_QUOTED_IDENTIFIER_CASE. SQL_IC_SENSITIVE is the
        // answer that transforms nothing.
        fn quoted_identifier_case(_conn: &Self::Connection) -> u16 {
            crate::types::SQL_IC_SENSITIVE
        }
        // Consistent with the isolation options a minimal mock declares.
        fn txn_capable(_conn: &Self::Connection) -> u16 {
            crate::types::SQL_TC_NONE as u16
        }
        fn integrity(_conn: &Self::Connection) -> bool {
            false
        }
        fn multiple_active_txn(_conn: &Self::Connection) -> bool {
            false
        }
        fn special_characters(_conn: &Self::Connection) -> Cow<'static, str> {
            Cow::Borrowed("")
        }
        fn accessible_procedures(_conn: &Self::Connection) -> bool {
            false
        }
        // Identity has no minimal value — the empty string is what these
        // methods exist to stop a driver reporting — so the mocks state
        // something recognisable instead.
        fn driver_name() -> Cow<'static, str> {
            Cow::Borrowed("Mock ODBC Driver")
        }
        fn driver_version() -> Cow<'static, str> {
            Cow::Borrowed("00.00.0000")
        }
        fn dbms_name(_conn: &Self::Connection) -> Cow<'static, str> {
            Cow::Borrowed("MockDB")
        }
        fn dbms_version(_conn: &Self::Connection) -> Cow<'static, str> {
            Cow::Borrowed("00.00.0000")
        }
    };
}

/// A one-column synthetic result set carrying `rows`.
///
/// Tests that need a statement with a genuinely open cursor must go through
/// `StatementHandle::set_result_set`, which derives
/// `cursor_open` from the column count — a zero-column result set opens no
/// cursor, because that is what an `UPDATE` produces.
pub fn synthetic_result_set(
    rows: Vec<Vec<crate::types::ColumnValue>>,
) -> crate::synthetic::SyntheticStatement {
    crate::synthetic::SyntheticStatement::new(
        vec![crate::types::ColumnDescriptor {
            name: "val".to_string(),
            type_name: String::new(),
            sql_type: crate::types::SqlDataType::INTEGER,
            precision: 10,
            scale: 0,
            nullable: Nullable::SqlNullable,
            ..Default::default()
        }],
        rows,
    )
}

// ---------------------------------------------------------------------------
// MockBackend — connect and disconnect succeed
// ---------------------------------------------------------------------------

/// A mock backend where `connect` and `disconnect` succeed.
/// Use this for tests that need to establish connections.
pub struct MockBackend;

impl Backend for MockBackend {
    type Connection = MockConnection;
    type Statement = MockStatement;
    type Error = MockError;
    type CancelToken = MockCancelToken;

    fn connect(_: &ConnectParams) -> Result<MockConnection, MockError> {
        Ok(MockConnection)
    }
    fn disconnect(_: &mut MockConnection) -> Result<(), MockError> {
        Ok(())
    }
    fn cancel_token(_conn: &Self::Connection) -> Self::CancelToken {
        MockCancelToken::default()
    }
    fn cancel(token: &Self::CancelToken) -> Result<(), Self::Error> {
        token
            .cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    fn is_cancelled(token: &Self::CancelToken) -> bool {
        token.cancelled.load(std::sync::atomic::Ordering::SeqCst)
    }
    fn exec_direct(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &str,
    ) -> Result<MockStatement, MockError> {
        Err(MockError)
    }
    fn prepare(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &str,
    ) -> Result<MockStatement, MockError> {
        Ok(MockStatement)
    }
    fn execute(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &mut MockStatement,
        _: &[crate::types::ColumnValue],
    ) -> Result<crate::types::ExecuteOutcome, MockError> {
        Ok(crate::types::ExecuteOutcome::default())
    }
    fn get_info(_: &MockConnection, _: crate::types::InfoType) -> Result<InfoValue, MockError> {
        Err(MockError)
    }
    fn get_functions() -> Cow<'static, [crate::function_id::FunctionId]> {
        Cow::Borrowed(&[])
    }
    fn get_type_info(_conn: &Self::Connection) -> Cow<'static, [TypeInfoRow]> {
        Cow::Borrowed(&[])
    }
    fn tables(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &crate::types::TablesQuery<'_>,
    ) -> Result<Vec<TableRow>, MockError> {
        Err(MockError)
    }
    fn columns(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &crate::types::ColumnsQuery<'_>,
    ) -> Result<Vec<ColumnRow>, MockError> {
        Err(MockError)
    }

    fn supports_catalogs(_conn: &Self::Connection) -> bool {
        true
    }
    fn supports_schemas(_conn: &Self::Connection) -> bool {
        true
    }
    // Deliberately non-zero and not a round number: these values are only
    // correct in a test if `default_get_info` actually read them from the
    // backend. The previous implementation returned a hard-coded 0 for both,
    // which a zero-valued mock could not have distinguished.
    fn alter_table_support(_conn: &Self::Connection) -> u32 {
        crate::types::SQL_AT_ADD_COLUMN_SINGLE | crate::types::SQL_AT_DROP_COLUMN_RESTRICT
    }
    fn outer_join_capabilities(_conn: &Self::Connection) -> u32 {
        crate::types::SQL_OJ_LEFT | crate::types::SQL_OJ_NESTED
    }
    // Deliberately SERIALIZABLE, not READ_COMMITTED. READ_COMMITTED is the
    // value core would most plausibly reach for if it answered the unset
    // `SQL_ATTR_TXN_ISOLATION` itself, and a mock declaring it could not tell
    // the hook being consulted from a constant that happens to agree.
    fn default_txn_isolation(_conn: &Self::Connection) -> u32 {
        crate::types::SQL_TXN_SERIALIZABLE
    }
    fn txn_isolation_options(_conn: &Self::Connection) -> u32 {
        crate::types::SQL_TXN_SERIALIZABLE
    }

    // A self-consistent SQL-92 Entry level declaration. The spec names the
    // value an entry-level driver returns for three of these, so declaring
    // `SQL_SC_SQL92_ENTRY` and then contradicting it would make this mock a
    // bad example as well as a bad test fixture. Each value is also
    // deliberately *not* the old core default, so a test cannot pass by
    // accident against the hard-coded value it replaced.
    fn group_by(_conn: &Self::Connection) -> u16 {
        crate::types::SQL_GB_GROUP_BY_EQUALS_SELECT // was SQL_GB_NO_RELATION
    }
    fn null_collation(_conn: &Self::Connection) -> u16 {
        crate::types::SQL_NC_END // was SQL_NC_HIGH (0), via the shape default
    }
    fn identifier_case(_conn: &Self::Connection) -> u16 {
        crate::types::SQL_IC_UPPER
    }
    // Deliberately not the same as `identifier_case` above: the two are
    // independent facts about the system catalog, and a mock that repeated one
    // could not tell a core that confused them apart from one that did not.
    fn quoted_identifier_case(_conn: &Self::Connection) -> u16 {
        crate::types::SQL_IC_SENSITIVE
    }
    // Switching succeeds; `current_catalog` stays at its `None` default, so the
    // getter's application-set branch is what these tests exercise.
    fn set_current_catalog(_conn: &Self::Connection, _catalog: &str) -> Result<(), MockError> {
        Ok(())
    }
    // Non-`SQL_TC_NONE`, as `txn_isolation_options` above declares a level.
    fn txn_capable(_conn: &Self::Connection) -> u16 {
        crate::types::SQL_TC_ALL as u16
    }
    fn integrity(_conn: &Self::Connection) -> bool {
        true
    }
    fn multiple_active_txn(_conn: &Self::Connection) -> bool {
        true
    }
    fn special_characters(_conn: &Self::Connection) -> Cow<'static, str> {
        Cow::Borrowed("$#")
    }
    fn accessible_procedures(_conn: &Self::Connection) -> bool {
        true
    }
    fn driver_name() -> Cow<'static, str> {
        Cow::Borrowed("Mock ODBC Driver")
    }
    fn driver_version() -> Cow<'static, str> {
        Cow::Borrowed("01.00.0000")
    }
    fn dbms_name(_conn: &Self::Connection) -> Cow<'static, str> {
        Cow::Borrowed("MockDB")
    }
    fn dbms_version(_conn: &Self::Connection) -> Cow<'static, str> {
        Cow::Borrowed("01.02.0003")
    }
    fn correlation_name(_conn: &Self::Connection) -> u16 {
        crate::types::SQL_CN_ANY // was SQL_CN_NONE (0)
    }
    fn non_nullable_columns(_conn: &Self::Connection) -> u16 {
        crate::types::SQL_NNC_NON_NULL // was SQL_NNC_NULL (0)
    }
    fn expressions_in_order_by(_conn: &Self::Connection) -> bool {
        true // was "" — neither "Y" nor "N"
    }
    fn sql_conformance(_conn: &Self::Connection) -> u32 {
        crate::types::SQL_SC_SQL92_ENTRY
    }
    // Deliberately different from each other, so one hook cannot serve both
    // without a test noticing.
    fn timedate_add_intervals(_conn: &Self::Connection) -> u32 {
        crate::types::SQL_FN_TSI_SECOND | crate::types::SQL_FN_TSI_DAY
    }
    fn timedate_diff_intervals(_conn: &Self::Connection) -> u32 {
        crate::types::SQL_FN_TSI_SECOND | crate::types::SQL_FN_TSI_YEAR
    }

    // The values the spec names for an entry-level driver, which is what keeps
    // this mock's `SQL_SC_SQL92_ENTRY` claim honest: `SQL_SQL_CONFORMANCE`
    // constrains each of the first three, so declaring a level and then
    // contradicting it here would make the mock itself a bad example.
    fn subqueries(_conn: &Self::Connection) -> u32 {
        crate::types::SQL_SQ_COMPARISON
            | crate::types::SQL_SQ_EXISTS
            | crate::types::SQL_SQ_IN
            | crate::types::SQL_SQ_QUANTIFIED
            | crate::types::SQL_SQ_CORRELATED_SUBQUERIES
    }
    fn column_alias(_conn: &Self::Connection) -> bool {
        true
    }
    fn concat_null_behavior(_conn: &Self::Connection) -> u16 {
        crate::types::SQL_CB_NULL
    }
    fn union_support(_conn: &Self::Connection) -> u32 {
        crate::types::SQL_U_UNION | crate::types::SQL_U_UNION_ALL
    }
    fn convert_functions(_conn: &Self::Connection) -> u32 {
        crate::types::SQL_FN_CVT_CAST
    }
    fn order_by_columns_in_select(_conn: &Self::Connection) -> bool {
        false
    }
    fn accessible_tables(_conn: &Self::Connection) -> bool {
        false
    }
    fn data_source_read_only(_conn: &Self::Connection) -> bool {
        false
    }
    fn search_pattern_escape(_conn: &Self::Connection) -> Cow<'static, str> {
        Cow::Borrowed("\\")
    }
    // Deliberately mixed: `SELECT` is in `ODBC_RESERVED_KEYWORDS` and must be
    // subtracted out, the other two are not and must survive. Unsorted, so the
    // ordering guarantee is exercised too.
    fn keywords(_conn: &Self::Connection) -> Cow<'static, [Cow<'static, str>]> {
        Cow::Borrowed(&[
            Cow::Borrowed("MOCK_PRAGMA"),
            Cow::Borrowed("SELECT"),
            Cow::Borrowed("MOCK_ATTACH"),
        ])
    }
    fn table_types(_conn: &Self::Connection) -> Vec<Cow<'static, str>> {
        vec![Cow::Borrowed("TABLE")]
    }
}

// ---------------------------------------------------------------------------
// MockNoCatalogBackend — a data source with neither catalogs nor schemas
// ---------------------------------------------------------------------------

/// A backend that declares no catalogs and no schemas, like SQLite.
///
/// Exists to pin the spec rule that `SQL_CATALOG_TERM`,
/// `SQL_CATALOG_NAME_SEPARATOR`, `SQL_SCHEMA_TERM`, `SQL_CATALOG_LOCATION`,
/// `SQL_CATALOG_USAGE` and `SQL_SCHEMA_USAGE` all collapse to an empty string
/// or zero when the underlying fact is "no".
pub struct MockNoCatalogBackend;

impl Backend for MockNoCatalogBackend {
    type Connection = MockConnection;
    type Statement = MockStatement;
    type Error = MockError;
    type CancelToken = MockCancelToken;

    fn connect(_: &ConnectParams) -> Result<MockConnection, MockError> {
        Ok(MockConnection)
    }
    fn disconnect(_: &mut MockConnection) -> Result<(), MockError> {
        Ok(())
    }
    fn cancel_token(_conn: &Self::Connection) -> Self::CancelToken {
        MockCancelToken::default()
    }
    fn cancel(token: &Self::CancelToken) -> Result<(), Self::Error> {
        token
            .cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    fn is_cancelled(token: &Self::CancelToken) -> bool {
        token.cancelled.load(std::sync::atomic::Ordering::SeqCst)
    }
    fn exec_direct(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &str,
    ) -> Result<MockStatement, MockError> {
        Err(MockError)
    }
    fn prepare(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &str,
    ) -> Result<MockStatement, MockError> {
        Ok(MockStatement)
    }
    fn execute(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &mut MockStatement,
        _: &[crate::types::ColumnValue],
    ) -> Result<crate::types::ExecuteOutcome, MockError> {
        Ok(crate::types::ExecuteOutcome::default())
    }
    fn get_info(_: &MockConnection, _: crate::types::InfoType) -> Result<InfoValue, MockError> {
        Err(MockError)
    }
    fn get_functions() -> Cow<'static, [crate::function_id::FunctionId]> {
        Cow::Borrowed(&[])
    }
    fn get_type_info(_conn: &Self::Connection) -> Cow<'static, [TypeInfoRow]> {
        Cow::Borrowed(&[])
    }
    fn tables(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &crate::types::TablesQuery<'_>,
    ) -> Result<Vec<TableRow>, MockError> {
        Err(MockError)
    }
    fn columns(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &crate::types::ColumnsQuery<'_>,
    ) -> Result<Vec<ColumnRow>, MockError> {
        Err(MockError)
    }

    fn supports_catalogs(_conn: &Self::Connection) -> bool {
        false
    }
    fn supports_schemas(_conn: &Self::Connection) -> bool {
        false
    }
    fn alter_table_support(_conn: &Self::Connection) -> u32 {
        0
    }
    fn outer_join_capabilities(_conn: &Self::Connection) -> u32 {
        0
    }
    fn default_txn_isolation(_conn: &Self::Connection) -> u32 {
        0
    }
    fn txn_isolation_options(_conn: &Self::Connection) -> u32 {
        0
    }

    minimal_capability_decls!();
}

// ---------------------------------------------------------------------------
// SQL_KEYWORDS subtraction mocks
// ---------------------------------------------------------------------------

/// Generates a minimal `Backend` whose only interesting declaration is
/// [`Backend::keywords`].
///
/// The `SQL_KEYWORDS` subtraction has three outcomes worth pinning separately —
/// nothing to filter, some entries filtered, everything filtered — and each
/// needs its own backend type because `keywords` is an associated function with
/// no receiver to vary.
macro_rules! mock_keywords_backend {
    ($(#[$doc:meta])* $name:ident, keywords = $keywords:expr) => {
        $(#[$doc])*
        pub struct $name;

        impl Backend for $name {
            type Connection = MockConnection;
            type Statement = MockStatement;
            type Error = MockError;
            type CancelToken = MockCancelToken;

            fn connect(_: &ConnectParams) -> Result<MockConnection, MockError> {
                Ok(MockConnection)
            }
            fn disconnect(_: &mut MockConnection) -> Result<(), MockError> {
                Ok(())
            }
            fn cancel_token(_conn: &Self::Connection) -> Self::CancelToken {
                MockCancelToken::default()
            }
            fn cancel(token: &Self::CancelToken) -> Result<(), Self::Error> {
                token.cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }
            fn is_cancelled(token: &Self::CancelToken) -> bool {
                token.cancelled.load(std::sync::atomic::Ordering::SeqCst)
            }
            fn exec_direct(
                _: &MockConnection,
                _: &Self::CancelToken,
                _: &str,
            ) -> Result<MockStatement, MockError> {
                Err(MockError)
            }
            fn prepare(
                _: &MockConnection,
                _: &Self::CancelToken,
                _: &str,
            ) -> Result<MockStatement, MockError> {
                Ok(MockStatement)
            }
            fn execute(
                _: &MockConnection,
                _: &Self::CancelToken,
                _: &mut MockStatement,
                _: &[crate::types::ColumnValue],
            ) -> Result<crate::types::ExecuteOutcome, MockError> {
                Ok(crate::types::ExecuteOutcome::default())
            }
            fn get_info(
                _: &MockConnection,
                _: crate::types::InfoType,
            ) -> Result<InfoValue, MockError> {
                Err(MockError)
            }
            fn get_functions() -> Cow<'static, [crate::function_id::FunctionId]> {
                Cow::Borrowed(&[])
            }
            fn get_type_info(_conn: &Self::Connection) -> Cow<'static, [TypeInfoRow]> {
                Cow::Borrowed(&[])
            }
            fn tables(
                _: &MockConnection,
                _: &Self::CancelToken,
                _: &crate::types::TablesQuery<'_>,
            ) -> Result<Vec<TableRow>, MockError> {
                Err(MockError)
            }
            fn columns(
                _: &MockConnection,
                _: &Self::CancelToken,
                _: &crate::types::ColumnsQuery<'_>,
            ) -> Result<Vec<ColumnRow>, MockError> {
                Err(MockError)
            }

            fn supports_catalogs(_conn: &Self::Connection) -> bool {
                false
            }
            fn supports_schemas(_conn: &Self::Connection) -> bool {
                false
            }
            fn alter_table_support(_conn: &Self::Connection) -> u32 {
                0
            }
            fn outer_join_capabilities(_conn: &Self::Connection) -> u32 {
                0
            }
            fn default_txn_isolation(_conn: &Self::Connection) -> u32 {
                0
            }
            fn txn_isolation_options(_conn: &Self::Connection) -> u32 {
                0
            }

            minimal_capability_decls!(keywords = $keywords);
        }
    };
}

mock_keywords_backend!(
    /// A data source that reserves nothing beyond ODBC — the value core
    /// produced for every backend before `Backend::keywords` existed.
    MockNoKeywordsBackend,
    keywords = &[]
);

mock_keywords_backend!(
    /// One keyword ODBC already reserves, one it does not.
    MockOverlappingKeywordsBackend,
    keywords = &["SELECT", "UNNEST"]
);

mock_keywords_backend!(
    /// A list that is entirely ODBC's, spelled in lower case: the subtraction
    /// is case-insensitive, so nothing survives it.
    MockReservedOnlyKeywordsBackend,
    keywords = &["select"]
);

// ---------------------------------------------------------------------------
// MockAltBackend — differs from MockBackend in every capability hook
// ---------------------------------------------------------------------------

/// A backend declaring a *different* value from [`MockBackend`] for every
/// hook `default_get_info` consults.
///
/// This exists for `default_get_info_answers_are_backend_derived_or_declared`,
/// which classifies each info type by asking whether the answer moves when the
/// backend does. An info type that answers identically for two backends with
/// nothing in common is, by construction, one core decides — so it must be
/// listed as a core fact with a reason, or it is a claim core has no business
/// making.
///
/// Every value here must differ from `MockBackend`'s. The test asserts that,
/// so a hook added to one mock and not the other cannot silently weaken it.
pub struct MockAltBackend;

impl Backend for MockAltBackend {
    type Connection = MockConnection;
    type Statement = MockStatement;
    type Error = MockError;
    type CancelToken = MockCancelToken;

    /// Names a secret that none of core's substring markers would catch, so a
    /// test can tell a backend-declared redaction from the built-in heuristic.
    fn sensitive_connect_keywords() -> Cow<'static, [Cow<'static, str>]> {
        Cow::Borrowed(&[Cow::Borrowed("wallet")])
    }

    fn connect(_: &ConnectParams) -> Result<MockConnection, MockError> {
        Ok(MockConnection)
    }
    fn disconnect(_: &mut MockConnection) -> Result<(), MockError> {
        Ok(())
    }
    fn cancel_token(_conn: &Self::Connection) -> Self::CancelToken {
        MockCancelToken::default()
    }
    fn cancel(token: &Self::CancelToken) -> Result<(), Self::Error> {
        token
            .cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    fn is_cancelled(token: &Self::CancelToken) -> bool {
        token.cancelled.load(std::sync::atomic::Ordering::SeqCst)
    }
    fn exec_direct(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &str,
    ) -> Result<MockStatement, MockError> {
        Err(MockError)
    }
    fn prepare(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &str,
    ) -> Result<MockStatement, MockError> {
        Ok(MockStatement)
    }
    fn execute(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &mut MockStatement,
        _: &[crate::types::ColumnValue],
    ) -> Result<crate::types::ExecuteOutcome, MockError> {
        Ok(crate::types::ExecuteOutcome::default())
    }
    fn get_info(_: &MockConnection, _: crate::types::InfoType) -> Result<InfoValue, MockError> {
        Err(MockError)
    }
    fn get_functions() -> Cow<'static, [crate::function_id::FunctionId]> {
        Cow::Borrowed(&[])
    }
    fn get_type_info(_conn: &Self::Connection) -> Cow<'static, [TypeInfoRow]> {
        Cow::Borrowed(&[])
    }
    fn tables(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &crate::types::TablesQuery<'_>,
    ) -> Result<Vec<TableRow>, MockError> {
        Err(MockError)
    }
    fn columns(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &crate::types::ColumnsQuery<'_>,
    ) -> Result<Vec<ColumnRow>, MockError> {
        Err(MockError)
    }

    fn supports_catalogs(_conn: &Self::Connection) -> bool {
        false
    }
    fn supports_schemas(_conn: &Self::Connection) -> bool {
        false
    }
    fn alter_table_support(_conn: &Self::Connection) -> u32 {
        crate::types::SQL_AT_ADD_CONSTRAINT
    }
    // Zero, i.e. no outer joins at all, where `MockBackend` declares
    // LEFT | NESTED. `SQL_OUTER_JOINS` is derived from this, so the two mocks
    // must disagree for the guard test to see that answer move with the
    // backend rather than being decided by core.
    fn outer_join_capabilities(_conn: &Self::Connection) -> u32 {
        0
    }
    fn default_txn_isolation(_conn: &Self::Connection) -> u32 {
        crate::types::SQL_TXN_READ_UNCOMMITTED
    }
    fn txn_isolation_options(_conn: &Self::Connection) -> u32 {
        crate::types::SQL_TXN_READ_UNCOMMITTED
    }
    fn group_by(_conn: &Self::Connection) -> u16 {
        crate::types::SQL_GB_NO_RELATION
    }
    fn identifier_case(_conn: &Self::Connection) -> u16 {
        crate::types::SQL_IC_MIXED
    }
    fn quoted_identifier_case(_conn: &Self::Connection) -> u16 {
        crate::types::SQL_IC_LOWER
    }
    // Reports a session catalog, so the getter's fallback branch — and
    // `SQL_DATABASE_NAME`, which reads the same two sources — have something to
    // find when the application has set nothing.
    fn current_catalog(_conn: &Self::Connection) -> Option<Cow<'static, str>> {
        Some(Cow::Borrowed("alt_catalog"))
    }
    fn set_current_catalog(_conn: &Self::Connection, _catalog: &str) -> Result<(), MockError> {
        Ok(())
    }
    fn txn_capable(_conn: &Self::Connection) -> u16 {
        crate::types::SQL_TC_DML as u16
    }
    fn integrity(_conn: &Self::Connection) -> bool {
        false
    }
    fn multiple_active_txn(_conn: &Self::Connection) -> bool {
        false
    }
    fn special_characters(_conn: &Self::Connection) -> Cow<'static, str> {
        Cow::Borrowed("@")
    }
    fn accessible_procedures(_conn: &Self::Connection) -> bool {
        false
    }
    fn driver_name() -> Cow<'static, str> {
        Cow::Borrowed("Alt ODBC Driver")
    }
    fn driver_version() -> Cow<'static, str> {
        Cow::Borrowed("09.08.0007")
    }
    fn dbms_name(_conn: &Self::Connection) -> Cow<'static, str> {
        Cow::Borrowed("AltDB")
    }
    fn dbms_version(_conn: &Self::Connection) -> Cow<'static, str> {
        Cow::Borrowed("09.09.9999")
    }
    fn null_collation(_conn: &Self::Connection) -> u16 {
        crate::types::SQL_NC_LOW
    }
    fn correlation_name(_conn: &Self::Connection) -> u16 {
        crate::types::SQL_CN_DIFFERENT
    }
    fn non_nullable_columns(_conn: &Self::Connection) -> u16 {
        crate::types::SQL_NNC_NULL
    }
    fn expressions_in_order_by(_conn: &Self::Connection) -> bool {
        false
    }
    fn sql_conformance(_conn: &Self::Connection) -> u32 {
        crate::types::SQL_SC_SQL92_FULL
    }
    fn timedate_add_intervals(_conn: &Self::Connection) -> u32 {
        crate::types::SQL_FN_TSI_HOUR
    }
    fn timedate_diff_intervals(_conn: &Self::Connection) -> u32 {
        crate::types::SQL_FN_TSI_WEEK
    }
    fn subqueries(_conn: &Self::Connection) -> u32 {
        crate::types::SQL_SQ_EXISTS
    }
    fn column_alias(_conn: &Self::Connection) -> bool {
        false
    }
    fn concat_null_behavior(_conn: &Self::Connection) -> u16 {
        crate::types::SQL_CB_NON_NULL
    }
    fn union_support(_conn: &Self::Connection) -> u32 {
        crate::types::SQL_U_UNION
    }
    fn convert_functions(_conn: &Self::Connection) -> u32 {
        crate::types::SQL_FN_CVT_CONVERT
    }
    fn order_by_columns_in_select(_conn: &Self::Connection) -> bool {
        true
    }
    fn accessible_tables(_conn: &Self::Connection) -> bool {
        true
    }
    fn data_source_read_only(_conn: &Self::Connection) -> bool {
        true
    }
    fn search_pattern_escape(_conn: &Self::Connection) -> Cow<'static, str> {
        Cow::Borrowed("/")
    }
    // Shares no entry with MockBackend's list, and spells its ODBC-reserved
    // overlap in lower case so the subtraction is proven case-insensitive.
    fn keywords(_conn: &Self::Connection) -> Cow<'static, [Cow<'static, str>]> {
        Cow::Borrowed(&[Cow::Borrowed("alter"), Cow::Borrowed("ALT_VACUUM")])
    }
    fn table_types(_conn: &Self::Connection) -> Vec<Cow<'static, str>> {
        vec![Cow::Borrowed("ALT_TABLE"), Cow::Borrowed("ALT_VIEW")]
    }

    // Differs from the default, so the SQL_MAX_*_NAME_LEN group moves with the
    // backend rather than looking core-decided.
    fn catalog_result_column_widths() -> crate::types::CatalogResultColumnWidths {
        crate::types::CatalogResultColumnWidths {
            identifier_len: 63,
            ..crate::types::CatalogResultColumnWidths::default()
        }
    }

    // Differs from MockBackend's `"`-quoted ANSI dialect, so
    // SQL_IDENTIFIER_QUOTE_CHAR moves with it.
    fn escape_dialect(_conn: &Self::Connection) -> crate::escape::EscapeDialect {
        crate::escape::EscapeDialect {
            identifier_quotes: &[('`', '`')],
            ..crate::escape::EscapeDialect::ansi_default()
        }
    }
    fn cursor_commit_behavior() -> crate::types::CursorBehavior {
        crate::types::CursorBehavior::Delete
    }
    fn cursor_rollback_behavior() -> crate::types::CursorBehavior {
        crate::types::CursorBehavior::Close
    }
}

// ---------------------------------------------------------------------------
// Isolation-level mocks
// ---------------------------------------------------------------------------

/// A connection that records the last isolation level applied to it.
///
/// Uses an atomic rather than a `Cell` because `Backend::set_txn_isolation`
/// receives `&Self::Connection`, and because it keeps the mock `Sync`.
pub struct MockIsolationConnection {
    pub applied: std::sync::atomic::AtomicU32,
}

/// Generates a `Backend` over [`MockIsolationConnection`] declaring
/// `$options` as its `SQL_TXN_ISOLATION_OPTION` bitmask and `$default` as its
/// `SQL_DEFAULT_TXN_ISOLATION`. Any trailing items are added to the `impl`, so
/// a mock either overrides [`Backend::set_txn_isolation`] or genuinely does
/// not define it and inherits the trait default.
macro_rules! mock_isolation_backend {
    ($name:ident, options = $options:expr, default = $default:expr $(, $extra:item)*) => {
        pub struct $name;

        impl Backend for $name {
            type Connection = MockIsolationConnection;
            type Statement = MockStatement;
            type Error = MockError;
            type CancelToken = MockCancelToken;

            fn connect(_: &ConnectParams) -> Result<MockIsolationConnection, MockError> {
                Ok(MockIsolationConnection {
                    applied: std::sync::atomic::AtomicU32::new(0),
                })
            }
            fn disconnect(_: &mut MockIsolationConnection) -> Result<(), MockError> {
                Ok(())
            }
            fn cancel_token(_conn: &Self::Connection) -> Self::CancelToken {
                MockCancelToken::default()
            }
            fn cancel(token: &Self::CancelToken) -> Result<(), Self::Error> {
                token.cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }
            fn is_cancelled(token: &Self::CancelToken) -> bool {
                token.cancelled.load(std::sync::atomic::Ordering::SeqCst)
            }
            fn exec_direct(
                _: &MockIsolationConnection,
                _: &Self::CancelToken,
                _: &str,
            ) -> Result<MockStatement, MockError> {
                Err(MockError)
            }
            fn prepare(
                _: &MockIsolationConnection,
                _: &Self::CancelToken,
                _: &str,
            ) -> Result<MockStatement, MockError> {
                Ok(MockStatement)
            }
            fn execute(
                _: &MockIsolationConnection,
                _: &Self::CancelToken,
                _: &mut MockStatement,
                _: &[crate::types::ColumnValue],
            ) -> Result<crate::types::ExecuteOutcome, MockError> {
                Ok(crate::types::ExecuteOutcome::default())
            }
            fn get_info(
                _: &MockIsolationConnection,
                _: crate::types::InfoType,
            ) -> Result<InfoValue, MockError> {
                Err(MockError)
            }
            fn get_functions() -> Cow<'static, [crate::function_id::FunctionId]> {
                Cow::Borrowed(&[])
            }
            fn get_type_info(_conn: &Self::Connection) -> Cow<'static, [TypeInfoRow]> {
                Cow::Borrowed(&[])
            }
            fn tables(
                _: &MockIsolationConnection,
                _: &Self::CancelToken,
                _: &crate::types::TablesQuery<'_>,
            ) -> Result<Vec<TableRow>, MockError> {
                Err(MockError)
            }
            fn columns(
                _: &MockIsolationConnection,
                _: &Self::CancelToken,
                _: &crate::types::ColumnsQuery<'_>,
            ) -> Result<Vec<ColumnRow>, MockError> {
                Err(MockError)
            }

            fn supports_catalogs(_conn: &Self::Connection) -> bool {
                true
            }
            fn supports_schemas(_conn: &Self::Connection) -> bool {
                true
            }
            fn alter_table_support(_conn: &Self::Connection) -> u32 {
                0
            }
            fn outer_join_capabilities(_conn: &Self::Connection) -> u32 {
                0
            }
            fn default_txn_isolation(_conn: &Self::Connection) -> u32 {
                $default
            }
            fn txn_isolation_options(_conn: &Self::Connection) -> u32 {
                $options
            }

            minimal_capability_decls!();

            $($extra)*
        }
    };
}

mock_isolation_backend!(
    MockIsolationBackend,
    options = crate::types::SQL_TXN_READ_COMMITTED
        | crate::types::SQL_TXN_REPEATABLE_READ
        | crate::types::SQL_TXN_SERIALIZABLE,
    default = crate::types::SQL_TXN_READ_COMMITTED,
    fn set_txn_isolation(conn: &MockIsolationConnection, level: u32) -> Result<(), MockError> {
        conn.applied
            .store(level, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
);
// Declares two levels but genuinely never implements `set_txn_isolation`, so
// it stands for the backend author who reports a capability and forgets to
// wire it up — and inherits the real trait default, not a copy of it.
mock_isolation_backend!(
    MockUnappliedIsolationBackend,
    options = crate::types::SQL_TXN_READ_COMMITTED | crate::types::SQL_TXN_SERIALIZABLE,
    default = crate::types::SQL_TXN_READ_COMMITTED
);

// ---------------------------------------------------------------------------
// Mocks that record what core applied to them
// ---------------------------------------------------------------------------

/// A connection that remembers the last value core pushed through a
/// "set" hook.
///
/// The point is to distinguish *core called the hook* from *core stored the
/// value and told the application it worked*. A test that only reads the
/// attribute back through `SQLGetStmtAttr` cannot tell those apart, and the
/// second is the bug the hook exists to remove.
///
/// `AtomicUsize` rather than a plain field because the hooks receive
/// `&Self::Connection`, not `&mut`. `usize` because `SQL_ATTR_QUERY_TIMEOUT` is
/// an `SQLULEN`.
pub struct MockAppliedConnection {
    pub query_timeout: std::sync::atomic::AtomicUsize,
    /// `Some(read_only)` once `set_access_mode` has been called, `None` before.
    /// The tri-state matters: `Some(false)` ("core applied read/write") and
    /// `None` ("core never called the hook") are exactly what an access-mode
    /// test has to tell apart, and a plain `bool` collapses them.
    pub access_mode: crate::sync::Mutex<Option<bool>>,
    /// Every value core pushed through `set_max_rows` and `set_max_length`, in
    /// order.
    ///
    /// A sequence rather than a latest-value slot because the fact under test
    /// is that the *reset* arrived, and both attributes default to 0: a
    /// `usize` initialised to 0 cannot tell "core called the hook with 0" from
    /// "core never called it". `[10]` and `[10, 0]` can.
    pub max_rows_calls: crate::sync::Mutex<Vec<usize>>,
    pub max_length_calls: crate::sync::Mutex<Vec<usize>>,
    /// What `ConnectParams::login_timeout` / `connection_timeout` reported at
    /// the moment `Backend::connect` ran. Captured there rather than read later
    /// because that call is the only place a backend ever sees them.
    pub seen_login_timeout: Option<u32>,
    pub seen_connection_timeout: Option<u32>,
    /// What `ConnectParams::dsn()` reported at the moment `Backend::connect`
    /// ran, for the same reason as the two timeouts above.
    ///
    /// The interesting case is `SQLConnectW`, whose only argument naming a data
    /// source is the DSN name itself: it is not one of the DSN's keys, so
    /// nothing puts it in the params unless core does.
    pub seen_dsn: Option<String>,
}

/// Generates a `Backend` whose only interesting behaviour is the extra items
/// passed in — everything else is the minimum the trait requires.
///
/// Same shape as [`mock_isolation_backend`]: the trailing `$extra` items are
/// spliced into the `impl`, so a variant that omits one genuinely inherits the
/// real trait default rather than a copy of it. That is what lets the
/// "backend said nothing" case be tested against the actual default.
macro_rules! mock_applied_backend {
    ($name:ident $(, $extra:item)*) => {
        mock_applied_backend!(@build $name, MockError $(, $extra)*);
    };
    // `error = OdbcError` for a mock that must report a *real* failure.
    // `MockError` converts to `OdbcError::NotImplemented` by design, so a mock
    // using it cannot express "this genuinely went wrong" — every error it
    // returns looks to core like "I do not implement this", which is a
    // different branch on every hook that distinguishes the two.
    (error = $err:ty, $name:ident $(, $extra:item)*) => {
        mock_applied_backend!(@build $name, $err $(, $extra)*);
    };
    (@build $name:ident, $err:ty $(, $extra:item)*) => {
        #[allow(dead_code)]
        pub struct $name;

        impl Backend for $name {
            type Connection = MockAppliedConnection;
            type Statement = MockStatement;
            type Error = $err;
            type CancelToken = MockCancelToken;

            fn connect(params: &ConnectParams) -> Result<MockAppliedConnection, $err> {
                Ok(MockAppliedConnection {
                    query_timeout: std::sync::atomic::AtomicUsize::new(0),
                    access_mode: crate::sync::Mutex::new(None),
                    max_rows_calls: crate::sync::Mutex::new(Vec::new()),
                    max_length_calls: crate::sync::Mutex::new(Vec::new()),
                    seen_login_timeout: params.login_timeout(),
                    seen_connection_timeout: params.connection_timeout(),
                    seen_dsn: params.dsn().map(str::to_owned),
                })
            }
            fn disconnect(_: &mut MockAppliedConnection) -> Result<(), $err> {
                Ok(())
            }
            fn cancel_token(_conn: &Self::Connection) -> Self::CancelToken {
                MockCancelToken::default()
            }
            // A real (if trivial) cancel, not the inert default: a mock that
            // answers `QueryTimeout::CoreCancels` is asserting that cancelling
            // works, and one whose `cancel` did nothing would let core's timer
            // fire into a void while the test still passed.
            fn cancel(token: &Self::CancelToken) -> Result<(), Self::Error> {
                token
                    .cancelled
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }
            fn is_cancelled(token: &Self::CancelToken) -> bool {
                token.cancelled.load(std::sync::atomic::Ordering::SeqCst)
            }
            // `"BLOCK"` stands in for a runaway query: it waits until something
            // cancels the token, then fails the way a real client library would
            // when its query was killed out from under it. That is the only
            // shape in which core's query timer can be observed end to end —
            // the timer exists precisely because a backend call blocks the
            // calling thread, so a mock that returns immediately can never
            // exercise it.
            //
            // The SQL text carries the switch rather than a static, so parallel
            // tests cannot interfere with each other.
            //
            // The wait is **bounded**, and that is not belt-and-braces. If core
            // ever stops arming its timer, nothing will cancel this token, and
            // an unbounded wait would turn the test that catches the regression
            // into a hung CI job — a much worse signal than a failed assertion.
            // The bound is far longer than any deadline a test sets, so it
            // cannot mask a genuine timeout.
            fn exec_direct(
                _: &MockAppliedConnection,
                cancel: &Self::CancelToken,
                sql: &str,
            ) -> Result<MockStatement, $err> {
                if sql == "BLOCK" {
                    // Counted rather than clock-measured: `Instant::now` is a
                    // disallowed method crate-wide, and a bound this loose does
                    // not need a real clock to serve its purpose.
                    const STEP_MS: u64 = 5;
                    const MAX_STEPS: u32 = 6_000; // ~30s, far above any test deadline
                    let mut steps = 0;
                    while !cancel.cancelled.load(std::sync::atomic::Ordering::SeqCst) {
                        steps += 1;
                        assert!(
                            steps < MAX_STEPS,
                            "nothing cancelled this call — core almost certainly \
                             stopped arming its query timer",
                        );
                        std::thread::sleep(std::time::Duration::from_millis(STEP_MS));
                    }
                    return Err(MockError.into());
                }
                Ok(MockStatement)
            }
            fn prepare(
                _: &MockAppliedConnection,
                _: &Self::CancelToken,
                _: &str,
            ) -> Result<MockStatement, $err> {
                Ok(MockStatement)
            }
            fn execute(
                _: &MockAppliedConnection,
                _: &Self::CancelToken,
                _: &mut MockStatement,
                _: &[crate::types::ColumnValue],
            ) -> Result<crate::types::ExecuteOutcome, $err> {
                Ok(crate::types::ExecuteOutcome::default())
            }
            fn get_info(
                _: &MockAppliedConnection,
                _: crate::types::InfoType,
            ) -> Result<InfoValue, $err> {
                Err(MockError.into())
            }
            fn get_functions() -> Cow<'static, [crate::function_id::FunctionId]> {
                Cow::Borrowed(&[])
            }
            fn get_type_info(_conn: &Self::Connection) -> Cow<'static, [TypeInfoRow]> {
                Cow::Borrowed(&[])
            }
            fn tables(
                _: &MockAppliedConnection,
                _: &Self::CancelToken,
                _: &crate::types::TablesQuery<'_>,
            ) -> Result<Vec<TableRow>, $err> {
                Err(MockError.into())
            }
            fn columns(
                _: &MockAppliedConnection,
                _: &Self::CancelToken,
                _: &crate::types::ColumnsQuery<'_>,
            ) -> Result<Vec<ColumnRow>, $err> {
                Err(MockError.into())
            }

            fn supports_catalogs(_conn: &Self::Connection) -> bool {
                true
            }
            fn supports_schemas(_conn: &Self::Connection) -> bool {
                true
            }
            fn alter_table_support(_conn: &Self::Connection) -> u32 {
                0
            }
            fn outer_join_capabilities(_conn: &Self::Connection) -> u32 {
                0
            }
            fn default_txn_isolation(_conn: &Self::Connection) -> u32 {
                0
            }
            fn txn_isolation_options(_conn: &Self::Connection) -> u32 {
                0
            }

            minimal_capability_decls!();

            $($extra)*
        }
    };
}

// A data source that really can enforce a deadline itself: records what it was
// given and takes ownership of it, so core must arm no timer of its own.
mock_applied_backend!(
    MockQueryTimeoutBackend,
    fn set_query_timeout(
        conn: &MockAppliedConnection,
        seconds: usize,
    ) -> Result<crate::types::QueryTimeout, MockError> {
        conn.query_timeout
            .store(seconds, std::sync::atomic::Ordering::SeqCst);
        Ok(crate::types::QueryTimeout::DataSource)
    }
);

// A backend that cannot set a server-side deadline but can be cancelled, so it
// hands the deadline to core. `cancel`/`is_cancelled` come from the macro, which
// is what makes the assertion meaningful: a mock that declared `CoreCancels`
// with an inert `cancel` would let the timer fire into a void and the test pass
// without proving anything reached the backend.
mock_applied_backend!(
    MockCoreCancelsTimeoutBackend,
    fn set_query_timeout(
        conn: &MockAppliedConnection,
        seconds: usize,
    ) -> Result<crate::types::QueryTimeout, MockError> {
        conn.query_timeout
            .store(seconds, std::sync::atomic::Ordering::SeqCst);
        Ok(crate::types::QueryTimeout::CoreCancels)
    }
);

// A data source that supports timeouts but whose connection is broken, so the
// attempt fails for a reason that is *not* "unimplemented". Pins that core
// propagates such a failure instead of quietly substituting 0 and reporting
// 01S02, which would tell the application its timeout was merely capped.
mock_applied_backend!(
    error = OdbcError,
    MockFailingQueryTimeoutBackend,
    fn set_query_timeout(
        _conn: &MockAppliedConnection,
        _seconds: usize,
    ) -> Result<crate::types::QueryTimeout, OdbcError> {
        Err(OdbcError::general(
            "mock set_query_timeout failure",
            crate::types::SqlState::communication_link_failure(),
        ))
    }
);

// Declares nothing about timeouts at all, and so inherits the real
// `Backend::set_query_timeout` default. This is the backend every existing
// driver is, and its 01S02 substitution is the behaviour that must not change.
//
// It doubles as the `connection_dead` control: it inherits that default too, so
// a test pairing it with `MockDeadConnectionBackend` below sees the answer move
// with the backend rather than with the test.
mock_applied_backend!(MockNoQueryTimeoutBackend);

// Records the access mode core applied, so a test can tell "core called the
// hook" from "core stored the attribute and told the application it worked".
mock_applied_backend!(
    MockAccessModeBackend,
    fn set_access_mode(conn: &MockAppliedConnection, read_only: bool) -> Result<(), MockError> {
        if let Ok(mut slot) = conn.access_mode.lock() {
            *slot = Some(read_only);
        }
        Ok(())
    }
);

// A data source with no read-only session mode that judges silently ignoring
// the request to be worse than refusing it. Pins that core propagates the
// refusal and stores nothing, rather than reporting success for a mode the
// data source never entered.
mock_applied_backend!(
    error = OdbcError,
    MockRefusingAccessModeBackend,
    fn set_access_mode(_conn: &MockAppliedConnection, _read_only: bool) -> Result<(), OdbcError> {
        Err(OdbcError::NotImplemented {
            feature: "SQL_ATTR_ACCESS_MODE".into(),
        })
    }
);

// A data source that can genuinely cap a result set and truncate wide columns
// server-side, which is the only situation the spec sanctions either attribute
// in. It records what it was told so a test can tell "core called the hook"
// from "core stored the value and told the application it worked".
mock_applied_backend!(
    MockLimitsBackend,
    fn set_max_rows(conn: &MockAppliedConnection, rows: usize) -> Result<(), MockError> {
        if let Ok(mut calls) = conn.max_rows_calls.lock() {
            calls.push(rows);
        }
        Ok(())
    },
    fn set_max_length(conn: &MockAppliedConnection, bytes: usize) -> Result<(), MockError> {
        if let Ok(mut calls) = conn.max_length_calls.lock() {
            calls.push(bytes);
        }
        Ok(())
    }
);

// A backend whose connection has been lost — what a pool must not be handed.
mock_applied_backend!(
    MockDeadConnectionBackend,
    fn connection_dead(_conn: &MockAppliedConnection) -> bool {
        true
    }
);

// ---------------------------------------------------------------------------
// Transaction-capable mocks
// ---------------------------------------------------------------------------

/// A connection that remembers whether its `end_tran` should fail.
///
/// The flag is read from the connection string (`ENDTRANFAIL=1`) rather than
/// from a static, so one environment can hold both failing and succeeding
/// connections and the tests stay parallel-safe.
pub struct MockTxnConnection {
    pub end_tran_fails: bool,
}

/// Generates a transaction-capable `Backend` with the given declared cursor
/// behaviours. `end_tran` succeeds unless the connection was opened with
/// `ENDTRANFAIL=1`, in which case it fails with a generic HY000. Pass
/// `error = ...` for a backend whose `end_tran` reports something more
/// specific — e.g. `OdbcError::NotConnected`, to exercise a *backend*
/// reporting the same variant core's own "no open connection" pre-check uses
/// internally (see `end_tran_on_connection`'s `EndTranOutcome`).
macro_rules! mock_txn_backend {
    ($name:ident, commit = $commit:expr, rollback = $rollback:expr) => {
        mock_txn_backend!(
            $name,
            commit = $commit,
            rollback = $rollback,
            error = OdbcError::general(
                "mock end_tran failure",
                crate::types::SqlState::general_error(),
            )
        );
    };
    ($name:ident, commit = $commit:expr, rollback = $rollback:expr, error = $error:expr) => {
        #[allow(dead_code)]
        pub struct $name;

        impl Backend for $name {
            type Connection = MockTxnConnection;
            type Statement = MockStatement;
            /// `OdbcError` rather than `MockError`: `end_tran` below constructs
            /// a real error with a message and SQLSTATE, and routing that
            /// through `MockError` would collapse it to `NotImplemented`. Using
            /// `OdbcError` directly also exercises the case of a backend whose
            /// error type simply *is* core's — which the bounds allow, since
            /// `OdbcError` converts to and from itself.
            type Error = OdbcError;
            type CancelToken = MockCancelToken;

            fn connect(params: &ConnectParams) -> Result<MockTxnConnection, OdbcError> {
                Ok(MockTxnConnection {
                    end_tran_fails: params.get("endtranfail") == Some("1"),
                })
            }
            fn disconnect(_: &mut MockTxnConnection) -> Result<(), OdbcError> {
                Ok(())
            }
            fn cancel_token(_conn: &Self::Connection) -> Self::CancelToken {
                MockCancelToken::default()
            }
            fn cancel(token: &Self::CancelToken) -> Result<(), Self::Error> {
                token
                    .cancelled
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }
            fn is_cancelled(token: &Self::CancelToken) -> bool {
                token.cancelled.load(std::sync::atomic::Ordering::SeqCst)
            }
            // Succeeds, unlike `MockBackend::exec_direct`, so a test can tell
            // "SQLExecDirect was allowed" from "SQLExecDirect was rejected".
            // `MockStatement` reports zero columns, so it stands for a
            // non-result-set statement and opens no cursor.
            fn exec_direct(
                _: &MockTxnConnection,
                _: &Self::CancelToken,
                _: &str,
            ) -> Result<MockStatement, OdbcError> {
                Ok(MockStatement)
            }
            fn prepare(
                _: &MockTxnConnection,
                _: &Self::CancelToken,
                _: &str,
            ) -> Result<MockStatement, OdbcError> {
                Ok(MockStatement)
            }
            fn execute(
                _: &MockTxnConnection,
                _: &Self::CancelToken,
                _: &mut MockStatement,
                _: &[crate::types::ColumnValue],
            ) -> Result<crate::types::ExecuteOutcome, OdbcError> {
                Ok(crate::types::ExecuteOutcome::default())
            }
            fn get_info(
                _: &MockTxnConnection,
                _: crate::types::InfoType,
            ) -> Result<InfoValue, OdbcError> {
                Err(OdbcError::NotImplemented {
                    feature: "mock txn backend".into(),
                })
            }
            fn get_functions() -> Cow<'static, [crate::function_id::FunctionId]> {
                Cow::Borrowed(&[])
            }
            fn get_type_info(_conn: &Self::Connection) -> Cow<'static, [TypeInfoRow]> {
                Cow::Borrowed(&[])
            }
            fn tables(
                _: &MockTxnConnection,
                _: &Self::CancelToken,
                _: &crate::types::TablesQuery<'_>,
            ) -> Result<Vec<TableRow>, OdbcError> {
                Err(OdbcError::NotImplemented {
                    feature: "mock txn backend".into(),
                })
            }
            fn columns(
                _: &MockTxnConnection,
                _: &Self::CancelToken,
                _: &crate::types::ColumnsQuery<'_>,
            ) -> Result<Vec<ColumnRow>, OdbcError> {
                Err(OdbcError::NotImplemented {
                    feature: "mock txn backend".into(),
                })
            }

            fn set_autocommit(_: &MockTxnConnection, _: bool) -> Result<(), OdbcError> {
                Ok(())
            }

            fn end_tran(conn: &MockTxnConnection, _commit: bool) -> Result<(), OdbcError> {
                if conn.end_tran_fails {
                    Err($error)
                } else {
                    Ok(())
                }
            }

            fn cursor_commit_behavior() -> crate::types::CursorBehavior {
                $commit
            }
            fn cursor_rollback_behavior() -> crate::types::CursorBehavior {
                $rollback
            }

            fn supports_catalogs(_conn: &Self::Connection) -> bool {
                true
            }
            fn supports_schemas(_conn: &Self::Connection) -> bool {
                true
            }
            fn alter_table_support(_conn: &Self::Connection) -> u32 {
                0
            }
            fn outer_join_capabilities(_conn: &Self::Connection) -> u32 {
                0
            }
            // A single supported level, so `Backend::set_txn_isolation`'s
            // default applies it without these mocks overriding anything.
            fn default_txn_isolation(_conn: &Self::Connection) -> u32 {
                crate::types::SQL_TXN_SERIALIZABLE
            }
            fn txn_isolation_options(_conn: &Self::Connection) -> u32 {
                crate::types::SQL_TXN_SERIALIZABLE
            }

            minimal_capability_decls!();
        }
    };
}

mock_txn_backend!(
    MockTxnCloseBackend,
    commit = crate::types::CursorBehavior::Close,
    rollback = crate::types::CursorBehavior::Close
);
mock_txn_backend!(
    MockTxnDeleteBackend,
    commit = crate::types::CursorBehavior::Delete,
    rollback = crate::types::CursorBehavior::Delete
);
mock_txn_backend!(
    MockTxnPreserveBackend,
    commit = crate::types::CursorBehavior::Preserve,
    rollback = crate::types::CursorBehavior::Preserve
);
// Deliberately mismatched, to prove the commit and rollback values are read
// from separate hooks and not from one shared value.
mock_txn_backend!(
    MockTxnDeleteCloseBackend,
    commit = crate::types::CursorBehavior::Delete,
    rollback = crate::types::CursorBehavior::Close
);
// `end_tran` fails with `OdbcError::NotConnected` rather than a generic
// error: that variant is also core's own signal for "no open connection on
// this handle" (see `end_tran_on_connection`'s `EndTranOutcome`), so this
// mock exercises a *backend* reporting it instead, from inside a call that
// core already knows is on a connected connection. That must still surface
// as a failure, not be mistaken for core's own silent skip.
mock_txn_backend!(
    MockTxnNotConnectedBackend,
    commit = crate::types::CursorBehavior::Close,
    rollback = crate::types::CursorBehavior::Close,
    error = OdbcError::NotConnected
);

// ---------------------------------------------------------------------------
// A backend that declares type info
// ---------------------------------------------------------------------------

/// Declares several `SQLGetTypeInfo` rows in deliberately wrong order, so a
/// test can tell that `sql_get_type_info` sorts rather than passing the
/// backend's list through.
pub struct MockTypeInfoBackend;

impl MockTypeInfoBackend {
    /// Out of spec order on both keys: DATA_TYPE descends, and the two
    /// `VARCHAR` rows are reverse-alphabetical by TYPE_NAME.
    const TYPES: &'static [TypeInfoRow] = &[
        type_info_row("VARCHAR2", crate::types::SqlDataType::VARCHAR),
        type_info_row("INTEGER", crate::types::SqlDataType::INTEGER),
        type_info_row("VARCHAR", crate::types::SqlDataType::VARCHAR),
        type_info_row("BIGINT", crate::types::SqlDataType::EXT_BIG_INT),
    ];
}

/// A minimal `TypeInfoRow`; only `type_name` and `data_type` matter for
/// ordering.
const fn type_info_row(
    type_name: &'static str,
    data_type: crate::types::SqlDataType,
) -> TypeInfoRow {
    TypeInfoRow {
        type_name: Cow::Borrowed(type_name),
        data_type,
        column_size: 0,
        literal_prefix: None,
        literal_suffix: None,
        create_params: None,
        nullable: crate::types::Nullable::SqlNullable,
        case_sensitive: false,
        searchable: 3,
        unsigned: None,
        fixed_prec_scale: false,
        auto_unique_value: None,
        local_type_name: None,
        minimum_scale: None,
        maximum_scale: None,
        sql_data_type: 0,
        sql_datetime_sub: None,
        num_prec_radix: None,
        interval_precision: None,
    }
}

impl Backend for MockTypeInfoBackend {
    type Connection = MockConnection;
    type Statement = MockStatement;
    type Error = MockError;
    type CancelToken = MockCancelToken;

    fn connect(_: &ConnectParams) -> Result<MockConnection, MockError> {
        Ok(MockConnection)
    }
    fn disconnect(_: &mut MockConnection) -> Result<(), MockError> {
        Ok(())
    }
    fn cancel_token(_conn: &Self::Connection) -> Self::CancelToken {
        MockCancelToken::default()
    }
    fn cancel(token: &Self::CancelToken) -> Result<(), Self::Error> {
        token
            .cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    fn is_cancelled(token: &Self::CancelToken) -> bool {
        token.cancelled.load(std::sync::atomic::Ordering::SeqCst)
    }
    fn exec_direct(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &str,
    ) -> Result<MockStatement, MockError> {
        Err(MockError)
    }
    fn prepare(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &str,
    ) -> Result<MockStatement, MockError> {
        Ok(MockStatement)
    }
    fn execute(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &mut MockStatement,
        _: &[crate::types::ColumnValue],
    ) -> Result<crate::types::ExecuteOutcome, MockError> {
        Ok(crate::types::ExecuteOutcome::default())
    }
    fn get_info(_: &MockConnection, _: crate::types::InfoType) -> Result<InfoValue, MockError> {
        Err(MockError)
    }
    fn get_functions() -> Cow<'static, [crate::function_id::FunctionId]> {
        Cow::Borrowed(&[])
    }
    fn get_type_info(_conn: &Self::Connection) -> Cow<'static, [TypeInfoRow]> {
        Cow::Borrowed(Self::TYPES)
    }
    fn tables(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &crate::types::TablesQuery<'_>,
    ) -> Result<Vec<TableRow>, MockError> {
        Err(MockError)
    }
    fn columns(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &crate::types::ColumnsQuery<'_>,
    ) -> Result<Vec<ColumnRow>, MockError> {
        Err(MockError)
    }

    fn supports_catalogs(_conn: &Self::Connection) -> bool {
        false
    }
    fn supports_schemas(_conn: &Self::Connection) -> bool {
        false
    }
    fn alter_table_support(_conn: &Self::Connection) -> u32 {
        0
    }
    fn outer_join_capabilities(_conn: &Self::Connection) -> u32 {
        0
    }
    fn default_txn_isolation(_conn: &Self::Connection) -> u32 {
        0
    }
    fn txn_isolation_options(_conn: &Self::Connection) -> u32 {
        0
    }

    minimal_capability_decls!();
}

// ---------------------------------------------------------------------------
// A backend that declares catalog rows
// ---------------------------------------------------------------------------

/// Declares real catalog rows, in deliberately wrong order, so a test can tell
/// that the catalog FFI functions sort rather than passing the backend's rows
/// through.
///
/// These rows are not on [`MockBackend`]: a backend that returns rows changes
/// what `SQLTables` does on every test that touches it, and the existing ones
/// assume it returns nothing.
pub struct MockCatalogBackend;

// `SQL_TRUE`/`SQL_FALSE` and the `SQL_SCOPE_*` constants are declared at the
// width of the argument that carries them — `u32` for the attribute values,
// `u16` for `SQLSpecialColumns`' `Scope` argument. The `NON_UNIQUE` and
// `SCOPE` *result* columns are Smallint. Same spec values, narrower column;
// `i16::try_from` is not usable in a `const` initialiser, and none of these
// five values can truncate.
const NON_UNIQUE_TRUE: i16 = SQL_TRUE as i16;
const NON_UNIQUE_FALSE: i16 = SQL_FALSE as i16;
const SCOPE_CURROW: i16 = SQL_SCOPE_CURROW as i16;
const SCOPE_TRANSACTION: i16 = SQL_SCOPE_TRANSACTION as i16;
const SCOPE_SESSION: i16 = SQL_SCOPE_SESSION as i16;

impl Backend for MockCatalogBackend {
    type Connection = MockConnection;
    type Statement = MockStatement;
    type Error = MockError;
    type CancelToken = MockCancelToken;

    fn connect(_: &ConnectParams) -> Result<MockConnection, MockError> {
        Ok(MockConnection)
    }
    fn disconnect(_: &mut MockConnection) -> Result<(), MockError> {
        Ok(())
    }
    fn cancel_token(_conn: &Self::Connection) -> Self::CancelToken {
        MockCancelToken::default()
    }
    fn cancel(token: &Self::CancelToken) -> Result<(), Self::Error> {
        token
            .cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    fn is_cancelled(token: &Self::CancelToken) -> bool {
        token.cancelled.load(std::sync::atomic::Ordering::SeqCst)
    }
    fn exec_direct(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &str,
    ) -> Result<MockStatement, MockError> {
        Err(MockError)
    }
    fn prepare(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &str,
    ) -> Result<MockStatement, MockError> {
        Err(MockError)
    }
    fn execute(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &mut MockStatement,
        _: &[crate::types::ColumnValue],
    ) -> Result<crate::types::ExecuteOutcome, MockError> {
        Err(MockError)
    }
    fn get_info(_: &MockConnection, _: crate::types::InfoType) -> Result<InfoValue, MockError> {
        Err(MockError)
    }
    fn get_functions() -> Cow<'static, [crate::function_id::FunctionId]> {
        Cow::Borrowed(&[])
    }
    fn get_type_info(_conn: &Self::Connection) -> Cow<'static, [TypeInfoRow]> {
        Cow::Borrowed(&[])
    }
    /// Out of spec order on the dominant key: the `VIEW` comes first, and the
    /// two `TABLE` rows are reverse-alphabetical. Passing these through
    /// unsorted therefore fails the ordering test rather than passing by luck.
    fn tables(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &crate::types::TablesQuery<'_>,
    ) -> Result<Vec<TableRow>, MockError> {
        Ok(vec![
            TableRow {
                catalog: Some("cat".into()),
                schema: Some("sch".into()),
                name: Some("a_view".into()),
                table_type: Some("VIEW".into()),
                remarks: None,
            },
            TableRow {
                catalog: Some("cat".into()),
                schema: Some("sch".into()),
                name: Some("z_table".into()),
                table_type: Some("TABLE".into()),
                remarks: None,
            },
            TableRow {
                catalog: Some("cat".into()),
                schema: Some("sch".into()),
                name: Some("b_table".into()),
                table_type: Some("TABLE".into()),
                remarks: None,
            },
        ])
    }
    /// Out of spec order on both the table name and the ordinal position, and
    /// the ordinals are `10, 2, 1` within one table: compared as text those
    /// sort `1, 10, 2`, which is the realistic bug this row set exists to
    /// catch.
    fn columns(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &crate::types::ColumnsQuery<'_>,
    ) -> Result<Vec<ColumnRow>, MockError> {
        let col = |table: &str, name: &str, ordinal: i32| ColumnRow {
            catalog: Some("cat".into()),
            schema: Some("sch".into()),
            table_name: table.into(),
            column_name: name.into(),
            ordinal_position: ordinal,
            ..Default::default()
        };
        Ok(vec![
            col("t_one", "j", 10),
            col("t_one", "b", 2),
            col("t_one", "a", 1),
            col("a_table", "z_first", 1),
        ])
    }
    /// Out of spec order on both `TABLE_NAME` and `KEY_SEQ`.
    fn primary_keys(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &crate::types::PrimaryKeysQuery<'_>,
    ) -> Result<Vec<PrimaryKeyRow>, MockError> {
        let pk = |table: &str, column: &str, key_seq: i16| PrimaryKeyRow {
            catalog: Some("cat".into()),
            schema: Some("sch".into()),
            table_name: table.into(),
            column_name: column.into(),
            key_seq,
            pk_name: None,
        };
        Ok(vec![
            pk("t_two", "x", 1),
            pk("t_one", "c", 3),
            pk("t_one", "a", 1),
            pk("t_one", "b", 2),
        ])
    }
    /// The `PKTABLE_NAME` and `FKTABLE_NAME` orders disagree row for row, so a
    /// test can tell which of the two spec orders was applied. `FK_NAME`
    /// labels each row.
    fn foreign_keys(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &crate::types::ForeignKeysQuery<'_>,
    ) -> Result<Vec<ForeignKeyRow>, MockError> {
        let fk = |pk_table: &str, fk_table: &str, label: &str| ForeignKeyRow {
            pk_catalog: Some("cat".into()),
            pk_schema: Some("sch".into()),
            pk_table_name: pk_table.into(),
            pk_column_name: "id".into(),
            fk_catalog: Some("cat".into()),
            fk_schema: Some("sch".into()),
            fk_table_name: fk_table.into(),
            fk_column_name: "ref_id".into(),
            key_seq: 1,
            fk_name: Some(label.into()),
            ..Default::default()
        };
        Ok(vec![
            fk("p_b", "f_c", "first"),
            fk("p_a", "f_b", "second"),
            fk("p_c", "f_a", "third"),
        ])
    }
    /// Out of spec order on `NON_UNIQUE`, `INDEX_NAME` and `ORDINAL_POSITION`
    /// at once; `COLUMN_NAME` labels each row.
    fn statistics(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &crate::types::StatisticsQuery<'_>,
    ) -> Result<Vec<StatisticsRow>, MockError> {
        let stat = |non_unique: i16, index_name: &str, ordinal: i16, column: &str| StatisticsRow {
            catalog: Some("cat".into()),
            schema: Some("sch".into()),
            table_name: "t".into(),
            non_unique: Some(non_unique),
            index_qualifier: Some("q".into()),
            index_name: Some(index_name.into()),
            index_type: SQL_INDEX_OTHER,
            ordinal_position: Some(ordinal),
            column_name: Some(column.into()),
            ..Default::default()
        };
        Ok(vec![
            stat(NON_UNIQUE_TRUE, "i_u", 1, "d"),
            stat(NON_UNIQUE_FALSE, "i_b", 2, "c"),
            stat(NON_UNIQUE_FALSE, "i_a", 1, "b"),
            stat(NON_UNIQUE_FALSE, "i_b", 1, "a"),
        ])
    }
    /// Out of spec order on `SCOPE`; `COLUMN_NAME` labels each row.
    fn special_columns(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &crate::types::SpecialColumnsQuery<'_>,
    ) -> Result<Vec<SpecialColumnRow>, MockError> {
        let special = |scope: i16, column: &str| SpecialColumnRow {
            scope: Some(scope),
            column_name: column.into(),
            data_type: SqlDataType::INTEGER.0,
            type_name: "INTEGER".into(),
            pseudo_column: Some(SQL_PC_NOT_PSEUDO),
            ..Default::default()
        };
        Ok(vec![
            special(SCOPE_SESSION, "c"),
            special(SCOPE_CURROW, "a"),
            special(SCOPE_TRANSACTION, "b"),
        ])
    }

    /// Out of spec order on `PROCEDURE_NAME`; the catalog and schema are
    /// shared, so that column is the discriminating key.
    fn procedures(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &crate::types::ProceduresQuery<'_>,
    ) -> Result<Vec<ProcedureRow>, MockError> {
        let proc = |name: &str| ProcedureRow {
            catalog: Some("cat".into()),
            schema: Some("sch".into()),
            name: name.into(),
            procedure_type: Some(SQL_PT_PROCEDURE),
            ..Default::default()
        };
        Ok(vec![proc("z_proc"), proc("a_proc"), proc("m_proc")])
    }

    /// Out of spec order on `COLUMN_TYPE`, whose values are `ParamType`
    /// discriminants; `COLUMN_NAME` labels each row. Every row shares a
    /// procedure, so `COLUMN_TYPE` is the discriminating key — and it is an
    /// integer, so a sort comparing it as text would put `ReturnValue` (5)
    /// before `InputOutput` (2) undetected if the values were single digits
    /// only. They are, which is why the labels rather than the values are what
    /// the test reads.
    fn procedure_columns(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &crate::types::ProcedureColumnsQuery<'_>,
    ) -> Result<Vec<ProcedureColumnRow>, MockError> {
        let col = |column_type: ParamType, name: &str| ProcedureColumnRow {
            catalog: Some("cat".into()),
            schema: Some("sch".into()),
            procedure_name: "p".into(),
            column_name: name.into(),
            column_type: column_type as i16,
            data_type: SqlDataType::INTEGER.0,
            type_name: "INTEGER".into(),
            nullable: Nullable::SqlNullable as i16,
            sql_data_type: SqlDataType::INTEGER.0,
            ordinal_position: 1,
            ..Default::default()
        };
        Ok(vec![
            col(ParamType::ReturnValue, "c"),
            col(ParamType::Unknown, "a"),
            col(ParamType::InputOutput, "b"),
        ])
    }

    /// Out of spec order on `COLUMN_NAME` and, within one column, on
    /// `PRIVILEGE`. Two rows share a table *and* a column and differ only in
    /// `PRIVILEGE`, so the fifth sort key is genuinely exercised;
    /// `IS_GRANTABLE` labels each row.
    fn column_privileges(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &crate::types::ColumnPrivilegesQuery<'_>,
    ) -> Result<Vec<ColumnPrivilegeRow>, MockError> {
        let priv_row = |column: &str, privilege: &str, label: &str| ColumnPrivilegeRow {
            catalog: Some("cat".into()),
            schema: Some("sch".into()),
            table_name: "t".into(),
            column_name: column.into(),
            grantor: Some("owner".into()),
            grantee: "user".into(),
            privilege: privilege.into(),
            is_grantable: Some(label.into()),
        };
        Ok(vec![
            priv_row("z_col", "SELECT", "third"),
            priv_row("a_col", "UPDATE", "second"),
            priv_row("a_col", "SELECT", "first"),
        ])
    }

    /// Out of spec order on `PRIVILEGE` and, within one privilege, on
    /// `GRANTEE`. The spec sorts by PRIVILEGE *before* GRANTEE, so the two
    /// `SELECT` rows must come out grantee-ordered while `UPDATE` follows both
    /// regardless of its grantee — which a keys list truncated to the table
    /// trio, or one with the last two keys swapped, gets wrong.
    /// `IS_GRANTABLE` labels each row.
    fn table_privileges(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &crate::types::TablePrivilegesQuery<'_>,
    ) -> Result<Vec<TablePrivilegeRow>, MockError> {
        let priv_row = |privilege: &str, grantee: &str, label: &str| TablePrivilegeRow {
            catalog: Some("cat".into()),
            schema: Some("sch".into()),
            table_name: "t".into(),
            grantor: Some("owner".into()),
            grantee: grantee.into(),
            privilege: privilege.into(),
            is_grantable: Some(label.into()),
        };
        Ok(vec![
            priv_row("UPDATE", "a_user", "third"),
            priv_row("SELECT", "z_user", "second"),
            priv_row("SELECT", "a_user", "first"),
        ])
    }

    /// Deliberately out of order, and distinct from the table names above, so
    /// an `SQL_ALL_CATALOGS` enumeration that fell through to `tables` would
    /// be visible rather than coincidentally right.
    fn catalogs(_: &MockConnection, _: &Self::CancelToken) -> Result<Vec<String>, Self::Error> {
        Ok(vec!["cat_b".into(), "cat_a".into()])
    }
    /// Out of order, for the same reason as [`MockCatalogBackend::catalogs`].
    fn schemas(_: &MockConnection, _: &Self::CancelToken) -> Result<Vec<String>, Self::Error> {
        Ok(vec!["sch_b".into(), "sch_a".into()])
    }

    fn supports_catalogs(_conn: &Self::Connection) -> bool {
        true
    }
    fn supports_schemas(_conn: &Self::Connection) -> bool {
        true
    }
    fn alter_table_support(_conn: &Self::Connection) -> u32 {
        0
    }
    fn outer_join_capabilities(_conn: &Self::Connection) -> u32 {
        0
    }
    fn default_txn_isolation(_conn: &Self::Connection) -> u32 {
        0
    }
    fn txn_isolation_options(_conn: &Self::Connection) -> u32 {
        0
    }

    // Out of order, so the `SQL_ALL_TABLE_TYPES` test sees core's sort rather
    // than the backend's declaration order.
    minimal_capability_decls!(table_types = &["VIEW", "TABLE"]);
}

// ---------------------------------------------------------------------------
// A backend that records the catalog arguments it was given
// ---------------------------------------------------------------------------

/// The arguments one catalog `Backend` method received, by spec argument name.
///
/// `catalog`/`schema`/`table` are `SQLForeignKeys`' *PK* trio; its FK trio
/// lands in the `fk_*` fields.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecordedCatalogArgs {
    pub catalog: Option<String>,
    pub schema: Option<String>,
    pub table: Option<String>,
    /// `SQLProcedures`' and `SQLProcedureColumns`' `ProcName`. Kept apart from
    /// `table` so a test cannot pass by reading the wrong argument.
    pub proc: Option<String>,
    pub column: Option<String>,
    /// `SQLTables`' `TableType`, as the parsed value list core passes down —
    /// not the raw string the application supplied. Empty means no filter.
    pub table_types: Vec<String>,
    pub fk_catalog: Option<String>,
    pub fk_schema: Option<String>,
    pub fk_table: Option<String>,
}

thread_local! {
    /// The arguments of the most recent catalog call on this thread. The test
    /// harness gives each test its own thread, and every test using this mock
    /// makes exactly one catalog call, so last-write-wins needs no reset hook.
    static RECORDED_CATALOG_ARGS: std::cell::RefCell<Option<RecordedCatalogArgs>> =
        const { std::cell::RefCell::new(None) };
}

/// Records what core actually passed down to each catalog method, so a test can
/// assert what `SQL_ATTR_METADATA_ID` normalisation did to the application's
/// arguments before the backend ever saw them.
///
/// It declares `SQL_IC_UPPER` and `"\\"` rather than the minimal
/// `SQL_IC_SENSITIVE` and no escape character: those two are the only inputs
/// normalisation reads, and with the minimal pair it folds nothing and escapes
/// nothing, so every assertion would pass whether core normalised or not.
///
/// It also declares catalogs, schemas and table types, so the `SQL_ALL_*`
/// enumerations can be exercised *with* `METADATA_ID` set — a normalised `"%"`
/// would be escaped to `"\\%"` and stop being the sentinel, which is exactly
/// the ordering bug the enumeration test pins.
pub struct MockCatalogArgsBackend;

impl MockCatalogArgsBackend {
    fn record(args: RecordedCatalogArgs) {
        RECORDED_CATALOG_ARGS.with(|cell| *cell.borrow_mut() = Some(args));
    }

    /// The arguments of the most recent catalog call on this thread, or `None`
    /// if no catalog method was reached — which is itself worth asserting when
    /// a test expects core to answer without the backend.
    pub fn recorded() -> Option<RecordedCatalogArgs> {
        RECORDED_CATALOG_ARGS.with(|cell| cell.borrow().clone())
    }
}

impl Backend for MockCatalogArgsBackend {
    type Connection = MockConnection;
    type Statement = MockStatement;
    type Error = MockError;
    type CancelToken = MockCancelToken;

    fn connect(_: &ConnectParams) -> Result<MockConnection, MockError> {
        Ok(MockConnection)
    }
    fn disconnect(_: &mut MockConnection) -> Result<(), MockError> {
        Ok(())
    }
    fn cancel_token(_conn: &Self::Connection) -> Self::CancelToken {
        MockCancelToken::default()
    }
    fn cancel(token: &Self::CancelToken) -> Result<(), Self::Error> {
        token
            .cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    fn is_cancelled(token: &Self::CancelToken) -> bool {
        token.cancelled.load(std::sync::atomic::Ordering::SeqCst)
    }
    fn exec_direct(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &str,
    ) -> Result<MockStatement, MockError> {
        Err(MockError)
    }
    fn prepare(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &str,
    ) -> Result<MockStatement, MockError> {
        Err(MockError)
    }
    fn execute(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &mut MockStatement,
        _: &[crate::types::ColumnValue],
    ) -> Result<crate::types::ExecuteOutcome, MockError> {
        Err(MockError)
    }
    fn get_info(_: &MockConnection, _: crate::types::InfoType) -> Result<InfoValue, MockError> {
        Err(MockError)
    }
    fn get_functions() -> Cow<'static, [crate::function_id::FunctionId]> {
        Cow::Borrowed(&[])
    }
    fn get_type_info(_conn: &Self::Connection) -> Cow<'static, [TypeInfoRow]> {
        Cow::Borrowed(&[])
    }

    fn tables(
        _: &MockConnection,
        _: &Self::CancelToken,
        query: &crate::types::TablesQuery<'_>,
    ) -> Result<Vec<TableRow>, MockError> {
        Self::record(RecordedCatalogArgs {
            catalog: query.catalog().map(str::to_string),
            schema: query.schema().map(str::to_string),
            table: query.table().map(str::to_string),
            table_types: query.table_types().to_vec(),
            ..Default::default()
        });
        Ok(Vec::new())
    }
    fn columns(
        _: &MockConnection,
        _: &Self::CancelToken,
        query: &crate::types::ColumnsQuery<'_>,
    ) -> Result<Vec<ColumnRow>, MockError> {
        Self::record(RecordedCatalogArgs {
            catalog: query.catalog().map(str::to_string),
            schema: query.schema().map(str::to_string),
            table: query.table().map(str::to_string),
            column: query.column().map(str::to_string),
            ..Default::default()
        });
        Ok(Vec::new())
    }
    fn primary_keys(
        _: &MockConnection,
        _: &Self::CancelToken,
        query: &crate::types::PrimaryKeysQuery<'_>,
    ) -> Result<Vec<PrimaryKeyRow>, MockError> {
        Self::record(RecordedCatalogArgs {
            catalog: query.catalog().map(str::to_string),
            schema: query.schema().map(str::to_string),
            table: query.table().map(str::to_string),
            ..Default::default()
        });
        Ok(Vec::new())
    }
    fn foreign_keys(
        _: &MockConnection,
        _: &Self::CancelToken,
        query: &crate::types::ForeignKeysQuery<'_>,
    ) -> Result<Vec<ForeignKeyRow>, MockError> {
        Self::record(RecordedCatalogArgs {
            catalog: query.pk_catalog().map(str::to_string),
            schema: query.pk_schema().map(str::to_string),
            table: query.pk_table().map(str::to_string),
            fk_catalog: query.fk_catalog().map(str::to_string),
            fk_schema: query.fk_schema().map(str::to_string),
            fk_table: query.fk_table().map(str::to_string),
            ..Default::default()
        });
        Ok(Vec::new())
    }
    fn statistics(
        _: &MockConnection,
        _: &Self::CancelToken,
        query: &crate::types::StatisticsQuery<'_>,
    ) -> Result<Vec<StatisticsRow>, MockError> {
        Self::record(RecordedCatalogArgs {
            catalog: query.catalog().map(str::to_string),
            schema: query.schema().map(str::to_string),
            table: query.table().map(str::to_string),
            ..Default::default()
        });
        Ok(Vec::new())
    }
    fn special_columns(
        _: &MockConnection,
        _: &Self::CancelToken,
        query: &crate::types::SpecialColumnsQuery<'_>,
    ) -> Result<Vec<SpecialColumnRow>, MockError> {
        Self::record(RecordedCatalogArgs {
            catalog: query.catalog().map(str::to_string),
            schema: query.schema().map(str::to_string),
            table: query.table().map(str::to_string),
            ..Default::default()
        });
        Ok(Vec::new())
    }

    fn procedures(
        _: &MockConnection,
        _: &Self::CancelToken,
        query: &crate::types::ProceduresQuery<'_>,
    ) -> Result<Vec<ProcedureRow>, MockError> {
        Self::record(RecordedCatalogArgs {
            catalog: query.catalog().map(str::to_string),
            schema: query.schema().map(str::to_string),
            proc: query.proc_name().map(str::to_string),
            ..Default::default()
        });
        Ok(Vec::new())
    }
    fn procedure_columns(
        _: &MockConnection,
        _: &Self::CancelToken,
        query: &crate::types::ProcedureColumnsQuery<'_>,
    ) -> Result<Vec<ProcedureColumnRow>, MockError> {
        Self::record(RecordedCatalogArgs {
            catalog: query.catalog().map(str::to_string),
            schema: query.schema().map(str::to_string),
            proc: query.proc_name().map(str::to_string),
            column: query.column().map(str::to_string),
            ..Default::default()
        });
        Ok(Vec::new())
    }
    fn column_privileges(
        _: &MockConnection,
        _: &Self::CancelToken,
        query: &crate::types::ColumnPrivilegesQuery<'_>,
    ) -> Result<Vec<ColumnPrivilegeRow>, MockError> {
        Self::record(RecordedCatalogArgs {
            catalog: query.catalog().map(str::to_string),
            schema: query.schema().map(str::to_string),
            table: query.table().map(str::to_string),
            column: query.column().map(str::to_string),
            ..Default::default()
        });
        Ok(Vec::new())
    }
    fn table_privileges(
        _: &MockConnection,
        _: &Self::CancelToken,
        query: &crate::types::TablePrivilegesQuery<'_>,
    ) -> Result<Vec<TablePrivilegeRow>, MockError> {
        Self::record(RecordedCatalogArgs {
            catalog: query.catalog().map(str::to_string),
            schema: query.schema().map(str::to_string),
            table: query.table().map(str::to_string),
            ..Default::default()
        });
        Ok(Vec::new())
    }

    fn catalogs(_: &MockConnection, _: &Self::CancelToken) -> Result<Vec<String>, Self::Error> {
        Ok(vec!["cat_a".into()])
    }
    fn schemas(_: &MockConnection, _: &Self::CancelToken) -> Result<Vec<String>, Self::Error> {
        Ok(vec!["sch_a".into()])
    }

    fn supports_catalogs(_conn: &Self::Connection) -> bool {
        true
    }
    fn supports_schemas(_conn: &Self::Connection) -> bool {
        true
    }
    fn alter_table_support(_conn: &Self::Connection) -> u32 {
        0
    }
    fn outer_join_capabilities(_conn: &Self::Connection) -> u32 {
        0
    }
    fn default_txn_isolation(_conn: &Self::Connection) -> u32 {
        0
    }
    fn txn_isolation_options(_conn: &Self::Connection) -> u32 {
        0
    }

    minimal_capability_decls!(
        table_types = &["TABLE"],
        identifier_case = crate::types::SQL_IC_UPPER,
        search_pattern_escape = "\\"
    );
}

// ---------------------------------------------------------------------------
// A backend that declares functions
// ---------------------------------------------------------------------------

/// Declares a realistic ODBC 3.x function list, so `SQLGetFunctions` actually
/// executes its bitmap and 2.x-mapping arms.
///
/// `MockBackend::get_functions` returns an empty slice, which meant every
/// existing `SQLGetFunctions` test walked a loop with no iterations and no
/// mapping arm ever ran — the gap that let `SQLGetConnectOption` sit at the
/// wrong index in the 2.x array.
pub struct MockFunctionsBackend;

impl Backend for MockFunctionsBackend {
    type Connection = MockConnection;
    type Statement = MockStatement;
    type Error = MockError;
    type CancelToken = MockCancelToken;

    fn connect(_: &ConnectParams) -> Result<MockConnection, MockError> {
        Ok(MockConnection)
    }
    fn disconnect(_: &mut MockConnection) -> Result<(), MockError> {
        Ok(())
    }
    fn cancel_token(_conn: &Self::Connection) -> Self::CancelToken {
        MockCancelToken::default()
    }
    fn cancel(token: &Self::CancelToken) -> Result<(), Self::Error> {
        token
            .cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    fn is_cancelled(token: &Self::CancelToken) -> bool {
        token.cancelled.load(std::sync::atomic::Ordering::SeqCst)
    }
    fn exec_direct(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &str,
    ) -> Result<MockStatement, MockError> {
        Err(MockError)
    }
    fn prepare(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &str,
    ) -> Result<MockStatement, MockError> {
        Ok(MockStatement)
    }
    fn execute(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &mut MockStatement,
        _: &[crate::types::ColumnValue],
    ) -> Result<crate::types::ExecuteOutcome, MockError> {
        Ok(crate::types::ExecuteOutcome::default())
    }
    fn get_info(_: &MockConnection, _: crate::types::InfoType) -> Result<InfoValue, MockError> {
        Err(MockError)
    }
    fn get_functions() -> Cow<'static, [crate::function_id::FunctionId]> {
        use crate::function_id::FunctionId as F;
        Cow::Borrowed(&[
            F::AllocHandle,
            F::FreeHandle,
            F::GetConnectAttr,
            F::SetConnectAttr,
            F::GetStmtAttr,
            F::SetStmtAttr,
            F::GetDiagRec,
            F::EndTran,
            F::ExecDirect,
            F::Fetch,
        ])
    }
    fn get_type_info(_conn: &Self::Connection) -> Cow<'static, [TypeInfoRow]> {
        Cow::Borrowed(&[])
    }
    fn tables(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &crate::types::TablesQuery<'_>,
    ) -> Result<Vec<TableRow>, MockError> {
        Err(MockError)
    }
    fn columns(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &crate::types::ColumnsQuery<'_>,
    ) -> Result<Vec<ColumnRow>, MockError> {
        Err(MockError)
    }

    fn supports_catalogs(_conn: &Self::Connection) -> bool {
        false
    }
    fn supports_schemas(_conn: &Self::Connection) -> bool {
        false
    }
    fn alter_table_support(_conn: &Self::Connection) -> u32 {
        0
    }
    fn outer_join_capabilities(_conn: &Self::Connection) -> u32 {
        0
    }
    fn default_txn_isolation(_conn: &Self::Connection) -> u32 {
        0
    }
    fn txn_isolation_options(_conn: &Self::Connection) -> u32 {
        0
    }

    minimal_capability_decls!();
}

// ---------------------------------------------------------------------------
// A backend whose cursor close fails
// ---------------------------------------------------------------------------

/// A statement whose `close_cursor` fails, standing for a networked data source
/// where closing a partially-read result set is a round trip that can fail.
///
/// It reports one column so `set_result_set` treats it as having an open
/// cursor, which is what makes `SQL_CB_CLOSE` reach `close_cursor` at all.
pub struct MockFailingCloseStatement;

impl StatementBackend for MockFailingCloseStatement {
    type Error = OdbcError;

    fn column_count(&self) -> i16 {
        1
    }

    fn close_cursor(&mut self) -> Result<(), OdbcError> {
        Err(OdbcError::general(
            "mock close_cursor failure",
            crate::types::SqlState::communication_link_failure(),
        ))
    }
}

/// Declares `SQL_CB_CLOSE` and hands out statements that cannot close.
///
/// Exists so a test can prove `SQLEndTran` does not report success when the
/// only thing that closes the cursor under `SQL_CB_CLOSE` has failed.
pub struct MockFailingCloseBackend;

impl Backend for MockFailingCloseBackend {
    type Connection = MockConnection;
    type Statement = MockFailingCloseStatement;
    type Error = OdbcError;
    type CancelToken = MockCancelToken;

    fn connect(_: &ConnectParams) -> Result<MockConnection, OdbcError> {
        Ok(MockConnection)
    }
    fn disconnect(_: &mut MockConnection) -> Result<(), OdbcError> {
        Ok(())
    }
    fn cancel_token(_conn: &Self::Connection) -> Self::CancelToken {
        MockCancelToken::default()
    }
    /// The one mock `cancel` that can actually fail: `should_fail` lets a
    /// test exercise `sql_cancel`'s error-propagation arm, which needs a real
    /// `OdbcError` rather than `MockError`'s collapse to `NotImplemented`.
    fn cancel(token: &Self::CancelToken) -> Result<(), Self::Error> {
        if token.should_fail.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(OdbcError::general(
                "mock backend declined the cancel request",
                crate::types::SqlState::general_error(),
            ));
        }
        token
            .cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    /// Paired with the `cancel` above: a declined cancel leaves `cancelled`
    /// unset, so this reports `false` and the in-flight call keeps its own
    /// SQLSTATE rather than being relabelled `HY008`.
    fn is_cancelled(token: &Self::CancelToken) -> bool {
        token.cancelled.load(std::sync::atomic::Ordering::SeqCst)
    }
    fn exec_direct(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &str,
    ) -> Result<MockFailingCloseStatement, OdbcError> {
        Ok(MockFailingCloseStatement)
    }
    fn prepare(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &str,
    ) -> Result<MockFailingCloseStatement, OdbcError> {
        Ok(MockFailingCloseStatement)
    }
    fn execute(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &mut MockFailingCloseStatement,
        _: &[crate::types::ColumnValue],
    ) -> Result<crate::types::ExecuteOutcome, OdbcError> {
        Ok(crate::types::ExecuteOutcome::default())
    }
    fn get_info(_: &MockConnection, _: crate::types::InfoType) -> Result<InfoValue, OdbcError> {
        Err(OdbcError::NotImplemented {
            feature: "get_info".into(),
        })
    }
    fn get_functions() -> Cow<'static, [crate::function_id::FunctionId]> {
        Cow::Borrowed(&[])
    }
    fn get_type_info(_conn: &Self::Connection) -> Cow<'static, [TypeInfoRow]> {
        Cow::Borrowed(&[])
    }
    fn tables(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &crate::types::TablesQuery<'_>,
    ) -> Result<Vec<TableRow>, OdbcError> {
        Err(OdbcError::NotImplemented {
            feature: "tables".into(),
        })
    }
    fn columns(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &crate::types::ColumnsQuery<'_>,
    ) -> Result<Vec<ColumnRow>, OdbcError> {
        Err(OdbcError::NotImplemented {
            feature: "columns".into(),
        })
    }

    // Must succeed, or the default `NotImplemented` would fail SQLEndTran
    // before it ever reaches the cursor close this mock exists to exercise.
    fn end_tran(_: &MockConnection, _commit: bool) -> Result<(), OdbcError> {
        Ok(())
    }

    fn cursor_commit_behavior() -> crate::types::CursorBehavior {
        crate::types::CursorBehavior::Close
    }
    fn cursor_rollback_behavior() -> crate::types::CursorBehavior {
        crate::types::CursorBehavior::Close
    }

    fn supports_catalogs(_conn: &Self::Connection) -> bool {
        false
    }
    fn supports_schemas(_conn: &Self::Connection) -> bool {
        false
    }
    fn alter_table_support(_conn: &Self::Connection) -> u32 {
        0
    }
    fn outer_join_capabilities(_conn: &Self::Connection) -> u32 {
        0
    }
    fn default_txn_isolation(_conn: &Self::Connection) -> u32 {
        crate::types::SQL_TXN_SERIALIZABLE
    }
    fn txn_isolation_options(_conn: &Self::Connection) -> u32 {
        crate::types::SQL_TXN_SERIALIZABLE
    }

    minimal_capability_decls!();
}

// ---------------------------------------------------------------------------
// MockLongDataBackend — a row whose columns SQLGetData can actually read
// ---------------------------------------------------------------------------

/// The character column [`MockLongDataStatement`] serves, long enough that a
/// small buffer needs several `SQLGetData` calls to drain it.
///
/// Deliberately not a repetition of one character: reassembling it in the wrong
/// order, or dropping a chunk, has to be detectable. It is pure ASCII so that
/// the UTF-8 byte count and the UTF-16 code-unit count agree, which keeps a
/// test's arithmetic the same for `SQL_C_CHAR` and `SQL_C_WCHAR`.
pub const LONG_TEXT: &str = "abcdefghijklmnopqrstuvwxyz0123456789";

/// The binary column [`MockLongDataStatement`] serves, for the `SQL_C_BINARY`
/// chunking path, which reserves no null terminator and so has different
/// arithmetic from the two character paths.
pub const LONG_BYTES: &[u8] = &[
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
];

/// One row with three columns: long text (1), a fixed-width `i32` (2), and long
/// binary (3).
///
/// Exists because no other mock returns row data at all — `MockStatement` takes
/// the trait defaults and answers `NotImplemented` — so nothing could exercise
/// `SQLGetData` past its argument validation, let alone the multi-call loop the
/// spec defines for retrieving variable-length data in parts.
///
/// `get_data` ignores the offset and always returns the whole value: chunking is
/// core's job, and a mock that pre-sliced would be testing the mock. That it can
/// answer for any column in any order, repeatedly, is what the driver contract
/// already requires from `SQL_GD_ANY_COLUMN | SQL_GD_ANY_ORDER`.
#[derive(Default)]
pub struct MockLongDataStatement {
    rows_left: u8,
}

impl StatementBackend for MockLongDataStatement {
    type Error = OdbcError;

    fn column_count(&self) -> i16 {
        3
    }

    fn fetch(&mut self) -> Result<crate::types::FetchResult, OdbcError> {
        if self.rows_left == 0 {
            return Ok(crate::types::FetchResult::NoData);
        }
        self.rows_left -= 1;
        Ok(crate::types::FetchResult::Row)
    }

    fn get_data(
        &mut self,
        col: u16,
        _target_type: crate::types::CDataType,
    ) -> Result<Cow<'_, crate::types::ColumnValue>, OdbcError> {
        match col {
            1 => Ok(Cow::Owned(crate::types::ColumnValue::String(
                LONG_TEXT.to_string(),
            ))),
            2 => Ok(Cow::Owned(crate::types::ColumnValue::I32(4242))),
            3 => Ok(Cow::Owned(crate::types::ColumnValue::Bytes(
                LONG_BYTES.to_vec(),
            ))),
            _ => Err(OdbcError::general(
                format!("no such column: {col}"),
                crate::types::SqlState::invalid_descriptor_index(),
            )),
        }
    }
}

/// Hands out [`MockLongDataStatement`]s, so a test can execute, fetch a row and
/// read columns out of it through the real FFI entry points.
pub struct MockLongDataBackend;

impl Backend for MockLongDataBackend {
    type Connection = MockConnection;
    type Statement = MockLongDataStatement;
    type Error = OdbcError;
    type CancelToken = MockCancelToken;

    fn connect(_: &ConnectParams) -> Result<MockConnection, OdbcError> {
        Ok(MockConnection)
    }
    fn disconnect(_: &mut MockConnection) -> Result<(), OdbcError> {
        Ok(())
    }
    fn cancel_token(_conn: &Self::Connection) -> Self::CancelToken {
        MockCancelToken::default()
    }
    fn exec_direct(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &str,
    ) -> Result<MockLongDataStatement, OdbcError> {
        // Two rows, so a test can prove the chunk position resets across a
        // fetch rather than leaking into the next row.
        Ok(MockLongDataStatement { rows_left: 2 })
    }
    fn prepare(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &str,
    ) -> Result<MockLongDataStatement, OdbcError> {
        Ok(MockLongDataStatement { rows_left: 2 })
    }
    fn execute(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &mut MockLongDataStatement,
        _: &[crate::types::ColumnValue],
    ) -> Result<crate::types::ExecuteOutcome, OdbcError> {
        Ok(crate::types::ExecuteOutcome::default())
    }
    fn get_info(_: &MockConnection, _: crate::types::InfoType) -> Result<InfoValue, OdbcError> {
        Err(OdbcError::NotImplemented {
            feature: "get_info".into(),
        })
    }
    fn get_functions() -> Cow<'static, [crate::function_id::FunctionId]> {
        Cow::Borrowed(&[])
    }
    fn get_type_info(_conn: &Self::Connection) -> Cow<'static, [TypeInfoRow]> {
        Cow::Borrowed(&[])
    }

    /// Describes parameter 1 and declines the rest, so one mock covers both
    /// branches: the backend's answer reaching the application unchanged, and
    /// core's generic `VARCHAR` fallback for a parameter the backend cannot
    /// describe. A mock that answered for every parameter could not show the
    /// fallback is still reachable.
    fn describe_param(
        _conn: &Self::Connection,
        _sql: &str,
        parameter_number: u16,
    ) -> Result<Option<crate::types::ParamDescriptor>, OdbcError> {
        if parameter_number == 1 {
            Ok(Some(
                crate::types::ParamDescriptor::new(crate::types::SqlDataType::DECIMAL)
                    .with_parameter_size(18)
                    .with_decimal_digits(4)
                    .with_nullable(crate::types::Nullable::SqlNoNulls),
            ))
        } else {
            Ok(None)
        }
    }

    fn tables(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &crate::types::TablesQuery<'_>,
    ) -> Result<Vec<TableRow>, OdbcError> {
        Err(OdbcError::NotImplemented {
            feature: "tables".into(),
        })
    }
    fn columns(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &crate::types::ColumnsQuery<'_>,
    ) -> Result<Vec<ColumnRow>, OdbcError> {
        Err(OdbcError::NotImplemented {
            feature: "columns".into(),
        })
    }

    fn cursor_commit_behavior() -> crate::types::CursorBehavior {
        crate::types::CursorBehavior::Close
    }
    fn cursor_rollback_behavior() -> crate::types::CursorBehavior {
        crate::types::CursorBehavior::Close
    }

    fn supports_catalogs(_conn: &Self::Connection) -> bool {
        false
    }
    fn supports_schemas(_conn: &Self::Connection) -> bool {
        false
    }
    fn alter_table_support(_conn: &Self::Connection) -> u32 {
        0
    }
    fn outer_join_capabilities(_conn: &Self::Connection) -> u32 {
        0
    }
    fn default_txn_isolation(_conn: &Self::Connection) -> u32 {
        crate::types::SQL_TXN_SERIALIZABLE
    }
    fn txn_isolation_options(_conn: &Self::Connection) -> u32 {
        crate::types::SQL_TXN_SERIALIZABLE
    }

    minimal_capability_decls!();
}

// ---------------------------------------------------------------------------
// MockRowCountBackend — column and row counts chosen from the SQL text
// ---------------------------------------------------------------------------

/// A statement whose `column_count` and `row_count` are fixed at construction,
/// so one mock covers every case `SQLExecDirect` and `SQLExecute` weigh when
/// deciding `SQL_NO_DATA`.
///
/// No other mock can express them: `MockStatement` takes the trait defaults
/// (`0` columns and `None` rows) and `MockLongDataStatement` is fixed at three
/// columns.
pub struct MockRowCountStatement {
    columns: i16,
    rows: Option<i64>,
}

impl StatementBackend for MockRowCountStatement {
    type Error = OdbcError;

    fn column_count(&self) -> i16 {
        self.columns
    }

    fn row_count(&self) -> Option<i64> {
        self.rows
    }

    fn fetch(&mut self) -> Result<crate::types::FetchResult, OdbcError> {
        Ok(crate::types::FetchResult::NoData)
    }
}

/// Hands out [`MockRowCountStatement`]s shaped by the SQL text, so a test picks
/// a case by the statement it executes rather than by reaching into a handle:
///
/// - text containing `SELECT` — one column, zero rows. A query with an empty
///   result set, which is `SQL_SUCCESS`, not `SQL_NO_DATA`.
/// - text containing `MANY` — no columns, three rows. DML that affected rows.
/// - text containing `UNKNOWN` — no columns, no row count. A backend that
///   cannot say, where `SQL_SUCCESS` stands.
/// - anything else — no columns, zero rows. The searched DML the spec answers
///   with `SQL_NO_DATA`.
pub struct MockRowCountBackend;

fn row_count_statement_for(sql: &str) -> MockRowCountStatement {
    let upper = sql.to_uppercase();
    if upper.contains("SELECT") {
        MockRowCountStatement {
            columns: 1,
            rows: Some(0),
        }
    } else if upper.contains("MANY") {
        MockRowCountStatement {
            columns: 0,
            rows: Some(3),
        }
    } else if upper.contains("UNKNOWN") {
        MockRowCountStatement {
            columns: 0,
            rows: None,
        }
    } else {
        MockRowCountStatement {
            columns: 0,
            rows: Some(0),
        }
    }
}

impl Backend for MockRowCountBackend {
    type Connection = MockConnection;
    type Statement = MockRowCountStatement;
    type Error = OdbcError;
    type CancelToken = MockCancelToken;

    fn connect(_: &ConnectParams) -> Result<MockConnection, OdbcError> {
        Ok(MockConnection)
    }
    fn disconnect(_: &mut MockConnection) -> Result<(), OdbcError> {
        Ok(())
    }
    fn cancel_token(_conn: &Self::Connection) -> Self::CancelToken {
        MockCancelToken::default()
    }
    fn exec_direct(
        _: &MockConnection,
        _: &Self::CancelToken,
        sql: &str,
    ) -> Result<MockRowCountStatement, OdbcError> {
        Ok(row_count_statement_for(sql))
    }
    fn prepare(
        _: &MockConnection,
        _: &Self::CancelToken,
        sql: &str,
    ) -> Result<MockRowCountStatement, OdbcError> {
        Ok(row_count_statement_for(sql))
    }
    fn execute(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &mut MockRowCountStatement,
        _: &[crate::types::ColumnValue],
    ) -> Result<crate::types::ExecuteOutcome, OdbcError> {
        Ok(crate::types::ExecuteOutcome::default())
    }
    fn get_info(_: &MockConnection, _: crate::types::InfoType) -> Result<InfoValue, OdbcError> {
        Err(OdbcError::NotImplemented {
            feature: "get_info".into(),
        })
    }
    fn get_functions() -> Cow<'static, [crate::function_id::FunctionId]> {
        Cow::Borrowed(&[])
    }
    fn get_type_info(_conn: &Self::Connection) -> Cow<'static, [TypeInfoRow]> {
        Cow::Borrowed(&[])
    }
    fn tables(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &crate::types::TablesQuery<'_>,
    ) -> Result<Vec<TableRow>, OdbcError> {
        Err(OdbcError::NotImplemented {
            feature: "tables".into(),
        })
    }
    fn columns(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &crate::types::ColumnsQuery<'_>,
    ) -> Result<Vec<ColumnRow>, OdbcError> {
        Err(OdbcError::NotImplemented {
            feature: "columns".into(),
        })
    }

    fn supports_catalogs(_conn: &Self::Connection) -> bool {
        false
    }
    fn supports_schemas(_conn: &Self::Connection) -> bool {
        false
    }
    fn alter_table_support(_conn: &Self::Connection) -> u32 {
        0
    }
    fn outer_join_capabilities(_conn: &Self::Connection) -> u32 {
        0
    }
    fn default_txn_isolation(_conn: &Self::Connection) -> u32 {
        crate::types::SQL_TXN_SERIALIZABLE
    }
    fn txn_isolation_options(_conn: &Self::Connection) -> u32 {
        crate::types::SQL_TXN_SERIALIZABLE
    }

    minimal_capability_decls!();
}

// ---------------------------------------------------------------------------
// MockRecordingBackend — proves a statement-producing call gets its own token
// ---------------------------------------------------------------------------

/// A backend whose `exec_direct` records the [`MockCancelToken`] it was
/// handed, by setting [`MockCancelToken::saw_execution`].
///
/// Exists to pin the property `Backend::cancel_token`'s doc comment promises
/// a driver author: a statement-producing call receives *this statement's*
/// token, not merely a token, so a backend whose cancellation needs a value
/// only known at execution time (a query id, say) can actually record it
/// there. `MockBackend::exec_direct` cannot stand in for this — it discards
/// every argument and returns `Err` unconditionally.
pub struct MockRecordingBackend;

impl Backend for MockRecordingBackend {
    type Connection = MockConnection;
    type Statement = MockStatement;
    type Error = MockError;
    type CancelToken = MockCancelToken;

    fn connect(_: &ConnectParams) -> Result<MockConnection, MockError> {
        Ok(MockConnection)
    }
    fn disconnect(_: &mut MockConnection) -> Result<(), MockError> {
        Ok(())
    }
    fn cancel_token(_conn: &Self::Connection) -> Self::CancelToken {
        MockCancelToken::default()
    }
    fn cancel(token: &Self::CancelToken) -> Result<(), Self::Error> {
        token
            .cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    fn is_cancelled(token: &Self::CancelToken) -> bool {
        token.cancelled.load(std::sync::atomic::Ordering::SeqCst)
    }
    fn exec_direct(
        _: &MockConnection,
        cancel: &Self::CancelToken,
        _: &str,
    ) -> Result<MockStatement, MockError> {
        cancel
            .saw_execution
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(MockStatement)
    }
    fn prepare(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &str,
    ) -> Result<MockStatement, MockError> {
        Ok(MockStatement)
    }
    fn execute(
        _: &MockConnection,
        cancel: &Self::CancelToken,
        _: &mut MockStatement,
        params: &[crate::types::ColumnValue],
    ) -> Result<crate::types::ExecuteOutcome, MockError> {
        // Recorded so a test can assert what the backend was actually handed:
        // a bound parameter's value is not observable anywhere else, and
        // "the call succeeded" says nothing about which address it read.
        // See `MockCancelToken::executed_params`.
        if let Ok(mut recorded) = cancel.executed_params.lock() {
            *recorded = params.to_vec();
        }
        Ok(crate::types::ExecuteOutcome::default())
    }
    fn get_info(_: &MockConnection, _: crate::types::InfoType) -> Result<InfoValue, MockError> {
        Err(MockError)
    }
    fn get_functions() -> Cow<'static, [crate::function_id::FunctionId]> {
        Cow::Borrowed(&[])
    }
    fn get_type_info(_conn: &Self::Connection) -> Cow<'static, [TypeInfoRow]> {
        Cow::Borrowed(&[])
    }
    fn tables(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &crate::types::TablesQuery<'_>,
    ) -> Result<Vec<TableRow>, MockError> {
        Err(MockError)
    }
    fn columns(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &crate::types::ColumnsQuery<'_>,
    ) -> Result<Vec<ColumnRow>, MockError> {
        Err(MockError)
    }

    fn supports_catalogs(_conn: &Self::Connection) -> bool {
        true
    }
    fn supports_schemas(_conn: &Self::Connection) -> bool {
        true
    }
    fn alter_table_support(_conn: &Self::Connection) -> u32 {
        0
    }
    fn outer_join_capabilities(_conn: &Self::Connection) -> u32 {
        0
    }
    fn default_txn_isolation(_conn: &Self::Connection) -> u32 {
        0
    }
    fn txn_isolation_options(_conn: &Self::Connection) -> u32 {
        0
    }

    minimal_capability_decls!();
}

// ---------------------------------------------------------------------------
// A backend that can be made to fail, and to cancel itself while failing
// ---------------------------------------------------------------------------

thread_local! {
    /// The next backend call on this thread returns `Err`.
    static FAIL_NEXT_CALL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The next backend call signals its own token just before returning.
    static CANCEL_WHILE_RUNNING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The next `StatementBackend::fetch` returns `Err`.
    static FAIL_NEXT_FETCH: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Models "another thread cancelled this call while it was in flight" without
/// needing a second thread.
///
/// The interleaving that produces `HY008` is: a backend call is running, a
/// `SQLCancel` on another thread signals its token, and the call then fails.
/// [`Self::cancel_before_returning`] makes the mock signal its own token from
/// *inside* the call, immediately before returning `Err`, which leaves core
/// facing exactly the same state — a failed call whose token reads cancelled —
/// with none of a real thread's timing nondeterminism.
///
/// The genuinely concurrent path is proved separately by the cross-thread test;
/// this mock is for the per-entry-point coverage, where spinning up a thread per
/// assertion would buy nothing and cost determinism.
///
/// Each switch is one-shot and thread-local: the harness gives every test its
/// own thread, and a switch consumed by the call under test cannot leak into
/// the next one.
pub struct MockCancelAwareBackend;

impl MockCancelAwareBackend {
    /// The next backend call fails.
    pub fn fail_next_execution() {
        FAIL_NEXT_CALL.with(|c| c.set(true));
    }
    /// The next `StatementBackend::fetch` fails.
    pub fn fail_next_fetch() {
        FAIL_NEXT_FETCH.with(|c| c.set(true));
    }
    /// The next backend call signals its own token before returning.
    pub fn cancel_before_returning() {
        CANCEL_WHILE_RUNNING.with(|c| c.set(true));
    }

    /// Consume both call switches, applying the cancel to `token`. Returns
    /// whether the call should fail.
    fn take_call_outcome(token: &MockCancelToken) -> bool {
        if CANCEL_WHILE_RUNNING.with(std::cell::Cell::take) {
            token
                .cancelled
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        FAIL_NEXT_CALL.with(std::cell::Cell::take)
    }
}

/// The statement [`MockCancelAwareBackend`] produces: one row, so a test can
/// reach `SQLFetch` at all, and a one-shot failure switch for it.
#[derive(Debug, Default)]
pub struct MockCancelAwareStatement {
    rows_left: u8,
}

impl StatementBackend for MockCancelAwareStatement {
    type Error = MockError;

    fn column_count(&self) -> i16 {
        1
    }

    fn fetch(&mut self) -> Result<crate::types::FetchResult, Self::Error> {
        if FAIL_NEXT_FETCH.with(std::cell::Cell::take) {
            return Err(MockError);
        }
        if self.rows_left == 0 {
            return Ok(crate::types::FetchResult::NoData);
        }
        self.rows_left -= 1;
        Ok(crate::types::FetchResult::Row)
    }

    fn get_data(
        &mut self,
        _col: u16,
        _target_type: crate::types::CDataType,
    ) -> Result<Cow<'_, crate::types::ColumnValue>, Self::Error> {
        if FAIL_NEXT_FETCH.with(std::cell::Cell::take) {
            return Err(MockError);
        }
        Ok(Cow::Owned(crate::types::ColumnValue::I32(1)))
    }
}

impl Backend for MockCancelAwareBackend {
    type Connection = MockConnection;
    type Statement = MockCancelAwareStatement;
    type Error = MockError;
    type CancelToken = MockCancelToken;

    fn connect(_: &ConnectParams) -> Result<MockConnection, MockError> {
        Ok(MockConnection)
    }
    fn disconnect(_: &mut MockConnection) -> Result<(), MockError> {
        Ok(())
    }
    fn cancel_token(_conn: &Self::Connection) -> Self::CancelToken {
        MockCancelToken::default()
    }
    fn cancel(token: &Self::CancelToken) -> Result<(), Self::Error> {
        token
            .cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    fn is_cancelled(token: &Self::CancelToken) -> bool {
        token.cancelled.load(std::sync::atomic::Ordering::SeqCst)
    }
    fn exec_direct(
        _: &MockConnection,
        cancel: &Self::CancelToken,
        _: &str,
    ) -> Result<MockCancelAwareStatement, MockError> {
        if Self::take_call_outcome(cancel) {
            return Err(MockError);
        }
        Ok(MockCancelAwareStatement { rows_left: 1 })
    }
    fn prepare(
        _: &MockConnection,
        cancel: &Self::CancelToken,
        _: &str,
    ) -> Result<MockCancelAwareStatement, MockError> {
        if Self::take_call_outcome(cancel) {
            return Err(MockError);
        }
        Ok(MockCancelAwareStatement { rows_left: 1 })
    }
    fn execute(
        _: &MockConnection,
        cancel: &Self::CancelToken,
        _: &mut MockCancelAwareStatement,
        _: &[crate::types::ColumnValue],
    ) -> Result<crate::types::ExecuteOutcome, MockError> {
        if Self::take_call_outcome(cancel) {
            return Err(MockError);
        }
        Ok(crate::types::ExecuteOutcome::default())
    }
    fn get_info(_: &MockConnection, _: crate::types::InfoType) -> Result<InfoValue, MockError> {
        Err(MockError)
    }
    fn get_functions() -> Cow<'static, [crate::function_id::FunctionId]> {
        Cow::Borrowed(&[])
    }
    fn get_type_info(_conn: &Self::Connection) -> Cow<'static, [TypeInfoRow]> {
        Cow::Borrowed(&[])
    }
    fn tables(
        _: &MockConnection,
        cancel: &Self::CancelToken,
        _: &crate::types::TablesQuery<'_>,
    ) -> Result<Vec<TableRow>, MockError> {
        if Self::take_call_outcome(cancel) {
            return Err(MockError);
        }
        Ok(Vec::new())
    }
    fn columns(
        _: &MockConnection,
        cancel: &Self::CancelToken,
        _: &crate::types::ColumnsQuery<'_>,
    ) -> Result<Vec<ColumnRow>, MockError> {
        if Self::take_call_outcome(cancel) {
            return Err(MockError);
        }
        Ok(Vec::new())
    }
    /// Always answers `NotImplemented` (which is what `MockError` collapses
    /// to), while still honouring [`Self::cancel_before_returning`].
    ///
    /// That combination is deliberate and is not reachable through the other
    /// methods: `SQLStatistics` converts the backend's error before matching so
    /// it can turn `NotImplemented` into the spec's empty result set, and this
    /// mock lets a test prove that arm survives a signalled token instead of
    /// being relabelled `HY008`.
    fn statistics(
        _: &MockConnection,
        cancel: &Self::CancelToken,
        _: &crate::types::StatisticsQuery<'_>,
    ) -> Result<Vec<StatisticsRow>, MockError> {
        let _ = Self::take_call_outcome(cancel);
        Err(MockError)
    }

    fn supports_catalogs(_conn: &Self::Connection) -> bool {
        false
    }
    fn supports_schemas(_conn: &Self::Connection) -> bool {
        false
    }
    fn alter_table_support(_conn: &Self::Connection) -> u32 {
        0
    }
    fn outer_join_capabilities(_conn: &Self::Connection) -> u32 {
        0
    }
    fn default_txn_isolation(_conn: &Self::Connection) -> u32 {
        0
    }
    fn txn_isolation_options(_conn: &Self::Connection) -> u32 {
        0
    }

    minimal_capability_decls!();
}

// ---------------------------------------------------------------------------
// A backend that blocks inside a call, so another thread can cancel it
// ---------------------------------------------------------------------------

/// Set by [`MockBlockingBackend::exec_direct`] once it is inside the backend
/// call and holding the connection's group lock.
///
/// A plain `static` rather than a `thread_local!`, because the whole point is
/// that two threads observe it. Exactly one test uses this backend, so there is
/// no cross-test interference to guard against — and adding a second test that
/// uses it would need this reset between them.
static BLOCKING_CALL_STARTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Blocks inside `exec_direct` until its cancel token is signalled, so a test
/// can prove the genuine cross-thread path rather than a simulation of it.
///
/// [`MockCancelAwareBackend`] models the same interleaving by having the
/// backend signal its own token, which is deterministic and right for
/// per-entry-point coverage. This one is the real thing: thread A is *inside*
/// the backend holding the connection's group lock when thread B calls
/// `SQLCancel`, which is the only way to exercise `sql_cancel`'s lock-free
/// `try_lock`-failed branch end to end.
pub struct MockBlockingBackend;

impl MockBlockingBackend {
    /// Spin until the blocking call has entered the backend. Bounded, so a
    /// regression that stops `exec_direct` being reached fails the test rather
    /// than hanging CI.
    pub fn wait_until_started() {
        for _ in 0..10_000 {
            if BLOCKING_CALL_STARTED.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        panic!("the blocking backend call never started");
    }
}

impl Backend for MockBlockingBackend {
    type Connection = MockConnection;
    type Statement = MockStatement;
    type Error = MockError;
    type CancelToken = MockCancelToken;

    fn connect(_: &ConnectParams) -> Result<MockConnection, MockError> {
        Ok(MockConnection)
    }
    fn disconnect(_: &mut MockConnection) -> Result<(), MockError> {
        Ok(())
    }
    fn cancel_token(_conn: &Self::Connection) -> Self::CancelToken {
        MockCancelToken::default()
    }
    fn cancel(token: &Self::CancelToken) -> Result<(), Self::Error> {
        token
            .cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    fn is_cancelled(token: &Self::CancelToken) -> bool {
        token.cancelled.load(std::sync::atomic::Ordering::SeqCst)
    }
    /// Announces that it has started, then waits to be cancelled and fails —
    /// which is what a real client library does when a query is aborted
    /// server-side.
    ///
    /// The wait is bounded: on timeout it fails *without* the token signalled,
    /// so the test's `HY008` assertion fails with a useful message instead of
    /// the suite hanging.
    fn exec_direct(
        _: &MockConnection,
        cancel: &Self::CancelToken,
        _: &str,
    ) -> Result<MockStatement, MockError> {
        BLOCKING_CALL_STARTED.store(true, std::sync::atomic::Ordering::SeqCst);
        for _ in 0..10_000 {
            if cancel.cancelled.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        Err(MockError)
    }
    fn prepare(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &str,
    ) -> Result<MockStatement, MockError> {
        Err(MockError)
    }
    fn execute(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &mut MockStatement,
        _: &[crate::types::ColumnValue],
    ) -> Result<crate::types::ExecuteOutcome, MockError> {
        Err(MockError)
    }
    fn get_info(_: &MockConnection, _: crate::types::InfoType) -> Result<InfoValue, MockError> {
        Err(MockError)
    }
    fn get_functions() -> Cow<'static, [crate::function_id::FunctionId]> {
        Cow::Borrowed(&[])
    }
    fn get_type_info(_conn: &Self::Connection) -> Cow<'static, [TypeInfoRow]> {
        Cow::Borrowed(&[])
    }
    fn tables(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &crate::types::TablesQuery<'_>,
    ) -> Result<Vec<TableRow>, MockError> {
        Err(MockError)
    }
    fn columns(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &crate::types::ColumnsQuery<'_>,
    ) -> Result<Vec<ColumnRow>, MockError> {
        Err(MockError)
    }

    fn supports_catalogs(_conn: &Self::Connection) -> bool {
        false
    }
    fn supports_schemas(_conn: &Self::Connection) -> bool {
        false
    }
    fn alter_table_support(_conn: &Self::Connection) -> u32 {
        0
    }
    fn outer_join_capabilities(_conn: &Self::Connection) -> u32 {
        0
    }
    fn default_txn_isolation(_conn: &Self::Connection) -> u32 {
        0
    }
    fn txn_isolation_options(_conn: &Self::Connection) -> u32 {
        0
    }

    minimal_capability_decls!();
}

// ---------------------------------------------------------------------------
// A backend whose *fetch* blocks, for core's query timer at SQLFetch
// ---------------------------------------------------------------------------

/// The statement [`MockFetchTimeoutBackend`] produces: its `fetch` blocks until
/// its cancel token is signalled.
///
/// This is the shape the query timer exists for, moved one call later than
/// [`MockCoreCancelsTimeoutBackend`] puts it. That backend blocks in
/// `exec_direct`, which proves core arms a deadline at a statement-producing
/// call; it cannot prove anything about `SQLFetch`, because its statement
/// returns from `fetch` immediately. A data source that answers with column
/// metadata before computing a row — the case this exists for — inverts those
/// two costs entirely.
///
/// The token is held by `Arc` rather than borrowed: `exec_direct` receives the
/// token core minted for that execution and the statement must observe *that*
/// one later, from a call that no longer has it in hand. `Arc<MockCancelToken>`
/// is therefore the `CancelToken` type here, which is also the aliasing shape
/// AGENTS.md requires of a token that must survive a concurrent free.
pub struct MockFetchTimeoutStatement {
    token: std::sync::Arc<MockCancelToken>,
}

impl StatementBackend for MockFetchTimeoutStatement {
    type Error = MockError;

    fn column_count(&self) -> i16 {
        1
    }

    /// Blocks until cancelled, then fails — a runaway fetch.
    ///
    /// The wait is **bounded**, for the reason
    /// [`MockCoreCancelsTimeoutBackend`]'s `exec_direct` records: if core stops
    /// arming its timer here, nothing signals this token, and an unbounded wait
    /// would turn the test that catches the regression into a hung CI job
    /// instead of a failed assertion. The bound is far above any deadline a
    /// test sets, so it cannot mask a real timeout.
    fn fetch(&mut self) -> Result<crate::types::FetchResult, Self::Error> {
        // Counted rather than clock-measured: `Instant::now` is disallowed
        // crate-wide, and a bound this loose needs no real clock.
        const STEP_MS: u64 = 5;
        const MAX_STEPS: u32 = 6_000; // ~30s, far above any test deadline
        let mut steps = 0;
        while !self
            .token
            .cancelled
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            steps += 1;
            assert!(
                steps < MAX_STEPS,
                "nothing cancelled this fetch — core almost certainly stopped \
                 arming its query timer at SQLFetch",
            );
            std::thread::sleep(std::time::Duration::from_millis(STEP_MS));
        }
        Err(MockError)
    }
}

/// Delegates `SQL_ATTR_QUERY_TIMEOUT` to core and then blocks in `fetch`.
pub struct MockFetchTimeoutBackend;

impl Backend for MockFetchTimeoutBackend {
    type Connection = MockConnection;
    type Statement = MockFetchTimeoutStatement;
    type Error = MockError;
    type CancelToken = std::sync::Arc<MockCancelToken>;

    fn connect(_: &ConnectParams) -> Result<MockConnection, MockError> {
        Ok(MockConnection)
    }
    fn disconnect(_: &mut MockConnection) -> Result<(), MockError> {
        Ok(())
    }
    fn cancel_token(_conn: &Self::Connection) -> Self::CancelToken {
        std::sync::Arc::new(MockCancelToken::default())
    }
    fn cancel(token: &Self::CancelToken) -> Result<(), Self::Error> {
        token
            .cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    fn is_cancelled(token: &Self::CancelToken) -> bool {
        token.cancelled.load(std::sync::atomic::Ordering::SeqCst)
    }
    /// Hands the deadline to core, which is what arms the timer at all.
    fn set_query_timeout(
        _conn: &Self::Connection,
        _seconds: usize,
    ) -> Result<crate::types::QueryTimeout, MockError> {
        Ok(crate::types::QueryTimeout::CoreCancels)
    }
    /// Returns at once — the whole point. The wait is all in `fetch`.
    fn exec_direct(
        _: &MockConnection,
        cancel: &Self::CancelToken,
        _: &str,
    ) -> Result<MockFetchTimeoutStatement, MockError> {
        Ok(MockFetchTimeoutStatement {
            token: std::sync::Arc::clone(cancel),
        })
    }
    fn prepare(
        _: &MockConnection,
        cancel: &Self::CancelToken,
        _: &str,
    ) -> Result<MockFetchTimeoutStatement, MockError> {
        Ok(MockFetchTimeoutStatement {
            token: std::sync::Arc::clone(cancel),
        })
    }
    fn execute(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &mut MockFetchTimeoutStatement,
        _: &[crate::types::ColumnValue],
    ) -> Result<crate::types::ExecuteOutcome, MockError> {
        Ok(crate::types::ExecuteOutcome::default())
    }
    fn get_info(_: &MockConnection, _: crate::types::InfoType) -> Result<InfoValue, MockError> {
        Err(MockError)
    }
    fn get_functions() -> Cow<'static, [crate::function_id::FunctionId]> {
        Cow::Borrowed(&[])
    }
    fn get_type_info(_conn: &Self::Connection) -> Cow<'static, [TypeInfoRow]> {
        Cow::Borrowed(&[])
    }
    fn tables(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &crate::types::TablesQuery<'_>,
    ) -> Result<Vec<TableRow>, MockError> {
        Ok(Vec::new())
    }
    fn columns(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &crate::types::ColumnsQuery<'_>,
    ) -> Result<Vec<ColumnRow>, MockError> {
        Ok(Vec::new())
    }

    fn supports_catalogs(_conn: &Self::Connection) -> bool {
        false
    }
    fn supports_schemas(_conn: &Self::Connection) -> bool {
        false
    }
    fn alter_table_support(_conn: &Self::Connection) -> u32 {
        0
    }
    fn outer_join_capabilities(_conn: &Self::Connection) -> u32 {
        0
    }
    fn default_txn_isolation(_conn: &Self::Connection) -> u32 {
        0
    }
    fn txn_isolation_options(_conn: &Self::Connection) -> u32 {
        0
    }

    minimal_capability_decls!();
}

// ---------------------------------------------------------------------------
// A backend whose describe_col fails, for SQLDescribeCol / SQLColAttribute
// ---------------------------------------------------------------------------

/// A statement whose `describe_col` fails with a real, specific SQLSTATE.
///
/// `type Error = OdbcError`, and that is the whole point rather than a
/// convenience. `MockError` converts to `OdbcError::NotImplemented` by design,
/// so a mock built on it sends core down its *unimplemented* branch instead of
/// its "this genuinely failed" branch — a test written against one would assert
/// nothing about whether the backend's SQLSTATE survives.
///
/// `column_count` is **2, and that is load-bearing**: core range-checks the
/// column number before calling this method, so a test asking about column 1 or
/// 2 reaches the backend while one asking about column 3 does not. A mock
/// reporting 0 columns would short-circuit every call to `07009` and make the
/// whole group pass vacuously, against the old code as readily as the new.
pub struct MockFailingDescribeStatement;

impl StatementBackend for MockFailingDescribeStatement {
    type Error = OdbcError;

    fn column_count(&self) -> i16 {
        2
    }

    fn describe_col(
        &self,
        _col: u16,
    ) -> Result<Cow<'_, crate::types::ColumnDescriptor>, OdbcError> {
        Err(OdbcError::general(
            "mock describe_col failure",
            crate::types::SqlState::communication_link_failure(),
        ))
    }
}

/// Hands out statements that cannot be described.
///
/// Exists so a test can prove `SQLDescribeColW` and `SQLColAttributeW` report
/// the backend's own failure rather than overwriting it with `07009` "column
/// number out of range" — which is what both did before, for every failure
/// whatever its cause.
pub struct MockFailingDescribeBackend;

impl Backend for MockFailingDescribeBackend {
    type Connection = MockConnection;
    type Statement = MockFailingDescribeStatement;
    type Error = OdbcError;
    type CancelToken = MockCancelToken;

    fn connect(_: &ConnectParams) -> Result<MockConnection, OdbcError> {
        Ok(MockConnection)
    }
    fn disconnect(_: &mut MockConnection) -> Result<(), OdbcError> {
        Ok(())
    }
    fn cancel_token(_conn: &Self::Connection) -> Self::CancelToken {
        MockCancelToken::default()
    }
    fn cancel(token: &Self::CancelToken) -> Result<(), Self::Error> {
        token
            .cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    fn is_cancelled(token: &Self::CancelToken) -> bool {
        token.cancelled.load(std::sync::atomic::Ordering::SeqCst)
    }
    fn exec_direct(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &str,
    ) -> Result<MockFailingDescribeStatement, OdbcError> {
        Ok(MockFailingDescribeStatement)
    }
    fn prepare(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &str,
    ) -> Result<MockFailingDescribeStatement, OdbcError> {
        Ok(MockFailingDescribeStatement)
    }
    fn execute(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &mut MockFailingDescribeStatement,
        _: &[crate::types::ColumnValue],
    ) -> Result<crate::types::ExecuteOutcome, OdbcError> {
        Ok(crate::types::ExecuteOutcome::default())
    }
    fn get_info(_: &MockConnection, _: crate::types::InfoType) -> Result<InfoValue, OdbcError> {
        Err(OdbcError::NotImplemented {
            feature: "get_info".into(),
        })
    }
    fn get_functions() -> Cow<'static, [crate::function_id::FunctionId]> {
        Cow::Borrowed(&[])
    }
    fn get_type_info(_conn: &Self::Connection) -> Cow<'static, [TypeInfoRow]> {
        Cow::Borrowed(&[])
    }
    fn tables(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &crate::types::TablesQuery<'_>,
    ) -> Result<Vec<TableRow>, OdbcError> {
        Ok(Vec::new())
    }
    fn columns(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &crate::types::ColumnsQuery<'_>,
    ) -> Result<Vec<ColumnRow>, OdbcError> {
        Ok(Vec::new())
    }

    fn supports_catalogs(_conn: &Self::Connection) -> bool {
        false
    }
    fn supports_schemas(_conn: &Self::Connection) -> bool {
        false
    }
    fn alter_table_support(_conn: &Self::Connection) -> u32 {
        0
    }
    fn outer_join_capabilities(_conn: &Self::Connection) -> u32 {
        0
    }
    fn default_txn_isolation(_conn: &Self::Connection) -> u32 {
        0
    }
    fn txn_isolation_options(_conn: &Self::Connection) -> u32 {
        0
    }

    minimal_capability_decls!();
}

// ---------------------------------------------------------------------------
// A backend whose column_count is negative, for the u16 narrowing regression
// (Task 2.10)
// ---------------------------------------------------------------------------

/// A statement whose `column_count` cannot be represented in `u16`.
///
/// `StatementBackend::column_count` returns `i16`, so this is the *only* way
/// `u16::try_from` on it can fail: a positive `i16` always fits, since its
/// max (32 767) is below `u16::MAX` (65 535). A backend cannot report "too
/// many columns" through this method at all — it can only report a count
/// that is negative, which is what this mock does.
///
/// `describe_col` succeeds for any column, so a test can tell "core's range
/// check wrongly rejected column 1" (07009, `describe_col` never reached)
/// apart from "the call reached the backend and got an answer"
/// (`SQL_SUCCESS`).
pub struct MockNegativeColumnCountStatement;

impl StatementBackend for MockNegativeColumnCountStatement {
    type Error = OdbcError;

    fn column_count(&self) -> i16 {
        -1
    }

    fn describe_col(
        &self,
        _col: u16,
    ) -> Result<Cow<'_, crate::types::ColumnDescriptor>, OdbcError> {
        Ok(Cow::Owned(crate::types::ColumnDescriptor::default()))
    }
}

/// Hands out statements whose reported column count does not fit `u16`.
///
/// Exists to prove `u16::try_from(column_count).unwrap_or(0)` — which
/// collapses a count `u16` cannot hold to zero — does not turn "the count is
/// unrepresentable" into "reject every column, including valid ones." The
/// fix saturates up instead, so column 1 must still reach `describe_col`.
pub struct MockNegativeColumnCountBackend;

impl Backend for MockNegativeColumnCountBackend {
    type Connection = MockConnection;
    type Statement = MockNegativeColumnCountStatement;
    type Error = OdbcError;
    type CancelToken = MockCancelToken;

    fn connect(_: &ConnectParams) -> Result<MockConnection, OdbcError> {
        Ok(MockConnection)
    }
    fn disconnect(_: &mut MockConnection) -> Result<(), OdbcError> {
        Ok(())
    }
    fn cancel_token(_conn: &Self::Connection) -> Self::CancelToken {
        MockCancelToken::default()
    }
    fn cancel(token: &Self::CancelToken) -> Result<(), Self::Error> {
        token
            .cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    fn is_cancelled(token: &Self::CancelToken) -> bool {
        token.cancelled.load(std::sync::atomic::Ordering::SeqCst)
    }
    fn exec_direct(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &str,
    ) -> Result<MockNegativeColumnCountStatement, OdbcError> {
        Ok(MockNegativeColumnCountStatement)
    }
    fn prepare(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &str,
    ) -> Result<MockNegativeColumnCountStatement, OdbcError> {
        Ok(MockNegativeColumnCountStatement)
    }
    fn execute(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &mut MockNegativeColumnCountStatement,
        _: &[crate::types::ColumnValue],
    ) -> Result<crate::types::ExecuteOutcome, OdbcError> {
        Ok(crate::types::ExecuteOutcome::default())
    }
    fn get_info(_: &MockConnection, _: crate::types::InfoType) -> Result<InfoValue, OdbcError> {
        Err(OdbcError::NotImplemented {
            feature: "get_info".into(),
        })
    }
    fn get_functions() -> Cow<'static, [crate::function_id::FunctionId]> {
        Cow::Borrowed(&[])
    }
    fn get_type_info(_conn: &Self::Connection) -> Cow<'static, [TypeInfoRow]> {
        Cow::Borrowed(&[])
    }
    fn tables(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &crate::types::TablesQuery<'_>,
    ) -> Result<Vec<TableRow>, OdbcError> {
        Ok(Vec::new())
    }
    fn columns(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &crate::types::ColumnsQuery<'_>,
    ) -> Result<Vec<ColumnRow>, OdbcError> {
        Ok(Vec::new())
    }

    fn supports_catalogs(_conn: &Self::Connection) -> bool {
        false
    }
    fn supports_schemas(_conn: &Self::Connection) -> bool {
        false
    }
    fn alter_table_support(_conn: &Self::Connection) -> u32 {
        0
    }
    fn outer_join_capabilities(_conn: &Self::Connection) -> u32 {
        0
    }
    fn default_txn_isolation(_conn: &Self::Connection) -> u32 {
        0
    }
    fn txn_isolation_options(_conn: &Self::Connection) -> u32 {
        0
    }

    minimal_capability_decls!();
}

// ---------------------------------------------------------------------------
// A backend whose describe_col panics, for SQLCopyDesc phase-one panic safety
// ---------------------------------------------------------------------------

/// A statement whose `describe_col` panics instead of returning an error.
///
/// `column_count` is 1 — enough to make `snapshot_ird`'s loop
/// (`src/handles/scope.rs`) call `describe_col` at all, which is the one
/// driver-author call `SQLCopyDesc`'s phase one runs before phase two's
/// `panic_safe` is even reached.
pub struct MockPanickingDescribeStatement;

impl StatementBackend for MockPanickingDescribeStatement {
    type Error = OdbcError;

    fn column_count(&self) -> i16 {
        1
    }

    fn describe_col(
        &self,
        _col: u16,
    ) -> Result<Cow<'_, crate::types::ColumnDescriptor>, OdbcError> {
        panic!("mock describe_col panic");
    }
}

/// Hands out statements whose column metadata cannot be described without
/// panicking.
///
/// The `MockFailingDescribeBackend` sibling above proves core surfaces a
/// backend's own *error*; this one proves core survives a backend's *panic*
/// — the distinction `SQLCopyDesc`'s phase one needed a guard for.
pub struct MockPanickingDescribeBackend;

impl Backend for MockPanickingDescribeBackend {
    type Connection = MockConnection;
    type Statement = MockPanickingDescribeStatement;
    type Error = OdbcError;
    type CancelToken = MockCancelToken;

    fn connect(_: &ConnectParams) -> Result<MockConnection, OdbcError> {
        Ok(MockConnection)
    }
    fn disconnect(_: &mut MockConnection) -> Result<(), OdbcError> {
        Ok(())
    }
    fn cancel_token(_conn: &Self::Connection) -> Self::CancelToken {
        MockCancelToken::default()
    }
    fn cancel(token: &Self::CancelToken) -> Result<(), Self::Error> {
        token
            .cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    fn is_cancelled(token: &Self::CancelToken) -> bool {
        token.cancelled.load(std::sync::atomic::Ordering::SeqCst)
    }
    fn exec_direct(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &str,
    ) -> Result<MockPanickingDescribeStatement, OdbcError> {
        Ok(MockPanickingDescribeStatement)
    }
    fn prepare(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &str,
    ) -> Result<MockPanickingDescribeStatement, OdbcError> {
        Ok(MockPanickingDescribeStatement)
    }
    fn execute(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &mut MockPanickingDescribeStatement,
        _: &[crate::types::ColumnValue],
    ) -> Result<crate::types::ExecuteOutcome, OdbcError> {
        Ok(crate::types::ExecuteOutcome::default())
    }
    fn get_info(_: &MockConnection, _: crate::types::InfoType) -> Result<InfoValue, OdbcError> {
        Err(OdbcError::NotImplemented {
            feature: "get_info".into(),
        })
    }
    fn get_functions() -> Cow<'static, [crate::function_id::FunctionId]> {
        Cow::Borrowed(&[])
    }
    fn get_type_info(_conn: &Self::Connection) -> Cow<'static, [TypeInfoRow]> {
        Cow::Borrowed(&[])
    }
    fn tables(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &crate::types::TablesQuery<'_>,
    ) -> Result<Vec<TableRow>, OdbcError> {
        Ok(Vec::new())
    }
    fn columns(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &crate::types::ColumnsQuery<'_>,
    ) -> Result<Vec<ColumnRow>, OdbcError> {
        Ok(Vec::new())
    }

    fn supports_catalogs(_conn: &Self::Connection) -> bool {
        false
    }
    fn supports_schemas(_conn: &Self::Connection) -> bool {
        false
    }
    fn alter_table_support(_conn: &Self::Connection) -> u32 {
        0
    }
    fn outer_join_capabilities(_conn: &Self::Connection) -> u32 {
        0
    }
    fn default_txn_isolation(_conn: &Self::Connection) -> u32 {
        0
    }
    fn txn_isolation_options(_conn: &Self::Connection) -> u32 {
        0
    }

    minimal_capability_decls!();
}

// ---------------------------------------------------------------------------
// A backend that rejects an unknown catalog, for 3D000
// ---------------------------------------------------------------------------

/// Rejects any catalog but `"good"`, with the spec's `3D000`.
///
/// `type Error = OdbcError` so the rejection is a genuine failure rather than
/// the `NotImplemented` a `MockError` collapses to — which core treats as "this
/// backend has no catalogs" and reports as `HYC00`, a different answer entirely.
///
/// Accepting *one* name matters as much as rejecting the rest: a mock that
/// failed unconditionally could not tell "core propagated the backend's
/// verdict" apart from "core rejects every catalog", and the success case is
/// what proves core stores the value only when the data source agreed to it.
pub struct MockCatalogRejectingBackend;

impl Backend for MockCatalogRejectingBackend {
    type Connection = MockConnection;
    type Statement = MockStatement;
    type Error = OdbcError;
    type CancelToken = MockCancelToken;

    fn connect(_: &ConnectParams) -> Result<MockConnection, OdbcError> {
        Ok(MockConnection)
    }
    fn disconnect(_: &mut MockConnection) -> Result<(), OdbcError> {
        Ok(())
    }
    fn cancel_token(_conn: &Self::Connection) -> Self::CancelToken {
        MockCancelToken::default()
    }
    fn set_current_catalog(_conn: &Self::Connection, catalog: &str) -> Result<(), OdbcError> {
        if catalog == "good" {
            Ok(())
        } else {
            Err(OdbcError::general(
                format!("no such catalog: {catalog}"),
                crate::types::SqlState::invalid_catalog_name(),
            ))
        }
    }
    fn exec_direct(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &str,
    ) -> Result<MockStatement, OdbcError> {
        Ok(MockStatement)
    }
    fn prepare(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &str,
    ) -> Result<MockStatement, OdbcError> {
        Ok(MockStatement)
    }
    fn execute(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &mut MockStatement,
        _: &[crate::types::ColumnValue],
    ) -> Result<crate::types::ExecuteOutcome, OdbcError> {
        Ok(crate::types::ExecuteOutcome::default())
    }
    fn get_info(_: &MockConnection, _: crate::types::InfoType) -> Result<InfoValue, OdbcError> {
        Err(OdbcError::NotImplemented {
            feature: "get_info".into(),
        })
    }
    fn get_functions() -> Cow<'static, [crate::function_id::FunctionId]> {
        Cow::Borrowed(&[])
    }
    fn get_type_info(_conn: &Self::Connection) -> Cow<'static, [TypeInfoRow]> {
        Cow::Borrowed(&[])
    }
    fn tables(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &crate::types::TablesQuery<'_>,
    ) -> Result<Vec<TableRow>, OdbcError> {
        Ok(Vec::new())
    }
    fn columns(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &crate::types::ColumnsQuery<'_>,
    ) -> Result<Vec<ColumnRow>, OdbcError> {
        Ok(Vec::new())
    }

    fn supports_catalogs(_conn: &Self::Connection) -> bool {
        true
    }
    fn supports_schemas(_conn: &Self::Connection) -> bool {
        false
    }
    fn alter_table_support(_conn: &Self::Connection) -> u32 {
        0
    }
    fn outer_join_capabilities(_conn: &Self::Connection) -> u32 {
        0
    }
    fn default_txn_isolation(_conn: &Self::Connection) -> u32 {
        0
    }
    fn txn_isolation_options(_conn: &Self::Connection) -> u32 {
        0
    }

    minimal_capability_decls!();
}

// ---------------------------------------------------------------------------
// MockBrowseBackend — requires one attribute, so SQLBrowseConnectW actually
// reaches its SQL_NEED_DATA branch
// ---------------------------------------------------------------------------

/// A backend that will not connect until the connection string carries `UID`.
///
/// Every other mock leaves `browse_connect_attrs` defaulted to an empty slice,
/// which makes the very first `SQLBrowseConnectW` call connect outright. None
/// of them can therefore reach the `SQL_NEED_DATA` branch, and none can leave a
/// half-finished browse on a handle — which is the state the browse-cancellation
/// and browse-hygiene tests are about.
pub struct MockBrowseBackend;

impl Backend for MockBrowseBackend {
    type Connection = MockConnection;
    type Statement = MockStatement;
    type Error = MockError;
    type CancelToken = MockCancelToken;

    /// `Cow::Owned`, not `Cow::Borrowed(&[Cow::Borrowed("uid")])`: a slice
    /// literal holding a `Cow` cannot be promoted to `'static`, because `Cow`
    /// has a `Drop` impl. The defaulted method gets away with `Cow::Borrowed`
    /// only because its slice is empty.
    fn browse_connect_attrs() -> Cow<'static, [Cow<'static, str>]> {
        Cow::Owned(vec![Cow::Borrowed("uid")])
    }

    fn connect(_: &ConnectParams) -> Result<MockConnection, MockError> {
        Ok(MockConnection)
    }
    fn disconnect(_: &mut MockConnection) -> Result<(), MockError> {
        Ok(())
    }
    fn cancel_token(_conn: &Self::Connection) -> Self::CancelToken {
        MockCancelToken::default()
    }
    fn exec_direct(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &str,
    ) -> Result<MockStatement, MockError> {
        Err(MockError)
    }
    fn prepare(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &str,
    ) -> Result<MockStatement, MockError> {
        Err(MockError)
    }
    fn execute(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &mut MockStatement,
        _: &[crate::types::ColumnValue],
    ) -> Result<crate::types::ExecuteOutcome, MockError> {
        Err(MockError)
    }
    fn get_info(_: &MockConnection, _: crate::types::InfoType) -> Result<InfoValue, MockError> {
        Err(MockError)
    }
    fn get_functions() -> Cow<'static, [crate::function_id::FunctionId]> {
        Cow::Borrowed(&[])
    }
    fn get_type_info(_conn: &Self::Connection) -> Cow<'static, [TypeInfoRow]> {
        Cow::Borrowed(&[])
    }
    fn tables(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &crate::types::TablesQuery<'_>,
    ) -> Result<Vec<TableRow>, MockError> {
        Err(MockError)
    }
    fn columns(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: &crate::types::ColumnsQuery<'_>,
    ) -> Result<Vec<ColumnRow>, MockError> {
        Err(MockError)
    }

    fn supports_catalogs(_conn: &Self::Connection) -> bool {
        false
    }
    fn supports_schemas(_conn: &Self::Connection) -> bool {
        false
    }
    fn alter_table_support(_conn: &Self::Connection) -> u32 {
        0
    }
    fn outer_join_capabilities(_conn: &Self::Connection) -> u32 {
        0
    }
    fn default_txn_isolation(_conn: &Self::Connection) -> u32 {
        0
    }
    fn txn_isolation_options(_conn: &Self::Connection) -> u32 {
        0
    }

    minimal_capability_decls!();
}

// ---------------------------------------------------------------------------
// MockPrompterBackend — declares a `Prompter`, and records whether one arrived
// ---------------------------------------------------------------------------

/// A [`Prompter`](crate::prompt::Prompter) that records what it was shown.
///
/// Recording rather than inert: a prompter that did nothing could not tell
/// "core handed the backend a live prompter" apart from "core handed it one it
/// could not call".
#[derive(Default)]
pub struct RecordingPrompter {
    pub urls: crate::sync::Mutex<Vec<String>>,
}

impl crate::prompt::Prompter for RecordingPrompter {
    fn present_url(&self, url: &str) -> Result<(), OdbcError> {
        let mut urls = self.urls.lock().map_err(|_| {
            OdbcError::general(
                "prompter mutex poisoned",
                crate::types::SqlState::general_error(),
            )
        })?;
        urls.push(url.to_owned());
        Ok(())
    }
}

/// What `Backend::connect` observed about the prompter core handed it.
pub struct MockPrompterConnection {
    /// Whether [`ConnectParams::prompter`] was `Some` at the moment
    /// `Backend::connect` ran. Captured there because that call is the only
    /// place a backend ever sees it.
    pub saw_prompter: bool,
    /// The URL the prompter was actually driven with, if `connect` called it.
    /// Proves the value core passed is a working prompter and not just a
    /// non-`None` pointer.
    pub presented: Option<String>,
}

/// A backend that declares a real prompter.
///
/// Its `connect` presents a fixed URL through whatever prompter it was given,
/// so a test can assert on both the gate (`saw_prompter`) and the fact that the
/// thing passed through is callable (`presented`).
pub struct MockPrompterBackend;

impl Backend for MockPrompterBackend {
    type Connection = MockPrompterConnection;
    type Statement = MockStatement;
    type Error = MockError;
    type CancelToken = MockCancelToken;

    fn prompter() -> Option<std::sync::Arc<dyn crate::prompt::Prompter>> {
        Some(std::sync::Arc::new(RecordingPrompter::default()))
    }

    fn connect(params: &ConnectParams) -> Result<MockPrompterConnection, MockError> {
        let presented = params.prompter().map(|p| {
            let url = "https://example.invalid/oauth";
            p.present_url(url).expect("RecordingPrompter never fails");
            url.to_owned()
        });
        Ok(MockPrompterConnection {
            saw_prompter: params.prompter().is_some(),
            presented,
        })
    }
    fn disconnect(_: &mut MockPrompterConnection) -> Result<(), MockError> {
        Ok(())
    }
    fn cancel_token(_conn: &Self::Connection) -> Self::CancelToken {
        MockCancelToken::default()
    }
    fn exec_direct(
        _: &Self::Connection,
        _: &Self::CancelToken,
        _: &str,
    ) -> Result<MockStatement, MockError> {
        Err(MockError)
    }
    fn prepare(
        _: &Self::Connection,
        _: &Self::CancelToken,
        _: &str,
    ) -> Result<MockStatement, MockError> {
        Err(MockError)
    }
    fn execute(
        _: &Self::Connection,
        _: &Self::CancelToken,
        _: &mut MockStatement,
        _: &[crate::types::ColumnValue],
    ) -> Result<crate::types::ExecuteOutcome, MockError> {
        Err(MockError)
    }
    fn get_info(_: &Self::Connection, _: crate::types::InfoType) -> Result<InfoValue, MockError> {
        Err(MockError)
    }
    fn get_functions() -> Cow<'static, [crate::function_id::FunctionId]> {
        Cow::Borrowed(&[])
    }
    fn get_type_info(_conn: &Self::Connection) -> Cow<'static, [TypeInfoRow]> {
        Cow::Borrowed(&[])
    }
    fn tables(
        _: &Self::Connection,
        _: &Self::CancelToken,
        _: &crate::types::TablesQuery<'_>,
    ) -> Result<Vec<TableRow>, MockError> {
        Err(MockError)
    }
    fn columns(
        _: &Self::Connection,
        _: &Self::CancelToken,
        _: &crate::types::ColumnsQuery<'_>,
    ) -> Result<Vec<ColumnRow>, MockError> {
        Err(MockError)
    }

    fn supports_catalogs(_conn: &Self::Connection) -> bool {
        false
    }
    fn supports_schemas(_conn: &Self::Connection) -> bool {
        false
    }
    fn alter_table_support(_conn: &Self::Connection) -> u32 {
        0
    }
    fn outer_join_capabilities(_conn: &Self::Connection) -> u32 {
        0
    }
    fn default_txn_isolation(_conn: &Self::Connection) -> u32 {
        0
    }
    fn txn_isolation_options(_conn: &Self::Connection) -> u32 {
        0
    }

    minimal_capability_decls!();
}
