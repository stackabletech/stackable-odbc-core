//! Driver information: `SQLGetInfoW`, `SQLGetTypeInfo`, `SQLGetFunctions`.

use std::ffi::c_void;

use crate::backend::Backend;
use crate::errors::{IntoOdbc, OdbcError};
use crate::handles::ConnectionHandle;
use crate::handles::StatementData;
use crate::panic::panic_safe;
use crate::synthetic::SyntheticStatement;
use crate::types::{
    CREATE_PARAMS_LEN, CatalogResultColumnWidths, ColumnDescriptor, InfoValue, LITERAL_AFFIX_LEN,
    Nullable, SqlDataType, SqlReturn, character, identifier, integer, smallint,
};
use crate::utf16::write_utf16;

/// Returns the Driver-Manager-safe default response for an info type that the
/// backend does not specifically implement.
///
/// Tries [`Backend::get_info_raw`] first: it is where a backend answers any
/// info type that neither its own typed `get_info` nor [`default_get_info`]
/// has an arm for, whether that info type is genuinely absent from
/// `odbc_sys::InfoType` (e.g. `SQL_CURSOR_ROLLBACK_BEHAVIOR`) or is a real
/// `InfoType` variant that simply has no typed arm yet (e.g.
/// `SQL_FILE_USAGE`, `SQL_AGGREGATE_FUNCTIONS`). `get_info_raw` needs a live
/// connection, so pre-connect callers (`conn = None`) skip straight to the
/// range default below.
///
/// Falls back to `0xFFFFFFFF` ("all conversions supported") for the genuine
/// per-source-type `SQL_CONVERT_*` info types: `SQL_CONVERT_BIGINT` (53)
/// through `SQL_CONVERT_LONGVARBINARY` (71), `SQL_CONVERT_WCHAR` (122)
/// through `SQL_CONVERT_WVARCHAR` (126), and `SQL_CONVERT_GUID` (173), which
/// are numbered outside those two contiguous blocks.
///
/// For every other raw value that names a real `odbc_sys::InfoType` (i.e.
/// [`crate::types::info_type_from_raw`] recognises it), the fallback is
/// shape-aware: an empty string for a `String`-shaped info type, `U16(0)`
/// for a `U16`-shaped one, `U32(0)` for a `U32`-shaped one (see
/// [`crate::types::expected_kind`]). A raw value with no name at all has no
/// declared shape to honor and keeps the default `U32(0)`.
///
/// The `SQL_CONVERT_*` numeric range is not homogeneous, so the fallback
/// classifies it against `/usr/include/sqlext.h` rather than treating the
/// whole 48-73 span as "all conversions supported":
///
/// - 48 (`SQL_CONVERT_FUNCTIONS`) is a bitmask of `SQL_FN_CVT_CAST`/
///   `SQL_FN_CVT_CONVERT`: whether the driver supports the `CAST`
///   expression / `CONVERT` scalar function syntax at all, not a per-type
///   conversion map. It has an `odbc_sys::InfoType` variant
///   (`ConvertFunctions`) that backends answer honestly via
///   [`crate::backend::default_get_info`], so it does not reach this
///   fallback in practice; it is kept out of the "all supported" range
///   regardless, so a future named-but-unimplemented `ConvertFunctions`
///   arm would not silently degrade to "all bits set" either.
/// - 49-52 (`SQL_NUMERIC_FUNCTIONS`/`SQL_STRING_FUNCTIONS`/
///   `SQL_SYSTEM_FUNCTIONS`/`SQL_TIMEDATE_FUNCTIONS`) are scalar-function
///   bitmasks unrelated to `CONVERT`/`CAST`. Claiming "all supported" here is
///   an outright lie for a backend that has not implemented them (a BI tool
///   may then emit an unsupported `{fn SOUNDEX(x)}`); the honest, spec-safe
///   default is `0` ("no scalar functions"), same as any other unclassified
///   info type.
/// - 53-71 (`SQL_CONVERT_BIGINT` .. `SQL_CONVERT_LONGVARBINARY`) and 173
///   (`SQL_CONVERT_GUID`) are the actual per-source-type conversion-support
///   bitmaps (see
///   [`crate::types::SQL_CONVERT_FUNCTIONS_FIRST`]/[`crate::types::SQL_CONVERT_FUNCTIONS_LAST`]/
///   [`crate::types::SQL_CONVERT_GUID`]). These keep the `0xFFFFFFFF` default:
///   returning `0` for a genuine `SQL_CONVERT_*` type is what makes the
///   Windows Driver Manager block `SQLGetData` with `HYC00` (see
///   `AGENTS.md`'s Windows Driver Manager compatibility checklist), so this
///   default must never regress to `0` for any of these.
/// - 73 (`SQL_INTEGRITY`/`SQL_ODBC_SQL_OPT_IEF`) is a `Y`/`N` *string* info
///   type (whether the driver supports the ODBC Integrity Enhancement
///   Facility), not a conversion bitmap at all; it has an `InfoType`
///   variant (`Integrity`) that backends answer via `default_get_info`, so
///   like 48 it does not reach this fallback; it is kept out of the "all
///   supported" range regardless, since a `48..=73` bound would hand a
///   4-byte-integer `0xFFFFFFFF` to an application expecting a string if it
///   ever reached it.
///
/// Two further blocks share the same shape requirement, enumerated by the
/// info-type conformance test that iterates every `odbc_sys::InfoType` (see
/// [`crate::conformance`]):
///
/// - 122-126 (`SQL_CONVERT_WCHAR` .. `SQL_CONVERT_WVARCHAR`) are a *second*
///   contiguous block of genuine per-source-type `SQL_CONVERT_*` codes (see
///   [`crate::types::SQL_CONVERT_WCHAR`]); they must keep the `0xFFFFFFFF`
///   default for the same `HYC00` reason as 53-71/173, so the range check
///   below covers them explicitly.
/// - Every *other* named-but-unhandled `InfoType` (i.e. not a genuine
///   `SQL_CONVERT_*` code) gets a value in the shape the spec declares for
///   it, not a flat `U32(0)` regardless of shape — the same requirement as
///   `SQL_INTEGRITY` above. The fallback below is shape-aware for any info
///   type [`crate::types::info_type_from_raw`] recognises (see
///   [`crate::types::expected_kind`]), covering all 109 named info types.
///
/// Returning `SQL_ERROR` from `SQLGetInfoW` for an info type the backend
/// simply hasn't implemented corrupts the Windows Driver Manager's internal
/// state, so this function must always produce a value; the only `Err` it
/// can return is a genuine backend failure reported by `get_info_raw` itself.
///
/// # The `get_info_raw`-first ordering is load-bearing
///
/// `get_info_raw` MUST be tried before the numeric range defaults, never
/// after and never removed as a "simplification". A *named*
/// `odbc_sys::InfoType` variant may have no arm in [`default_get_info`] or in
/// a backend's own `get_info` match while still having a real value in
/// [`Backend::get_info_raw`]; for such a type this is the *only* place that
/// value is ever produced. `SQL_ATTR`-style capability bitmaps reach
/// applications this way.
///
/// If the ordering were reversed, or `get_info_raw` stopped being called
/// here, every such value would silently degrade to the generic `U32(0)` /
/// `0xFFFFFFFF` default while `SQLGetInfoW` kept returning `SQL_SUCCESS`;
/// nothing would surface the degradation. Applications that read capability
/// bitmaps to decide which operations can be pushed down to SQL, rather than
/// evaluated locally, would silently change plan.
///
/// Backends that rely on this must guard it: assert the exact expected value
/// for each affected info type through the full `sql_get_info_w` path, not by
/// calling `get_info_raw` directly, which would not exercise this ordering.
///
/// # The cursor-behaviour info types never take the generic default
///
/// `SQL_CURSOR_COMMIT_BEHAVIOR` (23) and `SQL_CURSOR_ROLLBACK_BEHAVIOR` (24)
/// are answered here from [`Backend::cursor_commit_behavior`] /
/// [`Backend::cursor_rollback_behavior`], the same hooks
/// [`crate::ffi::tran::sql_end_tran`] applies. Without that, a backend that
/// answers neither info type anywhere would fall through to the shape-aware
/// default and report `U16(0)` for 23 and `U32(0)` for 24 — both
/// `SQL_CB_DELETE`, the second in the wrong shape — while `sql_end_tran`
/// applied whatever the hooks actually declare. That mismatch is the exact
/// defect the hooks exist to prevent: an application that believes
/// `SQL_CB_DELETE` discards its statements' state per the `SQLEndTran`
/// transition table.
///
/// A backend can still deliberately override both, from its own typed
/// `get_info` match (for 23) or from [`Backend::get_info_raw`] (for either) —
/// `get_info_raw` is consulted above, before this special case — but it must
/// then keep the reported value and its hooks in sync itself.
fn info_type_default_response<B: Backend>(
    conn: Option<&B::Connection>,
    current_catalog: Option<&str>,
    info_type: u16,
) -> Result<InfoValue, OdbcError> {
    if let Some(conn) = conn
        && let Some(result) = B::get_info_raw(conn, info_type)
    {
        return result.into_odbc();
    }

    // Derived from the backend hook so that a backend which answers this info
    // type nowhere still reports what `sql_end_tran` actually does.
    // See "The cursor-behaviour info types never take the generic default".
    // `SQL_CURSOR_ROLLBACK_BEHAVIOR` gets the same treatment one step below,
    // via `common_get_info_raw`.
    if info_type == crate::types::SQL_CURSOR_COMMIT_BEHAVIOR {
        return Ok(InfoValue::U16(B::cursor_commit_behavior().as_u16()));
    }

    // Spec, `SQL_DATABASE_NAME`: "In ODBC 3.x, the value returned for this
    // InfoType can also be returned by calling SQLGetConnectAttr with an
    // Attribute argument of SQL_ATTR_CURRENT_CATALOG." So it is that attribute,
    // read back through a second name, and answering it from anywhere else
    // would give one fact two sources. The attribute lives on the connection
    // *handle*, which is why this cannot sit in `common_get_info_raw` with the
    // rest of the raw-path answers. A backend that knows the real current
    // database still wins: `get_info_raw` above runs first.
    if info_type == crate::types::SQL_DATABASE_NAME {
        return Ok(InfoValue::String(
            current_catalog.unwrap_or_default().to_string(),
        ));
    }

    // The shared raw-path answers, for a backend whose `get_info_raw` does not
    // delegate to `common_get_info_raw` (or does not exist). Without this,
    // "the info type has a correct shared value" and "this backend happens to
    // delegate" stay coupled: `SQL_ROW_UPDATES` and `SQL_PROCEDURES` would
    // fall to `U32(0)` for a Y/N string, and `SQL_QUOTED_IDENTIFIER_CASE` to
    // `U16(0)`, which is not one of the four `SQL_IC_*` values.
    if let Some(value) = crate::backend::common_get_info_raw::<B>(conn, info_type) {
        return Ok(value);
    }

    // Core's own typed answers, derived from the backend's capability
    // declarations. Reached only when nothing above answered, so a backend
    // still overrides everything here from its own `get_info` or
    // `get_info_raw`.
    if let Some(type_id) = crate::types::info_type_from_raw(info_type)
        && let Some(value) = crate::backend::default_get_info::<B>(conn, type_id)
    {
        return Ok(value);
    }

    use crate::types::{
        InfoValueKind, SQL_CONVERT_FUNCTIONS_FIRST, SQL_CONVERT_FUNCTIONS_LAST, SQL_CONVERT_GUID,
        SQL_CONVERT_WCHAR, SQL_CONVERT_WVARCHAR, expected_kind, info_type_from_raw,
    };
    let is_genuine_convert_info_type = (SQL_CONVERT_FUNCTIONS_FIRST..=SQL_CONVERT_FUNCTIONS_LAST)
        .contains(&info_type)
        || (SQL_CONVERT_WCHAR..=SQL_CONVERT_WVARCHAR).contains(&info_type)
        || info_type == SQL_CONVERT_GUID;

    if is_genuine_convert_info_type {
        tracing::debug!(
            "SQLGetInfoW: SQL_CONVERT_* info type {info_type}, returning all-supported"
        );
        return Ok(InfoValue::U32(u32::MAX));
    }

    // Four info types the spec declares `SQLUSMALLINT` that `odbc_sys::InfoType`
    // does not model, so `info_type_from_raw` cannot supply their shape below.
    //
    // Getting this wrong is a buffer overrun, not a cosmetic mismatch:
    // `SQLGetInfo`'s `BufferLength` is *ignored* for a non-string value — the
    // driver is required to assume the buffer matches the type the spec
    // declares — so answering `U32` here writes four bytes into the two an
    // application correctly allocated for a `SQLUSMALLINT`.
    //
    // Listed rather than shape-derived because there is nothing to derive from:
    // being absent from odbc-sys is precisely the problem. Keep in step with the
    // constants' own doc comments in `types/constants.rs`.
    const SMALLINT_SHAPED_UNMODELLED_INFO_TYPES: [u16; 4] = [
        crate::types::SQL_ODBC_API_CONFORMANCE,
        crate::types::SQL_ODBC_SAG_CLI_CONFORMANCE,
        crate::types::SQL_ODBC_SQL_CONFORMANCE,
        crate::types::SQL_MAX_PROCEDURE_NAME_LEN,
    ];
    if SMALLINT_SHAPED_UNMODELLED_INFO_TYPES.contains(&info_type) {
        tracing::debug!(
            "SQLGetInfoW: info type {info_type} is SQLUSMALLINT-shaped but unmodelled by \
             odbc-sys; defaulting to U16(0)"
        );
        return Ok(InfoValue::U16(0));
    }

    // A *named* InfoType that reaches here (because no backend answered it)
    // still gets a value in the shape the spec declares for it, never an
    // arbitrary U32 masquerading as e.g. a Y/N string (see the shape-aware
    // fallback documented above). A raw value with no name at all has no
    // declared shape to honor and keeps the default U32(0).
    let value = match info_type_from_raw(info_type).map(expected_kind) {
        Some(InfoValueKind::String) => InfoValue::String(String::new()),
        Some(InfoValueKind::U16) => InfoValue::U16(0),
        Some(InfoValueKind::U32) | None => InfoValue::U32(0),
    };
    tracing::debug!("SQLGetInfoW: info type {info_type} defaulting to {value:?}");
    Ok(value)
}

