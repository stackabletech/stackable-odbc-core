//! Handle lifecycle entry points: `SQLAllocHandle`, `SQLFreeHandle`, `SQLFreeStmt`.

use crate::backend::{Backend, StatementBackend};
use crate::descriptor::DescriptorRole;
use crate::errors::OdbcError;
use crate::handles::registry::{HandleKind, registry};
use crate::handles::{
    AllocFailure, AllocType, ConnectionHandle, StatementHandle, alloc_connection, alloc_descriptor,
    alloc_environment, alloc_statement, free_connection, free_descriptor, free_environment,
    free_statement, revert_statements_using,
};
use crate::panic::panic_safe;
use crate::types::{SqlReturn, free_stmt_option_from_raw, handle_type_from_raw};
use odbc_sys::{FreeStmtOption, HandleType};
use std::ffi::c_void;
/// The error every registry-exhaustion path reports.
///
/// `SQLAllocHandle`'s table lists `HY014` ("limit on the number of handles
/// exceeded") and this is the only condition in this function that means it.
fn registry_exhausted() -> OdbcError {
    OdbcError::general(
        "SQLAllocHandle: the handle registry is exhausted",
        crate::types::SqlState::limit_on_handles_exceeded(),
    )
}

/// Turn an `alloc_*` outcome into this function's return value.
///
/// The exhaustion arm becomes an `Err`, so `panic_safe` posts `HY014` to
/// `InputHandle` — the queue the spec names as this call's output channel.
/// `SQL_HANDLE_ENV` is the one arm where that posts nothing, because its
/// `InputHandle` is `SQL_NULL_HANDLE` and the handle the diagnostic would be
/// read from does not exist yet; it still fails with `SQL_ERROR`.
fn finish_alloc(outcome: Result<(), AllocFailure>) -> Result<SqlReturn, OdbcError> {
    match outcome {
        Ok(()) => Ok(SqlReturn::SUCCESS),
        Err(AllocFailure::InvalidHandle) => Ok(SqlReturn::INVALID_HANDLE),
        Err(AllocFailure::RegistryExhausted) => Err(registry_exhausted()),
    }
}

