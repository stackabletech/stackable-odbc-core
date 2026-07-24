//! Row fetching and column retrieval: `SQLFetch`, `SQLFetchScroll`, `SQLGetData`.

use std::ffi::c_void;

use odbc_sys::FetchOrientation;

use crate::backend::{Backend, StatementBackend};
use crate::column_value::write_column_value;
use crate::errors::OdbcError;
use crate::handles::{StatementHandle, as_handle_ref};
use crate::panic::panic_safe;
use crate::types::{ColumnValue, FetchResult, SqlReturn, SqlState, fetch_orientation_from_raw};

/// Generic implementation of SQLFetch.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlfetch-function>
///
/// Advances the cursor to the next row. Returns `SQL_SUCCESS` if a row is
/// available, `SQL_NO_DATA` when the result set is exhausted.
///
/// # Parameters
///
/// - `statement_handle`: Statement handle (input).
///
/// # Spec compliance
///
/// - 01000 (general warning): not returned; driver-specific informational messages are not
///   generated.
/// - 01004 (string data right truncated): pushed as a diagnostic, and `SQL_SUCCESS_WITH_INFO`
///   returned, when `write_column_value` reports that character or binary data was truncated
///   to fit a bound buffer.
/// - 01S01 (error in row): not returned; only single-row rowsets are processed.
/// - 01S07 (fractional truncation): raised by `write_column_value` as
///   `OdbcError::FractionalTruncation` when numeric fractional data is truncated, or when a
///   non-zero `ColumnValue::Time` fraction is dropped writing to `SQL_C_TYPE_TIME`; the
///   diagnostic is pushed by `panic_safe`.
/// - 07006 (restricted data type attribute violation): returned via `write_column_value` when
///   a column value cannot be converted to the bound C type.
/// - 07009 (invalid descriptor index): column 0 (bookmark) bindings and ODBC 2.x
///   `SQLExtendedFetch` absence are handled by the Driver Manager; not returned here.
/// - 08S01 (communication link failure): propagated from the backend fetch.
/// - 22001 (string data right truncated for bookmark): not applicable; bookmarks are not
///   supported (the `Backend` trait has no concept of stable row identifiers).
/// - 22002 (indicator variable required but not supplied): returned when a bound column
///   contains NULL data and `str_len_or_ind_ptr` for that binding is null.
/// - 22003 (numeric value out of range): returned via `write_column_value` for whole-part
///   numeric overflow.
/// - 22007 (invalid datetime format): returned via `write_column_value`.
/// - 22012 (division by zero): propagated from the backend if the data source reports it.
/// - 22015 (interval field overflow): returned via `write_column_value`.
/// - 22018 (invalid character value for cast specification): returned via `write_column_value`.
/// - 24000 (invalid cursor state): returned as HY010 (function sequence error) when no
///   statement has been executed; the distinction is academic here since a statement handle
///   cannot be in an executed-but-no-result-set state.
/// - 40001 (serialization failure): propagated from the backend.
/// - 40003 (statement completion unknown): propagated from the backend.
/// - HY000 (general error): propagated from the backend.
/// - HY001 (memory allocation error): not returned; Rust panics on allocation failure.
/// - HY008 (operation canceled): not applicable; the `Backend` trait is synchronous.
/// - HY010 (function sequence error): returned when `stmt.statement` is `None`, i.e. no
///   result set is open. (DM) variants (async context, `SQLExtendedFetch` mixing) are
///   driver-manager-handled; not returned here.
/// - HY013 (memory management error): not returned.
/// - HY090 (invalid string or buffer length): not applicable; bookmark buffer length
///   validation is driver-manager-handled.
/// - HY107 (row value out of range): not applicable; keyset cursors are not supported.
/// - HY117 (connection suspended): driver-manager-handled; not returned here.
/// - HYC00 (optional feature not implemented): returned via `write_column_value` when an
///   unsupported type conversion is requested.
/// - HYT00 (timeout expired): not implemented; query timeouts are not supported.
/// - HYT01 (connection timeout expired): not implemented.
/// - IM001 (driver does not support this function): driver-manager-handled; not returned here.
/// - IM017, IM018: driver-manager-handled; not returned here.
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
pub unsafe fn sql_fetch<B: Backend>(statement_handle: *mut c_void) -> SqlReturn {
    tracing::debug!("SQLFetch(stmt={:?})", statement_handle);
    // SAFETY: statement_handle is null or a valid StatementHandle<B> allocated by
    // sql_alloc_handle; tag is validated by as_handle_ref inside the closure.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, || {
            let stmt = as_handle_ref::<StatementHandle<B>>(statement_handle)?;
            stmt.diagnostics.clear();

            // Spec HY010: No result set.
            let Some(ref mut statement) = stmt.statement else {
                return Err(OdbcError::general(
                    "No result set available; statement not executed",
                    SqlState::function_sequence_error(),
                ));
            };

            match statement.fetch()? {
                FetchResult::Row => {
                    // Populate bound columns.
                    // Collect binding info first to avoid borrowing stmt mutably
                    // while also borrowing stmt.statement.
                    let binding_info: Vec<(
                        u16,
                        odbc_sys::CDataType,
                        *mut c_void,
                        isize,
                        *mut isize,
                    )> = stmt
                        .bindings
                        .iter()
                        .map(|(&col, b)| {
                            (
                                col,
                                b.target_type,
                                b.target_value_ptr,
                                b.buffer_length,
                                b.str_len_or_ind_ptr,
                            )
                        })
                        .collect();

                    let mut truncated = false;
                    if let Some(ref mut statement) = stmt.statement {
                        for (col, c_type, target_ptr, buf_len, ind_ptr) in &binding_info {
                            let value = statement.get_data(*col, *c_type)?;
                            // Spec 22002: if data is NULL and no indicator variable was supplied, return error.
                            if matches!(*value, ColumnValue::Null) && ind_ptr.is_null() {
                                return Err(OdbcError::general(
                                    format!(
                                        "Column {col} is NULL but no indicator variable was supplied (str_len_or_ind_ptr is null)"
                                    ),
                                    SqlState::indicator_variable_required(),
                                ));
                            }
                            let written = write_column_value(
                                &value,
                                *c_type,
                                *target_ptr,
                                *buf_len,
                                *ind_ptr,
                            )?;
                            if written == SqlReturn::SUCCESS_WITH_INFO {
                                truncated = true;
                            }
                        }
                    }

                    // Spec 01004: "If the data is truncated because the length of
                    // the data buffer is too small ... SQLFetch returns SQLSTATE
                    // 01004 (Data truncated) and SQL_SUCCESS_WITH_INFO."
                    // Returning plain SQL_SUCCESS would leave the application
                    // reading truncated data believing it complete.
                    //
                    // Fractional truncation (01S07) arrives as an `Err` from
                    // write_column_value and is handled by panic_safe.
                    if truncated {
                        stmt.diagnostics.push(&OdbcError::StringTruncated);
                        return Ok(SqlReturn::SUCCESS_WITH_INFO);
                    }

                    Ok(SqlReturn::SUCCESS)
                }
                FetchResult::NoData => Ok(SqlReturn::NO_DATA),
            }
        })
    };
    tracing::debug!("SQLFetch -> {:?}", ret);
    ret
}

