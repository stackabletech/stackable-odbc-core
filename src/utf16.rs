//! UTF-16 conversion helpers for the Wide ODBC ABI (`utf16_to_string`,
//! `write_utf16`), and the bound every `SQL_NTS` scan in the crate shares.

use crate::errors::OdbcError;
use crate::types::SqlReturn;

/// Maximum number of units to scan when searching for a null terminator
/// (`SQL_NTS`) — UTF-16 code units or bytes, depending on the scan. This
/// prevents unbounded reads on malformed input. 32 767 units is generous for
/// any realistic ODBC string.
///
/// `pub(crate)` because `SQLPutData` resolves `SQL_NTS` over an
/// application-supplied buffer too. A second bound stated there would be a
/// second answer to one question, and the answer that went missing is the one
/// that let an unterminated buffer be read past its allocation.
pub(crate) const MAX_NTS_SCAN: usize = i16::MAX as usize;

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

    // Read element-wise rather than building a `&[u16]`: `from_raw_parts`
    // requires the pointer to be aligned for its pointee type, and this one is
    // application-supplied, so an ODBC application binding row-wise can hand
    // over an odd address. Constructing the slice at all would be undefined
    // behaviour, before anything is even read from it.
    let units: Vec<u16> = if len < 0 {
        // Scan to null terminator, bounded to prevent OOB reads.
        let mut units = Vec::new();
        while units.len() < MAX_NTS_SCAN {
            // SAFETY: the caller guarantees a null-terminated string, and the
            // scan is bounded above.
            let unit = unsafe { std::ptr::read_unaligned(ptr.add(units.len())) };
            if unit == 0 {
                break;
            }
            units.push(unit);
        }
        units
    } else {
        // SAFETY: the caller guarantees `len` readable code units.
        (0..len as usize)
            .map(|i| unsafe { std::ptr::read_unaligned(ptr.add(i)) })
            .collect()
    };

    Ok(String::from_utf16_lossy(&units))
}

/// Length, in `u16` code units, of a null-terminated UTF-16 string, bounded to
/// [`MAX_NTS_SCAN`]. The terminator itself is not counted.
///
/// The counting half of [`utf16_to_string`]'s scan, for a caller that wants the
/// length rather than the decoded text — `SQLPutData` accumulates raw bytes and
/// decodes once, at the end, so decoding each chunk would be both wasteful and
/// wrong across a split surrogate pair.
///
/// # Safety
///
/// `ptr` must be non-null and either point to a null-terminated UTF-16 string
/// or be valid for [`MAX_NTS_SCAN`] readable code units.
pub(crate) unsafe fn nts_utf16_len(ptr: *const u16) -> usize {
    let mut len = 0;
    while len < MAX_NTS_SCAN {
        // SAFETY: the caller guarantees a terminator within the bound, or that
        // many readable units. `read_unaligned` because an ODBC application may
        // place its buffer at any offset inside a packed structure.
        if unsafe { std::ptr::read_unaligned(ptr.add(len)) } == 0 {
            break;
        }
        len += 1;
    }
    len
}

/// Length, in bytes, of a null-terminated byte string, bounded to
/// [`MAX_NTS_SCAN`]. The terminator itself is not counted.
///
/// The byte counterpart of [`nts_utf16_len`], for `SQL_C_CHAR` and the other
/// single-byte C types. `CStr::from_ptr` is the alternative and is unbounded,
/// so a buffer whose terminator is missing is read past its own allocation.
///
/// # Safety
///
/// `ptr` must be non-null and either point to a null-terminated byte string or
/// be valid for [`MAX_NTS_SCAN`] readable bytes.
pub(crate) unsafe fn nts_byte_len(ptr: *const u8) -> usize {
    let mut len = 0;
    while len < MAX_NTS_SCAN {
        // SAFETY: as above. `u8` has alignment 1, so no unaligned read is
        // possible and no `read_unaligned` is needed.
        if unsafe { *ptr.add(len) } == 0 {
            break;
        }
        len += 1;
    }
    len
}

