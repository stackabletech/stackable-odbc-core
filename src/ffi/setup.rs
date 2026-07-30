//! `ConfigDSNW`, the ODBC installer entry point for DSN configuration.

use std::collections::HashMap;

/// Longest single DSN attribute segment scanned before giving up, in UTF-16
/// code units. Mirrors `utf16.rs`'s `MAX_NTS_SCAN`: without a bound, an
/// attribute list missing its terminator walks memory until it faults.
const MAX_ATTRIBUTE_SCAN: usize = i16::MAX as usize;

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
        // Walk to the next null u16.
        //
        // Read unaligned: the pointer comes from the Driver Manager and carries
        // no alignment guarantee, and an aligned read of a misaligned address
        // is undefined behaviour — in a debug build the standard library's
        // precondition check turns it into a non-unwinding abort that
        // `panic_safe` cannot catch.
        //
        // Bounded for the same reason `utf16_to_string` bounds its `SQL_NTS`
        // scan: an attribute list missing its terminator would otherwise walk
        // memory until it segfaulted. The bound is per segment, which is far
        // more than any real DSN attribute.
        //
        // SAFETY: ptr is non-null (checked above) and caller guarantees it points to
        // a double-null-terminated UTF-16 sequence, so reading ptr.add(pos) is valid
        // until the double-null terminator is reached.
        while pos - start < MAX_ATTRIBUTE_SCAN
            && unsafe { std::ptr::read_unaligned(ptr.add(pos)) } != 0
        {
            pos += 1;
        }
        if pos - start >= MAX_ATTRIBUTE_SCAN {
            tracing::warn!(
                "ConfigDSNW: attribute segment exceeded {MAX_ATTRIBUTE_SCAN} code units \
                 with no terminator; abandoning the rest of the list"
            );
            break;
        }
        if pos == start {
            // Empty segment signals end of list
            break;
        }
        // SAFETY: ptr is non-null and valid (same invariant as above). The slice
        // [start, pos) lies within the same contiguous allocation and does not cross
        // the double-null terminator, so from_raw_parts is safe here.
        // Read element-wise: `from_raw_parts` would require `ptr` to be
        // u16-aligned, which a Driver Manager pointer does not guarantee.
        let code_units: Vec<u16> = (start..pos)
            .map(|i| unsafe { std::ptr::read_unaligned(ptr.add(i)) })
            .collect();
        let segment = String::from_utf16_lossy(&code_units);
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

/// Attribute keys `ConfigDSN` must never write into a DSN's own section.
///
/// `DSN` names the section rather than a value inside it, and
/// `SQLWriteDSNToIni` has already registered it.
///
/// `DRIVER` is forbidden outright, and the spec says so twice. Comments:
/// "(**ConfigDSN** does not accept the **DRIVER** keyword.)" Modifying a Data
/// Source: "**ConfigDSN** may not delete or change the value of the **Driver**
/// keyword."
///
/// The second one is the one with teeth. `SQLWriteDSNToIni` writes `Driver`
/// from the *lpszDriver* argument the Driver Manager supplied; a `DRIVER=` pair
/// in `lpszAttributes` was written over it a few lines later, so an attribute
/// list could repoint a data source at any DLL it named.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) const RESERVED_DSN_KEYS: &[&str] = &["DSN", "DRIVER"];

/// True when `key` names a value `ConfigDSN` may not write to the DSN's
/// section.
///
/// Case-insensitive, because the registry grammar is and because every other
/// keyword comparison in this file already is. A case-sensitive check leaves
/// `Driver=` open, which is the spelling a connection string actually uses.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn is_reserved_dsn_key(key: &str) -> bool {
    RESERVED_DSN_KEYS
        .iter()
        .any(|reserved| key.eq_ignore_ascii_case(reserved))
}

