//! Cursor and result-set entry points (`SQLNumResultCols`, `SQLRowCount`,
//! `SQLMoreResults`, `SQLCloseCursor`, `SQLCancel`, cursor names, bulk
//! operations, `SQLSetPos`).

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::backend::{Backend, StatementBackend};
use crate::errors::{IntoOdbc, OdbcError};
use crate::handles::StatementHandle;
use crate::handles::registry::{HandleKind, registry};
use crate::handles::scope::HandleScope;
use crate::panic::{panic_safe, panic_safe_unlocked};
#[cfg(test)]
use crate::types::Nullable;
use crate::types::{
    SQL_DELETE, SQL_LOCK_EXCLUSIVE, SQL_LOCK_NO_CHANGE, SQL_LOCK_UNLOCK, SQL_POSITION, SQL_REFRESH,
    SQL_UPDATE, SqlReturn, SqlState, bulk_operation_from_raw,
};
use crate::utf16::{utf16_to_string, write_utf16};

/// Global counter for auto-generating cursor names.
static CURSOR_NAME_COUNTER: AtomicU32 = AtomicU32::new(1);

/// Generic implementation of SQLNumResultCols.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlnumresultcols-function>
///
/// Returns the number of columns in the result set.
///
/// # Parameters
///
/// - `statement_handle`: \[Input\] Statement handle (`SQLHSTMT`).
/// - `column_count_ptr`: \[Output\] Pointer to a buffer in which to return the number of columns
///   in the result set. This count does not include a bound bookmark column. May be null (the
///   count is computed but not written).
///
/// # Spec compliance
///
/// Diagnostics table from the ODBC spec:
///
/// - 01000 (general warning): driver-specific informational message — not produced here.
/// - 08S01 (communication link failure): not applicable; the framework is in-process.
/// - HY000 (general error): returned via `OdbcError::general` for unexpected failures.
/// - HY001 (memory allocation error): not applicable; Rust allocation panics are caught by
///   `panic_safe`.
/// - HY008: Operation canceled; not returned here. This call makes no fallible backend call —
///   `StatementBackend::column_count` returns a plain `i16` — so there is no error for a
///   cancellation to be reported through. The asynchronous clause is inapplicable: core never
///   returns `SQL_STILL_EXECUTING`.
/// - HY010 (function sequence error): returned with SQLSTATE `HY010` when no result set is
///   available (statement not yet executed). The `(DM)` variants (async in progress, etc.) are
///   driver-manager-handled; not returned here.
/// - HY013 (memory management error): not applicable; Rust memory access cannot fail silently.
/// - HY117 (connection suspended): (driver-manager-handled; not returned here)
/// - HYT01 (connection timeout): not applicable; the framework is in-process.
/// - IM001 (driver does not support function): (driver-manager-handled; not returned here)
/// - IM017 (polling disabled in async notification mode): (driver-manager-handled; not returned here)
/// - IM018 (SQLCompleteAsync not called): (driver-manager-handled; not returned here)
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
/// `column_count_ptr` must be a valid writable pointer.
pub unsafe fn sql_num_result_cols<B: Backend>(
    statement_handle: *mut c_void,
    column_count_ptr: *mut i16,
) -> SqlReturn {
    tracing::debug!(
        "SQLNumResultCols(stmt={:?}, count_ptr={:?})",
        statement_handle,
        column_count_ptr
    );
    // SAFETY: statement_handle is either null or a valid StatementHandle<B> pointer
    // previously allocated by sql_alloc_handle. scope.get validates kind and group
    // before any cast, and panic_safe catches any panics. column_count_ptr
    // is only written through after a null check inside the closure.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, |scope| {
            let stmt = scope.get::<StatementHandle<B>>(statement_handle)?;
            stmt.diagnostics.clear();

            // Spec HY010: No result set.
            let Some(ref statement) = stmt.statement else {
                return Err(OdbcError::general(
                    "No result set available; statement not executed",
                    SqlState::function_sequence_error(),
                ));
            };

            if !column_count_ptr.is_null() {
                // SAFETY: column_count_ptr is non-null (checked above) and the
                // caller guarantees it points to a valid writable i16.
                // No narrowing here any more: `column_count` is already the
                // `SQLSMALLINT` the ABI writes, so a backend that cannot express
                // its count has to say so where it knows the real number,
                // instead of core clamping a value it cannot interpret.
                std::ptr::write_unaligned(column_count_ptr, statement.column_count());
            }

            Ok(SqlReturn::SUCCESS)
        })
    };
    tracing::debug!("SQLNumResultCols -> {:?}", ret);
    ret
}

/// The row count `SQLRowCount` reports, as an `SQLLEN`.
///
/// Shared with `SQLGetDiagField`'s `SQL_DIAG_ROW_COUNT`, because the spec makes
/// them one value: "The data in this field is also returned in the
/// *RowCountPtr* argument of **SQLRowCount**." Two computations of one number
/// is how the two come to disagree — the same reason the IRD reads through
/// `col_attr::get_column_attribute` rather than through a second table.
pub(crate) fn statement_row_count<B: Backend>(stmt: &StatementHandle<B>) -> isize {
    match stmt.statement {
        Some(ref statement) => match statement.row_count() {
            // A backend that knows the count but cannot determine it reports
            // `Some(SQL_NO_TOTAL)` itself; core does not second-guess a value it
            // was given.
            Some(n) => isize::try_from(n).unwrap_or_else(|_| {
                tracing::warn!(
                    "row count {n} does not fit SQLLEN on this target; \
                     reporting -1 (not available)"
                );
                -1
            }),
            // Not applicable to this statement — distinct from the backend
            // saying it could not work the count out.
            None => -1,
        },
        None => 0, // no statement executed
    }
}

/// Generic implementation of SQLRowCount.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlrowcount-function>
///
/// Returns the number of rows affected by an UPDATE, INSERT, or DELETE
/// statement, or -1 if unknown.
///
/// # Parameters
///
/// - `statement_handle`: \[Input\] Statement handle (`SQLHSTMT`).
/// - `row_count_ptr`: \[Output\] Points to a buffer in which to return the row count. For UPDATE,
///   INSERT, and DELETE statements the value is the number of affected rows, or -1 if not
///   available. For other statements the value is driver-defined. May be null (the count is
///   computed but not written).
///
/// # Spec compliance
///
/// Diagnostics table from the ODBC spec:
///
/// - 01000 (general warning): driver-specific informational message — not produced here.
/// - HY000 (general error): returned via `OdbcError::general` for unexpected failures.
/// - HY001 (memory allocation error): not applicable; Rust allocation panics are caught by
///   `panic_safe`.
/// - HY010 (function sequence error): the spec lists this for cases where no statement has been
///   executed yet; `SUCCESS` with row count 0 is returned in that case (no-execute path),
///   which is consistent with the spec's "driver-defined" rule for non-DML statements. The `(DM)`
///   variants (async in progress, etc.) are driver-manager-handled; not returned here.
/// - HY013 (memory management error): not applicable; Rust memory access cannot fail silently.
/// - HY117 (connection suspended): (driver-manager-handled; not returned here)
/// - HYT01 (connection timeout): not applicable; the framework is in-process.
/// - IM001 (driver does not support function): (driver-manager-handled; not returned here)
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
/// `row_count_ptr` must be a valid writable pointer.
pub unsafe fn sql_row_count<B: Backend>(
    statement_handle: *mut c_void,
    row_count_ptr: *mut isize,
) -> SqlReturn {
    tracing::debug!("SQLRowCount(stmt={:?})", statement_handle);
    // SAFETY: statement_handle is either null or a valid StatementHandle<B> pointer
    // previously allocated by sql_alloc_handle. scope.get validates kind and group
    // before any cast, and panic_safe catches any panics. row_count_ptr
    // is only written through after a null check inside the closure.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, |scope| {
            let stmt = scope.get::<StatementHandle<B>>(statement_handle)?;
            stmt.diagnostics.clear();

            let row_count = statement_row_count::<B>(stmt);

            if !row_count_ptr.is_null() {
                // SAFETY: row_count_ptr is non-null (checked above) and the
                // caller guarantees it points to a valid writable isize.
                std::ptr::write_unaligned(row_count_ptr, row_count);
            }

            Ok(SqlReturn::SUCCESS)
        })
    };
    tracing::debug!("SQLRowCount -> {:?}", ret);
    ret
}

/// Generic implementation of SQLMoreResults.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlmoreresults-function>
///
/// Always returns `SQL_NO_DATA`: only single result sets are supported, so there is never
/// a next result to make available. The current one is discarded on the way out — Appendix
/// B's `SQL_NO_DATA` entries for this function are `S1` when the statement was not prepared
/// and `S2`/`S3` when it was, which is `SQLFreeStmt(SQL_CLOSE)`'s row exactly. A statement
/// in `S1 Allocated` or `S2-S3 Prepared` is left untouched: those columns are `--` with the
/// footnote "The function always returns SQL_NO_DATA in this state".
///
/// # Parameters
///
/// - `statement_handle`: \[Input\] Statement handle (`SQLHSTMT`).
///
/// # Spec compliance
///
/// Diagnostics table from the ODBC spec:
///
/// - 01000 (general warning): driver-specific informational message — not produced here.
/// - 01S02 (option value changed): would be returned if a statement attribute changed while
///   processing a batch; not applicable, as there is never a next result to move to.
/// - 08S01 (communication link failure): **returned by this driver** when
///   `StatementBackend::close_cursor` fails with that state. Discarding the result set is
///   the transition this function performs, and for a networked data source that is a round
///   trip. The discard happens even so, per `sql_close_cursor`'s reasoning.
/// - 40001 (serialization failure): not applicable; the `Backend` trait is synchronous and
///   does not return partial batch results.
/// - 40003 (statement completion unknown): not applicable; the framework is in-process.
/// - HY000 (general error): returned via `OdbcError::general` for unexpected failures.
/// - HY001 (memory allocation error): not applicable; Rust allocation panics are caught by
///   `panic_safe`.
/// - HY008: Operation canceled; not returned here. This call reaches the backend only
///   through `StatementBackend::close_cursor`, which core does not reclassify against the
///   cancel token — `crate::cancel` is applied at the statement-producing and
///   cursor-consuming calls, not at teardown. The asynchronous clause is inapplicable: core
///   never returns `SQL_STILL_EXECUTING`.
/// - HY010 (function sequence error): (driver-manager-handled; not returned here)
/// - HY013 (memory management error): not applicable; Rust memory access cannot fail silently.
/// - HY117 (connection suspended): (driver-manager-handled; not returned here)
/// - HYT01 (connection timeout): not applicable; the framework is in-process.
/// - IM001 (driver does not support function): (driver-manager-handled; not returned here)
/// - IM017 (polling disabled in async notification mode): (driver-manager-handled; not returned here)
/// - IM018 (SQLCompleteAsync not called): (driver-manager-handled; not returned here)
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
pub unsafe fn sql_more_results<B: Backend>(statement_handle: *mut c_void) -> SqlReturn {
    tracing::debug!("SQLMoreResults(stmt={:?})", statement_handle);
    // SAFETY: statement_handle is either null or a valid StatementHandle<B> pointer
    // previously allocated by sql_alloc_handle. scope.get validates kind and group
    // before any cast, and panic_safe catches any panics.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, |scope| {
            let stmt = scope.get::<StatementHandle<B>>(statement_handle)?;
            stmt.diagnostics.clear();

            // Spec Comments: "If there was a current result set with unfetched
            // rows, SQLMoreResults discards that result set and makes the next
            // result set or count available. If all results have been processed,
            // SQLMoreResults returns SQL_NO_DATA."
            //
            // This driver never has a next result, so every call is the second
            // sentence — but the first still governs what is left behind, and
            // Appendix B says exactly what that is. Restricted to the single
            // result core produces (footnote [4], "the current result is the
            // last result"), the SQL_NO_DATA entries are `S1 [np]` / `S2 [p]`
            // from S4 and `S1 [np]` / `S3 [p]` from S5-S7 — the same pair
            // SQLFreeStmt(SQL_CLOSE)'s own row gives, so this does what that
            // option does: tell the backend, then discard. `prepared_sql`
            // survives the discard, which is what makes the `[p]` half of those
            // entries true.
            //
            // The `S1 Allocated` and `S2-S3 Prepared` columns are `--` with
            // footnote [1], "The function always returns SQL_NO_DATA in this
            // state", so a statement that has not executed is left alone. That
            // is why this reads `executed` and not `statement.is_some()`: a
            // prepared statement holds a backend statement and must keep it.
            if stmt.cursor_open || stmt.executed {
                // Gated on `cursor_open` for the same reason SQLFreeStmt's
                // SQL_CLOSE arm is: asking a backend to close a cursor it never
                // opened is not something any spec text calls for.
                let close_err = if stmt.cursor_open {
                    stmt.statement
                        .as_mut()
                        .and_then(|statement| statement.close_cursor().err())
                } else {
                    None
                };
                // Discarded even when the close failed, as in `sql_close_cursor`:
                // the application would otherwise hold a cursor it cannot clear.
                stmt.discard_result_set();
                if let Some(e) = close_err {
                    return Err(e);
                }
            }

            Ok(SqlReturn::NO_DATA)
        })
    };
    tracing::debug!("SQLMoreResults -> {:?}", ret);
    ret
}

