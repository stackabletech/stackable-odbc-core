//! Shared support for the `SQLGetInfoW` info-type conformance test.
//!
//! Both `stackable-odbc-core`'s own tests (driving [`crate::backend::Backend`] via
//! `MockBackend`) and each driver crate's own FFI integration tests (driving
//! their real `Backend` impl) check the same two properties for every
//! `InfoType` `odbc-sys` compiles, through the same real
//! [`crate::ffi::info::sql_get_info_w`] entry point:
//!
//! 1. **Shape** — the returned [`crate::types::InfoValue`] variant matches
//!    what [`crate::types::expected_kind`] declares for that info type (see
//!    [`crate::types::info_type_shape`] for the spec transcription and the
//!    three real bugs this catches).
//! 2. **No genuine `SQL_CONVERT_*` code ever returns 0** — a `0` conversion
//!    bitmap is what makes the Windows Driver Manager block `SQLGetData`
//!    with `HYC00` (see `AGENTS.md`'s Windows Driver Manager compatibility
//!    checklist).
//! 3. **Info types that constrain each other agree** — see
//!    [`crate::conformance::info_group_inconsistencies`]. Several `SQLGetInfo`
//!    answers are statements about one fact under different names, and core
//!    cannot police a backend's [`crate::backend::Backend::get_info`] at
//!    runtime because that method is entitled to answer anything. Stating the
//!    invariants here lets each driver's suite catch a group it overrode only
//!    half of.
//!
//! This module supplies the pieces every such test needs: the *derived* (not
//! hand-copied) list of every `InfoType` the FFI boundary can produce, the list
//! of genuine `SQL_CONVERT_*` codes, ways to observe which
//! [`crate::types::InfoValueKind`] and which value `sql_get_info_w` actually
//! wrote without bypassing that function, and the group-consistency check.
//!
//! No backend-specific references belong here or in any of its callers in
//! this crate — this module is shared, driver-agnostic infrastructure.

use std::ffi::c_void;

use odbc_sys::InfoType;

use crate::backend::Backend;
use crate::ffi::info::sql_get_info_w;
pub use crate::types::InfoValueKind;
use crate::types::{
    SQL_CONVERT_FUNCTIONS_FIRST, SQL_CONVERT_FUNCTIONS_LAST, SQL_CONVERT_GUID, SQL_CONVERT_WCHAR,
    SQL_CONVERT_WVARCHAR,
};
use crate::types::{SqlReturn, info_type_from_raw};

/// Every `InfoType` variant `odbc-sys` compiles in this feature set, derived
/// by scanning the full raw `u16` space through
/// [`crate::types::info_type_from_raw`] (the exact conversion function the
/// FFI boundary itself uses) rather than maintained as a second, separately
/// curated list that could silently drift from it. A hand-maintained list of
/// "info types someone thought to test" is precisely the failure mode that
/// let `SQL_CONVERT_GUID`, `SQL_INTEGRITY`, and the 49-52 scalar-function
/// bitmaps ship unchecked.
pub fn all_info_types() -> Vec<InfoType> {
    (0..=u16::MAX).filter_map(info_type_from_raw).collect()
}

/// Every genuine per-source-type `SQL_CONVERT_*` info type: the codes for
/// which `0` is not a legitimate answer (per `AGENTS.md`), because it isn't
/// distinguishable from "the driver forgot to implement this"; see
/// [`crate::types::SQL_CONVERT_FUNCTIONS_FIRST`] and
/// [`crate::types::SQL_CONVERT_WCHAR`] for why these fall into two
/// non-adjacent numeric ranges plus one standalone code. None of these 25
/// raw values has a named `odbc_sys::InfoType` variant, so they are not part
/// of [`all_info_types`]; they can only be queried as raw `u16`s.
pub fn genuine_convert_info_types() -> Vec<u16> {
    (SQL_CONVERT_FUNCTIONS_FIRST..=SQL_CONVERT_FUNCTIONS_LAST)
        .chain(SQL_CONVERT_WCHAR..=SQL_CONVERT_WVARCHAR)
        .chain(std::iter::once(SQL_CONVERT_GUID))
        .collect()
}

