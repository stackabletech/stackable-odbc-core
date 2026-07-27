//! Cursor and result-set entry points (`SQLNumResultCols`, `SQLRowCount`,
//! `SQLMoreResults`, `SQLCloseCursor`, `SQLCancel`, cursor names, bulk
//! operations, `SQLSetPos`).

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::backend::{Backend, StatementBackend};
use crate::errors::{IntoOdbc, OdbcError};
use crate::handles::{StatementHandle, as_handle_ref};
use crate::panic::panic_safe;
#[cfg(test)]
use crate::types::Nullable;
use crate::types::{
    SQL_ADD, SQL_DELETE, SQL_DELETE_BY_BOOKMARK, SQL_FETCH_BY_BOOKMARK, SQL_LOCK_EXCLUSIVE,
    SQL_LOCK_NO_CHANGE, SQL_LOCK_UNLOCK, SQL_POSITION, SQL_REFRESH, SQL_UPDATE,
    SQL_UPDATE_BY_BOOKMARK, SqlReturn, SqlState,
};
use crate::utf16::{utf16_to_string, write_utf16};

/// Global counter for auto-generating cursor names.
static CURSOR_NAME_COUNTER: AtomicU32 = AtomicU32::new(1);

/// Generic implementation of SQLNumResultCols.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlnumresultcols-function>
///
/// Returns the number of columns in the result set.
///
/// # Parameters
///
/// - `statement_handle`: \[Input\] Statement handle (`SQLHSTMT`).
/// - `column_count_ptr`: \[Output\] Pointer to a buffer in which to return the number of columns
///   in the result set. This count does not include a bound bookmark column. May be null (the
///   count is computed but not written).
///
/// # Spec compliance
///
/// Diagnostics table from the ODBC spec:
///
/// - 01000 (general warning): driver-specific informational message — not produced here.
/// - 08S01 (communication link failure): not applicable; the framework is in-process.
/// - HY000 (general error): returned via `OdbcError::general` for unexpected failures.
/// - HY001 (memory allocation error): not applicable; Rust allocation panics are caught by
///   `panic_safe`.
/// - HY008 (operation canceled): (driver-manager-handled; not returned here)
/// - HY010 (function sequence error): returned with SQLSTATE `HY010` when no result set is
///   available (statement not yet executed). The `(DM)` variants (async in progress, etc.) are
///   driver-manager-handled; not returned here.
/// - HY013 (memory management error): not applicable; Rust memory access cannot fail silently.
/// - HY117 (connection suspended): (driver-manager-handled; not returned here)
/// - HYT01 (connection timeout): not applicable; the framework is in-process.
/// - IM001 (driver does not support function): (driver-manager-handled; not returned here)
/// - IM017 (polling disabled in async notification mode): (driver-manager-handled; not returned here)
/// - IM018 (SQLCompleteAsync not called): (driver-manager-handled; not returned here)
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
/// `column_count_ptr` must be a valid writable pointer.
pub unsafe fn sql_num_result_cols<B: Backend>(
    statement_handle: *mut c_void,
    column_count_ptr: *mut i16,
) -> SqlReturn {
    tracing::debug!(
        "SQLNumResultCols(stmt={:?}, count_ptr={:?})",
        statement_handle,
        column_count_ptr
    );
    // SAFETY: statement_handle is either null or a valid StatementHandle<B> pointer
    // previously allocated by sql_alloc_handle. panic_safe validates the handle tag
    // via as_handle_ref before any cast, and catches any panics. column_count_ptr
    // is only written through after a null check inside the closure.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, || {
            let stmt = as_handle_ref::<StatementHandle<B>>(statement_handle)?;
            stmt.diagnostics.clear();

            // Spec HY010: No result set.
            let Some(ref statement) = stmt.statement else {
                return Err(OdbcError::general(
                    "No result set available; statement not executed",
                    SqlState::function_sequence_error(),
                ));
            };

            if !column_count_ptr.is_null() {
                // SAFETY: column_count_ptr is non-null (checked above) and the
                // caller guarantees it points to a valid writable i16.
                // No narrowing here any more: `column_count` is already the
                // `SQLSMALLINT` the ABI writes, so a backend that cannot express
                // its count has to say so where it knows the real number,
                // instead of core clamping a value it cannot interpret.
                std::ptr::write_unaligned(column_count_ptr, statement.column_count());
            }

            Ok(SqlReturn::SUCCESS)
        })
    };
    tracing::debug!("SQLNumResultCols -> {:?}", ret);
    ret
}

/// Generic implementation of SQLRowCount.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlrowcount-function>
///
/// Returns the number of rows affected by an UPDATE, INSERT, or DELETE
/// statement, or -1 if unknown.
///
/// # Parameters
///
/// - `statement_handle`: \[Input\] Statement handle (`SQLHSTMT`).
/// - `row_count_ptr`: \[Output\] Points to a buffer in which to return the row count. For UPDATE,
///   INSERT, and DELETE statements the value is the number of affected rows, or -1 if not
///   available. For other statements the value is driver-defined. May be null (the count is
///   computed but not written).
///
/// # Spec compliance
///
/// Diagnostics table from the ODBC spec:
///
/// - 01000 (general warning): driver-specific informational message — not produced here.
/// - HY000 (general error): returned via `OdbcError::general` for unexpected failures.
/// - HY001 (memory allocation error): not applicable; Rust allocation panics are caught by
///   `panic_safe`.
/// - HY010 (function sequence error): the spec lists this for cases where no statement has been
///   executed yet; `SUCCESS` with row count 0 is returned in that case (no-execute path),
///   which is consistent with the spec's "driver-defined" rule for non-DML statements. The `(DM)`
///   variants (async in progress, etc.) are driver-manager-handled; not returned here.
/// - HY013 (memory management error): not applicable; Rust memory access cannot fail silently.
/// - HY117 (connection suspended): (driver-manager-handled; not returned here)
/// - HYT01 (connection timeout): not applicable; the framework is in-process.
/// - IM001 (driver does not support function): (driver-manager-handled; not returned here)
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
/// `row_count_ptr` must be a valid writable pointer.
pub unsafe fn sql_row_count<B: Backend>(
    statement_handle: *mut c_void,
    row_count_ptr: *mut isize,
) -> SqlReturn {
    tracing::debug!("SQLRowCount(stmt={:?})", statement_handle);
    // SAFETY: statement_handle is either null or a valid StatementHandle<B> pointer
    // previously allocated by sql_alloc_handle. panic_safe validates the handle tag
    // via as_handle_ref before any cast, and catches any panics. row_count_ptr
    // is only written through after a null check inside the closure.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, || {
            let stmt = as_handle_ref::<StatementHandle<B>>(statement_handle)?;
            stmt.diagnostics.clear();

            let row_count = match stmt.statement {
                Some(ref statement) => match statement.row_count() {
                    // A backend that knows the count but cannot determine it
                    // reports `Some(SQL_NO_TOTAL)` itself; core does not
                    // second-guess a value it was given.
                    Some(n) => isize::try_from(n).unwrap_or_else(|_| {
                        tracing::warn!(
                            "SQLRowCount: row count {n} does not fit SQLLEN on this target; \
                             reporting -1 (not available)"
                        );
                        -1
                    }),
                    // Not applicable to this statement — distinct from the
                    // backend saying it could not work the count out.
                    None => -1,
                },
                None => 0, // no statement executed
            };

            if !row_count_ptr.is_null() {
                // SAFETY: row_count_ptr is non-null (checked above) and the
                // caller guarantees it points to a valid writable isize.
                std::ptr::write_unaligned(row_count_ptr, row_count);
            }

            Ok(SqlReturn::SUCCESS)
        })
    };
    tracing::debug!("SQLRowCount -> {:?}", ret);
    ret
}

