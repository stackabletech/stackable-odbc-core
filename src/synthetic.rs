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

    fn column_count(&self) -> u16 {
        self.columns.len() as u16
    }

    fn describe_col(&self, col: u16) -> Result<ColumnDescriptor, OdbcError> {
        let idx = col as usize;
        if idx == 0 || idx > self.columns.len() {
            return Err(OdbcError::general(
                "Column index out of range",
                SqlState::general_error(),
            ));
        }
        // PERF: Clones ColumnDescriptor (including the name String) on every
        // describe_col() call. Called once per column during SQLDescribeCol /
        // SQLColAttribute, not a per-row hot path, so impact is minor.
        Ok(self.columns[idx - 1].clone())
    }

    fn row_count(&self) -> Option<usize> {
        Some(self.rows.len())
    }

    fn close_cursor(&mut self) {
        self.cursor = -1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SqlDataType;

    fn test_columns() -> Vec<ColumnDescriptor> {
        vec![ColumnDescriptor {
            name: "val".to_string(),
            type_name: String::new(),
            sql_type: SqlDataType::INTEGER,
            precision: 10,
            scale: 0,
            nullable: true,
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
        stmt.close_cursor();
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
