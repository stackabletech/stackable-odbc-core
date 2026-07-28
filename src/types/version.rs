//! ODBC version-string formatting.
//!
//! `SQL_DRIVER_VER` and `SQL_DBMS_VER` are both defined by the ODBC spec as
//! "a character string of the form ##.##.####, where the first two digits are
//! the major version, the next two digits are the minor version, and the last
//! four digits are the release version". This module is the only place that
//! renders that form, which is what keeps a driver version from drifting out of
//! step with `Cargo.toml` and a server version from being reported malformed.

/// Formats a version triple into the ODBC `##.##.####` form required by
/// `SQL_DRIVER_VER` and `SQL_DBMS_VER`.
///
/// Fields narrower than their spec width are zero-padded. Fields *wider* than
/// their spec width are widened rather than truncated: a server whose major
/// version is a bare three-digit integer (`467`) must not be rendered as `67`,
/// which would name a different server. The spec's field widths are a minimum
/// rendering, and every consumer parses this string by splitting on `.` rather
/// than by fixed offsets.
pub fn format_odbc_version(major: u32, minor: u32, release: u32) -> String {
    format!("{major:02}.{minor:02}.{release:04}")
}

/// Parses a dotted version string into a `(major, minor, release)` triple.
///
/// Accepts one, two or three numeric components. Missing components are zero.
/// Components beyond the third are ignored. Parsing stops at the first
/// character that is neither a digit nor a `.`, so a build suffix such as
/// `"468-SNAPSHOT"` parses as `(468, 0, 0)`.
///
/// Returns `None` when the string does not begin with a number, which is the
/// caller's signal that the version is unavailable.
///
/// ## Overflow handling (deliberately inconsistent, see below)
///
/// A numeric component too large to fit `u32` is handled differently
/// depending on which field it lands in:
///
/// - **Major**: `.parse().ok()?` propagates the parse failure, so the whole
///   function returns `None`.
/// - **Minor / release**: `.and_then(|p| p.parse().ok()).unwrap_or(0)`
///   silently substitutes `0` — indistinguishable from that component being
///   genuinely absent. `"3.99999999999.1"` therefore parses as
///   `Some((3, 0, 1))`, not `None` and not a truncated/wrapped value.
///
/// This is intentional, not a bug: no real version string this driver ever
/// parses (a `Cargo.toml` version or a database server's reported version)
/// comes anywhere close to `u32::MAX`, so the difference in behaviour between
/// the fields is unobservable in practice. It is documented here, with a test
/// pinning it (`overflowing_minor_or_release_is_treated_as_absent`), purely so
/// a future reader does not mistake the asymmetry for an oversight and "fix"
/// it into a behaviour change.
pub fn parse_dotted_version(raw: &str) -> Option<(u32, u32, u32)> {
    let numeric_prefix: &str = raw
        .split(|c: char| !c.is_ascii_digit() && c != '.')
        .next()
        .unwrap_or("");

    let mut parts = numeric_prefix.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let release: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);

    Some((major, minor, release))
}

/// Expands to this crate's version rendered as an ODBC `SQL_DRIVER_VER` string.
///
/// `env!` expands at the macro's *call site*, so each driver crate gets its own
/// `Cargo.toml` version. This is what stops `SQL_DRIVER_VER` from drifting: the
/// only way to change it is to change the version the crate is published under.
///
/// A component that fails to parse falls back to `0` rather than panicking --
/// Cargo guarantees these are numeric, so the fallback is unreachable in
/// practice, but a driver must not fail to load over a version string.
#[macro_export]
macro_rules! driver_version {
    () => {
        $crate::types::format_odbc_version(
            env!("CARGO_PKG_VERSION_MAJOR").parse().unwrap_or(0),
            env!("CARGO_PKG_VERSION_MINOR").parse().unwrap_or(0),
            env!("CARGO_PKG_VERSION_PATCH").parse().unwrap_or(0),
        )
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_a_typical_version_with_zero_padding() {
        assert_eq!(format_odbc_version(3, 8, 0), "03.08.0000");
        assert_eq!(format_odbc_version(0, 2, 0), "00.02.0000");
        assert_eq!(format_odbc_version(1, 12, 345), "01.12.0345");
    }

    /// A three-digit major version does not fit the spec's two-digit major
    /// field. Widening is the only lossless option — truncating "467" to "67"
    /// would report a different server.
    #[test]
    fn widens_rather_than_truncates_an_oversized_major() {
        assert_eq!(format_odbc_version(467, 0, 0), "467.00.0000");
    }

    #[test]
    fn widens_rather_than_truncates_an_oversized_release() {
        assert_eq!(format_odbc_version(1, 2, 99999), "01.02.99999");
    }

    #[test]
    fn parses_dotted_versions() {
        assert_eq!(parse_dotted_version("3.45.1"), Some((3, 45, 1)));
        assert_eq!(parse_dotted_version("467"), Some((467, 0, 0)));
        assert_eq!(parse_dotted_version("0.215"), Some((0, 215, 0)));
    }

    /// Some servers report a build suffix such as "468-SNAPSHOT". Everything
    /// from the first non-numeric, non-dot character onward is ignored.
    #[test]
    fn parses_a_version_with_a_trailing_suffix() {
        assert_eq!(parse_dotted_version("468-SNAPSHOT"), Some((468, 0, 0)));
        assert_eq!(parse_dotted_version("3.45.1+extra"), Some((3, 45, 1)));
    }

    #[test]
    fn rejects_a_version_with_no_leading_number() {
        assert_eq!(parse_dotted_version(""), None);
        assert_eq!(parse_dotted_version("unknown"), None);
    }

    /// Extra components beyond major.minor.release are dropped, not folded
    /// into the release field, so "1.2.3.4" and "1.2.3" agree.
    #[test]
    fn ignores_components_beyond_the_third() {
        assert_eq!(parse_dotted_version("1.2.3.4"), Some((1, 2, 3)));
    }

    /// Pins the documented (deliberately asymmetric) overflow behaviour: a
    /// component too large for `u32` fails the whole parse in the major
    /// field, but is silently treated as absent (0) in the minor/release
    /// fields. See `parse_dotted_version`'s doc comment for why this is
    /// acceptable rather than a bug to fix.
    #[test]
    fn overflowing_minor_or_release_is_treated_as_absent() {
        // Major overflow fails the whole parse.
        assert_eq!(parse_dotted_version("99999999999.1.2"), None);

        // Minor overflow is indistinguishable from an absent minor.
        assert_eq!(
            parse_dotted_version("3.99999999999.1"),
            Some((3, 0, 1)),
            "an oversized minor component should be treated as absent (0), \
             not fail the parse"
        );

        // Release overflow is indistinguishable from an absent release.
        assert_eq!(
            parse_dotted_version("3.4.99999999999"),
            Some((3, 4, 0)),
            "an oversized release component should be treated as absent (0), \
             not fail the parse"
        );
    }
}