/// Write a UTF-16 encoded string into an output buffer.
///
/// Always writes the total length (in `u16` code units, not including null) to
/// `len_ptr` if it is non-null.
///
/// If `out_ptr` is null, nothing is written and `SUCCESS` is returned: that is
/// the length-query form, and the caller asked for no data.
///
/// If `out_ptr` is non-null but `buf_len <= 0`, nothing is written and
/// `SUCCESS_WITH_INFO` is returned: the caller supplied a buffer with no room
/// for even the null terminator, which is total truncation.
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
        // Unaligned: an application using row-wise binding hands out pointers
        // at arbitrary offsets into a packed buffer, so `*len_ptr = ..` would
        // be undefined behaviour. In a debug build it is not even silent — the
        // standard library's precondition check fires a *non-unwinding* panic,
        // which `panic_safe` cannot catch, so the host process aborts.
        unsafe { std::ptr::write_unaligned(len_ptr, reported_len) };
    }

    // A null output buffer is a pure length query: the application asked how much
    // room it needs and gets SQL_SUCCESS plus the count, having supplied nowhere
    // to write. The spec sanctions it by name: "If DiagInfoPtr is NULL,
    // StringLengthPtr will still return the total number of bytes ... available
    // to return in the buffer pointed to by DiagInfoPtr."
    if out_ptr.is_null() {
        return SqlReturn::SUCCESS;
    }

    // A non-null buffer with no room in it is a different thing, and sharing the
    // branch above gave the two the same answer. The application supplied
    // somewhere to write and nothing was written, not even the null terminator,
    // which is total truncation, and reporting SUCCESS made it
    // indistinguishable from a complete write. The length reported back is the
    // length *needed*, so it is the same number either way and cannot be used to
    // tell them apart.
    if buf_len <= 0 {
        return SqlReturn::SUCCESS_WITH_INFO;
    }

    let capacity = (buf_len - 1) as usize; // reserve one slot for null terminator
    let copy_count = wide.len().min(capacity);

    // Copied as bytes rather than as `u16`s: `copy_nonoverlapping` requires
    // both pointers to be aligned for their pointee type, and `out_ptr` is
    // application-supplied so it may be odd. `u8` has alignment 1, so the
    // byte-wise form carries no alignment requirement at all.
    unsafe {
        std::ptr::copy_nonoverlapping(
            wide.as_ptr().cast::<u8>(),
            out_ptr.cast::<u8>(),
            copy_count * size_of::<u16>(),
        );
        // Unaligned for the same reason. `add` itself is fine on an unaligned
        // pointer; only the store needs care.
        std::ptr::write_unaligned(out_ptr.add(copy_count), 0u16); // null terminator
    }

    if copy_count < wide.len() {
        SqlReturn::SUCCESS_WITH_INFO
    } else {
        SqlReturn::SUCCESS
    }
}

