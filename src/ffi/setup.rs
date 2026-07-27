//! `ConfigDSNW`, the ODBC installer entry point for DSN configuration.

use std::collections::HashMap;

/// Parses a null-separated, double-null terminated ODBC UTF-16 attribute string.
///
/// Each segment is a `Key=Value` pair encoded as UTF-16LE, separated by a single
/// null `u16`, with the list terminated by a double null.
///
/// # Safety
/// `ptr` must be either null or point to a valid double-null-terminated `u16` sequence.
// Only `config_dsn_w` (Windows-only) calls this outside of tests, so the lint is
// suppressed on non-Windows targets only, on Windows a genuinely unused parser
// still warns.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) unsafe fn parse_attributes_w(ptr: *const u16) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if ptr.is_null() {
        return map;
    }
    let mut pos = 0usize;
    loop {
        let start = pos;
        // Walk to the next null u16
        // SAFETY: ptr is non-null (checked above) and caller guarantees it points to
        // a double-null-terminated UTF-16 sequence, so reading ptr.add(pos) is valid
        // until the double-null terminator is reached.
        while unsafe { *ptr.add(pos) } != 0 {
            pos += 1;
        }
        if pos == start {
            // Empty segment signals end of list
            break;
        }
        // SAFETY: ptr is non-null and valid (same invariant as above). The slice
        // [start, pos) lies within the same contiguous allocation and does not cross
        // the double-null terminator, so from_raw_parts is safe here.
        let code_units = unsafe { std::slice::from_raw_parts(ptr.add(start), pos - start) };
        let segment = String::from_utf16_lossy(code_units);
        if let Some(eq_pos) = segment.find('=') {
            map.insert(
                segment[..eq_pos].to_string(),
                segment[eq_pos + 1..].to_string(),
            );
            // Segments without '=' are silently skipped
        }
        pos += 1; // skip the null u16
    }
    map
}

#[cfg(windows)]
#[link(name = "odbccp32", kind = "raw-dylib")]
unsafe extern "system" {
    /// Registers a DSN name under ODBC.INI (Unicode variant).
    unsafe fn SQLWriteDSNToIniW(lpszDSN: *const u16, lpszDriver: *const u16) -> i32;
    /// Removes a DSN entry from ODBC.INI (Unicode variant).
    unsafe fn SQLRemoveDSNFromIniW(lpszDSN: *const u16) -> i32;
    /// Writes a per-DSN attribute to ODBC.INI via the ODBC installer (Unicode variant).
    ///
    /// This is distinct from the kernel32 `WritePrivateProfileStringW`: it goes
    /// through `odbccp32.dll` and writes to the ODBC registry section.
    unsafe fn SQLWritePrivateProfileStringW(
        lpszSection: *const u16,
        lpszEntry: *const u16,
        lpszString: *const u16,
        lpszFilename: *const u16,
    ) -> i32;
}

#[cfg(windows)]
const ODBC_ADD_DSN: u16 = 1;
#[cfg(windows)]
const ODBC_CONFIG_DSN: u16 = 2;
#[cfg(windows)]
const ODBC_REMOVE_DSN: u16 = 3;