/// Runs the fallback in [`info_type_default_response`] when `result` is
/// `NotImplemented`, otherwise passes the value or error through unchanged.
///
/// This is what makes "the info type has a name" and "the backend answers it"
/// independent conditions: a named-but-unhandled `InfoType` must reach the
/// exact same benign default as an unnamed raw value, never `SQL_ERROR`. Any
/// other error variant is a genuine backend failure and must still propagate.
fn info_type_or_default<B: Backend>(
    result: Result<InfoValue, OdbcError>,
    conn: Option<&B::Connection>,
    current_catalog: Option<&str>,
    info_type: u16,
) -> Result<InfoValue, OdbcError> {
    match result {
        Ok(value) => Ok(value),
        Err(OdbcError::NotImplemented { .. }) => {
            info_type_default_response::<B>(conn, current_catalog, info_type)
        }
        Err(e) => Err(e),
    }
}

/// Generic implementation of SQLGetInfoW.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlgetinfo-function>
///
/// Returns information about the driver and data source associated with a
/// connection. The information is returned as a string, u16, or u32 value
/// depending on the `info_type`.
///
/// # Parameters
///
/// - `connection_handle`: Connection handle.
/// - `info_type`: Type of information requested (`SQL_*` constant, e.g. `SQL_DRIVER_NAME`).
/// - `info_value_ptr`: Output buffer for the value. For string info types this is a
///   `*mut u16` (UTF-16); for numeric types it is `*mut u16` or `*mut u32`.
/// - `buffer_length`: Size of `info_value_ptr` in bytes.
/// - `string_length_ptr`: On output, receives the number of bytes available to
///   return in `*info_value_ptr` (excluding the null terminator) for string types,
///   or 2/4 for numeric types.
///
/// # Spec compliance
///
/// - 01000 (general warning): not returned; driver-specific info messages not produced.
/// - 01004 (string truncated): returned by `write_utf16` when the output buffer is
///   smaller than the full value.
/// - 08003 (connection not open): (driver-manager-handled; not returned here)
/// - 08S01 (communication link failure): propagated from the backend via `OdbcError`.
/// - HY000 (general error): propagated via `OdbcError` for unclassified failures.
/// - HY001 (memory allocation error): not explicitly returned; Rust panics on OOM.
/// - HY010 (function sequence error): (driver-manager-handled; not returned here)
/// - HY013 (memory management error): not explicitly returned; Rust panics on OOM.
/// - HY024 (invalid attribute value for SQL_DRIVER_HSTMT/HDESC): (driver-manager-handled;
///   not returned here)
/// - HY090 (invalid buffer length): (driver-manager-handled; not returned here)
/// - HY096 (info type out of range): not returned; unknown info types return `U32(0)` for
///   DM compatibility (returning SQL_ERROR corrupts the Windows DM's internal state).
/// - HY117 (connection suspended): (driver-manager-handled; not returned here)
/// - HYC00 (optional feature not implemented): not returned here. A recognised `InfoType`
///   the backend reports as `OdbcError::NotImplemented` falls through to the same
///   DM-safe default as an unnamed info type (see `info_type_or_default`) rather than
///   surfacing HYC00 — naming a raw value must never be what turns a benign default
///   into an error.
/// - HYT01 (connection timeout): propagated from the backend via `OdbcError`.
/// - IM001 (driver does not support function): (driver-manager-handled; not returned here)
///
/// # Safety
///
/// `connection_handle` must point to a valid `ConnectionHandle<B>`.
/// `info_value_ptr` must be a valid writable buffer of at least `buffer_length` bytes
/// (for string types) or large enough for u16/u32.
pub unsafe fn sql_get_info_w<B: Backend>(
    connection_handle: *mut c_void,
    info_type: u16,
    info_value_ptr: *mut c_void,
    buffer_length: i16,
    string_length_ptr: *mut i16,
) -> SqlReturn {
    tracing::trace!(
        "SQLGetInfoW(conn={:?}, info_type_raw={})",
        connection_handle,
        info_type
    );
    let info_type_id = crate::types::info_type_from_raw(info_type).ok_or(());
    tracing::debug!(
        "SQLGetInfoW(conn={:?}, info_type={:?} ({}))",
        connection_handle,
        info_type_id,
        info_type
    );
    // SAFETY: connection_handle is null or a valid ConnectionHandle<B> allocated by
    // sql_alloc_handle. scope.get validates kind and group before any cast, and
    // panic_safe catches any panics.
    let ret = unsafe {
        panic_safe::<B, _>(connection_handle, |scope| {
            let handle = scope.get::<ConnectionHandle<B>>(connection_handle)?;
            handle.diagnostics.clear();

            // Some info types (SQL_DRIVER_ODBC_VER, SQL_DRIVER_NAME, etc.) are
            // queried by the Driver Manager before the connection is established.
            // Try the connected path first; fall back to get_info_pre_connect.
            //
            // An info type can reach the DM-safe default via two different
            // routes: it may have no name at all (`info_type_id == Err(())`), or
            // it may be a perfectly good `InfoType` that this backend simply
            // hasn't implemented (`B::get_info` / `get_info_pre_connect` return
            // `NotImplemented`). Both routes must produce the same benign
            // answer (see `info_type_default_response`): naming a raw value must
            // never turn a benign default into `SQL_ERROR`.
            // `SQL_ATTR_CURRENT_CATALOG`, which the spec makes the same value
            // as `SQL_DATABASE_NAME`. Read before the dispatch below, which
            // borrows the handle's diagnostics mutably.
            // What the application set, else what the session is actually
            // using — the same two sources, in the same order, that
            // `SQLGetConnectAttr(SQL_ATTR_CURRENT_CATALOG)` reads. The spec
            // makes these one value, so they must not consult different things.
            let current_catalog = handle
                .attr_strings
                .get(&odbc_sys::ConnectionAttribute::CURRENT_CATALOG.0)
                .cloned()
                .or_else(|| {
                    handle
                        .connection
                        .as_ref()
                        .and_then(|c| B::current_catalog(c))
                        .map(std::borrow::Cow::into_owned)
                });
            let current_catalog = current_catalog.as_deref();
            let info = match handle.connection.as_ref() {
                Some(conn) => match info_type_id {
                    Ok(type_id) => info_type_or_default::<B>(
                        B::get_info(conn, type_id).into_odbc(),
                        Some(conn),
                        current_catalog,
                        info_type,
                    )?,
                    Err(()) => {
                        info_type_default_response::<B>(Some(conn), current_catalog, info_type)?
                    }
                },
                None => match info_type_id {
                    Ok(type_id) => info_type_or_default::<B>(
                        B::get_info_pre_connect(type_id).into_odbc(),
                        None,
                        current_catalog,
                        info_type,
                    )?,
                    Err(()) => info_type_default_response::<B>(None, current_catalog, info_type)?,
                },
            };

            match info {
                InfoValue::String(s) => {
                    // buffer_length is in bytes; write_utf16 takes u16 units.
                    let buf_len_u16 = buffer_length / 2;
                    // write_utf16 writes its length output through a plain (aligned)
                    // dereference, so, like write_diag_string in diag.rs, pass it a
                    // local instead of the caller's pointer, which is not guaranteed to
                    // be aligned (row-wise-bound applications pass pointers at arbitrary
                    // offsets into a packed buffer).
                    let mut units: i16 = 0;
                    let ret = crate::utf16::note_truncation(
                        write_utf16(&s, info_value_ptr as *mut u16, buf_len_u16, &mut units),
                        &mut handle.diagnostics,
                    );
                    // Spec: SQLGetInfoW reports StringLengthPtr in bytes,
                    // but write_utf16 reports in u16 units. Convert.
                    if !string_length_ptr.is_null() {
                        let bytes = i16::try_from(i32::from(units) * 2).unwrap_or_else(|_| {
                            tracing::warn!(
                                "SQLGetInfoW: byte length for {} code units overflows i16, saturating to i16::MAX",
                                units
                            );
                            i16::MAX
                        });
                        // SAFETY: non-null checked above; caller guarantees a valid
                        // writable i16, but not necessarily an aligned one.
                        std::ptr::write_unaligned(string_length_ptr, bytes);
                    }
                    Ok(ret)
                }
                InfoValue::U16(v) => {
                    if !info_value_ptr.is_null() {
                        // SAFETY: non-null checked above; caller guarantees the buffer
                        // is large enough for a u16. write_unaligned is used because
                        // some ODBC applications pass misaligned pointers.
                        (info_value_ptr as *mut u16).write_unaligned(v);
                    }
                    if !string_length_ptr.is_null() {
                        // SAFETY: non-null checked above; caller guarantees writable i16,
                        // not necessarily aligned.
                        std::ptr::write_unaligned(string_length_ptr, 2);
                    }
                    Ok(SqlReturn::SUCCESS)
                }
                InfoValue::U32(v) => {
                    if !info_value_ptr.is_null() {
                        // SAFETY: non-null checked above; caller guarantees the buffer
                        // is large enough for a u32. write_unaligned is used because
                        // some ODBC applications pass non-4-byte-aligned buffers.
                        (info_value_ptr as *mut u32).write_unaligned(v);
                    }
                    if !string_length_ptr.is_null() {
                        // SAFETY: non-null checked above; caller guarantees writable i16,
                        // not necessarily aligned.
                        std::ptr::write_unaligned(string_length_ptr, 4);
                    }
                    Ok(SqlReturn::SUCCESS)
                }
            }
        })
    };
    tracing::debug!("SQLGetInfoW -> {:?}", ret);
    ret
}

