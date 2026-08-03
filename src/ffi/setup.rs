//! `ConfigDSNW`, the ODBC installer entry point for DSN configuration.

use std::collections::HashMap;

/// Longest single DSN attribute segment scanned before giving up, in UTF-16
/// code units. Serves the same purpose as `utf16.rs`'s `MAX_NTS_SCAN` — without
/// a bound, an attribute list missing its terminator walks memory until it
/// faults — over a different subject, so the two values are set separately.
///
/// `MAX_NTS_SCAN` had to be raised past `i16::MAX` because it governs SQL text,
/// which is routinely longer than that. This one governs one `Keyword=Value`
/// segment of a `ConfigDSN` attribute list, whose parts the spec sizes at
/// `SQL_MAX_DSN_LENGTH` (32) and `SQL_MAX_OPTION_STRING_LENGTH` (256).
/// `i16::MAX` is already three orders of magnitude above that, so nothing here
/// argues for moving it, and a shared constant would tie a DSN keyword's length
/// to a statement's.
const MAX_ATTRIBUTE_SCAN: usize = i16::MAX as usize;

/// What went wrong while reading an attribute list.
///
/// Both variants are the spec's `ODBC_ERROR_INVALID_KEYWORD_VALUE` ("The
/// *lpszAttributes* argument contained a syntax error") but they are kept
/// apart because they say different things to whoever reads the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) enum AttributeSyntaxError {
    /// A segment carried no `=`, so it is not a keyword-value pair. The spec
    /// defines the argument as "a doubly null-terminated list of attributes in
    /// the form of keyword-value pairs".
    SegmentWithoutEquals,
    /// A segment ran past [`MAX_ATTRIBUTE_SCAN`] with no null in it, so the list
    /// has no terminator and the walk was abandoned. Whatever was read before
    /// that point is arbitrary.
    Unterminated,
}

/// What [`parse_attributes_w`] found, and what it could not read.
///
/// The parser is deliberately lenient and reports rather than refuses: a
/// partial map is more useful to a caller than none, and the *policy* (that
/// `ConfigDSN` treats any syntax error as a failure) belongs at the call site
/// rather than in the parser. Before this carried a report, both malformed
/// shapes were indistinguishable from a well-formed list at the call site,
/// which is how a data source came to be written with keywords silently
/// missing.
#[derive(Debug, Default)]
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) struct AttributeList {
    /// Every `Key=Value` pair the walk read, keys in their original case.
    pub(crate) attributes: HashMap<String, String>,
    /// The **first** problem encountered, if any. First rather than all,
    /// because one is enough to reject the list and the rest are noise.
    pub(crate) syntax_error: Option<AttributeSyntaxError>,
}

/// Parses a null-separated, double-null terminated ODBC UTF-16 attribute string.
///
/// Each segment is a `Key=Value` pair encoded as UTF-16LE, separated by a single
/// null `u16`, with the list terminated by a double null.
///
/// # Returns
///
/// An [`AttributeList`]: every pair the walk read, plus the first syntax error
/// it hit. The walk is lenient by design (it skips what it cannot read and
/// keeps going), so the report is the only way a caller can tell a partial map
/// from a complete one.
///
/// # Safety
/// `ptr` must be either null or point to a valid double-null-terminated `u16` sequence.
// Only `config_dsn_w` (Windows-only) calls this outside of tests, so the lint is
// suppressed on non-Windows targets only, on Windows a genuinely unused parser
// still warns.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) unsafe fn parse_attributes_w(ptr: *const u16) -> AttributeList {
    let mut parsed = AttributeList::default();
    if ptr.is_null() {
        return parsed;
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
            parsed
                .syntax_error
                .get_or_insert(AttributeSyntaxError::Unterminated);
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
            parsed.attributes.insert(
                segment[..eq_pos].to_string(),
                segment[eq_pos + 1..].to_string(),
            );
        } else {
            // Skipped, but no longer silently. AGENTS.md requires a `warn!` for
            // a silent accept, and the caller needs to know the list it handed
            // over was not entirely readable: a partial map otherwise looks
            // exactly like a complete one. The segment's text is not logged, as
            // an attribute list routinely carries `PWD=` and a segment without
            // `=` could be a password whose separator was mangled.
            tracing::warn!(
                "ConfigDSNW: attribute segment {} carries no '=' and is not a \
                 keyword-value pair; skipping it",
                parsed.attributes.len() + 1
            );
            parsed
                .syntax_error
                .get_or_insert(AttributeSyntaxError::SegmentWithoutEquals);
        }
        pos += 1; // skip the null u16
    }
    parsed
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
            // name only. A DSN attribute list routinely carries `PWD=`, and
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

// `SQLConfigDataSource` request flags, from `odbcinst.h`. Not `#[cfg(windows)]`:
// `config_request_from_raw` below is the boundary conversion, and keeping it
// outside the gate is what lets a Linux test reach it at all.
//
// Only the first three are `ConfigDSN`'s. The rest are listed because they are
// the values a Driver Manager can plausibly forward here and `ConfigDSN` must
// reject — its diagnostics table names the condition exactly: "The *fRequest*
// argument was not one of the following: ODBC_ADD_DSN ODBC_CONFIG_DSN
// ODBC_REMOVE_DSN."
#[cfg_attr(not(windows), allow(dead_code))]
const ODBC_ADD_DSN: u16 = 1;
#[cfg_attr(not(windows), allow(dead_code))]
const ODBC_CONFIG_DSN: u16 = 2;
#[cfg_attr(not(windows), allow(dead_code))]
const ODBC_REMOVE_DSN: u16 = 3;
/// `ODBC_ADD_SYS_DSN` (4) — `SQLConfigDataSource`'s system-DSN request, which
/// `ConfigDSN` does not accept. Named only so the guard test can reject it by
/// name rather than by literal.
#[allow(dead_code)]
const ODBC_ADD_SYS_DSN: u16 = 4;
/// `ODBC_CONFIG_SYS_DSN` (5) — likewise.
#[allow(dead_code)]
const ODBC_CONFIG_SYS_DSN: u16 = 5;
/// `ODBC_REMOVE_SYS_DSN` (6) — likewise.
#[allow(dead_code)]
const ODBC_REMOVE_SYS_DSN: u16 = 6;
/// `ODBC_REMOVE_DEFAULT_DSN` (7) — likewise.
#[allow(dead_code)]
const ODBC_REMOVE_DEFAULT_DSN: u16 = 7;