/// Generic implementation of SQLMoreResults.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlmoreresults-function>
///
/// Always returns `SQL_NO_DATA`; only single result sets are supported.
///
/// # Parameters
///
/// - `statement_handle`: \[Input\] Statement handle (`SQLHSTMT`).
///
/// # Spec compliance
///
/// Diagnostics table from the ODBC spec:
///
/// - 01000 (general warning): driver-specific informational message — not produced here.
/// - 01S02 (option value changed): would be returned if a statement attribute changed while
///   processing a batch; not applicable as `SQL_NO_DATA` is always returned immediately.
/// - 08S01 (communication link failure): not applicable; the framework is in-process.
/// - 40001 (serialization failure): not applicable; the `Backend` trait is synchronous and
///   does not return partial batch results.
/// - 40003 (statement completion unknown): not applicable; the framework is in-process.
/// - HY000 (general error): returned via `OdbcError::general` for unexpected failures.
/// - HY001 (memory allocation error): not applicable; Rust allocation panics are caught by
///   `panic_safe`.
/// - HY008 (operation canceled): (driver-manager-handled; not returned here)
/// - HY010 (function sequence error): (driver-manager-handled; not returned here)
/// - HY013 (memory management error): not applicable; Rust memory access cannot fail silently.
/// - HY117 (connection suspended): (driver-manager-handled; not returned here)
/// - HYT01 (connection timeout): not applicable; the framework is in-process.
/// - IM001 (driver does not support function): (driver-manager-handled; not returned here)
/// - IM017 (polling disabled in async notification mode): (driver-manager-handled; not returned here)
/// - IM018 (SQLCompleteAsync not called): (driver-manager-handled; not returned here)
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
pub unsafe fn sql_more_results<B: Backend>(statement_handle: *mut c_void) -> SqlReturn {
    tracing::debug!("SQLMoreResults(stmt={:?})", statement_handle);
    // SAFETY: statement_handle is either null or a valid StatementHandle<B> pointer
    // previously allocated by sql_alloc_handle. panic_safe validates the handle tag
    // via as_handle_ref before any cast, and catches any panics.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, || {
            let stmt = as_handle_ref::<StatementHandle<B>>(statement_handle)?;
            stmt.diagnostics.clear();
            Ok(SqlReturn::NO_DATA)
        })
    };
    tracing::debug!("SQLMoreResults -> {:?}", ret);
    ret
}

/// Generic implementation of SQLCloseCursor.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlclosecursor-function>
///
/// Closes the cursor associated with the statement. The statement remains
/// allocated with its result set metadata intact, but the cursor position
/// is reset.
///
/// # Parameters
///
/// - `statement_handle`: \[Input\] Statement handle (`SQLHSTMT`).
///
/// # Spec compliance
///
/// Diagnostics table from the ODBC spec:
///
/// - 01000 (general warning): driver-specific informational message — not produced here.
/// - 24000 (invalid cursor state): returned with SQLSTATE `24000` when no cursor is open on the
///   statement handle (ODBC 3.x driver behaviour). A statement that is only prepared, or that
///   executed without producing a result set, has no cursor to close and gets this code, as does
///   one whose cursor `SQLEndTran` already closed under `SQL_CB_CLOSE`.
/// - HY000 (general error): returned via `OdbcError::general` for unexpected failures.
/// - HY001 (memory allocation error): not applicable; Rust allocation panics are caught by
///   `panic_safe`.
/// - HY010 (function sequence error): (driver-manager-handled; not returned here)
/// - HY013 (memory management error): not applicable; Rust memory access cannot fail silently.
/// - HY117 (connection suspended): (driver-manager-handled; not returned here)
/// - HYT01 (connection timeout): not applicable; the framework is in-process.
/// - IM001 (driver does not support function): (driver-manager-handled; not returned here)
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
pub unsafe fn sql_close_cursor<B: Backend>(statement_handle: *mut c_void) -> SqlReturn {
    tracing::debug!("SQLCloseCursor(stmt={:?})", statement_handle);
    // SAFETY: statement_handle is either null or a valid StatementHandle<B> pointer
    // previously allocated by sql_alloc_handle. panic_safe validates the handle tag
    // via as_handle_ref before any cast, and catches any panics.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, || {
            let stmt = as_handle_ref::<StatementHandle<B>>(statement_handle)?;
            stmt.diagnostics.clear();

            // Spec 24000: No cursor open. A statement that is merely prepared
            // (S2/S3) has a `statement` but no cursor, so this reads
            // `cursor_open`.
            if !stmt.cursor_open {
                return Err(OdbcError::general(
                    "No cursor is open",
                    SqlState::invalid_cursor_state(),
                ));
            }

            // Discard the result set. After this the statement handle is in a
            // clean state and SQLExecDirect / SQLExecute can be called again.
            stmt.discard_result_set();
            Ok(SqlReturn::SUCCESS)
        })
    };
    tracing::debug!("SQLCloseCursor -> {:?}", ret);
    ret
}

