//! `ConfigDSNW`, the ODBC installer entry point for DSN configuration.

use std::collections::HashMap;

// Split by platform on purpose: only `ConfigRequest` has a cross-platform user.
// `InstallerError` and `config_request_from_raw` are reached only from
// `#[cfg(windows)]` code, so an unconditional `use` fails clippy on Linux.
use crate::setup::ConfigRequest;
#[cfg(windows)]
use crate::setup::{InstallerError, config_request_from_raw};

/// Longest single DSN attribute segment scanned before giving up, in UTF-16
/// code units. Serves the same purpose as `utf16.rs`'s `MAX_NTS_SCAN`: without
/// a bound, an attribute list missing its terminator walks memory until it
/// faults. The subject differs, so the two values are set separately.
///
/// `MAX_NTS_SCAN` sits past `i16::MAX` because it governs SQL text, which is
/// routinely longer than that. This one governs one `Keyword=Value`
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
/// The parser is lenient and reports rather than refuses: a partial map is more
/// useful to a caller than none, and the *policy* (that `ConfigDSN` treats any
/// syntax error as a failure) belongs at the call site rather than in the
/// parser. Without the report, both malformed shapes read as a well-formed list
/// at the call site, which is how a data source comes to be written with
/// keywords silently missing.
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
        // is undefined behaviour: in a debug build the standard library's
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
            // Skipped, and never silently. AGENTS.md requires a `warn!` for
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

/// Whether this request reads the data source's existing keywords before the
/// driver's hook runs.
///
/// `Config` and `Remove` both operate on a data source that exists by
/// definition, so both prefill. `Add` must not: `SQLWriteDSNToIni` "removes the
/// old section before creating the new one", so merging in the stored keywords
/// would silently resurrect exactly the ones the caller meant to drop.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn should_prefill(request: ConfigRequest) -> bool {
    matches!(request, ConfigRequest::Config | ConfigRequest::Remove)
}

/// The value of the `DSN` keyword, whatever its case.
///
/// One reader rather than three `find` closures: the keyword is looked for
/// before the hook runs, after it runs, and again to build the section name,
/// and a case-sensitive slip at any one of them is invisible until a client
/// spells it differently from the Administrator.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn dsn_name(attrs: &HashMap<String, String>) -> Option<&str> {
    attrs
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("DSN"))
        .map(|(_, v)| v.as_str())
}

/// Whether the setup hook changed a data source name that was supplied to it,
/// and if so, both spellings for the diagnostic.
///
/// The spec: "if a data source name was passed to it, **ConfigDSN** displays
/// that name but does not allow the user to change it." Core enforces that
/// rather than trusting the hook, because the failure it prevents is
/// destructive: a hook altering `DSN=` on an `ODBC_REMOVE_DSN` deletes a data
/// source the user never named.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn renamed<'a>(
    before: Option<&'a str>,
    after: Option<&'a str>,
) -> Option<(&'a str, &'a str)> {
    let before = before?;
    let after = after.unwrap_or("");
    (!after.eq_ignore_ascii_case(before)).then_some((before, after))
}

/// Split the key list `SQLGetPrivateProfileString` returns for a NULL entry.
///
/// The buffer is a run of null-separated names terminated by a double null, so
/// the first empty segment ends the list. Pure, and separated from the
/// `odbcinst` call for that reason: the call itself cannot be unit-tested
/// without a real `ODBC.INI`, and this is the half where the parsing mistakes
/// live.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn parse_key_list(buf: &[u16]) -> Vec<String> {
    buf.split(|&unit| unit == 0)
        .take_while(|segment| !segment.is_empty())
        .map(String::from_utf16_lossy)
        .collect()
}

/// Merge a data source's stored keywords under the ones the Driver Manager
/// supplied.
///
/// Supplied wins, the same precedence `merge_dsn_params` applies to `DSN=` in a
/// connection string. Two filters on the stored side:
///
/// - reserved keys are dropped, so a stored `Driver` value cannot re-enter the
///   attribute map and be written back over the one *lpszDriver* named;
/// - a stored key that any supplied key matches case-insensitively is dropped,
///   because ODBC keywords are case-insensitive while `HashMap` is not, and
///   keeping both would write one setting twice under two spellings.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn merge_prefill(
    stored: HashMap<String, String>,
    incoming: HashMap<String, String>,
) -> HashMap<String, String> {
    let mut merged: HashMap<String, String> = stored
        .into_iter()
        .filter(|(k, _)| !is_reserved_dsn_key(k))
        .filter(|(k, _)| {
            !incoming
                .keys()
                .any(|supplied| supplied.eq_ignore_ascii_case(k))
        })
        .collect();
    merged.extend(incoming);
    merged
}