/// `ConfigDSN`'s *fRequest* argument, as one of the three values its spec
/// defines.
///
/// A typed enum because the crate converts raw ABI integers at the boundary
/// rather than passing them through, and because the alternative — a `_` arm at
/// the bottom of a `match f_request` — is what put the request check *after*
/// the attribute checks, so an unrecognised request was diagnosed as a bad
/// attribute list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) enum ConfigRequest {
    /// `ODBC_ADD_DSN` (1) — add a new data source.
    Add,
    /// `ODBC_CONFIG_DSN` (2) — configure (modify) an existing data source.
    Config,
    /// `ODBC_REMOVE_DSN` (3) — remove an existing data source.
    Remove,
}

/// Recognise a *fRequest* value, following the crate's `*_from_raw` idiom.
///
/// `None` for anything else, including `odbcinst.h`'s `ODBC_ADD_SYS_DSN` and
/// its neighbours: those belong to `SQLConfigDataSource`, and `ConfigDSN`'s own
/// table lists three values and no others.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn config_request_from_raw(raw: u16) -> Option<ConfigRequest> {
    match raw {
        ODBC_ADD_DSN => Some(ConfigRequest::Add),
        ODBC_CONFIG_DSN => Some(ConfigRequest::Config),
        ODBC_REMOVE_DSN => Some(ConfigRequest::Remove),
        _ => None,
    }
}

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

/// Log and hand back an `odbccp32` BOOL.
///
/// The single place an installer call's result becomes something
/// [`config_dsn_body`] acts on. Two things follow from that being one place
/// rather than five:
///
/// - a discarded result is a compile-visible mistake rather than a silent one,
///   and the source audit in this file's tests can enumerate the call sites;
/// - the failure is logged under the name of the call that produced it, which
///   the bare `BOOL` this function's callers return cannot carry.
///
/// It deliberately does **not** post an installer error. `SQLWriteDSNToIniW`,
/// `SQLWritePrivateProfileStringW` and `SQLRemoveDSNFromIniW` each post their
/// own before returning zero, and overwriting it with
/// `ODBC_ERROR_REQUEST_FAILED` would replace a specific cause with a generic
/// one.
#[cfg(windows)]
fn installer_result(call: &str, ret: i32) -> i32 {
    if ret == 0 {
        tracing::error!("ConfigDSNW: {call} failed; odbccp32 has posted its own installer error");
    }
    ret
}

/// The key every registered data source has, and therefore the one to read when
/// asking whether it is registered at all.
///
/// `SQLWriteDSNToIni` "creates a specification section for the data source and
/// adds a single keyword (**Driver**) with the name of the driver DLL as its
/// value", so a section without it is not a data source this or any other
/// installer created.
#[cfg(windows)]
const DSN_DRIVER_KEY: &str = "Driver";

/// The ODBC system information file `ConfigDSN` reads and writes.
#[cfg(windows)]
const ODBC_INI: &str = "ODBC.INI";

/// Whether `dsn_w` names a data source already in ODBC.INI.
///
/// The spec's modify precondition: "**ConfigDSN** checks that the data source
/// name is in the Odbc.ini file (or registry)."
///
/// A four-unit buffer is enough. The question is whether *anything* was read,
/// not what: a longer `Driver` value is truncated to three units plus a
/// terminator and still answers a positive count.
#[cfg(windows)]
fn dsn_is_registered(dsn_w: &[u16]) -> bool {
    let entry_w = to_wide_null(DSN_DRIVER_KEY);
    let default_w = to_wide_null("");
    let odbc_ini_w = to_wide_null(ODBC_INI);
    let mut buf = [0u16; 4];
    // SAFETY: `dsn_w` is null-terminated by its constructor and the other three
    // pointers are null-terminated UTF-16 buffers allocated in this scope.
    // `buf` is writable for exactly the length passed alongside it.
    let read = unsafe {
        crate::odbcinst::SQLGetPrivateProfileStringW(
            dsn_w.as_ptr(),
            entry_w.as_ptr(),
            default_w.as_ptr(),
            buf.as_mut_ptr(),
            i32::try_from(buf.len()).unwrap_or(i32::MAX),
            odbc_ini_w.as_ptr(),
        )
    };
    read > 0
}

