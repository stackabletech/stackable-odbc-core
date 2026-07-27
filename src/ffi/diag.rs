//! Diagnostic retrieval: `SQLGetDiagRecW`, `SQLGetDiagFieldW`.

use std::ffi::c_void;

use crate::backend::Backend;
use crate::panic::panic_safe;
use crate::types::{SqlReturn, handle_type_from_raw};
use crate::utf16::write_utf16;
use odbc_sys::HeaderDiagnosticIdentifier;

/// Maximum buffer size for ODBC diagnostic message text (SQL_MAX_MESSAGE_LENGTH = 512 bytes = 256 UTF-16 code units).
#[cfg(test)]
const DIAG_MSG_BUF_LEN: usize = 256;

// `SQLGetDiagField` diag_identifier values, derived from `odbc-sys` rather than
// restated. The record fields are the reason: `sqlext.h` puts
// `SQL_DIAG_ROW_NUMBER` at -1248 and `SQL_DIAG_COLUMN_NUMBER` at -1247, far
// from the small positive header-field numbers around them, and 12 is
// `SQL_DIAG_DYNAMIC_FUNCTION_CODE`. A transcribed copy that drifts therefore
// does not simply fail to match — it answers a different field.
//
// These are `const` bindings rather than direct enum uses because the match
// below needs them in patterns, including an inclusive range.
const SQL_DIAG_RETURNCODE: i16 = HeaderDiagnosticIdentifier::ReturnCode as i16;
const SQL_DIAG_NUMBER: i16 = HeaderDiagnosticIdentifier::Number as i16;
const SQL_DIAG_SQLSTATE: i16 = HeaderDiagnosticIdentifier::SqlState as i16;
const SQL_DIAG_NATIVE: i16 = HeaderDiagnosticIdentifier::Native as i16;
const SQL_DIAG_MESSAGE_TEXT: i16 = HeaderDiagnosticIdentifier::MessageText as i16;
const SQL_DIAG_CLASS_ORIGIN: i16 = HeaderDiagnosticIdentifier::ClassOrigin as i16;
const SQL_DIAG_SERVER_NAME: i16 = HeaderDiagnosticIdentifier::ServerName as i16;
const SQL_DIAG_COLUMN_NUMBER: i16 = HeaderDiagnosticIdentifier::ColumnNumber as i16;
const SQL_DIAG_ROW_NUMBER: i16 = HeaderDiagnosticIdentifier::RowNumber as i16;
// Sentinels for the two fields above, from `sqlext.h`; `odbc-sys` has neither.
// Private because only `SQLGetDiagField` produces them.
const SQL_NO_COLUMN_NUMBER: i32 = -1;
const SQL_NO_ROW_NUMBER: isize = -1;
// SQL_DIAG_RETURNCODE value for SQL_SUCCESS
const SQL_SUCCESS_VALUE: i16 = 0;

/// Write a `SQLGetDiagField` character-string field.
///
/// `SQLGetDiagField`'s `BufferLength` and `StringLengthPtr` are both specified
/// in bytes (spec: "the total number of bytes ... available to return in
/// *DiagInfoPtr*"), but `write_utf16` operates in UTF-16 code units. This
/// converts `buffer_length` from bytes to code units on the way in, and the
/// code-unit count `write_utf16` reports back to bytes on the way out.
///
/// # Safety
///
/// Same preconditions as [`write_utf16`]: `diag_info` must be null or point
/// to a buffer of at least `buffer_length / 2` `u16` elements, and
/// `string_length` must be null or a valid, writable `i16` (not necessarily
/// aligned; this uses an unaligned write).
unsafe fn write_diag_string(
    value: &str,
    diag_info: *mut c_void,
    buffer_length: i16,
    string_length: *mut i16,
) -> SqlReturn {
    let buf_len_u16 = buffer_length / 2;
    let mut units: i16 = 0;
    // SAFETY: caller guarantees diag_info/buffer_length and string_length preconditions;
    // we pass a local `units` output instead of `string_length` so we can convert the
    // reported UTF-16 code-unit count to bytes before writing it to the caller's pointer.
    let ret = unsafe { write_utf16(value, diag_info as *mut u16, buf_len_u16, &mut units) };
    if !string_length.is_null() {
        let bytes = i16::try_from(i32::from(units) * 2).unwrap_or_else(|_| {
            tracing::warn!(
                "write_diag_string: byte length for {} code units overflows i16, saturating to i16::MAX",
                units
            );
            i16::MAX
        });
        // SAFETY: string_length is non-null (checked above); caller guarantees it is a
        // valid writable i16, but alignment is not guaranteed, so use an unaligned write.
        unsafe { std::ptr::write_unaligned(string_length, bytes) };
    }
    ret
}