/// Generic implementation of SQLCancel.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlcancel-function>
///
/// Cancels an asynchronous operation on a statement. The `Backend` trait is synchronous
/// (there is nothing to cancel), so this is a no-op that returns SUCCESS as long
/// as the handle is valid.
///
/// # Parameters
///
/// - `statement_handle`: \[Input\] Statement handle (`SQLHSTMT`).
///
/// # Spec compliance
///
/// Diagnostics table from the ODBC spec:
///
/// - 01000 (general warning): driver-specific informational message — not produced here.
/// - HY000 (general error): returned via `OdbcError::general` for unexpected failures.
/// - HY001 (memory allocation error): not applicable; Rust allocation panics are caught by
///   `panic_safe`.
/// - HY010 (function sequence error): (driver-manager-handled; not returned here)
/// - HY013 (memory management error): not applicable; Rust memory access cannot fail silently.
/// - HY018 (server declined cancel request): not applicable; the `Backend` trait is synchronous
///   and the cancel is always a no-op.
/// - HY117 (connection suspended): (driver-manager-handled; not returned here)
/// - HYT01 (connection timeout): not applicable; the framework is in-process.
/// - IM001 (driver does not support function): (driver-manager-handled; not returned here)
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
pub unsafe fn sql_cancel<B: Backend>(statement_handle: *mut c_void) -> SqlReturn {
    tracing::debug!("SQLCancel(stmt={:?})", statement_handle);
    // SAFETY: statement_handle is either null or a valid StatementHandle<B> pointer
    // previously allocated by sql_alloc_handle. panic_safe validates the handle tag
    // via as_handle_ref before any cast, and catches any panics.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, || {
            let stmt = as_handle_ref::<StatementHandle<B>>(statement_handle)?;
            stmt.diagnostics.clear();

            // `ref mut`: cancel must clear the statement's streaming state
            // (see `Backend::cancel`).
            if let Some(crate::handles::StatementData::Backend(ref mut backend_stmt)) =
                stmt.statement
            {
                // Converted before matching: `B::cancel` now reports
                // `B::Error`, and the arm below has to recognise core's own
                // `NotImplemented` inside it.
                match B::cancel(backend_stmt).into_odbc() {
                    Ok(()) => {}
                    // NotImplemented is fine; no pending operation to cancel.
                    Err(crate::errors::OdbcError::NotImplemented { .. }) => {}
                    Err(e) => return Err(e),
                }
            }

            Ok(SqlReturn::SUCCESS)
        })
    };
    tracing::debug!("SQLCancel -> {:?}", ret);
    ret
}

/// Generic implementation of SQLGetCursorNameW.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlgetcursorname-function>
///
/// Returns the cursor name associated with the statement. If no cursor name has been set
/// via `SQLSetCursorNameW`, an implementation-defined name of the form `SQL_CURSRnnnn` is
/// auto-generated and stored on the handle.
///
/// # Parameters
///
/// - `statement_handle`: \[Input\] Statement handle (`SQLHSTMT`).
/// - `cursor_name`: \[Output\] Pointer to a buffer in which to return the cursor name (UTF-16).
/// - `buffer_length`: \[Input\] Length of `cursor_name` buffer in UTF-16 code units.
/// - `name_length_ptr`: \[Output\] Pointer to a buffer in which to return the total number of
///   UTF-16 code units (excluding null terminator) available to return.
///
/// # Spec compliance
///
/// Diagnostics table from the ODBC spec:
///
/// - 01000 (general warning): driver-specific informational message — not produced here.
/// - 01004 (string data, right truncated): returned when the cursor name is longer than
///   `buffer_length`; handled by `write_utf16`.
/// - HY000 (general error): returned via `OdbcError::general` for unexpected failures.
/// - HY001 (memory allocation error): not applicable; Rust allocation panics are caught by
///   `panic_safe`.
/// - HY010 (function sequence error): (driver-manager-handled; not returned here)
/// - HY013 (memory management error): not applicable; Rust memory access cannot fail silently.
/// - HY015 (no cursor name available): not applicable; this implementation auto-generates a
///   name when none has been set.
/// - HY117 (connection suspended): (driver-manager-handled; not returned here)
/// - HYT01 (connection timeout): not applicable; the framework is in-process.
/// - IM001 (driver does not support function): (driver-manager-handled; not returned here)
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
/// `cursor_name` must be a valid writable buffer of at least `buffer_length` UTF-16 code units,
/// or null. `name_length_ptr` must be a valid writable pointer or null.
pub unsafe fn sql_get_cursor_name_w<B: Backend>(
    statement_handle: *mut c_void,
    cursor_name: *mut u16,
    buffer_length: i16,
    name_length_ptr: *mut i16,
) -> SqlReturn {
    tracing::debug!(
        "SQLGetCursorNameW(stmt={:?}, buf_len={})",
        statement_handle,
        buffer_length
    );
    // SAFETY: statement_handle is either null or a valid StatementHandle<B> pointer
    // previously allocated by sql_alloc_handle. panic_safe validates the handle tag
    // via as_handle_ref before any cast, and catches any panics. cursor_name and
    // name_length_ptr are only written through by write_utf16, which performs its
    // own null checks.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, || {
            let stmt = as_handle_ref::<StatementHandle<B>>(statement_handle)?;
            stmt.diagnostics.clear();

            // Auto-generate a cursor name if none has been set.
            if stmt.cursor_name.is_none() {
                let n = CURSOR_NAME_COUNTER.fetch_add(1, Ordering::Relaxed);
                stmt.cursor_name = Some(format!("SQL_CURSR{n:04}"));
            }

            let name = stmt.cursor_name.as_deref().unwrap_or("");

            // write_utf16 handles truncation (01004 → SUCCESS_WITH_INFO),
            // null output pointer, and length reporting.
            Ok(write_utf16(
                name,
                cursor_name,
                buffer_length,
                name_length_ptr,
            ))
        })
    };
    tracing::debug!("SQLGetCursorNameW -> {:?}", ret);
    ret
}