/// Drives the real `sql_get_info_w::<B>` entry point for `info_type` and
/// reports which [`InfoValueKind`] it actually produced, without bypassing
/// that function: the defects this test suite guards against lived
/// entirely inside it, so observing its behavior from anywhere else would
/// prove nothing.
///
/// # How the shape is observed through a type-erased C ABI
///
/// `SQLGetInfoW` writes through a single `void*`, so the only way to tell
/// "did the driver treat this as a string, a `u16`, or a `u32`" from outside
/// is to exploit how `sql_get_info_w` writes each shape differently when
/// given `buffer_length = 0` and a non-null, sentinel-filled buffer:
///
/// - **String**: `write_utf16` bails out before writing anything once
///   `buf_len <= 0` (it still reports the true length through
///   `string_length_ptr`); every sentinel byte survives untouched.
///
///   Note the *return code* for this case is `SQL_SUCCESS_WITH_INFO`, not
///   `SQL_SUCCESS`. A non-null buffer with no room in it is total truncation,
///   and `SQLGetInfo`'s own diagnostics table carries the `01004` row that
///   says so. A caller asserting on the return value of this helper wants
///   "not `SQL_ERROR`" rather than "is `SQL_SUCCESS`": the shape probe
///   deliberately supplies a buffer it has declared to be empty.
/// - **`U16`**: the numeric branch does not consult `buffer_length` at all;
///   it always writes exactly 2 bytes through `write_unaligned` and reports
///   `string_length_ptr = 2`.
/// - **`U32`**: same, but 4 bytes and `string_length_ptr = 4`.
///
/// These three outcomes are mutually exclusive and observable purely from
/// which sentinel bytes changed, so this reports the actual variant
/// `sql_get_info_w` picked with no need to special-case short strings whose
/// *content* happens to be 1-2 characters: the discriminator is which
/// *code path* ran, not the byte count of whatever it produced.
///
/// # Safety
///
/// `connection_handle` must be null or a valid `ConnectionHandle<B>`
/// allocated by `sql_alloc_handle`, exactly as `sql_get_info_w` requires.
pub unsafe fn observe_info_value_kind<B: Backend>(
    connection_handle: *mut c_void,
    info_type: u16,
) -> (SqlReturn, InfoValueKind, i16) {
    // A pattern very unlikely to collide with any real returned numeric
    // value (all known bitmasks/counts/lengths in this codebase are far
    // smaller than 0xEEEE / 0xEEEEEEEE).
    const SENTINEL: u8 = 0xEE;
    let mut buf = [SENTINEL; 8];
    let mut string_length: i16 = -1;

    // SAFETY: forwarded from the caller's own safety contract; buf is a
    // valid 8-byte writable buffer for the duration of this call.
    let ret = unsafe {
        sql_get_info_w::<B>(
            connection_handle,
            info_type,
            buf.as_mut_ptr() as *mut c_void,
            0, // buffer_length = 0: see the doc comment for why this matters
            &mut string_length,
        )
    };

    let kind = if buf[0..2] != [SENTINEL; 2] && buf[2..8] == [SENTINEL; 6] {
        InfoValueKind::U16
    } else if buf[0..4] != [SENTINEL; 4] && buf[4..8] == [SENTINEL; 4] {
        InfoValueKind::U32
    } else if buf == [SENTINEL; 8] {
        InfoValueKind::String
    } else {
        unreachable!(
            "sql_get_info_w wrote a byte pattern for info_type {info_type} \
             ({:?}) inconsistent with the U16/U32/String write shapes: {buf:?}",
            info_type_from_raw(info_type),
        );
    };

    (ret, kind, string_length)
}

/// Drives the real `sql_get_info_w::<B>` entry point for a raw `info_type`
/// already known (or expected) to be `U32`-shaped and returns the actual
/// 4-byte value it wrote. Used for property 2 (no genuine `SQL_CONVERT_*`
/// code ever returns 0), where the specific *value* matters, not just its
/// shape.
///
/// # Safety
///
/// Same contract as [`observe_info_value_kind`].
pub unsafe fn observe_u32_value<B: Backend>(
    connection_handle: *mut c_void,
    info_type: u16,
) -> (SqlReturn, u32) {
    let mut value: u32 = 0;
    let mut string_length: i16 = 0;
    // SAFETY: forwarded from the caller's own safety contract.
    let ret = unsafe {
        sql_get_info_w::<B>(
            connection_handle,
            info_type,
            &mut value as *mut u32 as *mut c_void,
            4,
            &mut string_length,
        )
    };
    (ret, value)
}

