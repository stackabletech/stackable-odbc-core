//! Handle lifecycle entry points: `SQLAllocHandle`, `SQLFreeHandle`, `SQLFreeStmt`.

use crate::backend::Backend;
use crate::errors::OdbcError;
use crate::handles::{
    ConnectionHandle, StatementHandle, alloc_connection, alloc_environment, alloc_statement,
    free_connection, free_environment, free_statement,
};
use crate::panic::panic_safe;
use crate::types::{SqlReturn, free_stmt_option_from_raw, handle_type_from_raw};
use odbc_sys::{FreeStmtOption, HandleType};
use std::ffi::c_void;
/// Generic implementation of SQLAllocHandle.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlallochandle-function>
///
/// Dispatches based on `handle_type`:
/// - `SQL_HANDLE_ENV` (1) — allocate an environment handle (`input_handle` must be null)
/// - `SQL_HANDLE_DBC` (2) — allocate a connection handle (`input_handle` must be a valid env)
/// - `SQL_HANDLE_STMT` (3) — allocate a statement handle (`input_handle` must be a valid conn)
/// - `SQL_HANDLE_DESC` (4) — not implemented; returns `SQL_ERROR` with HYC00
/// - `SQL_HANDLE_DBC_INFO_TOKEN` — not implemented; returns `SQL_ERROR` with HYC00
///
/// # Spec compliance
///
/// Every SQLSTATE from the SQLAllocHandle Diagnostics table, and whether this
/// driver returns it:
///
/// - 01000: General warning (SQL_SUCCESS_WITH_INFO). Not produced here.
/// - 08003: Connection not open (driver-manager-handled; not returned here).
/// - HY000: General error. Returned by the backend if handle setup fails
///   without a more specific code.
/// - HY001: Memory allocation error (driver-manager-handled; allocation here is
///   an infallible `Box`, so not returned).
/// - HY009: Returns `SQL_ERROR` if `output_handle_ptr` is null. The spec
///   annotates this (DM); it is guarded defensively here.
/// - HY010: Function sequence error (driver-manager-handled; not returned here).
/// - HY013: Memory management error (driver-manager-handled; not returned here).
/// - HY014: Limit on the number of handles exceeded. Not returned; no fixed
///   limit is imposed.
/// - HY092: Returns `SQL_ERROR` if `handle_type` is not a recognized value.
///   Sets `*output_handle` to null on error (unless `output_handle` itself is
///   null). The spec annotates this (DM); it is guarded defensively here.
/// - HY117: Connection suspended due to unknown transaction state
///   (driver-manager-handled; not returned here).
/// - HYC00: Optional feature not implemented. Returned, with this SQLSTATE
///   posted, for `SQL_HANDLE_DESC` and `SQL_HANDLE_DBC_INFO_TOKEN` allocation.
///   This is the only un-annotated code in this function's table covering an
///   unimplemented handle type, and the spec names `SQL_HANDLE_DESC` in its
///   description; `IM001`, the alternative, is (DM). Note `SQLFreeHandle`
///   answers `HY000` for the same condition, because its table has no `HYC00`
///   row at all — the asymmetry is what the two tables say.
/// - HYT01: Connection timeout expired (not returned here; allocation performs
///   no network I/O).
/// - IM001: Driver does not support this function (driver-manager-handled; not
///   returned here).
///
/// Handle-specific rules:
/// - For Env: `input_handle` must be `SQL_NULL_HANDLE` (null).
/// - For Dbc/Stmt: `input_handle` must be non-null and a valid parent handle.
///
/// # Safety
///
/// `input_handle` must be a valid handle of the appropriate parent type (or null
/// for environment allocation). `output_handle` must be a valid pointer if non-null.
pub unsafe fn sql_alloc_handle<B: Backend>(
    handle_type: i16,
    input_handle: *mut c_void,
    output_handle_ptr: *mut *mut c_void,
) -> SqlReturn {
    if handle_type == HandleType::Env as i16 {
        crate::logging::init_logging();
    }
    tracing::trace!(
        "SQLAllocHandle(handle_type={}, input_handle={:?}, output_handle_ptr={:?})",
        handle_type,
        input_handle,
        output_handle_ptr,
    );
    let ht_log = handle_type_from_raw(handle_type);
    tracing::debug!(
        "SQLAllocHandle(handle_type={:?}, input_handle={:?}, output_handle_ptr={:?})",
        ht_log,
        input_handle,
        output_handle_ptr,
    );
    // Wrapped in panic_safe like every other FFI entry point: allocation
    // runs Box allocation and backend construction, and a panic must not unwind
    // across the extern "system" boundary (undefined behaviour). input_handle is
    // the new child's parent (null for SQL_HANDLE_ENV, which has none), so this
    // holds exactly the group the registration below joins — SQL_HANDLE_DBC
    // locks the environment, SQL_HANDLE_STMT locks the connection, and neither
    // nests. The output pointer is set to SQL_NULL_HANDLE up front so every
    // error path, including a caught panic, leaves it null, and only the
    // success paths overwrite it.
    let ret = unsafe {
        panic_safe::<B, _>(input_handle, |scope| {
            // Spec: the diagnostic for this call is read with `Handle` set to
            // `InputHandle`, so that queue is this call's output channel and is
            // cleared at entry like every other function's. Before any error
            // return, so a diagnostic this call posts survives. `None` for a
            // null `InputHandle` (environment allocation, which has no parent).
            if let Some(queue) = scope.diagnostics::<B>(input_handle) {
                queue.clear();
            }
            // Spec HY009: OutputHandlePtr must not be null.
            if output_handle_ptr.is_null() {
                tracing::error!("SQLAllocHandle: output_handle_ptr is null (HY009)");
                return Ok(SqlReturn::ERROR);
            }
            // Spec: on any error OutputHandlePtr is set to SQL_NULL_HANDLE.
            // SAFETY: output_handle_ptr was verified non-null above. Unsafe ops in
            // this closure are covered by the outer `unsafe` block around panic_safe.
            std::ptr::write_unaligned(output_handle_ptr, std::ptr::null_mut());
            // Parse handle type. Spec HY092: must be a recognized value.
            let Some(ht) = handle_type_from_raw(handle_type) else {
                tracing::error!(
                    "SQLAllocHandle: invalid handle_type {} (HY092)",
                    handle_type
                );
                return Ok(SqlReturn::ERROR);
            };
            let ret = match ht {
                HandleType::Env => {
                    // Spec: InputHandle must be SQL_NULL_HANDLE for environment allocation.
                    if !input_handle.is_null() {
                        tracing::error!(
                            "SQLAllocHandle: input_handle must be null for SQL_HANDLE_ENV"
                        );
                        return Ok(SqlReturn::ERROR);
                    }
                    // SAFETY: output_handle_ptr was verified non-null above; alloc_environment
                    // writes a Box<EnvironmentHandle<B>> pointer and transfers ownership to the caller.
                    alloc_environment::<B>(output_handle_ptr)
                }
                HandleType::Dbc => {
                    // Spec: InputHandle must be a valid environment handle.
                    if input_handle.is_null() {
                        tracing::error!(
                            "SQLAllocHandle: input_handle must be non-null for SQL_HANDLE_DBC"
                        );
                        return Ok(SqlReturn::ERROR);
                    }
                    // SAFETY: input_handle is non-null (checked above); alloc_connection looks
                    // it up in the registry, which validates liveness and that it names an
                    // environment specifically, without ever dereferencing it. output_handle_ptr
                    // is non-null.
                    alloc_connection::<B>(input_handle, output_handle_ptr)
                }
                HandleType::Stmt => {
                    // Spec: InputHandle must be a valid connection handle.
                    if input_handle.is_null() {
                        tracing::error!(
                            "SQLAllocHandle: input_handle must be non-null for SQL_HANDLE_STMT"
                        );
                        return Ok(SqlReturn::ERROR);
                    }
                    // The connection's `SQL_ATTR_METADATA_ID`, which the new
                    // statement starts from — see `alloc_statement`'s doc
                    // comment for why this attribute and no other. Read
                    // through the scope, so the value comes from a validated
                    // handle under the group lock this call already holds.
                    // A connection that never had it set contributes nothing.
                    let inherited_metadata_id = scope
                        .get::<ConnectionHandle<B>>(input_handle)
                        .ok()
                        .and_then(|conn| {
                            conn.attrs
                                .get(&crate::types::ConnectionAttribute::METADATA_ID.0)
                                .copied()
                        });
                    if let Some(value) = inherited_metadata_id {
                        tracing::debug!(
                            "SQLAllocHandle: statement inherits SQL_ATTR_METADATA_ID={} from its connection",
                            value
                        );
                    }
                    // SAFETY: input_handle is non-null (checked above); alloc_statement looks
                    // it up in the registry, which validates liveness and that it names a
                    // connection specifically, without ever dereferencing it. output_handle_ptr
                    // is non-null.
                    alloc_statement::<B>(input_handle, output_handle_ptr, inherited_metadata_id)
                }
                HandleType::Desc => {
                    // Explicit descriptor handle allocation (SQL_HANDLE_DESC) is not yet implemented.
                    // The Windows DM auto-allocates implicit descriptors; applications rarely call this directly.
                    // Full implementation requires a descriptor handle registry. Deferred.
                    //
                    // HYC00 is the only un-annotated code this function's table
                    // offers for an unimplemented handle type, and it names this
                    // case directly; IM001, the alternative, is (DM). Note
                    // SQLFreeHandle answers HY000 for the same condition, because
                    // its table lists no HYC00 at all. The `*output_handle_ptr =
                    // SQL_NULL_HANDLE` write above already ran, so the spec's
                    // "set to SQL_NULL_HANDLE on error" still holds on this path.
                    return Err(OdbcError::general(
                        "SQLAllocHandle: SQL_HANDLE_DESC is not implemented",
                        crate::types::SqlState::optional_feature_not_implemented(),
                    ));
                }
                HandleType::DbcInfoToken => {
                    // Only used between Driver Manager and drivers for connection pooling.
                    // Applications should not use this handle type.
                    return Err(OdbcError::general(
                        "SQLAllocHandle: SQL_HANDLE_DBC_INFO_TOKEN is not implemented",
                        crate::types::SqlState::optional_feature_not_implemented(),
                    ));
                }
            };
            Ok(ret)
        })
    };
    tracing::debug!("SQLAllocHandle -> {:?}", ret);
    ret
}
/// Generic implementation of SQLFreeHandle.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlfreehandle-function>
///
/// Frees resources associated with a specific environment, connection, statement, or descriptor
/// handle. Dispatches based on `handle_type`:
/// - `SQL_HANDLE_ENV` (1) — free an environment handle
/// - `SQL_HANDLE_DBC` (2) — free a connection handle
/// - `SQL_HANDLE_STMT` (3) — free a statement handle
/// - `SQL_HANDLE_DESC` (4) — not implemented; returns `SQL_ERROR` with HY000
/// - `SQL_HANDLE_DBC_INFO_TOKEN` — not implemented; returns `SQL_ERROR` with HY000
///
/// Returns `SQL_INVALID_HANDLE` for unrecognized handle types — a value outside
/// the five the spec defines, which is what the spec prescribes for that case.
/// If `SQL_ERROR` is returned the handle is still valid.
///
/// # Parameters
///
/// - `handle_type`: The type of handle to free (`SQL_HANDLE_ENV`, `SQL_HANDLE_DBC`,
///   `SQL_HANDLE_STMT`, or `SQL_HANDLE_DESC`).
/// - `handle`: The handle to free.
///
/// # Spec compliance
///
/// - HY000: Returns `SQL_ERROR` with this SQLSTATE when `handle_type` is
///   `SQL_HANDLE_DESC` or `SQL_HANDLE_DBC_INFO_TOKEN` — both valid handle types
///   this driver does not implement. The spec's table for this function lists no
///   `HYC00`, so the catch-all is the correct code here even though
///   `SQLAllocHandle` answers `HYC00` for the same condition.
/// - HY001: Memory allocation error (driver-manager-handled; not returned here).
/// - HY010: Returns `SQL_ERROR` if `handle_type` is `SQL_HANDLE_ENV` and at least one
///   connection handle is still allocated under it (`SQLFreeHandle` with `SQL_HANDLE_DBC`
///   must be called first for every connection). Also returns `SQL_ERROR` if `handle_type`
///   is `SQL_HANDLE_DBC` and the connection is still open (`SQLDisconnect` must be called
///   first) or there are still active statement handles. The remaining HY010 conditions
///   (async execution in progress, data-at-execution pending, etc.) are
///   driver-manager-handled; not returned here.
/// - HY013: Memory management error (driver-manager-handled; not returned here).
/// - HY017: Invalid use of an automatically allocated descriptor handle
///   (driver-manager-handled; not returned here).
/// - HY117: Connection suspended due to unknown transaction state
///   (driver-manager-handled; not returned here).
/// - HYT01: Connection timeout expired (driver-manager-handled; not returned here).
/// - IM001: Driver does not support this function (driver-manager-handled; not returned
///   here).
///
/// # Safety
///
/// `handle` must point to a valid handle of the corresponding type, previously
/// allocated by the matching `alloc_*` function.
pub unsafe fn sql_free_handle<B: Backend>(handle_type: i16, handle: *mut c_void) -> SqlReturn {
    tracing::trace!(
        "SQLFreeHandle(handle_type={}, handle={:?})",
        handle_type,
        handle
    );
    let ht_log = handle_type_from_raw(handle_type);
    tracing::debug!(
        "SQLFreeHandle(handle_type={:?}, handle={:?})",
        ht_log,
        handle
    );
    // Wrapped in panic_safe like every other FFI entry point: free_connection
    // and free_statement drop the backend connection/statement, whose Drop can run
    // arbitrary code (closing sockets, draining residual pages). A panic there must
    // not unwind across the extern "system" boundary (undefined behaviour). `handle`
    // itself is passed to panic_safe (rather than null), because `handle` names its
    // own lock group regardless of whether it turns out to be an environment, a
    // connection or a statement — resolving that group is exactly what
    // `free_environment`/`free_connection` need `scope` for, and what holds the lock
    // for the duration of `free_statement`'s registry unregister and `Box::from_raw`.
    let ret = unsafe {
        panic_safe::<B, _>(handle, |scope| {
            // Spec: clear at the start of the call. This matters when the free
            // fails and the handle survives — a connection with live
            // statements, or an unimplemented handle type — because that is
            // exactly when an application reads diagnostics, and a stale record
            // from the previous call would be served as record 1.
            if let Some(queue) = scope.diagnostics::<B>(handle) {
                queue.clear();
            }
            let Some(ht) = handle_type_from_raw(handle_type) else {
                tracing::error!("SQLFreeHandle: invalid handle_type {}", handle_type);
                return Ok(SqlReturn::INVALID_HANDLE);
            };
            let ret = match ht {
                HandleType::Env => free_environment::<B>(handle, scope),
                HandleType::Dbc => free_connection::<B>(handle, scope),
                // SAFETY: free_statement does its own registry validation; the
                // `unsafe` here is covered by the outer `unsafe` block.
                HandleType::Stmt => free_statement::<B>(handle),
                HandleType::Desc | HandleType::DbcInfoToken => {
                    // Explicit descriptor and DBC_INFO_TOKEN free: not implemented
                    // (matching alloc). Deferred.
                    //
                    // HY000, not HYC00: SQLFreeHandle's diagnostics table has no
                    // HYC00 row, while HY000 is listed and is the spec's catch-all
                    // for a condition with no specific SQLSTATE. (SQLAllocHandle's
                    // table *does* list HYC00 un-annotated, which is why its
                    // equivalent arm differs — the asymmetry is what the two
                    // tables say, not an oversight.) Not SQL_INVALID_HANDLE
                    // either: the spec reserves that for a HandleType outside the
                    // five valid values, and both of these are valid.
                    //
                    // Returned as an error rather than logged so the record
                    // reaches the queue; `panic_safe` posts it and converts it to
                    // SQL_ERROR. No `tracing::error!` alongside it — an OdbcError
                    // already logs, and pairing the two double-logs.
                    return Err(OdbcError::general(
                        format!("SQLFreeHandle: handle type {ht:?} is not implemented"),
                        crate::types::SqlState::general_error(),
                    ));
                }
            };
            Ok(ret)
        })
    };
    tracing::debug!("SQLFreeHandle -> {:?}", ret);
    ret
}
/// Generic implementation of SQLFreeStmt.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlfreestmt-function>
///
/// Stops processing associated with a specific statement, optionally closes any open cursor,
/// discards pending results, or releases column/parameter bindings.
///
/// # Parameters
///
/// - `statement_handle`: The statement handle to operate on.
/// - `option`: One of the following option values:
///   - `SQL_CLOSE` (0) — closes any open cursor and discards pending results; no error if
///     no cursor is open.
///   - `SQL_DROP` (1) — deprecated; the Driver Manager maps this to `SQLFreeHandle`
///     before it reaches the driver. If received directly, `SQL_ERROR` is returned
///     (HY092: option type out of range).
///   - `SQL_UNBIND` (2) — releases all column bindings set by `SQLBindCol`.
///   - `SQL_RESET_PARAMS` (3) — releases all parameter bindings set by `SQLBindParameter`.
///
/// # Spec compliance
///
/// - 01000: General warning (driver-manager-handled; not returned here).
/// - HY000: General error (driver-manager-handled; not returned here).
/// - HY001: Memory allocation error (driver-manager-handled; not returned here).
/// - HY010: Function sequence error (async execution in progress, data-at-execution pending,
///   etc.) — driver-manager-handled; not returned here.
/// - HY013: Memory management error (driver-manager-handled; not returned here).
/// - HY092: Returns `SQL_ERROR` and posts this SQLSTATE if `option` is not one of
///   the recognised values (`SQL_CLOSE`, `SQL_UNBIND`, `SQL_RESET_PARAMS`). The
///   spec marks this row **(DM)**, so a conforming Driver Manager normally
///   rejects the call before it reaches the driver; the check is kept because the
///   function must still do something with an option it cannot parse, and posting
///   the same SQLSTATE the DM would means an application branches identically
///   whichever layer caught it. `SQL_DROP` (1) is handled as a special case for
///   Windows DM compatibility (the Windows DM passes it through to the driver
///   instead of mapping it to `SQLFreeHandle`); it is forwarded to
///   `sql_free_handle` rather than rejected.
/// - HYT01: Connection timeout expired (driver-manager-handled; not returned here).
/// - IM001: Driver does not support this function (driver-manager-handled; not returned
///   here).
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
pub unsafe fn sql_free_stmt<B: Backend>(statement_handle: *mut c_void, option: u16) -> SqlReturn {
    tracing::trace!(
        "SQLFreeStmt(handle={:?}, option={})",
        statement_handle,
        option
    );
    // SQL_DROP (1) is deprecated. The ODBC 3.x spec says the Driver Manager maps this to
    // SQLFreeHandle(SQL_HANDLE_STMT) before it reaches the driver. unixODBC does this correctly,
    // but the Windows DM passes SQL_DROP through to the driver directly. Handle it here for
    // Windows DM compatibility.
    if option == crate::types::SQL_DROP {
        tracing::warn!(
            "SQLFreeStmt: received SQL_DROP (Windows DM compat) - forwarding to sql_free_handle"
        );
        let ret = unsafe { sql_free_handle::<B>(HandleType::Stmt as i16, statement_handle) };
        tracing::debug!("SQLFreeStmt(SQL_DROP) -> {:?}", ret);
        return ret;
    }
    // Wrapped in panic_safe like every other FFI entry point: SQL_CLOSE drops
    // the backend statement, and a backend cursor's Drop can run arbitrary code
    // (draining residual pages, for example). A panic there would otherwise
    // unwind across the `extern "system"` boundary, which is undefined behaviour.
    //
    // SAFETY: statement_handle is null or a valid StatementHandle<B> allocated by
    // sql_alloc_handle; kind and group are validated by scope.get inside the closure.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, |scope| {
            let stmt = scope.get::<StatementHandle<B>>(statement_handle)?;
            stmt.diagnostics.clear();

            // Parsed inside `panic_safe`, not before it: returning early out
            // there left no HandleScope, so the SQL_ERROR reached the
            // application with an empty diagnostic queue and SQLGetDiagRec
            // answered SQL_NO_DATA — a failure with no SQLSTATE to branch on.
            //
            // The spec marks HY092 (DM) for this function, so a conforming
            // Driver Manager rejects the call before the driver sees it. The
            // check is kept regardless, because the function must still do
            // something with an option it cannot parse, and posting the same
            // SQLSTATE the DM would means an application branches identically
            // whichever layer caught it.
            let Some(opt) = free_stmt_option_from_raw(option) else {
                return Err(OdbcError::general(
                    format!("SQLFreeStmt: option {option} is not a recognised value"),
                    crate::types::SqlState::invalid_attribute_option_identifier(),
                ));
            };
            tracing::debug!(
                "SQLFreeStmt(handle={:?}, option={:?})",
                statement_handle,
                opt
            );

            match opt {
                // Discard the result set so the handle is ready for a new statement.
                FreeStmtOption::Close => stmt.discard_result_set(),
                FreeStmtOption::Unbind => stmt.bindings.clear(),
                FreeStmtOption::ResetParams => stmt.param_bindings.clear(),
            }

            Ok(SqlReturn::SUCCESS)
        })
    };
    tracing::debug!("SQLFreeStmt -> {:?}", ret);
    ret
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::OdbcError;
    use crate::test_utils::{MockBackend, with_handle};
    use odbc_sys::FreeStmtOption;

    use crate::types::SQL_DROP;
    #[test]
    fn alloc_handle_env_via_ffi() {
        let mut output: *mut c_void = std::ptr::null_mut();
        let ret = unsafe {
            sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut output,
            )
        };
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert!(!output.is_null());
        let _ = unsafe { sql_free_handle::<MockBackend>(HandleType::Env as i16, output) };
    }
    #[test]
    fn alloc_handle_invalid_type_returns_error() {
        let mut output: *mut c_void = std::ptr::null_mut();
        let ret = unsafe { sql_alloc_handle::<MockBackend>(99, std::ptr::null_mut(), &mut output) };
        // Spec HY092: invalid handle type returns SQL_ERROR (not INVALID_HANDLE)
        assert_eq!(ret, SqlReturn::ERROR);
        assert!(output.is_null());
    }
    #[test]
    fn alloc_handle_null_output_returns_error() {
        // Spec HY009: null OutputHandlePtr returns SQL_ERROR
        let ret = unsafe {
            sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SqlReturn::ERROR);
    }
    #[test]
    fn alloc_handle_env_with_non_null_input_fails() {
        // Spec: InputHandle must be SQL_NULL_HANDLE for SQL_HANDLE_ENV
        let mut output: *mut c_void = std::ptr::null_mut();
        let fake_input = 0x1234 as *mut c_void;
        let ret = unsafe {
            sql_alloc_handle::<MockBackend>(HandleType::Env as i16, fake_input, &mut output)
        };
        assert_eq!(ret, SqlReturn::ERROR);
        assert!(output.is_null());
    }
    #[test]
    fn alloc_handle_dbc_with_null_input_fails() {
        // Spec: InputHandle must be non-null for SQL_HANDLE_DBC
        let mut output: *mut c_void = std::ptr::null_mut();
        let ret = unsafe {
            sql_alloc_handle::<MockBackend>(
                HandleType::Dbc as i16,
                std::ptr::null_mut(),
                &mut output,
            )
        };
        assert_eq!(ret, SqlReturn::ERROR);
        assert!(output.is_null());
    }
    #[test]
    fn alloc_handle_dbc_via_ffi() {
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );
            let mut conn: *mut c_void = std::ptr::null_mut();
            let ret = sql_alloc_handle::<MockBackend>(HandleType::Dbc as i16, env, &mut conn);
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert!(!conn.is_null());
            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }
    #[test]
    fn free_handle_invalid_type() {
        let ret = unsafe { sql_free_handle::<MockBackend>(99, std::ptr::null_mut()) };
        assert_eq!(ret, SqlReturn::INVALID_HANDLE);
    }
    #[test]
    fn alloc_handle_desc_returns_error() {
        let mut output: *mut c_void = std::ptr::null_mut();
        let ret = unsafe {
            sql_alloc_handle::<MockBackend>(
                HandleType::Desc as i16,
                std::ptr::null_mut(),
                &mut output,
            )
        };
        // Desc is not implemented; should return ERROR and set output to null
        assert_eq!(ret, SqlReturn::ERROR);
        assert!(output.is_null());
    }
    #[test]
    fn free_null_handle_returns_invalid_handle() {
        // A valid handle type with a null pointer must return INVALID_HANDLE,
        // not a panic or UB; handle resolution checks for null before dereferencing.
        let ret =
            unsafe { sql_free_handle::<MockBackend>(HandleType::Env as i16, std::ptr::null_mut()) };
        assert_eq!(ret, SqlReturn::INVALID_HANDLE);
    }

    #[test]
    fn free_env_with_open_connection_returns_error() {
        // Spec HY010: cannot free SQL_HANDLE_ENV while child connections still exist.
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );
            let mut conn: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(HandleType::Dbc as i16, env, &mut conn);
            // Try to free env while a connection is still allocated; should fail per spec HY010.
            let ret = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
            assert_eq!(ret, SqlReturn::ERROR);
            // Spec HY010: verify the diagnostic SQLSTATE is HY010 (function_sequence_error).
            with_handle::<MockBackend, crate::handles::EnvironmentHandle<MockBackend>, _>(
                env,
                |env_handle| {
                    let rec = env_handle
                        .diagnostics
                        .get(0)
                        .expect("expected diagnostic record");
                    assert_eq!(
                        rec.sqlstate.as_str(),
                        crate::types::sql_state::FUNCTION_SEQUENCE_ERROR
                    );
                },
            );
            // Clean up properly.
            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn free_stmt_clears_diagnostics_on_entry() {
        // Spec: every function clears the handle's diagnostics at the start of
        // the call, so a stale record cannot be read back after a success.
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );
            let mut conn: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(HandleType::Dbc as i16, env, &mut conn);
            let mut stmt: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(HandleType::Stmt as i16, conn, &mut stmt);

            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                handle.diagnostics.push(&OdbcError::NotConnected);
                assert_eq!(handle.diagnostics.len(), 1, "precondition");
            });

            let ret = sql_free_stmt::<MockBackend>(stmt, FreeStmtOption::Close as u16);
            assert_eq!(ret, SqlReturn::SUCCESS);

            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                assert_eq!(
                    handle.diagnostics.len(),
                    0,
                    "stale diagnostic survived SQLFreeStmt"
                );
            });

            let _ = sql_free_handle::<MockBackend>(HandleType::Stmt as i16, stmt);
            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn free_stmt_close_option_succeeds() {
        // SQL_CLOSE should succeed even when no cursor is open.
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );
            let mut conn: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(HandleType::Dbc as i16, env, &mut conn);
            let mut stmt: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(HandleType::Stmt as i16, conn, &mut stmt);

            let ret = sql_free_stmt::<MockBackend>(stmt, FreeStmtOption::Close as u16);
            assert_eq!(ret, SqlReturn::SUCCESS);

            let _ = sql_free_handle::<MockBackend>(HandleType::Stmt as i16, stmt);
            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn free_stmt_unbind_option_succeeds() {
        // SQL_UNBIND should clear column bindings.
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );
            let mut conn: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(HandleType::Dbc as i16, env, &mut conn);
            let mut stmt: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(HandleType::Stmt as i16, conn, &mut stmt);

            let ret = sql_free_stmt::<MockBackend>(stmt, FreeStmtOption::Unbind as u16);
            assert_eq!(ret, SqlReturn::SUCCESS);

            let _ = sql_free_handle::<MockBackend>(HandleType::Stmt as i16, stmt);
            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn free_stmt_reset_params_option_succeeds() {
        // SQL_RESET_PARAMS should clear parameter bindings.
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );
            let mut conn: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(HandleType::Dbc as i16, env, &mut conn);
            let mut stmt: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(HandleType::Stmt as i16, conn, &mut stmt);

            let ret = sql_free_stmt::<MockBackend>(stmt, FreeStmtOption::ResetParams as u16);
            assert_eq!(ret, SqlReturn::SUCCESS);

            let _ = sql_free_handle::<MockBackend>(HandleType::Stmt as i16, stmt);
            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn free_stmt_drop_option_frees_statement() {
        // SQL_DROP (1) is deprecated. unixODBC maps it to SQLFreeHandle before it reaches
        // the driver, but the Windows DM passes it through directly. The driver must handle
        // it by freeing the statement (Windows DM compat).
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );
            let mut conn: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(HandleType::Dbc as i16, env, &mut conn);
            let mut stmt: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(HandleType::Stmt as i16, conn, &mut stmt);

            // SQL_DROP should free the statement handle and return SUCCESS.
            let ret = sql_free_stmt::<MockBackend>(stmt, SQL_DROP);
            assert_eq!(ret, SqlReturn::SUCCESS);
            // stmt is now freed; do not double-free.

            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn free_stmt_invalid_option_posts_hy092() {
        // The spec marks HY092 (DM) for this function, so a conforming Driver
        // Manager normally catches an unrecognised Option before the driver
        // sees it. This driver keeps the check anyway — it must do something
        // with an option it cannot parse — and the fix is only that the failure
        // becomes reportable: returning SQL_ERROR before `panic_safe` left no
        // HandleScope and no handle to post onto, so SQLGetDiagRec answered
        // SQL_NO_DATA and the application saw a failure with no SQLSTATE it
        // could branch on.
        const UNRECOGNISED_FREE_STMT_OPTION: u16 = 99;
        unsafe {
            let (env, conn, stmt) = crate::test_utils::alloc_env_conn_stmt();

            let ret = sql_free_stmt::<MockBackend>(stmt, UNRECOGNISED_FREE_STMT_OPTION);
            assert_eq!(ret, SqlReturn::ERROR);

            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                assert_eq!(handle.diagnostics.len(), 1, "SQL_ERROR must be reportable");
                let rec = handle.diagnostics.get(0).expect("record 1 exists");
                assert_eq!(rec.sqlstate.as_str(), "HY092");
            });

            crate::test_utils::cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    #[test]
    fn free_handle_clears_diagnostics_on_entry() {
        // Spec: every function clears the handle's diagnostics at the start of
        // the call. It matters here precisely when the free FAILS and the
        // handle survives — freeing a connection that still has live
        // statements — because that is exactly when an application reads them.
        // A stale record would otherwise be served as record 1, describing a
        // different call entirely.
        unsafe {
            let (env, conn, stmt) = crate::test_utils::alloc_env_conn_stmt();

            with_handle::<MockBackend, crate::handles::ConnectionHandle<MockBackend>, _>(
                conn,
                |handle| {
                    handle.diagnostics.push(&OdbcError::NotConnected);
                    assert_eq!(handle.diagnostics.len(), 1, "precondition");
                },
            );

            // Fails: `stmt` is still allocated on this connection.
            let ret = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            assert_eq!(ret, SqlReturn::ERROR);

            with_handle::<MockBackend, crate::handles::ConnectionHandle<MockBackend>, _>(
                conn,
                |handle| {
                    assert_eq!(
                        handle.diagnostics.len(),
                        1,
                        "the stale 08003 survived alongside this call's own record"
                    );
                    let rec = handle.diagnostics.get(0).expect("record 1 exists");
                    assert_eq!(
                        rec.sqlstate.as_str(),
                        "HY010",
                        "record 1 must describe this call, not the previous one"
                    );
                },
            );

            crate::test_utils::cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    #[test]
    fn alloc_handle_unimplemented_type_posts_hyc00() {
        // HYC00, not the HY000 its SQLFreeHandle counterpart uses: unlike that
        // function's table, SQLAllocHandle's *does* list HYC00, un-annotated,
        // for exactly this case ("The HandleType argument was SQL_HANDLE_DESC").
        // IM001, the other candidate, is (DM).
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );
            let mut conn: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(HandleType::Dbc as i16, env, &mut conn);

            let mut desc: *mut c_void = std::ptr::null_mut();
            let ret = sql_alloc_handle::<MockBackend>(HandleType::Desc as i16, conn, &mut desc);
            assert_eq!(ret, SqlReturn::ERROR);
            assert!(
                desc.is_null(),
                "spec: OutputHandlePtr is SQL_NULL_HANDLE on error"
            );

            with_handle::<MockBackend, crate::handles::ConnectionHandle<MockBackend>, _>(
                conn,
                |handle| {
                    assert_eq!(
                        handle.diagnostics.len(),
                        1,
                        "SQL_ERROR with no diagnostic is unreportable"
                    );
                    let rec = handle.diagnostics.get(0).expect("record 1 exists");
                    assert_eq!(rec.sqlstate.as_str(), "HYC00");
                },
            );

            crate::test_utils::cleanup_env_conn_stmt(env, conn, std::ptr::null_mut());
        }
    }

    #[test]
    fn free_handle_unimplemented_type_posts_a_diagnostic() {
        // SQL_HANDLE_DESC is a valid HandleType this driver does not implement,
        // so SQL_ERROR is right but a bare SQL_ERROR is not: with no record on
        // the queue, SQLGetDiagRec answers SQL_NO_DATA and the application has
        // a failure it can neither report nor branch on.
        //
        // HY000, not HYC00: SQLFreeHandle's diagnostics table has no HYC00 row,
        // while HY000 is listed and is the spec's catch-all. Not
        // SQL_INVALID_HANDLE either — the spec reserves that for a HandleType
        // outside the five valid values, and SQL_HANDLE_DESC is one of them.
        unsafe {
            let (env, conn, stmt) = crate::test_utils::alloc_env_conn_stmt();

            let ret = sql_free_handle::<MockBackend>(HandleType::Desc as i16, stmt);
            assert_eq!(ret, SqlReturn::ERROR);

            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                assert_eq!(
                    handle.diagnostics.len(),
                    1,
                    "SQL_ERROR with no diagnostic is unreportable"
                );
                let rec = handle.diagnostics.get(0).expect("record 1 exists");
                assert_eq!(rec.sqlstate.as_str(), "HY000");
            });

            crate::test_utils::cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    #[test]
    fn alloc_handle_clears_input_handle_diagnostics_on_entry() {
        // Spec, SQLAllocHandle Diagnostics: the SQLSTATE is read "with Handle
        // set to the value of InputHandle" — so InputHandle's queue is this
        // call's output channel, and must be cleared at entry like any other.
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );

            with_handle::<MockBackend, crate::handles::EnvironmentHandle<MockBackend>, _>(
                env,
                |handle| {
                    handle.diagnostics.push(&OdbcError::NotConnected);
                    assert_eq!(handle.diagnostics.len(), 1, "precondition");
                },
            );

            let mut conn: *mut c_void = std::ptr::null_mut();
            let ret = sql_alloc_handle::<MockBackend>(HandleType::Dbc as i16, env, &mut conn);
            assert_eq!(ret, SqlReturn::SUCCESS);

            with_handle::<MockBackend, crate::handles::EnvironmentHandle<MockBackend>, _>(
                env,
                |handle| {
                    assert_eq!(
                        handle.diagnostics.len(),
                        0,
                        "stale diagnostic survived SQLAllocHandle"
                    );
                },
            );

            crate::test_utils::cleanup_env_conn_stmt(env, conn, std::ptr::null_mut());
        }
    }
}