/// Generic implementation of SQLAllocHandle.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlallochandle-function>
///
/// Dispatches based on `handle_type`:
/// - `SQL_HANDLE_ENV` (1) — allocate an environment handle (`input_handle` must be null)
/// - `SQL_HANDLE_DBC` (2) — allocate a connection handle (`input_handle` must be a valid env)
/// - `SQL_HANDLE_STMT` (3) — allocate a statement handle (`input_handle` must be a valid conn)
/// - `SQL_HANDLE_DESC` (4) — allocate an explicit descriptor handle
///   (`input_handle` must be a valid connection)
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
/// - HY001: Memory allocation error. The row has two sentences and only the
///   first carries `(DM)`, the Driver Manager's own allocation failure. The
///   second is unmarked and is the driver's: "the driver was unable to allocate
///   memory for the specified handle". Core still does not return it, because
///   allocation here is an infallible `Box` — Rust's allocator aborts on OOM
///   rather than returning an error.
/// - HY009: Returns `SQL_ERROR` if `output_handle_ptr` is null. The spec
///   annotates this (DM); it is guarded defensively here.
/// - HY010: Function sequence error (driver-manager-handled; not returned here).
/// - HY013: Memory management error. The row carries no `(DM)` marker, so this is
///   the driver's answer to give: not returned, for the same reason as `HY001`.
/// - HY014: Limit on the number of handles exceeded. **Returned by this driver**
///   when the handle registry has no slot left. A token packs a slot index into
///   half a `usize`, so the ceiling is `MAX_SLOT_INDEX` live handles
///   (`handles/registry.rs`): `2^32 - 1` on a 64-bit target, but **65 535 on a
///   32-bit one**, which ODBC still very much has — Excel and Access are 32-bit
///   on Windows — so a handle-leaking application can reach it. The diagnostic
///   goes to `InputHandle`, this call's output channel: the environment for a
///   connection, the connection for a statement or an explicit descriptor.
///   `SQL_HANDLE_ENV` is the one arm that cannot carry it, because its
///   `InputHandle` is `SQL_NULL_HANDLE` and the handle the application would
///   read the diagnostic from does not exist yet; it returns `SQL_ERROR` with no
///   record, which `env_allocation_exhaustion_fails_with_no_diagnostic_to_post_to`
///   pins.
/// - HY092: Returns `SQL_ERROR` if `handle_type` is not a recognized value.
///   Sets `*output_handle` to null on error (unless `output_handle` itself is
///   null). The spec annotates this (DM); it is guarded defensively here.
/// - HY117: Connection suspended due to unknown transaction state
///   (driver-manager-handled; not returned here).
/// - HYC00: Optional feature not implemented. The row carries no `(DM)` marker, so
///   it is the driver's to return, and it is. Returned, with this SQLSTATE
///   posted, for `SQL_HANDLE_DBC_INFO_TOKEN` allocation. This is the only
///   un-annotated code in this function's table covering an unimplemented handle
///   type; `IM001`, the alternative, is the Driver Manager's. The spec's own description of the
///   row names `SQL_HANDLE_DESC`, which core no longer refuses. Note
///   `SQLFreeHandle` answers `HY000` for an unimplemented type, because its
///   table has no `HYC00` row at all — the asymmetry is what the two tables say.
/// - HYT01: Connection timeout expired (not returned here; allocation performs
///   no network I/O).
/// - IM001: Driver does not support this function (driver-manager-handled; not
///   returned here).
///
/// Handle-specific rules:
/// - For Env: `input_handle` must be `SQL_NULL_HANDLE` (null).
/// - For Dbc/Stmt: `input_handle` must be non-null and a valid parent handle.
/// - For Desc: `input_handle` must be a valid **connection**. An explicit
///   descriptor belongs to a connection, not a statement, and joins that
///   connection's lock group — the one every statement on it already shares, so
///   a descriptor an application later associates with several statements adds no
///   lock. A token that does not name a live connection is
///   `SQL_INVALID_HANDLE`; `08003` ("connection not open") is (DM), so core does
///   not check that the connection is open.
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
    // holds the *parent's* group. For SQL_HANDLE_STMT and SQL_HANDLE_DESC that is
    // also the group the child joins. For SQL_HANDLE_DBC it is not: a connection
    // starts a fresh `GroupLock` of its own, which is what makes it the unit every
    // statement and descriptor under it shares. Either way nothing nests, so there
    // is no ordering to get wrong here — the crate's one nested-lock site is
    // SQLEndTran(SQL_HANDLE_ENV). The output pointer is set to SQL_NULL_HANDLE up front so every
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
                    finish_alloc(alloc_environment::<B>(output_handle_ptr))?
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
                    finish_alloc(alloc_connection::<B>(input_handle, output_handle_ptr))?
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
                    finish_alloc(alloc_statement::<B>(
                        input_handle,
                        output_handle_ptr,
                        inherited_metadata_id,
                    ))?
                }
                HandleType::Desc => {
                    // An explicit descriptor belongs to a **connection**, not a
                    // statement, and joins that connection's lock group — which
                    // every statement on it already shares, so a descriptor an
                    // application later associates with several statements adds
                    // no lock and no ordering rule.
                    //
                    // `08003` ("connection not open") is (DM), so core does not
                    // check that the connection is open — only that `input_handle`
                    // really names one, which `group_of_kind` answers without
                    // dereferencing it. The `*output_handle_ptr =
                    // SQL_NULL_HANDLE` write above already ran, so the spec's
                    // "set to SQL_NULL_HANDLE on error" holds on both exits here.
                    let Some(group) = registry().group_of_kind(input_handle, HandleKind::Dbc)
                    else {
                        return Ok(SqlReturn::INVALID_HANDLE);
                    };
                    // Role `App`: the spec says "it is not known whether an
                    // explicitly allocated application descriptor is an APD or ARD
                    // until execute time".
                    let Some(token) = alloc_descriptor(
                        DescriptorRole::App,
                        AllocType::User,
                        &group,
                        input_handle,
                    ) else {
                        return Err(registry_exhausted());
                    };
                    tracing::debug!(
                        "SQLAllocHandle: allocated explicit descriptor {:?} on connection {:?}",
                        token,
                        input_handle
                    );
                    std::ptr::write_unaligned(output_handle_ptr, token);
                    SqlReturn::SUCCESS
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
/// - `SQL_HANDLE_DESC` (4) — free an explicit descriptor handle, i.e. one this
///   driver allocated against a **connection**. A statement's own descriptor is
///   refused with `HY000`; see that SQLSTATE below
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
/// - HY000: General error. The row carries no `(DM)` marker, so it is the driver's
///   to return, and it is. Returns `SQL_ERROR` with this SQLSTATE in two cases. First, when
///   `handle_type` is `SQL_HANDLE_DBC_INFO_TOKEN` — a valid handle type this
///   driver does not implement. The spec's table for this function lists no
///   `HYC00`, so the catch-all is the correct code even though `SQLAllocHandle`
///   answers `HYC00` for the same condition. Second, when `handle` names one of
///   the four descriptors allocated implicitly with a statement: this function
///   allocated only the descriptors whose parent is a connection, and retiring a
///   statement's own slot would leave that statement pointing at nothing. The
///   refusal is expressed as ownership rather than as a spec check, and borrows no
///   Driver-Manager-only code to say so — `HY017` is the spec's name for the
///   condition, and the spec annotates it for the Driver Manager, so core does not
///   return it (see below). Under a real Driver Manager that branch never fires,
///   since the Driver Manager blocks the call first; its observers
///   are core's own tests and an embedder linking core directly, and for those a
///   general error naming the condition is as useful as `HY017`.
/// - Returns `SQL_INVALID_HANDLE` for `SQL_HANDLE_DESC` with a token that is not
///   a live descriptor at all, which is a different question from ownership.
/// - HY001: Memory allocation error. The row carries no `(DM)` marker here, unlike
///   `SQLAllocHandle`'s: not returned, because Rust's allocator aborts on OOM rather
///   than returning an error.
/// - HY010: Function sequence error — every clause of this row is `(DM)`. Two of them are
///   guarded defensively here anyway, because they are load-bearing for memory safety
///   rather than for the spec: freeing an environment that still has connections, or a
///   connection that is still open or still has children registered under it, would leave
///   live handles pointing at a retired parent. The children counted are every handle
///   registered under that connection — statements and any explicitly allocated
///   descriptors — not statements alone. The remaining clauses (async in progress,
///   data-at-execution pending) are driver-manager-handled; not returned here.
/// - HY013: Memory management error. The row carries no `(DM)` marker: not returned, for
///   the same reason as `HY001`.
/// - HY017: Invalid use of an automatically allocated descriptor handle
///   (driver-manager-handled; not returned here).
/// - HY117: Connection suspended due to unknown transaction state
///   (driver-manager-handled; not returned here).
/// - HYT01: Connection timeout expired. The row carries no `(DM)` marker: not returned,
///   because freeing a handle performs no network I/O.
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
                HandleType::Desc => {
                    // Routed by ownership, not by inspecting the alloc type: this
                    // function allocated the descriptors whose parent is a
                    // connection, and only those. A statement's own descriptor
                    // reaching here is a call core cannot perform — retiring that
                    // slot would leave the owning statement pointing at nothing.
                    //
                    // HY000, not HY017: the spec's name for this condition is
                    // (DM), and core returns no (DM) code. HY000 is the catch-all
                    // this same function already answers for an unimplemented
                    // handle type, whose table lists no HYC00. Under a real Driver
                    // Manager this branch never fires.
                    match registry().parent_kind_of(handle, HandleKind::Desc) {
                        Some(HandleKind::Dbc) => {
                            // Spec: "all statement handles to which the freed
                            // descriptor applied automatically revert to the
                            // descriptors implicitly allocated for them." Without
                            // this, a statement is left pointing at a retired slot
                            // and every later call through it fails.
                            if let Some(conn) = registry().parent_of(handle, HandleKind::Desc) {
                                revert_statements_using::<B>(scope, conn, handle);
                            }
                            free_descriptor(handle);
                            SqlReturn::SUCCESS
                        }
                        Some(_) => {
                            return Err(OdbcError::general(
                                "SQLFreeHandle: this descriptor was allocated implicitly with a \
                                 statement and is freed with it",
                                crate::types::SqlState::general_error(),
                            ));
                        }
                        None => SqlReturn::INVALID_HANDLE,
                    }
                }
                HandleType::DbcInfoToken => {
                    // DBC_INFO_TOKEN: not implemented (matching alloc). Deferred.
                    //
                    // HY000, not HYC00: SQLFreeHandle's diagnostics table has no
                    // HYC00 row, while HY000 is listed and is the spec's catch-all
                    // for a condition with no specific SQLSTATE. (SQLAllocHandle's
                    // table *does* list HYC00 un-annotated, which is why its
                    // equivalent arm differs — the asymmetry is what the two
                    // tables say, not an oversight.) Not SQL_INVALID_HANDLE
                    // either: the spec reserves that for a HandleType outside the
                    // five valid values, and this one is valid.
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
/// - 01000: General warning. The row carries no `(DM)` marker: not returned, because core
///   emits no driver-specific informational message from this function.
/// - HY000: General error. The row carries no `(DM)` marker: not returned, because every
///   failure this function can have already has a more specific state.
/// - HY001: Memory allocation error. The row carries no `(DM)` marker: not returned,
///   because Rust's allocator aborts on OOM rather than returning an error.
/// - HY010: Function sequence error (async execution in progress, data-at-execution pending,
///   etc.) — driver-manager-handled; not returned here.
/// - HY013: Memory management error. The row carries no `(DM)` marker: not returned, for
///   the same reason as `HY001`.
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
/// - HYT01: Connection timeout expired. The row carries no `(DM)` marker: not returned,
///   because core implements no connection timeout, so no deadline exists to expire.
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
            // Copied out of the statement now, so the `SQL_UNBIND` arm can reach
            // the descriptor with one registry lookup rather than resolving the
            // statement a second time through `desc_of`.
            let ard_token = stmt.descriptor_token(DescriptorRole::Ard);

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
                // Discard the result set so the handle is ready for a new
                // statement, telling the backend first — the spec makes this
                // option equivalent to `SQLCloseCursor` bar the `24000`, so it
                // owes `StatementBackend::close_cursor` the same call. See
                // `sql_close_cursor` for why the discard happens even on
                // failure.
                //
                // Gated on `cursor_open`, not on `statement.is_some()`: a
                // prepared-but-unexecuted statement (S2/S3) holds a backend
                // statement and no cursor, and the spec says this option "has no
                // effect for the application" when no cursor is open. Asking a
                // backend to close a cursor that was never opened is exactly
                // what that sentence rules out. `sql_close_cursor` needs no such
                // gate; its `24000` guard has already established one is open.
                FreeStmtOption::Close => {
                    let close_err = if stmt.cursor_open {
                        stmt.statement
                            .as_mut()
                            .and_then(|statement| statement.close_cursor().err())
                    } else {
                        None
                    };
                    stmt.discard_result_set();
                    if let Some(e) = close_err {
                        return Err(e);
                    }
                }
                FreeStmtOption::Unbind => scope.descriptor(ard_token)?.records.clear(),
                FreeStmtOption::ResetParams => {
                    scope.clear_param_records::<B>(statement_handle)?;
                }
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
    use crate::test_utils::{MockBackend, MockFailingCloseBackend, with_handle};
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
    /// A descriptor is allocated against a **connection**, so a null input
    /// handle names nothing to allocate it on.
    #[test]
    fn alloc_handle_desc_on_a_null_connection_is_refused() {
        let mut output: *mut c_void = std::ptr::null_mut();
        let ret = unsafe {
            sql_alloc_handle::<MockBackend>(
                HandleType::Desc as i16,
                std::ptr::null_mut(),
                &mut output,
            )
        };
        assert_eq!(ret, SqlReturn::INVALID_HANDLE);
        assert!(output.is_null());
    }

    /// `SQLAllocHandle(SQL_HANDLE_DESC)` yields a usable descriptor handle whose
    /// `SQL_DESC_ALLOC_TYPE` says the application allocated it.
    #[test]
    fn alloc_handle_allocates_an_explicit_descriptor() {
        unsafe {
            let (env, conn, stmt) = crate::test_utils::alloc_env_conn_stmt();
            let mut desc: *mut c_void = std::ptr::null_mut();
            let ret = sql_alloc_handle::<MockBackend>(HandleType::Desc as i16, conn, &mut desc);
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert!(!desc.is_null());

            let mut value: isize = 0;
            let ret = crate::ffi::desc::sql_get_desc_field_w::<MockBackend>(
                desc,
                0,
                odbc_sys::Desc::AllocType as i16,
                std::ptr::from_mut(&mut value).cast(),
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(value, crate::types::SQL_DESC_ALLOC_USER);

            // And a statement's own reads the other value, so the field really
            // follows the allocation rather than being a constant either way.
            let mut implicit: *mut c_void = std::ptr::null_mut();
            assert_eq!(
                crate::ffi::stmt_attr::sql_get_stmt_attr_w::<MockBackend>(
                    stmt,
                    odbc_sys::StatementAttribute::AppRowDesc as i32,
                    std::ptr::from_mut(&mut implicit).cast(),
                    0,
                    std::ptr::null_mut(),
                ),
                SqlReturn::SUCCESS
            );
            let mut auto_value: isize = 0;
            assert_eq!(
                crate::ffi::desc::sql_get_desc_field_w::<MockBackend>(
                    implicit,
                    0,
                    odbc_sys::Desc::AllocType as i16,
                    std::ptr::from_mut(&mut auto_value).cast(),
                    0,
                    std::ptr::null_mut(),
                ),
                SqlReturn::SUCCESS
            );
            assert_eq!(auto_value, crate::types::SQL_DESC_ALLOC_AUTO);

            assert_eq!(
                sql_free_handle::<MockBackend>(HandleType::Desc as i16, desc),
                SqlReturn::SUCCESS
            );
            crate::test_utils::cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// A statement's own descriptor is not this function's to free.
    ///
    /// `HY017` is the spec's name for the condition and is `(DM)`, so core
    /// answers `HY000` instead — the same code this function already returns for
    /// an unimplemented handle type, whose table lists no `HYC00` either. The
    /// statement must be untouched afterwards.
    #[test]
    fn free_handle_refuses_a_statements_own_descriptor() {
        unsafe {
            let (env, conn, stmt) = crate::test_utils::alloc_env_conn_stmt();
            let mut ard: *mut c_void = std::ptr::null_mut();
            assert_eq!(
                crate::ffi::stmt_attr::sql_get_stmt_attr_w::<MockBackend>(
                    stmt,
                    odbc_sys::StatementAttribute::AppRowDesc as i32,
                    std::ptr::from_mut(&mut ard).cast(),
                    0,
                    std::ptr::null_mut(),
                ),
                SqlReturn::SUCCESS
            );

            assert_eq!(
                sql_free_handle::<MockBackend>(HandleType::Desc as i16, ard),
                SqlReturn::ERROR
            );
            assert_eq!(
                first_sqlstate_of(ard),
                crate::types::sql_state::GENERAL_ERROR,
                "the refusal must be HY000, not a (DM) code"
            );

            // Still usable: the refusal must not have retired the slot.
            let mut buf = [0u8; 4];
            assert_eq!(
                crate::ffi::bind::sql_bind_col::<MockBackend>(
                    stmt,
                    1,
                    odbc_sys::CDataType::SLong as i16,
                    buf.as_mut_ptr().cast(),
                    4,
                    std::ptr::null_mut(),
                ),
                SqlReturn::SUCCESS
            );
            crate::test_utils::cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// `SQLDisconnect` "drops any statements or descriptors open on the
    /// connection", so an explicit descriptor left behind is freed with it —
    /// which Miri's leak check is what actually enforces.
    #[test]
    fn disconnect_frees_the_connections_explicit_descriptors() {
        unsafe {
            let (env, conn, stmt) =
                crate::test_utils::alloc_connected_env_conn_stmt::<MockBackend>();
            let mut desc: *mut c_void = std::ptr::null_mut();
            assert_eq!(
                sql_alloc_handle::<MockBackend>(HandleType::Desc as i16, conn, &mut desc),
                SqlReturn::SUCCESS
            );

            assert_eq!(
                crate::ffi::connect::sql_disconnect::<MockBackend>(conn),
                SqlReturn::SUCCESS
            );
            assert!(
                crate::handles::registry::registry()
                    .group_of(desc)
                    .is_none(),
                "the explicit descriptor's slot survived SQLDisconnect"
            );

            let _ = stmt;
            crate::test_utils::cleanup_env_conn_stmt(env, conn, std::ptr::null_mut());
        }
    }

    /// The first SQLSTATE on a handle's diagnostic queue, as `SQLGetDiagRec`
    /// would report it, or `None` if the queue is empty.
    fn first_sqlstate_of_kind(kind: HandleType, handle: *mut c_void) -> Option<String> {
        let mut state = [0u16; 6];
        let mut native: i32 = 0;
        let mut msg = [0u16; 256];
        let mut msg_len: i16 = 0;
        let ret = unsafe {
            crate::ffi::diag::sql_get_diag_rec_w::<MockBackend>(
                kind as i16,
                handle,
                1,
                state.as_mut_ptr(),
                &mut native,
                msg.as_mut_ptr(),
                256,
                &mut msg_len,
            )
        };
        (ret == SqlReturn::SUCCESS).then(|| String::from_utf16_lossy(&state[..5]))
    }

    /// Registry exhaustion is `SQLAllocHandle`'s `HY014`, and the diagnostic
    /// goes to `InputHandle` — the environment, for a connection.
    #[test]
    fn dbc_allocation_exhaustion_posts_hy014_on_the_environment() {
        let mut env: *mut c_void = std::ptr::null_mut();
        let ret = unsafe {
            sql_alloc_handle::<MockBackend>(HandleType::Env as i16, std::ptr::null_mut(), &mut env)
        };
        assert_eq!(ret, SqlReturn::SUCCESS);

        let mut conn: *mut c_void = std::ptr::null_mut();
        crate::handles::registry::fail_next_registration::arm();
        let ret =
            unsafe { sql_alloc_handle::<MockBackend>(HandleType::Dbc as i16, env, &mut conn) };
        crate::handles::registry::fail_next_registration::disarm();

        assert_eq!(
            ret,
            SqlReturn::ERROR,
            "an exhausted registry must fail the alloc"
        );
        assert!(
            conn.is_null(),
            "OutputHandlePtr must be SQL_NULL_HANDLE on error"
        );
        assert_eq!(
            first_sqlstate_of_kind(HandleType::Env, env).as_deref(),
            Some("HY014"),
            "the environment must carry HY014, the listed code for this condition"
        );

        unsafe {
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    /// The same, one level down: the diagnostic goes to the connection.
    #[test]
    fn stmt_allocation_exhaustion_posts_hy014_on_the_connection() {
        let mut env: *mut c_void = std::ptr::null_mut();
        unsafe {
            let _ = sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );
        }
        let mut conn: *mut c_void = std::ptr::null_mut();
        unsafe {
            let _ = sql_alloc_handle::<MockBackend>(HandleType::Dbc as i16, env, &mut conn);
        }

        let mut stmt: *mut c_void = std::ptr::null_mut();
        crate::handles::registry::fail_next_registration::arm();
        let ret =
            unsafe { sql_alloc_handle::<MockBackend>(HandleType::Stmt as i16, conn, &mut stmt) };
        crate::handles::registry::fail_next_registration::disarm();

        assert_eq!(ret, SqlReturn::ERROR);
        assert!(stmt.is_null());
        assert_eq!(
            first_sqlstate_of_kind(HandleType::Dbc, conn).as_deref(),
            Some("HY014")
        );

        unsafe {
            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    /// An explicit descriptor answered `HY000` for the same condition. It is
    /// registry exhaustion like the other three, so it answers `HY014` too.
    #[test]
    fn explicit_descriptor_exhaustion_posts_hy014_on_the_connection() {
        let mut env: *mut c_void = std::ptr::null_mut();
        unsafe {
            let _ = sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );
        }
        let mut conn: *mut c_void = std::ptr::null_mut();
        unsafe {
            let _ = sql_alloc_handle::<MockBackend>(HandleType::Dbc as i16, env, &mut conn);
        }

        let mut desc: *mut c_void = std::ptr::null_mut();
        crate::handles::registry::fail_next_registration::arm();
        let ret =
            unsafe { sql_alloc_handle::<MockBackend>(HandleType::Desc as i16, conn, &mut desc) };
        crate::handles::registry::fail_next_registration::disarm();

        assert_eq!(ret, SqlReturn::ERROR);
        assert!(desc.is_null());
        assert_eq!(
            first_sqlstate_of_kind(HandleType::Dbc, conn).as_deref(),
            Some("HY014"),
            "this path posted HY000 before; HY014 is the listed code"
        );

        unsafe {
            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    /// The environment is the one arm that cannot carry the diagnostic, and
    /// this pins that rather than leaving it to a comment. `InputHandle` is
    /// `SQL_NULL_HANDLE` for an environment allocation, so there is no queue
    /// to post to — the spec's own `Handle` for this call's diagnostic does
    /// not exist yet. It still fails.
    #[test]
    fn env_allocation_exhaustion_fails_with_no_diagnostic_to_post_to() {
        let mut env: *mut c_void = std::ptr::null_mut();
        crate::handles::registry::fail_next_registration::arm();
        let ret = unsafe {
            sql_alloc_handle::<MockBackend>(HandleType::Env as i16, std::ptr::null_mut(), &mut env)
        };
        crate::handles::registry::fail_next_registration::disarm();

        assert_eq!(ret, SqlReturn::ERROR);
        assert!(
            env.is_null(),
            "OutputHandlePtr must be SQL_NULL_HANDLE on error"
        );
    }

    /// The first SQLSTATE on a handle's diagnostic queue, as `SQLGetDiagRec`
    /// would report it.
    fn first_sqlstate_of(handle: *mut c_void) -> String {
        let mut state = [0u16; 6];
        let mut native: i32 = 0;
        let mut msg = [0u16; 256];
        let mut msg_len: i16 = 0;
        let ret = unsafe {
            crate::ffi::diag::sql_get_diag_rec_w::<MockBackend>(
                HandleType::Desc as i16,
                handle,
                1,
                state.as_mut_ptr(),
                &mut native,
                msg.as_mut_ptr(),
                256,
                &mut msg_len,
            )
        };
        assert_eq!(ret, SqlReturn::SUCCESS, "no diagnostic record was posted");
        String::from_utf16_lossy(&state[..5])
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
        // for an unimplemented handle type. IM001, the other candidate, is (DM).
        //
        // SQL_HANDLE_DBC_INFO_TOKEN is the only type left on this arm now that
        // SQL_HANDLE_DESC is implemented; the spec's own wording for the row
        // names SQL_HANDLE_DESC, which core no longer refuses.
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );
            let mut conn: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(HandleType::Dbc as i16, env, &mut conn);

            let mut token: *mut c_void = std::ptr::null_mut();
            let ret =
                sql_alloc_handle::<MockBackend>(HandleType::DbcInfoToken as i16, conn, &mut token);
            assert_eq!(ret, SqlReturn::ERROR);
            assert!(
                token.is_null(),
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
        // SQL_HANDLE_DBC_INFO_TOKEN is a valid HandleType this driver does not
        // implement, so SQL_ERROR is right but a bare SQL_ERROR is not: with no
        // record on the queue, SQLGetDiagRec answers SQL_NO_DATA and the
        // application has a failure it can neither report nor branch on.
        //
        // HY000, not HYC00: SQLFreeHandle's diagnostics table has no HYC00 row,
        // while HY000 is listed and is the spec's catch-all. Not
        // SQL_INVALID_HANDLE either — the spec reserves that for a HandleType
        // outside the five valid values, and this is one of them.
        unsafe {
            let (env, conn, stmt) = crate::test_utils::alloc_env_conn_stmt();

            let ret = sql_free_handle::<MockBackend>(HandleType::DbcInfoToken as i16, stmt);
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

    /// `SQLFreeHandle(SQL_HANDLE_DESC)` on a token that is not a descriptor at
    /// all is `SQL_INVALID_HANDLE`, not the ownership refusal: there is no
    /// descriptor here to decide the ownership of.
    #[test]
    fn free_handle_desc_refuses_a_statement_token_as_an_invalid_handle() {
        unsafe {
            let (env, conn, stmt) = crate::test_utils::alloc_env_conn_stmt();
            let ret = sql_free_handle::<MockBackend>(HandleType::Desc as i16, stmt);
            assert_eq!(ret, SqlReturn::INVALID_HANDLE);
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

    // -----------------------------------------------------------------------
    // SQLFreeStmt(SQL_CLOSE) is equivalent to SQLCloseCursor
    // -----------------------------------------------------------------------

    /// The spec makes the two the same call bar the `24000`: "Calling
    /// **SQLFreeStmt** with the SQL_CLOSE option is equivalent to calling
    /// **SQLCloseCursor**, except that **SQLFreeStmt** with SQL_CLOSE does not
    /// affect the application if no cursor is open." So whatever `SQLCloseCursor`
    /// does about `StatementBackend::close_cursor`, this must do too.
    #[test]
    fn free_stmt_close_calls_the_backend_and_reports_its_failure() {
        unsafe {
            let (env, conn, stmt) =
                crate::test_utils::alloc_connected_env_conn_stmt::<MockFailingCloseBackend>();
            let sql: Vec<u16> = "SELECT 1".encode_utf16().collect();
            assert_eq!(
                crate::ffi::execute::sql_exec_direct_w::<MockFailingCloseBackend>(
                    stmt,
                    sql.as_ptr(),
                    i32::try_from(sql.len()).expect("SQL fits in i32"),
                ),
                SqlReturn::SUCCESS,
                "precondition: a cursor is open",
            );

            assert_eq!(
                sql_free_stmt::<MockFailingCloseBackend>(stmt, FreeStmtOption::Close as u16),
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
            assert_eq!(state, "08S01", "the backend's own SQLSTATE");

            crate::test_utils::cleanup_connected_env_conn_stmt::<MockFailingCloseBackend>(
                env, conn, stmt,
            );
        }
    }

    /// The `cursor_open` gate, which nothing else pins.
    ///
    /// A prepared-but-unexecuted statement (ODBC state S2/S3) holds a backend
    /// statement but has no cursor, and the spec says SQL_CLOSE "has no effect
    /// for the application" when no cursor is open. Calling `close_cursor` there
    /// would drive the backend to tear down a cursor that was never opened —
    /// against this mock, turning a call the spec says succeeds into an `08S01`.
    ///
    /// `SQLCloseCursor` needs no such gate: its `24000` guard has already
    /// established that a cursor is open by the time it reaches the backend.
    #[test]
    fn free_stmt_close_does_not_call_the_backend_without_an_open_cursor() {
        unsafe {
            let (env, conn, stmt) =
                crate::test_utils::alloc_connected_env_conn_stmt::<MockFailingCloseBackend>();
            let sql: Vec<u16> = "SELECT 1".encode_utf16().collect();
            assert_eq!(
                crate::ffi::execute::sql_prepare_w::<MockFailingCloseBackend>(
                    stmt,
                    sql.as_ptr(),
                    i32::try_from(sql.len()).expect("SQL fits in i32"),
                ),
                SqlReturn::SUCCESS,
                "precondition: prepared, so a backend statement exists but no cursor does",
            );

            assert_eq!(
                sql_free_stmt::<MockFailingCloseBackend>(stmt, FreeStmtOption::Close as u16),
                SqlReturn::SUCCESS,
                "with no cursor open this option has no effect, so the backend is never asked",
            );

            crate::test_utils::cleanup_connected_env_conn_stmt::<MockFailingCloseBackend>(
                env, conn, stmt,
            );
        }
    }
}
