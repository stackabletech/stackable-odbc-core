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
    ///
    /// The message carries the causal chain, not just the outermost error:
    /// a driver that wrapped its client library's failure with
    /// [`OdbcError::with_source`] would otherwise see it stringified away here,
    /// which is the one place the application can still read it.
    pub fn push(&mut self, error: &OdbcError) {
        let mut message = error.to_string();
        // `Error::source` drops the `Send + Sync` bounds, so the walk is typed
        // as a plain `dyn Error` from the start.
        let mut cause: Option<&(dyn std::error::Error + 'static)> = error
            .cause()
            .map(|e| e as &(dyn std::error::Error + 'static));
        while let Some(err) = cause {
            use std::fmt::Write as _;
            // Cannot fail: the fmt::Error path is unreachable for a String sink,
            // and a lost suffix must not cost the caller its diagnostic.
            let _ = write!(message, ": {err}");
            cause = err.source();
        }
        self.records.push(DiagnosticRecord {
            sqlstate: error.sqlstate(),
            native_error: error.native_error(),
            message,
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
    ///
    /// Used by tests rather than by core itself, and kept because a `len`
    /// without an `is_empty` is the shape clippy's `len_without_is_empty`
    /// exists to prevent.
    #[allow(dead_code, reason = "companion to len(); exercised by tests")]
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

    #[derive(Debug, snafu::Snafu)]
    #[snafu(display("outer"))]
    struct Outer {
        source: Inner,
    }

    #[derive(Debug, snafu::Snafu)]
    #[snafu(display("inner"))]
    struct Inner;

    #[test]
    fn push_reports_the_data_sources_own_error_code() {
        // The data source's own code has to survive the trip to
        // `SQLGetDiagRec`'s NativeErrorPtr. Nothing else carries it, so if the
        // queue drops it the application sees 0 whatever the data source said,
        // and 0 is also the legitimate "no native code" answer.
        let mut q = DiagnosticQueue::new();
        q.push(&OdbcError::general("boom", SqlState::general_error()).with_native_error(1555));
        assert_eq!(q.get(0).expect("record").native_error, 1555);
    }

    #[test]
    fn push_defaults_native_error_to_zero_without_one() {
        let mut q = DiagnosticQueue::new();
        q.push(&OdbcError::NotConnected);
        assert_eq!(q.get(0).expect("record").native_error, 0);
    }

    #[test]
    fn push_message_carries_the_whole_causal_chain() {
        // The diagnostic message is the only place the application can read the
        // driver's cause, so the chain must not stop at the outermost error.
        let mut q = DiagnosticQueue::new();
        q.push(
            &OdbcError::general("wrapped", SqlState::general_error())
                .with_source(Outer { source: Inner }),
        );

        let msg = &q.get(0).expect("record").message;
        assert_eq!(
            msg, "wrapped: outer: inner",
            "expected the full chain, got {msg:?}"
        );
    }

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