/// Generic implementation of SQLCloseCursor.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlclosecursor-function>
///
/// Closes the cursor associated with the statement: calls
/// [`crate::backend::StatementBackend::close_cursor`] so the backend can
/// release whatever the cursor holds, then discards the result set. The
/// statement **handle** remains allocated and can be executed again — the
/// prepared SQL survives in `prepared_sql`, so a later `SQLExecute` re-prepares
/// — but the result set and its metadata do not survive the call.
///
/// # Parameters
///
/// - `statement_handle`: \[Input\] Statement handle (`SQLHSTMT`).
///
/// # Spec compliance
///
/// Diagnostics table from the ODBC spec:
///
/// - 01000 (general warning): driver-specific informational message — not produced here.
/// - 24000 (invalid cursor state): returned with SQLSTATE `24000` when no cursor is open on the
///   statement handle (ODBC 3.x driver behaviour). A statement that is only prepared, or that
///   executed without producing a result set, has no cursor to close and gets this code, as does
///   one whose cursor `SQLEndTran` already closed under `SQL_CB_CLOSE`.
/// - HY000 (general error): returned via `OdbcError::general` for unexpected failures, and the
///   home this table gives a failed `StatementBackend::close_cursor` when the driver's error
///   mapping produced no more specific state. A backend that maps it to something else — a
///   `08S01` link failure, say — has that propagated as-is: this table lists no `08S01` row, but
///   substituting `HY000` for a state the driver already determined would be less true, not more
///   compliant. Whatever the state, the result set is discarded either way, so the application is
///   never left holding a cursor it cannot clear.
/// - HY001 (memory allocation error): not applicable; Rust allocation panics are caught by
///   `panic_safe`.
/// - HY010 (function sequence error): (driver-manager-handled; not returned here)
/// - HY013 (memory management error): not applicable; Rust memory access cannot fail silently.
/// - HY117 (connection suspended): (driver-manager-handled; not returned here)
/// - HYT01 (connection timeout): not applicable; the framework is in-process.
/// - IM001 (driver does not support function): (driver-manager-handled; not returned here)
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
pub unsafe fn sql_close_cursor<B: Backend>(statement_handle: *mut c_void) -> SqlReturn {
    tracing::debug!("SQLCloseCursor(stmt={:?})", statement_handle);
    // SAFETY: statement_handle is either null or a valid StatementHandle<B> pointer
    // previously allocated by sql_alloc_handle. scope.get validates kind and group
    // before any cast, and panic_safe catches any panics.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, |scope| {
            let stmt = scope.get::<StatementHandle<B>>(statement_handle)?;
            stmt.diagnostics.clear();

            // Spec 24000: No cursor open. A statement that is merely prepared
            // (S2/S3) has a `statement` but no cursor, so this reads
            // `cursor_open`.
            if !stmt.cursor_open {
                return Err(OdbcError::general(
                    "No cursor is open",
                    SqlState::invalid_cursor_state(),
                ));
            }

            // Tell the backend before tearing the statement down. Core used to
            // reach only `discard_result_set`, which drops the backend
            // statement — so a backend needing to release a server-side cursor,
            // cancel a pending fetch, or return a connection to a pool got a
            // `Drop` and no way to report a failure.
            // `StatementBackend::close_cursor` is fallible precisely because
            // that teardown is a round trip that can fail.
            let close_err = stmt
                .statement
                .as_mut()
                .and_then(|statement| statement.close_cursor().err());

            // Discarded even when the close failed, and deliberately so: the
            // application would otherwise be left holding a cursor it has no way
            // to clear, since every retry would call the same failing backend.
            // The failure is still reported — `SQLEndTran`'s "recorded and
            // carried, not swallowed" shape. After this the statement handle is
            // in a clean state and SQLExecDirect / SQLExecute can be called
            // again.
            stmt.discard_result_set();

            match close_err {
                Some(e) => Err(e),
                None => Ok(SqlReturn::SUCCESS),
            }
        })
    };
    tracing::debug!("SQLCloseCursor -> {:?}", ret);
    ret
}

/// Generic implementation of SQLCancel.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlcancel-function>
///
/// Cancels processing on a statement. The spec names three things this can
/// cancel: a function running asynchronously on the statement, a function on
/// the statement that needs data, and **a function running on the statement
/// on another thread**. This implementation tells the third case apart from
/// the first two by whether it can take the statement's connection lock
/// without waiting:
///
/// - **Another thread holds the lock.** Per spec, cancelling a function
///   running on another thread "does not clear the diagnostic records of the
///   being canceled function and does not post its own diagnostic records",
///   and "only SQL_SUCCESS or SQL_ERROR can be returned". So this path only
///   signals `Backend::cancel` with the statement's token and touches no
///   handle state — which is also the only sound thing to do, since another
///   thread is in the middle of mutating that state. Taking the lock
///   unconditionally here would make cancel wait for the very call it was
///   asked to interrupt, which is why `try_lock` is load-bearing rather than
///   an optimisation.
/// - **The lock is free.** Nobody else is inside this connection, so this is
///   the data-at-execution case, or ODBC 3.5's "no processing in progress, no
///   effect at all" case. The full path runs: diagnostics are cleared (this
///   function's own entry-clear, the same as every other FFI call makes — the
///   cross-thread branch above is the spec's deliberate exception, not this
///   one), any pending data-at-execution state is discarded, `Backend::cancel`
///   runs, and its own diagnostic is posted if that fails.
///
/// The statement's cancel token is cloned out of the registry before either
/// branch is chosen, and before the connection lock is even attempted. The
/// lifetime guarantee this needs against a concurrent `SQLDisconnect` or
/// `SQLFreeHandle` does not come from that ordering, though: it comes from
/// `Registry::cancel_of` itself, which takes the registry's read lock, checks
/// the token's generation, and hands back an owned `Arc` clone — mutually
/// exclusive with `unregister`'s write lock, so either the clone wins while
/// the slot still holds its own reference, or the free wins first and
/// `cancel_of` correctly returns `None`. Neither order can observe a token
/// that "might still be there" only to find it gone underneath it; see
/// `Registry::cancel_of`'s doc comment for the SQLite precedent this
/// protects. What the clone-first order actually narrows is a race in
/// *outcome*, not in soundness: resolving the token later gives a concurrent
/// free a wider window to win that race, so cancel more often observes
/// `None` and cancels nothing — spec-legal, and the reason to keep the
/// clone first is to shrink that window, not to avoid a dangling reference.
///
/// # Consequences of running lock-free
///
/// - `try_lock` cannot distinguish "a sibling statement on this connection is
///   busy" from "my own statement's own operation is busy": either makes this
///   function take the cross-thread branch, so a merely-idle statement's
///   data-at-execution state is occasionally left uncleared where it strictly
///   could have been cleared. Harmless, and explicitly spec-legal ("How the
///   function is canceled depends on the driver and the operating system").
/// - A `SQLGetDiagRecW`/`SQLGetDiagFieldW` call immediately following a
///   cross-thread cancel now blocks until the cancelled call has unwound
///   through the backend: both of those take the connection's lock, and
///   reading the diagnostic queue while another thread pushes to it is
///   undefined behaviour, so there is no sound alternative. `SQLCancel`
///   itself still returns promptly; the wait moves to whichever call reads
///   diagnostics next, bounded by the backend's own cancel latency.
///
/// # Parameters
///
/// - `statement_handle`: \[Input\] Statement handle (`SQLHSTMT`).
///
/// # Spec compliance
///
/// Diagnostics table from the ODBC spec:
///
/// - 01000 (general warning): driver-specific informational message
///   (`SQL_SUCCESS_WITH_INFO`) — not produced here; this function only ever
///   returns `SQL_SUCCESS`, `SQL_ERROR` or `SQL_INVALID_HANDLE`.
/// - HY000 (general error): may surface from the backend's own error-mapping
///   function for an otherwise-unclassified `Backend::cancel` failure.
/// - HY001 (memory allocation error): not applicable; a Rust allocation panic
///   here is caught by `panic_safe_unlocked`, this function's own panic guard
///   (`panic_safe` cannot be used here — see that function's doc comment for
///   why). Unlike `panic_safe`, that catch posts no diagnostic record at all,
///   so this SQLSTATE is never actually produced — the panic surfaces only as
///   a bare `SQL_ERROR`.
/// - HY010 (function sequence error): the spec's whole entry for this
///   SQLSTATE is `(DM)`-prefixed (an asynchronous function on the associated
///   connection handle still executing); driver-manager-handled, not returned
///   here.
/// - HY013 (memory management error): not applicable; Rust memory access
///   cannot fail silently.
/// - HY018 (server declined cancel request): propagated from `Backend::cancel`
///   — mapping a declined cancellation to this SQLSTATE is the backend's
///   error-mapping function's job, not core's.
/// - HY117 (connection suspended): `(DM)`; not returned here.
/// - HYT01 (connection timeout expired): may surface from `Backend::cancel` if
///   signalling the data source itself times out.
/// - IM001 (driver does not support this function): `(DM)`; not returned here.
///
/// # Safety
///
/// `statement_handle` must be null or a token issued by `sql_alloc_handle`.
pub unsafe fn sql_cancel<B: Backend>(statement_handle: *mut c_void) -> SqlReturn {
    tracing::debug!("SQLCancel(stmt={:?})", statement_handle);

    // Cloned out of the registry before anything else — in particular, before
    // the `try_lock` below — so this `Arc` keeps the backend's token alive
    // even if `SQLDisconnect` or `SQLFreeHandle` frees this statement or its
    // connection on another thread while this call is in flight. `None` here
    // covers a stale/foreign token as well as a live statement that has made
    // no backend call yet; both are indistinguishable from "nothing to
    // cancel" and handled identically by `signal_cancel` below.
    let token = registry()
        .cancel_of(statement_handle)
        .and_then(|t| t.downcast::<B::CancelToken>().ok());

    // A live statement always has a lock group; a null, stale or wrong-kind
    // handle does not. This check has to happen before either branch below,
    // because neither a successful nor a failed `try_lock` can tell "invalid
    // handle" apart from "idle" or "busy" on its own.
    let Some(group) = registry().group_of_kind(statement_handle, HandleKind::Stmt) else {
        tracing::debug!("SQLCancel -> {:?}", SqlReturn::INVALID_HANDLE);
        return SqlReturn::INVALID_HANDLE;
    };

    // Owned by this function's own frame, not moved into the closure below:
    // matching on `&guard` (rather than `guard`) throughout means the closure
    // only ever borrows it. That is what keeps this call off the list of
    // paths that can poison the group lock (`GroupLock`'s doc comment): a
    // panic inside the closure unwinds no further than `panic_safe_unlocked`'s
    // `catch_unwind`, which sits *above* this frame, so `guard` is never in a
    // frame the unwind passes through and its `Drop` never runs as part of an
    // unwind. It drops normally, without poisoning, when this function
    // returns — the same reasoning `panic_safe`'s own `_guard` relies on.
    let guard = group.try_lock();

    let ret = panic_safe_unlocked(
        || match &guard {
            // Another thread holds the connection: signal only, per spec.
            None => match signal_cancel::<B>(&token) {
                Ok(()) => SqlReturn::SUCCESS,
                Err(e) => {
                    tracing::warn!("SQLCancel: backend cancel failed while connection busy: {e}");
                    SqlReturn::ERROR
                }
            },
            // Nobody else is here: the data-at-execution / no-processing case.
            Some(guard) => {
                let mut scope = HandleScope::new(Some(group.clone()), Some(guard));
                match scope.get::<StatementHandle<B>>(statement_handle) {
                    Ok(stmt) => {
                        stmt.diagnostics.clear();
                        // Spec: after cancelling a statement that needed data,
                        // the application may call SQLExecute/SQLExecDirect
                        // again, so any pending data-at-execution state must be
                        // discarded along with it.
                        stmt.data_at_exec = None;
                        match signal_cancel::<B>(&token) {
                            Ok(()) => SqlReturn::SUCCESS,
                            Err(e) => {
                                stmt.diagnostics.push(&e);
                                SqlReturn::ERROR
                            }
                        }
                    }
                    Err(_) => SqlReturn::INVALID_HANDLE,
                }
            }
        },
        // A bare SQL_ERROR and no diagnostic record: there is no scope to push
        // one through, for the reason `panic_safe_unlocked`'s doc gives.
        || SqlReturn::ERROR,
    );

    tracing::debug!("SQLCancel -> {:?}", ret);
    ret
}

