//! Generic implementation of SQLEndTran.

use std::ffi::c_void;

use odbc_sys::HandleType;

use crate::backend::{Backend, StatementBackend};
use crate::errors::{IntoOdbc, OdbcError};
use crate::handles::{ConnectionHandle, EnvironmentHandle, as_handle_ref};
use crate::panic::panic_safe;
use crate::types::{SqlReturn, SqlState, completion_type_from_raw, handle_type_from_raw};

/// Apply the backend's declared cursor behaviour to every statement on a
/// connection, after a successful `B::end_tran`.
///
/// Mirrors the `SQLEndTran` statement transition table:
///
/// - `SQL_CB_DELETE` (footnote `[1]`) returns prepared statements to the
///   allocated state S1: the backend statement, the access plan
///   (`prepared_sql`) and the parameter count are all dropped.
/// - `SQL_CB_CLOSE` (footnote `[2]`) closes the cursor and returns S4→S2 and
///   S5-S7→S3, leaving the prepared states S2/S3 **unchanged**. Core therefore
///   calls [`StatementBackend::close_cursor`] and keeps `stmt.statement`:
///   dropping it would send a prepared-but-never-executed statement (S2, which
///   has a `statement` because `SQLPrepare` stores one) back to S1, and
///   `SQLNumResultCols` would then fail with `HY010` where the spec allows it.
///   Note that `close_cursor` defaults to a no-op, so a backend declaring
///   `Close` must implement it — see [`Backend::cursor_commit_behavior`].
///   `StatementHandle::cursor_open` is cleared either way, which is what makes
///   the statement legal for a new `SQLExecDirect`, catalog call or
///   `SQLSetStmtAttr(SQL_ATTR_CURSOR_TYPE)` afterwards, exactly as S4→S2 and
///   S5-S7→S3 require.
/// - `SQL_CB_PRESERVE` (footnote `[3]`) is `--` in every state and changes
///   nothing.
///
/// Binding state, parameter bindings and the cursor name are deliberately
/// untouched: the spec keeps them orthogonal to prepare state, which is what
/// `SQLFreeStmt(SQL_UNBIND)` and `SQL_RESET_PARAMS` are for.
///
/// `data_at_exec` is cleared under `Delete` only, and purely to keep core's own
/// state self-consistent: `Delete` clears `param_count`, and
/// [`crate::ffi::params::sql_param_data`] builds its parameter vector from
/// `1..=param_count`, so a surviving data-at-execution sequence would execute
/// with zero parameters and silently discard everything the application had
/// streamed via `SQLPutData`. Clearing it turns that into `sql_param_data`'s
/// own "no data-at-execution operation in progress" error instead. No
/// driver-side `HY010` check is added for the need-data states S8-S10: the
/// transition table marks them `(HY010)`, i.e. Driver-Manager-detected.
///
/// # Safety
///
/// `conn_token` must be a live connection handle.
unsafe fn apply_cursor_behavior<B: Backend>(
    conn_token: *mut c_void,
    behavior: crate::types::CursorBehavior,
) -> Result<(), OdbcError> {
    use crate::types::CursorBehavior;

    if behavior == CursorBehavior::Preserve {
        return Ok(());
    }

    // An owned snapshot: freeing a statement mid-walk cannot shift the
    // sequence under this loop, because there is no shared list to shift.
    let statements = crate::handles::registry::registry().children_of(conn_token);

    tracing::debug!(
        "SQLEndTran: applying {:?} to {} statement(s)",
        behavior,
        statements.len()
    );

    let mut first_close_err: Option<OdbcError> = None;

    for stmt_ptr in statements {
        // SAFETY: stmt_ptr was registered by sql_alloc_handle and remains valid
        // while the connection handle is alive; the tag is validated here.
        let Ok(stmt) = (unsafe { as_handle_ref::<crate::handles::StatementHandle<B>>(stmt_ptr) })
        else {
            tracing::warn!("SQLEndTran: skipping statement with an invalid tag");
            continue;
        };

        if behavior == CursorBehavior::Close {
            // Close the cursor but keep the statement: footnote [2] leaves the
            // prepared states S2/S3 unchanged.
            if let Some(statement) = stmt.statement.as_mut()
                && let Err(e) = statement.close_cursor()
            {
                // Recorded and carried, not swallowed: under SQL_CB_CLOSE this
                // call *is* the cursor close the application was promised. The
                // loop still runs to completion so the remaining statements are
                // not left in a worse state than the one that failed, and the
                // first failure is what SQLEndTran reports.
                tracing::warn!("SQLEndTran: close_cursor failed: {e}");
                stmt.diagnostics.push(&e);
                first_close_err.get_or_insert(e);
            }
            stmt.cursor_open = false;
        } else {
            // Delete (Preserve returned early above): back to S1.
            stmt.discard_result_set();
            stmt.prepared_sql = None;
            stmt.param_count = None;
            stmt.data_at_exec = None;
        }
    }

    match first_close_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Generic implementation of SQLEndTran.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlendtran-function>
///
/// Commits or rolls back the active transaction on the given connection or on
/// all connections of the given environment.
///
/// With `SQL_HANDLE_ENV`, every connected connection is attempted even if an
/// earlier one fails, per the spec's "the driver will attempt to commit or roll
/// back transactions ... on all connections that are in a connected state on
/// that environment". The first error is returned and each failing connection
/// receives its own diagnostic record, so the application can do what the spec
/// tells it to and "call **SQLGetDiagRec** for each connection" to find out
/// which ones failed. Every visited connection's queue is cleared before that,
/// so a record left over from an earlier call cannot be read as this call's
/// outcome. Note that unixODBC never takes this
/// path — it loops over connections itself and calls the driver with
/// `SQL_HANDLE_DBC` — but the Windows Driver Manager does pass the driver's
/// environment handle straight through.
///
/// Core does not track autocommit state and calls `B::end_tran`
/// unconditionally. The spec has the Driver Manager suppress the call while a
/// connection is in autocommit mode, so a driver normally never sees one; a
/// backend that can be reached another way is responsible for treating a
/// commit or rollback with no open transaction as a no-op.
///
/// # Cursor behaviour
///
/// On success this applies the backend's declared cursor behaviour to every
/// statement on each affected connection:
/// [`Backend::cursor_commit_behavior`] for `SQL_COMMIT`,
/// [`Backend::cursor_rollback_behavior`] for `SQL_ROLLBACK`. The same values
/// are what `SQLGetInfoW` reports for `SQL_CURSOR_COMMIT_BEHAVIOR` and
/// `SQL_CURSOR_ROLLBACK_BEHAVIOR`, so an application is never told one thing
/// and given another.
///
/// Nothing is applied when `B::end_tran` fails. Core deliberately does not
/// discriminate on the SQLSTATE the backend reports, even though some of them
/// do say what happened: `25S03`, `40001` and `40002` all mean the transaction
/// was rolled back, so under those the cursor behaviour arguably applies.
/// Acting on that would make the outcome depend on a backend classifying its
/// client library's errors precisely, and the cost of the two mistakes is not
/// symmetric — applying the behaviour after a transaction that did *not* end
/// destroys cursor state the application may still need, while skipping it
/// after one that did leaves stale state that the spec's Suspended State
/// section expects `SQLDisconnect` to clean up anyway (`sql_disconnect` frees
/// every statement on the connection).
///
/// # Parameters
///
/// - `handle_type`: Must be `SQL_HANDLE_ENV` (1) or `SQL_HANDLE_DBC` (2). The
///   Driver Manager rejects any other value with SQLSTATE HY092; should one
///   reach the driver anyway, it returns `SQL_INVALID_HANDLE` (see the HY092
///   entry under Spec compliance).
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
/// - **25S01** — Transaction state unknown. Not applicable. This is a
///   *distributed* transaction code: it reports that the driver could not
///   guarantee the outcome of a global transaction. The spec is explicit that
///   no such guarantee is expected across an environment's connections —
///   "The Driver Manager does not simulate a global transaction across all
///   connections and therefore does not use two-phase commit protocols" — and
///   core has no distributed-transaction support to originate it from. A
///   backend enrolled in a real distributed transaction can surface it via
///   `OdbcError`.
///
///   Note this is *not* about the environment-level loop below, which attempts
///   every connected connection and reports the first failure while recording
///   a diagnostic on each failing connection.
///
/// - **25S02** — Transaction is still active. Like 25S01, a distributed
///   transaction code. Core never originates it; a backend enrolled in a
///   global transaction can surface it via `OdbcError`.
///
/// - **25S03** — Transaction is rolled back. Distributed transaction code; see
///   25S02. Core never originates it. Note that a backend returning this does
///   mean the transaction ended, but core still skips the cursor-behaviour
///   step on any error — see the cursor behaviour section below.
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
/// - **HY092** — Invalid attribute/option identifier. Not returned by the
///   driver: the spec annotates it **(DM)**, so it is the Driver Manager that
///   rejects a `HandleType` other than `SQL_HANDLE_ENV` or `SQL_HANDLE_DBC`
///   with this SQLSTATE. Should such a value reach the driver anyway, the `_`
///   arm returns `SQL_INVALID_HANDLE` (via `OdbcError::InvalidHandle`) with no
///   SQLSTATE record — the handle cannot be trusted to carry a diagnostic
///   queue matching the claimed type.
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
    // the closure. conn_ptr values from children_of are valid ConnectionHandle<B>
    // pointers registered during sql_alloc_handle and still alive while the env lives.
    let ret = unsafe {
        panic_safe::<B, _>(handle, || {
            match handle_type_from_raw(handle_type) {
                Some(HandleType::Env) => {
                    let env = as_handle_ref::<EnvironmentHandle<B>>(handle)?;
                    // Spec: clear diagnostics at the start of each ODBC call.
                    env.diagnostics.clear();
                    let behavior = if commit {
                        B::cursor_commit_behavior()
                    } else {
                        B::cursor_rollback_behavior()
                    };

                    let conn_ptrs = crate::handles::registry::registry().children_of(handle);

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
                        let conn = match as_handle_ref::<ConnectionHandle<B>>(conn_ptr) {
                            Ok(conn) => conn,
                            Err(_) => {
                                tracing::warn!(
                                    "SQLEndTran: connection {:?} failed tag validation",
                                    conn_ptr
                                );
                                // Not `OdbcError::InvalidHandle`: the input
                                // handle was valid, so returning
                                // SQL_INVALID_HANDLE would misreport the
                                // call — and `panic_safe` pushes no
                                // diagnostic for that variant, leaving
                                // nothing to explain the failure. This is a
                                // corrupt entry in the environment's own
                                // connection list, i.e. a general error.
                                first_err.get_or_insert_with(|| {
                                    OdbcError::general(
                                        "Environment holds a connection entry that failed \
                                             handle validation",
                                        SqlState::general_error(),
                                    )
                                });
                                continue;
                            }
                        };

                        // Spec: clear diagnostics at the start of each ODBC
                        // call. Done per visited connection too, before
                        // anything can push: the application is told to call
                        // SQLGetDiagRec on each connection to find which one
                        // failed, so a record left over from an earlier call
                        // would blame a connection that committed fine.
                        conn.diagnostics.clear();

                        let Some(ref connection) = conn.connection else {
                            // Not connected: "Connections that are not active do
                            // not affect the transaction."
                            continue;
                        };

                        match B::end_tran(connection, commit).into_odbc() {
                            Ok(()) => {
                                // SAFETY: children_of(conn_ptr) holds live
                                // StatementHandle<B> allocations; tags are
                                // validated inside. This call is within the
                                // outer `unsafe { panic_safe(...) }` closure.
                                //
                                // The transaction itself ended, but under
                                // SQL_CB_CLOSE a failure here means a cursor the
                                // application was promised would close did not.
                                // It joins the same per-connection reporting as
                                // a failed end_tran.
                                if let Err(e) = apply_cursor_behavior::<B>(conn_ptr, behavior) {
                                    first_err.get_or_insert(e);
                                }
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
                    // Spec: clear diagnostics at the start of each ODBC call.
                    conn.diagnostics.clear();
                    // Spec 08003: Connection not open.
                    let Some(ref connection) = conn.connection else {
                        return Err(OdbcError::NotConnected);
                    };
                    B::end_tran(connection, commit).into_odbc()?;
                    // Only on success. Core does not discriminate on the
                    // backend's SQLSTATE even though 25S03/40001/40002 do say
                    // the transaction ended; see the "Nothing is applied when
                    // B::end_tran fails" paragraph in the doc comment.
                    let behavior = if commit {
                        B::cursor_commit_behavior()
                    } else {
                        B::cursor_rollback_behavior()
                    };
                    // SAFETY: children_of(handle) holds live StatementHandle<B>
                    // allocations; tags are validated inside.
                    //
                    // Propagated: the commit or rollback succeeded, but under
                    // SQL_CB_CLOSE this is the call that closes the cursors, and
                    // reporting SQL_SUCCESS for a close that failed would tell
                    // the application its cursors are gone when they are not.
                    // The diagnostic was already pushed per statement.
                    apply_cursor_behavior::<B>(handle, behavior)?;
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
    use crate::handles::{StatementData, StatementHandle};
    use crate::synthetic::SyntheticStatement;
    use crate::test_utils::{
        MockBackend, MockFailingCloseBackend, MockFailingCloseStatement, MockTxnCloseBackend,
        MockTxnDeleteBackend, MockTxnPreserveBackend,
    };
    use crate::types::{
        ColumnDescriptor, ColumnValue, CompletionType, FetchResult, Nullable, SQL_NTS, SqlDataType,
    };

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
        handle.set_result_set(crate::handles::StatementData::Synthetic(one_row_synthetic()));
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

    /// A one-column, one-row synthetic result set. Lets a test observe whether
    /// `close_cursor` actually ran: `SyntheticStatement::close_cursor` rewinds
    /// the cursor, so an exhausted result set becomes fetchable again.
    fn one_row_synthetic() -> SyntheticStatement {
        SyntheticStatement::new(
            vec![ColumnDescriptor {
                name: "val".to_string(),
                type_name: String::new(),
                sql_type: SqlDataType::INTEGER,
                precision: 10,
                scale: 0,
                nullable: Nullable::SqlNullable,
                ..Default::default()
            }],
            vec![vec![ColumnValue::I32(1)]],
        )
    }

    #[test]
    fn end_tran_close_closes_the_cursor_and_keeps_the_statement() {
        // SQL_CB_CLOSE (transition-table footnote [2]): cursors closed,
        // prepared statements stay prepared. Core closes the cursor through
        // StatementBackend::close_cursor rather than dropping the statement.
        unsafe {
            let (env, conn, stmt) = alloc_connected_stmt::<MockTxnCloseBackend>("DRIVER=mock;");
            {
                let handle =
                    as_handle_ref::<StatementHandle<MockTxnCloseBackend>>(stmt).expect("valid");
                handle.set_result_set(StatementData::Synthetic(one_row_synthetic()));
                let data = handle.statement.as_mut().expect("statement");
                assert_eq!(data.fetch().expect("fetch"), FetchResult::Row);
                assert_eq!(data.fetch().expect("fetch"), FetchResult::NoData);
            }

            let ret = sql_end_tran::<MockTxnCloseBackend>(
                HandleType::Dbc as i16,
                conn,
                CompletionType::Commit as i16,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let handle =
                as_handle_ref::<StatementHandle<MockTxnCloseBackend>>(stmt).expect("valid");
            let data = handle
                .statement
                .as_mut()
                .expect("SQL_CB_CLOSE dropped the statement instead of closing its cursor");
            assert_eq!(
                data.fetch().expect("re-fetch"),
                FetchResult::Row,
                "SQL_CB_CLOSE did not close the cursor"
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
    fn end_tran_close_keeps_a_prepared_but_unexecuted_statement() {
        // ODBC state S2: prepared, never executed, no cursor open. SQLPrepare
        // stores a backend statement, so S2 has `statement == Some`, but
        // footnote [2] (SQL_CB_CLOSE) leaves S2 unchanged — there is no cursor
        // to close there. Dropping the statement would send it back to S1 and
        // make a subsequent SQLNumResultCols fail with HY010, though the spec
        // says that call is legal in S2.
        unsafe {
            let (env, conn, stmt) = alloc_connected_stmt::<MockTxnCloseBackend>("DRIVER=mock;");
            {
                // Undo the helper's cursor-open setup: this test wants the
                // state the real SQLPrepareW path produces, nothing more.
                let handle =
                    as_handle_ref::<StatementHandle<MockTxnCloseBackend>>(stmt).expect("valid");
                handle.discard_result_set();
                handle.prepared_sql = None;
                handle.param_count = None;
            }

            let mut sql: Vec<u16> = "SELECT a, b FROM t".encode_utf16().collect();
            sql.push(0);
            let ret = crate::ffi::execute::sql_prepare_w::<MockTxnCloseBackend>(
                stmt,
                sql.as_ptr(),
                SQL_NTS,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let ret = sql_end_tran::<MockTxnCloseBackend>(
                HandleType::Dbc as i16,
                conn,
                CompletionType::Commit as i16,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let handle =
                as_handle_ref::<StatementHandle<MockTxnCloseBackend>>(stmt).expect("valid");
            assert!(
                handle.statement.is_some(),
                "SQL_CB_CLOSE dropped a prepared-but-never-executed statement (S2)"
            );
            assert_eq!(handle.prepared_sql.as_deref(), Some("SELECT a, b FROM t"));
            assert!(
                !handle.cursor_open,
                "S2 has no cursor open, before or after SQLEndTran"
            );

            // The point of keeping the statement: SQLNumResultCols is legal in
            // S2 and must not fail with HY010.
            let mut cols: i16 = -1;
            let ret = crate::ffi::cursor::sql_num_result_cols::<MockTxnCloseBackend>(
                stmt,
                &mut cols as *mut i16,
            );
            assert_eq!(
                ret,
                SqlReturn::SUCCESS,
                "SQLNumResultCols failed in S2 after SQLEndTran"
            );

            cleanup_connected::<MockTxnCloseBackend>(env, conn, stmt);
        }
    }

    /// Asserts that the statement handle's most recent diagnostic is `24000`.
    unsafe fn assert_invalid_cursor_state<B: crate::backend::Backend>(stmt: *mut c_void) {
        let handle = unsafe { as_handle_ref::<StatementHandle<B>>(stmt) }.expect("valid");
        let rec = handle.diagnostics.get(0).expect("a diagnostic record");
        assert_eq!(
            rec.sqlstate.as_str(),
            crate::types::sql_state::INVALID_CURSOR_STATE,
            "expected 24000, got {}",
            rec.sqlstate.as_str()
        );
    }

    #[test]
    fn end_tran_close_lets_the_statement_execute_again() {
        // The transition table sends S5-S7 to S3 and S4 to S2 under
        // SQL_CB_CLOSE: no cursor is open afterwards, so SQLExecDirect is legal.
        // The trap is that SQL_CB_CLOSE deliberately keeps the statement, so
        // anything reading `statement.is_some()` as "a cursor is open" answers
        // 24000 to a statement the spec has just made executable again.
        unsafe {
            let (env, conn, stmt) = alloc_connected_stmt::<MockTxnCloseBackend>("DRIVER=mock;");

            let ret = sql_end_tran::<MockTxnCloseBackend>(
                HandleType::Dbc as i16,
                conn,
                CompletionType::Commit as i16,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let mut sql: Vec<u16> = "SELECT 1".encode_utf16().collect();
            sql.push(0);
            let ret = crate::ffi::execute::sql_exec_direct_w::<MockTxnCloseBackend>(
                stmt,
                sql.as_ptr(),
                SQL_NTS,
            );
            assert_eq!(
                ret,
                SqlReturn::SUCCESS,
                "SQLExecDirect was rejected after SQL_CB_CLOSE closed the cursor"
            );

            cleanup_connected::<MockTxnCloseBackend>(env, conn, stmt);
        }
    }

    #[test]
    fn end_tran_close_lets_a_catalog_function_run_again() {
        // Same transition, same reasoning, for the catalog functions — they
        // share the "cursor already open" guard with SQLExecDirect.
        unsafe {
            let (env, conn, stmt) = alloc_connected_stmt::<MockTxnCloseBackend>("DRIVER=mock;");

            let ret = sql_end_tran::<MockTxnCloseBackend>(
                HandleType::Dbc as i16,
                conn,
                CompletionType::Commit as i16,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let ret = crate::ffi::metadata::sql_statistics_w::<MockTxnCloseBackend>(
                stmt,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                crate::types::SQL_INDEX_ALL,
                crate::types::SQL_QUICK,
            );
            assert_eq!(
                ret,
                SqlReturn::SUCCESS,
                "SQLStatistics was rejected after SQL_CB_CLOSE closed the cursor"
            );

            cleanup_connected::<MockTxnCloseBackend>(env, conn, stmt);
        }
    }

    #[test]
    fn end_tran_close_lets_the_cursor_type_be_set_again() {
        // SQLSetStmtAttr(SQL_ATTR_CURSOR_TYPE) is rejected with 24000 only while
        // a cursor is open. After SQL_CB_CLOSE there is none.
        unsafe {
            let (env, conn, stmt) = alloc_connected_stmt::<MockTxnCloseBackend>("DRIVER=mock;");
            {
                // The helper also marks the statement prepared, which this
                // attribute rejects with HY011 for its own (correct) reasons.
                // Clear it so the test observes the 24000 guard alone.
                let handle =
                    as_handle_ref::<StatementHandle<MockTxnCloseBackend>>(stmt).expect("valid");
                handle.prepared_sql = None;
            }

            let ret = sql_end_tran::<MockTxnCloseBackend>(
                HandleType::Dbc as i16,
                conn,
                CompletionType::Commit as i16,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let ret = crate::ffi::stmt_attr::sql_set_stmt_attr_w::<MockTxnCloseBackend>(
                stmt,
                crate::types::StatementAttribute::CursorType as i32,
                crate::types::SQL_CURSOR_FORWARD_ONLY as *mut c_void,
                0,
            );
            assert_eq!(
                ret,
                SqlReturn::SUCCESS,
                "SQL_ATTR_CURSOR_TYPE was rejected after SQL_CB_CLOSE closed the cursor"
            );

            cleanup_connected::<MockTxnCloseBackend>(env, conn, stmt);
        }
    }

    #[test]
    fn end_tran_reports_a_failed_cursor_close_rather_than_success() {
        // Under SQL_CB_CLOSE, `close_cursor` is the only thing that closes the
        // cursor. If it fails, the application's cursors are still open, and
        // reporting SQL_SUCCESS would tell it the opposite.
        unsafe {
            let (env, conn, stmt) = alloc_connected_stmt::<MockFailingCloseBackend>("DRIVER=mock;");

            // Swap the synthetic result set for a backend one whose close fails.
            let handle =
                as_handle_ref::<StatementHandle<MockFailingCloseBackend>>(stmt).expect("valid");
            handle.set_result_set(crate::handles::StatementData::Backend(
                MockFailingCloseStatement,
            ));

            let ret = sql_end_tran::<MockFailingCloseBackend>(
                HandleType::Dbc as i16,
                conn,
                CompletionType::Commit as i16,
            );
            assert_eq!(
                ret,
                SqlReturn::ERROR,
                "SQLEndTran reported success though the cursor close failed"
            );

            // The statement carries the diagnostic, which is where the spec
            // tells an application to look.
            let handle =
                as_handle_ref::<StatementHandle<MockFailingCloseBackend>>(stmt).expect("valid");
            let rec = handle
                .diagnostics
                .get(0)
                .expect("a diagnostic per statement");
            assert_eq!(rec.sqlstate.as_str(), "08S01");
            assert!(
                rec.message.contains("mock close_cursor failure"),
                "expected the backend's own message, got {:?}",
                rec.message
            );

            cleanup_connected::<MockFailingCloseBackend>(env, conn, stmt);
        }
    }

    #[test]
    fn end_tran_close_leaves_no_cursor_for_sql_close_cursor() {
        // The mirror image: SQLCloseCursor needs an *open* cursor, so after
        // SQL_CB_CLOSE has closed it the call is 24000, not SUCCESS.
        unsafe {
            let (env, conn, stmt) = alloc_connected_stmt::<MockTxnCloseBackend>("DRIVER=mock;");

            let ret = sql_end_tran::<MockTxnCloseBackend>(
                HandleType::Dbc as i16,
                conn,
                CompletionType::Commit as i16,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let ret = crate::ffi::cursor::sql_close_cursor::<MockTxnCloseBackend>(stmt);
            assert_eq!(
                ret,
                SqlReturn::ERROR,
                "SQLCloseCursor reported success with no cursor open"
            );
            assert_invalid_cursor_state::<MockTxnCloseBackend>(stmt);

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
    fn end_tran_delete_clears_the_data_at_execution_state() {
        // SQL_CB_DELETE clears param_count, and sql_param_data builds its
        // parameter vector from 1..=param_count. A surviving DAE state would
        // therefore execute with zero parameters and silently discard
        // everything the application streamed via SQLPutData. Clearing it makes
        // sql_param_data report "no data-at-execution operation in progress"
        // instead.
        unsafe {
            let (env, conn, stmt) = alloc_connected_stmt::<MockTxnDeleteBackend>("DRIVER=mock;");
            {
                let handle =
                    as_handle_ref::<StatementHandle<MockTxnDeleteBackend>>(stmt).expect("valid");
                handle.param_count = Some(1);
                handle.data_at_exec = Some(crate::handles::DataAtExecState {
                    pending_params: std::collections::VecDeque::new(),
                    current_param: Some(1),
                    buffer: vec![0xAB],
                    collected_values: std::collections::HashMap::new(),
                    sql: "INSERT INTO t VALUES (?)".to_string(),
                });
            }

            let ret = sql_end_tran::<MockTxnDeleteBackend>(
                HandleType::Dbc as i16,
                conn,
                CompletionType::Commit as i16,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let handle =
                as_handle_ref::<StatementHandle<MockTxnDeleteBackend>>(stmt).expect("valid");
            assert!(
                handle.data_at_exec.is_none(),
                "SQL_CB_DELETE left a data-at-execution sequence pending with no param_count"
            );

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
            assert_eq!(handle.param_count, Some(0));

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

    /// Body shared by the three `end_tran_failure_*` tests: a failing
    /// `B::end_tran` must leave `statement` and `prepared_sql` exactly as they
    /// were, whatever the backend's declared cursor behaviour is. On error core
    /// cannot tell whether the transaction ended, so it must not destroy cursor
    /// state the application may still need.
    unsafe fn assert_failed_end_tran_leaves_state_untouched<B: crate::backend::Backend>() {
        unsafe {
            let (env, conn, stmt) = alloc_connected_stmt::<B>("DRIVER=mock;ENDTRANFAIL=1;");
            let ret =
                sql_end_tran::<B>(HandleType::Dbc as i16, conn, CompletionType::Commit as i16);
            assert_eq!(ret, SqlReturn::ERROR);

            let handle = as_handle_ref::<StatementHandle<B>>(stmt).expect("valid");
            assert!(
                handle.statement.is_some(),
                "cursor state destroyed on a failed SQLEndTran"
            );
            assert_eq!(handle.prepared_sql.as_deref(), Some("SELECT 1"));
            assert_eq!(handle.param_count, Some(0));

            cleanup_connected::<B>(env, conn, stmt);
        }
    }

    #[test]
    fn end_tran_failure_leaves_cursor_state_untouched_under_delete() {
        unsafe { assert_failed_end_tran_leaves_state_untouched::<MockTxnDeleteBackend>() }
    }

    #[test]
    fn end_tran_failure_leaves_cursor_state_untouched_under_close() {
        unsafe { assert_failed_end_tran_leaves_state_untouched::<MockTxnCloseBackend>() }
    }

    #[test]
    fn end_tran_failure_leaves_cursor_state_untouched_under_preserve() {
        unsafe { assert_failed_end_tran_leaves_state_untouched::<MockTxnPreserveBackend>() }
    }

    #[test]
    fn end_tran_delete_keeps_bindings_and_cursor_name() {
        // The spec says nothing about binding state here, and ODBC keeps it
        // orthogonal to prepare state (SQLFreeStmt(SQL_UNBIND) and
        // SQL_RESET_PARAMS exist for it).
        unsafe {
            let (env, conn, stmt) = alloc_connected_stmt::<MockTxnDeleteBackend>("DRIVER=mock;");
            {
                let handle =
                    as_handle_ref::<StatementHandle<MockTxnDeleteBackend>>(stmt).expect("valid");
                handle.cursor_name = Some("C1".to_string());
                handle.bindings.insert(
                    1,
                    crate::handles::ColumnBinding {
                        target_type: crate::types::CDataType::SLong,
                        target_value_ptr: std::ptr::null_mut(),
                        buffer_length: 4,
                        str_len_or_ind_ptr: std::ptr::null_mut(),
                    },
                );
                handle.param_bindings.insert(
                    1,
                    crate::handles::ParameterBinding {
                        input_output_type: crate::types::ParamType::Input,
                        c_type: crate::types::CDataType::SLong,
                        sql_type: crate::types::SqlDataType::INTEGER,
                        col_size: 10,
                        decimal_digits: 0,
                        value_ptr: std::ptr::null_mut(),
                        buffer_length: 4,
                        str_len_or_ind_ptr: std::ptr::null_mut(),
                    },
                );
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
            assert!(
                handle.bindings.contains_key(&1),
                "SQL_CB_DELETE cleared the column bindings"
            );
            assert!(
                handle.param_bindings.contains_key(&1),
                "SQL_CB_DELETE cleared the parameter bindings"
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
            handle.set_result_set(crate::handles::StatementData::Synthetic(one_row_synthetic()));
            (conn, stmt)
        };

        let (conn_fail, stmt_fail) = make("DRIVER=mock;ENDTRANFAIL=1;");
        let (conn_ok, stmt_ok) = make("DRIVER=mock;");

        (env, conn_fail, stmt_fail, conn_ok, stmt_ok)
    }

    /// Teardown for [`alloc_env_two_conns`], in the order Miri's leak reporting
    /// requires: statements, then a disconnect per connection, then the
    /// connection handles, then the environment.
    unsafe fn free_env_two_conns<B: crate::backend::Backend>(
        env: *mut c_void,
        conn_fail: *mut c_void,
        stmt_fail: *mut c_void,
        conn_ok: *mut c_void,
        stmt_ok: *mut c_void,
    ) {
        unsafe {
            let _ = sql_free_handle::<B>(HandleType::Stmt as i16, stmt_fail);
            let _ = sql_free_handle::<B>(HandleType::Stmt as i16, stmt_ok);
            let _ = crate::ffi::connect::sql_disconnect::<B>(conn_fail);
            let _ = crate::ffi::connect::sql_disconnect::<B>(conn_ok);
            let _ = sql_free_handle::<B>(HandleType::Dbc as i16, conn_fail);
            let _ = sql_free_handle::<B>(HandleType::Dbc as i16, conn_ok);
            let _ = sql_free_handle::<B>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn end_tran_env_attempts_every_connection_after_a_failure() {
        // Spec: "the driver will attempt to commit or roll back transactions
        // ... on all connections that are in a connected state on that
        // environment." A failure on one connection must not skip the rest.
        unsafe {
            let (env, conn_fail, stmt_fail, conn_ok, stmt_ok) =
                alloc_env_two_conns::<MockTxnDeleteBackend>();

            let ret = sql_end_tran::<MockTxnDeleteBackend>(
                HandleType::Env as i16,
                env,
                CompletionType::Rollback as i16,
            );
            assert_eq!(ret, SqlReturn::ERROR, "a failing connection must surface");

            // The second connection was still processed: SQL_CB_DELETE means
            // its statement is gone.
            let ok_stmt =
                as_handle_ref::<crate::handles::StatementHandle<MockTxnDeleteBackend>>(stmt_ok)
                    .expect("valid");
            assert!(
                ok_stmt.statement.is_none(),
                "the loop stopped at the first failing connection"
            );

            // The failing connection carries its own diagnostic, so the
            // application can find out which connection failed.
            let failed =
                as_handle_ref::<ConnectionHandle<MockTxnDeleteBackend>>(conn_fail).expect("valid");
            assert_eq!(
                failed.diagnostics.len(),
                1,
                "no per-connection diagnostic on the failing connection"
            );

            free_env_two_conns::<MockTxnDeleteBackend>(env, conn_fail, stmt_fail, conn_ok, stmt_ok);
        }
    }

    #[test]
    fn end_tran_env_applies_cursor_behavior_only_where_it_succeeded() {
        unsafe {
            let (env, conn_fail, stmt_fail, conn_ok, stmt_ok) =
                alloc_env_two_conns::<MockTxnDeleteBackend>();

            let _ = sql_end_tran::<MockTxnDeleteBackend>(
                HandleType::Env as i16,
                env,
                CompletionType::Commit as i16,
            );

            let failed_stmt =
                as_handle_ref::<crate::handles::StatementHandle<MockTxnDeleteBackend>>(stmt_fail)
                    .expect("valid");
            assert!(
                failed_stmt.statement.is_some(),
                "cursor state destroyed on a connection whose end_tran failed"
            );

            free_env_two_conns::<MockTxnDeleteBackend>(env, conn_fail, stmt_fail, conn_ok, stmt_ok);
        }
    }

    #[test]
    fn end_tran_env_clears_a_stale_diagnostic_on_a_succeeding_connection() {
        // The application is told to call SQLGetDiagRec on each connection to
        // find which one failed the commit. A record left over from an earlier
        // call on a connection that then commits fine would blame the wrong
        // connection, so every visited connection's queue is cleared first.
        unsafe {
            let (env, conn_fail, stmt_fail, conn_ok, stmt_ok) =
                alloc_env_two_conns::<MockTxnPreserveBackend>();
            {
                let ok = as_handle_ref::<ConnectionHandle<MockTxnPreserveBackend>>(conn_ok)
                    .expect("valid");
                ok.diagnostics.push(&OdbcError::general(
                    "stale record from an earlier call",
                    SqlState::general_error(),
                ));
                assert_eq!(ok.diagnostics.len(), 1);
            }

            let ret = sql_end_tran::<MockTxnPreserveBackend>(
                HandleType::Env as i16,
                env,
                CompletionType::Commit as i16,
            );
            assert_eq!(ret, SqlReturn::ERROR, "the first connection still fails");

            let ok =
                as_handle_ref::<ConnectionHandle<MockTxnPreserveBackend>>(conn_ok).expect("valid");
            assert_eq!(
                ok.diagnostics.len(),
                0,
                "a stale diagnostic on a connection that committed fine was left in place, \
                 so SQLGetDiagRec blames the wrong connection"
            );

            free_env_two_conns::<MockTxnPreserveBackend>(
                env, conn_fail, stmt_fail, conn_ok, stmt_ok,
            );
        }
    }

    #[test]
    fn end_tran_dbc_clears_stale_diagnostics() {
        unsafe {
            let (env, conn, stmt) = alloc_connected_stmt::<MockTxnPreserveBackend>("DRIVER=mock;");
            {
                let handle =
                    as_handle_ref::<ConnectionHandle<MockTxnPreserveBackend>>(conn).expect("valid");
                handle.diagnostics.push(&OdbcError::general(
                    "stale record from an earlier call",
                    SqlState::general_error(),
                ));
            }

            let ret = sql_end_tran::<MockTxnPreserveBackend>(
                HandleType::Dbc as i16,
                conn,
                CompletionType::Commit as i16,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let handle =
                as_handle_ref::<ConnectionHandle<MockTxnPreserveBackend>>(conn).expect("valid");
            assert_eq!(
                handle.diagnostics.len(),
                0,
                "SQLEndTran did not clear the connection's diagnostics at entry"
            );

            cleanup_connected::<MockTxnPreserveBackend>(env, conn, stmt);
        }
    }
}
