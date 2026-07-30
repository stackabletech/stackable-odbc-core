//! Diagnostic retrieval: `SQLGetDiagRecW`, `SQLGetDiagFieldW`.

use std::ffi::c_void;

use crate::backend::Backend;
use crate::handles::StatementHandle;
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
const SQL_DIAG_SUBCLASS_ORIGIN: i16 = HeaderDiagnosticIdentifier::SubclassOrigin as i16;
const SQL_DIAG_CONNECTION_NAME: i16 = HeaderDiagnosticIdentifier::ConnectionName as i16;

/// The `SQL_DIAG_CLASS_ORIGIN` / `SQL_DIAG_SUBCLASS_ORIGIN` value for a SQLSTATE
/// the Open Group and ISO call-level interface define.
const SQL_DIAG_ORIGIN_ISO: &str = "ISO 9075";

/// The same, for an ODBC-specific SQLSTATE. The literal "ODBC 3.0" is the
/// spec's, and does not track this driver's version.
const SQL_DIAG_ORIGIN_ODBC: &str = "ODBC 3.0";

/// The SQLSTATE class the spec calls ODBC-specific: "For ODBC-specific
/// SQLSTATEs (all those whose SQLSTATE class is 'IM')".
const ODBC_SPECIFIC_CLASS: &str = "IM";

/// The SQLSTATEs whose *subclass* the spec attributes to ODBC rather than to
/// ISO, transcribed verbatim from `SQL_DIAG_SUBCLASS_ORIGIN`'s row: "The
/// ODBC-specific SQLSTATES for which 'ODBC 3.0' is returned include the
/// following: …"
///
/// A closed list, and not a range: it skips `IM009`, and its `HY` entries are
/// nine of the class's several dozen. Testing membership by pattern rather than
/// by table would answer differently for both.
const ODBC_SPECIFIC_SUBCLASS_STATES: &[&str] = &[
    "01S00", "01S01", "01S02", "01S06", "01S07", "07S01", "08S01", "21S01", "21S02", "25S01",
    "25S02", "25S03", "42S01", "42S02", "42S11", "42S12", "42S21", "42S22", "HY095", "HY097",
    "HY098", "HY099", "HY100", "HY101", "HY105", "HY107", "HY109", "HY110", "HY111", "HYT00",
    "HYT01", "IM001", "IM002", "IM003", "IM004", "IM005", "IM006", "IM007", "IM008", "IM010",
    "IM011", "IM012",
];

/// The `SQL_DIAG_CLASS_ORIGIN` value for a record's SQLSTATE.
///
/// Spec: "Its value is 'ISO 9075' for all SQLSTATEs defined by Open Group and
/// ISO call-level interface. For ODBC-specific SQLSTATEs (all those whose
/// SQLSTATE class is 'IM'), its value is 'ODBC 3.0'."
///
/// Keys on the two-character class only, which is what makes this a different
/// question from [`subclass_origin`] rather than a cheaper version of it.
fn class_origin(sqlstate: &str) -> &'static str {
    if sqlstate
        .get(..2)
        .is_some_and(|class| class.eq_ignore_ascii_case(ODBC_SPECIFIC_CLASS))
    {
        SQL_DIAG_ORIGIN_ODBC
    } else {
        SQL_DIAG_ORIGIN_ISO
    }
}

/// The `SQL_DIAG_SUBCLASS_ORIGIN` value for a record's SQLSTATE.
///
/// Spec: "A string with the same format and valid values as
/// SQL_DIAG_CLASS_ORIGIN, that identifies the defining portion of the subclass
/// portion of the SQLSTATE code", against the closed list in
/// [`ODBC_SPECIFIC_SUBCLASS_STATES`].
///
/// Independent of [`class_origin`]: `08S01` has an ISO class and an ODBC
/// subclass, and `IM009` the reverse.
fn subclass_origin(sqlstate: &str) -> &'static str {
    if ODBC_SPECIFIC_SUBCLASS_STATES
        .iter()
        .any(|state| state.eq_ignore_ascii_case(sqlstate))
    {
        SQL_DIAG_ORIGIN_ODBC
    } else {
        SQL_DIAG_ORIGIN_ISO
    }
}

const SQL_DIAG_ROW_COUNT: i16 = HeaderDiagnosticIdentifier::RowCount as i16;
const SQL_DIAG_DYNAMIC_FUNCTION: i16 = HeaderDiagnosticIdentifier::DynamicFunction as i16;
const SQL_DIAG_DYNAMIC_FUNCTION_CODE: i16 = HeaderDiagnosticIdentifier::DynamicFunctionCode as i16;
const SQL_DIAG_CURSOR_ROW_COUNT: i16 = HeaderDiagnosticIdentifier::CursorRowCount as i16;