/// Call `Backend::cancel` if `token` names one, folding `NotImplemented` in
/// alongside "no token yet" as "nothing to cancel".
///
/// Shared by both of `sql_cancel`'s branches so that fold-in exists in exactly
/// one place rather than being duplicated per branch.
fn signal_cancel<B: Backend>(
    token: &Option<std::sync::Arc<B::CancelToken>>,
) -> Result<(), OdbcError> {
    let Some(token) = token else {
        return Ok(());
    };
    match B::cancel(token).into_odbc() {
        Ok(()) | Err(OdbcError::NotImplemented { .. }) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Generic implementation of SQLGetCursorNameW.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlgetcursorname-function>
///
/// Returns the cursor name associated with the statement. If no cursor name has been set
/// via `SQLSetCursorNameW`, an implementation-defined name of the form `SQL_CURSRnnnn` is
/// auto-generated and stored on the handle.
///
/// # Parameters
///
/// - `statement_handle`: \[Input\] Statement handle (`SQLHSTMT`).
/// - `cursor_name`: \[Output\] Pointer to a buffer in which to return the cursor name (UTF-16).
/// - `buffer_length`: \[Input\] Length of `cursor_name` buffer in UTF-16 code units.
/// - `name_length_ptr`: \[Output\] Pointer to a buffer in which to return the total number of
///   UTF-16 code units (excluding null terminator) available to return.
///
/// # Spec compliance
///
/// Diagnostics table from the ODBC spec:
///
/// - 01000 (general warning): driver-specific informational message — not produced here.
/// - 01004 (string data, right truncated): returned when the cursor name is longer than
///   `buffer_length`; handled by `write_utf16`.
/// - HY000 (general error): returned via `OdbcError::general` for unexpected failures.
/// - HY001 (memory allocation error): not applicable; Rust allocation panics are caught by
///   `panic_safe`.
/// - HY010 (function sequence error): (driver-manager-handled; not returned here)
/// - HY013 (memory management error): not applicable; Rust memory access cannot fail silently.
/// - HY015 (no cursor name available): not applicable; this implementation auto-generates a
///   name when none has been set.
/// - HY117 (connection suspended): (driver-manager-handled; not returned here)
/// - HYT01 (connection timeout): not applicable; the framework is in-process.
/// - IM001 (driver does not support function): (driver-manager-handled; not returned here)
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
/// `cursor_name` must be a valid writable buffer of at least `buffer_length` UTF-16 code units,
/// or null. `name_length_ptr` must be a valid writable pointer or null.
pub unsafe fn sql_get_cursor_name_w<B: Backend>(
    statement_handle: *mut c_void,
    cursor_name: *mut u16,
    buffer_length: i16,
    name_length_ptr: *mut i16,
) -> SqlReturn {
    tracing::debug!(
        "SQLGetCursorNameW(stmt={:?}, buf_len={})",
        statement_handle,
        buffer_length
    );
    // SAFETY: statement_handle is either null or a valid StatementHandle<B> pointer
    // previously allocated by sql_alloc_handle. scope.get validates kind and group
    // before any cast, and panic_safe catches any panics. cursor_name and
    // name_length_ptr are only written through by write_utf16, which performs its
    // own null checks.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, |scope| {
            let stmt = scope.get::<StatementHandle<B>>(statement_handle)?;
            stmt.diagnostics.clear();

            // Auto-generate a cursor name if none has been set.
            if stmt.cursor_name.is_none() {
                let n = CURSOR_NAME_COUNTER.fetch_add(1, Ordering::Relaxed);
                stmt.cursor_name = Some(format!("SQL_CURSR{n:04}"));
            }

            let name = stmt.cursor_name.clone().unwrap_or_default();

            // write_utf16 handles the null output pointer and length reporting;
            // note_truncation adds the 01004 record that goes with a truncated
            // write.
            Ok(crate::utf16::note_truncation(
                write_utf16(&name, cursor_name, buffer_length, name_length_ptr),
                &mut stmt.diagnostics,
            ))
        })
    };
    tracing::debug!("SQLGetCursorNameW -> {:?}", ret);
    ret
}

/// Generic implementation of SQLSetCursorNameW.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlsetcursorname-function>
///
/// Sets the cursor name associated with the statement handle.
///
/// # Parameters
///
/// - `statement_handle`: \[Input\] Statement handle (`SQLHSTMT`).
/// - `cursor_name`: \[Input\] Pointer to a UTF-16 encoded cursor name string.
/// - `name_length`: \[Input\] Length of `cursor_name` in UTF-16 code units, or `SQL_NTS`.
///
/// # Spec compliance
///
/// Diagnostics table from the ODBC spec:
///
/// - 01000 (general warning): not returned; driver-specific informational messages are
///   not generated.
/// - 01004 (string data, right truncated): not returned. The row describes a driver that
///   accepts an over-long name and silently truncates it; this one refuses instead, with
///   `34000`, which the same table provides for a name that "exceeded the maximum length
///   as defined by the driver". Truncating would leave the application believing it had
///   named a cursor it cannot then reference.
/// - 24000 (invalid cursor state): **returned by this driver**. The row carries no (DM)
///   marker, and the Comments state the rule in the positive direction — a cursor may be
///   renamed "as long as the cursor is in an allocated or prepared state". Appendix B's
///   row gives all four columns: `--` for `S1 Allocated` and `S2-S3 Prepared`, `24000`
///   for `S4 Executed` and `S5-S7 Cursor`. So a prepared-but-unexecuted statement is
///   accepted and an executed one is refused whether or not it opened a cursor, which is
///   why the check reads `StatementHandle::executed` rather than `statement.is_some()`.
/// - 34000 (invalid cursor name): **returned by this driver** for an empty name, for one
///   longer than `SQL_MAX_CURSOR_NAME_LEN`, and for one starting with `SQLCUR` or
///   `SQL_CUR` — prefixes reserved for the names [`sql_get_cursor_name_w`] generates.
/// - 3C000 (duplicate cursor name): **returned by this driver**. "All cursor names within
///   the connection must be unique", so the check walks this connection's statements —
///   all of which already share the group lock this call holds.
///
///   Names are compared **byte-exactly**, so `C1` and `c1` are two cursors. The spec
///   defines no notion of sameness for this row and states only the quoted half of the
///   rule — "in ODBC 3.x, if a cursor name is a quoted identifier, it is treated in a
///   case-sensitive manner" — which implies something about unquoted names without saying
///   what. Reading the mature drivers settles it in an unexpected direction: psqlODBC
///   (`PGAPI_SetCursorName`), MySQL Connector/ODBC (`MySQLSetCursorName`), FreeTDS
///   (`SQLSetCursorName`) and unixODBC's Driver Manager
///   (`DriverManager/SQLSetCursorName.c`) implement **no duplicate check at all**, and a
///   search for `3C000` finds nothing in the first three. So there is no established
///   folding rule to adopt, and case-folding here would be inference from the spec's
///   silence rather than evidence. Pinned by
///   `cursor_names_differing_only_in_case_are_distinct`.
/// - HY000 (general error): returned via `OdbcError::general` for unexpected failures.
/// - HY001 (memory allocation error): not applicable; Rust allocation panics are caught by
///   `panic_safe`.
/// - HY009 (invalid use of null pointer): returned when `cursor_name` is a null pointer.
///   The row is (DM)-marked, but a null reaching the driver is still refused rather than
///   dereferenced.
/// - HY010 (function sequence error): (driver-manager-handled; not returned here)
/// - HY013 (memory management error): not applicable; Rust memory access cannot fail silently.
/// - HY090 (invalid string or buffer length): not returned. The row is (DM)-marked and
///   describes `NameLength` "less than 0 but not equal to SQL_NTS", which the Driver
///   Manager rejects before the call arrives. An earlier revision returned it for an
///   *empty* name, which is a different condition and is now `34000`.
/// - HY117 (connection suspended): (driver-manager-handled; not returned here)
/// - HYT01 (connection timeout): not applicable; the framework is in-process.
/// - IM001 (driver does not support function): (driver-manager-handled; not returned here)
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
/// `cursor_name` must be a valid readable UTF-16 string of at least `name_length` code units,
/// or null-terminated if `name_length` is `SQL_NTS`.
pub unsafe fn sql_set_cursor_name_w<B: Backend>(
    statement_handle: *mut c_void,
    cursor_name: *const u16,
    name_length: i16,
) -> SqlReturn {
    tracing::trace!(
        "SQLSetCursorNameW(stmt={:?}, name_ptr={:?}, name_len={})",
        statement_handle,
        cursor_name,
        name_length
    );
    // SAFETY: statement_handle is either null or a valid StatementHandle<B> pointer
    // previously allocated by sql_alloc_handle. scope.get validates kind and group
    // before any cast, and panic_safe catches any panics. cursor_name is read
    // by utf16_to_string which handles null pointers by returning None.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, |scope| {
            scope
                .get::<StatementHandle<B>>(statement_handle)?
                .diagnostics
                .clear();

            // Spec HY009: null pointer.
            let name = utf16_to_string(cursor_name, i32::from(name_length)).map_err(|_| {
                OdbcError::general(
                    "Cursor name pointer is null",
                    SqlState::invalid_use_of_null_pointer(),
                )
            })?;
            tracing::debug!(
                "SQLSetCursorNameW(stmt={:?}, name={:?})",
                statement_handle,
                name
            );

            // Spec 34000: "the cursor name specified in *CursorName was invalid
            // because it exceeded the maximum length as defined by the driver, or
            // it started with 'SQLCUR' or 'SQL_CUR'". Those prefixes are reserved
            // for the names `sql_get_cursor_name_w` generates, so an application
            // taking one could collide with a cursor it does not own.
            //
            // An empty name is rejected here rather than with `HY090`, which the
            // spec's table marks (DM) and defines as "NameLength was less than 0
            // but not equal to SQL_NTS" — a different condition, and the Driver
            // Manager's.
            if name.is_empty() {
                return Err(OdbcError::general(
                    "Cursor name must not be empty",
                    SqlState::invalid_cursor_name(),
                ));
            }
            // Counted in characters against the very value `SQLGetInfo` reports
            // for `SQL_MAX_CURSOR_NAME_LEN`, so the driver cannot advertise one
            // limit and enforce another.
            let chars = name.chars().count();
            if chars > usize::from(crate::types::SQL_MAX_CURSOR_NAME_LEN) {
                return Err(OdbcError::general(
                    format!(
                        "Cursor name is {chars} characters; this driver reports a \
                         maximum of {} for SQL_MAX_CURSOR_NAME_LEN",
                        crate::types::SQL_MAX_CURSOR_NAME_LEN
                    ),
                    SqlState::invalid_cursor_name(),
                ));
            }
            let upper = name.to_uppercase();
            if upper.starts_with("SQLCUR") || upper.starts_with("SQL_CUR") {
                return Err(OdbcError::general(
                    format!(
                        "Cursor name {name:?} uses a prefix reserved for \
                         driver-generated names (SQLCUR, SQL_CUR)"
                    ),
                    SqlState::invalid_cursor_name(),
                ));
            }

            // Spec 24000: "the statement corresponding to StatementHandle was
            // already in an executed or cursor-positioned state". The Comments
            // say the same in the positive direction — a cursor may be renamed
            // "as long as the cursor is in an allocated or prepared state" — and
            // Appendix B's row spells out all four columns: `--` for
            // `S1 Allocated` and `S2-S3 Prepared`, `24000` for `S4 Executed` and
            // `S5-S7 Cursor`.
            //
            // So this reads `executed`, not `statement.is_some()`: `SQLPrepare`
            // stores a backend statement without executing anything, and
            // `SQLPrepare` -> `SQLSetCursorName` -> `SQLExecute` is the ordinary
            // positioned-update setup. `cursor_open` alone would not do either —
            // an `UPDATE` that executed leaves S4, which this row still refuses.
            //
            // `SQLEndTran` under `SQL_CB_CLOSE` deliberately leaves `executed`
            // set: it closes the cursor and keeps the statement, and no spec text
            // says a statement returns to the renameable prepared state that way.
            let stmt = scope.get::<StatementHandle<B>>(statement_handle)?;
            if stmt.cursor_open || stmt.executed {
                return Err(OdbcError::general(
                    "Cannot set a cursor name once the statement has been executed",
                    SqlState::invalid_cursor_state(),
                ));
            }

            // Spec 3C000: "All cursor names within the connection must be unique."
            // Every statement on this connection shares the group lock this call
            // already holds, so the walk adds no lock and no ordering rule — the
            // same footing as `SQLEndTran`'s. An owned snapshot, for the reason
            // given there: a statement freed mid-walk cannot shift it.
            let registry = crate::handles::registry::registry();
            if let Some(conn_token) = registry.parent_of(statement_handle, HandleKind::Stmt) {
                for sibling in registry.children_of(conn_token) {
                    if std::ptr::eq(sibling, statement_handle) {
                        continue;
                    }
                    // A sibling retired between the snapshot and here is simply
                    // gone, and holds no name to clash with.
                    let Ok(other) = scope.get::<StatementHandle<B>>(sibling) else {
                        continue;
                    };
                    // Byte-exact, deliberately: the spec defines no notion of
                    // sameness here and states only that a *quoted* identifier
                    // is case-sensitive. None of psqlODBC, MySQL
                    // Connector/ODBC, FreeTDS or unixODBC's Driver Manager
                    // implements this check at all, so no established practice
                    // supplies a folding rule and case-folding would be
                    // inference from the spec's silence. See the 3C000 row of
                    // this function's doc comment for the citations.
                    if other.cursor_name.as_deref() == Some(name.as_str()) {
                        return Err(OdbcError::general(
                            format!("Cursor name {name:?} is already in use on this connection"),
                            SqlState::duplicate_cursor_name(),
                        ));
                    }
                }
            }

            scope
                .get::<StatementHandle<B>>(statement_handle)?
                .cursor_name = Some(name);
            Ok(SqlReturn::SUCCESS)
        })
    };
    tracing::debug!("SQLSetCursorNameW -> {:?}", ret);
    ret
}