/// Generic implementation of SQLGetDiagRecW.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlgetdiagrec-function>
///
/// Retrieves the diagnostic record at the 1-based `rec_number` from the
/// handle's diagnostic queue. Writes the SQLSTATE to `sql_state`, the native
/// error code to `native_error`, and the message text to `message_text`.
///
/// Wrapped in `panic_safe` like every other entry point, for the group lock
/// and panic safety, but the closure below always returns `Ok` and never
/// `Err`: this function reads diagnostics and must not clear them or push a
/// new one on error. Per spec: "SQLGetDiagRec does not post diagnostic
/// records for itself."
///
/// # Parameters
///
/// - `handle_type`: handle type identifier (SQL_HANDLE_ENV, _DBC, _STMT, _DESC)
/// - `handle`: the handle whose diagnostic queue is read
/// - `rec_number`: 1-based index of the diagnostic record to retrieve
/// - `sql_state`: output buffer for the 5-character SQLSTATE + null terminator (6 u16 values)
/// - `native_error_ptr`: output for the driver-specific native error code
/// - `message_text`: output buffer for the diagnostic message text
/// - `buffer_length`: length of `message_text` in characters
/// - `text_length_ptr`: output for the number of characters available in `message_text`
///
/// # Spec compliance
///
/// This function does not post diagnostic records for itself; it reports its
/// outcome via return value only (no SQLSTATEs in the Diagnostics table).
///
/// - `SQL_SUCCESS` — diagnostic information returned successfully.
/// - `SQL_SUCCESS_WITH_INFO` — `*MessageText` buffer was too small; message
///   was truncated. `*TextLengthPtr` contains the full untruncated character
///   count.
/// - `SQL_INVALID_HANDLE` — `handle` is not a valid ODBC handle.
/// - `SQL_ERROR` — `rec_number` was negative or 0; or `buffer_length` was less
///   than zero.
///   - Async-operation-not-complete case: not applicable (the `Backend` trait is synchronous).
/// - `SQL_NO_DATA` — `rec_number` was greater than the number of diagnostic
///   records for `handle`, or `handle` has no diagnostic records at all.
///
/// # Safety
///
/// `handle` must be a valid ODBC handle. Output pointers must be valid or null.
#[allow(clippy::too_many_arguments)]
pub unsafe fn sql_get_diag_rec_w<B: Backend>(
    _handle_type: i16,
    handle: *mut c_void,
    rec_number: i16,
    sql_state: *mut u16,
    native_error_ptr: *mut i32,
    message_text: *mut u16,
    buffer_length: i16,
    text_length_ptr: *mut i16,
) -> SqlReturn {
    tracing::trace!(
        "SQLGetDiagRecW(handle_type_raw={}, handle={:?}, rec={})",
        _handle_type,
        handle,
        rec_number
    );
    let ht_log = handle_type_from_raw(_handle_type);
    tracing::debug!(
        "SQLGetDiagRecW(handle_type={:?}, handle={:?}, rec={})",
        ht_log,
        handle,
        rec_number
    );

    // SAFETY: handle must be null or a token previously issued by an
    // `alloc_*` function, per this function's own safety contract.
    let ret = unsafe {
        panic_safe::<B, _>(handle, |scope| {
            // Spec: SQL_ERROR if RecNumber is negative or 0.
            if rec_number <= 0 {
                return Ok(SqlReturn::ERROR);
            }

            // Spec: SQL_ERROR if BufferLength is less than zero.
            if buffer_length < 0 {
                return Ok(SqlReturn::ERROR);
            }

            // Get diagnostic queue; if the handle is invalid, return INVALID_HANDLE.
            let queue = match scope.diagnostics::<B>(handle) {
                Some(q) => q,
                None => return Ok(SqlReturn::INVALID_HANDLE),
            };
            let index = (rec_number - 1) as usize;

            let record = match queue.get(index) {
                Some(r) => r,
                None => return Ok(SqlReturn::NO_DATA),
            };

            // Write SQLSTATE: 5 chars + null terminator = 6 u16 values
            if !sql_state.is_null() {
                let state_str = record.sqlstate.as_str();
                let state_wide: Vec<u16> = state_str.encode_utf16().collect();
                // SAFETY: sql_state is non-null (checked above) and caller guarantees
                // a buffer of at least 6 u16 values per the ODBC spec for SQLSTATE.
                // Assembled locally, then copied out byte-wise. `from_raw_parts_mut`
                // would require `sql_state` to be u16-aligned, which an
                // application-supplied pointer does not guarantee.
                let mut buf = [0u16; 6];
                for (i, &ch) in state_wide.iter().enumerate().take(5) {
                    buf[i] = ch;
                }
                // Pad with '0' if shorter than 5 (shouldn't happen with well-formed
                // states). `.min(5)` because a longer state would otherwise make
                // this range reversed, which panics.
                buf[state_wide.len().min(5)..5].fill(b'0' as u16);
                buf[5] = 0u16; // null terminator
                // SAFETY: sql_state is non-null (checked above) and the caller
                // guarantees at least 6 writable u16 values; u8 has alignment 1, so
                // the byte-wise copy carries no alignment requirement.
                std::ptr::copy_nonoverlapping(
                    buf.as_ptr().cast::<u8>(),
                    sql_state.cast::<u8>(),
                    buf.len() * size_of::<u16>(),
                );
            }

            // Write native error code
            if !native_error_ptr.is_null() {
                // SAFETY: native_error_ptr is non-null (checked above) and caller
                // guarantees it points to a valid i32 output parameter.
                std::ptr::write_unaligned(native_error_ptr, record.native_error);
            }

            // Write message text via write_utf16, which handles truncation.
            // SAFETY: message_text is either null (write_utf16 handles that) or a
            // caller-allocated buffer of at least buffer_length u16 values.
            Ok(write_utf16(
                &record.message,
                message_text,
                buffer_length,
                text_length_ptr,
            ))
        })
    };

    tracing::debug!("SQLGetDiagRecW -> {:?}", ret);
    ret
}