/// Longest `ODBC.INI` read attempted, in UTF-16 code units, before the value is
/// accepted truncated. A DSN section far larger than this is not one this driver
/// wrote.
const DSN_SECTION_BUF_MAX: usize = 1 << 16;

/// First read size. Large enough that a normal DSN section needs one call.
const DSN_SECTION_BUF_START: usize = 4096;

/// Read one `ODBC.INI` value, growing the buffer until it is not truncated.
///
/// `entry` is `None` to ask for the section's key list, which is what
/// [`parse_key_list`] parses. Returns `None` if the installer reported an error.
#[cfg_attr(not(windows), allow(dead_code))]
fn profile_string(section: &[u16], entry: Option<&[u16]>) -> Option<Vec<u16>> {
    let default_w = to_wide_null("");
    let odbc_ini_w = to_wide_null(ODBC_INI);
    let mut len = DSN_SECTION_BUF_START;
    loop {
        let mut buf = vec![0u16; len];
        // SAFETY: `section`, `default_w` and `odbc_ini_w` are null-terminated
        // UTF-16 buffers alive for the duration of the call, and `entry` is
        // either null or one too. `buf` is writable for exactly the length
        // passed alongside it.
        let read = unsafe {
            crate::odbcinst::SQLGetPrivateProfileStringW(
                section.as_ptr(),
                entry.map_or(std::ptr::null(), <[u16]>::as_ptr),
                default_w.as_ptr(),
                buf.as_mut_ptr(),
                i32::try_from(buf.len()).unwrap_or(i32::MAX),
                odbc_ini_w.as_ptr(),
            )
        };
        let Ok(read) = usize::try_from(read) else {
            tracing::warn!("ConfigDSNW: SQLGetPrivateProfileStringW reported an error");
            return None;
        };
        // The installer signals truncation by filling the buffer to within its
        // terminator, so anything shorter is a complete answer.
        if read + 2 < len || len >= DSN_SECTION_BUF_MAX {
            buf.truncate(read.min(len));
            return Some(buf);
        }
        len *= 2;
    }
}

/// Every keyword the data source `dsn` already has in `ODBC.INI`.
///
/// Best-effort by design: a read that fails yields an empty map and a `warn!`
/// rather than failing the call. `ODBC_CONFIG_DSN` only writes the keywords it
/// is given and leaves the rest standing, so a dialog that opens without its
/// prefill cannot destroy an existing data source; it just shows blank fields.
/// Failing the whole call instead would turn a transient installer problem into
/// an unconfigurable data source.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn read_dsn_section(dsn: &str) -> HashMap<String, String> {
    let section_w = to_wide_null(dsn);
    let Some(key_buf) = profile_string(&section_w, None) else {
        return HashMap::new();
    };
    let mut section = HashMap::new();
    for key in parse_key_list(&key_buf) {
        let entry_w = to_wide_null(&key);
        if let Some(value) = profile_string(&section_w, Some(&entry_w)) {
            section.insert(key, String::from_utf16_lossy(&value));
        }
    }
    tracing::debug!(
        "ConfigDSNW: prefilled {} keyword(s) from ODBC.INI",
        section.len()
    );
    section
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

/// Post an installer error and return `ConfigDSN`'s FALSE.
///
/// Every `return 0` in [`config_dsn_w`] goes through here. `ConfigDSN` returns a
/// bare `BOOL`, so a bare `0` tells the ODBC Administrator that something failed
/// and nothing about what. The spec's Diagnostics section exists to close that
/// gap: "When **ConfigDSN** returns FALSE, an associated
/// *\*pfErrorCode* value is posted to the installer error buffer by a call to
/// **SQLPostInstallerError**."
#[cfg(windows)]
fn fail(code: InstallerError, message: &str) -> i32 {
    tracing::error!("ConfigDSNW: {message} (installer error {})", code.code());
    let msg = to_wide_null(message);
    // SAFETY: `msg` is a null-terminated UTF-16 buffer that outlives the call.
    unsafe { SQLPostInstallerErrorW(code.code(), msg.as_ptr()) };
    0
}

