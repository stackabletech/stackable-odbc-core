//! Generic ODBC driver framework.
//!
//! `stackable-odbc-core` provides the database-independent half of an ODBC driver. A
//! concrete driver crate implements the [`backend::Backend`] and
//! [`backend::StatementBackend`] traits, then calls the
//! [`forward_ffi!`](macro@forward_ffi) macro to generate all 73 C ABI entry
//! points (72 `SQL*` functions plus `ConfigDSNW`) automatically.
//!
//! # Call flow
//!
//! Every ODBC function call enters through one of the generated FFI entry
//! points in [`ffi`]. Each one:
//! 1. Validates the handle tag (prevents type confusion at the C boundary).
//! 2. Wraps execution in [`panic::panic_safe`] to catch panics and convert them
//!    to `SQL_ERROR`.
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
//! # Unicode only
//!
//! Only the Wide (W-suffix) ODBC functions are exported. The Driver Manager
//! translates ANSI application calls to Wide calls automatically.

pub mod backend;
pub mod column_value;
pub mod conformance;
pub mod diagnostics;
pub mod errors;
pub mod escape;
pub mod ffi;
mod forward_ffi;
pub mod function_id;
pub mod handles;
pub mod logging;
pub mod panic;
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