/// Generic implementation of SQLGetDiagFieldW.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlgetdiagfield-function>
///
/// Returns individual diagnostic fields. The Windows Driver Manager calls
/// this after `SUCCESS_WITH_INFO` to retrieve class origin, subclass origin,
/// etc. Returning `SQL_ERROR` here corrupts the DM's internal state.
///
/// # Parameters
///
/// - `handle_type`: handle type identifier (SQL_HANDLE_ENV, _DBC, _STMT, _DESC)
/// - `handle`: the handle whose diagnostic data structure is read
/// - `rec_number`: 1-based index of the diagnostic record (ignored for header fields)
/// - `diag_identifier`: the field to return (SQL_DIAG_NUMBER, SQL_DIAG_SQLSTATE, etc.)
/// - `diag_info`: output buffer; data type depends on `diag_identifier`
/// - `buffer_length`: length of `diag_info` in bytes (for character fields)
/// - `string_length`: output for the number of bytes available in `diag_info`
///
/// Wrapped in `panic_safe` like every other entry point, for the group lock
/// and panic safety, but the closure below always returns `Ok`, never `Err`:
/// this function reads diagnostics and must not clear them or push a new one
/// on error.
///
/// # Spec compliance
///
/// This function does not post diagnostic records for itself; it reports its
/// outcome via return value only (no SQLSTATEs in the Diagnostics table).
///
/// - `SQL_SUCCESS` — diagnostic field returned successfully.
/// - `SQL_SUCCESS_WITH_INFO` — `*DiagInfoPtr` was too small; character data
///   was truncated. `*StringLengthPtr` contains the full byte count.
/// - `SQL_INVALID_HANDLE` — `handle` is not a valid ODBC handle.
///   - For header fields (SQL_DIAG_NUMBER, SQL_DIAG_RETURNCODE), an invalid
///     handle is deliberately *not* surfaced as `SQL_INVALID_HANDLE`: these two
///     fields answer with a default value (count 0 / `SQL_SUCCESS`) instead.
///     This is a known, deliberate deviation from the spec.
/// - `SQL_ERROR` — one of:
///   - `diag_identifier` was not a valid value (driver returns `SQL_NO_DATA`
///     for unknown identifiers to avoid corrupting the Driver Manager state;
///     spec says SQL_ERROR but the DM handles unrecognised values itself).
///   - `diag_identifier` is SQL_DIAG_CURSOR_ROW_COUNT,
///     SQL_DIAG_DYNAMIC_FUNCTION, SQL_DIAG_DYNAMIC_FUNCTION_CODE, or
///     SQL_DIAG_ROW_COUNT and `handle` is not a statement handle
///     (driver-manager-handled; not returned here).
///   - `rec_number` was negative or 0 for a record field (not a header field).
///   - `buffer_length` was less than zero for a character string field.
///   - Async-operation-not-complete case: not applicable (the `Backend` trait is synchronous).
/// - `SQL_NO_DATA` — `rec_number` was greater than the number of diagnostic
///   records, or the handle has no diagnostic records.
///
/// # Safety
///
/// All pointer arguments must be valid or null.
#[allow(clippy::too_many_arguments)]
pub unsafe fn sql_get_diag_field_w<B: Backend>(
    _handle_type: i16,
    handle: *mut c_void,
    rec_number: i16,
    diag_identifier: i16,
    diag_info: *mut c_void,
    buffer_length: i16,
    string_length: *mut i16,
) -> SqlReturn {
    tracing::trace!(
        "SQLGetDiagFieldW(handle_type_raw={}, handle={:?}, rec={}, diag_id={})",
        _handle_type,
        handle,
        rec_number,
        diag_identifier
    );
    let ht_log = handle_type_from_raw(_handle_type);
    tracing::debug!(
        "SQLGetDiagFieldW(handle_type={:?}, handle={:?}, rec={}, diag_id={})",
        ht_log,
        handle,
        rec_number,
        diag_identifier
    );

    // SAFETY: handle must be null or a token previously issued by an
    // `alloc_*` function, per this function's own safety contract.
    let ret = unsafe {
        panic_safe::<B, _>(handle, |scope| {
            // Header fields (rec_number = 0)
            // SQL_DIAG_NUMBER: number of diagnostic records
            if diag_identifier == SQL_DIAG_NUMBER {
                // Per spec, SQL_DIAG_NUMBER should return SQL_INVALID_HANDLE for
                // invalid handles; this driver deliberately answers count=0
                // instead (see the doc comment above).
                let count = match scope.diagnostics::<B>(handle) {
                    Some(q) => q.len() as i32,
                    None => 0,
                };
                if !diag_info.is_null() {
                    // SAFETY: diag_info is non-null (checked above); caller guarantees
                    // it points to a valid i32 output buffer for SQL_DIAG_NUMBER.
                    std::ptr::write_unaligned(diag_info as *mut i32, count);
                }
                if !string_length.is_null() {
                    // SAFETY: string_length is non-null (checked above) and caller
                    // guarantees it is a valid i16 output parameter.
                    std::ptr::write_unaligned(string_length, std::mem::size_of::<i32>() as i16);
                }
                return Ok(SqlReturn::SUCCESS);
            }

            // SQL_DIAG_RETURNCODE: return code of the last function
            if diag_identifier == SQL_DIAG_RETURNCODE {
                // Per spec, SQL_DIAG_RETURNCODE should return SQL_INVALID_HANDLE
                // for invalid handles; this driver deliberately answers with the
                // default value instead (see the doc comment above).
                if !diag_info.is_null() {
                    // SAFETY: diag_info is non-null (checked above); caller guarantees
                    // it points to a valid i16 output buffer for SQL_DIAG_RETURNCODE.
                    std::ptr::write_unaligned(diag_info as *mut i16, SQL_SUCCESS_VALUE);
                }
                if !string_length.is_null() {
                    // SAFETY: string_length is non-null (checked above) and caller
                    // guarantees it is a valid i16 output parameter.
                    std::ptr::write_unaligned(string_length, std::mem::size_of::<i16>() as i16);
                }
                return Ok(SqlReturn::SUCCESS);
            }

            // Record fields: need a valid record number
            if rec_number <= 0 {
                return Ok(SqlReturn::ERROR);
            }

            // Spec: SQL_ERROR if BufferLength < 0 for a character string field.
            // Check conservatively here (before we know the field type) so that
            // negative buffer lengths are never passed to write_utf16.
            if buffer_length < 0 {
                return Ok(SqlReturn::ERROR);
            }

            let queue = match scope.diagnostics::<B>(handle) {
                Some(q) => q,
                None => return Ok(SqlReturn::INVALID_HANDLE),
            };

            let index = (rec_number - 1) as usize;
            let record = match queue.get(index) {
                Some(r) => r,
                None => return Ok(SqlReturn::NO_DATA),
            };

            Ok(match diag_identifier {
                // SQL_DIAG_SQLSTATE
                SQL_DIAG_SQLSTATE => {
                    let state_str = record.sqlstate.as_str();
                    // SAFETY: diag_info is either null (write_utf16 handles that) or a
                    // caller-allocated buffer of at least buffer_length/2 u16 values.
                    write_diag_string(state_str, diag_info, buffer_length, string_length)
                }
                // SQL_DIAG_NATIVE
                SQL_DIAG_NATIVE => {
                    if !diag_info.is_null() {
                        // SAFETY: diag_info is non-null (checked above); caller guarantees
                        // it points to a valid i32 output buffer for SQL_DIAG_NATIVE.
                        std::ptr::write_unaligned(diag_info as *mut i32, record.native_error);
                    }
                    if !string_length.is_null() {
                        // SAFETY: string_length is non-null (checked above) and caller
                        // guarantees it is a valid i16 output parameter.
                        std::ptr::write_unaligned(string_length, std::mem::size_of::<i32>() as i16);
                    }
                    SqlReturn::SUCCESS
                }
                // SQL_DIAG_MESSAGE_TEXT
                SQL_DIAG_MESSAGE_TEXT => {
                    // SAFETY: diag_info is either null (write_utf16 handles that) or a
                    // caller-allocated buffer of at least buffer_length/2 u16 values.
                    write_diag_string(&record.message, diag_info, buffer_length, string_length)
                }
                // SQL_DIAG_CLASS_ORIGIN, SQL_DIAG_SUBCLASS_ORIGIN (9),
                // SQL_DIAG_CONNECTION_NAME (10), SQL_DIAG_SERVER_NAME
                // Return empty strings (these are optional).
                SQL_DIAG_CLASS_ORIGIN..=SQL_DIAG_SERVER_NAME => {
                    // SAFETY: diag_info is either null (write_utf16 handles that) or a
                    // caller-allocated buffer of at least buffer_length/2 u16 values.
                    write_diag_string("", diag_info, buffer_length, string_length)
                }
                // An arm each, because the spec's Record Fields table types these
                // two differently: SQL_DIAG_COLUMN_NUMBER is SQLINTEGER and
                // SQL_DIAG_ROW_NUMBER is SQLLEN. Sharing one four-byte write would
                // leave the high half of a caller's SQLLEN untouched on a 64-bit
                // platform.
                SQL_DIAG_COLUMN_NUMBER => {
                    if !diag_info.is_null() {
                        // SAFETY: diag_info is non-null (checked above); the caller
                        // guarantees it points to a valid SQLINTEGER output buffer.
                        std::ptr::write_unaligned(diag_info as *mut i32, SQL_NO_COLUMN_NUMBER);
                    }
                    SqlReturn::SUCCESS
                }
                SQL_DIAG_ROW_NUMBER => {
                    if !diag_info.is_null() {
                        // SAFETY: diag_info is non-null (checked above); the caller
                        // guarantees it points to a valid SQLLEN output buffer.
                        std::ptr::write_unaligned(diag_info as *mut isize, SQL_NO_ROW_NUMBER);
                    }
                    SqlReturn::SUCCESS
                }
                // Unknown field — return NO_DATA rather than ERROR
                _ => {
                    tracing::debug!(
                        "SQLGetDiagFieldW: unknown diag_identifier {}, returning NO_DATA",
                        diag_identifier
                    );
                    SqlReturn::NO_DATA
                }
            })
        })
    };

    tracing::debug!("SQLGetDiagFieldW -> {:?}", ret);
    ret
}

