//! UTF-16 conversion helpers for the Wide ODBC ABI (`utf16_to_string`,
//! `write_utf16`).

use crate::errors::OdbcError;
use crate::types::SqlReturn;

/// Maximum number of UTF-16 code units to scan when searching for a null
/// terminator (`SQL_NTS`). This prevents unbounded reads on malformed input.
/// 32 767 code units (64 KiB) is generous for any realistic ODBC string.
const MAX_NTS_SCAN: usize = i16::MAX as usize;

/// Convert a UTF-16 encoded string pointer into a Rust `String`.
///
/// `len` is measured in UTF-16 code units (not bytes). Pass `SQL_NTS` (-3) or
/// any negative value to scan for a null terminator.
///
/// A null `ptr` yields SQLSTATE `HY009` (invalid use of null pointer), which is
/// what the spec's diagnostics tables list for a required string argument that
/// is null. It is *not* `SQL_INVALID_HANDLE`: that return code is reserved for
/// a bad handle argument, and returning it here would tell the application its
/// connection or statement had been corrupted.
///
/// # Safety
///
/// The caller must ensure:
/// - If `len >= 0`, the pointer must be valid for at least `len` `u16` elements.
/// - If `len < 0` (SQL_NTS), the pointer must point to a null-terminated UTF-16
///   string. The scan is bounded to `MAX_NTS_SCAN` code units to prevent
///   unbounded reads on malformed input.
pub unsafe fn utf16_to_string(ptr: *const u16, len: i32) -> Result<String, OdbcError> {
    if ptr.is_null() {
        return Err(OdbcError::general(
            "null string pointer",
            crate::types::SqlState::invalid_use_of_null_pointer(),
        ));
    }

    let slice: &[u16] = if len < 0 {
        // Scan to null terminator, bounded to prevent OOB reads.
        let mut end = 0usize;
        while end < MAX_NTS_SCAN && unsafe { *ptr.add(end) } != 0 {
            end += 1;
        }
        unsafe { std::slice::from_raw_parts(ptr, end) }
    } else {
        unsafe { std::slice::from_raw_parts(ptr, len as usize) }
    };

    Ok(String::from_utf16_lossy(slice))
}

