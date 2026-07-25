//! Generic implementation of SQLEndTran.

use std::ffi::c_void;

use odbc_sys::HandleType;

use crate::backend::Backend;
use crate::errors::OdbcError;
use crate::handles::{ConnectionHandle, EnvironmentHandle, as_handle_ref};
use crate::panic::panic_safe;
use crate::types::{SqlReturn, completion_type_from_raw, handle_type_from_raw};

/// Apply the backend's declared cursor behaviour to every statement on a
/// connection, after a successful `B::end_tran`.
///
/// Mirrors the `SQLEndTran` statement transition table: `SQL_CB_DELETE`
/// returns prepared statements to the allocated state S1 (dropping the access
/// plan), `SQL_CB_CLOSE` returns them to their prepared state S2/S3, and
/// `SQL_CB_PRESERVE` changes nothing.
///
/// Binding state, parameter bindings and the cursor name are deliberately
/// untouched: the spec keeps them orthogonal to prepare state, which is what
/// `SQLFreeStmt(SQL_UNBIND)` and `SQL_RESET_PARAMS` are for. `data_at_exec` is
/// likewise untouched — the transition table marks the need-data states
/// S8-S10 as `(HY010)`, a Driver-Manager-detected error, so `SQLEndTran`
/// cannot reach the driver while a data-at-execution sequence is pending.
///
/// # Safety
///
/// The raw pointers in `conn.statements` must be valid `StatementHandle<B>`
/// allocations registered by `sql_alloc_handle` and still alive.
unsafe fn apply_cursor_behavior<B: Backend>(
    conn: &mut ConnectionHandle<B>,
    behavior: crate::types::CursorBehavior,
) {
    use crate::types::CursorBehavior;

    if behavior == CursorBehavior::Preserve {
        return;
    }

    tracing::debug!(
        "SQLEndTran: applying {:?} to {} statement(s)",
        behavior,
        conn.statements.len()
    );

    for &stmt_ptr in &conn.statements {
        // SAFETY: stmt_ptr was registered by sql_alloc_handle and remains valid
        // while the connection handle is alive; the tag is validated here.
        let Ok(stmt) = (unsafe {
            as_handle_ref::<crate::handles::StatementHandle<B>>(stmt_ptr as *mut c_void)
        }) else {
            tracing::warn!("SQLEndTran: skipping statement with an invalid tag");
            continue;
        };

        stmt.statement = None;

        if behavior == CursorBehavior::Delete {
            stmt.prepared_sql = None;
            stmt.param_count = None;
        }
    }
}

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
                    let behavior = if commit {
                        B::cursor_commit_behavior()
                    } else {
                        B::cursor_rollback_behavior()
                    };

                    // Snapshot the pointer list so no borrow of `env` is held
                    // across the `&mut ConnectionHandle` borrows below.
                    let conn_ptrs: Vec<_> = env.connections.clone();

                    // Spec: "the driver will attempt to commit or roll back
                    // transactions ... on all connections that are in a
                    // connected state on that environment." Every connection is
                    // attempted; the first error is returned once the loop
                    // finishes. Stopping early would leave later connections
                    // holding open transactions the application asked to end.
                    let mut first_err: Option<OdbcError> = None;

                    for conn_ptr in conn_ptrs {
                        // SAFETY: conn_ptr was registered by sql_alloc_handle and
                        // remains valid while the environment handle is alive.
                        let conn =
                            match as_handle_ref::<ConnectionHandle<B>>(conn_ptr as *mut c_void) {
                                Ok(conn) => conn,
                                Err(e) => {
                                    tracing::warn!(
                                        "SQLEndTran: connection {:?} failed tag validation",
                                        conn_ptr
                                    );
                                    first_err.get_or_insert(e);
                                    continue;
                                }
                            };

                        let Some(ref connection) = conn.connection else {
                            // Not connected: "Connections that are not active do
                            // not affect the transaction."
                            continue;
                        };

                        match B::end_tran(connection, commit) {
                            Ok(()) => {
                                // SAFETY: conn.statements holds live StatementHandle<B>
                                // allocations; tags are validated inside. This call is
                                // within the outer `unsafe { panic_safe(...) }` closure.
                                apply_cursor_behavior::<B>(conn, behavior);
                            }
                            Err(e) => {
                                // Spec: "To determine which connection or
                                // connections failed ... the application can call
                                // SQLGetDiagRec for each connection."
                                conn.diagnostics.push(&e);
                                first_err.get_or_insert(e);
                            }
                        }
                    }

                    match first_err {
                        Some(e) => Err(e),
                        None => Ok(SqlReturn::SUCCESS),
                    }
                }
                Some(HandleType::Dbc) => {
                    let conn = as_handle_ref::<ConnectionHandle<B>>(handle)?;
                    // Spec 08003: Connection not open.
                    let Some(ref connection) = conn.connection else {
                        return Err(OdbcError::NotConnected);
                    };
                    B::end_tran(connection, commit)?;
                    // Only on success: on failure core cannot tell whether the
                    // transaction ended, and guessing wrong destroys cursor
                    // state the application may still need.
                    let behavior = if commit {
                        B::cursor_commit_behavior()
                    } else {
                        B::cursor_rollback_behavior()
                    };
                    // SAFETY: conn.statements holds live StatementHandle<B>
                    // allocations; tags are validated inside.
                    apply_cursor_behavior::<B>(conn, behavior);
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
    use crate::handles::StatementHandle;
    use crate::test_utils::{
        MockBackend, MockTxnCloseBackend, MockTxnDeleteBackend, MockTxnPreserveBackend,
    };
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

    /// Allocates env + connection + statement for a transaction-capable
    /// backend, connects, and puts the statement into a state that has both an
    /// open cursor and prepared SQL, so a test can tell `Close` from `Delete`.
    ///
    /// `conn_str` is passed through to `SQLDriverConnectW`; pass
    /// `"ENDTRANFAIL=1;"` to make `end_tran` fail for this connection.
    unsafe fn alloc_connected_stmt<B: crate::backend::Backend>(
        conn_str: &str,
    ) -> (*mut c_void, *mut c_void, *mut c_void) {
        let mut env: *mut c_void = std::ptr::null_mut();
        let _ = unsafe {
            sql_alloc_handle::<B>(HandleType::Env as i16, std::ptr::null_mut(), &mut env)
        };
        let mut conn: *mut c_void = std::ptr::null_mut();
        let _ = unsafe { sql_alloc_handle::<B>(HandleType::Dbc as i16, env, &mut conn) };

        let mut wide: Vec<u16> = conn_str.encode_utf16().collect();
        wide.push(0);
        let _ = unsafe {
            crate::ffi::connect::sql_driver_connect_w::<B>(
                conn,
                std::ptr::null_mut(),
                wide.as_ptr(),
                crate::types::SQL_NTS as i16,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                0,
            )
        };

        let mut stmt: *mut c_void = std::ptr::null_mut();
        let _ = unsafe { sql_alloc_handle::<B>(HandleType::Stmt as i16, conn, &mut stmt) };

        let handle = unsafe { as_handle_ref::<StatementHandle<B>>(stmt) }.expect("valid stmt");
        handle.statement = Some(crate::handles::StatementData::Synthetic(
            crate::synthetic::SyntheticStatement::new(vec![], vec![]),
        ));
        handle.prepared_sql = Some("SELECT 1".to_string());
        handle.param_count = Some(0);

        (env, conn, stmt)
    }

    unsafe fn cleanup_connected<B: crate::backend::Backend>(
        env: *mut c_void,
        conn: *mut c_void,
        stmt: *mut c_void,
    ) {
        unsafe {
            let _ = sql_free_handle::<B>(HandleType::Stmt as i16, stmt);
            let _ = crate::ffi::connect::sql_disconnect::<B>(conn);
            let _ = sql_free_handle::<B>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<B>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn end_tran_close_clears_the_cursor_and_keeps_the_access_plan() {
        // SQL_CB_CLOSE: cursors closed, prepared statements stay prepared.
        unsafe {
            let (env, conn, stmt) = alloc_connected_stmt::<MockTxnCloseBackend>("DRIVER=mock;");
            let ret = sql_end_tran::<MockTxnCloseBackend>(
                HandleType::Dbc as i16,
                conn,
                CompletionType::Commit as i16,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let handle =
                as_handle_ref::<StatementHandle<MockTxnCloseBackend>>(stmt).expect("valid");
            assert!(
                handle.statement.is_none(),
                "SQL_CB_CLOSE left the cursor open"
            );
            assert_eq!(
                handle.prepared_sql.as_deref(),
                Some("SELECT 1"),
                "SQL_CB_CLOSE discarded the access plan"
            );
            assert_eq!(handle.param_count, Some(0));

            cleanup_connected::<MockTxnCloseBackend>(env, conn, stmt);
        }
    }

    #[test]
    fn end_tran_delete_clears_the_cursor_and_the_access_plan() {
        // SQL_CB_DELETE: statement returns to the allocated (unprepared) state S1.
        unsafe {
            let (env, conn, stmt) = alloc_connected_stmt::<MockTxnDeleteBackend>("DRIVER=mock;");
            let ret = sql_end_tran::<MockTxnDeleteBackend>(
                HandleType::Dbc as i16,
                conn,
                CompletionType::Commit as i16,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let handle =
                as_handle_ref::<StatementHandle<MockTxnDeleteBackend>>(stmt).expect("valid");
            assert!(handle.statement.is_none());
            assert!(
                handle.prepared_sql.is_none(),
                "SQL_CB_DELETE kept the access plan"
            );
            assert!(handle.param_count.is_none());

            cleanup_connected::<MockTxnDeleteBackend>(env, conn, stmt);
        }
    }

    #[test]
    fn end_tran_preserve_leaves_the_statement_untouched() {
        unsafe {
            let (env, conn, stmt) = alloc_connected_stmt::<MockTxnPreserveBackend>("DRIVER=mock;");
            let ret = sql_end_tran::<MockTxnPreserveBackend>(
                HandleType::Dbc as i16,
                conn,
                CompletionType::Commit as i16,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let handle =
                as_handle_ref::<StatementHandle<MockTxnPreserveBackend>>(stmt).expect("valid");
            assert!(
                handle.statement.is_some(),
                "SQL_CB_PRESERVE closed the cursor"
            );
            assert_eq!(handle.prepared_sql.as_deref(), Some("SELECT 1"));

            cleanup_connected::<MockTxnPreserveBackend>(env, conn, stmt);
        }
    }

    #[test]
    fn end_tran_rollback_uses_the_rollback_behavior() {
        unsafe {
            let (env, conn, stmt) = alloc_connected_stmt::<MockTxnDeleteBackend>("DRIVER=mock;");
            let ret = sql_end_tran::<MockTxnDeleteBackend>(
                HandleType::Dbc as i16,
                conn,
                CompletionType::Rollback as i16,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let handle =
                as_handle_ref::<StatementHandle<MockTxnDeleteBackend>>(stmt).expect("valid");
            assert!(handle.statement.is_none());
            assert!(handle.prepared_sql.is_none());

            cleanup_connected::<MockTxnDeleteBackend>(env, conn, stmt);
        }
    }

    #[test]
    fn end_tran_failure_leaves_cursor_state_untouched() {
        // On error core cannot tell whether the transaction ended, so it must
        // not destroy cursor state the application may still need.
        unsafe {
            let (env, conn, stmt) =
                alloc_connected_stmt::<MockTxnDeleteBackend>("DRIVER=mock;ENDTRANFAIL=1;");
            let ret = sql_end_tran::<MockTxnDeleteBackend>(
                HandleType::Dbc as i16,
                conn,
                CompletionType::Commit as i16,
            );
            assert_eq!(ret, SqlReturn::ERROR);

            let handle =
                as_handle_ref::<StatementHandle<MockTxnDeleteBackend>>(stmt).expect("valid");
            assert!(
                handle.statement.is_some(),
                "cursor state destroyed on a failed SQLEndTran"
            );
            assert_eq!(handle.prepared_sql.as_deref(), Some("SELECT 1"));

            cleanup_connected::<MockTxnDeleteBackend>(env, conn, stmt);
        }
    }

    #[test]
    fn end_tran_delete_keeps_bindings_and_cursor_name() {
        // The spec says nothing about binding state here, and ODBC keeps it
        // orthogonal to prepare state (SQLFreeStmt(SQL_UNBIND) exists for it).
        unsafe {
            let (env, conn, stmt) = alloc_connected_stmt::<MockTxnDeleteBackend>("DRIVER=mock;");
            {
                let handle =
                    as_handle_ref::<StatementHandle<MockTxnDeleteBackend>>(stmt).expect("valid");
                handle.cursor_name = Some("C1".to_string());
            }

            let ret = sql_end_tran::<MockTxnDeleteBackend>(
                HandleType::Dbc as i16,
                conn,
                CompletionType::Commit as i16,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let handle =
                as_handle_ref::<StatementHandle<MockTxnDeleteBackend>>(stmt).expect("valid");
            assert_eq!(
                handle.cursor_name.as_deref(),
                Some("C1"),
                "SQL_CB_DELETE cleared the cursor name"
            );

            cleanup_connected::<MockTxnDeleteBackend>(env, conn, stmt);
        }
    }

    /// Allocates one environment with two connected connections. The first is
    /// opened with `ENDTRANFAIL=1` so its `end_tran` fails; the second
    /// succeeds. Each gets a statement with an open cursor.
    unsafe fn alloc_env_two_conns<B: crate::backend::Backend>() -> (
        *mut c_void,
        *mut c_void,
        *mut c_void,
        *mut c_void,
        *mut c_void,
    ) {
        let mut env: *mut c_void = std::ptr::null_mut();
        let _ = unsafe {
            sql_alloc_handle::<B>(HandleType::Env as i16, std::ptr::null_mut(), &mut env)
        };

        let make = |conn_str: &str| -> (*mut c_void, *mut c_void) {
            let mut conn: *mut c_void = std::ptr::null_mut();
            let _ = unsafe { sql_alloc_handle::<B>(HandleType::Dbc as i16, env, &mut conn) };
            let mut wide: Vec<u16> = conn_str.encode_utf16().collect();
            wide.push(0);
            let _ = unsafe {
                crate::ffi::connect::sql_driver_connect_w::<B>(
                    conn,
                    std::ptr::null_mut(),
                    wide.as_ptr(),
                    crate::types::SQL_NTS as i16,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    0,
                )
            };
            let mut stmt: *mut c_void = std::ptr::null_mut();
            let _ = unsafe { sql_alloc_handle::<B>(HandleType::Stmt as i16, conn, &mut stmt) };
            let handle = unsafe { as_handle_ref::<crate::handles::StatementHandle<B>>(stmt) }
                .expect("valid");
            handle.statement = Some(crate::handles::StatementData::Synthetic(
                crate::synthetic::SyntheticStatement::new(vec![], vec![]),
            ));
            (conn, stmt)
        };

        let (conn_fail, stmt_fail) = make("DRIVER=mock;ENDTRANFAIL=1;");
        let (conn_ok, stmt_ok) = make("DRIVER=mock;");

        (env, conn_fail, stmt_fail, conn_ok, stmt_ok)
    }

    #[test]
    fn end_tran_env_attempts_every_connection_after_a_failure() {
        // Spec: "the driver will attempt to commit or roll back transactions
        // ... on all connections that are in a connected state on that
        // environment." A failure on one connection must not skip the rest.
        unsafe {
            let (env, conn_fail, stmt_fail, conn_ok, stmt_ok) =
                alloc_env_two_conns::<MockTxnCloseBackend>();

            let ret = sql_end_tran::<MockTxnCloseBackend>(
                HandleType::Env as i16,
                env,
                CompletionType::Rollback as i16,
            );
            assert_eq!(ret, SqlReturn::ERROR, "a failing connection must surface");

            // The second connection was still processed: SQL_CB_CLOSE means its
            // cursor is gone.
            let ok_stmt =
                as_handle_ref::<crate::handles::StatementHandle<MockTxnCloseBackend>>(stmt_ok)
                    .expect("valid");
            assert!(
                ok_stmt.statement.is_none(),
                "the loop stopped at the first failing connection"
            );

            // The failing connection carries its own diagnostic, so the
            // application can find out which connection failed.
            let failed =
                as_handle_ref::<ConnectionHandle<MockTxnCloseBackend>>(conn_fail).expect("valid");
            assert_eq!(
                failed.diagnostics.len(),
                1,
                "no per-connection diagnostic on the failing connection"
            );

            let _ = sql_free_handle::<MockTxnCloseBackend>(HandleType::Stmt as i16, stmt_fail);
            let _ = sql_free_handle::<MockTxnCloseBackend>(HandleType::Stmt as i16, stmt_ok);
            let _ = crate::ffi::connect::sql_disconnect::<MockTxnCloseBackend>(conn_fail);
            let _ = crate::ffi::connect::sql_disconnect::<MockTxnCloseBackend>(conn_ok);
            let _ = sql_free_handle::<MockTxnCloseBackend>(HandleType::Dbc as i16, conn_fail);
            let _ = sql_free_handle::<MockTxnCloseBackend>(HandleType::Dbc as i16, conn_ok);
            let _ = sql_free_handle::<MockTxnCloseBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn end_tran_env_applies_cursor_behavior_only_where_it_succeeded() {
        unsafe {
            let (env, conn_fail, stmt_fail, conn_ok, stmt_ok) =
                alloc_env_two_conns::<MockTxnCloseBackend>();

            let _ = sql_end_tran::<MockTxnCloseBackend>(
                HandleType::Env as i16,
                env,
                CompletionType::Commit as i16,
            );

            let failed_stmt =
                as_handle_ref::<crate::handles::StatementHandle<MockTxnCloseBackend>>(stmt_fail)
                    .expect("valid");
            assert!(
                failed_stmt.statement.is_some(),
                "cursor state destroyed on a connection whose end_tran failed"
            );

            let _ = sql_free_handle::<MockTxnCloseBackend>(HandleType::Stmt as i16, stmt_fail);
            let _ = sql_free_handle::<MockTxnCloseBackend>(HandleType::Stmt as i16, stmt_ok);
            let _ = crate::ffi::connect::sql_disconnect::<MockTxnCloseBackend>(conn_fail);
            let _ = crate::ffi::connect::sql_disconnect::<MockTxnCloseBackend>(conn_ok);
            let _ = sql_free_handle::<MockTxnCloseBackend>(HandleType::Dbc as i16, conn_fail);
            let _ = sql_free_handle::<MockTxnCloseBackend>(HandleType::Dbc as i16, conn_ok);
            let _ = sql_free_handle::<MockTxnCloseBackend>(HandleType::Env as i16, env);
        }
    }
}
