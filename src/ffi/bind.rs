//! `SQLBindCol` — bind an application buffer to a result column.

use std::ffi::c_void;

use crate::backend::Backend;
use crate::descriptor::{DescriptorRecord, DescriptorRole};
use crate::handles::StatementHandle;
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
/// If both `target_value_ptr` and `str_len_or_ind_ptr` are null, the binding for
/// that column is removed. A null `target_value_ptr` with a live
/// `str_len_or_ind_ptr` unbinds only the data buffer and keeps the
/// length/indicator buffer bound, which the spec states in as many words: "An
/// application can unbind the data buffer for a column but still have a
/// length/indicator buffer bound for the column, if the `TargetValuePtr`
/// argument in the call to `SQLBindCol` is a null pointer but the
/// `StrLen_or_IndPtr` argument is a valid value."
///
/// # Parameters
///
/// - `statement_handle`: Statement handle.
/// - `column_number`: Number of the result set column to bind. Columns are numbered starting
///   at 1; column 0 is the bookmark column (bookmarks are not supported: the `Backend` trait
///   has no concept of stable row identifiers).
/// - `target_type`: The C data type identifier (`SQL_C_*`) for the target buffer.
/// - `target_value_ptr`: Pointer to the data buffer to bind to the column. If null, the
///   column's data buffer is unbound; the binding itself is removed only when
///   `str_len_or_ind_ptr` is null as well.
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
            // Copied out now, so reaching the ARD below is one registry lookup
            // rather than resolving this statement a second time.
            let ard_token = stmt.descriptor_token(DescriptorRole::Ard);

            // Spec HYC00: bookmark column (col=0) is not supported by this driver.
            if column_number == 0 {
                return Err(crate::errors::OdbcError::NotImplemented {
                    feature: "SQLBindCol: bookmark column (column_number=0)".into(),
                });
            }

            // Spec, *TargetValuePtr*: "An application can unbind the data
            // buffer for a column but still have a length/indicator buffer
            // bound for the column, if the TargetValuePtr argument in the call
            // to SQLBindCol is a null pointer but the StrLen_or_IndPtr argument
            // is a valid value." So a null data pointer alone is not an unbind:
            // it is the removal of one of the two buffers. Only a call
            // supplying neither removes the record, which is the same pair
            // `SQLBindParameter` treats as an unbind.
            //
            // The two mature drivers disagree here, so this follows the spec
            // rather than a majority. MySQL Connector/ODBC draws the same line
            // — `if (!TargetValuePtr && !StrLen_or_IndPtr) /* Handling
            // unbinding */` in its own `SQLBindCol` — while psqlODBC's
            // `PGAPI_BindCol` clears the whole binding on a null `rgbValue`
            // without consulting `pcbValue` at all. The spec sentence above is
            // unconditional, and an application that asked only for a length
            // has no other way to get one.
            if target_value_ptr.is_null() && str_len_or_ind_ptr.is_null() {
                tracing::debug!("SQLBindCol: col={} unbind", column_number);
                scope.descriptor(ard_token)?.records.remove(&column_number);
            } else {
                let c_type = crate::types::c_data_type_from_raw(target_type).ok_or_else(|| {
                    crate::errors::OdbcError::general(
                        format!("Unknown C data type: {target_type}"),
                        crate::types::SqlState::invalid_application_buffer_type(),
                    )
                })?;
                tracing::debug!(
                    "SQLBindCol: col={}, c_type={:?}, buf_len={}, data={}, indicator={}",
                    column_number,
                    c_type,
                    buffer_length,
                    if target_value_ptr.is_null() {
                        "unbound"
                    } else {
                        "bound"
                    },
                    if str_len_or_ind_ptr.is_null() {
                        "unbound"
                    } else {
                        "bound"
                    },
                );
                let mut record = DescriptorRecord {
                    data_ptr: target_value_ptr,
                    octet_length: buffer_length,
                    indicator_ptr: str_len_or_ind_ptr,
                    ..DescriptorRecord::default()
                };
                record.set_concise_type(c_type as i16);
                // Spec HY021, `SQLSetDescRec`'s "Consistency Checks": "This
                // check is always performed when SQLBindParameter or
                // SQLBindCol is called". Before the insert, so a rejected bind
                // leaves the previous binding — or no binding — in place.
                crate::descriptor::consistency_check(&record, DescriptorRole::Ard)?;
                scope
                    .descriptor(ard_token)?
                    .records
                    .insert(column_number, record);
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
    use crate::test_utils::{
        MockBackend, alloc_env_conn_stmt, cleanup_env_conn_stmt, with_descriptor,
    };
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

    /// Spec, `SQLBindCol`'s *TargetValuePtr*: "An application can unbind the
    /// data buffer for a column but still have a length/indicator buffer bound
    /// for the column, if the TargetValuePtr argument in the call to
    /// SQLBindCol is a null pointer but the StrLen_or_IndPtr argument is a
    /// valid value."
    ///
    /// Removing the record on a null data pointer alone destroys that state
    /// outright, so the application's indicator is never written again and it
    /// has no way to learn a column's length without a buffer to hold it.
    #[test]
    fn bind_col_keeps_an_indicator_only_binding() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let mut buf: i64 = 0;
            let mut indicator: isize = 0;
            let indicator_ptr = std::ptr::from_mut(&mut indicator);

            assert_eq!(
                sql_bind_col::<MockBackend>(
                    stmt,
                    1,
                    CDataType::SBigInt as i16,
                    std::ptr::from_mut(&mut buf).cast::<c_void>(),
                    isize::try_from(std::mem::size_of::<i64>()).expect("small"),
                    indicator_ptr,
                ),
                SqlReturn::SUCCESS,
            );

            // Unbind the data buffer only.
            assert_eq!(
                sql_bind_col::<MockBackend>(
                    stmt,
                    1,
                    CDataType::SBigInt as i16,
                    std::ptr::null_mut(),
                    0,
                    indicator_ptr,
                ),
                SqlReturn::SUCCESS,
            );

            with_descriptor::<MockBackend, _>(stmt, DescriptorRole::Ard, |ard| {
                let record = ard
                    .records
                    .get(&1)
                    .expect("the length/indicator buffer is still bound");
                assert!(
                    record.data_ptr.is_null(),
                    "the data buffer was supposed to be unbound",
                );
                assert_eq!(record.indicator_ptr, indicator_ptr);
                assert!(
                    !record.is_bound(),
                    "SQL_DESC_DATA_PTR is null, so this is not a data binding",
                );
            });

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// The other half of the same rule, and the one that must not change: both
    /// pointers null is still an unbind, exactly as `SQLBindParameter` treats
    /// the pair.
    #[test]
    fn bind_col_removes_the_record_when_both_pointers_are_null() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let mut buf: i64 = 0;
            let mut indicator: isize = 0;

            assert_eq!(
                sql_bind_col::<MockBackend>(
                    stmt,
                    1,
                    CDataType::SBigInt as i16,
                    std::ptr::from_mut(&mut buf).cast::<c_void>(),
                    isize::try_from(std::mem::size_of::<i64>()).expect("small"),
                    std::ptr::from_mut(&mut indicator),
                ),
                SqlReturn::SUCCESS,
            );
            assert_eq!(
                sql_bind_col::<MockBackend>(
                    stmt,
                    1,
                    CDataType::SBigInt as i16,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                ),
                SqlReturn::SUCCESS,
            );

            with_descriptor::<MockBackend, _>(stmt, DescriptorRole::Ard, |ard| {
                assert!(!ard.records.contains_key(&1), "both pointers null unbinds");
            });

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
            with_descriptor::<MockBackend, _>(stmt, DescriptorRole::Ard, |ard| {
                assert!(ard.records.contains_key(&1));
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
            with_descriptor::<MockBackend, _>(stmt, DescriptorRole::Ard, |ard| {
                assert!(!ard.records.contains_key(&1));
            });

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// The spec makes binding a column *be* setting ARD fields — "when
    /// `SQLBindCol` is called, the driver sets fields in the ARD". After this
    /// change there is one storage rather than a binding map beside a
    /// descriptor, so the two cannot disagree, which is the whole point.
    ///
    /// Every field is checked, not just the key: a record found under the right
    /// number with the wrong buffer would satisfy `contains_key` above and
    /// still hand `SQLFetch` the wrong pointer.
    #[test]
    fn a_bound_column_is_a_record_in_the_ard() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let mut buf: i64 = 0;
            let mut indicator: isize = 0;
            let buf_ptr = std::ptr::from_mut(&mut buf).cast::<c_void>();
            let indicator_ptr = std::ptr::from_mut(&mut indicator);

            let ret = sql_bind_col::<MockBackend>(
                stmt,
                1,
                CDataType::SBigInt as i16,
                buf_ptr,
                std::mem::size_of::<i64>() as isize,
                indicator_ptr,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            with_descriptor::<MockBackend, _>(stmt, DescriptorRole::Ard, |ard| {
                let record = ard
                    .records
                    .get(&1)
                    .expect("SQLBindCol did not write a record into the ARD");
                assert_eq!(
                    record.c_type().expect("SQLBindCol stored a valid C type"),
                    CDataType::SBigInt
                );
                assert_eq!(record.data_ptr, buf_ptr);
                assert_eq!(record.octet_length, std::mem::size_of::<i64>() as isize);
                assert_eq!(record.indicator_ptr, indicator_ptr);
            });

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// The consistency check runs at `SQLBindCol` too, and a datetime column is
    /// where it would wrongly fire.
    ///
    /// `SQL_DESC_DATETIME_INTERVAL_CODE` holds the *subcode* — `SQL_CODE_DATE`
    /// is 1 — while the concise type is `SQL_TYPE_DATE`, which is 91. A record
    /// built by copying the concise type into the subcode would carry 91 in a
    /// field whose only legal datetime values are 1, 2 and 3, and the check
    /// would then reject a binding that has nothing wrong with it. That is the
    /// one way this task could have broken an existing driver silently.
    #[test]
    fn bind_col_accepts_a_date_column_and_records_its_subcode() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let mut buf = [0u8; 6];
            let buf_ptr = buf.as_mut_ptr().cast::<c_void>();

            let ret = sql_bind_col::<MockBackend>(
                stmt,
                1,
                CDataType::TypeDate as i16,
                buf_ptr,
                buf.len() as isize,
                std::ptr::null_mut(),
            );
            assert_eq!(
                ret,
                SqlReturn::SUCCESS,
                "the consistency check rejected a plain SQL_C_TYPE_DATE binding"
            );

            with_descriptor::<MockBackend, _>(stmt, DescriptorRole::Ard, |ard| {
                let record = ard
                    .records
                    .get(&1)
                    .expect("SQLBindCol did not write a record into the ARD");
                assert_eq!(record.concise_type, CDataType::TypeDate as i16);
                assert_eq!(record.verbose_type, crate::types::SQL_DATETIME);
                assert_eq!(record.datetime_interval_code, crate::types::SQL_CODE_DATE);
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