/// Records the `01004` diagnostic that goes with a truncated string write.
///
/// [`write_utf16`] reports truncation by returning `SQL_SUCCESS_WITH_INFO`, but
/// the spec also requires a diagnostic record saying why: an application that
/// sees `SQL_SUCCESS_WITH_INFO` calls `SQLGetDiagRec` to find out what happened,
/// and with no record there it cannot distinguish truncation from any other
/// informational condition.
///
/// Not used by the `SQLGetDiagRec` / `SQLGetDiagField` paths themselves. Those
/// return `SQL_SUCCESS_WITH_INFO` for a truncated message too, but posting a
/// record about reading a record would both recurse and overwrite the queue the
/// application is in the middle of reading.
pub(crate) fn note_truncation(
    ret: SqlReturn,
    diagnostics: &mut crate::diagnostics::DiagnosticQueue,
) -> SqlReturn {
    if ret == SqlReturn::SUCCESS_WITH_INFO {
        diagnostics.push(&crate::errors::OdbcError::StringTruncated);
    }
    ret
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ODBC applications using row-wise binding hand the driver pointers at
    /// arbitrary offsets into a packed buffer, so alignment is never
    /// guaranteed. `ffi/metadata.rs` says so and uses unaligned writes for that
    /// reason; `write_utf16` did not, and every existing test here passes a
    /// naturally aligned `Vec<u16>`, which is why nothing caught it.
    ///
    /// On x86-64 an unaligned access merely works, so this test only fails
    /// under Miri. That is the point: it is a correctness statement about the
    /// abstract machine, not about this host.
    #[test]
    fn write_utf16_accepts_an_unaligned_output_buffer() {
        // Offset one byte into a u16-aligned allocation, so the result is
        // guaranteed odd on every platform. Offsetting into a Vec<u8> would
        // not be: a byte allocation has alignment 1, so its base may already
        // be odd and +1 would land back on an even address.
        let mut arena = vec![0u16; 16];
        let mut len_arena = vec![0i16; 4];
        // SAFETY: both offsets stay inside their allocation.
        let out_ptr = unsafe { arena.as_mut_ptr().cast::<u8>().add(1) }.cast::<u16>();
        let len_ptr = unsafe { len_arena.as_mut_ptr().cast::<u8>().add(1) }.cast::<i16>();

        let ret = unsafe { write_utf16("hi", out_ptr, 8, len_ptr) };
        assert_eq!(ret, SqlReturn::SUCCESS);

        // SAFETY: read back through the same unaligned pointers.
        unsafe {
            assert_eq!(std::ptr::read_unaligned(out_ptr), 'h' as u16);
            assert_eq!(std::ptr::read_unaligned(out_ptr.add(1)), 'i' as u16);
            assert_eq!(
                std::ptr::read_unaligned(out_ptr.add(2)),
                0,
                "null terminator"
            );
            assert_eq!(std::ptr::read_unaligned(len_ptr), 2);
        }
    }

    #[test]
    fn write_utf16_accepts_an_unaligned_buffer_when_truncating() {
        // The truncating path writes the terminator at a different offset, so
        // it is a separate unaligned store.
        let mut arena = vec![0u16; 16];
        // SAFETY: the offset stays inside the allocation.
        let out_ptr = unsafe { arena.as_mut_ptr().cast::<u8>().add(1) }.cast::<u16>();

        let ret = unsafe { write_utf16("hello", out_ptr, 3, std::ptr::null_mut()) };
        assert_eq!(ret, SqlReturn::SUCCESS_WITH_INFO);

        // SAFETY: read back through the same unaligned pointer.
        unsafe {
            assert_eq!(std::ptr::read_unaligned(out_ptr), 'h' as u16);
            assert_eq!(std::ptr::read_unaligned(out_ptr.add(1)), 'e' as u16);
            assert_eq!(
                std::ptr::read_unaligned(out_ptr.add(2)),
                0,
                "null terminator after buf_len - 1 characters"
            );
        }
    }

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

    /// A non-null buffer with no room in it is total truncation: nothing was
    /// written, not even the null terminator. Reporting SUCCESS gave it the same
    /// answer as a complete write, so an application had no way to tell them
    /// apart: the length it reads back is the length *needed*, which is the same
    /// number either way.
    ///
    /// Spec, on the SQL_SUCCESS_WITH_INFO row: "The \*MessageText buffer was too
    /// small to hold the requested diagnostic message."
    #[test]
    fn write_utf16_reports_truncation_for_a_zero_length_non_null_buffer() {
        let mut buf = [0u16; 4];
        let mut len: i16 = -1;
        // SAFETY: `buf` is a real allocation and `len` is writable; the declared
        // length of 0 is the input under test.
        let ret = unsafe { write_utf16("hello", buf.as_mut_ptr(), 0, &mut len) };
        assert_eq!(ret, SqlReturn::SUCCESS_WITH_INFO);
        assert_eq!(len, 5, "the required length is still reported");
        assert_eq!(
            buf, [0u16; 4],
            "nothing may be written into a zero-length buffer"
        );
    }

    /// A null buffer is a pure length query and stays SUCCESS. The spec
    /// sanctions it: "If *DiagInfoPtr* is NULL, *StringLengthPtr* will still
    /// return the total number of bytes ... available to return in the buffer".
    /// Collapsing the two cases is what produced the wrong answer above.
    #[test]
    fn write_utf16_still_reports_success_for_a_null_buffer_length_query() {
        let mut len: i16 = -1;
        // SAFETY: a null out pointer is the documented length-query form.
        let ret = unsafe { write_utf16("hello", std::ptr::null_mut(), 0, &mut len) };
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!(len, 5);
    }

    /// A negative declared length is the same case: the application supplied a
    /// buffer and said there is no room in it.
    #[test]
    fn write_utf16_reports_truncation_for_a_negative_buffer_length() {
        let mut buf = [0u16; 4];
        let mut len: i16 = 0;
        // SAFETY: as above.
        let ret = unsafe { write_utf16("hi", buf.as_mut_ptr(), -1, &mut len) };
        assert_eq!(ret, SqlReturn::SUCCESS_WITH_INFO);
        assert_eq!(buf, [0u16; 4]);
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