/// `SQL_DIAG_UNKNOWN_STATEMENT` (0), derived rather than restated.
///
/// The spec's "Values of the Dynamic Function Fields" table pairs it with an
/// empty `SQL_DIAG_DYNAMIC_FUNCTION` on the row headed "Unknown", which is what
/// a driver that parses no SQL can honestly report.
const SQL_DIAG_UNKNOWN_STATEMENT: i32 =
    odbc_sys::DynamicDiagnosticIdentifier::UnknownStatement as i32;

/// The `SQL_DIAG_DYNAMIC_FUNCTION` half of that same row.
const SQL_DIAG_DYNAMIC_FUNCTION_UNKNOWN: &str = "";

/// `SQL_DIAG_CURSOR_ROW_COUNT` when no cursor row count is available.
///
/// Not a spec sentinel — the field has none. The field's own row makes its
/// meaning conditional: "Its semantics depend on the **SQLGetInfo** information
/// types … SQL_FORWARD_ONLY_CURSOR_ATTRIBUTES2 … (in the SQL_CA2_CRC_EXACT and
/// SQL_CA2_CRC_APPROXIMATE bits)". Core sets neither of those two bits in that
/// info type (`crate::backend::default_get_info`), so it has declared that no
/// row count is available and zero is the consequence rather than a guess.
/// Note the info type itself is not zero — it carries
/// `SQL_CA2_READ_ONLY_CONCURRENCY` — so it is the two named bits that matter
/// here, not the whole value.
const CURSOR_ROW_COUNT_UNAVAILABLE: isize = 0;

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

