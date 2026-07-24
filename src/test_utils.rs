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
}
