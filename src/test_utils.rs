//! Shared test infrastructure for stackable-odbc-core.
//!
//! Provides `MockBackend` (connect succeeds) and `MockFailBackend` (connect fails)
//! so test modules don't each need their own copy.

use crate::backend::{Backend, StatementBackend};
use crate::errors::OdbcError;
use crate::types::{ConnectParams, InfoValue, TypeInfoRow};

// ---------------------------------------------------------------------------
// Shared types
// ---------------------------------------------------------------------------

pub struct MockConnection;
pub struct MockStatement;

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

// Uses trait defaults (all return NotImplemented).
impl StatementBackend for MockStatement {}

/// A one-column synthetic result set carrying `rows`.
///
/// Tests that need a statement with a genuinely open cursor must go through
/// [`crate::handles::StatementHandle::set_result_set`], which derives
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
            nullable: true,
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

    fn connect(_: &ConnectParams) -> Result<MockConnection, MockError> {
        Ok(MockConnection)
    }
    fn disconnect(_: &mut MockConnection) -> Result<(), MockError> {
        Ok(())
    }
    fn exec_direct(_: &MockConnection, _: &str) -> Result<MockStatement, MockError> {
        Err(MockError)
    }
    fn prepare(_: &MockConnection, _: &str) -> Result<MockStatement, MockError> {
        Ok(MockStatement)
    }
    fn execute(
        _: &MockConnection,
        _: &mut MockStatement,
        _: &[crate::types::ColumnValue],
    ) -> Result<crate::types::ExecuteOutcome, MockError> {
        Ok(crate::types::ExecuteOutcome::default())
    }
    fn get_info(_: &MockConnection, _: crate::types::InfoType) -> Result<InfoValue, MockError> {
        Err(MockError)
    }
    fn get_functions() -> &'static [crate::function_id::FunctionId] {
        &[]
    }
    fn get_type_info() -> &'static [TypeInfoRow] {
        &[]
    }
    fn tables(
        _: &MockConnection,
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<MockStatement, MockError> {
        Err(MockError)
    }
    fn columns(
        _: &MockConnection,
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<MockStatement, MockError> {
        Err(MockError)
    }

    fn supports_catalogs() -> bool {
        true
    }
    fn supports_schemas() -> bool {
        true
    }
    // Deliberately non-zero and not a round number: these values are only
    // correct in a test if `default_get_info` actually read them from the
    // backend. The previous implementation returned a hard-coded 0 for both,
    // which a zero-valued mock could not have distinguished.
    fn alter_table_support() -> u32 {
        crate::types::SQL_AT_ADD_COLUMN_SINGLE | crate::types::SQL_AT_DROP_COLUMN_RESTRICT
    }
    fn outer_join_capabilities() -> u32 {
        crate::types::SQL_OJ_LEFT | crate::types::SQL_OJ_NESTED
    }
    // Deliberately SERIALIZABLE, not READ_COMMITTED: core used to hard-code
    // READ_COMMITTED as the unset `SQL_ATTR_TXN_ISOLATION` value, so a mock
    // declaring READ_COMMITTED could not tell the hook from the old constant.
    fn default_txn_isolation() -> u32 {
        crate::types::SQL_TXN_SERIALIZABLE
    }
    fn txn_isolation_options() -> u32 {
        crate::types::SQL_TXN_SERIALIZABLE
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

    fn connect(_: &ConnectParams) -> Result<MockConnection, MockError> {
        Ok(MockConnection)
    }
    fn disconnect(_: &mut MockConnection) -> Result<(), MockError> {
        Ok(())
    }
    fn exec_direct(_: &MockConnection, _: &str) -> Result<MockStatement, MockError> {
        Err(MockError)
    }
    fn prepare(_: &MockConnection, _: &str) -> Result<MockStatement, MockError> {
        Ok(MockStatement)
    }
    fn execute(
        _: &MockConnection,
        _: &mut MockStatement,
        _: &[crate::types::ColumnValue],
    ) -> Result<crate::types::ExecuteOutcome, MockError> {
        Ok(crate::types::ExecuteOutcome::default())
    }
    fn get_info(_: &MockConnection, _: crate::types::InfoType) -> Result<InfoValue, MockError> {
        Err(MockError)
    }
    fn get_functions() -> &'static [crate::function_id::FunctionId] {
        &[]
    }
    fn get_type_info() -> &'static [TypeInfoRow] {
        &[]
    }
    fn tables(
        _: &MockConnection,
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<MockStatement, MockError> {
        Err(MockError)
    }
    fn columns(
        _: &MockConnection,
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<MockStatement, MockError> {
        Err(MockError)
    }

    fn supports_catalogs() -> bool {
        false
    }
    fn supports_schemas() -> bool {
        false
    }
    fn alter_table_support() -> u32 {
        0
    }
    fn outer_join_capabilities() -> u32 {
        0
    }
    fn default_txn_isolation() -> u32 {
        0
    }
    fn txn_isolation_options() -> u32 {
        0
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

            fn connect(_: &ConnectParams) -> Result<MockIsolationConnection, MockError> {
                Ok(MockIsolationConnection {
                    applied: std::sync::atomic::AtomicU32::new(0),
                })
            }
            fn disconnect(_: &mut MockIsolationConnection) -> Result<(), MockError> {
                Ok(())
            }
            fn exec_direct(
                _: &MockIsolationConnection,
                _: &str,
            ) -> Result<MockStatement, MockError> {
                Err(MockError)
            }
            fn prepare(_: &MockIsolationConnection, _: &str) -> Result<MockStatement, MockError> {
                Ok(MockStatement)
            }
            fn execute(
                _: &MockIsolationConnection,
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
            fn get_functions() -> &'static [crate::function_id::FunctionId] {
                &[]
            }
            fn get_type_info() -> &'static [TypeInfoRow] {
                &[]
            }
            fn tables(
                _: &MockIsolationConnection,
                _: Option<&str>,
                _: Option<&str>,
                _: Option<&str>,
                _: Option<&str>,
            ) -> Result<MockStatement, MockError> {
                Err(MockError)
            }
            fn columns(
                _: &MockIsolationConnection,
                _: Option<&str>,
                _: Option<&str>,
                _: Option<&str>,
                _: Option<&str>,
            ) -> Result<MockStatement, MockError> {
                Err(MockError)
            }

            fn supports_catalogs() -> bool {
                true
            }
            fn supports_schemas() -> bool {
                true
            }
            fn alter_table_support() -> u32 {
                0
            }
            fn outer_join_capabilities() -> u32 {
                0
            }
            fn default_txn_isolation() -> u32 {
                $default
            }
            fn txn_isolation_options() -> u32 {
                $options
            }

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
    fn set_txn_isolation(conn: &MockIsolationConnection, level: u32) -> Result<(), OdbcError> {
        conn.applied
            .store(level, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
);
// Declares two levels but genuinely never implements `set_txn_isolation`, so
// it stands for the backend author who reports a capability and forgets to
// wire it up -- and inherits the real trait default, not a copy of it.
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
/// `ENDTRANFAIL=1`.
macro_rules! mock_txn_backend {
    ($name:ident, commit = $commit:expr, rollback = $rollback:expr) => {
        #[allow(dead_code)]
        pub struct $name;

        impl Backend for $name {
            type Connection = MockTxnConnection;
            type Statement = MockStatement;
            type Error = MockError;

            fn connect(params: &ConnectParams) -> Result<MockTxnConnection, MockError> {
                Ok(MockTxnConnection {
                    end_tran_fails: params.get("endtranfail") == Some("1"),
                })
            }
            fn disconnect(_: &mut MockTxnConnection) -> Result<(), MockError> {
                Ok(())
            }
            // Succeeds, unlike `MockBackend::exec_direct`, so a test can tell
            // "SQLExecDirect was allowed" from "SQLExecDirect was rejected".
            // `MockStatement` reports zero columns, so it stands for a
            // non-result-set statement and opens no cursor.
            fn exec_direct(_: &MockTxnConnection, _: &str) -> Result<MockStatement, MockError> {
                Ok(MockStatement)
            }
            fn prepare(_: &MockTxnConnection, _: &str) -> Result<MockStatement, MockError> {
                Ok(MockStatement)
            }
            fn execute(
                _: &MockTxnConnection,
                _: &mut MockStatement,
                _: &[crate::types::ColumnValue],
            ) -> Result<crate::types::ExecuteOutcome, MockError> {
                Ok(crate::types::ExecuteOutcome::default())
            }
            fn get_info(
                _: &MockTxnConnection,
                _: crate::types::InfoType,
            ) -> Result<InfoValue, MockError> {
                Err(MockError)
            }
            fn get_functions() -> &'static [crate::function_id::FunctionId] {
                &[]
            }
            fn get_type_info() -> &'static [TypeInfoRow] {
                &[]
            }
            fn tables(
                _: &MockTxnConnection,
                _: Option<&str>,
                _: Option<&str>,
                _: Option<&str>,
                _: Option<&str>,
            ) -> Result<MockStatement, MockError> {
                Err(MockError)
            }
            fn columns(
                _: &MockTxnConnection,
                _: Option<&str>,
                _: Option<&str>,
                _: Option<&str>,
                _: Option<&str>,
            ) -> Result<MockStatement, MockError> {
                Err(MockError)
            }

            fn set_autocommit(_: &MockTxnConnection, _: bool) -> Result<(), OdbcError> {
                Ok(())
            }

            fn end_tran(conn: &MockTxnConnection, _commit: bool) -> Result<(), OdbcError> {
                if conn.end_tran_fails {
                    Err(OdbcError::general(
                        "mock end_tran failure",
                        crate::types::SqlState::general_error(),
                    ))
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

            fn supports_catalogs() -> bool {
                true
            }
            fn supports_schemas() -> bool {
                true
            }
            fn alter_table_support() -> u32 {
                0
            }
            fn outer_join_capabilities() -> u32 {
                0
            }
            // A single supported level, so `Backend::set_txn_isolation`'s
            // default applies it without these mocks overriding anything.
            fn default_txn_isolation() -> u32 {
                crate::types::SQL_TXN_SERIALIZABLE
            }
            fn txn_isolation_options() -> u32 {
                crate::types::SQL_TXN_SERIALIZABLE
            }
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
