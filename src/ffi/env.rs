//! Environment attributes: `SQLSetEnvAttr`, `SQLGetEnvAttr`.

use std::ffi::c_void;

use odbc_sys::EnvironmentAttribute;

use crate::backend::Backend;
use crate::errors::OdbcError;
use crate::handles::EnvironmentHandle;
use crate::panic::panic_safe;
use crate::types::{
    SQL_TRUE, SqlReturn, declared_odbc_version_from_raw, environment_attribute_from_raw,
};

/// Generic implementation of SQLSetEnvAttr.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlsetenvattr-function>
///
/// Supported attributes:
/// - `SQL_ATTR_ODBC_VERSION`: `SQL_OV_ODBC2` (2), `SQL_OV_ODBC3` (3), `SQL_OV_ODBC3_80` (380)
/// - `SQL_ATTR_OUTPUT_NTS`: `SQL_TRUE` only (HYC00 for `SQL_FALSE`)
///
/// # Parameters
///
/// - `environment_handle`: Environment handle to configure.
/// - `attribute`: The environment attribute identifier (e.g. `SQL_ATTR_ODBC_VERSION`).
/// - `value_ptr`: Pointer to the value to associate with the attribute. For integer
///   attributes, the value is passed directly as a pointer-sized integer (not dereferenced).
/// - `_string_length`: Length of `*value_ptr` if it is a character string; ignored for
///   integer attributes.
///
/// # Spec compliance
///
/// - 01000: General warning. The row carries no `(DM)` marker: not returned, because core
///   emits no driver-specific informational message from this function.
/// - 01S02: Option value changed. The row carries no `(DM)` marker: not returned, because
///   core substitutes no environment attribute value; an attribute it cannot honour is
///   refused rather than quietly changed.
/// - HY000: General error — returned for unexpected internal failures.
/// - HY001: Memory allocation failure (not returned here; Rust panics on
///   allocation failure).
/// - HY009: Returns `SQL_ERROR` if `value_ptr` is null for a string-valued attribute.
///   Not applicable — all supported attributes are integer-valued.
/// - HY010: Function sequence error. **Both** clauses of this row carry `(DM)` —
///   "**(DM)** A connection handle has been allocated on *EnvironmentHandle*"
///   and "**(DM)** SQL_ATTR_ODBC_VERSION has not been set" — so neither is the
///   driver's by the spec's own marking. The first is nevertheless checked here
///   as defence-in-depth for a caller that linked this driver directly: the
///   spec's Comments section states the rule with no marker at all ("An
///   application can call **SQLSetEnvAttr** only if no connection handle is
///   allocated on the environment"), and unixODBC's Driver Manager enforces it
///   before the call could reach any driver (`DriverManager/SQLSetEnvAttr.c`
///   posts `ERROR_S1010` when `environment->connection_count > 0`). Under a real
///   Driver Manager this branch never fires. The second clause is not checked
///   here at all.
/// - HY013: Memory management error (not returned here).
/// - HY024: Returns `SQL_ERROR` for an invalid attribute value — for
///   `SQL_ATTR_ODBC_VERSION`, anything that is not one of the three `SQL_OV_*`
///   values the spec defines. `SQL_OV_ODBC2` is one of them: unixODBC forwards
///   it to the driver verbatim at connect time
///   (`DriverManager/SQLConnect.c`), and a driver that refuses it is recorded
///   by that Driver Manager as an ODBC 2.x driver.
/// - HY090: Returns `SQL_ERROR` if string length is invalid for a string attribute.
///   Not applicable — all supported attributes are integer-valued.
/// - HY092: Invalid attribute/option identifier (driver-manager-handled; unsupported
///   attributes return `HYC00` rather than `HY092`).
/// - HY117: Connection suspended state (driver-manager-handled; not returned here).
/// - HYC00: Returns `SQL_ERROR` for `SQL_ATTR_OUTPUT_NTS = SQL_FALSE`, which is the row's
///   second clause and is `(DM)`-marked; it is guarded defensively here. The first clause
///   carries no marker and is the driver's — an attribute that is a valid ODBC environment
///   attribute for the driver's ODBC version but is not supported by the driver — and core
///   returns it for any unrecognised attribute identifier (see the implementation comment;
///   this is deliberate for DM compatibility).
///
/// # Safety
///
/// `environment_handle` must point to a valid `EnvironmentHandle<B>`.
pub unsafe fn sql_set_env_attr<B: Backend>(
    environment_handle: *mut c_void,
    attribute: i32,
    value_ptr: *mut c_void,
    _string_length: i32,
) -> SqlReturn {
    tracing::trace!(
        "SQLSetEnvAttr(env={:?}, attr_raw={}, value={:?})",
        environment_handle,
        attribute,
        value_ptr
    );
    let attr = environment_attribute_from_raw(attribute);
    tracing::debug!("SQLSetEnvAttr: attr={:?}", attr);
    // SAFETY: environment_handle is null or a token previously issued by
    // sql_alloc_handle; kind and group are validated by scope.get inside the
    // closure.
    let ret = unsafe {
        panic_safe::<B, _>(environment_handle, |scope| {
            let env = scope.get::<EnvironmentHandle<B>>(environment_handle)?;
            // Spec: clear diagnostics at the start of each ODBC call.
            env.diagnostics.clear();

            // Spec Comments, stated without a marker: "An application can call
            // SQLSetEnvAttr only if no connection handle is allocated on the
            // environment." The HY010 row marks this clause (DM) and unixODBC's
            // Driver Manager does make the check, so this is defence-in-depth
            // for a caller that linked the driver directly rather than a state
            // core owes. The registry, not a field of this handle, is the source
            // of truth for whether any connection still names it as parent.
            if !crate::handles::registry::registry()
                .children_of(environment_handle)
                .is_empty()
            {
                return Err(OdbcError::general(
                    "Cannot set environment attribute: connections already allocated",
                    crate::types::SqlState::function_sequence_error(),
                ));
            }

            match attr {
                Some(EnvironmentAttribute::OdbcVersion) => {
                    let version_value = value_ptr as usize as i32;
                    let version =
                        declared_odbc_version_from_raw(version_value).ok_or_else(|| {
                            // Spec HY024: Invalid attribute value.
                            OdbcError::general(
                                format!("Invalid ODBC version value: {version_value}"),
                                crate::types::SqlState::invalid_attribute_value(),
                            )
                        })?;
                    if version.is_odbc_2() {
                        // Accepted, and recorded, but core answers nothing
                        // differently for it: the spec's 2.x behaviours are the
                        // Driver Manager's mapping, which unixODBC drives from
                        // the application's requested version rather than from
                        // the driver's.
                        tracing::warn!(
                            "SQLSetEnvAttr: application declared SQL_OV_ODBC2; the version is \
                             recorded and reported back, but core's own SQLSTATEs and datetime \
                             type codes stay 3.x and the Driver Manager maps them"
                        );
                    }
                    env.odbc_version = version;
                    Ok(SqlReturn::SUCCESS)
                }
                Some(EnvironmentAttribute::OutputNts) => {
                    // Spec: SQL_ATTR_OUTPUT_NTS — SQL_TRUE is always supported,
                    // SQL_FALSE returns HYC00 (optional feature not implemented).
                    // ODBC convention: integer-valued attributes are passed as (SQLPOINTER)(SQLUINTEGER)value.
                    // SQL_TRUE = 1, SQL_FALSE = 0; recover by casting pointer to integer.
                    let nts_value = value_ptr as usize as i32;
                    if nts_value == SQL_TRUE as i32 {
                        // SQL_TRUE — this is the default and only supported value.
                        Ok(SqlReturn::SUCCESS)
                    } else {
                        Err(OdbcError::NotImplemented {
                            feature: "SQL_ATTR_OUTPUT_NTS = SQL_FALSE".into(),
                        })
                    }
                }
                _ => Err(OdbcError::NotImplemented {
                    feature: format!("SQLSetEnvAttr attribute {attribute}"),
                }),
            }
        })
    };
    tracing::debug!("SQLSetEnvAttr -> {:?}", ret);
    ret
}