/// Reads a character-shaped info type through the real `sql_get_info_w::<B>`
/// entry point.
///
/// # Safety
///
/// Same contract as [`observe_info_value_kind`].
pub unsafe fn observe_string_value<B: Backend>(
    connection_handle: *mut c_void,
    info_type: u16,
) -> (SqlReturn, String) {
    let mut buf = [0u16; 256];
    let mut string_length: i16 = 0;
    // SAFETY: forwarded from the caller's own safety contract.
    let ret = unsafe {
        sql_get_info_w::<B>(
            connection_handle,
            info_type,
            buf.as_mut_ptr() as *mut c_void,
            (buf.len() * 2) as i16,
            &mut string_length,
        )
    };
    let units = usize::try_from(string_length / 2)
        .unwrap_or(0)
        .min(buf.len());
    (ret, String::from_utf16_lossy(&buf[..units]))
}

/// Reads a `SQLUSMALLINT`-shaped info type through the real
/// `sql_get_info_w::<B>` entry point.
///
/// Separate from [`observe_u32_value`] because the driver writes exactly two
/// bytes for these, and reading four would leave the upper half as whatever
/// the caller's buffer held.
///
/// # Safety
///
/// Same contract as [`observe_info_value_kind`].
pub unsafe fn observe_u16_value<B: Backend>(
    connection_handle: *mut c_void,
    info_type: u16,
) -> (SqlReturn, u16) {
    let mut value: u16 = 0;
    let mut string_length: i16 = 0;
    // SAFETY: forwarded from the caller's own safety contract.
    let ret = unsafe {
        sql_get_info_w::<B>(
            connection_handle,
            info_type,
            &mut value as *mut u16 as *mut c_void,
            2,
            &mut string_length,
        )
    };
    (ret, value)
}