#[cfg(test)]
mod tests {
    use super::*;
    use odbc_sys::{HandleType, HeaderDiagnosticIdentifier};

    use crate::ffi::handle::{sql_alloc_handle, sql_free_handle};
    use crate::handles::{ConnectionHandle, EnvironmentHandle, StatementHandle};
    use crate::test_utils::{MockBackend, with_handle};
    use crate::types::sql_state;

    #[test]
    fn diag_rec_returns_no_data_when_empty() {
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );

            let mut state = [0u16; 6];
            let mut native_err: i32 = 0;
            let mut msg_buf = [0u16; DIAG_MSG_BUF_LEN];
            let mut msg_len: i16 = 0;

            let ret = sql_get_diag_rec_w::<MockBackend>(
                HandleType::Env as i16,
                env,
                1, // rec_number 1
                state.as_mut_ptr(),
                &mut native_err,
                msg_buf.as_mut_ptr(),
                DIAG_MSG_BUF_LEN as i16,
                &mut msg_len,
            );
            assert_eq!(ret, SqlReturn::NO_DATA);

            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn diag_rec_returns_record_after_error() {
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );

            // Push an error onto the env diagnostic queue directly
            with_handle::<MockBackend, EnvironmentHandle<MockBackend>, _>(env, |handle| {
                handle
                    .diagnostics
                    .push(&crate::errors::OdbcError::NotConnected);
            });