/// [`write_diag_string`] with `SQLGetDiagField`'s negative-`BufferLength` rule,
/// which applies to character fields and to nothing else.
///
/// The spec's SQL_ERROR list: "The value requested **was a character string**
/// and *BufferLength* was less than zero." An integer field is exempt by name —
/// "If *DiagIdentifier* is an ODBC-defined field and \**DiagInfoPtr* is an
/// integer, *BufferLength* is ignored" — and applications are told to pass a
/// negative value there: "If *\*DiagInfoPtr* contains a fixed-length data type,
/// *BufferLength* is SQL_IS_INTEGER, SQL_IS_UINTEGER, SQL_IS_SMALLINT, or
/// SQL_IS_USMALLINT, as appropriate", which are -6, -5, -8 and -7.
///
/// A single check ahead of the field match was therefore failing conventional,
/// spec-recommended calls to `SQL_DIAG_NATIVE`, `SQL_DIAG_COLUMN_NUMBER` and
/// `SQL_DIAG_ROW_NUMBER`. Here instead of triplicated at the three character
/// arms, so a fourth character field cannot be added without it.
///
/// # Safety
///
/// Same preconditions as [`write_diag_string`].
unsafe fn write_diag_string_checked(
    value: &str,
    diag_info: *mut c_void,
    buffer_length: i16,
    string_length: *mut i16,
) -> SqlReturn {
    if buffer_length < 0 {
        return SqlReturn::ERROR;
    }
    // SAFETY: the caller's contract is passed straight through.
    unsafe { write_diag_string(value, diag_info, buffer_length, string_length) }
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
            //
            // `Ok`, not `Err`: per this function's no-clear/no-push invariant
            // (see the doc comment above), nothing in this closure may ever
            // become an `Err`. An `Err` here would push a diagnostic through
            // `panic_safe`'s error path, corrupting the very record set an
            // application may be mid-iteration over.
            if rec_number <= 0 {
                return Ok(SqlReturn::ERROR);
            }

            // Spec: SQL_ERROR if BufferLength is less than zero. `Ok`, not
            // `Err` — same invariant as immediately above.
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

/// The four header fields the spec defines only for statement handles.
///
/// A typed enum rather than four comparisons at the call site, so the match
/// that answers them is exhaustive and needs no unreachable arm — the `panic`
/// lint is denied outside tests.
#[derive(Clone, Copy)]
enum StatementHeaderField {
    /// `SQL_DIAG_ROW_COUNT`
    RowCount,
    /// `SQL_DIAG_CURSOR_ROW_COUNT`
    CursorRowCount,
    /// `SQL_DIAG_DYNAMIC_FUNCTION`
    DynamicFunction,
    /// `SQL_DIAG_DYNAMIC_FUNCTION_CODE`
    DynamicFunctionCode,
}

/// Recognise a statement-only header field, following the crate's
/// `*_from_raw` idiom for turning an ABI integer into a type.
fn statement_header_field(diag_identifier: i16) -> Option<StatementHeaderField> {
    match diag_identifier {
        SQL_DIAG_ROW_COUNT => Some(StatementHeaderField::RowCount),
        SQL_DIAG_CURSOR_ROW_COUNT => Some(StatementHeaderField::CursorRowCount),
        SQL_DIAG_DYNAMIC_FUNCTION => Some(StatementHeaderField::DynamicFunction),
        SQL_DIAG_DYNAMIC_FUNCTION_CODE => Some(StatementHeaderField::DynamicFunctionCode),
        _ => None,
    }
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
///     SQL_DIAG_ROW_COUNT and `handle` is not a statement handle. **Returned by
///     this driver.** The Diagnostics list marks the clause (DM), but the Header
///     Fields table states it once per field without a marker, and the Comments
///     section a fifth time — "except for SQL_DIAG_CURSOR_ROW_COUNT or
///     SQL_DIAG_ROW_COUNT, which will return SQL_ERROR if *Handle* is not a
///     statement handle."
///   - `rec_number` was negative or 0 for a record field (not a header field).
///   - `buffer_length` was less than zero **for a character-string field**. An
///     integer-valued field ignores it, per "If *DiagIdentifier* is an
///     ODBC-defined field and \**DiagInfoPtr* is an integer, *BufferLength* is
///     ignored" — and applications are told to pass a negative sentinel there
///     ("*BufferLength* is SQL_IS_INTEGER, SQL_IS_UINTEGER, SQL_IS_SMALLINT, or
///     SQL_IS_USMALLINT, as appropriate"), so a blanket check fails a
///     conventional call.
///   - Async-operation-not-complete case: not applicable (the `Backend` trait is synchronous).
/// - `SQL_NO_DATA` — `rec_number` was greater than the number of diagnostic
///   records, or the handle has no diagnostic records.
///
/// # The statement-only header fields
///
/// All four are answered, and `RecNumber` is ignored for them as the spec
/// requires ("*RecNumber* is ignored for header fields"):
///
/// - **SQL_DIAG_ROW_COUNT** — the same `SQLLEN` `SQLRowCount` reports, from the
///   shared `crate::ffi::cursor::statement_row_count`. The spec's row: "The data
///   in this field is also returned in the *RowCountPtr* argument of
///   **SQLRowCount**."
/// - **SQL_DIAG_CURSOR_ROW_COUNT** — `0`. Its semantics "depend on the
///   **SQLGetInfo** information types … SQL_FORWARD_ONLY_CURSOR_ATTRIBUTES2 …
///   (in the SQL_CA2_CRC_EXACT and SQL_CA2_CRC_APPROXIMATE bits)", and core
///   sets neither of those bits, so no cursor row count is available.
/// - **SQL_DIAG_DYNAMIC_FUNCTION** — the empty string, and
///   **SQL_DIAG_DYNAMIC_FUNCTION_CODE** — `SQL_DIAG_UNKNOWN_STATEMENT`. Those
///   are one row of the spec's "Values of the Dynamic Function Fields" table,
///   headed "Unknown"; core parses no SQL and cannot classify the statement it
///   ran.
///
/// # The origin fields
///
/// `SQL_DIAG_CLASS_ORIGIN` and `SQL_DIAG_SUBCLASS_ORIGIN` are derived from the
/// record's own SQLSTATE, per the spec's two rules: `"ISO 9075"` unless the
/// class is `IM` for the first, and membership of the spec's closed
/// forty-two-state list for the second. They are independent: `08S01` has an ISO
/// class and an ODBC subclass.
///
/// `SQL_DIAG_CONNECTION_NAME` and `SQL_DIAG_SERVER_NAME` are the empty string,
/// which those two rows sanction by name ("this field is a zero-length string").
/// Both are driver-defined, and answering `SQL_DIAG_SERVER_NAME` with the data
/// source name would need it plumbed to a function that sees only a diagnostic
/// record.
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

            // The four statement-only header fields. Placed here with
            // SQL_DIAG_NUMBER and SQL_DIAG_RETURNCODE rather than in the
            // record-field match below, because the spec ignores RecNumber for
            // all six: "RecNumber is ignored for header fields." Reached from
            // below, the spec-correct rec_number of 0 would answer SQL_ERROR and
            // a positive one would fall through to the unknown-identifier arm.
            if let Some(field) = statement_header_field(diag_identifier) {
                // Spec, once per field in the Header Fields table: "Calling
                // SQLGetDiagField with a DiagIdentifier of ... on other than a
                // statement handle will return SQL_ERROR." The Diagnostics list
                // marks the same clause (DM), but the table states it four times
                // without a marker and the Comments section a fifth, so core
                // answers it rather than relying on the Driver Manager.
                let Ok(stmt) = scope.get::<StatementHandle<B>>(handle) else {
                    return Ok(SqlReturn::ERROR);
                };
                return Ok(match field {
                    StatementHeaderField::RowCount => {
                        let count = crate::ffi::cursor::statement_row_count::<B>(stmt);
                        if !diag_info.is_null() {
                            // SAFETY: diag_info is non-null (checked above); the
                            // spec types this field SQLLEN, so a whole isize is
                            // written. Unaligned because the pointer is the
                            // application's.
                            std::ptr::write_unaligned(diag_info as *mut isize, count);
                        }
                        if !string_length.is_null() {
                            // SAFETY: string_length is non-null (checked above)
                            // and the caller guarantees a writable i16.
                            std::ptr::write_unaligned(string_length, size_of::<isize>() as i16);
                        }
                        SqlReturn::SUCCESS
                    }
                    StatementHeaderField::CursorRowCount => {
                        if !diag_info.is_null() {
                            // SAFETY: as above; SQLLEN-wide for the same reason.
                            std::ptr::write_unaligned(
                                diag_info as *mut isize,
                                CURSOR_ROW_COUNT_UNAVAILABLE,
                            );
                        }
                        if !string_length.is_null() {
                            // SAFETY: as above.
                            std::ptr::write_unaligned(string_length, size_of::<isize>() as i16);
                        }
                        SqlReturn::SUCCESS
                    }
                    StatementHeaderField::DynamicFunction => {
                        // A character field, so the spec's negative-BufferLength
                        // rule applies: "The value requested was a character
                        // string and BufferLength was less than zero."
                        // `write_diag_string_checked` is the one place that
                        // states it.
                        //
                        // SAFETY: diag_info is either null (write_utf16 handles
                        // that) or a buffer of at least buffer_length/2 u16s.
                        write_diag_string_checked(
                            SQL_DIAG_DYNAMIC_FUNCTION_UNKNOWN,
                            diag_info,
                            buffer_length,
                            string_length,
                        )
                    }
                    StatementHeaderField::DynamicFunctionCode => {
                        if !diag_info.is_null() {
                            // SAFETY: diag_info is non-null (checked above); the
                            // spec types this field SQLINTEGER.
                            std::ptr::write_unaligned(
                                diag_info as *mut i32,
                                SQL_DIAG_UNKNOWN_STATEMENT,
                            );
                        }
                        if !string_length.is_null() {
                            // SAFETY: as above.
                            std::ptr::write_unaligned(string_length, size_of::<i32>() as i16);
                        }
                        SqlReturn::SUCCESS
                    }
                });
            }

            // Record fields: need a valid record number.
            //
            // `Ok`, not `Err`: same no-clear/no-push invariant as
            // `sql_get_diag_rec_w` above. An `Err` here would push a
            // diagnostic through `panic_safe`'s error path, corrupting the
            // very record set an application may be mid-iteration over.
            if rec_number <= 0 {
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
                    write_diag_string_checked(state_str, diag_info, buffer_length, string_length)
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
                    write_diag_string_checked(
                        &record.message,
                        diag_info,
                        buffer_length,
                        string_length,
                    )
                }
                // Both derived from this record's own SQLSTATE. The spec defines
                // exact values for them and marks neither optional; the empty
                // string it does sanction belongs to the two below.
                SQL_DIAG_CLASS_ORIGIN => {
                    // SAFETY: diag_info is either null (write_utf16 handles that) or a
                    // caller-allocated buffer of at least buffer_length/2 u16 values.
                    write_diag_string_checked(
                        class_origin(record.sqlstate.as_str()),
                        diag_info,
                        buffer_length,
                        string_length,
                    )
                }
                SQL_DIAG_SUBCLASS_ORIGIN => {
                    // SAFETY: as above.
                    write_diag_string_checked(
                        subclass_origin(record.sqlstate.as_str()),
                        diag_info,
                        buffer_length,
                        string_length,
                    )
                }
                // SQL_DIAG_CONNECTION_NAME and SQL_DIAG_SERVER_NAME. These two
                // really are allowed to be empty, and each says so in its own
                // row: "For diagnostic data structures associated with the
                // environment handle and for diagnostics that do not relate to
                // any connection, this field is a zero-length string."
                //
                // Both are "driver-defined". Answering SQL_DIAG_SERVER_NAME
                // ("the same as the value returned for a call to SQLGetInfo with
                // the SQL_DATA_SOURCE_NAME option") would mean plumbing the
                // connection's data source name to a function that is handed a
                // diagnostic record and nothing else. That is a change with its
                // own justification, not an oversight to be quietly inherited.
                SQL_DIAG_CONNECTION_NAME..=SQL_DIAG_SERVER_NAME => {
                    // SAFETY: as above.
                    write_diag_string_checked("", diag_info, buffer_length, string_length)
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
    use crate::test_utils::{
        MockBackend, alloc_env_conn_stmt, cleanup_env_conn_stmt, synthetic_result_set, with_handle,
    };
    use crate::types::sql_state;
    use crate::types::{InfoType, InfoValue};

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
        // careless transcription reaches for — 12 — is a real identifier,
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
        assert_eq!(SQL_DIAG_ROW_COUNT, 3);
        assert_eq!(SQL_DIAG_DYNAMIC_FUNCTION, 7);
        // The identifier the old SQL_DIAG_COLUMN_NUMBER collided with.
        assert_eq!(SQL_DIAG_DYNAMIC_FUNCTION_CODE, 12);
        assert_eq!(SQL_DIAG_CURSOR_ROW_COUNT, -1249);
        assert_eq!(SQL_DIAG_UNKNOWN_STATEMENT, 0);
    }

    /// `SQLGetDiagRecW` with a real buffer and no room in it must say the
    /// message was truncated. Its spec row is explicit ("SQL_SUCCESS_WITH_INFO:
    /// The \*MessageText buffer was too small to hold the requested diagnostic
    /// message") and the total-truncation case is the one an application is
    /// least able to detect for itself.
    #[test]
    fn diag_rec_zero_length_message_buffer_reports_truncation() {
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
            let mut msg_buf = [0u16; 1];
            let mut msg_len: i16 = 0;

            let ret = sql_get_diag_rec_w::<MockBackend>(
                HandleType::Env as i16,
                env,
                1,
                state.as_mut_ptr(),
                &mut native_err,
                msg_buf.as_mut_ptr(),
                0, // a real buffer, declared to have no room
                &mut msg_len,
            );
            assert_eq!(ret, SqlReturn::SUCCESS_WITH_INFO);
            assert!(msg_len > 0, "the required length is still reported");

            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    /// Four combinations, and all four occur. The class rule keys on the
    /// two-character class ("For ODBC-specific SQLSTATEs (all those whose
    /// SQLSTATE class is 'IM'), its value is 'ODBC 3.0'") while the subclass
    /// rule keys on the whole five-character state against a closed list, so the
    /// two answers are independent and a single shared implementation would be
    /// wrong for two of these rows.
    #[test]
    fn class_and_subclass_origin_are_independent_readings_of_the_sqlstate() {
        // ISO class, ISO subclass: an ordinary Open Group state.
        assert_eq!(
            class_origin(sql_state::CONNECTION_NOT_OPEN),
            SQL_DIAG_ORIGIN_ISO
        );
        assert_eq!(
            subclass_origin(sql_state::CONNECTION_NOT_OPEN),
            SQL_DIAG_ORIGIN_ISO
        );

        // ISO class, ODBC subclass: 08S01 is in the spec's enumerated list.
        assert_eq!(
            class_origin(sql_state::COMMUNICATION_LINK_FAILURE),
            SQL_DIAG_ORIGIN_ISO
        );
        assert_eq!(
            subclass_origin(sql_state::COMMUNICATION_LINK_FAILURE),
            SQL_DIAG_ORIGIN_ODBC
        );

        // ODBC class, ODBC subclass: IM001 is both.
        assert_eq!(class_origin("IM001"), SQL_DIAG_ORIGIN_ODBC);
        assert_eq!(subclass_origin("IM001"), SQL_DIAG_ORIGIN_ODBC);

        // ODBC class, ISO subclass: IM009 is class IM and is absent from the
        // spec's list, which is closed and does not run consecutively.
        assert_eq!(class_origin("IM009"), SQL_DIAG_ORIGIN_ODBC);
        assert_eq!(subclass_origin("IM009"), SQL_DIAG_ORIGIN_ISO);
    }

    /// The enumerated list is transcribed from the spec and is closed. Pinning
    /// its length and its two ends catches a paste that dropped or duplicated a
    /// row, which no individual lookup above would notice.
    #[test]
    fn the_odbc_specific_subclass_list_matches_the_spec() {
        assert_eq!(
            ODBC_SPECIFIC_SUBCLASS_STATES.len(),
            42,
            "the spec lists forty-two ODBC-specific SQLSTATEs for this field"
        );
        assert_eq!(ODBC_SPECIFIC_SUBCLASS_STATES.first(), Some(&"01S00"));
        assert_eq!(ODBC_SPECIFIC_SUBCLASS_STATES.last(), Some(&"IM012"));

        let mut sorted = ODBC_SPECIFIC_SUBCLASS_STATES.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            ODBC_SPECIFIC_SUBCLASS_STATES.len(),
            "a duplicated entry means one of the spec's rows was pasted over"
        );
    }

    /// End to end: the two fields answer from the record's own SQLSTATE, and the
    /// two that the spec really does allow to be empty stay empty. The Windows
    /// Driver Manager queries all four after SUCCESS_WITH_INFO, so these are read
    /// in practice.
    #[test]
    fn diag_field_origin_fields_answer_from_the_record() {
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );
            with_handle::<MockBackend, EnvironmentHandle<MockBackend>, _>(env, |handle| {
                handle.diagnostics.push(&crate::errors::OdbcError::general(
                    "a driver-specific condition".to_string(),
                    crate::types::SqlState::new("IM001"),
                ));
            });

            let read = |field: i16| -> String {
                let mut buf = [0u16; 16];
                let mut str_len: i16 = -1;
                assert_eq!(
                    sql_get_diag_field_w::<MockBackend>(
                        HandleType::Env as i16,
                        env,
                        1,
                        field,
                        buf.as_mut_ptr().cast::<c_void>(),
                        (buf.len() * 2) as i16,
                        &mut str_len,
                    ),
                    SqlReturn::SUCCESS
                );
                let units = (str_len / 2) as usize;
                String::from_utf16_lossy(&buf[..units])
            };

            assert_eq!(read(SQL_DIAG_CLASS_ORIGIN), SQL_DIAG_ORIGIN_ODBC);
            assert_eq!(read(SQL_DIAG_SUBCLASS_ORIGIN), SQL_DIAG_ORIGIN_ODBC);
            // Sanctioned empty: "For diagnostic data structures associated with
            // the environment handle ... this field is a zero-length string."
            assert_eq!(read(SQL_DIAG_CONNECTION_NAME), "");
            assert_eq!(read(SQL_DIAG_SERVER_NAME), "");

            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    /// The spec tells applications to pass a negative `BufferLength` for a
    /// fixed-length field — "If *\*DiagInfoPtr* contains a fixed-length data
    /// type, *BufferLength* is SQL_IS_INTEGER, SQL_IS_UINTEGER, SQL_IS_SMALLINT,
    /// or SQL_IS_USMALLINT, as appropriate" — and every one of those constants
    /// is negative. Its SQL_ERROR condition is narrower than the check that was
    /// here: "The value requested **was a character string** and *BufferLength*
    /// was less than zero."
    ///
    /// So the integer fields must answer, and a conventional call must not be a
    /// failure.
    #[test]
    fn diag_field_integer_fields_ignore_a_negative_buffer_length() {
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

            let mut native: i32 = 0;
            assert_eq!(
                sql_get_diag_field_w::<MockBackend>(
                    HandleType::Env as i16,
                    env,
                    1,
                    SQL_DIAG_NATIVE,
                    std::ptr::from_mut(&mut native).cast::<c_void>(),
                    odbc_sys::IS_INTEGER as i16,
                    std::ptr::null_mut(),
                ),
                SqlReturn::SUCCESS,
                "SQL_DIAG_NATIVE is SQLINTEGER, so BufferLength is ignored"
            );

            let mut column: i32 = 0;
            assert_eq!(
                sql_get_diag_field_w::<MockBackend>(
                    HandleType::Env as i16,
                    env,
                    1,
                    SQL_DIAG_COLUMN_NUMBER,
                    std::ptr::from_mut(&mut column).cast::<c_void>(),
                    odbc_sys::IS_INTEGER as i16,
                    std::ptr::null_mut(),
                ),
                SqlReturn::SUCCESS
            );
            assert_eq!(column, SQL_NO_COLUMN_NUMBER);

            let mut row: isize = 0;
            assert_eq!(
                sql_get_diag_field_w::<MockBackend>(
                    HandleType::Env as i16,
                    env,
                    1,
                    SQL_DIAG_ROW_NUMBER,
                    std::ptr::from_mut(&mut row).cast::<c_void>(),
                    odbc_sys::IS_SMALLINT as i16,
                    std::ptr::null_mut(),
                ),
                SqlReturn::SUCCESS
            );
            assert_eq!(row, SQL_NO_ROW_NUMBER);

            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    /// The other half of the same rule: a character field with a negative
    /// `BufferLength` is still SQL_ERROR, and narrowing the check must not have
    /// dropped it. `SQL_DIAG_SQLSTATE` is already covered by
    /// `diag_field_negative_buffer_length_returns_error`; this covers the rest,
    /// including the statement-only `SQL_DIAG_DYNAMIC_FUNCTION`.
    #[test]
    fn diag_field_character_fields_still_reject_a_negative_buffer_length() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                handle.statement = Some(crate::handles::StatementData::Synthetic(
                    synthetic_result_set(vec![]),
                ));
                handle
                    .diagnostics
                    .push(&crate::errors::OdbcError::NoResultSet);
            });

            let mut buf = [0u16; DIAG_MSG_BUF_LEN];
            for (field, rec_number) in [
                (SQL_DIAG_MESSAGE_TEXT, 1i16),
                (SQL_DIAG_CLASS_ORIGIN, 1),
                (SQL_DIAG_SERVER_NAME, 1),
                (SQL_DIAG_DYNAMIC_FUNCTION, 0),
            ] {
                assert_eq!(
                    sql_get_diag_field_w::<MockBackend>(
                        HandleType::Stmt as i16,
                        stmt,
                        rec_number,
                        field,
                        buf.as_mut_ptr().cast::<c_void>(),
                        -1,
                        std::ptr::null_mut(),
                    ),
                    SqlReturn::ERROR,
                    "character field {field} must reject a negative BufferLength"
                );
            }

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// `SQL_DIAG_ROW_COUNT` is the same number `SQLRowCount` reports — the
    /// spec's row says so outright: "The data in this field is also returned in
    /// the *RowCountPtr* argument of **SQLRowCount**." Asserting both in one
    /// test is what stops the two computations drifting apart.
    #[test]
    fn diag_field_row_count_is_the_number_sql_row_count_reports() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                handle.statement = Some(crate::handles::StatementData::Synthetic(
                    synthetic_result_set(vec![
                        vec![crate::types::ColumnValue::I32(1)],
                        vec![crate::types::ColumnValue::I32(2)],
                        vec![crate::types::ColumnValue::I32(3)],
                    ]),
                ));
            });

            let mut diag_count: isize = -999;
            let mut str_len: i16 = 0;
            assert_eq!(
                sql_get_diag_field_w::<MockBackend>(
                    HandleType::Stmt as i16,
                    stmt,
                    0, // header field: RecNumber is ignored
                    SQL_DIAG_ROW_COUNT,
                    std::ptr::from_mut(&mut diag_count).cast::<c_void>(),
                    0,
                    &mut str_len,
                ),
                SqlReturn::SUCCESS
            );

            let mut row_count: isize = -999;
            assert_eq!(
                crate::ffi::cursor::sql_row_count::<MockBackend>(stmt, &mut row_count),
                SqlReturn::SUCCESS
            );

            assert_eq!(diag_count, 3);
            assert_eq!(
                diag_count, row_count,
                "SQL_DIAG_ROW_COUNT and SQLRowCount must be one number"
            );

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// Spec: "*RecNumber* is ignored for header fields." Both the spec-correct 0
    /// and a positive value must answer, which is precisely what the
    /// `rec_number <= 0` guard and the unknown-identifier arm between them made
    /// impossible before.
    #[test]
    fn diag_field_row_count_ignores_rec_number() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                handle.statement = Some(crate::handles::StatementData::Synthetic(
                    synthetic_result_set(vec![vec![crate::types::ColumnValue::I32(1)]]),
                ));
            });

            for rec_number in [0i16, 1, 7, -1] {
                let mut count: isize = -999;
                assert_eq!(
                    sql_get_diag_field_w::<MockBackend>(
                        HandleType::Stmt as i16,
                        stmt,
                        rec_number,
                        SQL_DIAG_ROW_COUNT,
                        std::ptr::from_mut(&mut count).cast::<c_void>(),
                        0,
                        std::ptr::null_mut(),
                    ),
                    SqlReturn::SUCCESS,
                    "rec_number {rec_number} must be ignored for a header field"
                );
                assert_eq!(count, 1);
            }

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// The Header Fields table says it four times, once per field: "Calling
    /// **SQLGetDiagField** with a *DiagIdentifier* of … on other than a
    /// statement handle will return SQL_ERROR." The Diagnostics list marks the
    /// same clause (DM), but four unmarked statements plus the Comments
    /// section's fifth outweigh it, so core answers rather than relying on the
    /// Driver Manager to intercept.
    #[test]
    fn diag_field_statement_only_headers_reject_a_non_statement_handle() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();

            for field in [
                SQL_DIAG_ROW_COUNT,
                SQL_DIAG_CURSOR_ROW_COUNT,
                SQL_DIAG_DYNAMIC_FUNCTION,
                SQL_DIAG_DYNAMIC_FUNCTION_CODE,
            ] {
                let mut buf = [0u16; DIAG_MSG_BUF_LEN];
                for handle in [env, conn] {
                    assert_eq!(
                        sql_get_diag_field_w::<MockBackend>(
                            HandleType::Env as i16,
                            handle,
                            0,
                            field,
                            buf.as_mut_ptr().cast::<c_void>(),
                            (DIAG_MSG_BUF_LEN * 2) as i16,
                            std::ptr::null_mut(),
                        ),
                        SqlReturn::ERROR,
                        "field {field} must be SQL_ERROR on a non-statement handle"
                    );
                }
            }

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// `SQL_DIAG_CURSOR_ROW_COUNT`'s semantics "depend on the SQLGetInfo
    /// information types … SQL_FORWARD_ONLY_CURSOR_ATTRIBUTES2 … (in the
    /// SQL_CA2_CRC_EXACT and SQL_CA2_CRC_APPROXIMATE bits)". Core claims
    /// neither bit, so no cursor row count is available and zero is the answer
    /// rather than a guess. Both halves are asserted here so the coupling is
    /// visible if either moves: a backend that starts claiming a CRC bit needs
    /// this field to start answering.
    ///
    /// The assertion is against the two CRC bits, not against the whole info
    /// value. That info type is *not* zero — it carries
    /// `SQL_CA2_READ_ONLY_CONCURRENCY` — and a test written against the whole
    /// value would fail the next time an unrelated capability bit is added while
    /// still not noticing a CRC bit appearing inside a value that stayed
    /// non-zero. The bits named by the spec's sentence are what this is about.
    #[test]
    fn diag_field_cursor_row_count_is_zero_because_core_claims_no_crc_bits() {
        let Some(InfoValue::U32(attrs2)) = crate::backend::default_get_info::<MockBackend>(
            None,
            InfoType::ForwardOnlyCursorAttributes2,
        ) else {
            panic!("SQL_FORWARD_ONLY_CURSOR_ATTRIBUTES2 is a U32-shaped info type core answers");
        };
        assert_eq!(
            attrs2 & (crate::types::SQL_CA2_CRC_EXACT | crate::types::SQL_CA2_CRC_APPROXIMATE),
            0,
            "core claims neither SQL_CA2_CRC_* bit, which is why the field below \
             is 0; a backend that starts claiming one needs this field to answer"
        );

        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                handle.statement = Some(crate::handles::StatementData::Synthetic(
                    synthetic_result_set(vec![vec![crate::types::ColumnValue::I32(1)]]),
                ));
            });

            let mut count: isize = !0;
            assert_eq!(
                sql_get_diag_field_w::<MockBackend>(
                    HandleType::Stmt as i16,
                    stmt,
                    0,
                    SQL_DIAG_CURSOR_ROW_COUNT,
                    std::ptr::from_mut(&mut count).cast::<c_void>(),
                    0,
                    std::ptr::null_mut(),
                ),
                SqlReturn::SUCCESS
            );
            assert_eq!(
                count, CURSOR_ROW_COUNT_UNAVAILABLE,
                "a four-byte write would leave the high half of this SQLLEN standing"
            );

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// The spec's "Values of the Dynamic Function Fields" table has a row for a
    /// driver that cannot classify the statement it ran: "Unknown | *empty
    /// string* | SQL_DIAG_UNKNOWN_STATEMENT". Core parses no SQL, so that row is
    /// the accurate answer rather than a placeholder.
    #[test]
    fn diag_field_dynamic_function_is_the_spec_unknown_statement_pair() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                handle.statement = Some(crate::handles::StatementData::Synthetic(
                    synthetic_result_set(vec![]),
                ));
            });

            let mut name = [0xFFFFu16; 8];
            let mut str_len: i16 = -1;
            assert_eq!(
                sql_get_diag_field_w::<MockBackend>(
                    HandleType::Stmt as i16,
                    stmt,
                    0,
                    SQL_DIAG_DYNAMIC_FUNCTION,
                    name.as_mut_ptr().cast::<c_void>(),
                    (name.len() * 2) as i16,
                    &mut str_len,
                ),
                SqlReturn::SUCCESS
            );
            assert_eq!(str_len, 0, "the empty string is zero bytes");
            assert_eq!(name[0], 0, "the buffer must be null-terminated");

            let mut code: i32 = -999;
            assert_eq!(
                sql_get_diag_field_w::<MockBackend>(
                    HandleType::Stmt as i16,
                    stmt,
                    0,
                    SQL_DIAG_DYNAMIC_FUNCTION_CODE,
                    std::ptr::from_mut(&mut code).cast::<c_void>(),
                    0,
                    std::ptr::null_mut(),
                ),
                SqlReturn::SUCCESS
            );
            assert_eq!(code, SQL_DIAG_UNKNOWN_STATEMENT);

            cleanup_env_conn_stmt(env, conn, stmt);
        }
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
        // The handle here is an environment, so the answer is the spec's
        // SQL_ERROR for a statement-only field rather than a value. What the
        // test is really pinning is the buffer: the record fields' sentinel is
        // -1, and -1 is a *valid* dynamic function code, SQL_DIAG_CREATE_INDEX.
        // A constant collision would tell the application every statement it ran
        // was a CREATE INDEX, with no error anywhere to suggest otherwise.
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
            assert_eq!(ret, SqlReturn::ERROR);
            assert_eq!(
                value, 0,
                "a statement-only field wrote through a non-statement handle"
            );

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