/// Generic implementation of SQLFetchScroll.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlfetchscroll-function>
///
/// This driver supports forward-only cursors only. `SQL_FETCH_NEXT` delegates
/// to `sql_fetch`; all other orientations return HY106 (fetch type out of range).
///
/// # Parameters
///
/// - `statement_handle`: Statement handle (input).
/// - `fetch_orientation`: Type of fetch — one of `SQL_FETCH_NEXT`, `SQL_FETCH_PRIOR`,
///   `SQL_FETCH_FIRST`, `SQL_FETCH_LAST`, `SQL_FETCH_ABSOLUTE`, `SQL_FETCH_RELATIVE`,
///   or `SQL_FETCH_BOOKMARK`. Only `SQL_FETCH_NEXT` is supported.
/// - `fetch_offset`: Row offset used with `SQL_FETCH_ABSOLUTE`, `SQL_FETCH_RELATIVE`, and
///   `SQL_FETCH_BOOKMARK`; ignored for all other orientations (input).
///
/// # Spec compliance
///
/// - 01000 (general warning): not returned; see `sql_fetch`.
/// - 01004 (string data right truncated): delegated to `sql_fetch` for `SQL_FETCH_NEXT`.
/// - 01S01 (error in row): not returned; see `sql_fetch`.
/// - 01S06 (attempt to fetch before result set returned first rowset): not applicable; only
///   `SQL_FETCH_NEXT` is supported, which cannot reach before-start.
/// - 01S07 (fractional truncation): delegated to `sql_fetch` for `SQL_FETCH_NEXT`.
/// - 07006 (restricted data type attribute violation): delegated to `sql_fetch`.
/// - 07009 (invalid descriptor index): driver-manager-handled; not returned here.
/// - 08S01 (communication link failure): delegated to `sql_fetch`.
/// - 22001–22018: delegated to `sql_fetch` for `SQL_FETCH_NEXT`.
/// - 24000 (invalid cursor state): delegated to `sql_fetch` for `SQL_FETCH_NEXT`.
/// - 40001, 40003: delegated to `sql_fetch`.
/// - HY000 (general error): propagated from the backend.
/// - HY001 (memory allocation error): not returned; Rust panics on allocation failure.
/// - HY008 (operation canceled): not applicable; the `Backend` trait is synchronous.
/// - HY010 (function sequence error): delegated to `sql_fetch` for `SQL_FETCH_NEXT`. (DM)
///   variants are driver-manager-handled; not returned here.
/// - HY013 (memory management error): not returned.
/// - HY090 (invalid string or buffer length): driver-manager-handled.
/// - HY106 (fetch type out of range): returned for any `FetchOrientation` other than
///   `SQL_FETCH_NEXT`. (DM) variants (invalid value, bookmark with USE_BOOKMARKS=OFF,
///   forward-only cursor with non-NEXT orientation) overlap with this check; HY106 is
///   returned directly without distinguishing DM vs. framework context.
/// - HY107 (row value out of range): not applicable; keyset cursors are not supported.
/// - HY111 (invalid bookmark value): not applicable; bookmarks are not supported.
/// - HY117 (connection suspended): driver-manager-handled; not returned here.
/// - HYC00 (optional feature not implemented): delegated to `sql_fetch` for `SQL_FETCH_NEXT`.
/// - HYT00 (timeout expired): not implemented.
/// - HYT01 (connection timeout expired): not implemented.
/// - IM001 (driver does not support this function): driver-manager-handled.
/// - IM017, IM018: driver-manager-handled; not returned here.
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
pub unsafe fn sql_fetch_scroll<B: Backend>(
    statement_handle: *mut c_void,
    fetch_orientation: i16,
    fetch_offset: isize,
) -> SqlReturn {
    tracing::trace!(
        "SQLFetchScroll(stmt={:?}, orientation_raw={}, offset={})",
        statement_handle,
        fetch_orientation,
        fetch_offset
    );
    let orientation = fetch_orientation_from_raw(fetch_orientation);
    tracing::debug!(
        "SQLFetchScroll(stmt={:?}, orientation={:?}, offset={})",
        statement_handle,
        orientation,
        fetch_offset
    );
    if orientation == Some(FetchOrientation::Next) {
        // Forward-only: delegate to sql_fetch.
        // SAFETY: same preconditions as this function: statement_handle is null or
        // a valid StatementHandle<B> allocated by sql_alloc_handle.
        return unsafe { sql_fetch::<B>(statement_handle) };
    }
    // SAFETY: statement_handle is null or a valid StatementHandle<B> allocated by
    // sql_alloc_handle; tag is validated by as_handle_ref inside the closure.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, || {
            Err(OdbcError::general(
                format!("SQLFetchScroll: unsupported fetch orientation {fetch_orientation}"),
                SqlState::fetch_type_out_of_range(),
            ))
        })
    };
    tracing::debug!("SQLFetchScroll -> {:?}", ret);
    ret
}