            let mut state = [0u16; 6];
            let mut native_err: i32 = 0;
            let mut msg_buf = [0u16; DIAG_MSG_BUF_LEN];
            let mut msg_len: i16 = 0;

            let ret = sql_get_diag_rec_w::<MockBackend>(
                1,
                env,
                1,
                state.as_mut_ptr(),
                &mut native_err,
                msg_buf.as_mut_ptr(),
                DIAG_MSG_BUF_LEN as i16,
                &mut msg_len,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            // Verify SQLSTATE is CONNECTION_NOT_OPEN (08003)
            let state_str = String::from_utf16_lossy(&state[..5]);
            assert_eq!(state_str, sql_state::CONNECTION_NOT_OPEN);

            // Verify message is non-empty
            assert!(msg_len > 0);
            let message = String::from_utf16_lossy(&msg_buf[..msg_len as usize]);
            assert!(message.contains("Connection not established"));

            // Record 2 should be NO_DATA
            let ret = sql_get_diag_rec_w::<MockBackend>(
                1,
                env,
                2,
                state.as_mut_ptr(),
                &mut native_err,
                msg_buf.as_mut_ptr(),
                DIAG_MSG_BUF_LEN as i16,
                &mut msg_len,
            );
            assert_eq!(ret, SqlReturn::NO_DATA);

            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn diag_rec_invalid_handle() {
        unsafe {
            let ret = sql_get_diag_rec_w::<MockBackend>(
                1,
                std::ptr::null_mut(),
                1,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::INVALID_HANDLE);
        }
    }

    #[test]
    fn diag_rec_zero_rec_number_returns_error() {
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );

            // Spec: RecNumber <= 0 returns SQL_ERROR
            let ret = sql_get_diag_rec_w::<MockBackend>(
                1,
                env,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::ERROR);

            // Negative rec_number also returns SQL_ERROR
            let ret = sql_get_diag_rec_w::<MockBackend>(
                1,
                env,
                -1,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::ERROR);

            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn diag_rec_negative_buffer_length_returns_error() {
        // Spec: BufferLength < 0 returns SQL_ERROR.
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );

            with_handle::<MockBackend, EnvironmentHandle<MockBackend>, _>(env, |handle| {
                handle
                    .diagnostics
                    .push(&crate::errors::OdbcError::NotConnected);
            });

            let mut state = [0u16; 6];
            let mut native_err: i32 = 0;
            let mut msg_buf = [0u16; DIAG_MSG_BUF_LEN];
            let mut msg_len: i16 = 0;

            let ret = sql_get_diag_rec_w::<MockBackend>(
                1,
                env,
                1,
                state.as_mut_ptr(),
                &mut native_err,
                msg_buf.as_mut_ptr(),
                -1, // invalid buffer length
                &mut msg_len,
            );
            assert_eq!(ret, SqlReturn::ERROR);

            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn diag_rec_truncated_message_returns_success_with_info() {
        // Spec: Truncated message returns SQL_SUCCESS_WITH_INFO.
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );

            with_handle::<MockBackend, EnvironmentHandle<MockBackend>, _>(env, |handle| {
                handle
                    .diagnostics
                    .push(&crate::errors::OdbcError::NotConnected);
            });

            let mut state = [0u16; 6];
            let mut native_err: i32 = 0;
            let mut msg_buf = [0u16; 4]; // very small buffer — will truncate
            let mut msg_len: i16 = 0;

            let ret = sql_get_diag_rec_w::<MockBackend>(
                1,
                env,
                1,
                state.as_mut_ptr(),
                &mut native_err,
                msg_buf.as_mut_ptr(),
                4, // only room for 3 chars + null
                &mut msg_len,
            );
            assert_eq!(ret, SqlReturn::SUCCESS_WITH_INFO);
            // msg_len should report the full untruncated length.
            assert!(msg_len > 3);

            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    // -----------------------------------------------------------------------
    // Selectively null output pointers
    // -----------------------------------------------------------------------

    #[test]
    fn diag_rec_null_sql_state_pointer() {
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );

            with_handle::<MockBackend, EnvironmentHandle<MockBackend>, _>(env, |handle| {
                handle
                    .diagnostics
                    .push(&crate::errors::OdbcError::NotConnected);
            });

            let mut native_err: i32 = 0;
            let mut msg_buf = [0u16; DIAG_MSG_BUF_LEN];
            let mut msg_len: i16 = 0;

            let ret = sql_get_diag_rec_w::<MockBackend>(
                1,
                env,
                1,
                std::ptr::null_mut(), // null sql_state
                &mut native_err,
                msg_buf.as_mut_ptr(),
                DIAG_MSG_BUF_LEN as i16,
                &mut msg_len,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert!(msg_len > 0);

            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn diag_rec_null_native_error_pointer() {
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );

            with_handle::<MockBackend, EnvironmentHandle<MockBackend>, _>(env, |handle| {
                handle
                    .diagnostics
                    .push(&crate::errors::OdbcError::NotConnected);
            });

            let mut state = [0u16; 6];
            let mut msg_buf = [0u16; DIAG_MSG_BUF_LEN];
            let mut msg_len: i16 = 0;

            let ret = sql_get_diag_rec_w::<MockBackend>(
                1,
                env,
                1,
                state.as_mut_ptr(),
                std::ptr::null_mut(), // null native_error
                msg_buf.as_mut_ptr(),
                DIAG_MSG_BUF_LEN as i16,
                &mut msg_len,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            let state_str = String::from_utf16_lossy(&state[..5]);
            assert_eq!(state_str, sql_state::CONNECTION_NOT_OPEN);

            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn diag_rec_null_message_text_pointer() {
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );

            with_handle::<MockBackend, EnvironmentHandle<MockBackend>, _>(env, |handle| {
                handle
                    .diagnostics
                    .push(&crate::errors::OdbcError::NotConnected);
            });

            let mut state = [0u16; 6];
            let mut native_err: i32 = 0;
            let mut msg_len: i16 = 0;

            let ret = sql_get_diag_rec_w::<MockBackend>(
                1,
                env,
                1,
                state.as_mut_ptr(),
                &mut native_err,
                std::ptr::null_mut(), // null message_text
                0,
                &mut msg_len,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            // msg_len should still report the full length needed
            assert!(msg_len > 0);

            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    // -----------------------------------------------------------------------
    // Multiple diagnostic records in correct order
    // -----------------------------------------------------------------------

    #[test]
    fn diag_rec_multiple_records_in_order() {
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );

            with_handle::<MockBackend, EnvironmentHandle<MockBackend>, _>(env, |handle| {
                handle
                    .diagnostics
                    .push(&crate::errors::OdbcError::NotConnected); // 08003
                handle
                    .diagnostics
                    .push(&crate::errors::OdbcError::NoResultSet); // 24000
                handle
                    .diagnostics
                    .push(&crate::errors::OdbcError::NotImplemented {
                        feature: "test".into(),
                    }); // HYC00
            });

            let mut state = [0u16; 6];
            let mut native_err: i32 = 0;
            let mut msg_buf = [0u16; DIAG_MSG_BUF_LEN];
            let mut msg_len: i16 = 0;

            // Record 1: NotConnected → 08003
            let ret = sql_get_diag_rec_w::<MockBackend>(
                1,
                env,
                1,
                state.as_mut_ptr(),
                &mut native_err,
                msg_buf.as_mut_ptr(),
                DIAG_MSG_BUF_LEN as i16,
                &mut msg_len,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(
                String::from_utf16_lossy(&state[..5]),
                sql_state::CONNECTION_NOT_OPEN
            );

            // Record 2: NoResultSet → INVALID_CURSOR_STATE (24000)
            let ret = sql_get_diag_rec_w::<MockBackend>(
                1,
                env,
                2,
                state.as_mut_ptr(),
                &mut native_err,
                msg_buf.as_mut_ptr(),
                DIAG_MSG_BUF_LEN as i16,
                &mut msg_len,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(
                String::from_utf16_lossy(&state[..5]),
                sql_state::INVALID_CURSOR_STATE
            );

            // Record 3: NotImplemented → OPTIONAL_FEATURE_NOT_IMPLEMENTED (HYC00)
            let ret = sql_get_diag_rec_w::<MockBackend>(
                1,
                env,
                3,
                state.as_mut_ptr(),
                &mut native_err,
                msg_buf.as_mut_ptr(),
                DIAG_MSG_BUF_LEN as i16,
                &mut msg_len,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(
                String::from_utf16_lossy(&state[..5]),
                sql_state::OPTIONAL_FEATURE_NOT_IMPLEMENTED
            );

            // Record 4: NO_DATA
            let ret = sql_get_diag_rec_w::<MockBackend>(
                1,
                env,
                4,
                state.as_mut_ptr(),
                &mut native_err,
                msg_buf.as_mut_ptr(),
                DIAG_MSG_BUF_LEN as i16,
                &mut msg_len,
            );
            assert_eq!(ret, SqlReturn::NO_DATA);

            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    // -----------------------------------------------------------------------
    // Different handle types (connection and statement)
    // -----------------------------------------------------------------------

    #[test]
    fn diag_rec_on_connection_handle() {
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );
            let mut conn: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(HandleType::Dbc as i16, env, &mut conn);

            with_handle::<MockBackend, ConnectionHandle<MockBackend>, _>(conn, |handle| {
                handle
                    .diagnostics
                    .push(&crate::errors::OdbcError::NotConnected);
            });

            let mut state = [0u16; 6];
            let mut native_err: i32 = 0;
            let mut msg_buf = [0u16; DIAG_MSG_BUF_LEN];
            let mut msg_len: i16 = 0;

            let ret = sql_get_diag_rec_w::<MockBackend>(
                HandleType::Dbc as i16,
                conn,
                1,
                state.as_mut_ptr(),
                &mut native_err,
                msg_buf.as_mut_ptr(),
                DIAG_MSG_BUF_LEN as i16,
                &mut msg_len,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(
                String::from_utf16_lossy(&state[..5]),
                sql_state::CONNECTION_NOT_OPEN
            );

            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn diag_rec_on_statement_handle() {
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );
            let mut conn: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(HandleType::Dbc as i16, env, &mut conn);
            let mut stmt: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(HandleType::Stmt as i16, conn, &mut stmt);

            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                handle
                    .diagnostics
                    .push(&crate::errors::OdbcError::NoResultSet);
            });

            let mut state = [0u16; 6];
            let mut native_err: i32 = 0;
            let mut msg_buf = [0u16; DIAG_MSG_BUF_LEN];
            let mut msg_len: i16 = 0;

            let ret = sql_get_diag_rec_w::<MockBackend>(
                HandleType::Stmt as i16,
                stmt,
                1,
                state.as_mut_ptr(),
                &mut native_err,
                msg_buf.as_mut_ptr(),
                DIAG_MSG_BUF_LEN as i16,
                &mut msg_len,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(
                String::from_utf16_lossy(&state[..5]),
                sql_state::INVALID_CURSOR_STATE
            );

            let _ = sql_free_handle::<MockBackend>(HandleType::Stmt as i16, stmt);
            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    // -----------------------------------------------------------------------
    // sql_get_diag_field_w stub
    // -----------------------------------------------------------------------

    #[test]
    fn diag_field_number_returns_zero_when_no_records() {
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );

            // SQL_DIAG_NUMBER (2): returns the count of diagnostic records
            let mut count: i32 = -1;
            let ret = sql_get_diag_field_w::<MockBackend>(
                HandleType::Env as i16,
                env,
                0,
                HeaderDiagnosticIdentifier::Number as i16,
                &mut count as *mut i32 as *mut c_void,
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(count, 0); // no diagnostics pushed

            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn diag_field_zero_rec_number_for_record_field_returns_error() {
        // Spec: RecNumber <= 0 for a record field returns SQL_ERROR.
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );

            with_handle::<MockBackend, EnvironmentHandle<MockBackend>, _>(env, |handle| {
                handle
                    .diagnostics
                    .push(&crate::errors::OdbcError::NotConnected);
            });

            let mut buf = [0u16; DIAG_MSG_BUF_LEN];
            let ret = sql_get_diag_field_w::<MockBackend>(
                1,
                env,
                0, // rec_number 0 — invalid for record field
                SQL_DIAG_SQLSTATE,
                buf.as_mut_ptr() as *mut c_void,
                DIAG_MSG_BUF_LEN as i16,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::ERROR);

            // Negative rec_number also returns SQL_ERROR
            let ret = sql_get_diag_field_w::<MockBackend>(
                1,
                env,
                -1,
                SQL_DIAG_SQLSTATE,
                buf.as_mut_ptr() as *mut c_void,
                DIAG_MSG_BUF_LEN as i16,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::ERROR);

            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn diag_field_negative_buffer_length_returns_error() {
        // Spec: BufferLength < 0 for a character string field returns SQL_ERROR.
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );

            with_handle::<MockBackend, EnvironmentHandle<MockBackend>, _>(env, |handle| {
                handle
                    .diagnostics
                    .push(&crate::errors::OdbcError::NotConnected);
            });

            let mut buf = [0u16; DIAG_MSG_BUF_LEN];
            let ret = sql_get_diag_field_w::<MockBackend>(
                1,
                env,
                1, // valid rec_number
                SQL_DIAG_SQLSTATE,
                buf.as_mut_ptr() as *mut c_void,
                -1, // invalid buffer length
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::ERROR);

            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn diag_field_sqlstate_returns_correct_value() {
        // SQL_DIAG_SQLSTATE for a record field returns the SQLSTATE string.
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );

            with_handle::<MockBackend, EnvironmentHandle<MockBackend>, _>(env, |handle| {
                handle
                    .diagnostics
                    .push(&crate::errors::OdbcError::NotConnected);
            }); // 08003

            let mut buf = [0u16; 12];
            let mut str_len: i16 = 0;
            let ret = sql_get_diag_field_w::<MockBackend>(
                1,
                env,
                1,
                SQL_DIAG_SQLSTATE,
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as i16 * 2,
                &mut str_len,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            // Spec: StringLengthPtr is in bytes. CONNECTION_NOT_OPEN (08003) is 5
            // UTF-16 code units = 10 bytes.
            assert_eq!(str_len, 10);
            let state = String::from_utf16_lossy(&buf[..5]);
            assert_eq!(state, sql_state::CONNECTION_NOT_OPEN);

            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn diag_identifiers_match_the_odbc_headers() {
        // Every identifier pinned against `sql.h` / `sqlext.h`, so that
        // deriving them from `odbc-sys` cannot silently move one. The two
        // record fields carry the risk: they sit at -1247 and -1248, nowhere
        // near the header fields they are listed beside, and the value a
        // careless transcription reaches for -- 12 -- is a real identifier,
        // `SQL_DIAG_DYNAMIC_FUNCTION_CODE`. A wrong value here answers the
        // wrong field rather than failing to match.
        assert_eq!(SQL_DIAG_COLUMN_NUMBER, -1247);
        assert_eq!(SQL_DIAG_ROW_NUMBER, -1248);
        assert_eq!(SQL_DIAG_RETURNCODE, 1);
        assert_eq!(SQL_DIAG_NUMBER, 2);
        assert_eq!(SQL_DIAG_SQLSTATE, 4);
        assert_eq!(SQL_DIAG_NATIVE, 5);
        assert_eq!(SQL_DIAG_MESSAGE_TEXT, 6);
        assert_eq!(SQL_DIAG_CLASS_ORIGIN, 8);
        assert_eq!(SQL_DIAG_SERVER_NAME, 11);
        // The identifier the old SQL_DIAG_COLUMN_NUMBER collided with.
        assert_eq!(HeaderDiagnosticIdentifier::DynamicFunctionCode as i16, 12);
    }

    #[test]
    fn diag_field_row_number_writes_a_full_sqllen() {
        // The spec's Record Fields table types SQL_DIAG_ROW_NUMBER as SQLLEN
        // and SQL_DIAG_COLUMN_NUMBER as SQLINTEGER, so the two cannot share a
        // write width. The buffer is pre-filled with a value whose high half is
        // non-zero: a four-byte write would leave that half standing on a
        // 64-bit platform, which no assertion on the low half would catch.
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );
            with_handle::<MockBackend, EnvironmentHandle<MockBackend>, _>(env, |handle| {
                handle
                    .diagnostics
                    .push(&crate::errors::OdbcError::NotConnected);
            });

            // Pre-fill with a value whose high half is non-zero, so a
            // four-byte write leaves evidence.
            let mut row_number: isize = !0;
            let mut str_len: i16 = 0;
            let ret = sql_get_diag_field_w::<MockBackend>(
                1,
                env,
                1,
                SQL_DIAG_ROW_NUMBER,
                &mut row_number as *mut isize as *mut c_void,
                0,
                &mut str_len,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(
                row_number, SQL_NO_ROW_NUMBER,
                "SQL_DIAG_ROW_NUMBER did not write a whole SQLLEN"
            );

            // SQL_DIAG_COLUMN_NUMBER really is four bytes wide.
            let mut column_number: i32 = 0;
            let ret = sql_get_diag_field_w::<MockBackend>(
                1,
                env,
                1,
                SQL_DIAG_COLUMN_NUMBER,
                &mut column_number as *mut i32 as *mut c_void,
                0,
                &mut str_len,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(column_number, SQL_NO_COLUMN_NUMBER);

            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn diag_field_dynamic_function_code_is_not_answered_as_a_row_number() {
        // Core does not implement SQL_DIAG_DYNAMIC_FUNCTION_CODE, so the honest
        // answer is the unknown-field one. What makes it worth a test is the
        // shape of the failure if a record-field constant ever collides with
        // 12: the sentinel those fields write is -1, and -1 is a *valid*
        // dynamic function code, SQL_DIAG_CREATE_INDEX. The application would
        // be told every statement it ran was a CREATE INDEX, with no error
        // anywhere to suggest otherwise.
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );
            with_handle::<MockBackend, EnvironmentHandle<MockBackend>, _>(env, |handle| {
                handle
                    .diagnostics
                    .push(&crate::errors::OdbcError::NotConnected);
            });

            let mut value: i32 = 0;
            let mut str_len: i16 = 0;
            let ret = sql_get_diag_field_w::<MockBackend>(
                1,
                env,
                1,
                HeaderDiagnosticIdentifier::DynamicFunctionCode as i16,
                &mut value as *mut i32 as *mut c_void,
                0,
                &mut str_len,
            );
            assert_eq!(ret, SqlReturn::NO_DATA);
            assert_eq!(value, 0, "an unimplemented field wrote to the buffer");

            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn diag_field_no_data_when_rec_number_exceeds_records() {
        // Spec: SQL_NO_DATA when RecNumber > number of diagnostic records.
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );

            with_handle::<MockBackend, EnvironmentHandle<MockBackend>, _>(env, |handle| {
                handle
                    .diagnostics
                    .push(&crate::errors::OdbcError::NotConnected);
            });

            let mut buf = [0u16; DIAG_MSG_BUF_LEN];
            let ret = sql_get_diag_field_w::<MockBackend>(
                1,
                env,
                2, // only 1 record exists
                SQL_DIAG_SQLSTATE,
                buf.as_mut_ptr() as *mut c_void,
                DIAG_MSG_BUF_LEN as i16 * 2,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::NO_DATA);

            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }
}
