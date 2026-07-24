//! Per-handle diagnostic queue that `SQLGetDiagRecW`/`SQLGetDiagFieldW` read from.

use crate::errors::OdbcError;
use crate::types::SqlState;

/// A single diagnostic record as returned by `SQLGetDiagRec`.
pub struct DiagnosticRecord {
    pub sqlstate: SqlState,
    pub native_error: i32,
    pub message: String,
}

/// A FIFO queue of diagnostic records attached to each ODBC handle.
pub struct DiagnosticQueue {
    records: Vec<DiagnosticRecord>,
}

impl DiagnosticQueue {
    /// Creates an empty diagnostic queue.
    pub fn new() -> Self {
        Self {
            records: Vec::new(),
        }
    }

    /// Clears all records. Called at the start of each ODBC function call per spec.
    pub fn clear(&mut self) {
        self.records.clear();
    }

    /// Appends a diagnostic record derived from `error`.
    pub fn push(&mut self, error: &OdbcError) {
        self.records.push(DiagnosticRecord {
            sqlstate: error.sqlstate(),
            native_error: 0,
            message: error.to_string(),
        });
    }

    /// Returns the record at the given 0-based index, or `None` if out of range.
    pub fn get(&self, index: usize) -> Option<&DiagnosticRecord> {
        self.records.get(index)
    }

    /// Returns the number of pending diagnostic records.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Returns `true` if there are no pending diagnostic records.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

impl Default for DiagnosticQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_and_retrieve() {
        let mut q = DiagnosticQueue::new();
        assert!(q.is_empty());

        let err = OdbcError::NotConnected;
        q.push(&err);

        assert_eq!(q.len(), 1);
        let rec = q.get(0).unwrap();
        assert_eq!(
            rec.sqlstate.as_str(),
            crate::types::sql_state::CONNECTION_NOT_OPEN
        );
        assert_eq!(rec.message, "Connection not established");
    }

    #[test]
    fn clear_empties_queue() {
        let mut q = DiagnosticQueue::new();
        q.push(&OdbcError::NoResultSet);
        assert!(!q.is_empty());
        q.clear();
        assert!(q.is_empty());
    }

    #[test]
    fn default_is_empty() {
        let q = DiagnosticQueue::default();
        assert!(q.is_empty());
    }
}