/// The attributes `ConfigDSN` writes into the DSN's own section: everything
/// [`is_reserved_dsn_key`] rejects, removed and logged, in a deterministic
/// order.
///
/// Sorted because these become a sequence of `SQLWritePrivateProfileString`
/// calls and a `HashMap` iterates arbitrarily: without it, two runs over one
/// attribute list write in two different orders, and a failure part-way leaves
/// two different half-configured data sources.
///
/// Not `#[cfg(windows)]`, deliberately. `config_dsn_w` links `odbccp32`, so no
/// Linux test can execute it; keeping the decision out here is what makes it
/// testable at all.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn dsn_section_attributes(attrs: &HashMap<String, String>) -> Vec<(&str, &str)> {
    let mut kept: Vec<(&str, &str)> = Vec::new();
    for (k, v) in attrs {
        if is_reserved_dsn_key(k) {
            // AGENTS.md: an intentional silent accept gets a `warn!`. Keyword
            // name only -- a DSN attribute list routinely carries `PWD=`, and
            // unlike `ConnectParams` this is a plain `HashMap` with no
            // redacting `Debug`.
            tracing::warn!(
                "ConfigDSNW: ignoring the {k} keyword. ConfigDSN does not accept \
                 DRIVER and may not change the Driver value, and DSN names the \
                 section rather than a value in it"
            );
            continue;
        }
        kept.push((k.as_str(), v.as_str()));
    }
    kept.sort_unstable_by_key(|(a, _)| *a);
    kept
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
    /// Posts an installer error, which the caller reads back with
    /// `SQLInstallerError`. This is `ConfigDSN`'s only way to say *why* it
    /// failed: the function returns a bare `BOOL`.
    unsafe fn SQLPostInstallerErrorW(dwErrorCode: u32, lpszErrMsg: *const u16) -> i16;
    /// Validates a data source name's length and characters (Unicode variant).
    unsafe fn SQLValidDSNW(lpszDSN: *const u16) -> i32;
}

#[cfg(windows)]
const ODBC_ADD_DSN: u16 = 1;
#[cfg(windows)]
const ODBC_CONFIG_DSN: u16 = 2;
#[cfg(windows)]
const ODBC_REMOVE_DSN: u16 = 3;

// `SQLInstallerError` codes, from `odbcinst.h`. Only the ones `ConfigDSN`'s own
// diagnostics table lists are defined here; the header carries eighteen more
// that belong to other installer entry points.
/// `ODBC_ERROR_INVALID_REQUEST_TYPE` (5) — `fRequest` was not one of
/// `ODBC_ADD_DSN`, `ODBC_CONFIG_DSN`, `ODBC_REMOVE_DSN`.
#[cfg(windows)]
const ODBC_ERROR_INVALID_REQUEST_TYPE: u32 = 5;
/// `ODBC_ERROR_INVALID_NAME` (7) — the `lpszDriver` argument was invalid.
#[cfg(windows)]
const ODBC_ERROR_INVALID_NAME: u32 = 7;
/// `ODBC_ERROR_INVALID_KEYWORD_VALUE` (8) — `lpszAttributes` contained a syntax
/// error.
#[cfg(windows)]
const ODBC_ERROR_INVALID_KEYWORD_VALUE: u32 = 8;
/// `ODBC_ERROR_REQUEST_FAILED` (11) — the operation `fRequest` asked for could
/// not be performed.
#[cfg(windows)]
const ODBC_ERROR_REQUEST_FAILED: u32 = 11;

