#![deny(missing_docs)]
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
//! Handles are internally synchronised, per connection. A connection and all of
//! its statements share one lock, so calls on different connections proceed in
//! parallel while calls on the same connection are serialised.
//!
//! [`SQLAllocHandle`'s Comments
//! section](https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlallochandle-function)
//! is why this crate does not leave that to the Driver Manager: "on operating
//! systems that support multiple threads, applications can use the same
//! environment, connection, statement, or descriptor handle on different
//! threads. Drivers must therefore support safe, multithread access to this
//! information."
//!
//! `SQLCancel` is the exception, deliberately: it may be called from another
//! thread while a function is executing on the statement, and it never takes
//! that lock. Taking it would make cancel wait for the query it was asked to
//! cancel. It signals the backend's [`backend::Backend::CancelToken`] instead
//! and, per the spec, posts no diagnostic records in that case.
//!
//! A driver's own state is its own responsibility, with one bounded exception:
//! whatever a backend puts in its `CancelToken` is the only thing core will
//! touch concurrently with a connection. That type — and only that type — needs
//! to be safe for concurrent use.
//!
//! On unixODBC, a driver built on this crate **must** declare `Threading = 2`
//! in `odbcinst.ini` for cross-thread `SQLCancel` to work at all. unixODBC's
//! own `SQLCancel` only takes the statement lock at the default protection
//! level `3`; at `2` it does not, which is what lets a cancel from another
//! thread reach this crate's lock-free path instead of blocking behind the
//! Driver Manager's own lock for the query it was asked to cancel.
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
mod sync;
pub mod synthetic;
/// Hooks a driver's test suite needs and its production build must not have.
/// Behind the same default-off `test-support` feature as [`conformance`].
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
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
