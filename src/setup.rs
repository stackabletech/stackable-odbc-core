//! Driver-facing types for the ODBC installer's `ConfigDSN` entry point.
//!
//! Core carries the types and all of `ConfigDSN`'s logic; a driver supplies only
//! the keywords its data source needs, through
//! [`Backend::configure_dsn`](crate::backend::Backend::configure_dsn). The split
//! is the same one [`crate::prompt`] makes for interactive authentication: core
//! decides *whether* and *when*, the driver decides *what*.
//!
//! Nothing here is `#[cfg(windows)]`. The `ConfigDSNW` export and the `odbccp32`
//! calls are, but a driver writes its hook once and core's own tests reach these
//! decisions on any platform, which is also why `dsn_section_attributes` is
//! un-gated in [`crate::ffi::setup`].

// `SQLConfigDataSource` request flags, from `odbcinst.h`. Only the first three
// are `ConfigDSN`'s. The rest are named so the guard test can reject them by
// name rather than by literal, since they are the values a Driver Manager can
// plausibly forward here and `ConfigDSN` must refuse. Its diagnostics table
// names the condition exactly: "The *fRequest* argument was not one of the
// following: ODBC_ADD_DSN ODBC_CONFIG_DSN ODBC_REMOVE_DSN."
const ODBC_ADD_DSN: u16 = 1;
const ODBC_CONFIG_DSN: u16 = 2;
const ODBC_REMOVE_DSN: u16 = 3;
// The four below are named only so the guard test can reject them by name
// rather than by literal, so they have no non-test user by design.
/// `ODBC_ADD_SYS_DSN` (4): `SQLConfigDataSource`'s system-DSN request, which
/// `ConfigDSN` does not accept.
#[allow(dead_code)]
const ODBC_ADD_SYS_DSN: u16 = 4;
/// `ODBC_CONFIG_SYS_DSN` (5): likewise.
#[allow(dead_code)]
const ODBC_CONFIG_SYS_DSN: u16 = 5;
/// `ODBC_REMOVE_SYS_DSN` (6): likewise.
#[allow(dead_code)]
const ODBC_REMOVE_SYS_DSN: u16 = 6;
/// `ODBC_REMOVE_DEFAULT_DSN` (7): likewise.
#[allow(dead_code)]
const ODBC_REMOVE_DEFAULT_DSN: u16 = 7;

/// `ConfigDSN`'s *fRequest* argument, as one of the three values its spec
/// defines.
///
/// A typed enum because the crate converts raw ABI integers at the boundary
/// rather than passing them through. The alternative, a `_` arm at the bottom of
/// a `match f_request`, puts the request check *after* the attribute checks, so
/// an unrecognised request gets diagnosed as a bad attribute list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigRequest {
    /// `ODBC_ADD_DSN` (1): add a new data source.
    Add,
    /// `ODBC_CONFIG_DSN` (2): configure (modify) an existing data source.
    Config,
    /// `ODBC_REMOVE_DSN` (3): remove an existing data source.
    Remove,
}

/// Recognise a *fRequest* value, following the crate's `*_from_raw` idiom.
///
/// `None` for anything else, including `odbcinst.h`'s `ODBC_ADD_SYS_DSN` and its
/// neighbours: those belong to `SQLConfigDataSource`, and `ConfigDSN`'s own
/// table lists three values and no others.
#[must_use]
pub fn config_request_from_raw(raw: u16) -> Option<ConfigRequest> {
    match raw {
        ODBC_ADD_DSN => Some(ConfigRequest::Add),
        ODBC_CONFIG_DSN => Some(ConfigRequest::Config),
        ODBC_REMOVE_DSN => Some(ConfigRequest::Remove),
        _ => None,
    }
}

// `SQLInstallerError` codes, from `odbcinst.h`. Only the ones `ConfigDSN`'s own
// diagnostics table lists are defined here; the header carries seventeen more
// that belong to other installer entry points.
//
// `ODBC_ERROR_DRIVER_SPECIFIC` is absent, and **not** because the value was
// unavailable. `ConfigDSN`'s spec diagnostics table names it, but no header
// defines it, Microsoft's own included. Checked against the Windows
// SDK 10.0.22621.0 `um/odbcinst.h` (from the `Microsoft.Windows.SDK.CPP` NuGet
// package, which needs no Windows host), plus unixODBC's, mingw-w64's and
// ReactOS's psdk mirror. All four agree exactly, and the SDK's list ends:
//
//     #define ODBC_ERROR_OUTPUT_STRING_TRUNCATED  22
//     #define ODBC_ERROR_NOTRANINFO               23
//     #define ODBC_ERROR_MAX                      ODBC_ERROR_NOTRANINFO
//
// So 23 is `ODBC_ERROR_NOTRANINFO` and is also the maximum defined code.
// Writing `DriverSpecific = 23` on the strength of the spec page naming it would
// post "no transaction info" to the ODBC Administrator.
//
// A driver's setup failure therefore posts `ODBC_ERROR_REQUEST_FAILED`, whose
// description covers it exactly: "Could not perform the operation requested by
// the fRequest argument." If a real numeric definition ever surfaces, add the
// variant plus its `code()` arm and a constructor; `InstallerError` is
// `#[non_exhaustive]`, so nothing else changes and no consumer breaks.
const ODBC_ERROR_INVALID_HWND: u32 = 3;
const ODBC_ERROR_INVALID_REQUEST_TYPE: u32 = 5;
const ODBC_ERROR_INVALID_NAME: u32 = 7;
const ODBC_ERROR_INVALID_KEYWORD_VALUE: u32 = 8;
const ODBC_ERROR_REQUEST_FAILED: u32 = 11;

