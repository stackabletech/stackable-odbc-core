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

/// The eight required capability declarations, for a mock that stands for a
/// minimal data source and whose test does not care about them.
///
/// Every value is the *least* capable one the spec defines, which for four of
/// them happens to be `0`. That is deliberate: `0` is a real claim for these
/// info types, so a mock that means "minimal" should say so explicitly rather
/// than have core assume it — which is the whole point of the methods being
/// required. Expand this in a mock that is actually testing one of them.
macro_rules! minimal_capability_decls {
    () => {
        fn group_by() -> u16 {
            crate::types::SQL_GB_NOT_SUPPORTED
        }
        fn null_collation() -> u16 {
            crate::types::SQL_NC_HIGH
        }
        fn correlation_name() -> u16 {
            crate::types::SQL_CN_NONE
        }
        fn non_nullable_columns() -> u16 {
            crate::types::SQL_NNC_NULL
        }
        fn expressions_in_order_by() -> bool {
            false
        }
        // Conforms to no SQL-92 level, which is consistent with the values
        // above -- an entry-level claim would contradict SQL_CN_NONE and
        // SQL_NNC_NULL.
        fn sql_conformance() -> u32 {
            0
        }
        fn timedate_add_intervals() -> u32 {
            0
        }
        fn timedate_diff_intervals() -> u32 {
            0
        }
        fn subqueries() -> u32 {
            0
        }
        fn column_alias() -> bool {
            false
        }
        fn concat_null_behavior() -> u16 {
            crate::types::SQL_CB_NULL
        }
        fn union_support() -> u32 {
            0
        }
        fn convert_functions() -> u32 {
            0
        }
        fn order_by_columns_in_select() -> bool {
            true
        }
        fn accessible_tables() -> bool {
            false
        }
        fn data_source_read_only() -> bool {
            false
        }
        fn search_pattern_escape() -> &'static str {
            ""
        }
    };
}

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

    // A self-consistent SQL-92 Entry level declaration. The spec names the
    // value an entry-level driver returns for three of these, so declaring
    // `SQL_SC_SQL92_ENTRY` and then contradicting it would make this mock a
    // bad example as well as a bad test fixture. Each value is also
    // deliberately *not* the old core default, so a test cannot pass by
    // accident against the hard-coded value it replaced.
    fn group_by() -> u16 {
        crate::types::SQL_GB_GROUP_BY_EQUALS_SELECT // was SQL_GB_NO_RELATION
    }
    fn null_collation() -> u16 {
        crate::types::SQL_NC_END // was SQL_NC_HIGH (0), via the shape default
    }
    fn correlation_name() -> u16 {
        crate::types::SQL_CN_ANY // was SQL_CN_NONE (0)
    }
    fn non_nullable_columns() -> u16 {
        crate::types::SQL_NNC_NON_NULL // was SQL_NNC_NULL (0)
    }
    fn expressions_in_order_by() -> bool {
        true // was "" -- neither "Y" nor "N"
    }
    fn sql_conformance() -> u32 {
        crate::types::SQL_SC_SQL92_ENTRY
    }
    // Deliberately different from each other, so one hook cannot serve both
    // without a test noticing.
    fn timedate_add_intervals() -> u32 {
        crate::types::SQL_FN_TSI_SECOND | crate::types::SQL_FN_TSI_DAY
    }
    fn timedate_diff_intervals() -> u32 {
        crate::types::SQL_FN_TSI_SECOND | crate::types::SQL_FN_TSI_YEAR
    }

    // The values core used to hard-code. Keeping them here rather than
    // changing them keeps this mock's `SQL_SC_SQL92_ENTRY` claim honest --
    // the spec names each of the first three as what an entry-level driver
    // returns -- and means the snapshot test still pins the same output.
    fn subqueries() -> u32 {
        crate::types::SQL_SQ_COMPARISON
            | crate::types::SQL_SQ_EXISTS
            | crate::types::SQL_SQ_IN
            | crate::types::SQL_SQ_QUANTIFIED
            | crate::types::SQL_SQ_CORRELATED_SUBQUERIES
    }
    fn column_alias() -> bool {
        true
    }
    fn concat_null_behavior() -> u16 {
        crate::types::SQL_CB_NULL
    }
    fn union_support() -> u32 {
        crate::types::SQL_U_UNION | crate::types::SQL_U_UNION_ALL
    }
    fn convert_functions() -> u32 {
        crate::types::SQL_FN_CVT_CAST
    }
    fn order_by_columns_in_select() -> bool {
        false
    }
    fn accessible_tables() -> bool {
        false
    }
    fn data_source_read_only() -> bool {
        false
    }
    fn search_pattern_escape() -> &'static str {
        "\\"
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

    minimal_capability_decls!();
}

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
        crate::types::SQL_AT_ADD_CONSTRAINT
    }
    fn outer_join_capabilities() -> u32 {
        crate::types::SQL_OJ_RIGHT
    }
    fn default_txn_isolation() -> u32 {
        crate::types::SQL_TXN_READ_UNCOMMITTED
    }
    fn txn_isolation_options() -> u32 {
        crate::types::SQL_TXN_READ_UNCOMMITTED
    }
    fn group_by() -> u16 {
        crate::types::SQL_GB_NO_RELATION
    }
    fn null_collation() -> u16 {
        crate::types::SQL_NC_LOW
    }
    fn correlation_name() -> u16 {
        crate::types::SQL_CN_DIFFERENT
    }
    fn non_nullable_columns() -> u16 {
        crate::types::SQL_NNC_NULL
    }
    fn expressions_in_order_by() -> bool {
        false
    }
    fn sql_conformance() -> u32 {
        crate::types::SQL_SC_SQL92_FULL
    }
    fn timedate_add_intervals() -> u32 {
        crate::types::SQL_FN_TSI_HOUR
    }
    fn timedate_diff_intervals() -> u32 {
        crate::types::SQL_FN_TSI_WEEK
    }
    fn subqueries() -> u32 {
        crate::types::SQL_SQ_EXISTS
    }
    fn column_alias() -> bool {
        false
    }
    fn concat_null_behavior() -> u16 {
        crate::types::SQL_CB_NON_NULL
    }
    fn union_support() -> u32 {
        crate::types::SQL_U_UNION
    }
    fn convert_functions() -> u32 {
        crate::types::SQL_FN_CVT_CONVERT
    }
    fn order_by_columns_in_select() -> bool {
        true
    }
    fn accessible_tables() -> bool {
        true
    }
    fn data_source_read_only() -> bool {
        true
    }
    fn search_pattern_escape() -> &'static str {
        "/"
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
    fn escape_dialect() -> crate::escape::EscapeDialect {
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