/// Checks the `SQLGetInfo` groups whose members constrain each other, and
/// returns one message per violation — empty when the driver is consistent.
///
/// # Why this is here rather than in core's own tests
///
/// Core cannot police a backend's [`Backend::get_info`] at runtime: that
/// method runs *first* and is entitled to answer anything. What core can do is
/// state the invariants once, in the shared harness every driver's test suite
/// already runs against its real backend, so a driver that overrides one member
/// of a group and forgets its neighbours fails its own tests.
///
/// This matters most for the vendor-terminology group, which has no `Backend`
/// hooks by design: a driver whose vendor says "database" states it by
/// answering `SQL_CATALOG_TERM` in `get_info`, and nothing but this check
/// notices if `SQL_CATALOG_NAME` still says `"N"`.
///
/// # The invariants, and why two of them are one-directional
///
/// - `SQL_CATALOG_NAME = "Y"` if and only if `SQL_CATALOG_TERM` and
///   `SQL_CATALOG_NAME_SEPARATOR` are both non-empty. The spec defines the
///   latter two in terms of the former: "an empty string is returned if
///   catalogs are not supported by the data source. To determine whether
///   catalogs are supported, an application calls **SQLGetInfo** with the
///   SQL_CATALOG_NAME information type."
/// - Catalogs unsupported implies `SQL_CATALOG_USAGE` and
///   `SQL_CATALOG_LOCATION` are `0`: there is nothing for a catalog to be used
///   in, or located relative to.
/// - An empty `SQL_SCHEMA_TERM` implies `SQL_SCHEMA_USAGE = 0`, but **not** the
///   converse. There is no `SQL_SCHEMA_NAME` info type to pair the term with —
///   the `SQL_SCHEMA_TERM` page names one, but no such code exists in
///   `sqlext.h` — so the term is itself the only support signal, and a data
///   source may have schemas that appear in no statement this bitmask
///   enumerates.
/// - `SQL_PROCEDURES = "Y"` implies a non-empty `SQL_PROCEDURE_TERM`, and
///   **not** the converse. The spec makes `SQL_PROCEDURES` a conjunction —
///   "the data source supports procedures **and** the driver supports the ODBC
///   procedure invocation syntax" — so a data source with procedures reports
///   `"N"` on a driver without `{call}`, while still having a vendor term for
///   them.
/// - `SQL_TXN_CAPABLE = SQL_TC_NONE` if and only if
///   `SQL_TXN_ISOLATION_OPTION = 0`, and `SQL_TC_NONE` implies
///   `SQL_DEFAULT_TXN_ISOLATION = 0`. A data source with no transactions has no
///   isolation levels to run them at.
///
/// # Safety
///
/// Same contract as [`observe_info_value_kind`].
pub unsafe fn info_group_inconsistencies<B: Backend>(
    connection_handle: *mut c_void,
) -> Vec<String> {
    use crate::types::{SQL_PROCEDURE_TERM, SQL_PROCEDURES, SQL_TC_NONE};

    // SAFETY: each read forwards this function's own safety contract.
    let string_of = |t: u16| unsafe { observe_string_value::<B>(connection_handle, t).1 };
    let u16_of = |t: u16| unsafe { observe_u16_value::<B>(connection_handle, t).1 };
    let u32_of = |t: u32| unsafe { observe_u32_value::<B>(connection_handle, t as u16).1 };

    let mut violations = Vec::new();
    let mut require = |holds: bool, message: String| {
        if !holds {
            violations.push(message);
        }
    };

    let catalogs = string_of(InfoType::CatalogName as u16) == "Y";
    let catalog_term = string_of(InfoType::CatalogTerm as u16);
    let separator = string_of(InfoType::CatalogNameSeparator as u16);
    let has_term = !catalog_term.is_empty();
    let has_separator = !separator.is_empty();
    require(
        catalogs == has_term,
        format!(
            "SQL_CATALOG_NAME says {}, but SQL_CATALOG_TERM is {catalog_term:?}",
            if catalogs { "\"Y\"" } else { "\"N\"" }
        ),
    );
    require(
        catalogs == has_separator,
        format!(
            "SQL_CATALOG_NAME says {}, but SQL_CATALOG_NAME_SEPARATOR is {separator:?}",
            if catalogs { "\"Y\"" } else { "\"N\"" }
        ),
    );
    if !catalogs {
        let usage = u32_of(InfoType::CatalogUsage as u32);
        let location = u16_of(InfoType::CatalogLocation as u16);
        require(
            usage == 0,
            format!("catalogs are unsupported, but SQL_CATALOG_USAGE is {usage:#x}"),
        );
        require(
            location == 0,
            format!("catalogs are unsupported, but SQL_CATALOG_LOCATION is {location}"),
        );
    }

    if string_of(InfoType::SchemaTerm as u16).is_empty() {
        let usage = u32_of(InfoType::SchemaUsage as u32);
        require(
            usage == 0,
            format!("SQL_SCHEMA_TERM is empty, but SQL_SCHEMA_USAGE is {usage:#x}"),
        );
    }

    if string_of(SQL_PROCEDURES) == "Y" {
        let term = string_of(SQL_PROCEDURE_TERM);
        require(
            !term.is_empty(),
            "SQL_PROCEDURES says \"Y\", but SQL_PROCEDURE_TERM is empty".to_string(),
        );
    }

    let txn_capable = u16_of(InfoType::TransactionCapable as u16);
    let options = u32_of(InfoType::TransactionIsolationProtocol as u32);
    require(
        (txn_capable == SQL_TC_NONE as u16) == (options == 0),
        format!(
            "SQL_TXN_CAPABLE is {txn_capable} and SQL_TXN_ISOLATION_OPTION is \
             {options:#x}; a data source with no transactions has no isolation \
             levels, and one with levels is not SQL_TC_NONE"
        ),
    );
    if txn_capable == SQL_TC_NONE as u16 {
        let default = u32_of(InfoType::DefaultTxnIsolation as u32);
        require(
            default == 0,
            format!(
                "SQL_TXN_CAPABLE is SQL_TC_NONE, but SQL_DEFAULT_TXN_ISOLATION \
                 is {default:#x}"
            ),
        );
    }

    violations
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffi::connect::sql_driver_connect_w;
    use crate::ffi::handle::{sql_alloc_handle, sql_free_handle};
    use crate::test_utils::MockBackend;
    use crate::types::{HandleType, expected_kind};

    /// Allocates env + connection handles, leaving the connection
    /// unconnected, the pre-connect path (`Backend::get_info_pre_connect`).
    unsafe fn alloc_env_and_conn() -> (*mut c_void, *mut c_void) {
        let mut env: *mut c_void = std::ptr::null_mut();
        let _ = unsafe {
            sql_alloc_handle::<MockBackend>(HandleType::Env as i16, std::ptr::null_mut(), &mut env)
        };
        let mut conn: *mut c_void = std::ptr::null_mut();
        let _ = unsafe { sql_alloc_handle::<MockBackend>(HandleType::Dbc as i16, env, &mut conn) };
        (env, conn)
    }

    /// Connects the handle so `sql_get_info_w` takes the connected
    /// (`Backend::get_info`) path. `MockBackend::connect` always succeeds.
    unsafe fn connect(conn: *mut c_void) -> SqlReturn {
        let input = "Host=localhost;Port=8080;Database=test;User=me";
        let wide: Vec<u16> = input.encode_utf16().collect();
        unsafe {
            sql_driver_connect_w::<MockBackend>(
                conn,
                std::ptr::null_mut(),
                wide.as_ptr(),
                wide.len() as i16,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                0,
            )
        }
    }

    unsafe fn cleanup(env: *mut c_void, conn: *mut c_void) {
        unsafe {
            let _ = crate::ffi::connect::sql_disconnect::<MockBackend>(conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    /// Property 1 (pre-connect path): `MockBackend::get_info_pre_connect`
    /// answers nothing (`NotImplemented` for every info type, the default
    /// `Backend` trait method), so this exercises exactly
    /// `info_type_default_response`'s shape-aware fallback for every one of
    /// the [`all_info_types`] `odbc-sys` compiles. This is the literal
    /// defect location for all three bugs `info_type_shape` documents:
    /// a test that went through a backend method instead of
    /// `sql_get_info_w` would not exercise this fallback at all.
    #[test]
    fn property1_shape_holds_pre_connect_for_every_named_info_type() {
        unsafe {
            let (env, conn) = alloc_env_and_conn();

            for info_type in all_info_types() {
                let (ret, kind, _string_length) =
                    observe_info_value_kind::<MockBackend>(conn, info_type as u16);
                assert_ne!(
                    ret,
                    SqlReturn::ERROR,
                    "{info_type:?}: SQLGetInfoW must never return SQL_ERROR for a \
                     named-but-unimplemented info type (corrupts the Windows DM's \
                     internal state)"
                );
                assert_eq!(
                    kind,
                    expected_kind(info_type),
                    "{info_type:?}: default response shape was {kind:?}, expected \
                     {:?} per the SQLGetInfo spec (see info_type_shape.rs)",
                    expected_kind(info_type)
                );
            }

            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    /// Property 1 (connected path): same as above, but through
    /// `Backend::get_info` instead of `get_info_pre_connect`.
    /// `MockBackend::get_info` also unconditionally reports `NotImplemented`
    /// for everything, so this exercises the exact same shape-aware
    /// fallback via the other call site in `sql_get_info_w`.
    #[test]
    fn property1_shape_holds_connected_for_every_named_info_type() {
        unsafe {
            let (env, conn) = alloc_env_and_conn();
            assert_eq!(connect(conn), SqlReturn::SUCCESS);

            for info_type in all_info_types() {
                let (ret, kind, _string_length) =
                    observe_info_value_kind::<MockBackend>(conn, info_type as u16);
                assert_ne!(
                    ret,
                    SqlReturn::ERROR,
                    "{info_type:?}: SQLGetInfoW must never return SQL_ERROR for a \
                     named-but-unimplemented info type"
                );
                assert_eq!(
                    kind,
                    expected_kind(info_type),
                    "{info_type:?}: default response shape was {kind:?}, expected \
                     {:?} per the SQLGetInfo spec (see info_type_shape.rs)",
                    expected_kind(info_type)
                );
            }

            cleanup(env, conn);
        }
    }

    /// Core plus a backend that answers no info type of its own must already
    /// satisfy every group invariant — otherwise the defaults core supplies
    /// contradict each other, and a driver inherits the contradiction before it
    /// has written a line of `get_info`.
    #[test]
    fn cores_own_answers_are_group_consistent() {
        unsafe {
            let (env, conn) = alloc_env_and_conn();
            assert_eq!(connect(conn), SqlReturn::SUCCESS);

            // Every invariant above is an implication or a biconditional
            // between two reads, so a reader that returned `""` and `0` for
            // everything would satisfy all of them. These four assertions are
            // what stops the check passing vacuously: they pin real values on
            // both sides of the two groups this mock exercises.
            assert_eq!(
                observe_string_value::<MockBackend>(conn, InfoType::CatalogName as u16).1,
                "Y"
            );
            assert_eq!(
                observe_string_value::<MockBackend>(conn, InfoType::CatalogTerm as u16).1,
                "catalog"
            );
            assert_eq!(
                observe_u16_value::<MockBackend>(conn, InfoType::TransactionCapable as u16).1,
                crate::types::SQL_TC_ALL as u16
            );
            assert_ne!(
                observe_u32_value::<MockBackend>(
                    conn,
                    InfoType::TransactionIsolationProtocol as u16
                )
                .1,
                0
            );

            let violations = info_group_inconsistencies::<MockBackend>(conn);
            assert!(
                violations.is_empty(),
                "core's own SQLGetInfo answers contradict each other:\n  {}",
                violations.join("\n  ")
            );

            cleanup(env, conn);
        }
    }

    /// Property 2: none of the genuine `SQL_CONVERT_*` codes may ever
    /// resolve to 0 through the real FFI path, connected or pre-connect:
    /// per `AGENTS.md`, a `0` conversion bitmap is what makes the Windows
    /// Driver Manager block `SQLGetData` with `HYC00`. None of these raw
    /// values has a named `InfoType`, so `MockBackend` (which never
    /// implements anything) drives every one of them straight into
    /// `info_type_default_response`'s genuine-convert-range branch.
    #[test]
    fn property2_no_genuine_convert_info_type_ever_returns_zero() {
        unsafe {
            let (env, conn) = alloc_env_and_conn();
            assert_eq!(connect(conn), SqlReturn::SUCCESS);

            for info_type in genuine_convert_info_types() {
                let (ret, value) = observe_u32_value::<MockBackend>(conn, info_type);
                assert_eq!(
                    ret,
                    SqlReturn::SUCCESS,
                    "raw SQL_CONVERT_* info type {info_type} must not error"
                );
                assert_ne!(
                    value, 0,
                    "raw SQL_CONVERT_* info type {info_type} returned 0 — this is the \
                     exact shape that makes the Windows Driver Manager block \
                     SQLGetData with HYC00 (AGENTS.md)"
                );
            }

            cleanup(env, conn);
        }
    }

    /// Same as above, but pre-connect: `info_type_default_response` is
    /// reached from `get_info_pre_connect`'s `NotImplemented` too, and the
    /// genuine-convert-range check does not depend on `conn` being `Some`.
    #[test]
    fn property2_no_genuine_convert_info_type_ever_returns_zero_pre_connect() {
        unsafe {
            let (env, conn) = alloc_env_and_conn();

            for info_type in genuine_convert_info_types() {
                let (ret, value) = observe_u32_value::<MockBackend>(conn, info_type);
                assert_eq!(
                    ret,
                    SqlReturn::SUCCESS,
                    "raw SQL_CONVERT_* info type {info_type} must not error pre-connect"
                );
                assert_ne!(
                    value, 0,
                    "raw SQL_CONVERT_* info type {info_type} returned 0 pre-connect"
                );
            }

            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    /// Sanity check on [`genuine_convert_info_types`] itself: it must
    /// actually contain the specific codes the module docs and
    /// `info_type_default_response` call out by name, including the 122-126
    /// block: this is the list a silent regression to `48..=73`-style
    /// over-narrowing would shrink without any other test noticing.
    #[test]
    fn genuine_convert_info_types_contains_the_documented_codes() {
        let codes = genuine_convert_info_types();
        for expected in [53u16, 71, 122, 123, 124, 125, 126, 173] {
            assert!(
                codes.contains(&expected),
                "genuine_convert_info_types() is missing {expected}"
            );
        }
        assert_eq!(
            codes.len(),
            25,
            "expected 19 (53-71) + 5 (122-126) + 1 (173) = 25"
        );
    }
}
