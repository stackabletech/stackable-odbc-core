//! [`DeclaredOdbcVersion`]: which `SQL_ATTR_ODBC_VERSION` the application
//! declared through `SQLSetEnvAttr`.

use crate::types::{SQL_OV_ODBC2, SQL_OV_ODBC3, SQL_OV_ODBC3_80};

/// The ODBC version an application declared for an environment.
///
/// Core's own type rather than [`odbc_sys::AttrOdbcVersion`], for one reason:
/// `odbc-sys` deliberately has no `SQL_OV_ODBC2` variant — its `attributes.rs`
/// carries `// Not supported by this crate` above a commented-out
/// `SQL_OV_ODBC2 = 2` — and core may not redefine one of its enums. The value
/// has to be storable, because the Driver Manager both sets and reads it: unixODBC
/// forwards `environment->requested_version` verbatim to the driver's
/// `SQLSetEnvAttr` at connect time and then reads it straight back with
/// `SQLGetEnvAttr` to decide how to treat the driver
/// (`DriverManager/SQLConnect.c`).
///
/// Declaring 2.x does **not** change how core answers anything else. The spec's
/// 2.x behaviours — 2.x SQLSTATEs, the 2.x datetime type codes — are the Driver
/// Manager's mapping, and unixODBC drives that mapping from the *application's*
/// requested version rather than from the driver's
/// (`DriverManager/SQLGetDiagRec.c`'s `__get_version`), so it happens whether
/// core knows about it or not. What core owes here is to accept the value and
/// report it back unchanged.
#[allow(
    non_camel_case_types,
    reason = "Odbc3_80 mirrors the spec's SQL_OV_ODBC3_80 and odbc-sys' own variant name"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclaredOdbcVersion {
    /// `SQL_OV_ODBC2`.
    Odbc2,
    /// `SQL_OV_ODBC3`. The default for a freshly allocated environment.
    Odbc3,
    /// `SQL_OV_ODBC3_80`.
    Odbc3_80,
}

impl DeclaredOdbcVersion {
    /// The `SQL_OV_*` value this version is written as over the C ABI.
    #[must_use]
    pub fn raw(self) -> i32 {
        match self {
            Self::Odbc2 => SQL_OV_ODBC2,
            Self::Odbc3 => SQL_OV_ODBC3,
            Self::Odbc3_80 => SQL_OV_ODBC3_80,
        }
    }

    /// Whether the application declared ODBC 2.x behaviour.
    #[must_use]
    pub fn is_odbc_2(self) -> bool {
        matches!(self, Self::Odbc2)
    }
}
