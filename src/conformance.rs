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
//!
//! This module supplies the three pieces every such test needs: the
//! *derived* (not hand-copied) list of every `InfoType` the FFI boundary can
//! produce, the list of genuine `SQL_CONVERT_*` codes, and a way to observe
//! which [`crate::types::InfoValueKind`] `sql_get_info_w` actually wrote
//! without bypassing that function.
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
                assert_eq!(
                    ret,
                    SqlReturn::SUCCESS,
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
                assert_eq!(
                    ret,
                    SqlReturn::SUCCESS,
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