/// Return `ConfigDSN`'s FALSE for a cancelled setup dialog, posting nothing.
///
/// The one exit in this file that answers FALSE without an installer error, and
/// the reason is that nothing failed: the user was asked and said no. The spec
/// ties that buffer to a *failure* ("When **ConfigDSN** returns FALSE, an
/// associated *\*pfErrorCode* value is posted"), and `ConfigDSN`'s own table
/// lists no cancellation code. `odbcinst.h` does define
/// `ODBC_ERROR_USER_CANCELED` (16), but it belongs to other installer entry
/// points and is absent from this function's table.
///
/// The spec's "Adding a Data Source" section describes this exit without
/// attaching a code to it: "If **ConfigDSN** cannot get complete connection
/// information for a data source, it returns FALSE."
#[cfg(windows)]
fn cancelled() -> i32 {
    tracing::debug!("ConfigDSNW: the driver's setup hook reported a cancellation");
    0
}

/// Encodes a Rust string as a null-terminated UTF-16 vector.
///
/// Not `#[cfg(windows)]`: [`read_dsn_section`] reaches `libodbcinst` on Unix
/// too, and keeping this un-gated is what lets a Linux build compile the same
/// buffers the Windows path uses.
#[cfg_attr(not(windows), allow(dead_code))]
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
///
/// Not `#[cfg(windows)]`, for the reason given on [`to_wide_null`].
#[cfg_attr(not(windows), allow(dead_code))]
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
/// - `hwnd_parent`: Parent window handle. Core never dereferences it; it is
///   forwarded to [`Backend::configure_dsn`](crate::backend::Backend::configure_dsn)
///   so a driver can parent its setup dialog on the ODBC Administrator's window.
///   A driver that has **not** overridden that hook has no dialog, and its
///   default implementation is what logs the non-null case at `WARN`, because
///   the spec attaches behaviour to it that such a driver does not provide: "If
///   it matches an existing name and *hwndParent* is not null, **ConfigDSN**
///   prompts the user to overwrite the existing name." A null value is fully
///   conforming: "The function will not display any dialog boxes if the handle
///   is null."
/// - `f_request`: Type of request. Must be one of:
///   - `ODBC_ADD_DSN` (1): Add a new data source. Registers the name with
///     `SQLWriteDSNToIni`, which "removes the old section before creating the
///     new one", so re-adding an existing data source drops any keyword this
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
/// **One exception, and it is not a failure.** When the driver's setup hook
/// returns `Ok(None)` the user cancelled the dialog, and `ConfigDSN` answers
/// FALSE with the installer error buffer untouched. The sentence above ties that
/// buffer to a failure, and `ConfigDSN`'s own diagnostics table lists no
/// cancellation code: `odbcinst.h` defines `ODBC_ERROR_USER_CANCELED` (16), but
/// it belongs to other installer entry points and is absent from this function's
/// table. The spec describes the exit without attaching a code to it: "If
/// **ConfigDSN** cannot get complete connection information for a data source,
/// it returns FALSE." See `cancelled`.
///
/// # Spec compliance
///
/// The ConfigDSN spec defines no ODBC SQLSTATEs; errors go through the installer
/// mechanism instead. Every code in its diagnostics table:
///
/// - **ODBC_ERROR_INVALID_HWND**: not returned. `hwnd_parent` is never
///   dereferenced: this implementation is headless and shows no dialog, so
///   there is no window handle for it to find invalid. A non-null value is
///   logged rather than rejected, because the spec's own fallback for a driver
///   that cannot prompt is the null-handle behaviour, which is what happens.
/// - **ODBC_ERROR_INVALID_KEYWORD_VALUE**: **posted** when the attribute list
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
///   spec's word is "does not accept": "(**ConfigDSN** does not accept the
///   **DRIVER** keyword.)". So it is dropped with a `warn!` rather than made a
///   failure, and the driver the caller actually asked for still reaches the
///   registry through the *lpszDriver* argument. Refusing the call instead
///   would make a data source unconfigurable whenever a setup tool round-trips
///   a connection string carrying that keyword.
/// - **ODBC_ERROR_INVALID_NAME**: **posted** when `lpszDriver` is null. A driver
///   name that is non-null but absent from the registry is `SQLWriteDSNToIniW`'s
///   to detect, and it posts its own error for that.
/// - **ODBC_ERROR_INVALID_REQUEST_TYPE**: **posted** for an `fRequest` outside
///   the three defined values, and posted *first*: it is checked before the
///   driver name, the attribute list and `SQLValidDSN`, so an unrecognised
///   request is diagnosed as one rather than as whatever else the call also got
///   wrong. `odbcinst.h`'s `ODBC_ADD_SYS_DSN` (4) and its neighbours are
///   `SQLConfigDataSource`'s and land here.
/// - **ODBC_ERROR_REQUEST_FAILED**: **posted** when a panic is caught (see
///   below), and when `ODBC_CONFIG_DSN` names a data source that is not in
///   ODBC.INI. The spec makes that check `ConfigDSN`'s: "**ConfigDSN** checks
///   that the data source name is in the Odbc.ini file (or registry)", and of
///   the codes this function's table offers, "Could not perform the operation
///   requested by the *fRequest* argument" is the one that describes it:
///   `ODBC_ERROR_INVALID_KEYWORD_VALUE` is for a syntax error, and
///   `odbcinst.h`'s `ODBC_ERROR_INVALID_DSN` is absent from this table.
///   A failure *inside* `odbccp32` is left to it: `SQLWriteDSNToIniW`,
///   `SQLWritePrivateProfileStringW` and `SQLRemoveDSNFromIniW` each post their
///   own error before returning zero, and overwriting it here would replace a
///   specific cause with a generic one. Their zero is **propagated**, not
///   discarded, because a posted error alongside a TRUE return is a diagnostic
///   no caller has any reason to read.
///
///   Also **posted** when a driver's
///   [`Backend::configure_dsn`](crate::backend::Backend::configure_dsn) returns
///   an `Err` carrying it, which is what
///   [`SetupError::request_failed`](crate::setup::SetupError::request_failed)
///   constructs and therefore the code a failing setup dialog reports; and when
///   that hook returns a data source name different from the one it was handed,
///   which the spec forbids: "if a data source name was passed to it,
///   **ConfigDSN** displays that name but does not allow the user to change it."
/// - **ODBC_ERROR_DRIVER_SPECIFIC**: not returned, and it cannot be. The spec's
///   table names it, but **no header defines it**, checked against the Windows
///   SDK 10.0.22621.0 `um/odbcinst.h` as well as unixODBC's, mingw-w64's and
///   ReactOS's psdk mirror. All four agree, and the SDK's list ends at
///   `ODBC_ERROR_NOTRANINFO` (23), which is also `ODBC_ERROR_MAX`, so the
///   plausible-looking 23 is a different code entirely. A driver's own setup
///   failure is reported as `ODBC_ERROR_REQUEST_FAILED` instead, whose
///   description covers it. See [`crate::setup::InstallerError`].
///
/// # Panic safety
///
/// The body runs inside [`panic_safe_unlocked`], because this is an
/// `extern "system"` boundary and an unwind across it lands in the ODBC
/// Administrator, a C++ process that cannot receive a Rust panic.
///
/// [`crate::panic::panic_safe`], which every `SQL*` entry point uses, is
/// **inapplicable** here: it takes a handle token, locks that handle's group
/// and pushes diagnostics through the resulting scope, and
/// `ConfigDSN` is handed no ODBC handle at all. That makes this the second of
/// the crate's two unlocked entry points, beside `SQLCancel`.
///
/// A caught panic posts `ODBC_ERROR_REQUEST_FAILED` and returns FALSE, so it
/// reaches the user as a failure with a cause rather than as an empty error
/// buffer, the same rule as every other exit here.
///
/// # Safety
/// - `lpsz_driver` must be null or a valid null-terminated UTF-16 string.
/// - `lpsz_attributes` must be null or a valid double-null-terminated UTF-16 attribute string.
#[cfg(windows)]
pub unsafe fn config_dsn_w<B: crate::backend::Backend>(
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
        || unsafe { config_dsn_body::<B>(hwnd_parent, f_request, lpsz_driver, lpsz_attributes) },
        || {
            fail(
                InstallerError::RequestFailed,
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
unsafe fn config_dsn_body<B: crate::backend::Backend>(
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
            InstallerError::InvalidRequestType,
            "fRequest was not ODBC_ADD_DSN, ODBC_CONFIG_DSN or ODBC_REMOVE_DSN",
        );
    };
    tracing::debug!("ConfigDSNW: request={:?}", request);

    // No `hwnd_parent` warning here. Core forwards the handle to
    // `Backend::configure_dsn` below, so core is not the thing ignoring it; the
    // deviation belongs to that hook's *default* implementation, which is the
    // only place that genuinely has no dialog, and it warns there.

    if lpsz_driver.is_null() {
        return fail(
            InstallerError::InvalidName,
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
            InstallerError::InvalidKeywordValue,
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

    let mut attrs = parsed.attributes;
    // The name as the Driver Manager supplied it, before the hook can touch it.
    // Cloned rather than borrowed because `attrs` is about to be consumed.
    let supplied_name = dsn_name(&attrs).map(str::to_owned);

    // Prefill before the hook, so a driver's dialog opens with the data
    // source's current settings and never has to read ODBC.INI itself. The spec
    // requires it for a modify: "for information not in lpszAttributes, it uses
    // information from the system information." Add is excluded; see
    // `should_prefill`.
    if should_prefill(request)
        && let Some(name) = supplied_name.as_deref()
    {
        attrs = merge_prefill(read_dsn_section(name), attrs);
    }

    // The driver's turn. Deliberately *before* the DSN keyword is required: the
    // Administrator's Add... button supplies an empty attribute list, and the
    // dialog is what produces the data source name.
    let attrs = match B::configure_dsn(hwnd_parent, request, attrs) {
        Ok(Some(attrs)) => attrs,
        Ok(None) => return cancelled(),
        Err(error) => return fail(error.code, &error.message),
    };

    // Spec: "if a data source name was passed to it, ConfigDSN displays that
    // name but does not allow the user to change it." Enforced rather than
    // documented, because the failure it prevents is destructive: a hook
    // altering DSN= on an ODBC_REMOVE_DSN deletes a data source the user never
    // named. With no name supplied (the Add... case) the hook is free.
    if let Some((before, after)) = renamed(supplied_name.as_deref(), dsn_name(&attrs)) {
        return fail(
            InstallerError::RequestFailed,
            &format!(
                "the setup hook changed the data source name from '{before}' to \
                 '{after}'; ConfigDSN displays a supplied name but does not allow \
                 it to be changed"
            ),
        );
    }

    let Some(dsn_value) = dsn_name(&attrs) else {
        return fail(
            InstallerError::InvalidKeywordValue,
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
            InstallerError::InvalidKeywordValue,
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
                    InstallerError::RequestFailed,
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
    const AUDITED_FUNCTIONS: &[&str] = &["unsafe fn config_dsn_body<", "fn write_dsn_attributes("];

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
    /// answers TRUE. The spec ties that buffer to the other answer ("When
    /// **ConfigDSN** returns FALSE, an associated *\*pfErrorCode* value is
    /// posted"), so an Administrator that got TRUE never reads it.
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
    /// that this file did not post. Checking it in one direction only lets the
    /// list name fewer functions than there are exits: one of them is a tail
    /// expression (`ODBC_REMOVE_DSN => unsafe { SQLRemoveDSNFromIniW(..) }`),
    /// which no whole-line pattern can match, so it goes uncounted and the
    /// counts agree anyway.
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

        // `cancelled()` is a FALSE this scan cannot see either, because it returns
        // through a helper, which is exactly the "a `return 0` moved into a
        // helper is a `return 0` the audit stops seeing" case AUDITED_FUNCTIONS
        // warns about. It is deliberately *not* in that list: it is the one
        // enumerated exception to this test's rule, because a user changing
        // their mind is not a failure and ConfigDSN's table has no code for it.
        // `cancelling_is_the_only_unposted_false_return` is what pins it to
        // exactly one occurrence, so a second silent exit cannot be added by
        // copying the first.

        // And the posting path itself must still exist. `fail` lives outside the
        // audited functions, so this one needs the whole file.
        let source = include_str!("setup.rs");
        assert!(
            source.contains("SQLPostInstallerErrorW(code.code(), msg.as_ptr())"),
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
    fn the_parent_window_reaches_the_drivers_setup_hook() {
        // The warning lives on `Backend::configure_dsn`'s default
        // implementation, where
        // `the_default_configure_dsn_warns_only_about_a_non_null_parent_window`
        // in `backend.rs` pins it. Core does not *ignore* the handle, so
        // warning here would be warning about something core does not do. What
        // matters here is that the handle is forwarded rather than dropped,
        // since a driver cannot parent a dialog without it.
        let body = function_source("unsafe fn config_dsn_body<");
        assert!(
            body.contains("configure_dsn(hwnd_parent,"),
            "config_dsn_body must pass hwndParent to the driver's setup hook; \
             without it no driver can parent its dialog on the Administrator's \
             window"
        );

        // And the argument must actually reach the body: an ignored
        // `_hwnd_parent` in `config_dsn_w` cannot be inspected at all.
        //
        // Asserted as the *absence* of the underscore rather than the presence
        // of the bare name, because `"hwnd_parent: *mut std::ffi::c_void"` is a
        // substring of `"_hwnd_parent: *mut std::ffi::c_void"` and a `contains`
        // check for it passes either way.
        let entry = function_source("pub unsafe fn config_dsn_w<");
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
        let body = function_source("unsafe fn config_dsn_body<");
        let syntax_check = body
            .find("syntax_error")
            .expect("config_dsn_body inspects the parser's report");
        // The DSN keyword is read through `dsn_name`, so the marker is the
        // diagnostic rather than the comparison: the first `dsn_name` call is
        // the pre-hook read of the *supplied* name, which is not the lookup
        // this ordering is about.
        let dsn_required = body
            .find("carries no DSN keyword")
            .expect("config_dsn_body rejects an attribute list with no DSN keyword");
        assert!(
            syntax_check < dsn_required,
            "a malformed attribute list must be reported as a syntax error, not \
             as a missing DSN keyword"
        );
    }

    // `only_the_three_config_dsn_requests_are_recognised` moved to
    // `crate::setup`, along with `ConfigRequest` and `config_request_from_raw`
    // themselves.

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
        let body = function_source("unsafe fn config_dsn_body<");
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
        let body = function_source("unsafe fn config_dsn_body<");
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
    /// same helper, so the reserved-key filter and the propagated failure
    /// cannot apply to one and not the other.
    #[test]
    fn both_write_arms_go_through_write_dsn_attributes() {
        // `config_dsn_body` alone. `audited_source()` also carries
        // `write_dsn_attributes`'s own signature line, which would make the
        // count 3 and the assertion meaningless.
        let body = function_source("unsafe fn config_dsn_body<");
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

    /// The hook runs before the `DSN` keyword is looked for.
    ///
    /// This ordering *is* the feature. The Administrator's **Add…** button
    /// calls `ConfigDSNW(hwnd, ODBC_ADD_DSN, "Driver Name", "")` with an empty
    /// attribute list: there is no `DSN` keyword until the driver's dialog has
    /// produced one. Looking for it first is what made **Add…** fail with
    /// `ODBC_ERROR_INVALID_KEYWORD_VALUE` for every driver built on core.
    #[test]
    fn the_setup_hook_runs_before_the_dsn_keyword_is_required() {
        let body = function_source("unsafe fn config_dsn_body<");
        let hook = body
            .find("configure_dsn(")
            .expect("config_dsn_body calls Backend::configure_dsn");
        let dsn_required = body
            .find("carries no DSN keyword")
            .expect("config_dsn_body rejects an attribute list with no DSN keyword");
        let valid_dsn = body
            .find("SQLValidDSNW(")
            .expect("config_dsn_body calls SQLValidDSN");

        assert!(
            hook < dsn_required,
            "Backend::configure_dsn must run before the DSN keyword is required, \
             or the Administrator's Add... button can never supply one"
        );
        assert!(
            hook < valid_dsn,
            "SQLValidDSN must validate the name the hook produced, not the one \
             it replaced"
        );
    }

    /// The hook runs *after* the attribute list has been parsed and its syntax
    /// checked, so a driver is never handed a partially-read map.
    #[test]
    fn the_setup_hook_runs_after_the_attribute_list_is_validated() {
        let body = function_source("unsafe fn config_dsn_body<");
        let syntax_check = body
            .find("syntax_error")
            .expect("config_dsn_body inspects the parser's report");
        let hook = body
            .find("configure_dsn(")
            .expect("config_dsn_body calls Backend::configure_dsn");

        assert!(
            syntax_check < hook,
            "a malformed attribute list must be rejected before a driver's \
             dialog is shown it"
        );
    }

    /// Core checks the hook's returned name against the supplied one.
    ///
    /// The spec: "if a data source name was passed to it, **ConfigDSN**
    /// displays that name but does not allow the user to change it." Trusting
    /// the hook is not enough, because the failure this prevents is
    /// destructive.
    #[test]
    fn the_returned_data_source_name_is_checked_against_the_supplied_one() {
        let body = function_source("unsafe fn config_dsn_body<");
        assert!(
            body.contains("renamed("),
            "config_dsn_body must compare the hook's DSN against the supplied \
             one, or a hook altering DSN= on a Remove deletes a data source the \
             user never named"
        );
    }

    /// Cancelling is the one FALSE that posts no installer error, and it is the
    /// *only* one.
    ///
    /// `every_false_return_from_config_dsn_w_posts_an_installer_error`
    /// enumerates the posting exits; this one pins the single exception by
    /// name, so a second silent exit cannot be added by copying the first.
    #[test]
    fn cancelling_is_the_only_unposted_false_return() {
        let body = audited_source();
        assert_eq!(
            body.matches("cancelled()").count(),
            1,
            "exactly one exit may return FALSE without posting an installer \
             error, and it is the user pressing Cancel. Every other failure must \
             call fail(code, why)."
        );

        let helper = function_source("fn cancelled(");
        assert!(
            !helper.contains("SQLPostInstallerError"),
            "cancelled() must not post: the user changing their mind is not a \
             failure"
        );
    }

    /// `ODBC_ADD_DSN` must never prefill.
    ///
    /// `SQLWriteDSNToIni` "removes the old section before creating the new
    /// one", so merging a data source's stored keywords into an Add would
    /// resurrect exactly the ones the caller meant to drop. `Config` and
    /// `Remove` both operate on a data source that exists by definition, so
    /// both prefill.
    #[test]
    fn only_config_and_remove_prefill_from_the_registry() {
        assert!(!should_prefill(ConfigRequest::Add));
        assert!(should_prefill(ConfigRequest::Config));
        assert!(should_prefill(ConfigRequest::Remove));
    }

    /// `SQLGetPrivateProfileString` with a NULL entry answers with the
    /// section's key names, null-separated and double-null terminated.
    #[test]
    fn a_null_separated_key_list_is_split_at_the_double_null() {
        let buf: Vec<u16> = "Host\0Port\0UID\0\0".encode_utf16().collect();
        assert_eq!(parse_key_list(&buf), vec!["Host", "Port", "UID"]);
    }

    /// An empty section answers with just the terminator, which is zero keys
    /// and not one empty key.
    #[test]
    fn an_empty_key_list_yields_no_keys() {
        let buf: Vec<u16> = "\0\0".encode_utf16().collect();
        assert!(parse_key_list(&buf).is_empty());
        assert!(parse_key_list(&[]).is_empty());
    }

    /// The attributes the Driver Manager supplied win over the ones read from
    /// `ODBC.INI`, the same precedence `merge_dsn_params` applies to `DSN=` in
    /// a connection string.
    #[test]
    fn supplied_attributes_win_over_stored_ones() {
        let mut stored = HashMap::new();
        stored.insert("Host".to_string(), "old.example.com".to_string());
        stored.insert("Port".to_string(), "8443".to_string());

        let mut incoming = HashMap::new();
        incoming.insert("DSN".to_string(), "MyDSN".to_string());
        incoming.insert("Host".to_string(), "new.example.com".to_string());

        let merged = merge_prefill(stored, incoming);

        assert_eq!(
            merged.get("Host").map(String::as_str),
            Some("new.example.com")
        );
        assert_eq!(merged.get("Port").map(String::as_str), Some("8443"));
        assert_eq!(merged.get("DSN").map(String::as_str), Some("MyDSN"));
        assert_eq!(merged.len(), 3);
    }

    /// A stored `Driver` value must never re-enter the attribute map.
    ///
    /// `dsn_section_attributes` already filters it on the way out, but a value
    /// that came back in through the prefill would be a second route to the
    /// same place, and the spec forbids it twice: "(ConfigDSN does not accept
    /// the DRIVER keyword.)" and "ConfigDSN may not delete or change the value
    /// of the Driver keyword."
    #[test]
    fn a_stored_driver_value_is_never_prefilled_back_in() {
        let mut stored = HashMap::new();
        stored.insert("Driver".to_string(), "/opt/lib/some.so".to_string());
        stored.insert("Host".to_string(), "example.com".to_string());

        let mut incoming = HashMap::new();
        incoming.insert("DSN".to_string(), "MyDSN".to_string());

        let merged = merge_prefill(stored, incoming);

        assert!(!merged.contains_key("Driver"));
        assert_eq!(merged.get("Host").map(String::as_str), Some("example.com"));
        assert_eq!(merged.len(), 2);
    }

    /// ODBC keywords are case-insensitive, and the registry returns whatever
    /// case it stored. A stored `host` beside a supplied `Host` must resolve to
    /// one keyword, not two, because otherwise the same setting is written
    /// twice under two spellings and which one wins is undefined.
    #[test]
    fn a_stored_key_is_shadowed_case_insensitively_by_a_supplied_one() {
        let mut stored = HashMap::new();
        stored.insert("host".to_string(), "old.example.com".to_string());

        let mut incoming = HashMap::new();
        incoming.insert("Host".to_string(), "new.example.com".to_string());

        let merged = merge_prefill(stored, incoming);

        assert_eq!(merged.len(), 1, "one keyword, not two spellings of it");
        assert_eq!(
            merged.get("Host").map(String::as_str),
            Some("new.example.com")
        );
    }

    /// The `DSN` keyword is found whatever its case, because a connection
    /// string and the ODBC Administrator do not agree on one spelling.
    #[test]
    fn the_dsn_keyword_is_found_case_insensitively() {
        for spelling in ["DSN", "dsn", "Dsn"] {
            let mut attrs = HashMap::new();
            attrs.insert(spelling.to_string(), "MyDSN".to_string());
            assert_eq!(
                dsn_name(&attrs),
                Some("MyDSN"),
                "{spelling} must be recognised as the DSN keyword"
            );
        }

        let mut without = HashMap::new();
        without.insert("Host".to_string(), "example.com".to_string());
        assert_eq!(dsn_name(&without), None);
    }

    /// The spec: "if a data source name was passed to it, **ConfigDSN**
    /// displays that name but does not allow the user to change it."
    ///
    /// Core enforces that rather than trusting the hook, because the failure it
    /// prevents is destructive: a hook that altered `DSN=` on an
    /// `ODBC_REMOVE_DSN` would delete a data source the user never named.
    #[test]
    fn changing_a_supplied_data_source_name_is_detected() {
        assert_eq!(
            renamed(Some("Sales"), Some("Marketing")),
            Some(("Sales", "Marketing"))
        );

        // Dropping the keyword entirely is a change too: core would otherwise
        // fall through to "no DSN keyword" and blame the attribute list.
        assert_eq!(renamed(Some("Sales"), None), Some(("Sales", "")));

        // Case alone is not a change; the registry grammar is insensitive.
        assert_eq!(renamed(Some("Sales"), Some("sales")), None);
        assert_eq!(renamed(Some("Sales"), Some("Sales")), None);
    }

    /// With no name supplied (the **Add…** case) the hook may return any name,
    /// because there is nothing it could have changed. That is what lets the
    /// Administrator's Add button work at all.
    #[test]
    fn supplying_no_name_leaves_the_hook_free_to_choose() {
        assert_eq!(renamed(None, Some("Anything")), None);
        assert_eq!(renamed(None, None), None);
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
