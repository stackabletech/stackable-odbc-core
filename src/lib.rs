//! Generic ODBC driver framework.
//!
//! `stackable-odbc-core` provides the database-independent half of an ODBC driver. A
//! concrete driver crate implements the [`backend::Backend`] and
//! [`backend::StatementBackend`] traits, then calls the
//! [`forward_ffi!`](macro@forward_ffi) macro to generate the C ABI entry points
//! automatically: 72 `SQL*` functions, plus `ConfigDSNW` on Windows, where the
//! installer library calls it to configure a DSN.
//!
//! # Call flow
//!
//! Every ODBC function call enters through one of the generated FFI entry
//! points in [`ffi`]. Each one:
//! 1. Validates the handle against the driver's handle registry, which is a
//!    bounds, generation and kind check that never dereferences the pointer the
//!    application passed.
//! 2. Wraps execution in a panic guard that catches panics and converts them to
//!    `SQL_ERROR`.
//! 3. Delegates to the generic implementation in the matching `ffi/` submodule.
//! 4. The generic implementation calls a [`backend::Backend`] method for
//!    any database-specific logic.
//!
//! # Thread safety
//!
//! All handle types require `B: Send + Sync`. Individual handles are **not**
//! internally synchronized — the ODBC Driver Manager serializes concurrent
//! calls to the same handle.
//!
//! # Unicode
//!
//! Every ODBC function that takes or returns a string is exported **only** in
//! its Wide (`W`-suffix) form — `SQLDriverConnectW`, `SQLGetInfoW`,
//! `SQLDescribeColW` and so on. The Driver Manager translates an ANSI
//! application's calls into those automatically, so there is no ANSI variant to
//! write.
//!
//! Most exported entry points are *not* `W`-suffixed, because most ODBC
//! functions take no strings at all: `SQLAllocHandle`, `SQLFetch`,
//! `SQLBindCol`, `SQLEndTran` and 36 others have a single spelling that serves
//! both. Of the 72 `SQL*` functions `forward_ffi!` generates, 32 are `W` forms
//! and 40 have no encoding in their signature to vary.

/// The `odbc-sys` version this crate was built against.
///
/// `odbc-sys` is a *public* dependency: its types appear in `Backend` and
/// `StatementBackend` signatures, in `forward_ffi!`'s generated entry points and
/// throughout [`types`]. A driver that declared its own `odbc-sys` dependency
/// had nothing pinning it to the same version, and two different versions of a
/// `#[repr(C)]` enum are two different types to the compiler — which surfaces as
/// a trait-impl mismatch that names the same type twice. Depend on this
/// re-export instead of adding the crate directly.
///
/// Feature note: cargo unifies features across a dependency graph, so a driver
/// enabling `odbc_version_4` turns it on for core's build too, adding
/// `InfoType` variants that `info_type_from_raw` does not know. Core is built
/// and tested against the default feature set.
pub use odbc_sys;

pub mod backend;
pub mod column_value;
/// `SQLGetInfoW` return-shape conformance checks, shared with driver test
/// suites. Behind the default-off `test-support` feature: see `Cargo.toml`.
#[cfg(any(test, feature = "test-support"))]
pub mod conformance;
pub(crate) mod diagnostics;
pub mod errors;
pub mod escape;
pub mod ffi;
mod forward_ffi;
pub mod function_id;
pub(crate) mod handles;
pub mod logging;
pub(crate) mod panic;
pub mod synthetic;
pub mod types;
pub mod utf16;

/// FFI binding to the ODBC installer library (`libodbcinst` on Unix, `odbccp32` on Windows).
///
/// This provides `SQLGetPrivateProfileStringW` which drivers use to read DSN
/// configuration from `odbc.ini`. Not part of `odbc-sys` because it rejected
/// installer functions: <https://github.com/pacman82/odbc-sys/pull/44>
pub(crate) mod odbcinst {
    #[cfg_attr(windows, link(name = "odbccp32", kind = "raw-dylib"))]
    #[cfg_attr(not(windows), link(name = "odbcinst"))]
    unsafe extern "system" {
        /// Read a value from the ODBC configuration files (odbc.ini / odbcinst.ini).
        ///
        /// When `entry` is NULL, returns a null-separated list of all key names in the section.
        /// Returns the number of characters written (negative on error).
        pub fn SQLGetPrivateProfileStringW(
            section: *const u16,
            entry: *const u16,
            default: *const u16,
            ret_buffer: *mut u16,
            ret_buffer_size: i32,
            filename: *const u16,
        ) -> i32;
    }
}

#[cfg(test)]
pub(crate) mod test_utils;