/// Generic implementation of SQLSetCursorNameW.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlsetcursorname-function>
///
/// Sets the cursor name associated with the statement handle.
///
/// # Parameters
///
/// - `statement_handle`: \[Input\] Statement handle (`SQLHSTMT`).
/// - `cursor_name`: \[Input\] Pointer to a UTF-16 encoded cursor name string.
/// - `name_length`: \[Input\] Length of `cursor_name` in UTF-16 code units, or `SQL_NTS`.
///
/// # Spec compliance
///
/// Diagnostics table from the ODBC spec:
///
/// - 34000 (invalid cursor name): not returned; any non-empty name is accepted.
/// - 3C000 (duplicate cursor name): not enforced; cursor name uniqueness across statements
///   is not validated.
/// - HY000 (general error): returned via `OdbcError::general` for unexpected failures.
/// - HY001 (memory allocation error): not applicable; Rust allocation panics are caught by
///   `panic_safe`.
/// - HY009 (invalid use of null pointer): returned when `cursor_name` is a null pointer.
/// - HY010 (function sequence error): (driver-manager-handled; not returned here)
/// - HY013 (memory management error): not applicable; Rust memory access cannot fail silently.
/// - HY090 (invalid string or buffer length): returned when the cursor name is empty.
/// - HY117 (connection suspended): (driver-manager-handled; not returned here)
/// - HYT01 (connection timeout): not applicable; the framework is in-process.
/// - IM001 (driver does not support function): (driver-manager-handled; not returned here)
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
/// `cursor_name` must be a valid readable UTF-16 string of at least `name_length` code units,
/// or null-terminated if `name_length` is `SQL_NTS`.
pub unsafe fn sql_set_cursor_name_w<B: Backend>(
    statement_handle: *mut c_void,
    cursor_name: *const u16,
    name_length: i16,
) -> SqlReturn {
    tracing::debug!(
        "SQLSetCursorNameW(stmt={:?}, name_ptr={:?}, name_len={})",
        statement_handle,
        cursor_name,
        name_length
    );
    // SAFETY: statement_handle is either null or a valid StatementHandle<B> pointer
    // previously allocated by sql_alloc_handle. panic_safe validates the handle tag
    // via as_handle_ref before any cast, and catches any panics. cursor_name is read
    // by utf16_to_string which handles null pointers by returning None.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, || {
            let stmt = as_handle_ref::<StatementHandle<B>>(statement_handle)?;
            stmt.diagnostics.clear();

            // Spec HY009: null pointer.
            let name = utf16_to_string(cursor_name, i32::from(name_length)).map_err(|_| {
                OdbcError::general(
                    "Cursor name pointer is null",
                    SqlState::invalid_use_of_null_pointer(),
                )
            })?;

            // Spec HY090: empty name.
            if name.is_empty() {
                return Err(OdbcError::general(
                    "Cursor name must not be empty",
                    SqlState::invalid_string_or_buffer_length(),
                ));
            }

            stmt.cursor_name = Some(name);
            Ok(SqlReturn::SUCCESS)
        })
    };
    tracing::debug!("SQLSetCursorNameW -> {:?}", ret);
    ret
}

/// Generic implementation of SQLBulkOperations.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlbulkoperations-function>
///
/// This driver uses forward-only cursors with no bookmark support. All valid `Operation`
/// values are recognised but the operation is immediately rejected with `HYC00` (optional
/// feature not implemented).
///
/// # Parameters
///
/// - `statement_handle`: \[Input\] Statement handle (`SQLHSTMT`).
/// - `operation`: \[Input\] Type of bulk operation to perform. One of `SQL_ADD` (4),
///   `SQL_UPDATE_BY_BOOKMARK` (5), `SQL_DELETE_BY_BOOKMARK` (6), or `SQL_FETCH_BY_BOOKMARK` (7).
///
/// # Spec compliance
///
/// Diagnostics table from the ODBC spec:
///
/// - 01000 (general warning): driver-specific informational message — not produced here.
/// - 01004 (string data, right truncated): not applicable; no data movement occurs.
/// - 01S01 (error in row): not applicable; HYC00 is returned before any row processing.
/// - 07006 (restricted data type attribute violation): not applicable; HYC00 precedes data conversion.
/// - 07009 (invalid descriptor index): not applicable; HYC00 precedes column access.
/// - 21S02 (degree of derived table does not match column list): not applicable.
/// - 22001 (string data right truncation): not applicable.
/// - 22003 (numeric value out of range): not applicable.
/// - 22007 (invalid datetime format): not applicable.
/// - 22008 (datetime field overflow): not applicable.
/// - 22015 (interval field overflow): not applicable.
/// - 22018 (invalid character value for cast specification): not applicable.
/// - 23000 (integrity constraint violation): not applicable.
/// - 24000 (invalid cursor state): not applicable; HYC00 is returned first.
/// - 40001 (serialization failure): not applicable.
/// - 40003 (statement completion unknown): not applicable.
/// - 42000 (syntax error or access violation): not applicable.
/// - 44000 (WITH CHECK OPTION violation): not applicable.
/// - HY000 (general error): returned via `OdbcError::general` for unexpected failures.
/// - HY001 (memory allocation error): not applicable; Rust allocation panics are caught by
///   `panic_safe`.
/// - HY008 (operation canceled): (driver-manager-handled; not returned here)
/// - HY010 (function sequence error): (driver-manager-handled; not returned here)
/// - HY011 (attribute cannot be set now): not applicable.
/// - HY013 (memory management error): not applicable; Rust memory access cannot fail silently.
/// - HY090 (invalid string or buffer length): not applicable; HYC00 precedes buffer access.
/// - HY092 (invalid attribute/option identifier): returned when `operation` is not one of the
///   four valid values defined by the spec (`SQL_ADD`, `SQL_UPDATE_BY_BOOKMARK`,
///   `SQL_DELETE_BY_BOOKMARK`, `SQL_FETCH_BY_BOOKMARK`).
/// - HY117 (connection suspended): (driver-manager-handled; not returned here)
/// - HYC00 (optional feature not implemented): returned for all valid `operation` values —
///   this driver does not support bulk operations or bookmarks.
/// - HYT00 (timeout expired): not applicable; the framework is in-process.
/// - HYT01 (connection timeout): not applicable; the framework is in-process.
/// - IM001 (driver does not support function): (driver-manager-handled; not returned here)
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
pub unsafe fn sql_bulk_operations<B: Backend>(
    statement_handle: *mut c_void,
    operation: i16,
) -> SqlReturn {
    tracing::trace!(
        "SQLBulkOperations(stmt={:?}, raw_operation={})",
        statement_handle,
        operation
    );
    // SAFETY: statement_handle is either null or a valid StatementHandle<B> pointer
    // previously allocated by sql_alloc_handle. panic_safe validates the handle tag
    // via as_handle_ref before any cast, and catches any panics.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, || {
            let stmt = as_handle_ref::<StatementHandle<B>>(statement_handle)?;
            stmt.diagnostics.clear();

            tracing::debug!("SQLBulkOperations(operation={})", operation);

            // Spec HY092: validate operation.
            match operation {
                SQL_ADD
                | SQL_UPDATE_BY_BOOKMARK
                | SQL_DELETE_BY_BOOKMARK
                | SQL_FETCH_BY_BOOKMARK => {}
                _ => {
                    return Err(OdbcError::general(
                        format!("Unknown bulk operation: {operation}"),
                        SqlState::invalid_attribute_option_identifier(),
                    ));
                }
            }

            // HYC00: this driver does not support bulk operations.
            Err(OdbcError::general(
                "SQLBulkOperations is not supported; this driver uses forward-only cursors without bookmark support",
                SqlState::optional_feature_not_implemented(),
            ))
        })
    };
    tracing::debug!("SQLBulkOperations -> {:?}", ret);
    ret
}

