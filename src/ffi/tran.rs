//! Generic implementation of SQLEndTran.

use std::ffi::c_void;

use odbc_sys::HandleType;

use crate::backend::Backend;
use crate::errors::OdbcError;
use crate::handles::{ConnectionHandle, EnvironmentHandle, as_handle_ref};
use crate::panic::panic_safe;
use crate::types::{SqlReturn, completion_type_from_raw, handle_type_from_raw};

/// Generic implementation of SQLEndTran.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlendtran-function>
///
/// Commits or rolls back the active transaction on the given connection or on
/// all connections of the given environment.
///
/// If the connection is in autocommit mode (no open transaction), this is a no-op.
///
/// # Parameters
///
/// - `handle_type`: Must be `SQL_HANDLE_ENV` (1) or `SQL_HANDLE_DBC` (2). Any other value
///   returns `SQL_INVALID_HANDLE` (SQLSTATE HY092).
/// - `handle`: Environment or connection handle. Must match `handle_type`.
/// - `completion_type`: `SQL_COMMIT` (0) or `SQL_ROLLBACK` (1). Any other value
///   returns `SQL_ERROR` (SQLSTATE HY012).
///
/// # Spec compliance
///
/// SQLSTATEs from the spec Diagnostics table:
///
/// - **01000** — General warning (SQL_SUCCESS_WITH_INFO). Returned by the backend if it has
///   driver-specific informational messages to report.
///   Not currently surfaced. Backends return `Ok(())` with no info diagnostics; returning
///   01000 would require a new backend API variant for informational messages. Deferred.
///
/// - **08003** — Connection not open: returned when `HandleType` is `SQL_HANDLE_DBC` and
///   the connection is not in a connected state (`conn.connection` is `None`).
///
/// - **08007** — Connection failure during transaction. Returned by the backend if the
///   connection fails during COMMIT/ROLLBACK and it is unknown whether the operation
///   succeeded. Backends can surface this via `OdbcError`.
///
/// - **25S01** — Transaction state unknown. One or more connections failed to complete the
///   transaction and the outcome is unknown. Only relevant when `HandleType=SQL_HANDLE_ENV`
///   and multiple connections are present.
///   Env-level aggregation is not implemented. The loop currently stops on the first error.
///   Full 25S01 support would require collecting all per-connection outcomes before deciding
///   the final SQLSTATE. Deferred.
///
/// - **25S02** — Transaction is still active. The driver could not guarantee atomic
///   completion of the global transaction; the transaction remains active.
///   Returned by the backend via `OdbcError` if applicable.
///
/// - **25S03** — Transaction is rolled back. The driver could not guarantee atomic
///   completion; all work was rolled back.
///   Returned by the backend via `OdbcError` if applicable.
///
/// - **40001** — Serialization failure (deadlock). Transaction was rolled back.
///   Returned by the backend via `OdbcError` if applicable.
///
/// - **40002** — Integrity constraint violation on COMMIT. Transaction was rolled back.
///   Returned by the backend via `OdbcError` if applicable.
///
/// - **HY000** — General error. Returned by the backend for errors without a specific
///   SQLSTATE.
///
/// - **HY001** — Memory allocation error. Returned by the backend if memory cannot be
///   allocated.
///
/// - **HY008** — Operation canceled (async). Not applicable; the `Backend` trait is
///   synchronous and has no async execution path.
///
/// - **HY010** — Function sequence error (async). Not applicable; the `Backend` trait is
///   synchronous and has no async execution path.
///
/// - **HY012** — Invalid transaction operation code. Returned when `completion_type` is
///   neither `SQL_COMMIT` nor `SQL_ROLLBACK`. Implemented: the raw value is parsed before
///   any handle is touched; an unrecognised value returns `SQL_ERROR`.
///   Note: the spec says this is a DM-only SQLSTATE; `SQL_ERROR` is returned without
///   a SQLSTATE record because no handle is available at that point.
///
/// - **HY013** — Memory management error. Not specifically implemented; covered by
///   general Rust memory safety.
///
/// - **HY092** — Invalid attribute/option identifier. Returned when `handle_type` is not
///   `SQL_HANDLE_ENV` or `SQL_HANDLE_DBC`. Implemented: the `_` arm returns
///   `OdbcError::InvalidHandle` which maps to `SQL_INVALID_HANDLE`.
///
/// - **HY115** — SQLEndTran not allowed for environment with async connection. Not
///   applicable; the `Backend` trait has no async connection functions.
///
/// - **HY117** — Connection suspended due to unknown transaction state. Not applicable;
///   the Windows 7+ suspended-connection state is not tracked.
///
/// - **HYC00** — Optional feature not implemented. Returned by the backend if ROLLBACK
///   is not supported.
///
/// - **HYT01** — Connection timeout expired. May be returned by the backend.
///
/// - **IM001** — Driver does not support this function. Not applicable.
///
/// - **IM017 / IM018** — Async notification polling. Not applicable.
///
/// # Safety
///
/// `handle` must point to a valid `EnvironmentHandle<B>` or `ConnectionHandle<B>`.
pub unsafe fn sql_end_tran<B: Backend>(
    handle_type: i16,
    handle: *mut c_void,
    completion_type: i16,
) -> SqlReturn {
    tracing::trace!(
        "SQLEndTran(handle_type={}, handle={:?}, completion_type={})",
        handle_type,
        handle,
        completion_type
    );
    let completion = completion_type_from_raw(completion_type);
    tracing::debug!("SQLEndTran: completion={:?}", completion);

    // Validate completion_type before touching any handle.
    let commit = match completion {
        Some(odbc_sys::CompletionType::Commit) => true,
        Some(odbc_sys::CompletionType::Rollback) => false,
        None => {
            tracing::error!("SQLEndTran: invalid completion_type {completion_type}");
            return SqlReturn::ERROR;
        }
    };

    // Route based on handle type. panic_safe dispatches diagnostics to the
    // correct queue regardless of whether the handle is an ENV or DBC.
    // SAFETY: handle is null or a valid EnvironmentHandle<B> or ConnectionHandle<B>
    // allocated by sql_alloc_handle; the tag is validated by as_handle_ref inside
    // the closure. conn_ptr values in env.connections are valid ConnectionHandle<B>
    // pointers registered during sql_alloc_handle and still alive while the env lives.
    let ret = unsafe {
        panic_safe::<B, _>(handle, || {
            match handle_type_from_raw(handle_type) {
                Some(HandleType::Env) => {
                    let env = as_handle_ref::<EnvironmentHandle<B>>(handle)?;
                    // Apply to every connection registered on this environment.
                    for &conn_ptr in &env.connections {
                        // SAFETY: conn_ptr was registered by sql_alloc_handle and remains
                        // valid while the environment handle is alive.
                        let conn = as_handle_ref::<ConnectionHandle<B>>(conn_ptr as *mut c_void)?;
                        if let Some(ref connection) = conn.connection {
                            B::end_tran(connection, commit)?;
                        }
                    }
                    Ok(SqlReturn::SUCCESS)
                }
                Some(HandleType::Dbc) => {
                    let conn = as_handle_ref::<ConnectionHandle<B>>(handle)?;
                    // Spec 08003: Connection not open.
                    let Some(ref connection) = conn.connection else {
                        return Err(OdbcError::NotConnected);
                    };
                    B::end_tran(connection, commit)?;
                    Ok(SqlReturn::SUCCESS)
                }
                _ => Err(OdbcError::InvalidHandle),
            }
        })
    };
    tracing::debug!("SQLEndTran -> {:?}", ret);
    ret
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffi::handle::{sql_alloc_handle, sql_free_handle};
    use crate::test_utils::MockBackend;
    use crate::types::CompletionType;

    unsafe fn alloc_env_conn() -> (*mut c_void, *mut c_void) {
        let mut env: *mut c_void = std::ptr::null_mut();
        let _ = unsafe {
            sql_alloc_handle::<MockBackend>(HandleType::Env as i16, std::ptr::null_mut(), &mut env)
        };
        let mut conn: *mut c_void = std::ptr::null_mut();
        let _ = unsafe { sql_alloc_handle::<MockBackend>(HandleType::Dbc as i16, env, &mut conn) };
        (env, conn)
    }

    unsafe fn cleanup(env: *mut c_void, conn: *mut c_void) {
        unsafe {
            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn end_tran_commit_dbc_not_connected_returns_08003() {
        // Spec 08003: SQLEndTran on SQL_HANDLE_DBC when no connection is open.
        unsafe {
            let (env, conn) = alloc_env_conn();
            let ret = sql_end_tran::<MockBackend>(
                HandleType::Dbc as i16,
                conn,
                CompletionType::Commit as i16,
            );
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn);
        }
    }

    #[test]
    fn end_tran_rollback_dbc_not_connected_returns_08003() {
        unsafe {
            let (env, conn) = alloc_env_conn();
            let ret = sql_end_tran::<MockBackend>(
                HandleType::Dbc as i16,
                conn,
                CompletionType::Rollback as i16,
            );
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn);
        }
    }

    #[test]
    fn end_tran_env_handle_returns_success() {
        unsafe {
            let (env, conn) = alloc_env_conn();
            let ret = sql_end_tran::<MockBackend>(
                HandleType::Env as i16,
                env,
                CompletionType::Commit as i16,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            cleanup(env, conn);
        }
    }

    #[test]
    fn end_tran_invalid_completion_type_returns_error() {
        unsafe {
            let (env, conn) = alloc_env_conn();
            let ret = sql_end_tran::<MockBackend>(HandleType::Dbc as i16, conn, 99);
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn);
        }
    }

    #[test]
    fn end_tran_invalid_handle_type_returns_invalid_handle() {
        // SQL_HANDLE_STMT is not a valid handle type for SQLEndTran; spec
        // requires SQL_INVALID_HANDLE.
        unsafe {
            let (env, conn) = alloc_env_conn();
            let ret = sql_end_tran::<MockBackend>(HandleType::Stmt as i16, conn, 0);
            assert_eq!(ret, SqlReturn::INVALID_HANDLE);
            cleanup(env, conn);
        }
    }

    #[test]
    fn end_tran_null_handle_returns_invalid() {
        unsafe {
            let ret = sql_end_tran::<MockBackend>(HandleType::Dbc as i16, std::ptr::null_mut(), 0);
            assert_eq!(ret, SqlReturn::INVALID_HANDLE);
        }
    }
}