/// Write a UTF-16 encoded string into an output buffer.
///
/// Always writes the total length (in `u16` code units, not including null) to
/// `len_ptr` if it is non-null.
///
/// If `out_ptr` is null or `buf_len <= 0`, no data is written and
/// `SUCCESS_WITH_INFO` is returned.
///
/// If the value fits entirely (including null terminator), returns `SUCCESS`.
/// Otherwise truncates to `buf_len - 1` chars, writes the null terminator, and
/// returns `SUCCESS_WITH_INFO`.
///
/// # Safety
///
/// The caller must ensure `out_ptr` (when non-null) points to a buffer of at
/// least `buf_len` `u16` elements, and that `len_ptr` (when non-null) is a
/// valid writable `i16`.
pub unsafe fn write_utf16(
    value: &str,
    out_ptr: *mut u16,
    buf_len: i16,
    len_ptr: *mut i16,
) -> SqlReturn {
    let wide: Vec<u16> = value.encode_utf16().collect();

    // Always report the total length needed (without null terminator).
    // Saturate to i16::MAX to avoid silent overflow on very long strings.
    if !len_ptr.is_null() {
        let reported_len = wide.len().min(i16::MAX as usize) as i16;
        unsafe { *len_ptr = reported_len };
    }

    // If no output buffer is provided, just report the length.
    // Return SUCCESS (not SUCCESS_WITH_INFO): the caller is just querying
    // the required buffer size, not experiencing truncation.
    if out_ptr.is_null() || buf_len <= 0 {
        return SqlReturn::SUCCESS;
    }

    let capacity = (buf_len - 1) as usize; // reserve one slot for null terminator
    let copy_count = wide.len().min(capacity);

    unsafe {
        std::ptr::copy_nonoverlapping(wide.as_ptr(), out_ptr, copy_count);
        *out_ptr.add(copy_count) = 0u16; // null terminator
    }

    if copy_count < wide.len() {
        SqlReturn::SUCCESS_WITH_INFO
    } else {
        SqlReturn::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_ascii() {
        let input = "hello world";
        let wide: Vec<u16> = input.encode_utf16().chain(std::iter::once(0)).collect();
        let result = unsafe { utf16_to_string(wide.as_ptr(), -1) }.unwrap();
        assert_eq!(result, input);
    }

    #[test]
    fn roundtrip_with_explicit_length() {
        let input = "hello";
        let wide: Vec<u16> = input.encode_utf16().collect();
        let result = unsafe { utf16_to_string(wide.as_ptr(), 5) }.unwrap();
        assert_eq!(result, input);
    }

    /// A null string argument is HY009, not `SQL_INVALID_HANDLE`. Returning the
    /// latter would tell the application its handle was unusable when only one
    /// argument was wrong.
    #[test]
    fn null_pointer_returns_invalid_use_of_null_pointer() {
        let err = unsafe { utf16_to_string(std::ptr::null(), 0) }
            .expect_err("null pointer must be rejected");
        assert_eq!(
            err.sqlstate().as_str(),
            crate::types::sql_state::INVALID_USE_OF_NULL_POINTER
        );
        assert_eq!(err.sql_return(), SqlReturn::ERROR);
    }

    #[test]
    fn write_utf16_fits_in_buffer() {
        let mut buf = [0u16; 20];
        let mut len: i16 = 0;
        let ret = unsafe { write_utf16("hello", buf.as_mut_ptr(), 20, &mut len) };
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!(len, 5);
        let result = String::from_utf16_lossy(&buf[..5]);
        assert_eq!(result, "hello");
    }

    #[test]
    fn write_utf16_truncation() {
        let mut buf = [0u16; 4]; // room for 3 chars + null
        let mut len: i16 = 0;
        let ret = unsafe { write_utf16("hello", buf.as_mut_ptr(), 4, &mut len) };
        assert_eq!(ret, SqlReturn::SUCCESS_WITH_INFO);
        assert_eq!(len, 5); // reports total length needed
        let result = String::from_utf16_lossy(&buf[..3]);
        assert_eq!(result, "hel");
    }

    #[test]
    fn write_utf16_null_output_ptr() {
        // Null output buffer just queries the required length; returns SUCCESS.
        let mut len: i16 = 0;
        let ret = unsafe { write_utf16("hello", std::ptr::null_mut(), 0, &mut len) };
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!(len, 5);
    }

    #[test]
    fn lone_high_surrogate_does_not_panic() {
        // U+D800 is a lone high surrogate, not valid UTF-16 but must not panic.
        let wide = [0xD800u16, 0u16]; // null-terminated
        let result = unsafe { utf16_to_string(wide.as_ptr(), -1) };
        // from_utf16_lossy replaces the surrogate with U+FFFD; we just verify no panic
        assert!(result.is_ok());
    }

    #[test]
    fn lone_low_surrogate_does_not_panic() {
        // U+DC00 is a lone low surrogate.
        let wide = [0xDC00u16, 0u16];
        let result = unsafe { utf16_to_string(wide.as_ptr(), -1) };
        assert!(result.is_ok());
    }

    #[test]
    fn null_terminator_mid_string_stops_scan() {
        // NTS mode: scan stops at the first null code unit.
        let wide = [b'h' as u16, b'i' as u16, 0u16, b'x' as u16, 0u16];
        let result = unsafe { utf16_to_string(wide.as_ptr(), -1) }.unwrap();
        assert_eq!(result, "hi");
    }

    #[test]
    fn explicit_length_includes_null_mid_string() {
        // Explicit length: null code units are treated as data (len=4 means 4 u16s).
        let wide = [b'h' as u16, 0u16, b'i' as u16, b'!' as u16];
        let result = unsafe { utf16_to_string(wide.as_ptr(), 4) }.unwrap();
        // The null at position 1 becomes a real null character in the string.
        assert_eq!(result.len(), 4); // 'h', '\0', 'i', '!'
    }

    #[test]
    fn empty_input_returns_empty_string() {
        let wide = [0u16]; // just a null terminator
        let result = unsafe { utf16_to_string(wide.as_ptr(), -1) }.unwrap();
        assert_eq!(result, "");
    }

    #[test]
    fn single_character_roundtrip() {
        let wide = [b'A' as u16, 0u16];
        let result = unsafe { utf16_to_string(wide.as_ptr(), -1) }.unwrap();
        assert_eq!(result, "A");
    }

    #[test]
    fn write_utf16_into_exact_fit_buffer() {
        // "hi" = 2 chars; buffer of 3 (2 chars + 1 null) = exact fit
        let mut buf = [0u16; 3];
        let mut len: i16 = 0;
        let ret = unsafe { write_utf16("hi", buf.as_mut_ptr(), 3, &mut len) };
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!(len, 2);
        assert_eq!(buf[0], b'h' as u16);
        assert_eq!(buf[1], b'i' as u16);
        assert_eq!(buf[2], 0u16); // null terminator
    }
}
