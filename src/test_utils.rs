//! Shared test infrastructure for stackable-odbc-core.
//!
//! Provides `MockBackend` (connect succeeds) and `MockFailBackend` (connect fails)
//! so test modules don't each need their own copy.

use std::borrow::Cow;
use std::ffi::c_void;

use odbc_sys::HandleType;

use crate::backend::{Backend, StatementBackend};
use crate::errors::OdbcError;
use crate::ffi::handle::{sql_alloc_handle, sql_free_handle};
use crate::types::{ConnectParams, InfoValue, Nullable, SqlReturn, TypeInfoRow};

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
#[derive(Debug, Default)]
pub struct MockCancelToken {
    pub cancelled: std::sync::atomic::AtomicBool,
    pub should_fail: std::sync::atomic::AtomicBool,
    pub saw_execution: std::sync::atomic::AtomicBool,
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

/// The eight required capability declarations, for a mock that stands for a
/// minimal data source and whose test does not care about them.
///
/// Every value is the *least* capable one the spec defines, which for four of
/// them happens to be `0`. That is deliberate: `0` is a real claim for these
/// info types, so a mock that means "minimal" should say so explicitly rather
/// than have core assume it — which is the whole point of the methods being
/// required. Expand this in a mock that is actually testing one of them.
///
/// `minimal_capability_decls!(keywords = <slice>)` keeps every other value
/// minimal but states a reserved-word list, for the mocks that exist only to
/// test the `SQL_KEYWORDS` subtraction.
macro_rules! minimal_capability_decls {
    () => {
        minimal_capability_decls!(keywords = &[]);
    };
    (keywords = $keywords:expr) => {
        fn keywords(_conn: &Self::Connection) -> Cow<'static, [Cow<'static, str>]> {
            let list: &'static [&'static str] = $keywords;
            Cow::Owned(list.iter().map(|s| Cow::Borrowed(*s)).collect())
        }
        fn group_by(_conn: &Self::Connection) -> u16 {
            crate::types::SQL_GB_NOT_SUPPORTED
        }
        fn null_collation(_conn: &Self::Connection) -> u16 {
            crate::types::SQL_NC_HIGH
        }
        // No "minimal" value exists: 0 is not a legal SQL_IDENTIFIER_CASE.
        fn identifier_case(_conn: &Self::Connection) -> u16 {
            crate::types::SQL_IC_SENSITIVE
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
            Cow::Borrowed("")
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
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<MockStatement, MockError> {
        Err(MockError)
    }
    fn columns(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<MockStatement, MockError> {
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
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<MockStatement, MockError> {
        Err(MockError)
    }
    fn columns(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<MockStatement, MockError> {
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
                _: Option<&str>,
                _: Option<&str>,
                _: Option<&str>,
                _: Option<&str>,
            ) -> Result<MockStatement, MockError> {
                Err(MockError)
            }
            fn columns(
                _: &MockConnection,
                _: &Self::CancelToken,
                _: Option<&str>,
                _: Option<&str>,
                _: Option<&str>,
                _: Option<&str>,
            ) -> Result<MockStatement, MockError> {
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
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<MockStatement, MockError> {
        Err(MockError)
    }
    fn columns(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<MockStatement, MockError> {
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
                _: Option<&str>,
                _: Option<&str>,
                _: Option<&str>,
                _: Option<&str>,
            ) -> Result<MockStatement, MockError> {
                Err(MockError)
            }
            fn columns(
                _: &MockIsolationConnection,
                _: &Self::CancelToken,
                _: Option<&str>,
                _: Option<&str>,
                _: Option<&str>,
                _: Option<&str>,
            ) -> Result<MockStatement, MockError> {
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
                _: Option<&str>,
                _: Option<&str>,
                _: Option<&str>,
                _: Option<&str>,
            ) -> Result<MockStatement, OdbcError> {
                Err(OdbcError::NotImplemented {
                    feature: "mock txn backend".into(),
                })
            }
            fn columns(
                _: &MockTxnConnection,
                _: &Self::CancelToken,
                _: Option<&str>,
                _: Option<&str>,
                _: Option<&str>,
                _: Option<&str>,
            ) -> Result<MockStatement, OdbcError> {
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
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<MockStatement, MockError> {
        Err(MockError)
    }
    fn columns(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<MockStatement, MockError> {
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
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<MockStatement, MockError> {
        Err(MockError)
    }
    fn columns(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<MockStatement, MockError> {
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
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<MockFailingCloseStatement, OdbcError> {
        Err(OdbcError::NotImplemented {
            feature: "tables".into(),
        })
    }
    fn columns(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<MockFailingCloseStatement, OdbcError> {
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
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<MockLongDataStatement, OdbcError> {
        Err(OdbcError::NotImplemented {
            feature: "tables".into(),
        })
    }
    fn columns(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<MockLongDataStatement, OdbcError> {
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
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<MockStatement, MockError> {
        Err(MockError)
    }
    fn columns(
        _: &MockConnection,
        _: &Self::CancelToken,
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<MockStatement, MockError> {
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