/// Build the 19 standard column descriptors for the SQLGetTypeInfo result set.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlgettypeinfo-function>
///
/// "not NULL" per the spec's column table: `TYPE_NAME` (1), `DATA_TYPE` (2),
/// `NULLABLE` (7), `CASE_SENSITIVE` (8), `SEARCHABLE` (9),
/// `FIXED_PREC_SCALE` (11) and `SQL_DATA_TYPE` (16).
///
/// The character columns split into two groups. `TYPE_NAME` and
/// `LOCAL_TYPE_NAME` name a data-source type, which is an identifier in the
/// data source's own namespace — the same quantity `SQLColumns.TYPE_NAME`
/// already reports — so they take `identifier_len`. `LITERAL_PREFIX`,
/// `LITERAL_SUFFIX` and `CREATE_PARAMS` hold literal syntax fragments rather
/// than names; they are not bounded by the data source's identifier limit and
/// so take their own spec-independent widths.
pub(crate) fn type_info_columns(widths: &CatalogResultColumnWidths) -> Vec<ColumnDescriptor> {
    vec![
        identifier("TYPE_NAME", widths, Nullable::SqlNoNulls),
        smallint("DATA_TYPE", Nullable::SqlNoNulls),
        integer("COLUMN_SIZE", Nullable::SqlNullable),
        character(
            "LITERAL_PREFIX",
            LITERAL_AFFIX_LEN,
            widths,
            Nullable::SqlNullable,
        ),
        character(
            "LITERAL_SUFFIX",
            LITERAL_AFFIX_LEN,
            widths,
            Nullable::SqlNullable,
        ),
        character(
            "CREATE_PARAMS",
            CREATE_PARAMS_LEN,
            widths,
            Nullable::SqlNullable,
        ),
        smallint("NULLABLE", Nullable::SqlNoNulls),
        smallint("CASE_SENSITIVE", Nullable::SqlNoNulls),
        smallint("SEARCHABLE", Nullable::SqlNoNulls),
        smallint("UNSIGNED_ATTRIBUTE", Nullable::SqlNullable),
        smallint("FIXED_PREC_SCALE", Nullable::SqlNoNulls),
        smallint("AUTO_UNIQUE_VALUE", Nullable::SqlNullable),
        identifier("LOCAL_TYPE_NAME", widths, Nullable::SqlNullable),
        smallint("MINIMUM_SCALE", Nullable::SqlNullable),
        smallint("MAXIMUM_SCALE", Nullable::SqlNullable),
        smallint("SQL_DATA_TYPE", Nullable::SqlNoNulls),
        smallint("SQL_DATETIME_SUB", Nullable::SqlNullable),
        integer("NUM_PREC_RADIX", Nullable::SqlNullable),
        smallint("INTERVAL_PRECISION", Nullable::SqlNullable),
    ]
}

/// Generic implementation of SQLGetTypeInfo.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlgettypeinfo-function>
///
/// Populates the statement handle with a synthetic result set containing type
/// information. If `data_type` is `SQL_ALL_TYPES` (0), returns all types.
/// Otherwise filters to matching types.
///
/// # Parameters
///
/// - `statement_handle`: Statement handle for the result set.
/// - `data_type`: The SQL data type to query, or `SQL_ALL_TYPES` (0) to return all types.
///   Must be a valid ODBC SQL data type identifier or a driver-specific data type identifier.
///
/// # Spec compliance
///
/// - 01000 (general warning): not returned; driver does not produce informational messages here.
/// - 01S02 (option value changed): (driver-manager-handled; not returned here)
/// - 08S01 (communication link failure): propagated from the backend via `OdbcError`.
/// - 24000 (invalid cursor state): (driver-manager-handled; not returned here)
/// - 40001 (serialization failure): propagated from the backend via `OdbcError` if applicable.
/// - 40003 (statement completion unknown): propagated from the backend via `OdbcError`.
/// - HY000 (general error): propagated via `OdbcError` for unclassified failures.
/// - HY001 (memory allocation error): not explicitly returned; Rust panics on OOM.
/// - HY004 (invalid SQL data type): invalid `data_type` values produce an empty result set
///   rather than SQL_ERROR/HY004. This is a common driver behavior: the result set column
///   structure is the same regardless of the filter, and returning an empty set is less
///   disruptive than rejecting values that may be driver-specific extensions.
/// - HY008: Operation canceled; not returned here. This call makes no fallible backend call —
///   `Backend::get_type_info` returns its rows infallibly, as a `Cow` — so there is no error for a
///   cancellation to be reported through. The asynchronous clause is inapplicable: core never
///   returns `SQL_STILL_EXECUTING`.
/// - HY010 (function sequence error): (driver-manager-handled; not returned here)
/// - HY013 (memory management error): not explicitly returned; Rust panics on OOM.
/// - HY117 (connection suspended): (driver-manager-handled; not returned here)
/// - HYC00 (optional feature not implemented): not returned; the full type-info result set
///   is always built in-driver without cursor/bookmark attributes.
/// - HYT00 (timeout expired): not returned; result set is built synchronously in-driver.
/// - HYT01 (connection timeout): propagated from the backend via `OdbcError`.
/// - IM001 (driver does not support function): (driver-manager-handled; not returned here)
/// - IM017 (polling disabled): (driver-manager-handled; not returned here)
/// - IM018 (SQLCompleteAsync not called): (driver-manager-handled; not returned here)
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
pub unsafe fn sql_get_type_info<B: Backend>(
    statement_handle: *mut c_void,
    data_type: i16,
) -> SqlReturn {
    let data_type_val = SqlDataType(data_type);
    tracing::debug!(
        "SQLGetTypeInfo(stmt={:?}, data_type={:?})",
        statement_handle,
        data_type_val
    );
    // SAFETY: statement_handle is null or a valid StatementHandle<B> allocated by
    // sql_alloc_handle. scope.stmt_with_parent validates kind and group for both
    // the statement and its parent connection before any cast, and
    // panic_safe catches any panics.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, |scope| {
            let (handle, conn) = scope.stmt_with_parent::<B>(statement_handle)?;
            handle.diagnostics.clear();

            // The type list is the data source's, so it comes from the
            // statement's connection.
            let Some(ref connection) = conn.connection else {
                return Err(OdbcError::general(
                    "Connection is not open",
                    crate::types::SqlState::function_sequence_error(),
                ));
            };
            let all_types = B::get_type_info(connection);

            // Filter by data_type if not SQL_ALL_TYPES (SqlDataType::UNKNOWN_TYPE = 0)
            let mut selected: Vec<_> = all_types
                .iter()
                .filter(|t| {
                    data_type_val == SqlDataType::UNKNOWN_TYPE || t.data_type == data_type_val
                })
                .collect();

            // Spec: "the result set is ordered by DATA_TYPE and then by how
            // closely the data type maps to the corresponding ODBC SQL data
            // type". Core cannot rank closeness of mapping, so it orders by
            // TYPE_NAME within a DATA_TYPE, which is stable and total. Sorted
            // here rather than left to the backend so that every driver's
            // result set is ordered, and ordered the same way — an application
            // picking "the first row for this DATA_TYPE" as the preferred type
            // otherwise gets whatever order the backend happened to declare.
            //
            // `sort_by` is stable, so a backend that has deliberately put its
            // preferred type first among several sharing a name keeps that
            // order.
            selected.sort_by(|a, b| {
                a.data_type
                    .0
                    .cmp(&b.data_type.0)
                    .then_with(|| a.type_name.cmp(&b.type_name))
            });

            let rows: Vec<_> = selected.iter().map(|t| t.to_column_values()).collect();

            let columns = type_info_columns(&B::catalog_result_column_widths());
            let synthetic = SyntheticStatement::new(columns, rows);
            handle.set_result_set(StatementData::Synthetic(synthetic));

            Ok(SqlReturn::SUCCESS)
        })
    };
    tracing::debug!("SQLGetTypeInfo -> {:?}", ret);
    ret
}