/// Post an installer error and return `ConfigDSN`'s FALSE.
///
/// Every `return 0` in [`config_dsn_w`] goes through here. `ConfigDSN` returns a
/// bare `BOOL`, so a bare `0` tells the ODBC Administrator that something failed
/// and nothing about what — the spec's Diagnostics section exists precisely to
/// close that gap: "When **ConfigDSN** returns FALSE, an associated
/// *\*pfErrorCode* value is posted to the installer error buffer by a call to
/// **SQLPostInstallerError**."
#[cfg(windows)]
fn fail(code: u32, message: &str) -> i32 {
    tracing::error!("ConfigDSNW: {message} (installer error {code})");
    let msg = to_wide_null(message);
    // SAFETY: `msg` is a null-terminated UTF-16 buffer that outlives the call.
    unsafe { SQLPostInstallerErrorW(code, msg.as_ptr()) };
    0
}

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
///
///   Any other value returns FALSE (0).
/// - `lpsz_driver`: Driver description string (e.g. the driver's registered name in the
///   Windows registry). Must be non-null; returns FALSE if null.
/// - `lpsz_attributes`: Double-null-terminated list of `Key=Value` pairs encoded as UTF-16.
///   The `DSN` key is required; its value names the data source. May be null (treated as
///   empty, which causes FALSE to be returned because no DSN key is present).
///
/// # Returns
///
/// Returns 1 (TRUE) on success, 0 (FALSE) on failure.
///
/// **Every FALSE carries a posted installer error**, either this function's or
/// `odbccp32`'s. The spec's Diagnostics section requires it: "When **ConfigDSN**
/// returns FALSE, an associated *\*pfErrorCode* value is posted to the installer
/// error buffer by a call to **SQLPostInstallerError** and can be obtained by
/// calling **SQLInstallerError**." Because the function's own return type is a
/// bare `BOOL`, that buffer is the only channel it has, and a FALSE that leaves
/// it empty shows the user of the ODBC Administrator a failure with no cause.
///
/// # Spec compliance
///
/// The ConfigDSN spec defines no ODBC SQLSTATEs; errors go through the installer
/// mechanism instead. Every code in its diagnostics table:
///
/// - **ODBC_ERROR_INVALID_HWND** — not returned. `hwnd_parent` is ignored
///   entirely: this implementation is headless and shows no dialog, so there is
///   no window handle for it to find invalid.
/// - **ODBC_ERROR_INVALID_KEYWORD_VALUE** — **posted** when the attribute list
///   carries no `DSN` keyword, and when `SQLValidDSN` rejects the name the `DSN`
///   keyword carries. The spec asks for the latter check by name: "ConfigDSN
///   should call **SQLValidDSN** to check the length of the data source name and
///   to verify that no invalid characters are included in the name."
///
///   A `DRIVER=` pair in the attribute list is **not** reported this way. The
///   spec's word is "does not accept" — "(**ConfigDSN** does not accept the
///   **DRIVER** keyword.)" — so it is dropped with a `warn!` rather than made a
///   failure, and the driver the caller actually asked for still reaches the
///   registry through the *lpszDriver* argument. Refusing the call instead
///   would make a data source unconfigurable whenever a setup tool round-trips
///   a connection string carrying that keyword.
/// - **ODBC_ERROR_INVALID_NAME** — **posted** when `lpszDriver` is null. A driver
///   name that is non-null but absent from the registry is `SQLWriteDSNToIniW`'s
///   to detect, and it posts its own error for that.
/// - **ODBC_ERROR_INVALID_REQUEST_TYPE** — **posted** by the `_` match arm, for
///   an `fRequest` outside the three defined values.
/// - **ODBC_ERROR_REQUEST_FAILED** — **posted** when a panic is caught (see
///   below). A failure *inside* `odbccp32` is left to it: `SQLWriteDSNToIniW`,
///   `SQLWritePrivateProfileStringW` and `SQLRemoveDSNFromIniW` each post their
///   own error before returning zero, and overwriting it here would replace a
///   specific cause with a generic one.
/// - **ODBC_ERROR_DRIVER_SPECIFIC** — not returned. Core is database-independent
///   and has no driver-specific failure to report; a driver overriding this entry
///   point is where one would originate.
///
/// # Panic safety
///
/// The body runs inside [`panic_safe_unlocked`], because this is an
/// `extern "system"` boundary and an unwind across it lands in the ODBC
/// Administrator — a C++ process that cannot receive a Rust panic.
///
/// [`crate::panic::panic_safe`], which every `SQL*` entry point uses, is not
/// merely unnecessary here but **inapplicable**: it takes a handle token, locks
/// that handle's group and pushes diagnostics through the resulting scope, and
/// `ConfigDSN` is handed no ODBC handle at all. That makes this the second of
/// the crate's two unlocked entry points, beside `SQLCancel`.
///
/// A caught panic posts `ODBC_ERROR_REQUEST_FAILED` and returns FALSE, so it
/// reaches the user as a failure with a cause rather than as an empty error
/// buffer — the same rule as every other exit here.
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
    let ret = crate::panic::panic_safe_unlocked(
        // SAFETY: the caller's contract on `lpsz_driver` and `lpsz_attributes`
        // is passed straight through to the body.
        || unsafe { config_dsn_body(f_request, lpsz_driver, lpsz_attributes) },
        || {
            fail(
                ODBC_ERROR_REQUEST_FAILED,
                "a panic was caught at the ConfigDSNW boundary; the data source \
                 was not changed",
            )
        },
    );
    tracing::debug!("ConfigDSNW -> {}", ret);
    ret
}