/// Write every non-reserved attribute under the DSN's own section.
///
/// Shared by `ODBC_ADD_DSN` and `ODBC_CONFIG_DSN` so that the reserved-key
/// filter and the propagated failure cannot apply to one request and not the
/// other.
///
/// Returns `ConfigDSN`'s TRUE/FALSE. A zero here means `odbccp32` has already
/// posted its own error; see [`installer_result`].
#[cfg(windows)]
fn write_dsn_attributes(dsn_w: &[u16], attrs: &std::collections::HashMap<String, String>) -> i32 {
    let odbc_ini_w = to_wide_null(ODBC_INI);
    for (k, v) in dsn_section_attributes(attrs) {
        let k_w = to_wide_null(k);
        let v_w = to_wide_null(v);
        // SAFETY: all four pointers are null-terminated UTF-16 strings alive for
        // the duration of the call.
        if installer_result("SQLWritePrivateProfileStringW", unsafe {
            SQLWritePrivateProfileStringW(
                dsn_w.as_ptr(),
                k_w.as_ptr(),
                v_w.as_ptr(),
                odbc_ini_w.as_ptr(),
            )
        }) == 0
        {
            // A data source whose name registered but whose attributes did not
            // is not configured, and reporting TRUE for it hands the caller a
            // DSN that cannot connect.
            return 0;
        }
    }
    1
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
/// - `hwnd_parent`: Parent window handle. This implementation never displays a
///   UI, so the value is only ever compared against null. A **non-null** value
///   is logged at `WARN`, because the spec attaches behaviour to it that is not
///   provided: "If it matches an existing name and *hwndParent* is not null,
///   **ConfigDSN** prompts the user to overwrite the existing name." A null
///   value is fully conforming: "The function will not display any dialog
///   boxes if the handle is null."
/// - `f_request`: Type of request. Must be one of:
///   - `ODBC_ADD_DSN` (1): Add a new data source. Registers the name with
///     `SQLWriteDSNToIni`, which "removes the old section before creating the
///     new one" — so re-adding an existing data source drops any keyword this
///     call leaves out.
///   - `ODBC_CONFIG_DSN` (2): Configure (modify) an existing data source.
///     Writes only through `SQLWritePrivateProfileString`, so keywords absent
///     from the call are left standing. Fails if the data source is not already
///     registered.
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
/// - **ODBC_ERROR_INVALID_HWND** — not returned. `hwnd_parent` is never
///   dereferenced: this implementation is headless and shows no dialog, so
///   there is no window handle for it to find invalid. A non-null value is
///   logged rather than rejected, because the spec's own fallback for a driver
///   that cannot prompt is the null-handle behaviour, which is what happens.
/// - **ODBC_ERROR_INVALID_KEYWORD_VALUE** — **posted** when the attribute list
///   carries no `DSN` keyword, and when `SQLValidDSN` rejects the name the `DSN`
///   keyword carries. The spec asks for the latter check by name: "ConfigDSN
///   should call **SQLValidDSN** to check the length of the data source name and
///   to verify that no invalid characters are included in the name."
///
///   Also **posted** when the attribute list itself is malformed: a segment
///   with no `=`, or a list with no double-null terminator. The spec's wording
///   is exactly that ("The *lpszAttributes* argument contained a syntax
///   error") and the argument is "a doubly null-terminated list of attributes
///   in the form of keyword-value pairs". Reported rather than skipped, because
///   a skipped segment writes a data source with keywords silently missing,
///   which surfaces later as a connection failure with nothing pointing back at
///   the attribute list.
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
/// - **ODBC_ERROR_INVALID_REQUEST_TYPE** — **posted** for an `fRequest` outside
///   the three defined values, and posted *first*: it is checked before the
///   driver name, the attribute list and `SQLValidDSN`, so an unrecognised
///   request is diagnosed as one rather than as whatever else the call also got
///   wrong. `odbcinst.h`'s `ODBC_ADD_SYS_DSN` (4) and its neighbours are
///   `SQLConfigDataSource`'s and land here.
/// - **ODBC_ERROR_REQUEST_FAILED** — **posted** when a panic is caught (see
///   below), and when `ODBC_CONFIG_DSN` names a data source that is not in
///   ODBC.INI. The spec makes that check `ConfigDSN`'s: "**ConfigDSN** checks
///   that the data source name is in the Odbc.ini file (or registry)", and of
///   the codes this function's table offers, "Could not perform the operation
///   requested by the *fRequest* argument" is the one that describes it —
///   `ODBC_ERROR_INVALID_KEYWORD_VALUE` is for a syntax error and
///   `odbcinst.h`'s `ODBC_ERROR_INVALID_DSN` is absent from this table.
///   A failure *inside* `odbccp32` is left to it: `SQLWriteDSNToIniW`,
///   `SQLWritePrivateProfileStringW` and `SQLRemoveDSNFromIniW` each post their
///   own error before returning zero, and overwriting it here would replace a
///   specific cause with a generic one. Their zero is **propagated**, not
///   discarded — a posted error alongside a TRUE return is a diagnostic no
///   caller has any reason to read.
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
    hwnd_parent: *mut std::ffi::c_void,
    f_request: u16,
    lpsz_driver: *const u16,
    lpsz_attributes: *const u16,
) -> i32 {
    tracing::trace!(
        "ConfigDSNW(request={}, driver={:?}, hwnd_parent={:?})",
        f_request,
        lpsz_driver,
        hwnd_parent
    );
    let ret = crate::panic::panic_safe_unlocked(
        // SAFETY: the caller's contract on `lpsz_driver` and `lpsz_attributes`
        // is passed straight through to the body. `hwnd_parent` is only ever
        // compared against null, never dereferenced.
        || unsafe { config_dsn_body(hwnd_parent, f_request, lpsz_driver, lpsz_attributes) },
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
/// Same contract as [`config_dsn_w`]. `hwnd_parent` adds none: it is compared
/// against null and never dereferenced.
#[cfg(windows)]
unsafe fn config_dsn_body(
    hwnd_parent: *mut std::ffi::c_void,
    f_request: u16,
    lpsz_driver: *const u16,
    lpsz_attributes: *const u16,
) -> i32 {
    // First, before anything else can fail. The spec ties
    // ODBC_ERROR_INVALID_REQUEST_TYPE to this condition alone ("The fRequest
    // argument was not one of the following: ODBC_ADD_DSN ODBC_CONFIG_DSN
    // ODBC_REMOVE_DSN"), and checking it last meant an out-of-range request
    // with a malformed attribute list was reported as a bad attribute list, so
    // the caller never learned the request itself was rejected.
    let Some(request) = config_request_from_raw(f_request) else {
        return fail(
            ODBC_ERROR_INVALID_REQUEST_TYPE,
            "fRequest was not ODBC_ADD_DSN, ODBC_CONFIG_DSN or ODBC_REMOVE_DSN",
        );
    };
    tracing::debug!("ConfigDSNW: request={:?}", request);

    if !hwnd_parent.is_null() {
        // AGENTS.md: an ignored feature gets a `warn!`. The spec attaches real
        // behaviour to this argument ("If it matches an existing name and
        // hwndParent is not null, ConfigDSN prompts the user to overwrite the
        // existing name") and this driver has no setup dialog, so the prompt
        // becomes an unconditional overwrite. Only a caller that passed non-null
        // is affected: "The function will not display any dialog boxes if the
        // handle is null" is exactly what a null caller gets.
        tracing::warn!(
            "ConfigDSNW: hwndParent is non-null but this driver ships no setup \
             dialog, so it proceeds headlessly. An existing data source of the \
             same name is overwritten without prompting."
        );
    }

    if lpsz_driver.is_null() {
        return fail(
            ODBC_ERROR_INVALID_NAME,
            "the lpszDriver argument was a null pointer",
        );
    }

    // SAFETY: lpsz_attributes is null or a valid double-null-terminated UTF-16
    // attribute string as guaranteed by the function's safety contract.
    let parsed = unsafe { parse_attributes_w(lpsz_attributes) };
    // Keyword names only. A DSN attribute list routinely carries `PWD=`, and
    // unlike `ConnectParams` this is a plain `HashMap` with no redacting `Debug`.
    let attr_keys: Vec<&str> = parsed.attributes.keys().map(|k| k.as_str()).collect();
    tracing::debug!(
        "ConfigDSNW: request={:?}, attr_keys={:?}",
        request,
        attr_keys
    );

    // Spec: ODBC_ERROR_INVALID_KEYWORD_VALUE is "The lpszAttributes argument
    // contained a syntax error", and the argument is defined as "a doubly
    // null-terminated list of attributes in the form of keyword-value pairs",
    // so a segment that is not a pair, and a list with no terminator, are both
    // it. Reported here, before the DSN keyword is looked for: a list too
    // malformed to parse would otherwise be diagnosed as "no DSN keyword",
    // which sends the caller looking in the wrong place.
    if let Some(error) = parsed.syntax_error {
        return fail(
            ODBC_ERROR_INVALID_KEYWORD_VALUE,
            match error {
                AttributeSyntaxError::SegmentWithoutEquals => {
                    "the attribute list carries a segment with no '=', so it is not \
                     a list of keyword-value pairs"
                }
                AttributeSyntaxError::Unterminated => {
                    "the attribute list has no double-null terminator, so only part \
                     of it could be read"
                }
            },
        );
    }

    let attrs = parsed.attributes;

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
    // diagnostics table offers. odbcinst.h's ODBC_ERROR_INVALID_DSN belongs to
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

    match request {
        ConfigRequest::Add => {
            // "Adding a Data Source": SQLWriteDSNToIni registers the name and the
            // Driver value, then "ConfigDSN calls SQLWritePrivateProfileString in
            // the installer DLL to add any additional keywords and values used by
            // the driver."
            //
            // Re-adding an existing data source is an overwrite, and both halves
            // of the spec say so: "If the data source name matches an existing
            // data source name and hwndParent is null, ConfigDSN overwrites the
            // existing name", and SQLWriteDSNToIni "removes the old section
            // before creating the new one". So an ADD carrying fewer keywords
            // than a previous ADD *drops* the ones it left out, so callers must
            // supply the full attribute set, or use ODBC_CONFIG_DSN.
            //
            // SAFETY: dsn_w and lpsz_driver are null-terminated UTF-16 strings;
            // lpsz_driver was validated non-null above, dsn_w was constructed here.
            if installer_result("SQLWriteDSNToIniW", unsafe {
                SQLWriteDSNToIniW(dsn_w.as_ptr(), lpsz_driver)
            }) == 0
            {
                return 0;
            }
            write_dsn_attributes(&dsn_w, &attrs)
        }
        ConfigRequest::Config => {
            // "Modifying a Data Source": "ConfigDSN checks that the data source
            // name is in the Odbc.ini file (or registry)", and "If the data
            // source name was not changed, ConfigDSN calls
            // SQLWritePrivateProfileString in the installer DLL to make any
            // other changes."
            //
            // SQLWriteDSNToIni is deliberately not called here. It "removes the
            // old section before creating the new one", so a modify carrying
            // three keywords would delete every other keyword the data source
            // had, which is what this arm did while it shared ODBC_ADD_DSN's
            // body. Keywords absent from a CONFIG are left standing, which is
            // what makes it a modify.
            //
            // The rename branch ("If the data source name was changed,
            // ConfigDSN first calls SQLRemoveDSNFromIni") is unreachable
            // rather than unimplemented: lpszAttributes carries one DSN keyword
            // and no previous name, so there is nothing to compare against and
            // no rename to detect.
            if !dsn_is_registered(&dsn_w) {
                return fail(
                    ODBC_ERROR_REQUEST_FAILED,
                    "ODBC_CONFIG_DSN names a data source that is not in ODBC.INI; \
                     use ODBC_ADD_DSN to create one",
                );
            }
            write_dsn_attributes(&dsn_w, &attrs)
        }
        // SAFETY: dsn_w is a null-terminated UTF-16 string constructed above.
        ConfigRequest::Remove => installer_result("SQLRemoveDSNFromIniW", unsafe {
            SQLRemoveDSNFromIniW(dsn_w.as_ptr())
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The source of one `fn` in this file, from its signature to the closing
    /// brace at column 0.
    ///
    /// `config_dsn_w` is `#[cfg(windows)]` and links `odbccp32`, so **no test on
    /// Linux can execute it** and the Windows job only compiles it. A source
    /// audit is the one guard that runs everywhere; the crate's precedent is
    /// `the_set_of_group_lock_acquisition_sites_is_closed`.
    fn function_source(name: &str) -> &'static str {
        let source = include_str!("setup.rs");
        let start = source
            .find(name)
            .unwrap_or_else(|| panic!("{name} is defined in this file"));
        let body = &source[start..];
        let end = body
            .find("\n}\n")
            .unwrap_or_else(|| panic!("{name} has a closing brace"));
        &body[..end]
    }

    /// Every function that can produce `ConfigDSNW`'s FALSE.
    ///
    /// A `return 0` moved into a helper is a `return 0` the audit stops seeing,
    /// so this list has to grow with the code. Adding a helper that can fail
    /// without adding it here silently narrows every audit below.
    const AUDITED_FUNCTIONS: &[&str] = &["unsafe fn config_dsn_body(", "fn write_dsn_attributes("];

    fn audited_source() -> String {
        AUDITED_FUNCTIONS
            .iter()
            .map(|f| function_source(f))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Every `odbccp32` BOOL that can become `ConfigDSN`'s return value goes
    /// through [`installer_result`].
    ///
    /// A call written without it discards the result, and the failure that
    /// produces is not visible from the outside: the DSN name registers, an
    /// attribute write fails, `odbccp32` posts its own error, and `ConfigDSN`
    /// answers TRUE. The spec ties that buffer to the other answer — "When
    /// **ConfigDSN** returns FALSE, an associated *\*pfErrorCode* value is
    /// posted" — so an Administrator that got TRUE never reads it.
    ///
    /// Counted rather than merely searched for: `installer_result("X", unsafe {
    /// X(..) })` names `X` twice, once as the literal and once as the call, so a
    /// guarded site contributes 2 and an unguarded one contributes 1.
    #[test]
    fn every_odbccp32_bool_result_goes_through_installer_result() {
        const BOOL_RETURNING: &[&str] = &[
            "SQLWriteDSNToIniW",
            "SQLWritePrivateProfileStringW",
            "SQLRemoveDSNFromIniW",
        ];

        let body = audited_source();
        for call in BOOL_RETURNING {
            let mentions = body.matches(call).count();
            let guarded = body
                .matches(&format!("installer_result(\"{call}\""))
                .count();
            assert_eq!(
                mentions,
                2 * guarded,
                "{call} is mentioned {mentions} time(s) in the audited functions \
                 but only {guarded} go through installer_result(...). A call whose \
                 BOOL is discarded turns a failed registry write into a TRUE return."
            );
        }
    }

    /// The `odbccp32` calls whose failure `ConfigDSNW` reports by returning
    /// FALSE without posting an error of its own.
    ///
    /// Each of these posts its own before returning zero, and overwriting it
    /// with `ODBC_ERROR_REQUEST_FAILED` would replace a specific cause with a
    /// generic one. The list is checked in **both** directions against the
    /// `installer_result` call sites, so it can neither miss a new exit nor keep
    /// naming a call that has gone.
    const ODBCCP32_POSTS_ITS_OWN: &[&str] = &[
        // Registering the DSN name and its Driver value.
        "SQLWriteDSNToIniW",
        // Writing one of the data source's own keywords. Returning FALSE for it
        // is the point: the name registered and the attributes did not, so the
        // data source exists and cannot connect.
        "SQLWritePrivateProfileStringW",
        // Removing the data source. A tail expression, and therefore the exit
        // that a whole-line pattern scan could not see at all.
        "SQLRemoveDSNFromIniW",
    ];

    /// `ODBCCP32_POSTS_ITS_OWN` must name **exactly** the calls that reach
    /// [`installer_result`], no more and no fewer.
    ///
    /// The list is the audit's claim about which exits carry an installer error
    /// that this file did not post. Checking it in one direction only is how it
    /// came to name one function while the doc comment named three: the third
    /// exit was a tail expression (`ODBC_REMOVE_DSN => unsafe {
    /// SQLRemoveDSNFromIniW(..) }`), which no whole-line pattern can match, so
    /// it was never counted and the counts agreed anyway.
    #[test]
    fn the_installer_error_list_names_exactly_the_calls_that_post_their_own() {
        let body = audited_source();

        // Every `installer_result("Name"` in the audited functions.
        let mut found: Vec<&str> = Vec::new();
        for (offset, _) in body.match_indices("installer_result(\"") {
            let rest = &body[offset + "installer_result(\"".len()..];
            let end = rest
                .find('"')
                .expect("installer_result's first argument is a string literal");
            found.push(&rest[..end]);
        }
        found.sort_unstable();
        found.dedup();

        let mut declared = ODBCCP32_POSTS_ITS_OWN.to_vec();
        declared.sort_unstable();

        assert_eq!(
            found, declared,
            "ODBCCP32_POSTS_ITS_OWN must name exactly the odbccp32 calls routed \
             through installer_result. Anything found and not declared is an exit \
             the audit is not covering; anything declared and not found is a call \
             that no longer exists."
        );
        assert!(
            !found.is_empty(),
            "no installer_result call sites found at all; the audit is reading \
             the wrong source"
        );
    }

    /// Every FALSE `ConfigDSNW` returns carries a posted installer error.
    ///
    /// The spec makes that buffer the function's only channel: "When
    /// **ConfigDSN** returns FALSE, an associated *\*pfErrorCode* value is posted
    /// to the installer error buffer by a call to **SQLPostInstallerError**." A
    /// bare `0` shows the ODBC Administrator a failure with no cause.
    ///
    /// Checked by reading the source rather than by calling the function, and
    /// that is the point: `config_dsn_w` is `#[cfg(windows)]` and links
    /// `odbccp32`, so **no test on Linux can execute it** and the Windows job
    /// only compiles it. The crate's precedent is
    /// `the_set_of_group_lock_acquisition_sites_is_closed`.
    ///
    /// The rule: a falsey exit either goes through [`fail`], which posts, or is
    /// the propagated result of an [`installer_result`] call, which is where
    /// `odbccp32` already posted. Both halves are enumerable, so neither is
    /// approximated by a pattern.
    #[test]
    fn every_false_return_from_config_dsn_w_posts_an_installer_error() {
        let body = audited_source();

        // A bare falsey exit is legitimate only immediately after an
        // `installer_result(...) == 0` test, so their counts must agree. Written
        // as `return 0;` (a guard) or as a trailing `0` (a helper's early exit).
        let bare = body
            .lines()
            .map(str::trim)
            .filter(|l| !l.starts_with("//"))
            .filter(|l| *l == "return 0;" || *l == "0" || *l == "0,")
            .count();
        let guards = body.matches("}) == 0").count();
        assert_eq!(
            bare, guards,
            "found {bare} bare falsey exit(s) but {guards} installer_result guard(s). \
             A failure path that is neither must call fail(code, why), so the ODBC \
             Administrator can say what went wrong."
        );

        // A tail-expression exit, the shape a line scan cannot see, is covered
        // by the both-directions check in
        // `the_installer_error_list_names_exactly_the_calls_that_post_their_own`.

        // And the posting path itself must still exist. `fail` lives outside the
        // audited functions, so this one needs the whole file.
        let source = include_str!("setup.rs");
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

        let attrs = unsafe { parse_attributes_w(utf16_vec.as_ptr()) }.attributes;
        assert_eq!(attrs.get("DSN").map(String::as_str), Some("MyDSN"));
        assert_eq!(attrs.get("Host").map(String::as_str), Some("example.com"));
        assert_eq!(attrs.get("Port").map(String::as_str), Some("8443"));
        assert_eq!(attrs.len(), 3);
    }

    #[test]
    fn null_pointer_returns_empty_map() {
        let attrs = unsafe { parse_attributes_w(std::ptr::null()) }.attributes;
        assert!(attrs.is_empty());
    }

    /// The parser skips a segment with no `=` rather than refusing the whole
    /// list. That is still deliberate: the report on `AttributeList` is what
    /// carries the problem to the caller, and `ConfigDSN` is what turns it into
    /// a failure. Keeping the two apart means a future caller that wants a
    /// best-effort parse can have one.
    #[test]
    #[allow(clippy::manual_c_str_literals)]
    fn segment_without_equals_is_skipped() {
        let s: &'static str = "BadToken\0DSN=MyDSN\0\0";
        let utf16_vec: Vec<u16> = s.encode_utf16().collect();
        let parsed = unsafe { parse_attributes_w(utf16_vec.as_ptr()) };
        assert_eq!(parsed.attributes.len(), 1);
        assert_eq!(
            parsed.attributes.get("DSN").map(String::as_str),
            Some("MyDSN")
        );
    }

    #[test]
    #[allow(clippy::manual_c_str_literals)]
    fn empty_attribute_list() {
        // Double-null immediately = no attributes
        let s: &'static str = "\0";
        let utf16_vec: Vec<u16> = s.encode_utf16().collect();
        let attrs = unsafe { parse_attributes_w(utf16_vec.as_ptr()) }.attributes;
        assert!(attrs.is_empty());
    }

    /// A non-null `hwndParent` must be logged, not silently ignored.
    ///
    /// The spec hangs real behaviour off it ("If it matches an existing name
    /// and *hwndParent* is not null, **ConfigDSN** prompts the user to overwrite
    /// the existing name") and this driver replaces the prompt with an
    /// unconditional overwrite. AGENTS.md requires a `warn!` for an ignored
    /// feature, and this is the whole of one.
    ///
    /// Source-audited rather than executed: `config_dsn_w` is `#[cfg(windows)]`
    /// and links `odbccp32`, and the crate has no log-capture harness for unit
    /// tests.
    #[test]
    fn a_non_null_parent_window_is_warned_about() {
        let body = function_source("unsafe fn config_dsn_body(");
        let branch_start = body
            .find("if !hwnd_parent.is_null() {")
            .expect("config_dsn_body inspects hwnd_parent");
        // Sliced to the branch's own closing brace, which sits at the
        // function's statement indent of four spaces. Bounding it by "the next
        // `if `" instead would find the word "if" inside the branch's own
        // prose comment and conclude the warning was outside the branch.
        let branch = &body[branch_start..];
        let branch_end = branch
            .find("\n    }")
            .expect("the hwnd_parent branch has a closing brace");
        let branch = &branch[..branch_end];
        assert!(
            branch.contains("tracing::warn!"),
            "the non-null hwnd_parent branch must warn"
        );

        // And the argument must actually reach the body: an ignored
        // `_hwnd_parent` in `config_dsn_w` cannot be inspected at all.
        //
        // Asserted as the *absence* of the underscore rather than the presence
        // of the bare name, because `"hwnd_parent: *mut std::ffi::c_void"` is a
        // substring of `"_hwnd_parent: *mut std::ffi::c_void"` and a `contains`
        // check for it passes either way.
        let entry = function_source("pub unsafe fn config_dsn_w(");
        assert!(
            !entry.contains("_hwnd_parent"),
            "config_dsn_w must name the argument rather than discard it as _hwnd_parent"
        );
        assert!(
            entry.contains("hwnd_parent: *mut std::ffi::c_void"),
            "config_dsn_w must still declare the argument"
        );
    }

    /// A segment with no `=` is not a keyword-value pair, and the spec defines
    /// `lpszAttributes` as "a doubly null-terminated list of attributes in the
    /// form of keyword-value pairs". The parser still skips it (a partial map
    /// is more useful to a caller than none) but it now says so, and
    /// `ConfigDSN` turns that into `ODBC_ERROR_INVALID_KEYWORD_VALUE`.
    #[test]
    #[allow(clippy::manual_c_str_literals)]
    fn a_segment_without_equals_is_reported_as_a_syntax_error() {
        let s: &'static str = "BadToken\0DSN=MyDSN\0\0";
        let utf16_vec: Vec<u16> = s.encode_utf16().collect();
        let parsed = unsafe { parse_attributes_w(utf16_vec.as_ptr()) };

        assert_eq!(
            parsed.syntax_error,
            Some(AttributeSyntaxError::SegmentWithoutEquals),
            "a segment that is not a keyword-value pair is a syntax error"
        );
        // The parser stays lenient: everything it could read is still there.
        assert_eq!(parsed.attributes.len(), 1);
        assert_eq!(
            parsed.attributes.get("DSN").map(String::as_str),
            Some("MyDSN")
        );
    }

    /// A list that runs past the scan bound has no terminator, so the map is
    /// whatever happened to fit, indistinguishable to the caller from a
    /// complete one. That is the case the report exists for.
    #[test]
    fn an_unterminated_attribute_list_is_reported_as_a_syntax_error() {
        // One segment longer than MAX_ATTRIBUTE_SCAN with no null in it at all.
        let mut units: Vec<u16> = vec![b'A' as u16; MAX_ATTRIBUTE_SCAN + 8];
        // No terminator is written deliberately; the scan bound is what stops
        // the walk, which is the condition under test.
        units.push(0);
        units.push(0);

        let parsed = unsafe { parse_attributes_w(units.as_ptr()) };
        assert_eq!(
            parsed.syntax_error,
            Some(AttributeSyntaxError::Unterminated),
            "abandoning the scan must be reported, not just logged"
        );
        assert!(parsed.attributes.is_empty());
    }

    /// `AttributeList::syntax_error` documents itself as "the **first** problem
    /// encountered", and this is what makes that true rather than incidental.
    /// A bad segment followed by an unterminated one reports the bad segment:
    /// swapping the parser's `get_or_insert` for a `replace` reports the
    /// unterminated one instead and every other test in this file still passes,
    /// so without this the doc comment would be an unchecked claim.
    #[test]
    #[allow(clippy::manual_c_str_literals)]
    fn the_first_syntax_error_is_the_one_reported() {
        // "BadToken" with no '=', then a segment with no null at all, so the
        // scan bound is what ends the walk.
        let mut units: Vec<u16> = "BadToken\0".encode_utf16().collect();
        units.extend(std::iter::repeat_n(b'A' as u16, MAX_ATTRIBUTE_SCAN + 8));
        units.push(0);
        units.push(0);

        let parsed = unsafe { parse_attributes_w(units.as_ptr()) };
        assert_eq!(
            parsed.syntax_error,
            Some(AttributeSyntaxError::SegmentWithoutEquals),
            "the bad segment came first, so it is the one reported"
        );
    }

    /// A well-formed list reports nothing, so the flag cannot be a
    /// permanently-set constant that happens to satisfy the two tests above.
    #[test]
    #[allow(clippy::manual_c_str_literals)]
    fn a_well_formed_attribute_list_reports_no_syntax_error() {
        let s: &'static str = "DSN=MyDSN\0Host=example.com\0\0";
        let utf16_vec: Vec<u16> = s.encode_utf16().collect();
        let parsed = unsafe { parse_attributes_w(utf16_vec.as_ptr()) };
        assert_eq!(parsed.syntax_error, None);
        assert_eq!(parsed.attributes.len(), 2);
    }

    /// `ConfigDSN` turns the report into the spec's own code, and does it before
    /// the DSN keyword is looked for: a list too malformed to parse cannot be
    /// diagnosed as "no DSN keyword", which is what it would otherwise be
    /// reported as.
    #[test]
    fn a_syntax_error_is_reported_before_the_missing_dsn_keyword() {
        let body = function_source("unsafe fn config_dsn_body(");
        let syntax_check = body
            .find("syntax_error")
            .expect("config_dsn_body inspects the parser's report");
        let dsn_lookup = body
            .find("k.eq_ignore_ascii_case(\"DSN\")")
            .expect("config_dsn_body looks for the DSN keyword");
        assert!(
            syntax_check < dsn_lookup,
            "a malformed attribute list must be reported as a syntax error, not \
             as a missing DSN keyword"
        );
    }

    /// `ODBC_ADD_SYS_DSN` (4) and its neighbours are real `SQLConfigDataSource`
    /// request flags — `/usr/include/odbcinst.h` lists 4 through 7 — that
    /// `ConfigDSN` does not accept. Each must be unrecognised, so the caller
    /// gets `ODBC_ERROR_INVALID_REQUEST_TYPE` and not a diagnosis of whatever
    /// else was wrong with the call.
    #[test]
    fn only_the_three_config_dsn_requests_are_recognised() {
        assert_eq!(
            config_request_from_raw(ODBC_ADD_DSN),
            Some(ConfigRequest::Add)
        );
        assert_eq!(
            config_request_from_raw(ODBC_CONFIG_DSN),
            Some(ConfigRequest::Config)
        );
        assert_eq!(
            config_request_from_raw(ODBC_REMOVE_DSN),
            Some(ConfigRequest::Remove)
        );

        // Values from odbcinst.h that belong to SQLConfigDataSource, not to
        // ConfigDSN, plus the ends of the range.
        for rejected in [
            0,
            ODBC_ADD_SYS_DSN,
            ODBC_CONFIG_SYS_DSN,
            ODBC_REMOVE_SYS_DSN,
            ODBC_REMOVE_DEFAULT_DSN,
            u16::MAX,
        ] {
            assert_eq!(
                config_request_from_raw(rejected),
                None,
                "fRequest {rejected} is not one of ConfigDSN's three"
            );
        }
    }

    /// The request type is validated before anything else can fail, so that an
    /// out-of-range `fRequest` is diagnosed as one. The spec ties
    /// `ODBC_ERROR_INVALID_REQUEST_TYPE` to exactly that condition; reaching the
    /// attribute checks first reports whatever was wrong with the attribute list
    /// instead, and the caller never learns the request itself was rejected.
    ///
    /// A source-order assertion because `config_dsn_body` is `#[cfg(windows)]`
    /// and links `odbccp32`.
    #[test]
    fn the_request_type_is_validated_before_the_attribute_list() {
        let body = function_source("unsafe fn config_dsn_body(");
        let request_check = body
            .find("config_request_from_raw(")
            .expect("config_dsn_body validates fRequest");
        let attribute_parse = body
            .find("parse_attributes_w(")
            .expect("config_dsn_body parses the attribute list");
        let valid_dsn = body
            .find("SQLValidDSNW(")
            .expect("config_dsn_body calls SQLValidDSN");

        assert!(
            request_check < attribute_parse,
            "fRequest must be checked before the attribute list is parsed"
        );
        assert!(
            request_check < valid_dsn,
            "fRequest must be checked before SQLValidDSN can reject the name"
        );
    }

    /// `ODBC_CONFIG_DSN` must not call `SQLWriteDSNToIni`.
    ///
    /// That function "removes the old section before creating the new one", so
    /// using it to *modify* a data source deletes every key the caller did not
    /// restate. The spec's modify recipe names the other function instead: "If
    /// the data source name was not changed, **ConfigDSN** calls
    /// **SQLWritePrivateProfileString** in the installer DLL to make any other
    /// changes."
    ///
    /// Checked at the source, because `config_dsn_body` is `#[cfg(windows)]` and
    /// links `odbccp32`.
    #[test]
    fn the_config_dsn_arm_never_calls_sql_write_dsn_to_ini() {
        // `config_dsn_body` alone, not `audited_source()`: the arms live here,
        // and slicing a concatenation would let a later-added function's text
        // fall between the markers.
        let body = function_source("unsafe fn config_dsn_body(");
        let config_arm_start = body
            .find("ConfigRequest::Config => {")
            .expect("ODBC_CONFIG_DSN has an arm of its own");
        let add_arm_start = body
            .find("ConfigRequest::Add => {")
            .expect("ODBC_ADD_DSN has an arm of its own");
        assert!(
            add_arm_start < config_arm_start,
            "the arms are expected in ADD, CONFIG order; adjust the slice below"
        );
        let config_arm = &body[config_arm_start..];
        let config_arm_end = config_arm
            .find("ConfigRequest::Remove")
            .expect("ODBC_REMOVE_DSN follows the CONFIG arm");
        let config_arm = &config_arm[..config_arm_end];

        assert!(
            !config_arm.contains("SQLWriteDSNToIniW"),
            "the ODBC_CONFIG_DSN arm calls SQLWriteDSNToIniW, which removes the \
             data source's whole section before recreating it, so a modify \
             carrying three keys deletes everything else the DSN had"
        );
        assert!(
            config_arm.contains("dsn_is_registered"),
            "the spec: 'ConfigDSN checks that the data source name is in the \
             Odbc.ini file (or registry)'. Without the check, a CONFIG of a \
             nonexistent data source silently creates one."
        );
    }

    /// The two arms are separate, and both write their attributes through the
    /// same helper — so the reserved-key filter and the propagated failure
    /// cannot apply to one and not the other.
    #[test]
    fn both_write_arms_go_through_write_dsn_attributes() {
        // `config_dsn_body` alone. `audited_source()` also carries
        // `write_dsn_attributes`'s own signature line, which would make the
        // count 3 and the assertion meaningless.
        let body = function_source("unsafe fn config_dsn_body(");
        assert_eq!(
            body.matches("write_dsn_attributes(").count(),
            2,
            "ODBC_ADD_DSN and ODBC_CONFIG_DSN must each write their attributes \
             through write_dsn_attributes, and nothing else may"
        );
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
        let attrs = unsafe { parse_attributes_w(utf16_vec.as_ptr()) }.attributes;
        // Keys are stored in original case
        assert!(attrs.contains_key("DSN"));
        assert!(attrs.contains_key("Host"));
        // HashMap lookup is case-sensitive, so lower-case keys must NOT match
        assert!(!attrs.contains_key("dsn"));
        assert!(!attrs.contains_key("host"));
    }
}