/// Generic implementation of SQLBulkOperations.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlbulkoperations-function>
///
/// This driver uses forward-only cursors with no bookmark support. All valid `Operation`
/// values are recognised but the operation is immediately rejected with `HYC00` (optional
/// feature not implemented).
///
/// # Parameters
///
/// - `statement_handle`: \[Input\] Statement handle (`SQLHSTMT`).
/// - `operation`: \[Input\] Type of bulk operation to perform. One of `SQL_ADD` (4),
///   `SQL_UPDATE_BY_BOOKMARK` (5), `SQL_DELETE_BY_BOOKMARK` (6), or `SQL_FETCH_BY_BOOKMARK` (7).
///
/// # Spec compliance
///
/// Diagnostics table from the ODBC spec:
///
/// - 01000 (general warning): driver-specific informational message — not produced here.
/// - 01004 (string data, right truncated): not applicable; no data movement occurs.
/// - 01S01 (error in row): not applicable; HYC00 is returned before any row processing.
/// - 07006 (restricted data type attribute violation): not applicable; HYC00 precedes data conversion.
/// - 07009 (invalid descriptor index): not applicable; HYC00 precedes column access.
/// - 21S02 (degree of derived table does not match column list): not applicable.
/// - 22001 (string data right truncation): not applicable.
/// - 22003 (numeric value out of range): not applicable.
/// - 22007 (invalid datetime format): not applicable.
/// - 22008 (datetime field overflow): not applicable.
/// - 22015 (interval field overflow): not applicable.
/// - 22018 (invalid character value for cast specification): not applicable.
/// - 23000 (integrity constraint violation): not applicable.
/// - 24000 (invalid cursor state): not applicable; HYC00 is returned first.
/// - 40001 (serialization failure): not applicable.
/// - 40003 (statement completion unknown): not applicable.
/// - 42000 (syntax error or access violation): not applicable.
/// - 44000 (WITH CHECK OPTION violation): not applicable.
/// - HY000 (general error): returned via `OdbcError::general` for unexpected failures.
/// - HY001 (memory allocation error): not applicable; Rust allocation panics are caught by
///   `panic_safe`.
/// - HY008: Operation canceled; not returned here. This call makes no fallible backend call —
///   `SQLBulkOperations` reports `HYC00` without asking the backend — so there is no error for a
///   cancellation to be reported through. The asynchronous clause is inapplicable: core never
///   returns `SQL_STILL_EXECUTING`.
/// - HY010 (function sequence error): (driver-manager-handled; not returned here)
/// - HY011 (attribute cannot be set now): not applicable.
/// - HY013 (memory management error): not applicable; Rust memory access cannot fail silently.
/// - HY090 (invalid string or buffer length): not applicable; HYC00 precedes buffer access.
/// - HY092 (invalid attribute/option identifier): returned when `operation` is not one of the
///   four valid values defined by the spec (`SQL_ADD`, `SQL_UPDATE_BY_BOOKMARK`,
///   `SQL_DELETE_BY_BOOKMARK`, `SQL_FETCH_BY_BOOKMARK`).
/// - HY117 (connection suspended): (driver-manager-handled; not returned here)
/// - HYC00 (optional feature not implemented): returned for all valid `operation` values —
///   this driver does not support bulk operations or bookmarks.
/// - HYT00 (timeout expired): not applicable; the framework is in-process.
/// - HYT01 (connection timeout): not applicable; the framework is in-process.
/// - IM001 (driver does not support function): (driver-manager-handled; not returned here)
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
pub unsafe fn sql_bulk_operations<B: Backend>(
    statement_handle: *mut c_void,
    operation: i16,
) -> SqlReturn {
    tracing::trace!(
        "SQLBulkOperations(stmt={:?}, raw_operation={})",
        statement_handle,
        operation
    );
    // SAFETY: statement_handle is either null or a valid StatementHandle<B> pointer
    // previously allocated by sql_alloc_handle. scope.get validates kind and group
    // before any cast, and panic_safe catches any panics.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, |scope| {
            let stmt = scope.get::<StatementHandle<B>>(statement_handle)?;
            stmt.diagnostics.clear();

            // Spec HY092: validate operation. Converting at the boundary is
            // what makes the value typed for the rest of the function; an
            // unrecognised code never reaches any logic below.
            let operation = bulk_operation_from_raw(operation).ok_or_else(|| {
                OdbcError::general(
                    format!("Unknown bulk operation: {operation}"),
                    SqlState::invalid_attribute_option_identifier(),
                )
            })?;

            tracing::debug!("SQLBulkOperations(operation={:?})", operation);

            // HYC00: this driver does not support bulk operations.
            Err(OdbcError::general(
                "SQLBulkOperations is not supported; this driver uses forward-only cursors without bookmark support",
                SqlState::optional_feature_not_implemented(),
            ))
        })
    };
    tracing::debug!("SQLBulkOperations -> {:?}", ret);
    ret
}