/// The body of [`config_dsn_w`], separated so the panic guard wraps every path
/// including the argument checks.
///
/// # Safety
/// Same contract as [`config_dsn_w`].
#[cfg(windows)]
unsafe fn config_dsn_body(
    f_request: u16,
    lpsz_driver: *const u16,
    lpsz_attributes: *const u16,
) -> i32 {
    if lpsz_driver.is_null() {
        return fail(
            ODBC_ERROR_INVALID_NAME,
            "the lpszDriver argument was a null pointer",
        );
    }

    // SAFETY: lpsz_attributes is null or a valid double-null-terminated UTF-16
    // attribute string as guaranteed by the function's safety contract.
    let attrs = unsafe { parse_attributes_w(lpsz_attributes) };
    // Keyword names only. A DSN attribute list routinely carries `PWD=`, and
    // unlike `ConnectParams` this is a plain `HashMap` with no redacting `Debug`.
    let attr_keys: Vec<&str> = attrs.keys().map(|k| k.as_str()).collect();
    tracing::debug!(
        "ConfigDSNW: request={}, attr_keys={:?}",
        f_request,
        attr_keys
    );

    let Some((_, dsn_value)) = attrs.iter().find(|(k, _)| k.eq_ignore_ascii_case("DSN")) else {
        return fail(
            ODBC_ERROR_INVALID_KEYWORD_VALUE,
            "the attribute list carries no DSN keyword, so there is no data source to name",
        );
    };

    let dsn_w = to_wide_null(dsn_value);

    // Spec: "ConfigDSN should call SQLValidDSN to check the length of the data
    // source name and to verify that no invalid characters are included in the
    // name." Reported as an invalid keyword *value*, since it is the DSN
    // keyword's value that is bad and that is the code ConfigDSN's own
    // diagnostics table offers -- odbcinst.h's ODBC_ERROR_INVALID_DSN belongs to
    // other installer entry points and is absent from this function's table.
    //
    // SAFETY: dsn_w is a null-terminated UTF-16 string constructed above.
    if unsafe { SQLValidDSNW(dsn_w.as_ptr()) } == 0 {
        return fail(
            ODBC_ERROR_INVALID_KEYWORD_VALUE,
            "SQLValidDSN rejected the data source name: too long, or it contains \
             a character the registry grammar forbids",
        );
    }

    match f_request {
        ODBC_ADD_DSN | ODBC_CONFIG_DSN => {
            // Register the DSN → driver mapping in ODBC.INI
            // SAFETY: dsn_w and lpsz_driver are null-terminated UTF-16 strings;
            // lpsz_driver was validated non-null above, dsn_w was constructed here.
            if unsafe { SQLWriteDSNToIniW(dsn_w.as_ptr(), lpsz_driver) } == 0 {
                // odbccp32 has posted its own error for this one, so overwriting
                // it would replace a specific cause with a generic one. Return
                // FALSE and leave the buffer as the installer set it.
                tracing::error!("ConfigDSNW: SQLWriteDSNToIniW failed");
                return 0;
            }
            // Write each attribute under the DSN's section. `dsn_section_attributes`
            // drops the keywords that must not appear there and logs each drop.
            let odbc_ini_w = to_wide_null("ODBC.INI");
            for (k, v) in dsn_section_attributes(&attrs) {
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
        // Likewise: a failure here is odbccp32's, and it has already said why.
        ODBC_REMOVE_DSN => unsafe { SQLRemoveDSNFromIniW(dsn_w.as_ptr()) },
        _ => fail(
            ODBC_ERROR_INVALID_REQUEST_TYPE,
            "fRequest was not ODBC_ADD_DSN, ODBC_CONFIG_DSN or ODBC_REMOVE_DSN",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every FALSE `config_dsn_w` returns must carry a posted installer error,
    /// because the spec makes that buffer the function's only channel: "When
    /// **ConfigDSN** returns FALSE, an associated *\*pfErrorCode* value is posted
    /// to the installer error buffer by a call to **SQLPostInstallerError**." A
    /// bare `0` shows the ODBC Administrator a failure with no cause, which is
    /// what this function did at every one of its own exits before.
    ///
    /// Checked by reading the source rather than by calling the function, and
    /// that is the point: `config_dsn_w` is `#[cfg(windows)]` and links
    /// `odbccp32`, so **no test on Linux can execute it** and the Windows job
    /// only compiles it. A source audit is the one guard that runs everywhere.
    /// The crate's precedent is
    /// `the_set_of_group_lock_acquisition_sites_is_closed`.
    ///
    /// The rule: inside `config_dsn_w`, a falsey return either goes through
    /// [`fail`] or is one of the sites where `odbccp32` has already posted its
    /// own — and those are listed here by the call that precedes them, so
    /// adding one is a deliberate act rather than an omission.
    #[test]
    fn every_false_return_from_config_dsn_w_posts_an_installer_error() {
        const ODBCCP32_POSTS_ITS_OWN: &[&str] = &[
            // SQLWriteDSNToIniW failing: the installer has already posted a
            // specific cause, and overwriting it would generalise it away.
            "SQLWriteDSNToIniW",
        ];

        let source = include_str!("setup.rs");
        // `config_dsn_body` holds every failure path; `config_dsn_w` is the panic
        // guard around it and has no falsey exit of its own.
        let start = source
            .find("unsafe fn config_dsn_body(")
            .expect("config_dsn_body is defined in this file");
        let body = &source[start..];
        let end = body
            .find("\n}\n")
            .expect("config_dsn_body has a closing brace");
        let body = &body[..end];

        // Falsey exits written as a bare literal, rather than through `fail`.
        let bare: Vec<&str> = body
            .lines()
            .map(str::trim)
            .filter(|l| !l.starts_with("//"))
            .filter(|l| *l == "return 0;" || *l == "_ => 0," || *l == "0" || *l == "0,")
            .collect();

        assert_eq!(
            bare.len(),
            ODBCCP32_POSTS_ITS_OWN.len(),
            "config_dsn_w has {} bare falsey return(s) but {} documented as \
             odbccp32's own. A new failure path must call `fail(code, why)` so the \
             ODBC Administrator can say what went wrong; if odbccp32 really did \
             post the error, add its call to ODBCCP32_POSTS_ITS_OWN. Found: {bare:?}",
            bare.len(),
            ODBCCP32_POSTS_ITS_OWN.len(),
        );

        for call in ODBCCP32_POSTS_ITS_OWN {
            assert!(
                body.contains(call),
                "{call} is listed as posting its own installer error but is no \
                 longer called in config_dsn_w"
            );
        }

        // And the posting path itself must still exist.
        assert!(
            source.contains("SQLPostInstallerErrorW(code, msg.as_ptr())"),
            "`fail` no longer posts an installer error"
        );
    }

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

    /// A `DRIVER=` pair in `lpszAttributes` must never be written into the DSN's
    /// section. `SQLWriteDSNToIni` has just written `Driver` from the
    /// *lpszDriver* argument the Driver Manager supplied; writing the
    /// attribute-list value over it repoints the data source at whatever DLL
    /// the caller named, which is the whole trust boundary of a DSN.
    ///
    /// The spec forbids it twice: "(**ConfigDSN** does not accept the **DRIVER**
    /// keyword.)" and "**ConfigDSN** may not delete or change the value of the
    /// **Driver** keyword."
    #[test]
    fn a_driver_attribute_is_never_written_into_the_dsn_section() {
        let mut attrs = HashMap::new();
        attrs.insert("DSN".to_string(), "MyDSN".to_string());
        attrs.insert("DRIVER".to_string(), "evil.dll".to_string());
        attrs.insert("Host".to_string(), "example.com".to_string());

        let written = dsn_section_attributes(&attrs);
        assert_eq!(
            written,
            vec![("Host", "example.com")],
            "only non-reserved keywords may reach SQLWritePrivateProfileString"
        );
    }

    /// The registry grammar is case-insensitive and so is every other keyword
    /// comparison in this file, so `Driver=`, `driver=` and `DrIvEr=` must all
    /// be caught. A case-sensitive check is the same hole with an extra step.
    #[test]
    fn reserved_dsn_keys_are_matched_case_insensitively() {
        for spelling in ["DRIVER", "Driver", "driver", "DrIvEr", "DSN", "dsn", "Dsn"] {
            assert!(
                is_reserved_dsn_key(spelling),
                "{spelling} must be treated as reserved"
            );
        }
        for spelling in ["DRIVERS", "MYDRIVER", "DSNName", "Host", "PWD", "UID"] {
            assert!(
                !is_reserved_dsn_key(spelling),
                "{spelling} is an ordinary keyword and must be written"
            );
        }
    }

    /// Everything that is not reserved survives, in a deterministic order. The
    /// order matters because these become a sequence of registry writes: a
    /// `HashMap` iterates arbitrarily, so without the sort two runs over the
    /// same attribute list write in two different orders, and a failure
    /// half-way leaves two different half-configured data sources.
    #[test]
    fn dsn_section_attributes_keeps_everything_else_in_a_stable_order() {
        let mut attrs = HashMap::new();
        attrs.insert("DSN".to_string(), "MyDSN".to_string());
        attrs.insert("Port".to_string(), "8443".to_string());
        attrs.insert("Host".to_string(), "example.com".to_string());
        attrs.insert("UID".to_string(), "smith".to_string());

        assert_eq!(
            dsn_section_attributes(&attrs),
            vec![("Host", "example.com"), ("Port", "8443"), ("UID", "smith")]
        );
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