/// Encodes a Rust string as a null-terminated UTF-16 vector.
#[cfg(windows)]
fn to_wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Headless implementation of the ODBC setup library's `ConfigDSNW` entry point.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/configdsn-function>
///
/// Handles `ODBC_ADD_DSN`, `ODBC_CONFIG_DSN`, and `ODBC_REMOVE_DSN` by writing
/// or removing entries in the Windows ODBC registry via `odbccp32.dll`. No UI
/// dialog is shown (`hwnd_parent` is ignored).
///
/// # Parameters
///
/// - `_hwnd_parent`: Parent window handle. This implementation never displays a UI, so
///   the value is ignored entirely (headless mode only).
/// - `f_request`: Type of request. Must be one of:
///   - `ODBC_ADD_DSN` (1): Add a new data source.
///   - `ODBC_CONFIG_DSN` (2): Configure (modify) an existing data source.
///   - `ODBC_REMOVE_DSN` (3): Remove an existing data source.
///   Any other value returns FALSE (0).
/// - `lpsz_driver`: Driver description string (e.g. the driver's registered name in the
///   Windows registry). Must be non-null; returns FALSE if null.
/// - `lpsz_attributes`: Double-null-terminated list of `Key=Value` pairs encoded as UTF-16.
///   The `DSN` key is required; its value names the data source. May be null (treated as
///   empty, which causes FALSE to be returned because no DSN key is present).
///
/// # Returns
///
/// Returns 1 (TRUE) on success, 0 (FALSE) on failure. On failure, error details are
/// available via `SQLInstallerError` / `SQLPostInstallerError` in `odbccp32.dll`; however
/// this implementation delegates error reporting to the underlying `odbccp32` calls and
/// does not call `SQLPostInstallerError` directly.
///
/// # Spec compliance
///
/// The ConfigDSN spec does not define ODBC SQLSTATEs. Errors are reported through the
/// ODBC installer error mechanism (`SQLInstallerError`). The relevant installer error
/// codes and how this implementation handles them:
///
/// - **ODBC_ERROR_INVALID_HWND** — `hwndParent` was invalid. Not checked; `hwnd_parent`
///   is ignored entirely (headless implementation).
/// - **ODBC_ERROR_INVALID_KEYWORD_VALUE** — `lpszAttributes` contained a syntax error.
///   This implementation returns FALSE (0) early if the `DSN` key is absent; individual
///   attribute syntax errors are not explicitly validated beyond key=value parsing.
/// - **ODBC_ERROR_INVALID_NAME** — `lpszDriver` was invalid or not found in the registry.
///   Checked implicitly by `SQLWriteDSNToIniW`; a zero return causes this function to
///   return FALSE.
/// - **ODBC_ERROR_INVALID_REQUEST_TYPE** — `fRequest` was not a valid request code.
///   Implemented: the `_` match arm returns FALSE (0).
/// - **ODBC_ERROR_REQUEST_FAILED** — The requested operation could not be performed.
///   Surfaced through the return values of `SQLWriteDSNToIniW`,
///   `SQLWritePrivateProfileStringW`, and `SQLRemoveDSNFromIniW`.
/// - **ODBC_ERROR_DRIVER_SPECIFIC** — Driver-specific error. Not explicitly raised by
///   this implementation.
///
/// # Safety
/// - `lpsz_driver` must be null or a valid null-terminated UTF-16 string.
/// - `lpsz_attributes` must be null or a valid double-null-terminated UTF-16 attribute string.
#[cfg(windows)]
pub unsafe fn config_dsn_w(
    _hwnd_parent: *mut std::ffi::c_void,
    f_request: u16,
    lpsz_driver: *const u16,
    lpsz_attributes: *const u16,
) -> i32 {
    tracing::trace!(
        "ConfigDSNW(request={}, driver={:?})",
        f_request,
        lpsz_driver
    );
    if lpsz_driver.is_null() {
        return 0;
    }

    // SAFETY: lpsz_attributes is null or a valid double-null-terminated UTF-16
    // attribute string as guaranteed by the function's safety contract.
    let attrs = unsafe { parse_attributes_w(lpsz_attributes) };
    // Keyword names only. A DSN attribute list routinely carries `PWD=`, and
    // unlike `ConnectParams` this is a plain `Vec` with no redacting `Debug`.
    let attr_keys: Vec<&str> = attrs.iter().map(|(k, _)| k.as_str()).collect();
    tracing::debug!(
        "ConfigDSNW: request={}, attr_keys={:?}",
        f_request,
        attr_keys
    );

    let Some((_, dsn_value)) = attrs.iter().find(|(k, _)| k.eq_ignore_ascii_case("DSN")) else {
        return 0;
    };

    let dsn_w = to_wide_null(dsn_value);

    match f_request {
        ODBC_ADD_DSN | ODBC_CONFIG_DSN => {
            // Register the DSN → driver mapping in ODBC.INI
            // SAFETY: dsn_w and lpsz_driver are null-terminated UTF-16 strings;
            // lpsz_driver was validated non-null above, dsn_w was constructed here.
            if unsafe { SQLWriteDSNToIniW(dsn_w.as_ptr(), lpsz_driver) } == 0 {
                return 0;
            }
            // Write each attribute under the DSN's section, excluding DSN itself
            // (SQLWriteDSNToIni handles the name registration).
            //
            // Note: keys absent from this call that existed in a prior registration
            // are NOT removed, so callers must always supply the full attribute set.
            let odbc_ini_w = to_wide_null("ODBC.INI");
            for (k, v) in &attrs {
                if k.eq_ignore_ascii_case("DSN") {
                    continue;
                }
                let k_w = to_wide_null(k);
                let v_w = to_wide_null(v);
                // SAFETY: all four pointers are null-terminated UTF-16 strings
                // allocated in this scope; they remain valid for the duration of
                // the call.
                unsafe {
                    SQLWritePrivateProfileStringW(
                        dsn_w.as_ptr(),
                        k_w.as_ptr(),
                        v_w.as_ptr(),
                        odbc_ini_w.as_ptr(),
                    );
                }
            }
            1
        }
        // SAFETY: dsn_w is a null-terminated UTF-16 string constructed above.
        ODBC_REMOVE_DSN => unsafe { SQLRemoveDSNFromIniW(dsn_w.as_ptr()) },
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_input() {
        // The byte literals below use interior null bytes intentionally: they are
        // multi-segment ODBC attribute strings, not simple null-terminated C strings.
        #[allow(clippy::manual_c_str_literals)]
        let s: &'static str = "DSN=MyDSN\0Host=example.com\0Port=8443\0\0";
        let utf16_vec: Vec<u16> = s.encode_utf16().collect();

        let attrs = unsafe { parse_attributes_w(utf16_vec.as_ptr()) };
        assert_eq!(attrs.get("DSN").map(String::as_str), Some("MyDSN"));
        assert_eq!(attrs.get("Host").map(String::as_str), Some("example.com"));
        assert_eq!(attrs.get("Port").map(String::as_str), Some("8443"));
        assert_eq!(attrs.len(), 3);
    }

    #[test]
    fn null_pointer_returns_empty_map() {
        let attrs = unsafe { parse_attributes_w(std::ptr::null()) };
        assert!(attrs.is_empty());
    }

    #[test]
    #[allow(clippy::manual_c_str_literals)]
    fn segment_without_equals_is_skipped() {
        let s: &'static str = "BadToken\0DSN=MyDSN\0\0";
        let utf16_vec: Vec<u16> = s.encode_utf16().collect();
        let attrs = unsafe { parse_attributes_w(utf16_vec.as_ptr()) };
        assert_eq!(attrs.len(), 1);
        assert_eq!(attrs.get("DSN").map(String::as_str), Some("MyDSN"));
    }

    #[test]
    #[allow(clippy::manual_c_str_literals)]
    fn empty_attribute_list() {
        // Double-null immediately = no attributes
        let s: &'static str = "\0";
        let utf16_vec: Vec<u16> = s.encode_utf16().collect();
        let attrs = unsafe { parse_attributes_w(utf16_vec.as_ptr()) };
        assert!(attrs.is_empty());
    }

    #[test]
    #[allow(clippy::manual_c_str_literals)]
    fn keys_preserve_original_case() {
        let s: &'static str = "DSN=MyDSN\0Host=example.com\0\0";
        let utf16_vec: Vec<u16> = s.encode_utf16().collect();
        let attrs = unsafe { parse_attributes_w(utf16_vec.as_ptr()) };
        // Keys are stored in original case
        assert!(attrs.contains_key("DSN"));
        assert!(attrs.contains_key("Host"));
        // HashMap lookup is case-sensitive, so lower-case keys must NOT match
        assert!(!attrs.contains_key("dsn"));
        assert!(!attrs.contains_key("host"));
    }
}