/// Generic implementation of SQLSetPos.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlsetpos-function>
///
/// This driver uses forward-only cursors. All valid `Operation`/`LockType` combinations are
/// recognised but immediately rejected with `HYC00` (optional feature not implemented).
///
/// # Parameters
///
/// - `statement_handle`: \[Input\] Statement handle (`SQLHSTMT`).
/// - `row_number`: \[Input\] Position of the row in the rowset on which to perform the operation.
///   If `row_number` is 0, the operation applies to every row in the rowset.
/// - `operation`: \[Input\] Operation to perform. One of `SQL_POSITION` (0), `SQL_REFRESH` (1),
///   `SQL_UPDATE` (2), or `SQL_DELETE` (3).
/// - `lock_type`: \[Input\] Lock to apply after the operation. One of `SQL_LOCK_NO_CHANGE` (0),
///   `SQL_LOCK_EXCLUSIVE` (1), or `SQL_LOCK_UNLOCK` (2).
///
/// # Spec compliance
///
/// Diagnostics table from the ODBC spec:
///
/// - 01000 (general warning): driver-specific informational message — not produced here.
/// - 01001 (cursor operation conflict): not applicable; HYC00 is returned before any operation.
/// - 01004 (string data, right truncated): not applicable.
/// - 01S01 (error in row): not applicable.
/// - 01S07 (fractional truncation): not applicable.
/// - 07006 (restricted data type attribute violation): not applicable.
/// - 07009 (invalid descriptor index): not applicable.
/// - 21S02 (degree of derived table does not match column list): not applicable.
/// - 22001 (string data right truncation): not applicable.
/// - 22003 (numeric value out of range): not applicable.
/// - 22007 (invalid datetime format): not applicable.
/// - 22008 (datetime field overflow): not applicable.
/// - 22015 (interval field overflow): not applicable.
/// - 22018 (invalid character value for cast specification): not applicable.
/// - 23000 (integrity constraint violation): not applicable.
/// - 24000 (invalid cursor state): not applicable; HYC00 is returned first.
/// - 40001 (serialization failure): not applicable.
/// - 40003 (statement completion unknown): not applicable.
/// - 42000 (syntax error or access violation): not applicable.
/// - 44000 (WITH CHECK OPTION violation): not applicable.
/// - HY000 (general error): returned via `OdbcError::general` for unexpected failures.
/// - HY001 (memory allocation error): not applicable; Rust allocation panics are caught by
///   `panic_safe`.
/// - HY008 (operation canceled): (driver-manager-handled; not returned here)
/// - HY010 (function sequence error): (driver-manager-handled; not returned here)
/// - HY011 (attribute cannot be set now): not applicable.
/// - HY013 (memory management error): not applicable; Rust memory access cannot fail silently.
/// - HY090 (invalid string or buffer length): not applicable.
/// - HY092 (invalid attribute/option identifier): returned when `operation` is not one of
///   `SQL_POSITION`, `SQL_REFRESH`, `SQL_UPDATE`, `SQL_DELETE`, or when `lock_type` is not
///   one of `SQL_LOCK_NO_CHANGE`, `SQL_LOCK_EXCLUSIVE`, `SQL_LOCK_UNLOCK`.
/// - HY107 (row value out of range): not applicable; HYC00 is returned before row validation.
/// - HY109 (invalid cursor position): not applicable; HYC00 is returned before cursor checks.
/// - HY117 (connection suspended): (driver-manager-handled; not returned here)
/// - HYC00 (optional feature not implemented): returned for all valid `operation`/`lock_type`
///   combinations — this driver does not support scrollable cursors or positioned operations.
/// - HYT00 (timeout expired): not applicable; the framework is in-process.
/// - HYT01 (connection timeout): not applicable; the framework is in-process.
/// - IM001 (driver does not support function): (driver-manager-handled; not returned here)
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
pub unsafe fn sql_set_pos<B: Backend>(
    statement_handle: *mut c_void,
    row_number: u64,
    operation: u16,
    lock_type: u16,
) -> SqlReturn {
    tracing::trace!(
        "SQLSetPos(stmt={:?}, row={}, raw_operation={}, raw_lock_type={})",
        statement_handle,
        row_number,
        operation,
        lock_type
    );
    // SAFETY: statement_handle is either null or a valid StatementHandle<B> pointer
    // previously allocated by sql_alloc_handle. panic_safe validates the handle tag
    // via as_handle_ref before any cast, and catches any panics.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, || {
            let stmt = as_handle_ref::<StatementHandle<B>>(statement_handle)?;
            stmt.diagnostics.clear();

            tracing::debug!(
                "SQLSetPos(row={}, operation={}, lock_type={})",
                row_number,
                operation,
                lock_type
            );

            // Spec HY092: validate operation.
            match operation {
                SQL_POSITION | SQL_REFRESH | SQL_UPDATE | SQL_DELETE => {}
                _ => {
                    return Err(OdbcError::general(
                        format!("Unknown SQLSetPos operation: {operation}"),
                        SqlState::invalid_attribute_option_identifier(),
                    ));
                }
            }

            // Spec HY092: validate lock_type.
            match lock_type {
                SQL_LOCK_NO_CHANGE | SQL_LOCK_EXCLUSIVE | SQL_LOCK_UNLOCK => {}
                _ => {
                    return Err(OdbcError::general(
                        format!("Unknown SQLSetPos lock type: {lock_type}"),
                        SqlState::invalid_attribute_option_identifier(),
                    ));
                }
            }

            // HYC00: this driver does not support positioned cursor operations.
            Err(OdbcError::general(
                "SQLSetPos is not supported; this driver uses forward-only cursors",
                SqlState::optional_feature_not_implemented(),
            ))
        })
    };
    tracing::debug!("SQLSetPos -> {:?}", ret);
    ret
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffi::handle::{sql_alloc_handle, sql_free_handle};
    use crate::test_utils::MockBackend;
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

    unsafe fn cleanup(env: *mut c_void, conn: *mut c_void, stmt_ptr: *mut c_void) {
        unsafe {
            let _ = sql_free_handle::<MockBackend>(HandleType::Stmt as i16, stmt_ptr);
            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn num_result_cols_without_execute_returns_error() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let mut count: i16 = 0;
            let ret = sql_num_result_cols::<MockBackend>(stmt, &mut count);
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn, stmt);
        }
    }

    /// SQLNumResultCols with a null output pointer and no result set should
    /// return ERROR (HY010 fires before the null-pointer check).
    #[test]
    fn num_result_cols_null_output_ptr_returns_error_when_no_result_set() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            // No statement executed → HY010, regardless of output pointer.
            let ret = sql_num_result_cols::<MockBackend>(stmt, std::ptr::null_mut());
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn num_result_cols_with_synthetic_statement() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let handle = as_handle_ref::<StatementHandle<MockBackend>>(stmt).expect("valid");
            handle.statement = Some(crate::handles::StatementData::Synthetic(
                crate::synthetic::SyntheticStatement::new(
                    vec![
                        crate::types::ColumnDescriptor {
                            name: "col1".into(),
                            type_name: String::new(),
                            sql_type: crate::types::SqlDataType(4), // INTEGER
                            precision: 10,
                            scale: 0,
                            nullable: Nullable::SqlNullable,
                            ..Default::default()
                        },
                        crate::types::ColumnDescriptor {
                            name: "col2".into(),
                            type_name: String::new(),
                            sql_type: crate::types::SqlDataType(12), // VARCHAR
                            precision: 255,
                            scale: 0,
                            nullable: Nullable::SqlNullable,
                            ..Default::default()
                        },
                    ],
                    vec![],
                ),
            ));

            let mut count: i16 = 0;
            let ret = sql_num_result_cols::<MockBackend>(stmt, &mut count);
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(count, 2);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn row_count_without_statement_returns_zero() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let mut count: isize = -999;
            let ret = sql_row_count::<MockBackend>(stmt, &mut count);
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(count, 0);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn more_results_always_returns_no_data() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = sql_more_results::<MockBackend>(stmt);
            assert_eq!(ret, SqlReturn::NO_DATA);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn close_cursor_without_statement_returns_error() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = sql_close_cursor::<MockBackend>(stmt);
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn close_cursor_with_open_cursor_succeeds() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let handle = as_handle_ref::<StatementHandle<MockBackend>>(stmt).expect("valid");
            handle.set_result_set(crate::handles::StatementData::Synthetic(
                crate::test_utils::synthetic_result_set(vec![]),
            ));

            let ret = sql_close_cursor::<MockBackend>(stmt);
            assert_eq!(ret, SqlReturn::SUCCESS);
            cleanup(env, conn, stmt);
        }
    }

    // -----------------------------------------------------------------------
    // row_count with synthetic statement (returns Some)
    // -----------------------------------------------------------------------

    #[test]
    fn row_count_with_synthetic_statement_returns_count() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let handle = as_handle_ref::<StatementHandle<MockBackend>>(stmt).expect("valid");
            handle.statement = Some(crate::handles::StatementData::Synthetic(
                crate::synthetic::SyntheticStatement::new(
                    vec![crate::types::ColumnDescriptor {
                        name: "col".into(),
                        type_name: String::new(),
                        sql_type: crate::types::SqlDataType(4),
                        precision: 10,
                        scale: 0,
                        nullable: Nullable::SqlNullable,
                        ..Default::default()
                    }],
                    vec![
                        vec![crate::types::ColumnValue::I32(1)],
                        vec![crate::types::ColumnValue::I32(2)],
                        vec![crate::types::ColumnValue::I32(3)],
                    ],
                ),
            ));

            let mut count: isize = -999;
            let ret = sql_row_count::<MockBackend>(stmt, &mut count);
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(count, 3);
            cleanup(env, conn, stmt);
        }
    }

    // -----------------------------------------------------------------------
    // row_count with mock backend statement (row_count returns None → -1)
    // -----------------------------------------------------------------------

    #[test]
    fn row_count_with_mock_statement_returns_unknown() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let handle = as_handle_ref::<StatementHandle<MockBackend>>(stmt).expect("valid");
            // MockStatement's default row_count() returns None
            handle.statement = Some(crate::handles::StatementData::Backend(
                crate::test_utils::MockStatement,
            ));

            let mut count: isize = 0;
            let ret = sql_row_count::<MockBackend>(stmt, &mut count);
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(count, -1); // unknown
            cleanup(env, conn, stmt);
        }
    }

    // -----------------------------------------------------------------------
    // Null count pointers
    // -----------------------------------------------------------------------

    #[test]
    fn num_result_cols_with_null_count_pointer() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let handle = as_handle_ref::<StatementHandle<MockBackend>>(stmt).expect("valid");
            handle.statement = Some(crate::handles::StatementData::Synthetic(
                crate::synthetic::SyntheticStatement::new(vec![], vec![]),
            ));

            let ret = sql_num_result_cols::<MockBackend>(stmt, std::ptr::null_mut());
            assert_eq!(ret, SqlReturn::SUCCESS);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn row_count_with_null_count_pointer() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = sql_row_count::<MockBackend>(stmt, std::ptr::null_mut());
            assert_eq!(ret, SqlReturn::SUCCESS);
            cleanup(env, conn, stmt);
        }
    }

    // -----------------------------------------------------------------------
    // close_cursor resets fetch position
    // -----------------------------------------------------------------------

    #[test]
    fn close_cursor_resets_fetch_position() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let handle = as_handle_ref::<StatementHandle<MockBackend>>(stmt).expect("valid");
            handle.set_result_set(crate::handles::StatementData::Synthetic(
                crate::test_utils::synthetic_result_set(vec![vec![
                    crate::types::ColumnValue::I32(1),
                ]]),
            ));

            // Fetch the one row
            let statement = handle.statement.as_mut().expect("statement");
            assert_eq!(
                statement.fetch().expect("fetch"),
                crate::types::FetchResult::Row
            );
            assert_eq!(
                statement.fetch().expect("fetch"),
                crate::types::FetchResult::NoData
            );

            // Close cursor via FFI; discards the result set entirely.
            let ret = sql_close_cursor::<MockBackend>(stmt);
            assert_eq!(ret, SqlReturn::SUCCESS);

            // After close_cursor the statement is gone; the handle is clean.
            let handle = as_handle_ref::<StatementHandle<MockBackend>>(stmt).expect("valid");
            assert!(handle.statement.is_none());

            // Closing an already-closed cursor returns 24000.
            let ret2 = sql_close_cursor::<MockBackend>(stmt);
            assert_eq!(ret2, SqlReturn::ERROR);

            cleanup(env, conn, stmt);
        }
    }

    // -----------------------------------------------------------------------
    // Invalid handles
    // -----------------------------------------------------------------------

    #[test]
    fn num_result_cols_null_handle_returns_invalid() {
        unsafe {
            let mut count: i16 = 0;
            let ret = sql_num_result_cols::<MockBackend>(std::ptr::null_mut(), &mut count);
            assert_eq!(ret, SqlReturn::INVALID_HANDLE);
        }
    }

    #[test]
    fn row_count_null_handle_returns_invalid() {
        unsafe {
            let mut count: isize = 0;
            let ret = sql_row_count::<MockBackend>(std::ptr::null_mut(), &mut count);
            assert_eq!(ret, SqlReturn::INVALID_HANDLE);
        }
    }

    #[test]
    fn more_results_null_handle_returns_invalid() {
        unsafe {
            let ret = sql_more_results::<MockBackend>(std::ptr::null_mut());
            assert_eq!(ret, SqlReturn::INVALID_HANDLE);
        }
    }

    #[test]
    fn close_cursor_null_handle_returns_invalid() {
        unsafe {
            let ret = sql_close_cursor::<MockBackend>(std::ptr::null_mut());
            assert_eq!(ret, SqlReturn::INVALID_HANDLE);
        }
    }

    // -----------------------------------------------------------------------
    // SQLCancel
    // -----------------------------------------------------------------------

    #[test]
    fn cancel_valid_handle_returns_success() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = sql_cancel::<MockBackend>(stmt);
            assert_eq!(ret, SqlReturn::SUCCESS);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn cancel_with_open_cursor_returns_success() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let handle = as_handle_ref::<StatementHandle<MockBackend>>(stmt).expect("valid");
            handle.set_result_set(crate::handles::StatementData::Synthetic(
                crate::test_utils::synthetic_result_set(vec![]),
            ));

            // Cancel should succeed even when a cursor is open (it's a no-op).
            let ret = sql_cancel::<MockBackend>(stmt);
            assert_eq!(ret, SqlReturn::SUCCESS);
            // Cursor must still be open; cancel does not close it.
            let handle = as_handle_ref::<StatementHandle<MockBackend>>(stmt).expect("valid");
            assert!(handle.statement.is_some());
            assert!(handle.cursor_open);

            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn cancel_null_handle_returns_invalid() {
        unsafe {
            let ret = sql_cancel::<MockBackend>(std::ptr::null_mut());
            assert_eq!(ret, SqlReturn::INVALID_HANDLE);
        }
    }

    /// SQLCancel on a statement with no active operation returns SUCCESS.
    #[test]
    fn cancel_no_active_statement_returns_success() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = sql_cancel::<MockBackend>(stmt);
            assert_eq!(ret, SqlReturn::SUCCESS);
            cleanup(env, conn, stmt);
        }
    }

    /// SQLCancel clears the diagnostic queue even when there is nothing to cancel.
    #[test]
    fn cancel_clears_diagnostics() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();

            // Push a diagnostic onto the statement handle.
            let queue = crate::handles::try_get_diagnostic_queue::<MockBackend>(stmt).unwrap();
            queue.push(&crate::errors::OdbcError::NotConnected);
            assert_eq!(queue.len(), 1);

            // Cancel should clear the diagnostics.
            let ret = sql_cancel::<MockBackend>(stmt);
            assert_eq!(ret, SqlReturn::SUCCESS);

            let queue = crate::handles::try_get_diagnostic_queue::<MockBackend>(stmt).unwrap();
            assert_eq!(queue.len(), 0);

            cleanup(env, conn, stmt);
        }
    }

    // -----------------------------------------------------------------------
    // SQLGetCursorNameW / SQLSetCursorNameW
    // -----------------------------------------------------------------------

    #[test]
    fn get_cursor_name_auto_generates_name() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let mut buf = [0u16; 64];
            let mut name_len: i16 = 0;
            let ret = sql_get_cursor_name_w::<MockBackend>(
                stmt,
                buf.as_mut_ptr(),
                buf.len() as i16,
                &mut name_len,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert!(name_len > 0);
            let name = String::from_utf16_lossy(&buf[..name_len as usize]);
            assert!(
                name.starts_with("SQL_CURSR"),
                "expected auto-generated name starting with SQL_CURSR, got {name:?}"
            );
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn set_then_get_cursor_name_round_trips() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();

            // Set cursor name to "MyCursor"
            let name_utf16: Vec<u16> = "MyCursor".encode_utf16().collect();
            let ret = sql_set_cursor_name_w::<MockBackend>(
                stmt,
                name_utf16.as_ptr(),
                name_utf16.len() as i16,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            // Get it back
            let mut buf = [0u16; 64];
            let mut name_len: i16 = 0;
            let ret = sql_get_cursor_name_w::<MockBackend>(
                stmt,
                buf.as_mut_ptr(),
                buf.len() as i16,
                &mut name_len,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            let name = String::from_utf16_lossy(&buf[..name_len as usize]);
            assert_eq!(name, "MyCursor");

            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn get_cursor_name_truncates_with_success_with_info() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();

            // Set a long cursor name: "VeryLongCursorName" (18 chars)
            let long_name = "VeryLongCursorName";
            let name_utf16: Vec<u16> = long_name.encode_utf16().collect();
            let ret = sql_set_cursor_name_w::<MockBackend>(
                stmt,
                name_utf16.as_ptr(),
                name_utf16.len() as i16,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            // Get into a small buffer (5 elements: room for 4 chars + null)
            let mut buf = [0u16; 5];
            let mut name_len: i16 = 0;
            let ret = sql_get_cursor_name_w::<MockBackend>(
                stmt,
                buf.as_mut_ptr(),
                buf.len() as i16,
                &mut name_len,
            );
            assert_eq!(ret, SqlReturn::SUCCESS_WITH_INFO);
            // name_len reports the full length, not the truncated length
            assert_eq!(name_len, 18);

            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn set_cursor_name_null_pointer_returns_error() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = sql_set_cursor_name_w::<MockBackend>(stmt, std::ptr::null(), 0);
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn, stmt);
        }
    }

    // -----------------------------------------------------------------------
    // SQLBulkOperations
    // -----------------------------------------------------------------------

    #[test]
    fn bulk_operations_null_handle_returns_invalid() {
        unsafe {
            let ret = sql_bulk_operations::<MockBackend>(std::ptr::null_mut(), SQL_ADD);
            assert_eq!(ret, SqlReturn::INVALID_HANDLE);
        }
    }

    #[test]
    fn bulk_operations_returns_hyc00() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = sql_bulk_operations::<MockBackend>(stmt, SQL_ADD);
            assert_eq!(ret, SqlReturn::ERROR);
            // Verify HYC00 diagnostic was set.
            let handle = as_handle_ref::<StatementHandle<MockBackend>>(stmt).expect("valid");
            let rec = handle.diagnostics.get(0).expect("diagnostic record");
            assert_eq!(
                rec.sqlstate.as_str(),
                "HYC00",
                "expected HYC00, got {}",
                rec.sqlstate.as_str()
            );
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn bulk_operations_invalid_operation_returns_hy092() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            // 99 is not a valid SQLBulkOperations operation.
            let ret = sql_bulk_operations::<MockBackend>(stmt, 99);
            assert_eq!(ret, SqlReturn::ERROR);
            let handle = as_handle_ref::<StatementHandle<MockBackend>>(stmt).expect("valid");
            let rec = handle.diagnostics.get(0).expect("diagnostic record");
            assert_eq!(
                rec.sqlstate.as_str(),
                "HY092",
                "expected HY092, got {}",
                rec.sqlstate.as_str()
            );
            cleanup(env, conn, stmt);
        }
    }

    // -----------------------------------------------------------------------
    // SQLSetPos
    // -----------------------------------------------------------------------

    #[test]
    fn set_pos_null_handle_returns_invalid() {
        unsafe {
            let ret = sql_set_pos::<MockBackend>(
                std::ptr::null_mut(),
                1,
                SQL_POSITION,
                SQL_LOCK_NO_CHANGE,
            );
            assert_eq!(ret, SqlReturn::INVALID_HANDLE);
        }
    }

    #[test]
    fn set_pos_returns_hyc00() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = sql_set_pos::<MockBackend>(stmt, 1, SQL_POSITION, SQL_LOCK_NO_CHANGE);
            assert_eq!(ret, SqlReturn::ERROR);
            // Verify HYC00 diagnostic was set.
            let handle = as_handle_ref::<StatementHandle<MockBackend>>(stmt).expect("valid");
            let rec = handle.diagnostics.get(0).expect("diagnostic record");
            assert_eq!(
                rec.sqlstate.as_str(),
                "HYC00",
                "expected HYC00, got {}",
                rec.sqlstate.as_str()
            );
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn set_pos_invalid_operation_returns_hy092() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            // 99 is not a valid SQLSetPos operation.
            let ret = sql_set_pos::<MockBackend>(stmt, 1, 99, SQL_LOCK_NO_CHANGE);
            assert_eq!(ret, SqlReturn::ERROR);
            let handle = as_handle_ref::<StatementHandle<MockBackend>>(stmt).expect("valid");
            let rec = handle.diagnostics.get(0).expect("diagnostic record");
            assert_eq!(
                rec.sqlstate.as_str(),
                "HY092",
                "expected HY092, got {}",
                rec.sqlstate.as_str()
            );
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn set_pos_invalid_lock_type_returns_hy092() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            // 99 is not a valid lock type.
            let ret = sql_set_pos::<MockBackend>(stmt, 1, SQL_POSITION, 99);
            assert_eq!(ret, SqlReturn::ERROR);
            let handle = as_handle_ref::<StatementHandle<MockBackend>>(stmt).expect("valid");
            let rec = handle.diagnostics.get(0).expect("diagnostic record");
            assert_eq!(
                rec.sqlstate.as_str(),
                "HY092",
                "expected HY092, got {}",
                rec.sqlstate.as_str()
            );
            cleanup(env, conn, stmt);
        }
    }
}