/// Generic implementation of SQLSetPos.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlsetpos-function>
///
/// This driver uses forward-only cursors. All valid `Operation`/`LockType` combinations are
/// recognised but immediately rejected with `HYC00` (optional feature not implemented).
///
/// # Parameters
///
/// - `statement_handle`: \[Input\] Statement handle (`SQLHSTMT`).
/// - `row_number`: \[Input\] Position of the row in the rowset on which to perform the operation.
///   If `row_number` is 0, the operation applies to every row in the rowset.
/// - `operation`: \[Input\] Operation to perform. One of `SQL_POSITION` (0), `SQL_REFRESH` (1),
///   `SQL_UPDATE` (2), or `SQL_DELETE` (3).
/// - `lock_type`: \[Input\] Lock to apply after the operation. One of `SQL_LOCK_NO_CHANGE` (0),
///   `SQL_LOCK_EXCLUSIVE` (1), or `SQL_LOCK_UNLOCK` (2).
///
/// # Spec compliance
///
/// Diagnostics table from the ODBC spec:
///
/// - 01000 (general warning): driver-specific informational message — not produced here.
/// - 01001 (cursor operation conflict): not applicable; HYC00 is returned before any operation.
/// - 01004 (string data, right truncated): not applicable.
/// - 01S01 (error in row): not applicable.
/// - 01S07 (fractional truncation): not applicable.
/// - 07006 (restricted data type attribute violation): not applicable.
/// - 07009 (invalid descriptor index): not applicable.
/// - 21S02 (degree of derived table does not match column list): not applicable.
/// - 22001 (string data right truncation): not applicable.
/// - 22003 (numeric value out of range): not applicable.
/// - 22007 (invalid datetime format): not applicable.
/// - 22008 (datetime field overflow): not applicable.
/// - 22015 (interval field overflow): not applicable.
/// - 22018 (invalid character value for cast specification): not applicable.
/// - 23000 (integrity constraint violation): not applicable.
/// - 24000 (invalid cursor state): not applicable; HYC00 is returned first.
/// - 40001 (serialization failure): not applicable.
/// - 40003 (statement completion unknown): not applicable.
/// - 42000 (syntax error or access violation): not applicable.
/// - 44000 (WITH CHECK OPTION violation): not applicable.
/// - HY000 (general error): returned via `OdbcError::general` for unexpected failures.
/// - HY001 (memory allocation error): not applicable; Rust allocation panics are caught by
///   `panic_safe`.
/// - HY008: Operation canceled; not returned here. This call makes no fallible backend call —
///   `SQLSetPos` reports `HYC00` without asking the backend — so there is no error for a
///   cancellation to be reported through. The asynchronous clause is inapplicable: core never
///   returns `SQL_STILL_EXECUTING`.
/// - HY010 (function sequence error): (driver-manager-handled; not returned here)
/// - HY011 (attribute cannot be set now): not applicable.
/// - HY013 (memory management error): not applicable; Rust memory access cannot fail silently.
/// - HY090 (invalid string or buffer length): not applicable.
/// - HY092 (invalid attribute/option identifier): returned when `operation` is not one of
///   `SQL_POSITION`, `SQL_REFRESH`, `SQL_UPDATE`, `SQL_DELETE`, or when `lock_type` is not
///   one of `SQL_LOCK_NO_CHANGE`, `SQL_LOCK_EXCLUSIVE`, `SQL_LOCK_UNLOCK`.
/// - HY107 (row value out of range): not applicable; HYC00 is returned before row validation.
/// - HY109 (invalid cursor position): not applicable; HYC00 is returned before cursor checks.
/// - HY117 (connection suspended): (driver-manager-handled; not returned here)
/// - HYC00 (optional feature not implemented): returned for all valid `operation`/`lock_type`
///   combinations — this driver does not support scrollable cursors or positioned operations.
/// - HYT00 (timeout expired): not applicable; the framework is in-process.
/// - HYT01 (connection timeout): not applicable; the framework is in-process.
/// - IM001 (driver does not support function): (driver-manager-handled; not returned here)
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
pub unsafe fn sql_set_pos<B: Backend>(
    statement_handle: *mut c_void,
    row_number: u64,
    operation: u16,
    lock_type: u16,
) -> SqlReturn {
    tracing::trace!(
        "SQLSetPos(stmt={:?}, row={}, raw_operation={}, raw_lock_type={})",
        statement_handle,
        row_number,
        operation,
        lock_type
    );
    // SAFETY: statement_handle is either null or a valid StatementHandle<B> pointer
    // previously allocated by sql_alloc_handle. scope.get validates kind and group
    // before any cast, and panic_safe catches any panics.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, |scope| {
            let stmt = scope.get::<StatementHandle<B>>(statement_handle)?;
            stmt.diagnostics.clear();

            tracing::debug!(
                "SQLSetPos(row={}, operation={}, lock_type={})",
                row_number,
                operation,
                lock_type
            );

            // Spec HY092: validate operation. Matched against the constants
            // rather than converted to `odbc_sys::Operation` — see the comment
            // over `SQL_POSITION` in `types/constants.rs` for why that type
            // cannot carry this value.
            match operation {
                SQL_POSITION | SQL_REFRESH | SQL_UPDATE | SQL_DELETE => {}
                _ => {
                    return Err(OdbcError::general(
                        format!("Unknown SQLSetPos operation: {operation}"),
                        SqlState::invalid_attribute_option_identifier(),
                    ));
                }
            }

            // Spec HY092: validate lock_type.
            match lock_type {
                SQL_LOCK_NO_CHANGE | SQL_LOCK_EXCLUSIVE | SQL_LOCK_UNLOCK => {}
                _ => {
                    return Err(OdbcError::general(
                        format!("Unknown SQLSetPos lock type: {lock_type}"),
                        SqlState::invalid_attribute_option_identifier(),
                    ));
                }
            }

            // HYC00: this driver does not support positioned cursor operations.
            Err(OdbcError::general(
                "SQLSetPos is not supported; this driver uses forward-only cursors",
                SqlState::optional_feature_not_implemented(),
            ))
        })
    };
    tracing::debug!("SQLSetPos -> {:?}", ret);
    ret
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffi::handle::{sql_alloc_handle, sql_free_handle};
    use crate::test_utils::{
        MockBackend, MockFailingCloseBackend, alloc_env_conn_stmt, cleanup_env_conn_stmt,
        with_handle,
    };
    use odbc_sys::{BulkOperation, HandleType};

    #[test]
    fn num_result_cols_without_execute_returns_error() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let mut count: i16 = 0;
            let ret = sql_num_result_cols::<MockBackend>(stmt, &mut count);
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// SQLNumResultCols with a null output pointer and no result set should
    /// return ERROR (HY010 fires before the null-pointer check).
    #[test]
    fn num_result_cols_null_output_ptr_returns_error_when_no_result_set() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            // No statement executed → HY010, regardless of output pointer.
            let ret = sql_num_result_cols::<MockBackend>(stmt, std::ptr::null_mut());
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    #[test]
    fn num_result_cols_with_synthetic_statement() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                handle.statement = Some(crate::handles::StatementData::Synthetic(
                    crate::synthetic::SyntheticStatement::new(
                        vec![
                            crate::types::ColumnDescriptor {
                                name: "col1".into(),
                                type_name: String::new(),
                                sql_type: crate::types::SqlDataType(4), // INTEGER
                                precision: 10,
                                scale: 0,
                                nullable: Nullable::SqlNullable,
                                ..Default::default()
                            },
                            crate::types::ColumnDescriptor {
                                name: "col2".into(),
                                type_name: String::new(),
                                sql_type: crate::types::SqlDataType(12), // VARCHAR
                                precision: 255,
                                scale: 0,
                                nullable: Nullable::SqlNullable,
                                ..Default::default()
                            },
                        ],
                        vec![],
                    ),
                ));
            });

            let mut count: i16 = 0;
            let ret = sql_num_result_cols::<MockBackend>(stmt, &mut count);
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(count, 2);
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    #[test]
    fn row_count_without_statement_returns_zero() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let mut count: isize = -999;
            let ret = sql_row_count::<MockBackend>(stmt, &mut count);
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(count, 0);
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    #[test]
    fn more_results_always_returns_no_data() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = sql_more_results::<MockBackend>(stmt);
            assert_eq!(ret, SqlReturn::NO_DATA);
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// Appendix B's `SQLMoreResults` row, restricted to the single-result-set
    /// case core produces: from `S5-S7 Cursor` the `[nf]` entries are
    /// `S1 [np]` and `S3 [p]` — the same pair `SQLFreeStmt(SQL_CLOSE)` names.
    /// Returning `SQL_NO_DATA` and leaving the cursor open left a following
    /// `SQLFetch` re-reading the result set that had just been reported away.
    #[test]
    fn more_results_discards_the_result_set_it_reports_away() {
        unsafe {
            let (env, conn, stmt) = cursor_open_stmt_for::<MockFailingCloseBackend>();
            with_handle::<MockFailingCloseBackend, StatementHandle<MockFailingCloseBackend>, _>(
                stmt,
                |handle| assert!(handle.cursor_open, "precondition: a cursor is open"),
            );

            // MockFailingCloseStatement::close_cursor fails, so this reports the
            // backend's 08S01 rather than SQL_NO_DATA -- and the discard must
            // happen anyway, exactly as SQLCloseCursor and SQLFreeStmt(SQL_CLOSE)
            // already do it.
            assert_eq!(
                sql_more_results::<MockFailingCloseBackend>(stmt),
                SqlReturn::ERROR,
                "a failing close_cursor is reported, not swallowed",
            );

            with_handle::<MockFailingCloseBackend, StatementHandle<MockFailingCloseBackend>, _>(
                stmt,
                |handle| {
                    assert!(!handle.cursor_open, "the cursor must be closed");
                    assert!(
                        handle.statement.is_none(),
                        "the result set must be discarded",
                    );
                },
            );

            crate::test_utils::cleanup_connected_env_conn_stmt::<MockFailingCloseBackend>(
                env, conn, stmt,
            );
        }
    }

    /// The `S1 Allocated` and `S2-S3 Prepared` columns of the same row are `--`
    /// with footnote [1], "The function always returns SQL_NO_DATA in this
    /// state". A prepared statement therefore keeps its backend statement: a
    /// teardown written against `statement.is_some()` would throw away work
    /// `SQLPrepare` had just done.
    #[test]
    fn more_results_leaves_a_prepared_statement_untouched() {
        unsafe {
            let (env, conn, stmt) =
                crate::test_utils::alloc_connected_env_conn_stmt::<MockBackend>();

            let sql: Vec<u16> = "SELECT 1".encode_utf16().collect();
            assert_eq!(
                crate::ffi::execute::sql_prepare_w::<MockBackend>(
                    stmt,
                    sql.as_ptr(),
                    i32::try_from(sql.len()).expect("the fixed test SQL is short"),
                ),
                SqlReturn::SUCCESS
            );

            assert_eq!(sql_more_results::<MockBackend>(stmt), SqlReturn::NO_DATA);

            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                assert!(
                    handle.statement.is_some(),
                    "a prepared statement must survive SQLMoreResults",
                );
                assert_eq!(handle.prepared_sql.as_deref(), Some("SELECT 1"));
            });

            crate::test_utils::cleanup_connected_env_conn_stmt::<MockBackend>(env, conn, stmt);
        }
    }

    #[test]
    fn close_cursor_without_statement_returns_error() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = sql_close_cursor::<MockBackend>(stmt);
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    #[test]
    fn close_cursor_with_open_cursor_succeeds() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                handle.set_result_set(crate::handles::StatementData::Synthetic(
                    crate::test_utils::synthetic_result_set(vec![]),
                ));
            });

            let ret = sql_close_cursor::<MockBackend>(stmt);
            assert_eq!(ret, SqlReturn::SUCCESS);
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    // -----------------------------------------------------------------------
    // row_count with synthetic statement (returns Some)
    // -----------------------------------------------------------------------

    #[test]
    fn row_count_with_synthetic_statement_returns_count() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                handle.statement = Some(crate::handles::StatementData::Synthetic(
                    crate::synthetic::SyntheticStatement::new(
                        vec![crate::types::ColumnDescriptor {
                            name: "col".into(),
                            type_name: String::new(),
                            sql_type: crate::types::SqlDataType(4),
                            precision: 10,
                            scale: 0,
                            nullable: Nullable::SqlNullable,
                            ..Default::default()
                        }],
                        vec![
                            vec![crate::types::ColumnValue::I32(1)],
                            vec![crate::types::ColumnValue::I32(2)],
                            vec![crate::types::ColumnValue::I32(3)],
                        ],
                    ),
                ));
            });

            let mut count: isize = -999;
            let ret = sql_row_count::<MockBackend>(stmt, &mut count);
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(count, 3);
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    // -----------------------------------------------------------------------
    // row_count with mock backend statement (row_count returns None → -1)
    // -----------------------------------------------------------------------

    #[test]
    fn row_count_with_mock_statement_returns_unknown() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                // MockStatement's default row_count() returns None
                handle.statement = Some(crate::handles::StatementData::Backend(
                    crate::test_utils::MockStatement,
                ));
            });

            let mut count: isize = 0;
            let ret = sql_row_count::<MockBackend>(stmt, &mut count);
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(count, -1); // unknown
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    // -----------------------------------------------------------------------
    // Null count pointers
    // -----------------------------------------------------------------------

    #[test]
    fn num_result_cols_with_null_count_pointer() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                handle.statement = Some(crate::handles::StatementData::Synthetic(
                    crate::synthetic::SyntheticStatement::new(vec![], vec![]),
                ));
            });

            let ret = sql_num_result_cols::<MockBackend>(stmt, std::ptr::null_mut());
            assert_eq!(ret, SqlReturn::SUCCESS);
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    #[test]
    fn row_count_with_null_count_pointer() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = sql_row_count::<MockBackend>(stmt, std::ptr::null_mut());
            assert_eq!(ret, SqlReturn::SUCCESS);
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    // -----------------------------------------------------------------------
    // close_cursor resets fetch position
    // -----------------------------------------------------------------------

    #[test]
    fn close_cursor_resets_fetch_position() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                handle.set_result_set(crate::handles::StatementData::Synthetic(
                    crate::test_utils::synthetic_result_set(vec![vec![
                        crate::types::ColumnValue::I32(1),
                    ]]),
                ));

                // Fetch the one row
                let statement = handle.statement.as_mut().expect("statement");
                assert_eq!(
                    statement.fetch().expect("fetch"),
                    crate::types::FetchResult::Row
                );
                assert_eq!(
                    statement.fetch().expect("fetch"),
                    crate::types::FetchResult::NoData
                );
            });

            // Close cursor via FFI; discards the result set entirely.
            let ret = sql_close_cursor::<MockBackend>(stmt);
            assert_eq!(ret, SqlReturn::SUCCESS);

            // After close_cursor the statement is gone; the handle is clean.
            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                assert!(handle.statement.is_none());
            });

            // Closing an already-closed cursor returns 24000.
            let ret2 = sql_close_cursor::<MockBackend>(stmt);
            assert_eq!(ret2, SqlReturn::ERROR);

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    // -----------------------------------------------------------------------
    // Invalid handles
    // -----------------------------------------------------------------------

    #[test]
    fn num_result_cols_null_handle_returns_invalid() {
        unsafe {
            let mut count: i16 = 0;
            let ret = sql_num_result_cols::<MockBackend>(std::ptr::null_mut(), &mut count);
            assert_eq!(ret, SqlReturn::INVALID_HANDLE);
        }
    }

    #[test]
    fn row_count_null_handle_returns_invalid() {
        unsafe {
            let mut count: isize = 0;
            let ret = sql_row_count::<MockBackend>(std::ptr::null_mut(), &mut count);
            assert_eq!(ret, SqlReturn::INVALID_HANDLE);
        }
    }

    #[test]
    fn more_results_null_handle_returns_invalid() {
        unsafe {
            let ret = sql_more_results::<MockBackend>(std::ptr::null_mut());
            assert_eq!(ret, SqlReturn::INVALID_HANDLE);
        }
    }

    #[test]
    fn close_cursor_null_handle_returns_invalid() {
        unsafe {
            let ret = sql_close_cursor::<MockBackend>(std::ptr::null_mut());
            assert_eq!(ret, SqlReturn::INVALID_HANDLE);
        }
    }

    // -----------------------------------------------------------------------
    // SQLCancel
    // -----------------------------------------------------------------------

    #[test]
    fn cancel_valid_handle_returns_success() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = sql_cancel::<MockBackend>(stmt);
            assert_eq!(ret, SqlReturn::SUCCESS);
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    #[test]
    fn cancel_with_open_cursor_returns_success() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                handle.set_result_set(crate::handles::StatementData::Synthetic(
                    crate::test_utils::synthetic_result_set(vec![]),
                ));
            });

            // This statement has no cancel token (nothing has stored one), so
            // `signal_cancel` finds nothing to call and `sql_cancel` returns
            // SUCCESS without touching the cursor; it is not a general claim
            // that cancelling an open cursor is always a no-op.
            // `cancel_calls_backend_cancel_...` below is what exercises
            // `Backend::cancel` actually running.
            let ret = sql_cancel::<MockBackend>(stmt);
            assert_eq!(ret, SqlReturn::SUCCESS);
            // Cursor must still be open; cancel does not close it.
            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                assert!(handle.statement.is_some());
                assert!(handle.cursor_open);
            });

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// As [`alloc_env_conn_stmt`], but generic over `B`. Needed for a backend
    /// other than `MockBackend`, specifically one whose `Error` is
    /// `OdbcError` directly, so `Backend::cancel` can return something other
    /// than the `NotImplemented` every `MockError` collapses to.
    unsafe fn alloc_env_conn_stmt_for<B: Backend>() -> (*mut c_void, *mut c_void, *mut c_void) {
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = crate::ffi::handle::sql_alloc_handle::<B>(
                odbc_sys::HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );
            let mut conn: *mut c_void = std::ptr::null_mut();
            let _ = crate::ffi::handle::sql_alloc_handle::<B>(
                odbc_sys::HandleType::Dbc as i16,
                env,
                &mut conn,
            );
            let mut stmt: *mut c_void = std::ptr::null_mut();
            let _ = crate::ffi::handle::sql_alloc_handle::<B>(
                odbc_sys::HandleType::Stmt as i16,
                conn,
                &mut stmt,
            );
            (env, conn, stmt)
        }
    }

    /// As [`cleanup_env_conn_stmt`], but generic over `B`, for
    /// [`alloc_env_conn_stmt_for`].
    unsafe fn cleanup_env_conn_stmt_for<B: Backend>(
        env: *mut c_void,
        conn: *mut c_void,
        stmt: *mut c_void,
    ) {
        unsafe {
            let _ =
                crate::ffi::handle::sql_free_handle::<B>(odbc_sys::HandleType::Stmt as i16, stmt);
            let _ =
                crate::ffi::handle::sql_free_handle::<B>(odbc_sys::HandleType::Dbc as i16, conn);
            let _ = crate::ffi::handle::sql_free_handle::<B>(odbc_sys::HandleType::Env as i16, env);
        }
    }

    /// `sql_cancel` resolves the statement's stored token through the
    /// registry and hands it to `Backend::cancel`. This test seeds the
    /// registry directly with `set_cancel` rather than going through a
    /// statement-producing call, so it can exercise that read in isolation
    /// from the call that would normally have created the token first.
    #[test]
    fn cancel_calls_backend_cancel_when_a_token_exists() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();

            let token = std::sync::Arc::new(crate::test_utils::MockCancelToken::default());
            crate::handles::registry::registry().set_cancel(
                stmt,
                std::sync::Arc::clone(&token) as std::sync::Arc<dyn std::any::Any + Send + Sync>,
            );

            let ret = sql_cancel::<MockBackend>(stmt);
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert!(
                token.cancelled.load(Ordering::SeqCst),
                "Backend::cancel must have run against the stored token"
            );

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// The error-propagation arm of `signal_cancel`: `Backend::cancel`
    /// returning `Err` must reach the caller as `SQL_ERROR` rather than being
    /// swallowed like `NotImplemented` is. `MockBackend` cannot produce this,
    /// since its `Error` is `MockError`, which converts to `OdbcError` as
    /// `NotImplemented` and would be silently treated as "nothing to
    /// cancel". `MockFailingCloseBackend` is the one mock whose `Error` is
    /// `OdbcError` directly, so its `cancel` can return a real error via
    /// `MockCancelToken::should_fail`.
    #[test]
    fn cancel_propagates_a_backend_cancel_error() {
        unsafe {
            let (env, conn, stmt) =
                alloc_env_conn_stmt_for::<crate::test_utils::MockFailingCloseBackend>();

            let token = crate::test_utils::MockCancelToken {
                should_fail: std::sync::atomic::AtomicBool::new(true),
                ..Default::default()
            };
            crate::handles::registry::registry().set_cancel(
                stmt,
                std::sync::Arc::new(token) as std::sync::Arc<dyn std::any::Any + Send + Sync>,
            );

            let ret = sql_cancel::<crate::test_utils::MockFailingCloseBackend>(stmt);
            assert_eq!(ret, SqlReturn::ERROR);

            cleanup_env_conn_stmt_for::<crate::test_utils::MockFailingCloseBackend>(
                env, conn, stmt,
            );
        }
    }

    #[test]
    fn cancel_null_handle_returns_invalid() {
        unsafe {
            let ret = sql_cancel::<MockBackend>(std::ptr::null_mut());
            assert_eq!(ret, SqlReturn::INVALID_HANDLE);
        }
    }

    /// SQLCancel on a statement with no active operation returns SUCCESS.
    #[test]
    fn cancel_no_active_statement_returns_success() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = sql_cancel::<MockBackend>(stmt);
            assert_eq!(ret, SqlReturn::SUCCESS);
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// The spec's own bifurcation, stated from the idle side: with nobody
    /// else inside the connection, `SQLCancel` takes the full path and clears
    /// diagnostics unconditionally, even with nothing to cancel — the same
    /// entry-clear every other FFI function performs. Only the cross-thread
    /// branch (`cancel_signals_the_backend_while_another_thread_holds_the_group`
    /// below) is the spec's deliberate exception to that.
    #[test]
    fn cancel_clears_diagnostics() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();

            // Push a diagnostic onto the statement handle.
            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                handle
                    .diagnostics
                    .push(&crate::errors::OdbcError::NotConnected);
                assert_eq!(handle.diagnostics.len(), 1);
            });

            // Cancel should clear the diagnostics.
            let ret = sql_cancel::<MockBackend>(stmt);
            assert_eq!(ret, SqlReturn::SUCCESS);

            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                assert_eq!(handle.diagnostics.len(), 0);
            });

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// The spec's own bifurcation, exercised from the other side: when another
    /// thread holds the connection, `SQLCancel` must still reach the backend
    /// without waiting for that thread to finish. A `sql_cancel` that took the
    /// group lock unconditionally would hang here instead of completing
    /// (verified with `timeout 30 cargo test --lib
    /// cancel_signals_the_backend_while_another_thread_holds_the_group`).
    #[test]
    fn cancel_signals_the_backend_while_another_thread_holds_the_group() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();

            let token = std::sync::Arc::new(crate::test_utils::MockCancelToken::default());
            registry().set_cancel(
                stmt,
                std::sync::Arc::clone(&token) as std::sync::Arc<dyn std::any::Any + Send + Sync>,
            );

            // Occupy the group, as a thread mid-execute would.
            let group = registry().group_of(stmt).expect("live");
            let (holding_tx, holding_rx) = std::sync::mpsc::channel::<()>();
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            let holder = std::thread::spawn(move || {
                let _guard = group.lock();
                holding_tx.send(()).expect("main thread still waiting");
                release_rx.recv().expect("main thread still running");
            });
            holding_rx
                .recv()
                .expect("worker thread panicked before locking");

            let ret = sql_cancel::<MockBackend>(stmt);
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert!(
                token.cancelled.load(Ordering::SeqCst),
                "the backend must be signalled even with the group held"
            );

            release_tx
                .send(())
                .expect("worker thread still waiting to release");
            holder.join().expect("worker thread panicked");

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// The other half of the cross-thread branch's spec obligation: not just
    /// that the backend is signalled (the test above), but that a record
    /// already posted by the function being canceled is left in place and
    /// that `SQLCancel` posts none of its own — "does not clear the
    /// diagnostic records of the being canceled function and does not post
    /// its own diagnostic records". Same handshake, with a diagnostic pushed
    /// before the holder thread ever takes the group, so this exercises the
    /// one thing the busy branch must *not* do, not just the one thing it
    /// must.
    #[test]
    fn cancel_leaves_diagnostics_untouched_while_another_thread_holds_the_group() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();

            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |h| {
                h.diagnostics.push(&crate::errors::OdbcError::NotConnected);
            });

            let group = registry().group_of(stmt).expect("live");
            let (holding_tx, holding_rx) = std::sync::mpsc::channel::<()>();
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            let holder = std::thread::spawn(move || {
                let _guard = group.lock();
                holding_tx.send(()).expect("main thread still waiting");
                release_rx.recv().expect("main thread still running");
            });
            holding_rx
                .recv()
                .expect("worker thread panicked before locking");

            let ret = sql_cancel::<MockBackend>(stmt);
            assert_eq!(ret, SqlReturn::SUCCESS);

            release_tx
                .send(())
                .expect("worker thread still waiting to release");
            holder.join().expect("worker thread panicked");

            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |h| {
                assert_eq!(
                    h.diagnostics.len(),
                    1,
                    "the cross-thread branch must neither clear the existing \
                     record nor post one of its own"
                );
            });

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// A cancel that has already cloned its token must survive the statement
    /// being freed underneath it — the SQLite close-during-interrupt hazard
    /// `Registry::cancel_of`'s doc comment names. Unlike
    /// `a_cloned_cancel_token_survives_the_handle_being_freed`
    /// (`handles::registry`'s own unit test, which drives `Registry::unregister`
    /// directly), this goes through the real `SQLFreeHandle` cascade — the
    /// statement, then its connection, then the environment — to prove the
    /// clone survives the production teardown path, not only the registry
    /// primitive it is built from.
    ///
    /// This does not, however, pin the ordering that matters most —
    /// that `sql_cancel` clones the token *before* attempting `try_lock`.
    /// That ordering is two adjacent statements at the top of `sql_cancel`'s
    /// body with no branch between them; the only way to turn it into a
    /// runtime-observable property would be a test-only hook that pauses
    /// `sql_cancel` between the clone and the `try_lock` so a concurrent free
    /// can be forced into that exact window, which is exactly the kind of
    /// contrived, production-only-for-tests instrumentation this crate
    /// avoids elsewhere. It is left unpinned by a test; reviewing the two
    /// lines is what guards it.
    #[test]
    fn a_cloned_token_outlives_the_statement() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();

            let token = std::sync::Arc::new(crate::test_utils::MockCancelToken::default());
            registry().set_cancel(
                stmt,
                std::sync::Arc::clone(&token) as std::sync::Arc<dyn std::any::Any + Send + Sync>,
            );

            let held = registry().cancel_of(stmt).expect("token");
            assert_eq!(
                crate::handles::free_statement::<MockBackend>(stmt),
                SqlReturn::SUCCESS
            );

            // Still usable: this is the call `SQLCancel` would be making.
            let held_token = held
                .downcast_ref::<crate::test_utils::MockCancelToken>()
                .expect("type");
            held_token.cancelled.store(true, Ordering::SeqCst);
            assert!(token.cancelled.load(Ordering::SeqCst));

            // The rest of the cascade: `free_environment`/`free_connection`
            // need a `&mut HandleScope`, so drive them the way an application
            // does, through `sql_free_handle`, rather than reaching for the
            // lower-level `handles::free_*` primitives `free_statement` above
            // is the exception to (it needs no scope at all).
            assert_eq!(
                crate::ffi::handle::sql_free_handle::<MockBackend>(
                    odbc_sys::HandleType::Dbc as i16,
                    conn
                ),
                SqlReturn::SUCCESS
            );
            assert_eq!(
                crate::ffi::handle::sql_free_handle::<MockBackend>(
                    odbc_sys::HandleType::Env as i16,
                    env
                ),
                SqlReturn::SUCCESS
            );
        }
    }

    // -----------------------------------------------------------------------
    // SQLGetCursorNameW / SQLSetCursorNameW
    // -----------------------------------------------------------------------

    #[test]
    fn get_cursor_name_auto_generates_name() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let mut buf = [0u16; 64];
            let mut name_len: i16 = 0;
            let ret = sql_get_cursor_name_w::<MockBackend>(
                stmt,
                buf.as_mut_ptr(),
                buf.len() as i16,
                &mut name_len,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert!(name_len > 0);
            let name = String::from_utf16_lossy(&buf[..name_len as usize]);
            assert!(
                name.starts_with("SQL_CURSR"),
                "expected auto-generated name starting with SQL_CURSR, got {name:?}"
            );
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    #[test]
    fn set_then_get_cursor_name_round_trips() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();

            // Set cursor name to "MyCursor"
            let name_utf16: Vec<u16> = "MyCursor".encode_utf16().collect();
            let ret = sql_set_cursor_name_w::<MockBackend>(
                stmt,
                name_utf16.as_ptr(),
                name_utf16.len() as i16,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            // Get it back
            let mut buf = [0u16; 64];
            let mut name_len: i16 = 0;
            let ret = sql_get_cursor_name_w::<MockBackend>(
                stmt,
                buf.as_mut_ptr(),
                buf.len() as i16,
                &mut name_len,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            let name = String::from_utf16_lossy(&buf[..name_len as usize]);
            assert_eq!(name, "MyCursor");

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// Reads the SQLSTATE of the statement's first diagnostic record.
    unsafe fn first_sqlstate(stmt: *mut c_void) -> String {
        let mut state = [0u16; 6];
        let mut msg = [0u16; 256];
        let mut native: i32 = 0;
        let mut msg_len: i16 = 0;
        let ret = unsafe {
            crate::ffi::diag::sql_get_diag_rec_w::<MockBackend>(
                HandleType::Stmt as i16,
                stmt,
                1,
                state.as_mut_ptr(),
                std::ptr::from_mut(&mut native),
                msg.as_mut_ptr(),
                256,
                std::ptr::from_mut(&mut msg_len),
            )
        };
        assert_eq!(ret, SqlReturn::SUCCESS, "no diagnostic record was posted");
        String::from_utf16_lossy(&state[..5])
    }

    unsafe fn set_cursor_name(stmt: *mut c_void, name: &str) -> SqlReturn {
        let utf16: Vec<u16> = name.encode_utf16().collect();
        unsafe { sql_set_cursor_name_w::<MockBackend>(stmt, utf16.as_ptr(), utf16.len() as i16) }
    }

    /// `SQLCUR` and `SQL_CUR` are the prefixes `sql_get_cursor_name_w` draws its
    /// generated names from, and the spec's `34000` row reserves them. An
    /// application allowed to take one could name a cursor it does not own.
    #[test]
    fn set_cursor_name_rejects_the_driver_reserved_prefixes() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();

            for name in ["SQLCUR_mine", "SQL_CURSOR1", "sqlcur_lowercase"] {
                assert_eq!(
                    set_cursor_name(stmt, name),
                    SqlReturn::ERROR,
                    "{name} must be refused"
                );
                assert_eq!(first_sqlstate(stmt), "34000", "{name}");
            }

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// An empty name was `HY090`, which the spec marks (DM) and defines as
    /// `NameLength < 0 && != SQL_NTS` — a different condition entirely.
    #[test]
    fn set_cursor_name_rejects_an_empty_name_with_34000_not_hy090() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();

            assert_eq!(set_cursor_name(stmt, ""), SqlReturn::ERROR);
            assert_eq!(first_sqlstate(stmt), "34000");

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// The limit enforced must be the one `SQLGetInfo` advertises, or the driver
    /// contradicts itself.
    #[test]
    fn set_cursor_name_rejects_a_name_longer_than_the_advertised_maximum() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();

            let max = usize::from(crate::types::SQL_MAX_CURSOR_NAME_LEN);
            assert_eq!(
                set_cursor_name(stmt, &"c".repeat(max)),
                SqlReturn::SUCCESS,
                "exactly the advertised maximum must be accepted"
            );
            assert_eq!(
                set_cursor_name(stmt, &"c".repeat(max + 1)),
                SqlReturn::ERROR
            );
            assert_eq!(first_sqlstate(stmt), "34000");

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// "All cursor names within the connection must be unique." Two statements on
    /// one connection may not share a name; the same name on a *different*
    /// connection is fine, which is what makes the scope load-bearing.
    #[test]
    fn set_cursor_name_rejects_a_duplicate_on_the_same_connection() {
        unsafe {
            let (env, conn, stmt_a) = alloc_env_conn_stmt();
            let mut stmt_b: *mut c_void = std::ptr::null_mut();
            assert_eq!(
                sql_alloc_handle::<MockBackend>(HandleType::Stmt as i16, conn, &mut stmt_b),
                SqlReturn::SUCCESS
            );

            assert_eq!(set_cursor_name(stmt_a, "shared"), SqlReturn::SUCCESS);
            assert_eq!(
                set_cursor_name(stmt_b, "shared"),
                SqlReturn::ERROR,
                "a sibling statement must not reuse the name"
            );
            assert_eq!(first_sqlstate(stmt_b), "3C000");

            // Renaming a statement to the name it already holds is not a clash
            // with itself.
            assert_eq!(set_cursor_name(stmt_a, "shared"), SqlReturn::SUCCESS);

            assert_eq!(
                sql_free_handle::<MockBackend>(HandleType::Stmt as i16, stmt_b),
                SqlReturn::SUCCESS
            );
            cleanup_env_conn_stmt(env, conn, stmt_a);
        }
    }

    /// Names differing only in case are **not** the same name. This is a
    /// deliberate ruling, not an oversight — do not "fix" it into a
    /// case-insensitive comparison without new evidence.
    ///
    /// The spec's `3C000` row defines no notion of sameness, and the Comments
    /// state only the quoted half of the rule: "in ODBC 3.x, if a cursor name is
    /// a quoted identifier, it is treated in a case-sensitive manner". That
    /// implies something about unquoted names without ever saying what, so this
    /// crate's policy is to read a mature driver rather than infer from the
    /// silence.
    ///
    /// Four were read, and none of them implements a duplicate-cursor-name
    /// check at all, so none can supply a folding rule:
    ///
    /// - psqlODBC `PGAPI_SetCursorName` (`results.c`) — checks the length
    ///   against `MAX_CURSOR_LEN` and stores; no search of sibling statements.
    /// - MySQL Connector/ODBC `MySQLSetCursorName` (`driver/cursor.cc`) —
    ///   length, plus `myodbc_casecmp` against the reserved `SQLCUR`/`SQL_CUR`
    ///   prefixes, then stores.
    /// - FreeTDS `SQLSetCursorName` (`src/odbc/odbc.c`) — `24000` if a cursor is
    ///   already open, then copies the string.
    /// - unixODBC's Driver Manager (`DriverManager/SQLSetCursorName.c`) —
    ///   validates the handle, the name and the statement state, then forwards.
    ///
    /// A search for the literal `3C000` finds nothing in the first three, and in
    /// unixODBC only a string-table entry in a bundled sample driver that no
    /// code path raises. Core is therefore ahead of all four in performing the
    /// check at all, and there is no established practice to match on how names
    /// are compared. Byte-exact is what remains once inference from silence is
    /// off the table.
    #[test]
    fn cursor_names_differing_only_in_case_are_distinct() {
        unsafe {
            let (env, conn, stmt_a) = alloc_env_conn_stmt();
            let mut stmt_b: *mut c_void = std::ptr::null_mut();
            assert_eq!(
                sql_alloc_handle::<MockBackend>(HandleType::Stmt as i16, conn, &mut stmt_b),
                SqlReturn::SUCCESS
            );

            assert_eq!(set_cursor_name(stmt_a, "C1"), SqlReturn::SUCCESS);
            assert_eq!(
                set_cursor_name(stmt_b, "c1"),
                SqlReturn::SUCCESS,
                "the comparison is byte-exact; no driver read supports folding",
            );

            assert_eq!(
                sql_free_handle::<MockBackend>(HandleType::Stmt as i16, stmt_b),
                SqlReturn::SUCCESS
            );
            cleanup_env_conn_stmt(env, conn, stmt_a);
        }
    }

    /// The same name on a second connection is legal — the spec scopes uniqueness
    /// to the connection, and a check that walked more would reject valid calls.
    #[test]
    fn set_cursor_name_allows_the_same_name_on_another_connection() {
        unsafe {
            let (env, conn_a, stmt_a) = alloc_env_conn_stmt();
            let mut conn_b: *mut c_void = std::ptr::null_mut();
            assert_eq!(
                sql_alloc_handle::<MockBackend>(HandleType::Dbc as i16, env, &mut conn_b),
                SqlReturn::SUCCESS
            );
            let mut stmt_b: *mut c_void = std::ptr::null_mut();
            assert_eq!(
                sql_alloc_handle::<MockBackend>(HandleType::Stmt as i16, conn_b, &mut stmt_b),
                SqlReturn::SUCCESS
            );

            assert_eq!(set_cursor_name(stmt_a, "shared"), SqlReturn::SUCCESS);
            assert_eq!(
                set_cursor_name(stmt_b, "shared"),
                SqlReturn::SUCCESS,
                "uniqueness is scoped to the connection"
            );

            assert_eq!(
                sql_free_handle::<MockBackend>(HandleType::Stmt as i16, stmt_b),
                SqlReturn::SUCCESS
            );
            assert_eq!(
                sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn_b),
                SqlReturn::SUCCESS
            );
            cleanup_env_conn_stmt(env, conn_a, stmt_a);
        }
    }

    #[test]
    fn get_cursor_name_truncates_with_success_with_info() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();

            // Set a long cursor name: "VeryLongCursorName" (18 chars)
            let long_name = "VeryLongCursorName";
            let name_utf16: Vec<u16> = long_name.encode_utf16().collect();
            let ret = sql_set_cursor_name_w::<MockBackend>(
                stmt,
                name_utf16.as_ptr(),
                name_utf16.len() as i16,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            // Get into a small buffer (5 elements: room for 4 chars + null)
            let mut buf = [0u16; 5];
            let mut name_len: i16 = 0;
            let ret = sql_get_cursor_name_w::<MockBackend>(
                stmt,
                buf.as_mut_ptr(),
                buf.len() as i16,
                &mut name_len,
            );
            assert_eq!(ret, SqlReturn::SUCCESS_WITH_INFO);
            // name_len reports the full length, not the truncated length
            assert_eq!(name_len, 18);

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    #[test]
    fn set_cursor_name_null_pointer_returns_error() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = sql_set_cursor_name_w::<MockBackend>(stmt, std::ptr::null(), 0);
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    // -----------------------------------------------------------------------
    // SQLBulkOperations
    // -----------------------------------------------------------------------

    #[test]
    fn bulk_operations_null_handle_returns_invalid() {
        unsafe {
            let ret =
                sql_bulk_operations::<MockBackend>(std::ptr::null_mut(), BulkOperation::Add as i16);
            assert_eq!(ret, SqlReturn::INVALID_HANDLE);
        }
    }

    #[test]
    fn bulk_operations_returns_hyc00() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = sql_bulk_operations::<MockBackend>(stmt, BulkOperation::Add as i16);
            assert_eq!(ret, SqlReturn::ERROR);
            // Verify HYC00 diagnostic was set.
            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                let rec = handle.diagnostics.get(0).expect("diagnostic record");
                assert_eq!(
                    rec.sqlstate.as_str(),
                    "HYC00",
                    "expected HYC00, got {}",
                    rec.sqlstate.as_str()
                );
            });
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    #[test]
    fn bulk_operations_invalid_operation_returns_hy092() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            // 99 is not a valid SQLBulkOperations operation.
            let ret = sql_bulk_operations::<MockBackend>(stmt, 99);
            assert_eq!(ret, SqlReturn::ERROR);
            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                let rec = handle.diagnostics.get(0).expect("diagnostic record");
                assert_eq!(
                    rec.sqlstate.as_str(),
                    "HY092",
                    "expected HY092, got {}",
                    rec.sqlstate.as_str()
                );
            });
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    // -----------------------------------------------------------------------
    // SQLSetPos
    // -----------------------------------------------------------------------

    #[test]
    fn set_pos_null_handle_returns_invalid() {
        unsafe {
            let ret = sql_set_pos::<MockBackend>(
                std::ptr::null_mut(),
                1,
                SQL_POSITION,
                SQL_LOCK_NO_CHANGE,
            );
            assert_eq!(ret, SqlReturn::INVALID_HANDLE);
        }
    }

    #[test]
    fn set_pos_returns_hyc00() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = sql_set_pos::<MockBackend>(stmt, 1, SQL_POSITION, SQL_LOCK_NO_CHANGE);
            assert_eq!(ret, SqlReturn::ERROR);
            // Verify HYC00 diagnostic was set.
            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                let rec = handle.diagnostics.get(0).expect("diagnostic record");
                assert_eq!(
                    rec.sqlstate.as_str(),
                    "HYC00",
                    "expected HYC00, got {}",
                    rec.sqlstate.as_str()
                );
            });
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    #[test]
    fn set_pos_invalid_operation_returns_hy092() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            // 99 is not a valid SQLSetPos operation.
            let ret = sql_set_pos::<MockBackend>(stmt, 1, 99, SQL_LOCK_NO_CHANGE);
            assert_eq!(ret, SqlReturn::ERROR);
            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                let rec = handle.diagnostics.get(0).expect("diagnostic record");
                assert_eq!(
                    rec.sqlstate.as_str(),
                    "HY092",
                    "expected HY092, got {}",
                    rec.sqlstate.as_str()
                );
            });
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    #[test]
    fn set_pos_invalid_lock_type_returns_hy092() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            // 99 is not a valid lock type.
            let ret = sql_set_pos::<MockBackend>(stmt, 1, SQL_POSITION, 99);
            assert_eq!(ret, SqlReturn::ERROR);
            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                let rec = handle.diagnostics.get(0).expect("diagnostic record");
                assert_eq!(
                    rec.sqlstate.as_str(),
                    "HY092",
                    "expected HY092, got {}",
                    rec.sqlstate.as_str()
                );
            });
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    // -----------------------------------------------------------------------
    // SQLCloseCursor tells the backend its cursor is closing
    // -----------------------------------------------------------------------

    /// Env + connection + statement with a cursor open, for an arbitrary
    /// backend.
    unsafe fn cursor_open_stmt_for<B: Backend>() -> (*mut c_void, *mut c_void, *mut c_void) {
        unsafe {
            let (env, conn, stmt) = crate::test_utils::alloc_connected_env_conn_stmt::<B>();
            let sql: Vec<u16> = "SELECT 1".encode_utf16().collect();
            assert_eq!(
                crate::ffi::execute::sql_exec_direct_w::<B>(
                    stmt,
                    sql.as_ptr(),
                    i32::try_from(sql.len()).expect("SQL fits in i32"),
                ),
                SqlReturn::SUCCESS,
                "precondition: a cursor is open, so SQLCloseCursor has something to close",
            );
            (env, conn, stmt)
        }
    }

    /// The spec's `24000` row carries no (DM) marker, and the Comments give the
    /// rule in the positive direction: a cursor may be renamed "as long as the
    /// cursor is in an allocated or prepared state". Renaming a cursor an
    /// application is already fetching from would leave a `WHERE CURRENT OF` in
    /// flight pointing at a name that no longer resolves.
    ///
    /// Driven through `sql_exec_direct_w` on a backend that really produces a
    /// result set — asserting on a hand-set `cursor_open` would prove only that
    /// the test can write a bool.
    #[test]
    fn set_cursor_name_is_refused_once_the_statement_has_executed() {
        unsafe {
            let (env, conn, stmt) = cursor_open_stmt_for::<MockFailingCloseBackend>();

            let name: Vec<u16> = "TooLate".encode_utf16().collect();
            assert_eq!(
                sql_set_cursor_name_w::<MockFailingCloseBackend>(
                    stmt,
                    name.as_ptr(),
                    name.len() as i16,
                ),
                SqlReturn::ERROR,
                "a cursor name set after execution must be refused"
            );

            let mut state = [0u16; 6];
            let mut msg = [0u16; 256];
            let mut native: i32 = 0;
            let mut msg_len: i16 = 0;
            assert_eq!(
                crate::ffi::diag::sql_get_diag_rec_w::<MockFailingCloseBackend>(
                    HandleType::Stmt as i16,
                    stmt,
                    1,
                    state.as_mut_ptr(),
                    std::ptr::from_mut(&mut native),
                    msg.as_mut_ptr(),
                    256,
                    std::ptr::from_mut(&mut msg_len),
                ),
                SqlReturn::SUCCESS
            );
            assert_eq!(String::from_utf16_lossy(&state[..5]), "24000");

            crate::test_utils::cleanup_connected_env_conn_stmt::<MockFailingCloseBackend>(
                env, conn, stmt,
            );
        }
    }

    /// Appendix B's `SQLSetCursorName` row reads `--` for `S2-S3 Prepared` and
    /// `24000` for `S4 Executed` and `S5-S7 Cursor`, and the Comments say the
    /// same in the positive direction: a cursor may be renamed "as long as the
    /// cursor is in an allocated or prepared state".
    ///
    /// `SQLPrepare` -> `SQLSetCursorName` -> `SQLExecute` is the ordinary
    /// positioned-update setup, so a driver that refuses the middle call locks
    /// the pattern out entirely.
    #[test]
    fn set_cursor_name_is_allowed_in_the_prepared_state() {
        unsafe {
            let (env, conn, stmt) =
                crate::test_utils::alloc_connected_env_conn_stmt::<MockBackend>();

            let sql: Vec<u16> = "SELECT * FROM t WHERE id = 1".encode_utf16().collect();
            assert_eq!(
                crate::ffi::execute::sql_prepare_w::<MockBackend>(
                    stmt,
                    sql.as_ptr(),
                    i32::try_from(sql.len()).expect("the fixed test SQL is short"),
                ),
                SqlReturn::SUCCESS,
                "precondition: the statement is prepared but not executed",
            );

            assert_eq!(
                set_cursor_name(stmt, "PreparedCursor"),
                SqlReturn::SUCCESS,
                "a prepared statement's cursor may still be named",
            );

            crate::test_utils::cleanup_connected_env_conn_stmt::<MockBackend>(env, conn, stmt);
        }
    }

    /// The other half of the same table row: state S4, executed with no result
    /// set, is `24000` even though no cursor was ever opened. Dropping the
    /// `statement.is_some()` term outright — the obvious way to let the prepared
    /// state through — would accept this, so it is pinned in its own test.
    ///
    /// `MockStatement` reports zero columns, so `SQLExecute` leaves the handle
    /// in S4 rather than S5.
    #[test]
    fn set_cursor_name_is_refused_in_the_executed_state_with_no_result_set() {
        unsafe {
            let (env, conn, stmt) =
                crate::test_utils::alloc_connected_env_conn_stmt::<MockBackend>();

            let sql: Vec<u16> = "UPDATE t SET a = 1".encode_utf16().collect();
            assert_eq!(
                crate::ffi::execute::sql_prepare_w::<MockBackend>(
                    stmt,
                    sql.as_ptr(),
                    i32::try_from(sql.len()).expect("the fixed test SQL is short"),
                ),
                SqlReturn::SUCCESS
            );
            assert_eq!(
                crate::ffi::execute::sql_execute::<MockBackend>(stmt),
                SqlReturn::SUCCESS,
                "precondition: the statement executed",
            );
            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                assert!(
                    !handle.cursor_open,
                    "precondition: state S4, executed with no result set",
                );
            });

            assert_eq!(
                set_cursor_name(stmt, "TooLate"),
                SqlReturn::ERROR,
                "state S4 is 24000 in Appendix B, cursor or no cursor",
            );
            assert_eq!(first_sqlstate(stmt), "24000");

            crate::test_utils::cleanup_connected_env_conn_stmt::<MockBackend>(env, conn, stmt);
        }
    }

    /// `SQLCloseCursor` reached only `discard_result_set`, so a backend needing
    /// to release a server-side cursor, cancel a pending fetch, or return a
    /// connection to a pool never heard about the most obvious place an
    /// application closes a cursor — it got a `Drop`, where a failure cannot be
    /// reported at all. `StatementBackend::close_cursor` is fallible precisely
    /// because that teardown is a round trip that can fail.
    #[test]
    fn close_cursor_calls_the_backend_and_reports_its_failure() {
        unsafe {
            let (env, conn, stmt) = cursor_open_stmt_for::<MockFailingCloseBackend>();

            assert_eq!(
                sql_close_cursor::<MockFailingCloseBackend>(stmt),
                SqlReturn::ERROR,
                "a cursor whose teardown failed must not be reported as closed cleanly",
            );
            let state = with_handle::<
                MockFailingCloseBackend,
                StatementHandle<MockFailingCloseBackend>,
                _,
            >(stmt, |h| {
                h.diagnostics
                    .get(0)
                    .expect("a diagnostic record")
                    .sqlstate
                    .as_str()
                    .to_owned()
            });
            assert_eq!(
                state, "08S01",
                "the backend's own SQLSTATE, not a state core invented",
            );

            crate::test_utils::cleanup_connected_env_conn_stmt::<MockFailingCloseBackend>(
                env, conn, stmt,
            );
        }
    }

    /// The result set is discarded **even when `close_cursor` failed**.
    ///
    /// Leaving it in place would hand the application a cursor it has no way to
    /// clear: the second `SQLCloseCursor` would call the failing backend again
    /// and fail again, for ever. `24000` here proves the first call did discard
    /// it. This mirrors `SQLEndTran`'s existing "recorded and carried, not
    /// swallowed" shape.
    #[test]
    fn close_cursor_discards_the_result_set_even_when_the_backend_fails() {
        unsafe {
            let (env, conn, stmt) = cursor_open_stmt_for::<MockFailingCloseBackend>();

            assert_eq!(
                sql_close_cursor::<MockFailingCloseBackend>(stmt),
                SqlReturn::ERROR,
                "precondition: the backend's close_cursor failed",
            );
            assert_eq!(
                sql_close_cursor::<MockFailingCloseBackend>(stmt),
                SqlReturn::ERROR,
                "a second close still errors, but for a different reason",
            );
            let state = with_handle::<
                MockFailingCloseBackend,
                StatementHandle<MockFailingCloseBackend>,
                _,
            >(stmt, |h| {
                h.diagnostics
                    .get(0)
                    .expect("a diagnostic record")
                    .sqlstate
                    .as_str()
                    .to_owned()
            });
            assert_eq!(
                state, "24000",
                "no cursor is left open, so the failed close did discard the result set",
            );

            crate::test_utils::cleanup_connected_env_conn_stmt::<MockFailingCloseBackend>(
                env, conn, stmt,
            );
        }
    }
}