/// Generic implementation of SQLGetFunctions.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlgetfunctions-function>
///
/// When `function_id` is `SQL_API_ODBC3_ALL_FUNCTIONS` (999), writes a 4000-bit
/// bitmap (250 u16 values) to `supported_ptr`. When `function_id` is
/// `SQL_API_ALL_FUNCTIONS` (0), writes a 100-element `u16` array where each
/// index is `SQL_TRUE`/`SQL_FALSE`. When `function_id` is a single function ID,
/// writes `SQL_TRUE` (1) or `SQL_FALSE` (0) to `*supported_ptr`.
///
/// # Parameters
///
/// - `connection_handle`: Connection handle.
/// - `function_id`: Identifies the function(s) to query. Use
///   `SQL_API_ODBC3_ALL_FUNCTIONS` (999) for the ODBC 3.x bitmap,
///   `SQL_API_ALL_FUNCTIONS` (0) for the ODBC 2.x array, or a specific
///   `SQL_API_*` constant for a single-function query.
/// - `supported_ptr`: Output pointer. For the bitmap query it must point to an
///   array of at least `SQL_API_ODBC3_ALL_FUNCTIONS_SIZE` (250) `u16` values;
///   for the 2.x array at least 100 `u16` values; for a single query a single
///   `u16`.
///
/// # Spec compliance
///
/// - 01000 (general warning): not returned; no driver-specific informational messages.
/// - 08S01 (communication link failure): propagated from the backend via `OdbcError`.
/// - HY000 (general error): propagated via `OdbcError` for unclassified failures.
/// - HY001 (memory allocation error): not explicitly returned; Rust panics on OOM.
/// - HY010 (function sequence error — called before connect): (driver-manager-handled;
///   not returned here)
/// - HY013 (memory management error): not explicitly returned; Rust panics on OOM.
/// - HY095 (function type out of range — invalid FunctionId): not returned as
///   SQL_ERROR; unknown single function IDs resolve to `SQL_FALSE` (unsupported),
///   which is safe and compatible with DM behavior.
/// - HY117 (connection suspended): (driver-manager-handled; not returned here)
/// - HYT01 (connection timeout): propagated from the backend via `OdbcError`.
///
/// # Safety
///
/// `connection_handle` must point to a valid `ConnectionHandle<B>`.
/// `supported_ptr` must be a valid writable pointer. For `function_id == 999` it
/// must point to at least 250 u16 values; for `function_id == 0` at least 100.
pub unsafe fn sql_get_functions<B: Backend>(
    connection_handle: *mut c_void,
    function_id: u16,
    supported_ptr: *mut u16,
) -> SqlReturn {
    tracing::trace!(
        "SQLGetFunctions(conn={:?}, function_id_raw={})",
        connection_handle,
        function_id
    );
    let func_id_log = crate::function_id::function_id_from_raw(function_id);
    tracing::debug!(
        "SQLGetFunctions(conn={:?}, function_id={:?} ({}))",
        connection_handle,
        func_id_log,
        function_id
    );
    // SAFETY: connection_handle is null or a valid ConnectionHandle<B> allocated by
    // sql_alloc_handle. scope.get validates kind and group before any cast, and
    // panic_safe catches any panics.
    let ret = unsafe {
        panic_safe::<B, _>(connection_handle, |scope| {
            let handle = scope.get::<ConnectionHandle<B>>(connection_handle)?;
            handle.diagnostics.clear();

            let functions = B::get_functions();

            use crate::function_id::{
                SQL_API_ODBC3_ALL_FUNCTIONS, SQL_API_ODBC3_ALL_FUNCTIONS_SIZE,
            };

            const SQL_API_ALL_FUNCTIONS: u16 = 0;
            const SQL_API_ALL_FUNCTIONS_SIZE: usize = 100;

            if function_id == SQL_API_ODBC3_ALL_FUNCTIONS {
                // ODBC 3.x bitmap: 250 u16 values, each bit = one function ID.
                if !supported_ptr.is_null() {
                    // SAFETY: supported_ptr is non-null (checked); caller guarantees it points to a
                    // contiguous array of SQL_API_ODBC3_ALL_FUNCTIONS_SIZE (250) writable u16 values
                    // as required by the ODBC 3.x bitmap spec.
                    // Assembled locally, then copied out byte-wise below.
                    // `from_raw_parts_mut` would require `supported_ptr` to be
                    // u16-aligned, which an application-supplied pointer does
                    // not guarantee.
                    let mut supported = [0u16; SQL_API_ODBC3_ALL_FUNCTIONS_SIZE];
                    for &func in functions.iter() {
                        let fid = func as u16;
                        let idx = (fid / 16) as usize;
                        let bit = fid % 16;
                        if idx < SQL_API_ODBC3_ALL_FUNCTIONS_SIZE {
                            supported[idx] |= 1 << bit;
                        }
                    }
                    // SAFETY: supported_ptr is non-null (checked above) and the
                    // caller guarantees the documented element count; u8 has
                    // alignment 1, so this carries no alignment requirement.
                    std::ptr::copy_nonoverlapping(
                        supported.as_ptr().cast::<u8>(),
                        supported_ptr.cast::<u8>(),
                        supported.len() * size_of::<u16>(),
                    );
                }
            } else if function_id == SQL_API_ALL_FUNCTIONS {
                // ODBC 2.x array: 100 u16 values, array[func_id] = SQL_TRUE/SQL_FALSE.
                // Only mark actually-supported functions. The `functions` list
                // uses ODBC 3.x IDs (1000+) for 3.x-only functions, so we must
                // also set the deprecated 2.x equivalents when the 3.x function
                // is present; the Windows DM checks the 2.x array for dispatch.
                use crate::function_id::FunctionId as F;

                // Value written into the SQL_API_ALL_FUNCTIONS array to indicate
                // a function is supported (SQL_TRUE = 1).
                const SQL_FUNC_EXISTS: u16 = 1;

                if !supported_ptr.is_null() {
                    // SAFETY: supported_ptr is non-null (checked); caller guarantees it points to a
                    // contiguous array of SQL_API_ALL_FUNCTIONS_SIZE (100) writable u16 values.
                    // All index accesses below are bounds-checked against SQL_API_ALL_FUNCTIONS_SIZE.
                    // Assembled locally then copied out, for the same
                    // alignment reason as the 3.x bitmap above.
                    let mut supported = [0u16; SQL_API_ALL_FUNCTIONS_SIZE];
                    for &func in functions.iter() {
                        let fid = usize::from(func as u16);
                        if fid < SQL_API_ALL_FUNCTIONS_SIZE {
                            supported[fid] = SQL_FUNC_EXISTS;
                        }
                    }
                    // Map 3.x-only IDs to their deprecated 2.x equivalents.
                    if functions.contains(&F::AllocHandle) {
                        supported[usize::from(F::AllocConnect as u16)] = SQL_FUNC_EXISTS;
                        supported[usize::from(F::AllocEnv as u16)] = SQL_FUNC_EXISTS;
                        supported[usize::from(F::AllocStmt as u16)] = SQL_FUNC_EXISTS;
                    }
                    if functions.contains(&F::FreeHandle) {
                        // FreeConnect (2.x ID 14), FreeEnv (15), FreeStmt (16)
                        // FreeStmt is already in the list; 14/15 are deprecated
                        // but the DM may check them.
                        supported[usize::from(F::FreeConnect as u16)] = SQL_FUNC_EXISTS;
                        supported[usize::from(F::FreeEnv as u16)] = SQL_FUNC_EXISTS;
                    }
                    if functions.contains(&F::GetConnectAttr) {
                        supported[usize::from(F::GetConnectOption as u16)] = SQL_FUNC_EXISTS;
                    }
                    if functions.contains(&F::SetConnectAttr) {
                        supported[usize::from(F::SetConnectOption as u16)] = SQL_FUNC_EXISTS;
                    }
                    if functions.contains(&F::GetStmtAttr) {
                        supported[usize::from(F::GetStmtOption as u16)] = SQL_FUNC_EXISTS;
                    }
                    if functions.contains(&F::SetStmtAttr) {
                        supported[usize::from(F::SetStmtOption as u16)] = SQL_FUNC_EXISTS;
                    }
                    if functions.contains(&F::GetDiagRec) {
                        supported[usize::from(F::Error as u16)] = SQL_FUNC_EXISTS;
                    }
                    if functions.contains(&F::EndTran) {
                        supported[usize::from(F::Transact as u16)] = SQL_FUNC_EXISTS;
                    }
                    // CloseCursor's 2.x equivalent is FreeStmt(SQL_CLOSE) (handled above).

                    // SAFETY: supported_ptr is non-null (checked above) and the
                    // caller guarantees SQL_API_ALL_FUNCTIONS_SIZE writable u16
                    // values; u8 has alignment 1, so this carries no alignment
                    // requirement.
                    std::ptr::copy_nonoverlapping(
                        supported.as_ptr().cast::<u8>(),
                        supported_ptr.cast::<u8>(),
                        supported.len() * size_of::<u16>(),
                    );
                }
            } else {
                // Single function query: convert to FunctionId for comparison
                let queried = crate::function_id::function_id_from_raw(function_id);
                let is_supported = queried.is_some_and(|q| functions.contains(&q));
                tracing::debug!(
                    "SQLGetFunctions: single query func_id={} -> {:?} supported={}",
                    function_id,
                    queried,
                    is_supported
                );
                if !supported_ptr.is_null() {
                    // SAFETY: non-null checked above; caller guarantees writable u16
                    std::ptr::write_unaligned(supported_ptr, u16::from(is_supported));
                }
            }

            Ok(SqlReturn::SUCCESS)
        })
    };
    tracing::debug!("SQLGetFunctions -> {:?}", ret);
    ret
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffi::handle::{sql_alloc_handle, sql_free_handle};
    use crate::handles::StatementHandle;
    use crate::test_utils::{
        MockBackend, MockFunctionsBackend, MockTxnDeleteCloseBackend, with_handle,
    };

    /// Drives `SQLGetFunctions` for the ODBC 2.x `SQL_API_ALL_FUNCTIONS` array.
    fn all_functions_2x<B: Backend>() -> [u16; 100] {
        let mut buf = [0u16; 100];
        unsafe {
            // Allocated inline rather than via `alloc_env_and_conn`, which is
            // fixed to `MockBackend`. `SQLGetFunctions` needs only an allocated
            // connection, not a connected one.
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<B>(HandleType::Env as i16, std::ptr::null_mut(), &mut env);
            let mut conn: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<B>(HandleType::Dbc as i16, env, &mut conn);

            let ret = sql_get_functions::<B>(conn, 0, buf.as_mut_ptr());
            assert_eq!(ret, crate::types::SqlReturn::SUCCESS);
            let _ = sql_free_handle::<B>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<B>(HandleType::Env as i16, env);
        }
        buf
    }

    #[test]
    fn all_functions_2x_marks_the_deprecated_equivalents_at_their_spec_ids() {
        use crate::function_id::FunctionId as F;
        let buf = all_functions_2x::<MockFunctionsBackend>();

        // SQLGetConnectOption is 42. It was recorded at 30 — an unassigned
        // slot — so the Windows DM, which dispatches from this array, was told
        // a function the driver exports did not exist.
        for (id, label) in [
            (F::GetConnectOption, "SQLGetConnectOption"),
            (F::SetConnectOption, "SQLSetConnectOption"),
            (F::GetStmtOption, "SQLGetStmtOption"),
            (F::SetStmtOption, "SQLSetStmtOption"),
            (F::Error, "SQLError"),
            (F::Transact, "SQLTransact"),
            (F::FreeConnect, "SQLFreeConnect"),
            (F::FreeEnv, "SQLFreeEnv"),
        ] {
            assert_eq!(
                buf[id as usize], 1,
                "{label} (id {}) not marked present in the 2.x array",
                id as u16
            );
        }

        assert_eq!(
            buf[30], 0,
            "slot 30 is not an assigned SQL_API_* id and must stay clear"
        );
    }
    use crate::types::{
        InfoType, SQL_CB_CLOSE, SQL_CB_DELETE, SQL_CURSOR_COMMIT_BEHAVIOR,
        SQL_CURSOR_ROLLBACK_BEHAVIOR,
    };
    use odbc_sys::HandleType;

    /// Helper: allocate env + connection handles.
    unsafe fn alloc_env_and_conn() -> (*mut c_void, *mut c_void) {
        let mut env: *mut c_void = std::ptr::null_mut();
        let _ = unsafe {
            sql_alloc_handle::<MockBackend>(HandleType::Env as i16, std::ptr::null_mut(), &mut env)
        };
        let mut conn: *mut c_void = std::ptr::null_mut();
        let _ = unsafe { sql_alloc_handle::<MockBackend>(HandleType::Dbc as i16, env, &mut conn) };
        (env, conn)
    }

    /// Helper: allocate env + connection + statement handles.
    unsafe fn alloc_env_conn_stmt() -> (*mut c_void, *mut c_void, *mut c_void) {
        let (env, conn) = unsafe { alloc_env_and_conn() };
        let mut stmt: *mut c_void = std::ptr::null_mut();
        let _ =
            unsafe { sql_alloc_handle::<MockBackend>(HandleType::Stmt as i16, conn, &mut stmt) };
        (env, conn, stmt)
    }

    /// Generic counterparts of the `MockBackend`-fixed helpers above, for tests
    /// that need a different backend.
    /// Allocates the handle chain and connects it. `SQLGetTypeInfo` reports the
    /// data source's types, so it needs an open connection to read them from.
    unsafe fn alloc_env_conn_stmt_for<B: Backend>() -> (*mut c_void, *mut c_void, *mut c_void) {
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<B>(HandleType::Env as i16, std::ptr::null_mut(), &mut env);
            let mut conn: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<B>(HandleType::Dbc as i16, env, &mut conn);
            let wide: Vec<u16> = "Host=localhost;Database=test".encode_utf16().collect();
            assert_eq!(
                crate::ffi::connect::sql_driver_connect_w::<B>(
                    conn,
                    std::ptr::null_mut(),
                    wide.as_ptr(),
                    wide.len() as i16,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    0,
                ),
                SqlReturn::SUCCESS,
            );
            let mut stmt: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<B>(HandleType::Stmt as i16, conn, &mut stmt);
            (env, conn, stmt)
        }
    }

    unsafe fn cleanup_for<B: Backend>(env: *mut c_void, conn: *mut c_void, stmt: *mut c_void) {
        unsafe {
            let _ = sql_free_handle::<B>(HandleType::Stmt as i16, stmt);
            // A connected handle cannot be freed.
            let _ = crate::ffi::connect::sql_disconnect::<B>(conn);
            let _ = sql_free_handle::<B>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<B>(HandleType::Env as i16, env);
        }
    }

    /// Helper: connect a handle using a valid connection string. `MockBackend::connect`
    /// always succeeds, so this establishes `handle.connection = Some(_)`, putting
    /// `sql_get_info_w` on the connected (`B::get_info`) path rather than the
    /// pre-connect (`B::get_info_pre_connect`) path.
    unsafe fn connect_handle(conn: *mut c_void) -> SqlReturn {
        let input = "Host=localhost;Port=8080;Database=test;User=me";
        let wide: Vec<u16> = input.encode_utf16().collect();
        unsafe {
            crate::ffi::connect::sql_driver_connect_w::<MockBackend>(
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

    unsafe fn cleanup(env: *mut c_void, conn: *mut c_void, stmt: *mut c_void) {
        unsafe {
            let _ = sql_free_handle::<MockBackend>(HandleType::Stmt as i16, stmt);
            let _ = crate::ffi::connect::sql_disconnect::<MockBackend>(conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    /// Reads a `SQLUSMALLINT`-shaped info type through the full
    /// `sql_get_info_w` path, asserting that exactly 2 bytes were written and
    /// that `StringLengthPtr` reports 2. A sentinel-filled buffer makes a
    /// wrong-shape answer (4 bytes for a `U32`) fail loudly instead of being
    /// silently truncated by the read.
    unsafe fn read_u16_info<B: Backend>(conn: *mut c_void, info_type: u16, what: &str) -> u16 {
        const SENTINEL: u8 = 0xEE;
        let mut buf = [SENTINEL; 8];
        let mut str_len: i16 = -1;
        let ret = unsafe {
            sql_get_info_w::<B>(
                conn,
                info_type,
                buf.as_mut_ptr() as *mut c_void,
                8,
                &mut str_len,
            )
        };
        assert_eq!(ret, SqlReturn::SUCCESS, "{what}");
        assert_eq!(
            str_len, 2,
            "{what} must be SQLUSMALLINT-shaped (2 bytes) per the SQLGetInfo spec"
        );
        assert_eq!(buf[2..], [SENTINEL; 6], "{what} wrote past 2 bytes");
        u16::from_ne_bytes([buf[0], buf[1]])
    }

    /// Four info types the spec declares `SQLUSMALLINT` have no
    /// `odbc_sys::InfoType` variant, so nothing gave the shape-aware fallback a
    /// shape to honour and they took the generic `U32(0)`.
    ///
    /// That is a buffer overrun rather than a cosmetic mismatch: `SQLGetInfo`
    /// *ignores* `BufferLength` for a non-string value — the driver must assume
    /// the buffer matches the type the spec declares — so four bytes land in the
    /// two an application correctly allocated. `read_u16_info`'s sentinel is
    /// what catches the two extra bytes; asserting on `StringLengthPtr` alone
    /// would not, since a `U32` answer reports a plausible `4`.
    #[test]
    fn smallint_shaped_info_types_unmodelled_by_odbc_sys_still_answer_in_two_bytes() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = connect_handle(conn);
            assert_eq!(ret, SqlReturn::SUCCESS);

            for (info_type, name) in [
                (
                    crate::types::SQL_ODBC_API_CONFORMANCE,
                    "SQL_ODBC_API_CONFORMANCE",
                ),
                (
                    crate::types::SQL_ODBC_SAG_CLI_CONFORMANCE,
                    "SQL_ODBC_SAG_CLI_CONFORMANCE",
                ),
                (
                    crate::types::SQL_ODBC_SQL_CONFORMANCE,
                    "SQL_ODBC_SQL_CONFORMANCE",
                ),
                (
                    crate::types::SQL_MAX_PROCEDURE_NAME_LEN,
                    "SQL_MAX_PROCEDURE_NAME_LEN",
                ),
            ] {
                let _ = read_u16_info::<MockBackend>(conn, info_type, name);
            }

            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn cursor_behavior_info_types_follow_the_hooks_when_no_backend_answers_them() {
        // `MockTxnDeleteCloseBackend::get_info` returns an error for every info
        // type and the backend does not override `get_info_raw`, so it answers
        // SQL_CURSOR_COMMIT_BEHAVIOR (23) and SQL_CURSOR_ROLLBACK_BEHAVIOR (24)
        // *nowhere* — the state a driver is in when it adds transaction support
        // and follows core's defaults. Without the special case in
        // `info_type_default_response` these fall through to U16(0) and U32(0),
        // both SQL_CB_DELETE (and the second in the wrong shape), while
        // `sql_end_tran` applies whatever the hooks declare. That is the
        // original defect: an application told SQL_CB_DELETE discards its
        // statements' state per the SQLEndTran transition table.
        //
        // This backend declares Delete for commit and Close for rollback, so a
        // regression to a hardcoded 0 is distinguishable on the rollback leg.
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockTxnDeleteCloseBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );
            let mut conn: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockTxnDeleteCloseBackend>(
                HandleType::Dbc as i16,
                env,
                &mut conn,
            );

            // Pre-connect: the Windows DM queries info types before
            // SQLDriverConnectW, and `get_info_raw` is not consulted at all
            // there, so the fallback is the only thing that can answer.
            assert_eq!(
                read_u16_info::<MockTxnDeleteCloseBackend>(
                    conn,
                    SQL_CURSOR_COMMIT_BEHAVIOR,
                    "SQL_CURSOR_COMMIT_BEHAVIOR (pre-connect)"
                ),
                SQL_CB_DELETE
            );
            assert_eq!(
                read_u16_info::<MockTxnDeleteCloseBackend>(
                    conn,
                    SQL_CURSOR_ROLLBACK_BEHAVIOR,
                    "SQL_CURSOR_ROLLBACK_BEHAVIOR (pre-connect)"
                ),
                SQL_CB_CLOSE
            );

            let wide: Vec<u16> = "DRIVER=mock;".encode_utf16().collect();
            let ret = crate::ffi::connect::sql_driver_connect_w::<MockTxnDeleteCloseBackend>(
                conn,
                std::ptr::null_mut(),
                wide.as_ptr(),
                wide.len() as i16,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            assert_eq!(
                read_u16_info::<MockTxnDeleteCloseBackend>(
                    conn,
                    SQL_CURSOR_COMMIT_BEHAVIOR,
                    "SQL_CURSOR_COMMIT_BEHAVIOR (connected)"
                ),
                SQL_CB_DELETE,
                "SQL_CURSOR_COMMIT_BEHAVIOR ignored Backend::cursor_commit_behavior"
            );
            assert_eq!(
                read_u16_info::<MockTxnDeleteCloseBackend>(
                    conn,
                    SQL_CURSOR_ROLLBACK_BEHAVIOR,
                    "SQL_CURSOR_ROLLBACK_BEHAVIOR (connected)"
                ),
                SQL_CB_CLOSE,
                "SQL_CURSOR_ROLLBACK_BEHAVIOR ignored Backend::cursor_rollback_behavior"
            );

            let _ = crate::ffi::connect::sql_disconnect::<MockTxnDeleteCloseBackend>(conn);
            let _ = sql_free_handle::<MockTxnDeleteCloseBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockTxnDeleteCloseBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn get_type_info_orders_by_data_type_then_type_name() {
        use crate::test_utils::MockTypeInfoBackend;

        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockTypeInfoBackend>();
            let ret = sql_get_type_info::<MockTypeInfoBackend>(
                stmt,
                crate::types::SqlDataType::UNKNOWN_TYPE.0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            use crate::backend::StatementBackend as _;
            let seen: Vec<(i16, String)> = with_handle::<
                MockTypeInfoBackend,
                StatementHandle<MockTypeInfoBackend>,
                _,
            >(stmt, |handle| {
                let mut seen: Vec<(i16, String)> = Vec::new();
                let statement = handle.statement.as_mut().expect("result set");
                while matches!(
                    statement.fetch().expect("fetch"),
                    crate::types::FetchResult::Row
                ) {
                    let name = match &*statement
                        .get_data(1, crate::types::CDataType::WChar)
                        .expect("TYPE_NAME")
                    {
                        crate::types::ColumnValue::String(s) => s.clone(),
                        other => panic!("TYPE_NAME was {other:?}"),
                    };
                    let dt = match &*statement
                        .get_data(2, crate::types::CDataType::SShort)
                        .expect("DATA_TYPE")
                    {
                        crate::types::ColumnValue::I16(v) => *v,
                        other => panic!("DATA_TYPE was {other:?}"),
                    };
                    seen.push((dt, name));
                }
                seen
            });

            let mut expected = seen.clone();
            expected.sort();
            assert_eq!(
                seen, expected,
                "SQLGetTypeInfo must order by DATA_TYPE then TYPE_NAME, however \
                 the backend declared its list"
            );
            assert!(
                seen.len() >= 3,
                "the mock must declare enough rows to order"
            );

            cleanup_for::<MockTypeInfoBackend>(env, conn, stmt);
        }
    }

    #[test]
    fn a_truncated_info_string_posts_the_01004_it_refers_to() {
        // SQL_SUCCESS_WITH_INFO tells the application to call SQLGetDiagRec.
        // With no record there it cannot tell truncation from any other
        // informational condition.
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            assert_eq!(connect_handle(conn), SqlReturn::SUCCESS);

            // Two bytes: room for one UTF-16 unit, i.e. the terminator only.
            let mut buf = [0u16; 4];
            let mut str_len: i16 = -1;
            let ret = sql_get_info_w::<MockBackend>(
                conn,
                InfoType::DriverOdbcVer as u16,
                buf.as_mut_ptr() as *mut c_void,
                2,
                &mut str_len,
            );
            assert_eq!(ret, SqlReturn::SUCCESS_WITH_INFO, "must report truncation");

            with_handle::<MockBackend, ConnectionHandle<MockBackend>, _>(conn, |handle| {
                let rec = handle
                    .diagnostics
                    .get(0)
                    .expect("a truncation must leave a diagnostic record");
                assert_eq!(rec.sqlstate.as_str(), "01004");
            });

            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn type_info_columns_and_row_values_describe_the_same_result_set() {
        // `type_info_columns` and `TypeInfoRow::to_column_values` declare the
        // 19 `SQLGetTypeInfo` columns independently, and `SyntheticStatement`
        // pairs them positionally. A mismatch shows up as `SQLGetData` reading
        // a neighbouring column, not as an error.
        use crate::types::{ColumnValue, TypeInfoRow};

        let row = TypeInfoRow {
            type_name: "VARCHAR".into(),
            data_type: crate::types::SqlDataType::VARCHAR,
            column_size: 255,
            literal_prefix: Some("'".into()),
            literal_suffix: Some("'".into()),
            create_params: Some("length".into()),
            nullable: crate::types::Nullable::SqlNullable,
            case_sensitive: true,
            searchable: 3,
            unsigned: None,
            fixed_prec_scale: false,
            auto_unique_value: None,
            local_type_name: Some("VARCHAR".into()),
            minimum_scale: None,
            maximum_scale: None,
            sql_data_type: 12,
            sql_datetime_sub: None,
            num_prec_radix: None,
            interval_precision: None,
        };

        let widths = CatalogResultColumnWidths::default();
        let columns = type_info_columns(&widths);
        let values = row.to_column_values();
        assert_eq!(
            columns.len(),
            values.len(),
            "type_info_columns declares {} columns but to_column_values produced {}",
            columns.len(),
            values.len()
        );

        // Each declared SQL type must match the kind of value actually produced,
        // so a column cannot be declared numeric and filled with a string.
        for (i, (col, val)) in columns.iter().zip(values.iter()).enumerate() {
            let declared_is_char = matches!(
                col.sql_type,
                crate::types::SqlDataType::VARCHAR
                    | crate::types::SqlDataType::CHAR
                    | crate::types::SqlDataType::EXT_W_VARCHAR
                    | crate::types::SqlDataType::EXT_W_CHAR
            );
            let value_is_char = matches!(val, ColumnValue::String(_) | ColumnValue::Null);
            assert!(
                !declared_is_char || value_is_char,
                "column {} ({}) is declared character but carries {:?}",
                i + 1,
                col.name,
                val
            );
            let value_is_numeric = matches!(
                val,
                ColumnValue::I16(_) | ColumnValue::I32(_) | ColumnValue::I64(_) | ColumnValue::Null
            );
            assert!(
                declared_is_char || value_is_numeric,
                "column {} ({}) is declared numeric but carries {:?}",
                i + 1,
                col.name,
                val
            );
        }
    }

    #[test]
    fn driver_odbc_ver_is_answered_before_the_connection_exists() {
        // The Windows Driver Manager asks for this one *before*
        // `SQLDriverConnectW`, and treats an unusable answer as "ODBC 2.x
        // driver", which blocks 3.x features such as `SQL_C_SBIGINT`.
        // `MockBackend` implements neither `get_info_pre_connect` nor
        // `get_info_raw`, so this reaches core's own answer or nothing.
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            // Deliberately NOT connected: this is the pre-connect path.

            let mut buf = [0xEEu16; 32];
            let mut str_len: i16 = -1;
            let ret = sql_get_info_w::<MockBackend>(
                conn,
                InfoType::DriverOdbcVer as u16,
                buf.as_mut_ptr() as *mut c_void,
                64,
                &mut str_len,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let units = (str_len / 2) as usize;
            let got = String::from_utf16_lossy(&buf[..units]);
            assert_eq!(
                got,
                crate::types::SQL_DRIVER_ODBC_VER_STRING,
                "pre-connect SQL_DRIVER_ODBC_VER must report core's version, not \"\""
            );

            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn get_functions_single_query_returns_false_for_mock() {
        // MockBackend returns empty function list
        unsafe {
            let (env, conn) = alloc_env_and_conn();
            let mut result: u16 = 99;
            let ret = sql_get_functions::<MockBackend>(conn, 1, &mut result);
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(result, 0); // not supported

            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn get_functions_bitmap_is_zeroed_for_empty_backend() {
        // MockBackend returns an empty function list, so the bitmap should be all zeros.
        unsafe {
            use crate::function_id::SQL_API_ODBC3_ALL_FUNCTIONS_SIZE;
            let (env, conn) = alloc_env_and_conn();
            let mut bitmap = [0xFFFFu16; SQL_API_ODBC3_ALL_FUNCTIONS_SIZE];
            let ret = sql_get_functions::<MockBackend>(
                conn,
                crate::function_id::SQL_API_ODBC3_ALL_FUNCTIONS,
                bitmap.as_mut_ptr(),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            for val in &bitmap {
                assert_eq!(*val, 0);
            }

            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn get_type_info_sets_synthetic_statement() {
        // MockBackend returns empty type info, so the result set should have 0 rows
        unsafe {
            let (env, conn) = alloc_env_and_conn();
            // The type list is the data source's, so SQLGetTypeInfo needs an
            // open connection to read it from.
            let wide: Vec<u16> = "Host=localhost;Database=test".encode_utf16().collect();
            assert_eq!(
                crate::ffi::connect::sql_driver_connect_w::<MockBackend>(
                    conn,
                    std::ptr::null_mut(),
                    wide.as_ptr(),
                    wide.len() as i16,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    0,
                ),
                SqlReturn::SUCCESS,
            );
            let mut stmt: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(HandleType::Stmt as i16, conn, &mut stmt);

            let ret = sql_get_type_info::<MockBackend>(stmt, 0);
            assert_eq!(ret, SqlReturn::SUCCESS);

            // Verify statement data was set
            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                assert!(handle.statement.is_some());
            });

            let _ = sql_free_handle::<MockBackend>(HandleType::Stmt as i16, stmt);
            let _ = crate::ffi::connect::sql_disconnect::<MockBackend>(conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn get_info_null_handle_returns_invalid() {
        unsafe {
            let mut buf = [0u8; 64];
            let mut str_len: i16 = 0;
            let ret = sql_get_info_w::<MockBackend>(
                std::ptr::null_mut(),
                17,
                buf.as_mut_ptr() as *mut c_void,
                64,
                &mut str_len,
            );
            assert_eq!(ret, SqlReturn::INVALID_HANDLE);
        }
    }

    #[test]
    fn get_functions_null_handle_returns_invalid() {
        unsafe {
            let mut result: u16 = 0;
            let ret = sql_get_functions::<MockBackend>(std::ptr::null_mut(), 1, &mut result);
            assert_eq!(ret, SqlReturn::INVALID_HANDLE);
        }
    }

    #[test]
    fn get_info_not_connected_falls_back_to_default_response() {
        // A named InfoType queried before connect must reach the same DM-safe default
        // as an *unnamed* raw info type: `get_info_pre_connect` returning NotImplemented
        // must not propagate to SQL_ERROR, which would corrupt the Windows DM's internal
        // state (see `info_type_default_response`).
        //
        // SQL_DBMS_NAME is a `String`-shaped InfoType (see `expected_kind`), so the
        // DM-safe default here is an empty string, not U32(0); asserting a numeric
        // read would itself be the shape bug this test suite exists to catch (the
        // shape-aware fallback in `info_type_default_response`'s doc comment).
        unsafe {
            let (env, conn) = alloc_env_and_conn();
            let mut buf = [0xEEu16; 8];
            let mut str_len: i16 = -1;

            let ret = sql_get_info_w::<MockBackend>(
                conn,
                InfoType::DbmsName as u16,
                buf.as_mut_ptr() as *mut c_void,
                16,
                &mut str_len,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(str_len, 0, "SQL_DBMS_NAME default must be the empty string");
            assert_eq!(buf[0], 0, "empty string must be null-terminated at index 0");

            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn newly_named_info_types_still_use_the_default_response() {
        // Naming an InfoType must not change what the driver answers: whether a raw
        // value has no name at all, or is a real InfoType the backend hasn't
        // implemented, `SQLGetInfoW` must reach the same DM-safe default. Returning
        // SQL_ERROR instead of the benign default corrupts the Windows Driver
        // Manager's internal state.
        //
        // MockBackend::get_info unconditionally returns MockError, which maps to
        // OdbcError::NotImplemented (see test_utils.rs), so a connected MockBackend
        // handle exercises exactly the "backend reports NotImplemented" fallback path
        // that a real driver's incomplete `get_info` match would hit.
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            assert_eq!(connect_handle(conn), SqlReturn::SUCCESS);

            // SqlFileUsage (84) is outside the SQL_CONVERT_* range and is
            // `U16`-shaped, so the shape-aware fallback in
            // `info_type_default_response` must give it U16(0), not the
            // blanket U32(0).
            let mut buf = [0xEEu16; 8];
            let mut str_len: i16 = -1;
            let ret = sql_get_info_w::<MockBackend>(
                conn,
                InfoType::SqlFileUsage as u16,
                buf.as_mut_ptr() as *mut c_void,
                16,
                &mut str_len,
            );
            assert_eq!(ret, SqlReturn::SUCCESS, "SqlFileUsage must not error");
            assert_eq!(buf[0], 0, "SqlFileUsage default must be U16(0)");

            // OuterJoins (38) is `String`-shaped ("Y"/"P"/"N" per spec), and is
            // derived from `Backend::outer_join_capabilities` rather than left
            // to the shape default — "" is not one of the values the spec
            // defines for it. `MockBackend` declares LEFT | NESTED.
            let mut buf = [0xEEu16; 8];
            let mut str_len: i16 = -1;
            let ret = sql_get_info_w::<MockBackend>(
                conn,
                InfoType::OuterJoins as u16,
                buf.as_mut_ptr() as *mut c_void,
                16,
                &mut str_len,
            );
            assert_eq!(ret, SqlReturn::SUCCESS, "OuterJoins must not error");
            assert_eq!(str_len, 2, "OuterJoins must report one UTF-16 unit");
            assert_eq!(
                String::from_utf16_lossy(&buf[..1]),
                "Y",
                "a backend declaring outer-join capabilities must report Y"
            );

            // StringFunctions (50) is SQL_STRING_FUNCTIONS, a scalar-function
            // bitmap, not one of the actual SQL_CONVERT_* codes: claiming
            // "all supported" here for a backend that has not implemented it
            // would be an outright lie (a BI tool could then emit an
            // unsupported scalar function call), so this defaults to the same
            // honest U32(0) as any other unclassified info type, not
            // 0xFFFFFFFF.
            let mut string_functions: u32 = 0xDEAD_BEEF;
            let ret = sql_get_info_w::<MockBackend>(
                conn,
                InfoType::StringFunctions as u16,
                &mut string_functions as *mut u32 as *mut c_void,
                4,
                &mut str_len,
            );
            assert_eq!(ret, SqlReturn::SUCCESS, "StringFunctions must not error");
            assert_eq!(string_functions, 0);

            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn genuine_convert_type_info_still_defaults_to_all_supported() {
        // The range must cover every real SQL_CONVERT_* info type, or the
        // Windows Driver Manager blocks SQLGetData with HYC00 (AGENTS.md's
        // Windows Driver Manager compatibility checklist).
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            assert_eq!(connect_handle(conn), SqlReturn::SUCCESS);

            // SQL_CONVERT_BIGINT (53): the first info type in the real range.
            let mut convert_bigint: u32 = 0;
            let mut str_len: i16 = 0;
            let ret = sql_get_info_w::<MockBackend>(
                conn,
                crate::types::SQL_CONVERT_FUNCTIONS_FIRST,
                &mut convert_bigint as *mut u32 as *mut c_void,
                4,
                &mut str_len,
            );
            assert_eq!(ret, SqlReturn::SUCCESS, "SQL_CONVERT_BIGINT must not error");
            assert_eq!(convert_bigint, u32::MAX);

            // SQL_CONVERT_LONGVARBINARY (71): the last info type in the range.
            let mut convert_longvarbinary: u32 = 0;
            let ret = sql_get_info_w::<MockBackend>(
                conn,
                crate::types::SQL_CONVERT_FUNCTIONS_LAST,
                &mut convert_longvarbinary as *mut u32 as *mut c_void,
                4,
                &mut str_len,
            );
            assert_eq!(
                ret,
                SqlReturn::SUCCESS,
                "SQL_CONVERT_LONGVARBINARY must not error"
            );
            assert_eq!(convert_longvarbinary, u32::MAX);

            // SQL_CONVERT_GUID (173): a genuine convert type numbered outside
            // the contiguous 53-71 block. Must keep the 0xFFFFFFFF default;
            // returning U32(0) here is the HYC00-risking shape the range
            // check guards against.
            let mut convert_guid: u32 = 0;
            let ret = sql_get_info_w::<MockBackend>(
                conn,
                crate::types::SQL_CONVERT_GUID,
                &mut convert_guid as *mut u32 as *mut c_void,
                4,
                &mut str_len,
            );
            assert_eq!(ret, SqlReturn::SUCCESS, "SQL_CONVERT_GUID must not error");
            assert_eq!(
                convert_guid,
                u32::MAX,
                "SQL_CONVERT_GUID must not regress to 0"
            );

            // SQL_CONVERT_WCHAR (122) and SQL_CONVERT_WVARCHAR (126): the
            // second contiguous genuine-convert block. These must keep the
            // 0xFFFFFFFF default for the same HYC00 reason as SQL_CONVERT_GUID
            // above; the conformance test enumerates every SQL_CONVERT_* code
            // to ensure the range covers them.
            let mut convert_wchar: u32 = 0;
            let ret = sql_get_info_w::<MockBackend>(
                conn,
                crate::types::SQL_CONVERT_WCHAR,
                &mut convert_wchar as *mut u32 as *mut c_void,
                4,
                &mut str_len,
            );
            assert_eq!(ret, SqlReturn::SUCCESS, "SQL_CONVERT_WCHAR must not error");
            assert_eq!(
                convert_wchar,
                u32::MAX,
                "SQL_CONVERT_WCHAR must not regress to 0"
            );

            let mut convert_wvarchar: u32 = 0;
            let ret = sql_get_info_w::<MockBackend>(
                conn,
                crate::types::SQL_CONVERT_WVARCHAR,
                &mut convert_wvarchar as *mut u32 as *mut c_void,
                4,
                &mut str_len,
            );
            assert_eq!(
                ret,
                SqlReturn::SUCCESS,
                "SQL_CONVERT_WVARCHAR must not error"
            );
            assert_eq!(
                convert_wvarchar,
                u32::MAX,
                "SQL_CONVERT_WVARCHAR must not regress to 0"
            );

            cleanup(env, conn, stmt);
        }
    }

    /// Every `SQLGetInfo` type the spec declares as a character string but
    /// which `odbc_sys::InfoType` has no variant for.
    ///
    /// `info_type_from_raw` returns `None` for each, so
    /// `info_type_default_response`'s shape-aware fallback has no declared
    /// shape to honour and reaches the unnamed-raw default `U32(0)`. An
    /// application reading any of them into a character buffer then gets four
    /// bytes of binary zero with `StringLength = 4`.
    ///
    /// This is the defect that was fixed for `SQL_ROW_UPDATES` and
    /// `SQL_PROCEDURES` one type at a time. The list is the result of sweeping
    /// every info-type number in `sql.h`/`sqlext.h` against
    /// `info_type_from_raw`, so a new one cannot be missed by hand again — and
    /// the assertion runs through the whole `sql_get_info_w` path rather than
    /// calling `common_get_info_raw` directly, so the dispatch ordering is
    /// exercised too.
    #[rustfmt::skip]
    const STRING_SHAPED_WITHOUT_INFOTYPE_VARIANT: &[(u16, &str, &str)] = &[
        (crate::types::SQL_ROW_UPDATES,         "N",         "SQL_ROW_UPDATES"),
        (crate::types::SQL_PROCEDURES,          "N",         "SQL_PROCEDURES"),
        // Backend-stated: `MockBackend::multiple_active_txn` declares `true`,
        // so this also proves the hook reaches the string-shaped path.
        (crate::types::SQL_MULTIPLE_ACTIVE_TXN, "Y",         "SQL_MULTIPLE_ACTIVE_TXN"),
        // Consistent with SQL_PROCEDURES = "N": no procedures, so no vendor
        // term for one. Same rule as the catalog/schema term group.
        (crate::types::SQL_PROCEDURE_TERM,      "",          "SQL_PROCEDURE_TERM"),
        // Every data source has tables, so unlike the catalog and schema terms
        // this one has no "empty if unsupported" case.
        (crate::types::SQL_TABLE_TERM,          "table",     "SQL_TABLE_TERM"),
        // A comma-separated list of data-source-specific reserved words,
        // derived from `MockBackend::keywords` with ODBC's own subtracted out
        // (`SELECT` is dropped) and the remainder sorted. Asserted here rather
        // than only in `backend.rs` so the value is proven to survive the
        // string-shaped `sql_get_info_w` path, not just the raw dispatch.
        (crate::types::SQL_KEYWORDS,            "MOCK_ATTACH,MOCK_PRAGMA", "SQL_KEYWORDS"),
        (crate::types::SQL_DATABASE_NAME,       "",          "SQL_DATABASE_NAME"),
    ];

    #[test]
    fn string_shaped_info_types_without_an_odbc_sys_variant_return_strings() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            assert_eq!(connect_handle(conn), SqlReturn::SUCCESS);

            for (info_type, expected, name) in STRING_SHAPED_WITHOUT_INFOTYPE_VARIANT {
                // Sentinel-filled, so a U32(0) answer (4 bytes of zero, then
                // 0xEEEE) is distinguishable from a genuine empty string.
                let mut buf = [0xEEu16; 32];
                let mut str_len: i16 = -1;
                let ret = sql_get_info_w::<MockBackend>(
                    conn,
                    *info_type,
                    buf.as_mut_ptr() as *mut c_void,
                    (buf.len() * 2) as i16,
                    &mut str_len,
                );
                assert_eq!(ret, SqlReturn::SUCCESS, "{name} must not error");

                let expected_units = expected.encode_utf16().count();
                assert_eq!(
                    str_len as usize,
                    expected_units * 2,
                    "{name} reported the wrong StringLength — a U32(0) answer \
                     reports 4 bytes for what the spec declares a character string"
                );
                let actual = String::from_utf16_lossy(&buf[..expected_units]);
                assert_eq!(actual, *expected, "{name} returned the wrong text");
                assert_eq!(
                    buf[expected_units], 0,
                    "{name} must be null-terminated after its {expected_units} code units"
                );
            }

            cleanup(env, conn, stmt);
        }
    }

    /// Spec, `SQL_DATABASE_NAME`: "In ODBC 3.x, the value returned for this
    /// InfoType can also be returned by calling SQLGetConnectAttr with an
    /// Attribute argument of SQL_ATTR_CURRENT_CATALOG." One fact, two names, so
    /// setting the attribute moves the info type.
    #[test]
    fn database_name_follows_the_current_catalog_attribute() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            assert_eq!(connect_handle(conn), SqlReturn::SUCCESS);

            let read = |buf: &mut [u16; 32]| {
                let mut str_len: i16 = -1;
                let ret = sql_get_info_w::<MockBackend>(
                    conn,
                    crate::types::SQL_DATABASE_NAME,
                    buf.as_mut_ptr() as *mut c_void,
                    (buf.len() * 2) as i16,
                    &mut str_len,
                );
                assert_eq!(ret, SqlReturn::SUCCESS);
                (
                    str_len,
                    String::from_utf16_lossy(&buf[..(str_len / 2) as usize]),
                )
            };

            // Unset: the empty string, in the shape a character info type
            // declares rather than a numeric zero.
            let mut buf = [0xEEu16; 32];
            assert_eq!(read(&mut buf), (0, String::new()));

            let catalog: Vec<u16> = "analytics".encode_utf16().collect();
            assert_eq!(
                crate::ffi::connect_attr::sql_set_connect_attr_w::<MockBackend>(
                    conn,
                    odbc_sys::ConnectionAttribute::CURRENT_CATALOG.0,
                    catalog.as_ptr() as *mut c_void,
                    (catalog.len() * 2) as i32,
                ),
                SqlReturn::SUCCESS
            );

            let mut buf = [0xEEu16; 32];
            assert_eq!(read(&mut buf), (18, "analytics".to_string()));

            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn backend_error_other_than_not_implemented_still_propagates() {
        // The fallback in `info_type_or_default` must only trigger for
        // OdbcError::NotImplemented. Any other error is a genuine backend failure
        // and must still surface as SQL_ERROR, never be silently replaced with a
        // benign default.
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            assert_eq!(connect_handle(conn), SqlReturn::SUCCESS);

            let ret = info_type_or_default::<MockBackend>(
                Err(OdbcError::general(
                    "boom",
                    crate::types::SqlState::general_error(),
                )),
                None,
                None,
                InfoType::OuterJoins as u16,
            );
            assert!(matches!(ret, Err(OdbcError::General { .. })));

            cleanup(env, conn, stmt);
        }
    }

    /// Runs `SQLGetInfoW(SQL_KEYWORDS)` against `B` through the whole FFI path
    /// — allocate, connect, query, disconnect, free — and returns the text it
    /// wrote. Generic, unlike the shared helpers above, because the point of
    /// these assertions is that the answer moves with the backend.
    unsafe fn sql_keywords_of<B: Backend>() -> String {
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<B>(HandleType::Env as i16, std::ptr::null_mut(), &mut env);
            let mut conn: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<B>(HandleType::Dbc as i16, env, &mut conn);

            let wide: Vec<u16> = "Host=localhost;Database=test".encode_utf16().collect();
            assert_eq!(
                crate::ffi::connect::sql_driver_connect_w::<B>(
                    conn,
                    std::ptr::null_mut(),
                    wide.as_ptr(),
                    wide.len() as i16,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    0,
                ),
                SqlReturn::SUCCESS,
            );

            // Sentinel-filled, so an empty answer is distinguishable from a
            // `U32(0)` one that merely looks empty.
            let mut buf = [0xEEu16; 64];
            let mut str_len: i16 = -1;
            let ret = sql_get_info_w::<B>(
                conn,
                crate::types::SQL_KEYWORDS,
                buf.as_mut_ptr() as *mut c_void,
                (buf.len() * 2) as i16,
                &mut str_len,
            );
            assert_eq!(ret, SqlReturn::SUCCESS, "SQL_KEYWORDS must not error");
            assert!(
                str_len >= 0 && str_len % 2 == 0,
                "SQL_KEYWORDS is a character string, so StringLength is an even \
                 byte count; got {str_len}"
            );
            let units = str_len as usize / 2;
            let value = String::from_utf16_lossy(&buf[..units]);
            assert_eq!(buf[units], 0, "SQL_KEYWORDS must be null-terminated");

            let _ = crate::ffi::connect::sql_disconnect::<B>(conn);
            let _ = sql_free_handle::<B>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<B>(HandleType::Env as i16, env);
            value
        }
    }

    /// `SQL_KEYWORDS` is the spec's *subtraction*: the data source's reserved
    /// words minus the ones ODBC already reserves. Core owns that rule and
    /// `Backend::keywords` owns the raw list, so what an application sees has
    /// to move with the backend. An empty string is not a neutral answer here:
    /// it is the claim "this data source reserves nothing beyond ODBC", which
    /// is how a generated identifier ends up unquoted.
    ///
    /// Asserted through `sql_get_info_w` rather than `common_get_info_raw`, so
    /// the dispatch ordering is covered too.
    #[test]
    fn sql_keywords_is_the_backend_list_minus_odbcs_own() {
        use crate::test_utils::{
            MockAltBackend, MockNoKeywordsBackend, MockOverlappingKeywordsBackend,
            MockReservedOnlyKeywordsBackend,
        };

        unsafe {
            // Nothing to subtract from: the same value core produced before the
            // hook existed, so the shape is unchanged for a data source that
            // genuinely reserves nothing extra.
            assert_eq!(sql_keywords_of::<MockNoKeywordsBackend>(), "");
            // `SELECT` is ODBC's; `UNNEST` is not.
            assert_eq!(
                sql_keywords_of::<MockOverlappingKeywordsBackend>(),
                "UNNEST"
            );
            // Case-insensitive, so a lower-case list of ODBC's own words leaves
            // nothing behind.
            assert_eq!(sql_keywords_of::<MockReservedOnlyKeywordsBackend>(), "");
            // Sorted and comma-separated with no spaces, whatever order the
            // backend enumerates in — `MockBackend` declares
            // `["MOCK_PRAGMA", "SELECT", "MOCK_ATTACH"]`.
            assert_eq!(sql_keywords_of::<MockBackend>(), "MOCK_ATTACH,MOCK_PRAGMA");
            // And it moves with the backend.
            assert_eq!(sql_keywords_of::<MockAltBackend>(), "ALT_VACUUM");
        }
    }
}