/// Generic implementation of SQLGetData.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlgetdata-function>
///
/// Retrieves data for a single column in the current row. The data is
/// converted to the requested C type and written into the caller's buffer.
///
/// # Parameters
///
/// - `statement_handle`: Statement handle (input).
/// - `col_or_param_num`: Column number (1-based) of the column to retrieve. Column 0 is the
///   bookmark column and is not supported (input).
/// - `target_type`: C data type identifier for the target buffer (input).
/// - `target_value_ptr`: Pointer to the buffer to receive the data (output). Must not be null.
/// - `buffer_length`: Length in bytes of the `*target_value_ptr` buffer (input).
/// - `str_len_or_ind_ptr`: Pointer to the buffer to receive the length or indicator value
///   (output). May be null if the caller does not need the length or indicator.
///
/// # Spec compliance
///
/// - 01000 (general warning): not returned; driver-specific informational messages are not
///   generated.
/// - 01004 (string data right truncated): returned via `write_column_value` when character
///   or binary data does not fit the buffer; diagnostic is pushed to the statement queue.
/// - 01S07 (fractional truncation): returned via `write_column_value` for numeric fractional
///   truncation, and when a non-zero `ColumnValue::Time` fraction is dropped writing to
///   `SQL_C_TYPE_TIME` (`SQL_TIME_STRUCT` has no fraction field).
/// - 07006 (restricted data type attribute violation): returned via `write_column_value`'s
///   two "unsupported conversion" fallthroughs — the column value's variant has no defined
///   conversion to the requested C type (e.g. `Bytes` requested as a numeric C type, or any
///   value/target-type combination not covered by a specific arm).
/// - 07009 (invalid descriptor index): returned when `col_or_param_num` is 0 (bookmark). The
///   column-greater-than-result-set-column-count case is delegated to the backend, which
///   returns HY000 rather than 07009; a precise 07009 check would require an extra round-trip
///   to obtain column count. (DM) variants (bound column ordering, ARD count) are
///   driver-manager-handled; not returned here.
/// - 08S01 (communication link failure): propagated from the backend.
/// - 22002 (indicator variable required but not supplied): returned when the column value
///   is NULL and `str_len_or_ind_ptr` is null.
/// - 22003 (numeric value out of range): returned via `write_column_value` when a numeric
///   pivot does not fit the requested C target type.
/// - 22007 (invalid datetime format): returned via `write_column_value` when character data
///   parses as a date/time/timestamp but a field value is out of range (e.g. month 13, or a
///   numeric field too large for its target width, e.g. year `"99999"`). Per this function's
///   diagnostics table this code (like 22018) is scoped to a character column source; a
///   non-character `ColumnValue` (e.g. a plain integer or float) requested as a datetime C type
///   has no defined conversion at all and instead falls into the generic 07006 case above. A
///   backend whose data source stores a datetime column using some other, numeric physical
///   encoding is responsible for decoding it into a proper `ColumnValue::Date`/`Time`/`Timestamp`
///   at fetch time, before it ever reaches this function — stackable-odbc-core has no such backend-specific
///   knowledge.
/// - 22012 (division by zero): propagated from the backend.
/// - 22015 (interval field overflow): not returned; `write_column_value` has no interval C type
///   arms, so a request for one falls through to the generic 07006 case above.
/// - 22018 (invalid character value for cast specification): returned via `write_column_value`
///   when character data does not parse as the requested numeric or datetime C type.
/// - 24000 (invalid cursor state): returned when `stmt.statement` is `None` (no cursor open).
///   (DM) variants (not yet fetched, before-start, after-end) are driver-manager-handled;
///   not returned here.
/// - HY000 (general error): propagated from the backend; `write_column_value` does not produce
///   this code — its coercion-failure paths return 07006 (see above).
/// - HY001 (memory allocation error): not returned; Rust panics on allocation failure.
/// - HY003 (program type out of range): returned both when `target_type` is not a recognized C
///   data type, and via `write_column_value`'s numeric-pivot catch-all for a `CDataType` with no
///   numeric arm. (DM) variants (column 0 with wrong bookmark type) are driver-manager-handled.
/// - HY008 (operation canceled): not applicable; the `Backend` trait is synchronous.
/// - HY009 (invalid use of null pointer): not checked; `target_value_ptr` null is not
///   validated. (DM) — driver-manager-handled.
/// - HY010 (function sequence error): driver-manager-handled; not returned here.
/// - HY013 (memory management error): not returned.
/// - HY090 (invalid string or buffer length): returned when `buffer_length < 0`. (DM)
///   variants (bound column buffer length checks) are driver-manager-handled.
/// - HY109 (invalid cursor position): not checked; detecting deleted/unfetchable rows
///   requires backend support.
/// - HY117 (connection suspended): driver-manager-handled; not returned here.
/// - HYC00 (optional feature not implemented): not returned by `write_column_value`; its
///   unsupported-conversion paths return 07006 (see above).
/// - HYT01 (connection timeout expired): not implemented.
/// - IM001 (driver does not support this function): driver-manager-handled.
/// - IM017, IM018: driver-manager-handled; not returned here.
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
/// `target_value_ptr` and `str_len_or_ind_ptr` must be valid writable pointers
/// (or null where documented by the ODBC spec).
pub unsafe fn sql_get_data<B: Backend>(
    statement_handle: *mut c_void,
    col_or_param_num: u16,
    target_type: i16,
    target_value_ptr: *mut c_void,
    buffer_length: isize,
    str_len_or_ind_ptr: *mut isize,
) -> SqlReturn {
    tracing::trace!(
        "SQLGetData(stmt={:?}, col={}, target_type_raw={}, buf_len={:?})",
        statement_handle,
        col_or_param_num,
        target_type,
        buffer_length
    );
    let c_type_log = crate::types::c_data_type_from_raw(target_type);
    tracing::debug!(
        "SQLGetData(stmt={:?}, col={}, c_type={:?})",
        statement_handle,
        col_or_param_num,
        c_type_log
    );
    // SAFETY: statement_handle is null or a valid StatementHandle<B> allocated by
    // sql_alloc_handle; tag is validated by as_handle_ref inside the closure.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, || {
            let stmt = as_handle_ref::<StatementHandle<B>>(statement_handle)?;
            stmt.diagnostics.clear();

            // Spec 24000 / HY010: No cursor open.
            let Some(ref mut statement) = stmt.statement else {
                return Err(OdbcError::general(
                    "No cursor is open",
                    SqlState::invalid_cursor_state(),
                ));
            };

            // Spec 07009: Column number 0 means bookmark, which we don't support.
            if col_or_param_num == 0 {
                return Err(OdbcError::general(
                    "Column number 0 (bookmark) is not supported",
                    SqlState::invalid_descriptor_index(),
                ));
            }

            // Spec HY090: buffer_length must be >= 0.
            if buffer_length < 0 {
                return Err(OdbcError::general(
                    format!("Invalid buffer_length: {buffer_length}"),
                    SqlState::invalid_string_or_buffer_length(),
                ));
            }

            let c_type = crate::types::c_data_type_from_raw(target_type).ok_or_else(|| {
                OdbcError::general(
                    format!("Unknown C data type: {target_type}"),
                    SqlState::invalid_application_buffer_type(),
                )
            })?;
            let value = statement.get_data(col_or_param_num, c_type)?;
            // Spec 22002: data is NULL but no indicator variable was supplied.
            if matches!(*value, ColumnValue::Null) && str_len_or_ind_ptr.is_null() {
                return Err(OdbcError::general(
                    "Data is NULL but no indicator variable was supplied (str_len_or_ind_ptr is null)",
                    SqlState::indicator_variable_required(),
                ));
            }
            let result = write_column_value(
                &value,
                c_type,
                target_value_ptr,
                buffer_length,
                str_len_or_ind_ptr,
            )?;

            // Spec 01004: If data was truncated, push a diagnostic.
            if result == SqlReturn::SUCCESS_WITH_INFO {
                stmt.diagnostics.push(&OdbcError::StringTruncated);
            }

            Ok(result)
        })
    };
    tracing::debug!("SQLGetData -> {:?}", ret);
    ret
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffi::handle::{sql_alloc_handle, sql_free_handle};
    use crate::test_utils::MockBackend;
    use crate::types::CDataType;
    use odbc_sys::HandleType;

    /// Helper: allocate env + connection + statement handles.
    unsafe fn alloc_env_conn_stmt() -> (*mut c_void, *mut c_void, *mut c_void) {
        let mut env: *mut c_void = std::ptr::null_mut();
        let _ = unsafe {
            sql_alloc_handle::<MockBackend>(HandleType::Env as i16, std::ptr::null_mut(), &mut env)
        };
        let mut conn: *mut c_void = std::ptr::null_mut();
        let _ = unsafe { sql_alloc_handle::<MockBackend>(HandleType::Dbc as i16, env, &mut conn) };
        let mut stmt: *mut c_void = std::ptr::null_mut();
        let _ =
            unsafe { sql_alloc_handle::<MockBackend>(HandleType::Stmt as i16, conn, &mut stmt) };
        (env, conn, stmt)
    }

    unsafe fn cleanup(env: *mut c_void, conn: *mut c_void, stmt: *mut c_void) {
        unsafe {
            let _ = sql_free_handle::<MockBackend>(HandleType::Stmt as i16, stmt);
            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn fetch_without_execute_returns_error() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = sql_fetch::<MockBackend>(stmt);
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn get_data_without_cursor_returns_error() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let mut buf: i32 = 0;
            let mut ind: isize = 0;
            let ret = sql_get_data::<MockBackend>(
                stmt,
                1,
                CDataType::SLong as i16,
                &mut buf as *mut i32 as *mut c_void,
                4,
                &mut ind,
            );
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn fetch_null_handle_returns_invalid() {
        unsafe {
            let ret = sql_fetch::<MockBackend>(std::ptr::null_mut());
            assert_eq!(ret, SqlReturn::INVALID_HANDLE);
        }
    }

    #[test]
    fn fetch_scroll_unsupported_orientation_returns_error() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            // SQL_FETCH_PRIOR (4) is not supported by forward-only cursor
            let ret = sql_fetch_scroll::<MockBackend>(stmt, 4, 0);
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn get_data_negative_buffer_length_without_cursor_returns_error() {
        // Without a cursor open, sql_get_data returns ERROR (HY010).
        // The HY090 buffer_length check fires only when a cursor is open.
        // This test verifies existing behavior is not disturbed.
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = sql_get_data::<MockBackend>(
                stmt,
                1,
                CDataType::Char as i16,
                std::ptr::null_mut(),
                -1, // negative — HY090 check fires after cursor check
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn get_data_col_zero_returns_error() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            // Manually set a statement to have a cursor open
            let handle = as_handle_ref::<StatementHandle<MockBackend>>(stmt).expect("valid handle");
            handle.statement = Some(crate::handles::StatementData::Synthetic(
                crate::synthetic::SyntheticStatement::new(
                    vec![],
                    vec![vec![crate::types::ColumnValue::I32(42)]],
                ),
            ));

            let mut buf: i32 = 0;
            let mut ind: isize = 0;
            let ret = sql_get_data::<MockBackend>(
                stmt,
                0, // bookmark column — not supported
                CDataType::SLong as i16,
                &mut buf as *mut i32 as *mut c_void,
                4,
                &mut ind,
            );
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn, stmt);
        }
    }
}
