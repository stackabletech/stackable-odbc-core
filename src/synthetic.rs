//! A statement backed by in-memory data (not a database query).
//!
//! Used for `SQLGetTypeInfo` and potentially catalog functions that return
//! driver-synthesized result sets.

use std::borrow::Cow;

use crate::backend::StatementBackend;
use crate::errors::OdbcError;
use crate::types::{ColumnDescriptor, ColumnValue, FetchResult, SqlState};
use odbc_sys::CDataType;

/// An in-memory result set that implements [`StatementBackend`].
///
/// Rows and column descriptors are provided at construction time. The cursor
/// starts before the first row (`-1`) and advances with each `fetch()` call.
pub struct SyntheticStatement {
    columns: Vec<ColumnDescriptor>,
    // PERF: Full result set materialized in memory before first fetch.
    // For the synthetic use case (SQLGetTypeInfo, catalog functions) the row
    // count is bounded by driver metadata (tens to hundreds of rows), so this
    // is acceptable. If SyntheticStatement is ever used for unbounded results,
    // replace with an iterator/streaming design.
    rows: Vec<Vec<ColumnValue>>,
    cursor: i64, // -1 = before first row
}

impl SyntheticStatement {
    /// Create a new synthetic statement with the given column descriptors and rows.
    pub fn new(columns: Vec<ColumnDescriptor>, rows: Vec<Vec<ColumnValue>>) -> Self {
        Self {
            columns,
            rows,
            cursor: -1,
        }
    }
}

impl StatementBackend for SyntheticStatement {
    /// `OdbcError` directly: this statement is core's own in-memory result set,
    /// with no backend behind it and so no driver error type to preserve.
    type Error = OdbcError;

    fn fetch(&mut self) -> Result<FetchResult, OdbcError> {
        self.cursor += 1;
        if (self.cursor as usize) < self.rows.len() {
            Ok(FetchResult::Row)
        } else {
            Ok(FetchResult::NoData)
        }
    }

    fn get_data(
        &mut self,
        col: u16,
        _target_type: CDataType,
    ) -> Result<Cow<'_, ColumnValue>, OdbcError> {
        if self.cursor < 0 {
            return Err(OdbcError::general(
                "Cursor not positioned on a row (call fetch first)",
                SqlState::invalid_cursor_state(),
            ));
        }
        let row_idx = self.cursor as usize;
        if row_idx >= self.rows.len() {
            return Err(OdbcError::general(
                "Cursor past end of result set",
                SqlState::invalid_cursor_state(),
            ));
        }
        let col_idx = col as usize;
        if col_idx == 0 || col_idx > self.rows[row_idx].len() {
            return Err(OdbcError::general(
                "Column index out of range",
                SqlState::general_error(),
            ));
        }
        Ok(Cow::Borrowed(&self.rows[row_idx][col_idx - 1]))
    }

    fn column_count(&self) -> i16 {
        i16::try_from(self.columns.len()).unwrap_or_else(|_| {
            tracing::warn!(
                "synthetic result set has {} columns, more than SQLSMALLINT can report; \
                 clamping to {}",
                self.columns.len(),
                i16::MAX
            );
            i16::MAX
        })
    }

    fn describe_col(&self, col: u16) -> Result<Cow<'_, ColumnDescriptor>, OdbcError> {
        let idx = col as usize;
        if idx == 0 || idx > self.columns.len() {
            return Err(OdbcError::general(
                "Column index out of range",
                SqlState::general_error(),
            ));
        }
        // Borrowed: the descriptors are owned by this statement and outlive the
        // call, so neither the name nor the type name is cloned any more.
        Ok(Cow::Borrowed(&self.columns[idx - 1]))
    }

    fn row_count(&self) -> Option<i64> {
        Some(i64::try_from(self.rows.len()).unwrap_or(i64::MAX))
    }

    fn close_cursor(&mut self) -> Result<(), OdbcError> {
        self.cursor = -1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Nullable, SqlDataType};

    fn test_columns() -> Vec<ColumnDescriptor> {
        vec![ColumnDescriptor {
            name: "val".to_string(),
            type_name: String::new(),
            sql_type: SqlDataType::INTEGER,
            precision: 10,
            scale: 0,
            nullable: Nullable::SqlNullable,
            ..Default::default()
        }]
    }

    #[test]
    fn fetch_returns_rows_then_no_data() {
        let rows = vec![vec![ColumnValue::I32(1)], vec![ColumnValue::I32(2)]];
        let mut stmt = SyntheticStatement::new(test_columns(), rows);

        assert_eq!(stmt.fetch().expect("fetch 1"), FetchResult::Row);
        assert_eq!(stmt.fetch().expect("fetch 2"), FetchResult::Row);
        assert_eq!(stmt.fetch().expect("fetch 3"), FetchResult::NoData);
    }

    #[test]
    fn get_data_returns_current_row_value() {
        let rows = vec![vec![ColumnValue::I32(42)]];
        let mut stmt = SyntheticStatement::new(test_columns(), rows);

        let _ = stmt.fetch().expect("fetch");
        let val = stmt.get_data(1, CDataType::Default).expect("get_data");
        assert_eq!(*val, ColumnValue::I32(42));
    }

    #[test]
    fn get_data_before_fetch_returns_error() {
        let stmt_rows: Vec<Vec<ColumnValue>> = vec![];
        let mut stmt = SyntheticStatement::new(test_columns(), stmt_rows);

        let result = stmt.get_data(1, CDataType::Default);
        assert!(result.is_err());
    }

    #[test]
    fn close_cursor_resets_position() {
        let rows = vec![vec![ColumnValue::I32(1)]];
        let mut stmt = SyntheticStatement::new(test_columns(), rows);

        let _ = stmt.fetch().expect("fetch");
        stmt.close_cursor()
            .expect("synthetic close_cursor is infallible");
        // After close, fetch should start from the beginning again.
        assert_eq!(stmt.fetch().expect("re-fetch"), FetchResult::Row);
    }

    #[test]
    fn column_count_and_describe_col() {
        let stmt = SyntheticStatement::new(test_columns(), vec![]);

        assert_eq!(stmt.column_count(), 1);
        let desc = stmt.describe_col(1).expect("describe_col");
        assert_eq!(desc.name, "val");
    }

    #[test]
    fn row_count_returns_total_rows() {
        let rows = vec![vec![ColumnValue::I32(1)], vec![ColumnValue::I32(2)]];
        let stmt = SyntheticStatement::new(test_columns(), rows);
        assert_eq!(stmt.row_count(), Some(2));
    }
}
