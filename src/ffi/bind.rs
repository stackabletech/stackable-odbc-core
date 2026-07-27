//! `SQLBindCol` — bind an application buffer to a result column.

use std::ffi::c_void;

use crate::backend::Backend;
use crate::handles::{ColumnBinding, StatementHandle};
use crate::panic::panic_safe;
use crate::types::SqlReturn;

/// Generic implementation of SQLBindCol.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlbindcol-function>
///
/// Stores the binding information on the statement handle. The actual data
/// transfer happens in `SQLFetch` which calls `write_column_value` for each
/// bound column.
///
/// If `target_value_ptr` is null, the binding for that column is removed.
///
/// # Parameters
///
/// - `statement_handle`: Statement handle.
/// - `column_number`: Number of the result set column to bind. Columns are numbered starting
///   at 1; column 0 is the bookmark column (bookmarks are not supported: the `Backend` trait
///   has no concept of stable row identifiers).
/// - `target_type`: The C data type identifier (`SQL_C_*`) for the target buffer.
/// - `target_value_ptr`: Pointer to the data buffer to bind to the column. If null, the
///   existing binding for this column is removed.
/// - `buffer_length`: Length of the `target_value_ptr` buffer in bytes.
/// - `str_len_or_ind_ptr`: Pointer to a length/indicator buffer. May be null.
///
/// # Spec compliance
///
/// Diagnostics from the ODBC spec Diagnostics table:
///
/// - `01000` General warning — (driver-manager-handled; not returned here)
/// - `07006` Restricted data type attribute violation — (driver-manager-handled; emitted when
///   `column_number == 0` and `target_type` is not `SQL_C_BOOKMARK` or `SQL_C_VARBOOKMARK`;
///   not returned here because bookmark columns are not supported)
/// - `07009` Invalid descriptor index — the spec requires returning 07009 when `column_number`
///   exceeds the maximum number of columns in the result set. The binding is stored without
///   checking: column count is not available at bind time (before `SQLExecute`), and
///   the DM validates descriptor indices against the result set. Deferred.
/// - `HY000` General error — returned for unexpected failures
/// - `HY001` Memory allocation error — (driver-manager-handled; not returned here)
/// - `HY003` Invalid application buffer type — returned when `target_type` is not a valid
///   C data type identifier (`c_data_type_from_raw` returns `None`)
/// - `HY010` Function sequence error — (driver-manager-handled; not returned here)
/// - `HY013` Memory management error — (driver-manager-handled; not returned here)
/// - `HY090` Invalid string or buffer length — (driver-manager-handled; not returned here;
///   the DM checks for `buffer_length < 0`)
/// - `HY117` Connection is suspended — (driver-manager-handled; not returned here)
/// - `HYC00` Optional feature not implemented — returned when `column_number == 0`
///   (bookmark column; bookmarks are not supported because the `Backend` trait has no
///   concept of stable row identifiers)
/// - `HYT01` Connection timeout expired — (driver-manager-handled; not returned here)
/// - `IM001` Driver does not support this function — (driver-manager-handled; not returned
///   here)
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
/// `target_value_ptr` and `str_len_or_ind_ptr` must remain valid until the
/// binding is changed or the statement is freed (ODBC application contract).
pub unsafe fn sql_bind_col<B: Backend>(
    statement_handle: *mut c_void,
    column_number: u16,
    target_type: i16,
    target_value_ptr: *mut c_void,
    buffer_length: isize,
    str_len_or_ind_ptr: *mut isize,
) -> SqlReturn {
    tracing::trace!(
        "SQLBindCol(stmt={:?}, col={}, type={}, buf_len={})",
        statement_handle,
        column_number,
        target_type,
        buffer_length
    );
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, |scope| {
            let stmt = scope.get::<StatementHandle<B>>(statement_handle)?;
            stmt.diagnostics.clear();

            // Spec HYC00: bookmark column (col=0) is not supported by this driver.
            if column_number == 0 {
                return Err(crate::errors::OdbcError::NotImplemented {
                    feature: "SQLBindCol: bookmark column (column_number=0)".into(),
                });
            }

            if target_value_ptr.is_null() {
                // Unbind the column
                tracing::debug!("SQLBindCol: col={} unbind", column_number);
                stmt.bindings.remove(&column_number);
            } else {
                let c_type = crate::types::c_data_type_from_raw(target_type).ok_or_else(|| {
                    crate::errors::OdbcError::general(
                        format!("Unknown C data type: {target_type}"),
                        crate::types::SqlState::invalid_application_buffer_type(),
                    )
                })?;
                tracing::debug!(
                    "SQLBindCol: col={}, c_type={:?}, buf_len={}",
                    column_number,
                    c_type,
                    buffer_length
                );
                stmt.bindings.insert(
                    column_number,
                    ColumnBinding {
                        target_type: c_type,
                        target_value_ptr,
                        buffer_length,
                        str_len_or_ind_ptr,
                    },
                );
            }

            Ok(SqlReturn::SUCCESS)
        })
    };
    tracing::debug!("SQLBindCol -> {:?}", ret);
    ret
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{MockBackend, alloc_env_conn_stmt, cleanup_env_conn_stmt, with_handle};
    use crate::types::CDataType;
    use std::ffi::c_void;

    #[test]
    fn bind_col_zero_returns_hyc00() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let mut buf: i64 = 0;
            let ret = sql_bind_col::<MockBackend>(
                stmt,
                0, // bookmark column — not supported
                CDataType::SBigInt as i16,
                &mut buf as *mut i64 as *mut c_void,
                std::mem::size_of::<i64>() as isize,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    #[test]
    fn bind_and_unbind_column() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let mut buf: i64 = 0;
            let mut indicator: isize = 0;

            // Bind column 1
            let ret = sql_bind_col::<MockBackend>(
                stmt,
                1,
                CDataType::SBigInt as i16,
                &mut buf as *mut i64 as *mut c_void,
                std::mem::size_of::<i64>() as isize,
                &mut indicator,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            // Verify binding exists
            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                assert!(handle.bindings.contains_key(&1));
            });

            // Unbind by passing null target_value_ptr
            let ret = sql_bind_col::<MockBackend>(
                stmt,
                1,
                CDataType::SBigInt as i16,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                assert!(!handle.bindings.contains_key(&1));
            });

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    #[test]
    fn bind_col_invalid_c_type_returns_error() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let mut buf: i64 = 0;
            let ret = sql_bind_col::<MockBackend>(
                stmt,
                1,
                9999, // invalid C data type
                &mut buf as *mut i64 as *mut c_void,
                std::mem::size_of::<i64>() as isize,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    #[test]
    fn bind_col_null_handle_returns_invalid_handle() {
        unsafe {
            let mut buf: i64 = 0;
            let ret = sql_bind_col::<MockBackend>(
                std::ptr::null_mut(),
                1,
                CDataType::SBigInt as i16,
                &mut buf as *mut i64 as *mut c_void,
                std::mem::size_of::<i64>() as isize,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::INVALID_HANDLE);
        }
    }
}