/// Generic implementation of SQLGetEnvAttr.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlgetenvattr-function>
///
/// Supported attributes:
/// - `SQL_ATTR_ODBC_VERSION`: Returns current ODBC version as `i32`.
/// - `SQL_ATTR_OUTPUT_NTS`: Always returns `SQL_TRUE` (1).
///
/// # Parameters
///
/// - `environment_handle`: Environment handle to query.
/// - `attribute`: The environment attribute identifier to retrieve.
/// - `value_ptr`: Pointer to a buffer that receives the attribute value. For integer
///   attributes this must point to a writable `i32`. May be null (value is not written,
///   but `string_length_ptr` is still populated).
/// - `_buffer_length`: Length of `*value_ptr` in bytes if it is a character string;
///   ignored for integer attributes.
/// - `string_length_ptr`: Pointer to a buffer that receives the byte count of data
///   available to return. For integer attributes this is set to `sizeof(i32)` (4).
///   May be null.
///
/// # Spec compliance
///
/// - 01000: General warning. The row carries no `(DM)` marker: not returned, because core
///   emits no driver-specific informational message from this function.
/// - 01004: String data right truncated — not returned here, and the row carries no `(DM)`
///   marker: reporting truncation through the output buffer is the driver's duty. It
///   cannot arise because every environment attribute core supports is integer-valued, so
///   nothing is written that a short buffer could truncate.
/// - HY000: General error — returned for unexpected internal failures.
/// - HY001: Memory allocation failure (not returned here; Rust panics on
///   allocation failure).
/// - HY010: Function sequence error — `SQL_ATTR_ODBC_VERSION` not yet set
///   (driver-manager-handled; not returned here).
/// - HY013: Memory management error (not returned here).
/// - HY092: Returns `SQL_ERROR` for unrecognised attribute identifiers; `HYC00` is returned
///   instead (see implementation comment; this is deliberate for DM compatibility).
/// - HY117: Connection suspended state (driver-manager-handled; not returned here).
/// - HYC00: Returns `SQL_ERROR` for valid ODBC attributes that are not supported.
/// - IM001: Driver does not support this function (driver-manager-handled; not returned
///   here).
///
/// # Safety
///
/// `environment_handle` must point to a valid `EnvironmentHandle<B>`.
/// `value_ptr` must be a valid pointer for writing an `i32` when requesting integer attributes.
pub unsafe fn sql_get_env_attr<B: Backend>(
    environment_handle: *mut c_void,
    attribute: i32,
    value_ptr: *mut c_void,
    _buffer_length: i32,
    string_length_ptr: *mut i32,
) -> SqlReturn {
    tracing::trace!(
        "SQLGetEnvAttr(env={:?}, attr_raw={})",
        environment_handle,
        attribute
    );
    let attr = environment_attribute_from_raw(attribute);
    tracing::debug!("SQLGetEnvAttr: attr={:?}", attr);
    // SAFETY: environment_handle is null or a token previously issued by
    // sql_alloc_handle; kind and group are validated by scope.get inside the
    // closure.
    let ret = unsafe {
        panic_safe::<B, _>(environment_handle, |scope| {
            let env = scope.get::<EnvironmentHandle<B>>(environment_handle)?;
            // Spec: clear diagnostics at the start of each ODBC call.
            env.diagnostics.clear();

            match attr {
                Some(EnvironmentAttribute::OdbcVersion) => {
                    if !value_ptr.is_null() {
                        // SAFETY: non-null checked above; caller guarantees writable i32
                        let out = value_ptr as *mut i32;
                        std::ptr::write_unaligned(out, env.odbc_version.raw());
                    }
                    // Spec: for integer attributes, write byte size to StringLengthPtr.
                    if !string_length_ptr.is_null() {
                        // SAFETY: non-null checked above; caller guarantees writable i32
                        std::ptr::write_unaligned(
                            string_length_ptr,
                            std::mem::size_of::<i32>() as i32,
                        );
                    }
                    Ok(SqlReturn::SUCCESS)
                }
                Some(EnvironmentAttribute::OutputNts) => {
                    // Spec: SQL_ATTR_OUTPUT_NTS always returns SQL_TRUE.
                    if !value_ptr.is_null() {
                        // SAFETY: non-null checked above; caller guarantees writable i32
                        let out = value_ptr as *mut i32;
                        std::ptr::write_unaligned(out, SQL_TRUE as i32);
                    }
                    if !string_length_ptr.is_null() {
                        // SAFETY: non-null checked above; caller guarantees writable i32
                        std::ptr::write_unaligned(
                            string_length_ptr,
                            std::mem::size_of::<i32>() as i32,
                        );
                    }
                    Ok(SqlReturn::SUCCESS)
                }
                // Returning HYC00 (NotImplemented) rather than HY092 for unrecognised attribute
                // identifiers is a deliberate DM-compatibility choice. HYC00 is accepted by all
                // common Driver Managers; HY092 is technically more correct.
                _ => Err(OdbcError::NotImplemented {
                    feature: format!("SQLGetEnvAttr attribute {attribute}"),
                }),
            }
        })
    };
    tracing::debug!("SQLGetEnvAttr -> {:?}", ret);
    ret
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::c_void;

    use odbc_sys::HandleType;

    use crate::ffi::handle::{sql_alloc_handle, sql_free_handle};
    use crate::test_utils::{MockBackend, with_handle};

    const ENV_ATTR_ODBC_VERSION: i32 = odbc_sys::EnvironmentAttribute::OdbcVersion as i32;

    #[test]
    fn set_and_get_odbc_version() {
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );

            // Set ODBC version to V3 (SQL_ATTR_ODBC_VERSION, value SQL_OV_ODBC3)
            let ret = sql_set_env_attr::<MockBackend>(
                env,
                ENV_ATTR_ODBC_VERSION,
                crate::types::SQL_OV_ODBC3 as usize as *mut c_void,
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            // Get ODBC version back
            let mut version: i32 = 0;
            let ret = sql_get_env_attr::<MockBackend>(
                env,
                ENV_ATTR_ODBC_VERSION,
                &mut version as *mut i32 as *mut c_void,
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(version, crate::types::SQL_OV_ODBC3);

            // Set to V3_80 (380) and verify
            let ret = sql_set_env_attr::<MockBackend>(
                env,
                ENV_ATTR_ODBC_VERSION,
                crate::types::SQL_OV_ODBC3_80 as usize as *mut c_void,
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let mut version: i32 = 0;
            let ret = sql_get_env_attr::<MockBackend>(
                env,
                ENV_ATTR_ODBC_VERSION,
                &mut version as *mut i32 as *mut c_void,
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(version, crate::types::SQL_OV_ODBC3_80);

            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn set_env_attr_invalid_version() {
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );

            let ret =
                sql_set_env_attr::<MockBackend>(env, ENV_ATTR_ODBC_VERSION, 999 as *mut c_void, 0);
            assert_eq!(ret, SqlReturn::ERROR);

            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    /// The check is defence-in-depth for a caller that linked this driver
    /// directly, not a state core owes by the spec's rules: **both** clauses of
    /// `SQLSetEnvAttr`'s `HY010` row carry `(DM)`, and unixODBC's Driver Manager
    /// makes the check itself (`DriverManager/SQLSetEnvAttr.c` posts
    /// `ERROR_S1010` when `environment->connection_count > 0`, without calling
    /// the driver at all). The spec's Comments section states the rule with no
    /// marker — "An application can call SQLSetEnvAttr only if no connection
    /// handle is allocated on the environment" — so it is kept, and pinned here
    /// so a future reader deletes it deliberately or not at all.
    #[test]
    fn setting_an_environment_attribute_with_a_connection_allocated_is_hy010() {
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );
            let mut conn: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(HandleType::Dbc as i16, env, &mut conn);

            assert_eq!(
                sql_set_env_attr::<MockBackend>(
                    env,
                    ENV_ATTR_ODBC_VERSION,
                    crate::types::SQL_OV_ODBC3 as usize as *mut c_void,
                    0,
                ),
                SqlReturn::ERROR,
            );
            with_handle::<MockBackend, EnvironmentHandle<MockBackend>, _>(env, |h| {
                assert_eq!(
                    h.diagnostics
                        .get(0)
                        .expect("a diagnostic record")
                        .sqlstate
                        .as_str(),
                    "HY010",
                );
            });

            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    /// `SQL_OV_ODBC2` is one of the three values `SQL_ATTR_ODBC_VERSION`
    /// defines, and unixODBC's Driver Manager forwards it to the driver
    /// verbatim at connect time (`DriverManager/SQLConnect.c`, which passes
    /// `environment->requested_version` to the driver's `SQLSetEnvAttr`
    /// whenever the driver exports `SQLAllocHandle`). Rejecting it makes that
    /// DM record the driver as `SQL_OV_ODBC2` and take its 2.x handle-allocation
    /// path — the opposite of what a 3.x driver wants.
    #[test]
    fn sql_ov_odbc2_is_accepted_and_reads_back_unchanged() {
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );

            assert_eq!(
                sql_set_env_attr::<MockBackend>(
                    env,
                    ENV_ATTR_ODBC_VERSION,
                    crate::types::SQL_OV_ODBC2 as usize as *mut c_void,
                    0,
                ),
                SqlReturn::SUCCESS,
                "SQL_OV_ODBC2 is a spec-defined value the Driver Manager forwards",
            );

            let mut version: i32 = 0;
            assert_eq!(
                sql_get_env_attr::<MockBackend>(
                    env,
                    ENV_ATTR_ODBC_VERSION,
                    std::ptr::from_mut(&mut version).cast::<c_void>(),
                    0,
                    std::ptr::null_mut(),
                ),
                SqlReturn::SUCCESS,
            );
            assert_eq!(
                version,
                crate::types::SQL_OV_ODBC2,
                "the Driver Manager reads the version back to decide how to treat \
                 this driver, so it must be the value the application declared",
            );

            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    /// A stale record from a failed call must not still be on the queue
    /// during a later successful one.
    #[test]
    fn set_env_attr_clears_diagnostics_from_an_earlier_call() {
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );

            // An invalid ODBC version fails with HY024 and leaves a record.
            let ret =
                sql_set_env_attr::<MockBackend>(env, ENV_ATTR_ODBC_VERSION, 999 as *mut c_void, 0);
            assert_eq!(ret, SqlReturn::ERROR);
            with_handle::<MockBackend, EnvironmentHandle<MockBackend>, _>(env, |h| {
                assert_eq!(h.diagnostics.len(), 1, "precondition: a record is queued");
            });

            // The next, valid call must start from an empty queue rather than
            // appending to the record the failed call left behind.
            let ret =
                sql_set_env_attr::<MockBackend>(env, ENV_ATTR_ODBC_VERSION, 3 as *mut c_void, 0);
            assert_eq!(ret, SqlReturn::SUCCESS);
            with_handle::<MockBackend, EnvironmentHandle<MockBackend>, _>(env, |h| {
                assert_eq!(
                    h.diagnostics.len(),
                    0,
                    "the queue must be cleared at entry, not appended to"
                );
            });

            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn get_env_attr_unsupported_attribute() {
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );

            let mut value: i32 = 0;
            let ret = sql_get_env_attr::<MockBackend>(
                env,
                999, // unsupported attribute
                &mut value as *mut i32 as *mut c_void,
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::ERROR);

            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    /// A stale record from a failed call must not still be on the queue
    /// during a later successful one.
    #[test]
    fn get_env_attr_clears_diagnostics_from_an_earlier_call() {
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );

            // An unsupported attribute fails with HYC00 and leaves a record.
            let mut value: i32 = 0;
            let ret = sql_get_env_attr::<MockBackend>(
                env,
                999, // unsupported attribute
                &mut value as *mut i32 as *mut c_void,
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::ERROR);
            with_handle::<MockBackend, EnvironmentHandle<MockBackend>, _>(env, |h| {
                assert_eq!(h.diagnostics.len(), 1, "precondition: a record is queued");
            });

            // The next, valid call must start from an empty queue rather than
            // appending to the record the failed call left behind.
            let ret = sql_get_env_attr::<MockBackend>(
                env,
                ENV_ATTR_ODBC_VERSION,
                &mut value as *mut i32 as *mut c_void,
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            with_handle::<MockBackend, EnvironmentHandle<MockBackend>, _>(env, |h| {
                assert_eq!(
                    h.diagnostics.len(),
                    0,
                    "the queue must be cleared at entry, not appended to"
                );
            });

            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn set_env_attr_fails_after_connection_allocated() {
        // Spec HY010: Cannot set env attr after connections allocated.
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );

            // Allocate a connection on this environment.
            let mut conn: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(HandleType::Dbc as i16, env, &mut conn);

            // Now trying to set ODBC version should fail.
            let ret =
                sql_set_env_attr::<MockBackend>(env, ENV_ATTR_ODBC_VERSION, 3 as *mut c_void, 0);
            assert_eq!(ret, SqlReturn::ERROR);

            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn set_env_attr_output_nts_true_succeeds() {
        // Spec: SQL_ATTR_OUTPUT_NTS = SQL_TRUE (1) must succeed.
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );

            // EnvironmentAttribute::OutputNts = 10001
            let ret = sql_set_env_attr::<MockBackend>(
                env,
                odbc_sys::EnvironmentAttribute::OutputNts as i32,
                std::ptr::dangling_mut::<c_void>(), // SQL_TRUE (non-null)
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn set_env_attr_output_nts_false_fails() {
        // Spec HYC00: SQL_ATTR_OUTPUT_NTS = SQL_FALSE is not supported.
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );

            let ret = sql_set_env_attr::<MockBackend>(
                env,
                odbc_sys::EnvironmentAttribute::OutputNts as i32,
                std::ptr::null_mut::<c_void>(), // SQL_FALSE
                0,
            );
            assert_eq!(ret, SqlReturn::ERROR);

            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn get_env_attr_output_nts_returns_true() {
        // Spec: SQL_ATTR_OUTPUT_NTS always returns SQL_TRUE.
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );

            let mut value: i32 = 0;
            let ret = sql_get_env_attr::<MockBackend>(
                env,
                odbc_sys::EnvironmentAttribute::OutputNts as i32,
                &mut value as *mut i32 as *mut c_void,
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(value, SQL_TRUE as i32);

            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn set_env_attr_null_handle_returns_invalid() {
        unsafe {
            let ret = sql_set_env_attr::<MockBackend>(
                std::ptr::null_mut(),
                ENV_ATTR_ODBC_VERSION,
                3 as *mut c_void,
                0,
            );
            assert_eq!(ret, SqlReturn::INVALID_HANDLE);
        }
    }

    #[test]
    fn get_env_attr_null_handle_returns_invalid() {
        unsafe {
            let mut value: i32 = 0;
            let ret = sql_get_env_attr::<MockBackend>(
                std::ptr::null_mut(),
                ENV_ATTR_ODBC_VERSION,
                &mut value as *mut i32 as *mut c_void,
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::INVALID_HANDLE);
        }
    }

    #[test]
    fn get_env_attr_writes_string_length() {
        // Spec: StringLengthPtr is populated with byte size for integer attrs.
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );

            let mut value: i32 = 0;
            let mut str_len: i32 = 0;
            let ret = sql_get_env_attr::<MockBackend>(
                env,
                ENV_ATTR_ODBC_VERSION,
                &mut value as *mut i32 as *mut c_void,
                0,
                &mut str_len,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(str_len, 4); // size_of::<i32>()

            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }
}