/// An installer error code, as posted by `SQLPostInstallerError`.
///
/// `ConfigDSN` returns a bare `BOOL`, so the installer error buffer is its only
/// channel for saying *why* it failed. The spec's Diagnostics section makes that
/// explicit: "When **ConfigDSN** returns FALSE, an associated *\*pfErrorCode*
/// value is posted to the installer error buffer by a call to
/// **SQLPostInstallerError**."
///
/// `#[non_exhaustive]` so adding a code later is not a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum InstallerError {
    /// `ODBC_ERROR_INVALID_HWND`: the *hwndParent* argument was invalid.
    ///
    /// Core never posts this: it is headless and never dereferences the window
    /// handle. A driver whose setup hook validates the handle before using it
    /// is where one would originate.
    InvalidHwnd,
    /// `ODBC_ERROR_INVALID_REQUEST_TYPE`: *fRequest* was not one of
    /// `ODBC_ADD_DSN`, `ODBC_CONFIG_DSN`, `ODBC_REMOVE_DSN`.
    InvalidRequestType,
    /// `ODBC_ERROR_INVALID_NAME`: the *lpszDriver* argument was invalid.
    InvalidName,
    /// `ODBC_ERROR_INVALID_KEYWORD_VALUE`: *lpszAttributes* contained a syntax
    /// error.
    InvalidKeywordValue,
    /// `ODBC_ERROR_REQUEST_FAILED`: the operation *fRequest* asked for could
    /// not be performed. This is also what a driver's setup hook reports; see
    /// [`SetupError::request_failed`].
    RequestFailed,
}

impl InstallerError {
    /// The numeric code `SQLPostInstallerError` expects.
    #[must_use]
    pub fn code(self) -> u32 {
        match self {
            Self::InvalidHwnd => ODBC_ERROR_INVALID_HWND,
            Self::InvalidRequestType => ODBC_ERROR_INVALID_REQUEST_TYPE,
            Self::InvalidName => ODBC_ERROR_INVALID_NAME,
            Self::InvalidKeywordValue => ODBC_ERROR_INVALID_KEYWORD_VALUE,
            Self::RequestFailed => ODBC_ERROR_REQUEST_FAILED,
        }
    }
}

/// Why a driver's setup hook could not complete.
///
/// Not [`OdbcError`](crate::errors::OdbcError): that type is SQLSTATE-shaped
/// and `ConfigDSN` defines no SQLSTATEs at all. Carrying an installer code
/// instead is what lets a driver's own reason reach the ODBC Administrator
/// rather than being flattened into a generic failure.
///
/// A cancelled dialog is **not** a `SetupError`; it is `Ok(None)` from
/// [`Backend::configure_dsn`](crate::backend::Backend::configure_dsn).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupError {
    /// The code core posts with `SQLPostInstallerError`.
    pub code: InstallerError,
    /// The message core posts alongside it. Must not contain a password or any
    /// other attribute *value*: it is written to the installer error buffer and
    /// displayed by the ODBC Administrator.
    pub message: String,
}

impl SetupError {
    /// A failure carrying a specific installer code.
    #[must_use]
    pub fn new(code: InstallerError, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// A setup hook that could not complete, which is the usual case.
    ///
    /// Posts `ODBC_ERROR_REQUEST_FAILED`: "Could not perform the operation
    /// requested by the *fRequest* argument." See the note on the constants
    /// above for why this rather than `ODBC_ERROR_DRIVER_SPECIFIC`.
    #[must_use]
    pub fn request_failed(message: impl Into<String>) -> Self {
        Self::new(InstallerError::RequestFailed, message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The codes `ConfigDSN`'s own diagnostics table lists, pinned to the values
    /// in `odbcinst.h`.
    ///
    /// Written as literals on purpose: this test *is* the transcription of the
    /// header, so asserting `InstallerError::InvalidName.code() ==
    /// ODBC_ERROR_INVALID_NAME` would compare the constant against itself and
    /// pin nothing.
    #[test]
    fn installer_error_codes_match_odbcinst_h() {
        assert_eq!(InstallerError::InvalidHwnd.code(), 3);
        assert_eq!(InstallerError::InvalidRequestType.code(), 5);
        assert_eq!(InstallerError::InvalidName.code(), 7);
        assert_eq!(InstallerError::InvalidKeywordValue.code(), 8);
        assert_eq!(InstallerError::RequestFailed.code(), 11);
    }

    /// `request_failed` is the constructor a driver reaches for, so it must
    /// carry the code core posts for a setup hook that could not complete.
    #[test]
    fn request_failed_is_the_constructor_for_a_driver_failure() {
        let e = SetupError::request_failed("no display available");
        assert_eq!(e.code, InstallerError::RequestFailed);
        assert_eq!(e.message, "no display available");
    }

    /// Only `ConfigDSN`'s three requests are recognised. `odbcinst.h`'s 4
    /// through 7 belong to `SQLConfigDataSource`, and must be rejected so the
    /// caller gets `ODBC_ERROR_INVALID_REQUEST_TYPE` rather than a diagnosis of
    /// whatever else was wrong with the call.
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
}
