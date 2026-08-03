//! Connection entry points: `SQLDriverConnectW`, `SQLBrowseConnectW`,
//! `SQLConnectW`, `SQLDisconnect`, `SQLNativeSqlW`.

use std::ffi::c_void;
use std::sync::Arc;

use crate::backend::Backend;
use crate::errors::{IntoOdbc, OdbcError};
use crate::handles::ConnectionHandle;
use crate::panic::panic_safe;
use crate::types::{
    ConnectParams, DriverConnectOption, SQL_NTS, SqlReturn, SqlState,
    driver_connect_option_from_raw,
};
use crate::utf16::{utf16_to_string, utf16_to_string_named, write_utf16};

/// Generic implementation of SQLDriverConnectW.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqldriverconnect-function>
///
/// Parses the UTF-16 connection string, calls `B::connect`, and stores the
/// resulting connection in the handle. If `out_connection_string` is non-null
/// the input connection string is echoed back (truncated if necessary).
///
/// # Parameters
///
/// - `connection_handle`: Connection handle.
/// - `_window_handle`: Parent window handle for dialog boxes; ignored (we never display dialogs).
/// - `in_connection_string`: Full or partial connection string (`Key=Value;...`).
/// - `string_length1`: Length of `in_connection_string` in characters, or `SQL_NTS` (-3).
/// - `out_connection_string`: Buffer to receive the completed connection string; may be null.
/// - `buffer_length`: Size of `out_connection_string` in characters.
/// - `string_length2_ptr`: Receives the length of the completed connection string; may be null.
/// - `driver_completion`: `SQL_DRIVER_*` completion flag. Core never puts up a dialog of its
///   own, so this argument decides exactly one thing: whether the backend is handed the
///   driver's [`Backend::prompter`]. Under
///   `SQL_DRIVER_NOPROMPT` it is not, and a driver needing interactive authentication has
///   nothing to call. See `prompter_for` for the unrecognised-value and no-argument cases.
///
/// # Spec compliance
///
/// - 01000: General warning — not returned here (no driver-specific info messages produced).
/// - 01004: String data, right-truncated — NOT returned here. When the echoed output connection
///   string does not fit the caller's buffer, this function still returns plain `SUCCESS` rather
///   than `SUCCESS_WITH_INFO`, because signalling truncation here drives a Windows Driver Manager
///   crash path; see inline comment.
/// - 01S00: Invalid connection string attribute but driver connected anyway — not returned here
///   (unknown attributes are silently ignored by `ConnectParams::parse`).
/// - 01S02: Option value changed — not returned here (no connection attribute substitution is
///   performed during connect).
/// - 01S08: Error saving file DSN — not returned here, and the row carries no `(DM)`
///   marker. The Comments section assigns the writing to the Driver Manager: a `SAVEFILE`
///   keyword makes the DM create the file DSN from the connection string this function
///   returns, so core never opens it and has no write to fail.
/// - 01S09: Invalid keyword (SAVEFILE without DRIVER/FILEDSN) — (driver-manager-handled; not returned here).
/// - 08001: Client unable to establish connection — returned by the backend via `B::connect`.
/// - 08002: Connection name in use — the spec annotates this `(DM)`; it is guarded
///   defensively here. The check is `handle.connection.is_some()`, kept because core is
///   also linked without a Driver Manager in front of it, and because connecting twice
///   through one handle would leak the first connection.
/// - 08004: Server rejected the connection — may be returned by the backend via `B::connect`.
/// - 08S01: Communication link failure — not returned here; a failure while
///   establishing the connection is 08001. 08S01 applies once the connection
///   exists (see AGENTS.md, "08001 versus 08S01").
/// - 28000: Invalid authorization specification — may be returned by the backend via `B::connect`.
/// - 3D000: Invalid catalog name — **absent from this function's diagnostics table**, yet
///   returned by this driver. A `SQL_ATTR_CURRENT_CATALOG` set before connecting is applied here,
///   and [`crate::backend::Backend::set_current_catalog`] owes `3D000` for a catalog the data
///   source does not have. The state is propagated rather than degraded: telling the application
///   its connection failed for an unrelated reason would be worse than naming a state this table
///   omits. The spec notes interoperable applications set this attribute *before* connecting, so
///   this is the path that matters most.
/// - HY000: General error — **returned by this driver** for any backend error with no
///   specific SQLSTATE, which is the clause this row states without a `(DM)` marker. The
///   page prints two further `HY000` rows that *are* marked, both about a `FILEDSN`
///   keyword naming a `.dsn` file the Driver Manager could not find or read; core never
///   opens one.
/// - HY001: Memory allocation failure — not returned here (Rust panics on alloc failure).
/// - HY008: Operation canceled; not returned here. Cancelling a connection-level call needs
///   `SQLCancelHandle` on a connection handle, which this driver does not export, so no cancel
///   token exists for this call to observe — `SQLCancel` takes a statement handle and cannot
///   reach one. The asynchronous clause is likewise inapplicable: core never returns
///   `SQL_STILL_EXECUTING`.
/// - HY009: Invalid use of null pointer — **absent from this function's diagnostics table**,
///   yet returned by this driver when `in_connection_string` is null. A null string
///   argument reaches `utf16_to_string`, which reports `HY009` rather than reading through
///   it. The check is a soundness guard, not a spec check, and is kept for the reason
///   `SQLAllocHandle`'s `(DM)` guards are kept: core is also linked directly, with no
///   Driver Manager in front of it.
/// - HY010: Function sequence error (async in progress) — (driver-manager-handled; not returned here).
/// - HY013: Memory management error — not returned here (Rust panics on alloc failure).
/// - HY090: Invalid string or buffer length — returned when `string_length1 < 0 && != SQL_NTS`,
///   or when `buffer_length < 0`. Note: spec marks the row's own clauses as `(DM)`, but they
///   are checked here as a defence-in-depth guard when called outside a full Driver Manager
///   stack. **Also returned** for a condition the row does not state: `InConnectionString`
///   passed as `SQL_NTS` with no null terminator within `MAX_NTS_SCAN` (1 048 576) code units,
///   which is a length the driver cannot determine. It is this function's only `SQL_NTS`
///   argument, so that is the whole set. A truncated connection string is quieter than a
///   truncated statement — `ConnectParams` parses whatever it is handed — so the connect
///   proceeded against a *different data source* than the application named. See
///   `driver_connect_refuses_an_nts_connection_string_that_runs_to_the_scan_cap`.
/// - HY092: Invalid attribute/option identifier — (driver-manager-handled; not returned here).
/// - HY110: Invalid driver completion — (driver-manager-handled; not returned here). Both
///   clauses of this row carry `(DM)`, so an unrecognised *DriverCompletion* is accepted
///   here and treated as permitting a prompt rather than rejected; see `prompter_for`.
/// - IM007: No data source or driver specified, dialog prohibited — not returned here. Core
///   puts up no dialog of its own, and a backend that cannot connect without one reports its
///   own failure through `B::connect`.
/// - IM008: Dialog failed — not returned here. Core never calls
///   [`Prompter::present_url`](crate::prompt::Prompter::present_url); a driver whose prompt
///   fails maps that itself, inside `B::connect`.
/// - HYC00: Optional feature not implemented — **returned by this driver**, but only from a
///   connection attribute set before the connect. Core applies those here (see
///   `apply_pending_connect_attrs`), and an unimplemented hook — most often the defaulted
///   [`crate::backend::Backend::set_current_catalog`] — reports `HYC00`, which fails the connect
///   and tears it down. Nothing in the connect path itself produces it.
/// - HYT00: Login timeout expired — not returned here (login timeout not enforced).
/// - HYT01: Connection timeout expired — not returned here (connection timeout not enforced).
/// - S1118: Driver does not support asynchronous notification — not returned here, and the
///   row carries no `(DM)` marker. Core implements no connection-level asynchronous
///   notification, so an application has no way to ask for it and nothing to be refused.
/// - IM001–IM006: Driver Manager internal codes — (driver-manager-handled; not returned
///   here). Core resolves a DSN through `merge_dsn_params` and never loads, translates or
///   negotiates with another driver, so none of the block's conditions can arise in it.
/// - IM009: Unable to load translation DLL — not returned here, and the row carries no
///   `(DM)` marker. Core loads no translation DLL: it implements no
///   `SQL_ATTR_TRANSLATE_LIB`, so there is nothing to fail to load.
/// - IM010–IM012: Driver Manager internal codes — (driver-manager-handled; not returned
///   here).
/// - IM014: Invalid name of File DSN — (driver-manager-handled; not returned here).
/// - IM015: Driver's SQLDriverConnect on SQL_HANDLE_DBC_INFO_TOKEN failed — not returned
///   here, and the row carries no `(DM)` marker. Core exports no
///   `SQL_HANDLE_DBC_INFO_TOKEN` entry point, so the call this row describes cannot be
///   made against it.
/// - IM017: Polling disabled; not returned here (the asynchronous notification model is not
///   supported — not DM-annotated in the spec).
/// - IM018: SQLCompleteAsync not called; not returned here (the asynchronous notification
///   model is not supported — not DM-annotated in the spec).
///
/// # Safety
///
/// `connection_handle` must point to a valid `ConnectionHandle<B>`.
/// `in_connection_string` must be a valid UTF-16 string of the given length
/// (or null-terminated if `string_length1` is `SQL_NTS`).
#[allow(clippy::too_many_arguments)]
pub unsafe fn sql_driver_connect_w<B: Backend>(
    connection_handle: *mut c_void,
    _window_handle: *mut c_void,
    in_connection_string: *const u16,
    string_length1: i16,
    out_connection_string: *mut u16,
    buffer_length: i16,
    string_length2_ptr: *mut i16,
    driver_completion: u16,
) -> SqlReturn {
    tracing::trace!(
        "SQLDriverConnectW(conn={:?}, in_len={}, raw_completion={})",
        connection_handle,
        string_length1,
        driver_completion
    );
    let completion = driver_connect_option_from_raw(driver_completion);
    if completion.is_none() {
        // HY110 is `(DM)` on both clauses, so this is not core's to reject.
        tracing::warn!(
            "SQLDriverConnectW: unrecognised DriverCompletion {}; accepting it and permitting \
             a prompt (HY110 is Driver-Manager-owned)",
            driver_completion
        );
    }
    tracing::debug!(
        "SQLDriverConnectW(conn={:?}, in_len={}, completion={:?})",
        connection_handle,
        string_length1,
        completion
    );
    // SAFETY: connection_handle is null or a valid ConnectionHandle<B> allocated by
    // sql_alloc_handle; kind and group are validated by scope.get inside the closure.
    let ret = unsafe {
        panic_safe::<B, _>(connection_handle, |scope| {
            let handle = scope.get::<ConnectionHandle<B>>(connection_handle)?;
            handle.diagnostics.clear();

            // Spec 08002: Connection already in use.
            if handle.connection.is_some() {
                return Err(OdbcError::general(
                    "Connection handle already has an active connection",
                    SqlState::connection_in_use(),
                ));
            }

            // Spec HY009: InConnectionString must not be null (check before using the pointer).
            if in_connection_string.is_null() {
                return Err(OdbcError::general(
                    "InConnectionString must not be null",
                    SqlState::invalid_use_of_null_pointer(),
                ));
            }

            // Spec HY090: string_length1 must be >= 0 or SQL_NTS.
            if string_length1 < 0 && string_length1 != SQL_NTS as i16 {
                return Err(OdbcError::general(
                    format!("Invalid string_length1: {string_length1}"),
                    SqlState::invalid_string_or_buffer_length(),
                ));
            }

            // Spec HY090: buffer_length must be >= 0.
            if buffer_length < 0 {
                return Err(OdbcError::general(
                    format!("Invalid buffer_length: {buffer_length}"),
                    SqlState::invalid_string_or_buffer_length(),
                ));
            }

            let conn_str = utf16_to_string(in_connection_string, string_length1.into())?;
            let mut params = merge_dsn_params::<B>(&conn_str, read_dsn_keys)?;
            // The parsed form, never `conn_str`: `ConnectParams`' `Debug`
            // redacts credential keywords and the raw connection string does
            // not. Both sibling entry points log the same thing at the same
            // point in their bodies.
            tracing::debug!("SQLDriverConnectW: params = {:?}", params);
            crate::ffi::connect_attr::carry_connect_timeouts::<B>(handle, &mut params);
            params.set_prompter(prompter_for::<B>(completion));
            let connection = B::connect(&params).into_odbc()?;
            handle.connection = Some(connection);
            // A deferred attribute the backend cannot honour fails the
            // connect. Close the connection first: the application sees
            // SQL_ERROR and so will never call SQLDisconnect for it.
            if let Err(e) = crate::ffi::connect_attr::apply_pending_connect_attrs::<B>(handle) {
                if let Some(mut c) = handle.connection.take() {
                    // Logged, not propagated: the application is already being
                    // told the connect failed, and this teardown error would
                    // replace the actual reason. It is worth a line because a
                    // failed teardown here can leak a session on the server that
                    // no SQLDisconnect will ever reclaim — the application never
                    // got a live connection to disconnect. Loggable at all only
                    // because `Backend::Error` is now bounded by `std::error::Error`.
                    if let Err(teardown) = B::disconnect(&mut c) {
                        tracing::warn!(
                            "disconnect failed while unwinding a half-open connection; \
                             the data source may still hold the session: {teardown}"
                        );
                    }
                }
                return Err(e);
            }

            // An abandoned browse leaves accumulated attributes on the handle,
            // and `SQLBrowseConnectW` merges them into whatever the next browse
            // supplies. Connecting by any route ends that sequence, so the state
            // goes with it — `SQLBrowseConnectW` does the same on its own
            // success path.
            handle.browse_request = None;

            if !out_connection_string.is_null() {
                let _ = write_utf16(
                    &conn_str,
                    out_connection_string,
                    buffer_length,
                    string_length2_ptr,
                );
            } else if !string_length2_ptr.is_null() {
                let _ = write_utf16(&conn_str, std::ptr::null_mut(), 0, string_length2_ptr);
            }
            // Always return SUCCESS: returning SUCCESS_WITH_INFO for
            // truncated output strings triggers a diagnostic retrieval
            // code path in the Windows DM that crashes.
            Ok(SqlReturn::SUCCESS)
        })
    };
    tracing::debug!("SQLDriverConnectW -> {:?}", ret);
    ret
}

/// Generic implementation of SQLConnectW.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlconnect-function>
///
/// Reads DSN configuration from `odbc.ini` via `SQLGetPrivateProfileStringW`,
/// builds a `ConnectParams`, and calls `B::connect`. The `user_name` and
/// `authentication` parameters override any DSN-configured values.
///
/// # Parameters
///
/// - `connection_handle`: Connection handle.
/// - `server_name`: Data source name (DSN); UTF-16 encoded.
/// - `name_length1`: Length of `server_name` in characters, or `SQL_NTS` (-3).
/// - `user_name`: User identifier; may be null.
/// - `name_length2`: Length of `user_name` in characters, or `SQL_NTS` (-3).
/// - `authentication`: Password; may be null.
/// - `name_length3`: Length of `authentication` in characters, or `SQL_NTS` (-3).
///
/// # Spec compliance
///
/// - 01000: General warning — not returned here (no driver-specific info messages produced).
/// - 01S02: Option value changed — not returned here (no connection attribute substitution is
///   performed during connect).
/// - 08001: Client unable to establish connection — returned by the backend via `B::connect`.
/// - 08002: Connection name in use — the spec annotates this `(DM)`; it is guarded
///   defensively here. The check is `handle.connection.is_some()`, kept because core is
///   also linked without a Driver Manager in front of it, and because connecting twice
///   through one handle would leak the first connection.
/// - 08004: Server rejected the connection — may be returned by the backend via `B::connect`.
/// - 08S01: Communication link failure — not returned here; a failure while
///   establishing the connection is 08001. 08S01 applies once the connection
///   exists (see AGENTS.md, "08001 versus 08S01").
/// - 28000: Invalid authorization specification — may be returned by the backend via `B::connect`.
/// - 3D000: Invalid catalog name — **absent from this function's diagnostics table**, yet
///   returned by this driver. A `SQL_ATTR_CURRENT_CATALOG` set before connecting is applied here,
///   and [`crate::backend::Backend::set_current_catalog`] owes `3D000` for a catalog the data
///   source does not have. The state is propagated rather than degraded: telling the application
///   its connection failed for an unrelated reason would be worse than naming a state this table
///   omits. The spec notes interoperable applications set this attribute *before* connecting, so
///   this is the path that matters most.
/// - HY000: General error — returned for any backend error with no specific SQLSTATE.
/// - HY001: Memory allocation failure — (driver-manager-handled; not returned here).
/// - HY008: Operation canceled; not returned here. Cancelling a connection-level call needs
///   `SQLCancelHandle` on a connection handle, which this driver does not export, so no cancel
///   token exists for this call to observe — `SQLCancel` takes a statement handle and cannot
///   reach one. The asynchronous clause is likewise inapplicable: core never returns
///   `SQL_STILL_EXECUTING`.
/// - HY010: Function sequence error (async in progress) — (driver-manager-handled; not returned here).
/// - HY013: Memory management error — not returned here (Rust panics on alloc failure).
/// - HY090: Invalid string or buffer length — both clauses of this row are `(DM)`-marked;
///   the first is guarded defensively here, returning `HY090` when `name_length1`,
///   `name_length2` or `name_length3` is negative and not equal to `SQL_NTS` (-3). The
///   second, a data source name over the maximum length, is not: core declares no maximum.
///
///   **Also returned here**, for a condition neither clause states: any of `ServerName`,
///   `UserName` or `Authentication` — all three, and no other argument — passed as
///   `SQL_NTS` with no null terminator within `MAX_NTS_SCAN` (1 048 576) code units, which is a
///   length the driver cannot determine. The diagnostic names which of the three it was. The
///   two credential arguments were read inside `if let Ok(..)` and so *discarded* the error:
///   a swallowed overrun connected with whatever credentials the DSN supplied, under a
///   *UserName* the application believed it had passed. See
///   `connect_refuses_an_nts_credential_that_runs_to_the_scan_cap`.
/// - HYT00: Login timeout expired — not returned here (login timeout not enforced).
/// - HY114: Driver does not support connection-level async — (driver-manager-handled; not returned here).
/// - HYT01: Connection timeout expired — not returned here (connection timeout not enforced).
/// - S1118: Driver does not support asynchronous notification — not returned here, and the
///   row carries no `(DM)` marker. Core implements no connection-level asynchronous
///   notification, so an application has no way to ask for it and nothing to be refused.
/// - IM001–IM005: Driver Manager internal codes — (driver-manager-handled; not returned
///   here). Core resolves a DSN through `merge_dsn_params` and never loads, translates or
///   negotiates with another driver, so none of the block's conditions can arise in it.
/// - IM006: Driver's SQLSetConnectAttr failed — not returned here, and the row carries no
///   `(DM)` marker. Core calls no other driver's `SQLSetConnectAttr`; a connection
///   attribute set before connecting is applied through a `Backend` hook, whose failure is
///   propagated with the hook's own state.
/// - IM009: Unable to load translation DLL — not returned here, and the row carries no
///   `(DM)` marker. Core loads no translation DLL: it implements no
///   `SQL_ATTR_TRANSLATE_LIB`, so there is nothing to fail to load.
/// - IM010: Data source name too long — (driver-manager-handled; not returned here).
/// - IM014: Invalid name of File DSN — (driver-manager-handled; not returned here).
/// - IM015: Driver's SQLDriverConnect on SQL_HANDLE_DBC_INFO_TOKEN failed — not returned
///   here, and the row carries no `(DM)` marker. Core exports no
///   `SQL_HANDLE_DBC_INFO_TOKEN` entry point, so the call this row describes cannot be
///   made against it.
/// - IM017: Polling disabled; not returned here (the asynchronous notification model is not
///   supported — not DM-annotated in the spec).
/// - IM018: SQLCompleteAsync not called; not returned here (the asynchronous notification
///   model is not supported — not DM-annotated in the spec).
///
/// # Safety
///
/// `connection_handle` must point to a valid `ConnectionHandle<B>`.
/// `server_name` must be a valid UTF-16 string of the given length (or null-terminated).
#[allow(clippy::too_many_arguments)]
pub unsafe fn sql_connect_w<B: Backend>(
    connection_handle: *mut c_void,
    server_name: *const u16,
    name_length1: i16,
    user_name: *const u16,
    name_length2: i16,
    authentication: *const u16,
    name_length3: i16,
) -> SqlReturn {
    tracing::debug!(
        "SQLConnectW(conn={:?}, server_len={})",
        connection_handle,
        name_length1
    );
    // SAFETY: connection_handle is null or a valid ConnectionHandle<B> allocated by
    // sql_alloc_handle; kind and group are validated by scope.get inside the closure.
    let ret = unsafe {
        panic_safe::<B, _>(connection_handle, |scope| {
            let handle = scope.get::<ConnectionHandle<B>>(connection_handle)?;
            handle.diagnostics.clear();

            // Spec 08002: Connection already in use.
            if handle.connection.is_some() {
                return Err(OdbcError::general(
                    "Connection handle already has an active connection",
                    SqlState::connection_in_use(),
                ));
            }

            // Spec HY090: name lengths must be >= 0 or SQL_NTS (-3).
            let sql_nts = SQL_NTS as i16;
            if name_length1 < 0 && name_length1 != sql_nts {
                return Err(OdbcError::general(
                    format!("Invalid NameLength1(ServerName): {name_length1}"),
                    SqlState::invalid_string_or_buffer_length(),
                ));
            }
            if !user_name.is_null() && name_length2 < 0 && name_length2 != sql_nts {
                return Err(OdbcError::general(
                    format!("Invalid NameLength2(UserName): {name_length2}"),
                    SqlState::invalid_string_or_buffer_length(),
                ));
            }
            if !authentication.is_null() && name_length3 < 0 && name_length3 != sql_nts {
                return Err(OdbcError::general(
                    format!("Invalid NameLength3(Authentication): {name_length3}"),
                    SqlState::invalid_string_or_buffer_length(),
                ));
            }

            // Parse DSN name
            let dsn = utf16_to_string_named(
                server_name,
                name_length1.into(),
                "SQLConnectW's ServerName argument",
            )?;
            tracing::debug!("SQLConnectW: DSN={}", dsn);

            // Read DSN keys from odbc.ini via SQLGetPrivateProfileStringW.
            // The DM's odbcinst library reads ODBCINI/ODBCSYSINI env vars.
            let dsn_keys = read_dsn_keys(&dsn);
            let mut params: ConnectParams = dsn_keys.into_iter().collect();
            // The DSN *name* is not among the DSN's keys. `read_dsn_keys`
            // enumerates the odbc.ini section, and a section lists the keywords
            // inside it, never its own heading — so without this the one
            // parameter this entry point is named for was the one a backend
            // could not see, and `ConnectParams::dsn()` answered `None` on the
            // exact path where the spec makes the value load-bearing:
            // `SQL_DATA_SOURCE_NAME` "is the value of the *ServerName* argument
            // in SQLConnect". `SQLDriverConnectW` has always had it, because
            // `merge_dsn_params` merges the file's keys *under* a connection
            // string that already carried `DSN=`.
            //
            // Inserted after the section's keys, so a stray `DSN` keyword
            // inside the section cannot displace the name the application
            // actually connected with.
            if !dsn.is_empty() {
                params.insert("dsn", dsn.clone());
            }
            params.declare_sensitive_keywords(B::sensitive_connect_keywords());

            // Override with user/password if provided.
            //
            // Propagated with `?`, not matched with `if let Ok(..)`. While the
            // helper's only failure was a null pointer the two were the same
            // thing, because the null check in front of it already ruled that
            // out. They are not the same now: an `SQL_NTS` credential running
            // to `MAX_NTS_SCAN` is `HY090`, and swallowing it would connect as
            // whoever the DSN names instead of telling the application its
            // *UserName* argument was unreadable.
            if !user_name.is_null() {
                let user = utf16_to_string_named(
                    user_name,
                    name_length2.into(),
                    "SQLConnectW's UserName argument",
                )?;
                if !user.is_empty() {
                    params.insert("user", user);
                }
            }
            if !authentication.is_null() {
                let auth = utf16_to_string_named(
                    authentication,
                    name_length3.into(),
                    "SQLConnectW's Authentication argument",
                )?;
                if !auth.is_empty() {
                    params.insert("password", auth);
                }
            }
            crate::ffi::connect_attr::carry_connect_timeouts::<B>(handle, &mut params);
            // `SQLConnect` has no *DriverCompletion* argument, so there is
            // nothing here that could forbid a prompt: `prompter_for`'s `None`
            // means "this entry point cannot be told", not "do not prompt".
            params.set_prompter(prompter_for::<B>(None));
            tracing::debug!("SQLConnectW: params = {:?}", params);
            let connection = B::connect(&params).into_odbc()?;
            handle.connection = Some(connection);
            // A deferred attribute the backend cannot honour fails the
            // connect. Close the connection first: the application sees
            // SQL_ERROR and so will never call SQLDisconnect for it.
            if let Err(e) = crate::ffi::connect_attr::apply_pending_connect_attrs::<B>(handle) {
                if let Some(mut c) = handle.connection.take() {
                    // Logged, not propagated: the application is already being
                    // told the connect failed, and this teardown error would
                    // replace the actual reason. It is worth a line because a
                    // failed teardown here can leak a session on the server that
                    // no SQLDisconnect will ever reclaim — the application never
                    // got a live connection to disconnect. Loggable at all only
                    // because `Backend::Error` is now bounded by `std::error::Error`.
                    if let Err(teardown) = B::disconnect(&mut c) {
                        tracing::warn!(
                            "disconnect failed while unwinding a half-open connection; \
                             the data source may still hold the session: {teardown}"
                        );
                    }
                }
                return Err(e);
            }

            // An abandoned browse leaves accumulated attributes on the handle,
            // and `SQLBrowseConnectW` merges them into whatever the next browse
            // supplies. Connecting by any route ends that sequence, so the state
            // goes with it — `SQLBrowseConnectW` does the same on its own
            // success path.
            handle.browse_request = None;

            Ok(SqlReturn::SUCCESS)
        })
    };
    tracing::debug!("SQLConnectW -> {:?}", ret);
    ret
}

/// Generic implementation of SQLBrowseConnectW.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlbrowseconnect-function>
///
/// Implements iterative connection browsing. The application calls this function
/// one or more times, supplying additional connection attributes on each call.
/// The driver checks which required attributes (from `B::browse_connect_attrs()`)
/// are still missing and either requests them via `NEED_DATA` or, when all are
/// present, establishes the connection and returns `SUCCESS`.
///
/// Each call merges the new parameters on top of any previously accumulated
/// browse state. The accumulated state is stored in `handle.browse_request` and
/// is cleared on successful connect.
///
/// # Parameters
///
/// - `connection_handle`: Connection handle.
/// - `in_connection_string`: Partial or full connection string (`Key=Value;...`); must not be null.
/// - `string_length1`: Length of `in_connection_string` in characters, or `SQL_NTS` (-3).
/// - `out_connection_string`: Buffer to receive the browse result string; may be null.
/// - `buffer_length`: Size of `out_connection_string` in characters.
/// - `string_length2_ptr`: Receives the length of the browse result string; may be null.
///
/// # Spec compliance
///
/// - 01000: General warning — not returned here (no driver-specific info messages produced).
/// - 01004: String data, right truncated — not returned here; output truncation is silently
///   accepted (same rationale as `SQLDriverConnectW`: avoiding a Windows DM crash path).
/// - 01S00: Invalid connection string attribute — not returned here (unknown attributes are
///   silently ignored by `ConnectParams::parse`).
/// - 01S02: Option value changed — not returned here (no attribute substitution at connect time).
/// - 08001: Client unable to establish connection — returned by the backend via `B::connect`.
/// - 08002: Connection name in use — the spec annotates this `(DM)`; it is guarded
///   defensively here. The check is `handle.connection.is_some()`, kept because core is
///   also linked without a Driver Manager in front of it, and because connecting twice
///   through one handle would leak the first connection.
/// - 08004: Server rejected the connection — may be returned by the backend via `B::connect`.
/// - 08S01: Communication link failure — not returned here; a failure while
///   establishing the connection is 08001. 08S01 applies once the connection
///   exists (see AGENTS.md, "08001 versus 08S01").
/// - 28000: Invalid authorization specification — may be returned by the backend via `B::connect`.
/// - 3D000: Invalid catalog name — **absent from this function's diagnostics table**, yet
///   returned by this driver. A `SQL_ATTR_CURRENT_CATALOG` set before connecting is applied here,
///   and [`crate::backend::Backend::set_current_catalog`] owes `3D000` for a catalog the data
///   source does not have. The state is propagated rather than degraded: telling the application
///   its connection failed for an unrelated reason would be worse than naming a state this table
///   omits. The spec notes interoperable applications set this attribute *before* connecting, so
///   this is the path that matters most.
/// - HY000: General error — returned for any backend error with no specific SQLSTATE.
/// - HY001: Memory allocation failure — every clause of this row is `(DM)`
///   (driver-manager-handled; not returned here). It could not arise anyway: Rust's
///   allocator aborts on OOM rather than returning an error.
/// - HY008: Operation canceled; not returned here. Cancelling a connection-level call needs
///   `SQLCancelHandle` on a connection handle, which this driver does not export, so no cancel
///   token exists for this call to observe — `SQLCancel` takes a statement handle and cannot
///   reach one. The asynchronous clause is likewise inapplicable: core never returns
///   `SQL_STILL_EXECUTING`.
/// - HY009: Invalid use of null pointer — **absent from this function's diagnostics table**,
///   yet returned by this driver when `in_connection_string` is null, for the reason
///   `SQLDriverConnectW`'s identical guard gives.
/// - HY010: Function sequence error (async in progress) — (driver-manager-handled; not returned here).
/// - HY013: Memory management error — not returned here (Rust panics on alloc failure).
/// - HY090: Invalid string or buffer length — returned when `string_length1 < 0 && != SQL_NTS`,
///   or when `buffer_length < 0`. Note: spec marks these as `(DM)`, but checked here as
///   defence-in-depth when called outside a full Driver Manager stack. **Also returned** for
///   a condition the row does not state: `InConnectionString` passed as `SQL_NTS` with no
///   null terminator within `MAX_NTS_SCAN` (1 048 576) code units, which is a length the driver
///   cannot determine. It is this function's only `SQL_NTS` argument, so that is the whole
///   set. The scanned prefix used to be browsed with instead, so the browse round-tripped
///   against a *different data source* than the application named. See
///   `browse_connect_refuses_an_nts_connection_string_that_runs_to_the_scan_cap`.
/// - HYC00: Optional feature not implemented — **absent from this function's diagnostics
///   table**, yet returned by this driver, but only from a
///   connection attribute set before the connect. Core applies those here (see
///   `apply_pending_connect_attrs`), and an unimplemented hook — most often the defaulted
///   [`crate::backend::Backend::set_current_catalog`] — reports `HYC00`, which fails the connect
///   and tears it down. Nothing in the connect path itself produces it.
/// - HYT00: Login timeout expired — not returned here (login timeout not enforced).
/// - HYT01: Connection timeout expired — not returned here (connection timeout not enforced).
/// - HY114: Driver does not support connection-level asynchronous function execution —
///   (driver-manager-handled; not returned here).
/// - S1118: Driver does not support asynchronous notification — not returned here, and the
///   row carries no `(DM)` marker. Core implements no connection-level asynchronous
///   notification, so an application has no way to ask for it and nothing to be refused.
/// - IM001–IM006: Driver Manager internal codes — (driver-manager-handled; not returned
///   here). Core resolves a DSN through `merge_dsn_params` and never loads, translates or
///   negotiates with another driver, so none of the block's conditions can arise in it.
/// - IM009: Unable to load translation DLL — not returned here, and the row carries no
///   `(DM)` marker. Core loads no translation DLL: it implements no
///   `SQL_ATTR_TRANSLATE_LIB`, so there is nothing to fail to load.
/// - IM010–IM012: Driver Manager internal codes — (driver-manager-handled; not returned
///   here).
/// - IM014: Invalid name of File DSN — (driver-manager-handled; not returned here).
/// - IM017: Polling disabled; not returned here (the asynchronous notification model is not
///   supported — not DM-annotated in the spec).
/// - IM018: SQLCompleteAsync not called; not returned here (the asynchronous notification
///   model is not supported — not DM-annotated in the spec).
///
/// # Safety
///
/// `connection_handle` must point to a valid `ConnectionHandle<B>`.
/// `in_connection_string` must be a valid UTF-16 string of the given length
/// (or null-terminated if `string_length1` is `SQL_NTS`).
pub unsafe fn sql_browse_connect_w<B: Backend>(
    connection_handle: *mut c_void,
    in_connection_string: *const u16,
    string_length1: i16,
    out_connection_string: *mut u16,
    buffer_length: i16,
    string_length2_ptr: *mut i16,
) -> SqlReturn {
    tracing::trace!(
        "SQLBrowseConnectW(conn={:?}, in_len={})",
        connection_handle,
        string_length1
    );
    // SAFETY: connection_handle is null or a valid ConnectionHandle<B> allocated by
    // sql_alloc_handle; kind and group are validated by scope.get inside the closure.
    let ret = unsafe {
        panic_safe::<B, _>(connection_handle, |scope| {
            let handle = scope.get::<ConnectionHandle<B>>(connection_handle)?;
            handle.diagnostics.clear();

            // Spec 08002: Connection already in use.
            if handle.connection.is_some() {
                return Err(OdbcError::general(
                    "Connection handle already has an active connection",
                    SqlState::connection_in_use(),
                ));
            }

            // Spec HY009: InConnectionString must not be null.
            if in_connection_string.is_null() {
                return Err(OdbcError::general(
                    "InConnectionString must not be null",
                    SqlState::invalid_use_of_null_pointer(),
                ));
            }

            // Spec HY090: string_length1 must be >= 0 or SQL_NTS.
            if string_length1 < 0 && string_length1 != SQL_NTS as i16 {
                return Err(OdbcError::general(
                    format!("Invalid string_length1: {string_length1}"),
                    SqlState::invalid_string_or_buffer_length(),
                ));
            }

            // Spec HY090: buffer_length must be >= 0.
            if buffer_length < 0 {
                return Err(OdbcError::general(
                    format!("Invalid buffer_length: {buffer_length}"),
                    SqlState::invalid_string_or_buffer_length(),
                ));
            }

            // Parse the incoming connection string.
            let conn_str = utf16_to_string(in_connection_string, string_length1.into())?;
            let new_params = merge_dsn_params::<B>(&conn_str, read_dsn_keys)?;
            // The parsed form, never the raw string: `ConnectParams`' `Debug`
            // redacts credential keywords and the raw string does not, so
            // logging `conn_str` wrote `PWD=` to the log file verbatim.
            tracing::debug!("SQLBrowseConnectW: params = {:?}", new_params);

            // Merge with any previously accumulated browse state.
            // New params take priority: start from new params and merge prior state into them
            // (merge does not overwrite existing keys, so new params win).
            let mut merged = new_params;
            if let Some(prior) = handle.browse_request.take() {
                merged.merge(&prior);
            }

            // Check which required attributes are still missing.
            let required = B::browse_connect_attrs();
            let missing: Vec<&str> = required
                .iter()
                .map(std::borrow::Cow::as_ref)
                .filter(|attr| merged.get(attr).is_none())
                .collect();

            if !missing.is_empty() {
                // Store accumulated state and request remaining attributes.
                let browse_result: String = missing
                    .iter()
                    .map(|attr| {
                        let upper = attr.to_uppercase();
                        format!("{attr}:{upper}=?")
                    })
                    .collect::<Vec<_>>()
                    .join(";");
                tracing::debug!(
                    "SQLBrowseConnectW: missing attrs={:?}, browse_result={:?}",
                    missing,
                    browse_result
                );
                handle.browse_request = Some(merged);
                if !out_connection_string.is_null() {
                    let _ = write_utf16(
                        &browse_result,
                        out_connection_string,
                        buffer_length,
                        string_length2_ptr,
                    );
                } else if !string_length2_ptr.is_null() {
                    let _ =
                        write_utf16(&browse_result, std::ptr::null_mut(), 0, string_length2_ptr);
                }
                return Ok(SqlReturn::NEED_DATA);
            }

            // All required attributes are present; connect.
            crate::ffi::connect_attr::carry_connect_timeouts::<B>(handle, &mut merged);
            // No *DriverCompletion* argument here either, for the reason given
            // at `sql_connect_w`. Set after the `NEED_DATA` branch above has
            // stored `merged` in `handle.browse_request`, so nothing that
            // outlives this call ever holds a prompter.
            merged.set_prompter(prompter_for::<B>(None));
            let connection = B::connect(&merged).into_odbc()?;
            handle.connection = Some(connection);
            // A deferred attribute the backend cannot honour fails the
            // connect. Close the connection first: the application sees
            // SQL_ERROR and so will never call SQLDisconnect for it.
            if let Err(e) = crate::ffi::connect_attr::apply_pending_connect_attrs::<B>(handle) {
                if let Some(mut c) = handle.connection.take() {
                    // Logged, not propagated: the application is already being
                    // told the connect failed, and this teardown error would
                    // replace the actual reason. It is worth a line because a
                    // failed teardown here can leak a session on the server that
                    // no SQLDisconnect will ever reclaim — the application never
                    // got a live connection to disconnect. Loggable at all only
                    // because `Backend::Error` is now bounded by `std::error::Error`.
                    if let Err(teardown) = B::disconnect(&mut c) {
                        tracing::warn!(
                            "disconnect failed while unwinding a half-open connection; \
                             the data source may still hold the session: {teardown}"
                        );
                    }
                }
                return Err(e);
            }
            handle.browse_request = None;

            // Write the complete connection string to the output buffer.
            //
            // Truncation is deliberately not reported, for the reason given in
            // `sql_driver_connect_w`: returning SQL_SUCCESS_WITH_INFO for a
            // truncated output connection string sends the Windows Driver
            // Manager down a diagnostic-retrieval path that crashes. The
            // required length still reaches the application through
            // `string_length2_ptr`, which is what lets it retry with a larger
            // buffer.
            let complete = merged.to_connection_string();
            if !out_connection_string.is_null() {
                let _ = write_utf16(
                    &complete,
                    out_connection_string,
                    buffer_length,
                    string_length2_ptr,
                );
            } else if !string_length2_ptr.is_null() {
                let _ = write_utf16(&complete, std::ptr::null_mut(), 0, string_length2_ptr);
            }
            Ok(SqlReturn::SUCCESS)
        })
    };
    tracing::debug!("SQLBrowseConnectW -> {:?}", ret);
    ret
}

/// The prompter this connect may use, given the entry point's *DriverCompletion*.
///
/// **The one place that decides whether a driver is allowed to prompt.** The
/// three connect entry points differ only in what they can pass here, and two
/// of them can pass nothing at all; putting the rule in each of them is how
/// they would come to disagree.
///
/// `completion` is `None` when the entry point has no *DriverCompletion*
/// argument — `SQLConnect` and `SQLBrowseConnect` — or when the value did not
/// name a flag. Both are read as permitting a prompt:
///
/// - `SQLConnect` is the DSN path, which is how `isql` and Excel connect, so
///   they are the likeliest interactive callers of the whole driver. Reading
///   the absent argument as `SQL_DRIVER_NOPROMPT` would lock DSN connections
///   out of interactive authentication entirely, and no spec text asks for
///   that.
/// - An unrecognised value is `HY110` ("Invalid driver completion") in
///   `SQLDriverConnect`'s diagnostics table, but **both** clauses of that row
///   carry `(DM)`. The check is the Driver Manager's; core adds none, and falls
///   back to the most permissive treatment rather than inventing a driver-side
///   error the spec assigns elsewhere.
///
/// Only `SQL_DRIVER_NOPROMPT` forbids prompting. `SQL_DRIVER_COMPLETE`,
/// `SQL_DRIVER_PROMPT` and `SQL_DRIVER_COMPLETE_REQUIRED` all permit it — the
/// spec's Driver Guidelines describe the driver putting up a dialog under each
/// of the three.
fn prompter_for<B: Backend>(
    completion: Option<DriverConnectOption>,
) -> Option<Arc<dyn crate::prompt::Prompter>> {
    if completion == Some(DriverConnectOption::NoPrompt) {
        return None;
    }
    B::prompter()
}

/// Parse a connection string, merging in DSN keys from odbc.ini when a `DSN=` key is present.
///
/// Explicit connection string values take priority over DSN-file values. The
/// direction matters: an application states its connection string in its own
/// code, whereas `odbc.ini` is a file any process running as the user can edit,
/// so the DSN file must never be able to redirect a connection or substitute
/// credentials the application supplied itself.
///
/// The merge is done on parsed values, never by concatenating strings and
/// re-parsing. A DSN value is arbitrary file content: rendered back into
/// `Key=Value;` form it could contain a `;` and inject further keywords.
/// [`ConnectParams::merge`] does not overwrite existing keys, which is exactly
/// the required precedence, and [`ConnectParams::to_connection_string`] quotes
/// what it renders so the result survives being parsed again.
///
/// `dsn_resolver` is injectable so unit tests can supply fake DSN data.
fn merge_dsn_params<B: Backend>(
    conn_str: &str,
    dsn_resolver: impl Fn(&str) -> Vec<(String, String)>,
) -> Result<ConnectParams, OdbcError> {
    let mut params = ConnectParams::parse(conn_str)?;
    if let Some(dsn) = params.get("dsn").map(str::to_owned) {
        let from_dsn: ConnectParams = dsn_resolver(&dsn).into_iter().collect();
        params.merge(&from_dsn);
    }
    // Seeded here rather than at each log site so that a driver's own `{:?}` on
    // the `ConnectParams` it receives in `connect` redacts too, not just core's.
    params.declare_sensitive_keywords(B::sensitive_connect_keywords());
    Ok(params)
}

/// Read key-value pairs for a DSN from the ODBC configuration file.
///
/// Uses `SQLGetPrivateProfileStringW` from `libodbcinst` to read the DSN
/// section from `odbc.ini`. First reads all keys (entry=NULL), then reads
/// the value for each key.
fn read_dsn_keys(dsn: &str) -> Vec<(String, String)> {
    let dsn_wide: Vec<u16> = dsn.encode_utf16().chain(std::iter::once(0)).collect();
    let filename: Vec<u16> = "odbc.ini"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let default: Vec<u16> = [0u16].to_vec();

    // First call: get all key names (entry = NULL → returns null-separated list of keys)
    let mut key_buf = vec![0u16; 4096];
    // SAFETY: dsn_wide, default, and filename are all null-terminated UTF-16 strings
    // owned by this function. key_buf is a valid writable buffer of key_buf.len() u16s.
    let key_len = unsafe {
        crate::odbcinst::SQLGetPrivateProfileStringW(
            dsn_wide.as_ptr(),
            std::ptr::null(), // NULL = get all keys
            default.as_ptr(),
            key_buf.as_mut_ptr(),
            key_buf.len() as i32,
            filename.as_ptr(),
        )
    };

    if key_len <= 0 {
        tracing::debug!("SQLConnectW: no DSN keys found for '{}'", dsn);
        return Vec::new();
    }

    // Parse null-separated key list (double-null terminated).
    //
    // `key_len` comes from the installer library, not from this crate. unixODBC
    // and odbccp32 have both been observed returning the length *required*
    // rather than the length *copied* when a buffer is too small, so it is
    // clamped rather than trusted: an unclamped slice would panic, and the
    // panic would cross the C ABI boundary from `SQLConnectW`.
    let key_len = (key_len as usize).min(key_buf.len());
    let keys: Vec<String> = key_buf[..key_len]
        .split(|&c| c == 0)
        .filter(|s| !s.is_empty())
        .map(String::from_utf16_lossy)
        .collect();

    // Read value for each key
    let mut result = Vec::new();
    for key in &keys {
        // Skip "Driver"; it's the DM's concern, not ours
        if key.eq_ignore_ascii_case("Driver") {
            continue;
        }
        let key_wide: Vec<u16> = key.encode_utf16().chain(std::iter::once(0)).collect();
        let mut val_buf = vec![0u16; 1024];
        // SAFETY: dsn_wide, key_wide, default, and filename are all null-terminated UTF-16
        // strings owned by this function. val_buf is a valid writable buffer of val_buf.len() u16s.
        let val_len = unsafe {
            crate::odbcinst::SQLGetPrivateProfileStringW(
                dsn_wide.as_ptr(),
                key_wide.as_ptr(),
                default.as_ptr(),
                val_buf.as_mut_ptr(),
                val_buf.len() as i32,
                filename.as_ptr(),
            )
        };
        if val_len > 0 {
            // Clamped for the same reason as `key_len` above.
            let val_len = (val_len as usize).min(val_buf.len());
            let value = String::from_utf16_lossy(&val_buf[..val_len]);
            result.push((key.clone(), value));
        }
    }

    result
}

/// Generic implementation of SQLDisconnect.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqldisconnect-function>
///
/// Calls `B::disconnect` on the active connection and clears it from the handle.
/// On success, frees all statements and descriptors allocated on this connection.
///
/// During a `SQLBrowseConnect` sequence there is no connection to close, and
/// this function cancels the browse instead: the spec makes it the only way out
/// of one.
///
/// # Parameters
///
/// - `connection_handle`: Connection handle.
///
/// # Spec compliance
///
/// - 01000: General warning — not returned here (no driver-specific info messages).
/// - 01002: Disconnect error (warning: error during disconnect but disconnect succeeded) — not
///   returned here; the backend `disconnect` API is all-or-nothing (either succeeds or returns
///   an error). Returning 01002 would require a two-phase disconnect API. Deferred.
/// - 08003: Connection not open — returned when the handle holds neither a
///   connection nor an in-progress browse. Note: spec marks this as `(DM)`, but
///   it is also checked here as a defence-in-depth guard. It is deliberately
///   **not** returned during a browse: the spec's Comments section requires this
///   function to cancel the browse and return the connection to an unconnected
///   state, and `SQLBrowseConnect` names it as the only way to do so.
/// - 25000: Invalid transaction state — not returned here.
///   Returning 25000 would require a `has_active_transaction` flag on `ConnectionHandle`,
///   which is not currently tracked. Deferred.
/// - HY000: General error — returned for any backend `disconnect` error with no specific SQLSTATE.
/// - HY001: Memory allocation failure — not returned here (Rust panics on alloc failure).
/// - HY008: Operation canceled; not returned here. Cancelling a connection-level call needs
///   `SQLCancelHandle` on a connection handle, which this driver does not export, so no cancel
///   token exists for this call to observe — `SQLCancel` takes a statement handle and cannot
///   reach one. The asynchronous clause is likewise inapplicable: core never returns
///   `SQL_STILL_EXECUTING`.
/// - HY010: Function sequence error (async in progress) — (driver-manager-handled; not returned here).
/// - HY013: Memory management error — not returned here (Rust panics on alloc failure).
/// - HY117: Connection suspended due to unknown transaction state — (driver-manager-handled; not returned here).
/// - HYT01: Connection timeout expired — not returned here (connection timeout not enforced).
/// - IM001: Driver does not support this function — (driver-manager-handled; not returned here).
/// - IM017: Polling disabled; not returned here (the asynchronous notification model is not
///   supported — not DM-annotated in the spec).
/// - IM018: SQLCompleteAsync not called; not returned here (the asynchronous notification
///   model is not supported — not DM-annotated in the spec).
///
/// # Safety
///
/// `connection_handle` must point to a valid `ConnectionHandle<B>`.
pub unsafe fn sql_disconnect<B: Backend>(connection_handle: *mut c_void) -> SqlReturn {
    tracing::debug!("SQLDisconnect(conn={:?})", connection_handle);
    // SAFETY: connection_handle is null or a valid ConnectionHandle<B> allocated by
    // sql_alloc_handle; kind and group are validated by scope.get inside the closure.
    // The raw stmt_ptr values children_of(connection_handle) returns are valid
    // Box<StatementHandle<B>> allocations registered at statement creation
    // time; no other owner exists after disconnect, so Box::from_raw is sound
    // here.
    let ret = unsafe {
        panic_safe::<B, _>(connection_handle, |scope| {
            let handle = scope.get::<ConnectionHandle<B>>(connection_handle)?;
            handle.diagnostics.clear();

            // Spec 08003: Connection not open — except during a browse.
            let Some(ref mut conn) = handle.connection else {
                // Spec (Comments): "If an application calls SQLDisconnect after
                // SQLBrowseConnect returns SQL_NEED_DATA and before it returns
                // a different return code, the driver cancels the connection
                // browsing process and returns the connection to an unconnected
                // state." SQLBrowseConnect's own page makes this the only exit:
                // "an application must use SQLDisconnect to cancel the browse
                // process." `handle.connection` is None for the whole of a
                // browse, so without this branch the call the spec names is the
                // one call that cannot be made. The 08003 row carries (DM), and
                // the Driver Manager passes the call through during a browse for
                // exactly this reason.
                //
                // `take()` rather than a read: the accumulated attributes must
                // not survive into the next browse sequence on this handle.
                if handle.browse_request.take().is_some() {
                    tracing::debug!(
                        "SQLDisconnect: cancelled an in-progress SQLBrowseConnectW sequence"
                    );
                    return Ok(SqlReturn::SUCCESS);
                }
                return Err(OdbcError::NotConnected);
            };

            B::disconnect(conn).into_odbc()?;
            handle.connection = None;

            // Spec: "the driver frees those statements and all descriptors that
            // have been explicitly allocated on the connection."
            // Invalidate and drop all statement handles. free_connection_statements
            // retires each one's registry slot before dropping its allocation, so
            // any subsequent access (e.g. application calls SQLFreeHandle on a
            // stale pointer) fails the registry's validity check and returns
            // SQL_INVALID_HANDLE rather than causing a double-free.
            // The registry, not the allocation, is what invalidates a handle:
            // the application may still hold the statement tokens and every one
            // of them must now be refused. This also retires the four
            // descriptor slots each statement owns.
            crate::handles::free_connection_statements::<B>(connection_handle);
            // The "and all descriptors that have been explicitly allocated on the
            // connection" half of that sentence. Freed after the statements, so
            // nothing is left holding one as an override.
            crate::handles::free_connection_descriptors(connection_handle);

            Ok(SqlReturn::SUCCESS)
        })
    };
    tracing::debug!("SQLDisconnect -> {:?}", ret);
    ret
}

/// Generic implementation of SQLNativeSqlW.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlnativesql-function>
///
/// Returns the SQL string as the driver would send it to the data source: input
/// SQL is run through [`crate::escape::translate_escapes`] using the backend's
/// [`crate::backend::Backend::escape_dialect`] to expand ODBC escape sequences
/// (`{fn}`, `{d}`/`{t}`/`{ts}`, `{oj}`, `{escape}`), then the translated text is
/// written to the output buffer. Unlike `SQLExecDirectW`/`SQLPrepareW`, this
/// translation is not gated by `SQL_ATTR_NOSCAN`: that is a statement attribute,
/// and `SQLNativeSqlW` takes only a connection handle.
///
/// # Parameters
///
/// - `connection_handle`: Connection handle.
/// - `in_statement_text`: SQL text string to be translated; must not be null.
/// - `text_length1`: Length of `in_statement_text` in characters, or `SQL_NTS` (-3).
/// - `out_statement_text`: Buffer to receive the translated SQL string; may be null.
/// - `buffer_length`: Size of `out_statement_text` in characters.
/// - `text_length2_ptr`: Receives the length of the translated SQL string; may be null.
///
/// # Spec compliance
///
/// - 01000: General warning — not returned here (no driver-specific info messages).
/// - 01004: String data, right truncated — returned as `SUCCESS_WITH_INFO` when the output
///   buffer is too small to hold the translated SQL string.
/// - 08003: Connection not open — returned when `handle.connection` is `None`.
/// - 08S01: Communication link failure — not returned here (no network I/O in NativeSql).
/// - 22007: Invalid datetime format — not returned here (date/time escapes are rendered
///   as literal text by the dialect's `render_date`/`render_time`/`render_timestamp`,
///   not validated).
/// - 24000: Invalid cursor state — not returned here (no cursor involvement in NativeSql).
/// - HY000: General error — returned for unexpected errors, including escape-translation
///   failures from `crate::escape::translate_escapes` (an unsupported `{call ...}`/
///   `{?= call ...}` stored-procedure escape, or an unterminated/malformed escape
///   sequence). Unlike `SQLExecDirectW`/`SQLPrepareW`, the SQLNativeSql diagnostics
///   table has no `42000` or `HYC00` entries, so those cases are reported as `HY000`
///   here rather than propagating the codes `translate_escapes` uses elsewhere.
/// - HY001: Memory allocation failure — not returned here (Rust panics on alloc failure).
/// - HY009: Invalid use of null pointer — the spec annotates this `(DM)`, and its single
///   clause is a null `InStatementText`; it is guarded defensively here, because a null
///   pointer that reaches `utf16_to_string` is a soundness question rather than a spec one.
/// - HY010: Function sequence error (async in progress) — (driver-manager-handled; not returned here).
/// - HY013: Memory management error — not returned here (Rust panics on alloc failure).
/// - HY090: Invalid string or buffer length — every clause of this row is `(DM)`, and it is
///   guarded defensively here for the reason `HY009` gives: returned when
///   `text_length1 < 0 && != SQL_NTS`, or when `buffer_length < 0` and `out_statement_text`
///   is not null. **Also returned** for `InStatementText` passed as `SQL_NTS` with no null
///   terminator within `MAX_NTS_SCAN` (1 048 576) code units — this function's only `SQL_NTS`
///   argument, so the whole set — which is a length the driver cannot determine, and where
///   the scanned prefix used to be translated and echoed back as though complete. An
///   explicitly measured `TextLength1` is not limited, at any size. See
///   `native_sql_refuses_an_nts_statement_that_runs_to_the_scan_cap`.
/// - HY109: Invalid cursor position — not returned here (no cursor involvement in NativeSql).
/// - HY117: Connection suspended due to unknown transaction state — (driver-manager-handled; not returned here).
/// - HYT01: Connection timeout expired — not returned here (connection timeout not enforced).
/// - IM001: Driver does not support this function — (driver-manager-handled; not returned here).
///
/// # Safety
///
/// `connection_handle` must point to a valid `ConnectionHandle<B>`.
pub unsafe fn sql_native_sql_w<B: Backend>(
    connection_handle: *mut c_void,
    in_statement_text: *const u16,
    text_length1: i32,
    out_statement_text: *mut u16,
    buffer_length: i32,
    text_length2_ptr: *mut i32,
) -> SqlReturn {
    tracing::debug!("SQLNativeSqlW(conn={:?})", connection_handle);
    // SAFETY: connection_handle is null or a valid ConnectionHandle<B> allocated by
    // sql_alloc_handle; kind and group are validated by scope.get inside the closure.
    // in_statement_text is a valid pointer to text_length1 UTF-16 code units (or
    // null-terminated if text_length1 == SQL_NTS); null checked before dereference.
    // out_statement_text and text_length2_ptr are checked for null before any write.
    let ret = unsafe {
        panic_safe::<B, _>(connection_handle, |scope| {
            let conn = scope.get::<ConnectionHandle<B>>(connection_handle)?;
            conn.diagnostics.clear();

            // Spec 08003: Connection not open. Bound rather than tested,
            // because the escape dialect below is a property of the
            // connection.
            let Some(ref connection) = conn.connection else {
                return Err(OdbcError::NotConnected);
            };

            // Spec HY009: InStatementText must not be NULL.
            if in_statement_text.is_null() {
                return Err(OdbcError::general(
                    "InStatementText must not be NULL",
                    SqlState::invalid_use_of_null_pointer(),
                ));
            }

            // Spec HY090: TextLength1 must be >= 0 or SQL_NTS.
            if text_length1 < 0 && text_length1 != SQL_NTS {
                return Err(OdbcError::general(
                    format!("Invalid TextLength1: {text_length1}"),
                    SqlState::invalid_string_or_buffer_length(),
                ));
            }

            // Spec HY090: BufferLength must be >= 0 when OutStatementText is not null.
            if buffer_length < 0 && !out_statement_text.is_null() {
                return Err(OdbcError::general(
                    format!("Invalid BufferLength: {buffer_length}"),
                    SqlState::invalid_string_or_buffer_length(),
                ));
            }

            let sql = utf16_to_string(in_statement_text, text_length1)?;
            // SQLNativeSql's diagnostics table has no 42000/HYC00 entries (unlike
            // SQLExecDirect/SQLPrepare), so a translation failure (a `{call}`
            // escape or a malformed escape) is surfaced here as HY000 instead of
            // propagating translate_escapes' own (off-table) SQLSTATE.
            let translated = crate::escape::translate_escapes(&sql, &B::escape_dialect(connection))
                .map_err(|e| {
                    OdbcError::general(
                        format!("failed to translate ODBC escapes: {e}"),
                        SqlState::general_error(),
                    )
                })?;
            let wide: Vec<u16> = translated.encode_utf16().collect();

            // Always report the number of characters (excluding null terminator).
            if !text_length2_ptr.is_null() {
                std::ptr::write_unaligned(text_length2_ptr, wide.len() as i32);
            }

            if !out_statement_text.is_null() {
                if buffer_length > 0 {
                    // Copy at most buffer_length-1 code units, then null-terminate.
                    let copy_len = wide.len().min((buffer_length - 1) as usize);
                    // SAFETY: out_statement_text is non-null (checked above) and the caller
                    // guarantees it points to a buffer of at least buffer_length u16 elements.
                    // copy_len <= buffer_length - 1 < buffer_length, so add(copy_len) is in bounds.
                    // Byte-wise, and unaligned for the terminator: the
                    // application's pointer carries no alignment guarantee.
                    std::ptr::copy_nonoverlapping(
                        wide.as_ptr().cast::<u8>(),
                        out_statement_text.cast::<u8>(),
                        copy_len * size_of::<u16>(),
                    );
                    std::ptr::write_unaligned(out_statement_text.add(copy_len), 0);
                    if copy_len < wide.len() {
                        // The 01004 record the SUCCESS_WITH_INFO refers to: an
                        // application that sees it calls SQLGetDiagRec to find out
                        // what happened.
                        conn.diagnostics.push(&OdbcError::StringTruncated);
                        return Ok(SqlReturn::SUCCESS_WITH_INFO);
                    }
                } else if !wide.is_empty() {
                    // A buffer the application declared empty is the extreme
                    // case of the spec's "not large enough": nothing fits, not
                    // even the null terminator, so nothing is written — but the
                    // truncation is real and must be reported. The required
                    // length has already gone to TextLength2Ptr above, which is
                    // what lets the application retry with a real buffer.
                    conn.diagnostics.push(&OdbcError::StringTruncated);
                    return Ok(SqlReturn::SUCCESS_WITH_INFO);
                }
            }

            Ok(SqlReturn::SUCCESS)
        })
    };
    tracing::debug!("SQLNativeSqlW -> {:?}", ret);
    ret
}

#[cfg(test)]
mod tests {
    use super::*;
    use odbc_sys::HandleType;

    use crate::ffi::handle::{sql_alloc_handle, sql_free_handle};
    use crate::test_utils::{MockBackend, MockBrowseBackend};

    /// Helper: allocate env + connection handles **for the backend the test
    /// then calls through**, returning both raw pointers.
    ///
    /// The type parameter is load-bearing, not decoration. A handle is a
    /// `ConnectionHandle<B>`, whose `connection: Option<B::Connection>` field
    /// has a different layout for every backend, while validation is a registry
    /// slot, generation and kind compare that never dereferences the caller's
    /// value. Allocating as one backend and calling as another therefore passes
    /// that compare and then reads memory laid out for a different type —
    /// undefined behaviour Miri catches and a plain `cargo test` does not.
    unsafe fn alloc_env_and_conn_for<B: Backend>() -> (*mut c_void, *mut c_void) {
        let mut env: *mut c_void = std::ptr::null_mut();
        let _ = unsafe {
            sql_alloc_handle::<B>(HandleType::Env as i16, std::ptr::null_mut(), &mut env)
        };
        let mut conn: *mut c_void = std::ptr::null_mut();
        let _ = unsafe { sql_alloc_handle::<B>(HandleType::Dbc as i16, env, &mut conn) };
        (env, conn)
    }

    /// [`alloc_env_and_conn_for`] for [`MockBackend`], which is what most tests
    /// in this module want.
    unsafe fn alloc_env_and_conn() -> (*mut c_void, *mut c_void) {
        unsafe { alloc_env_and_conn_for::<MockBackend>() }
    }

    // -----------------------------------------------------------------------
    // merge_dsn_params
    //
    // The DSN file is lower-trust than the connection string: an application
    // states its own connection string in code, while odbc.ini is a file any
    // process running as the user can edit. The spec, this function's doc
    // comment and AGENTS.md all say the explicit value wins.
    // -----------------------------------------------------------------------

    #[test]
    fn merge_dsn_params_takes_dsn_value_when_the_connection_string_omits_it() {
        let params = merge_dsn_params::<MockBackend>("DSN=prod", |dsn| {
            assert_eq!(dsn, "prod");
            vec![("Host".into(), "dsn-host.internal".into())]
        })
        .unwrap();

        assert_eq!(params.get("host"), Some("dsn-host.internal"));
    }

    #[test]
    fn merge_dsn_params_explicit_value_wins_over_the_dsn_file() {
        let params = merge_dsn_params::<MockBackend>("DSN=prod;Host=explicit.internal", |_| {
            vec![("Host".into(), "dsn-host.internal".into())]
        })
        .unwrap();

        assert_eq!(params.get("host"), Some("explicit.internal"));
    }

    #[test]
    fn merge_dsn_params_explicit_credentials_win_over_the_dsn_file() {
        let params = merge_dsn_params::<MockBackend>("DSN=prod;UID=alice;PWD=alicepw", |_| {
            vec![
                ("UID".into(), "admin".into()),
                ("PWD".into(), "hunter2".into()),
            ]
        })
        .unwrap();

        assert_eq!(params.get("uid"), Some("alice"));
        assert_eq!(params.get("pwd"), Some("alicepw"));
    }

    #[test]
    fn merge_dsn_params_dsn_value_containing_a_semicolon_cannot_inject_keywords() {
        // A `;` inside a DSN value must stay part of that value. Unquoted, it
        // would terminate the segment and everything after it would parse as
        // further keywords.
        let params = merge_dsn_params::<MockBackend>("DSN=prod;Host=explicit.internal", |_| {
            vec![(
                "Description".into(),
                "x;UID=admin;PWD=hunter2;Host=attacker.example.com".into(),
            )]
        })
        .unwrap();

        assert_eq!(
            params.get("description"),
            Some("x;UID=admin;PWD=hunter2;Host=attacker.example.com")
        );
        assert_eq!(params.get("host"), Some("explicit.internal"));
        assert_eq!(params.get("uid"), None);
        assert_eq!(params.get("pwd"), None);
    }

    #[test]
    fn merge_dsn_params_dsn_value_containing_an_equals_sign_survives_the_round_trip() {
        let params = merge_dsn_params::<MockBackend>("DSN=prod", |_| {
            vec![("Token".into(), "abc=def==".into())]
        })
        .unwrap();

        assert_eq!(params.get("token"), Some("abc=def=="));
    }

    #[test]
    fn merge_dsn_params_seeds_the_backend_declared_sensitive_keywords() {
        use crate::test_utils::MockAltBackend;

        // `wallet` matches none of core's substring markers, so a redaction
        // here can only have come from the backend's own declaration.
        let params =
            merge_dsn_params::<MockAltBackend>("Host=explicit.internal;Wallet=abc123", |_| vec![])
                .unwrap();
        let debug_str = format!("{params:?}");
        assert!(
            !debug_str.contains("abc123"),
            "backend-declared keyword must be redacted, got: {debug_str}"
        );
        assert!(debug_str.contains("explicit.internal"));
    }

    #[test]
    fn merge_dsn_params_leaves_a_backend_declaring_nothing_with_the_built_in_markers() {
        let params = merge_dsn_params::<MockBackend>(
            "Host=explicit.internal;Wallet=abc123;Pwd=s3cr3t",
            |_| vec![],
        )
        .unwrap();
        let debug_str = format!("{params:?}");
        // MockBackend declares none, so `wallet` is visible ...
        assert!(debug_str.contains("abc123"), "got: {debug_str}");
        // ... but core's own safeguard still covers the obvious one.
        assert!(!debug_str.contains("s3cr3t"), "got: {debug_str}");
    }

    #[test]
    fn merge_dsn_params_without_a_dsn_key_never_consults_the_resolver() {
        let params = merge_dsn_params::<MockBackend>("Host=explicit.internal", |_| {
            panic!("resolver must not be called when the connection string has no DSN=")
        })
        .unwrap();

        assert_eq!(params.get("host"), Some("explicit.internal"));
    }

    // -----------------------------------------------------------------------
    // The *DriverCompletion* gate
    //
    // `SQL_DRIVER_NOPROMPT` is the spec's instruction that the driver must not
    // prompt. Core enforces it by withholding the prompter, so a backend has
    // nothing to call and cannot forget the rule at a call site.
    // -----------------------------------------------------------------------

    /// Connect with `MockPrompterBackend` and report what `Backend::connect`
    /// saw: whether a prompter arrived, and the URL it was driven with.
    unsafe fn prompter_seen_by<B, F>(connect: F) -> (bool, Option<String>)
    where
        B: Backend<Connection = crate::test_utils::MockPrompterConnection>,
        F: FnOnce(*mut c_void),
    {
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<B>(HandleType::Env as i16, std::ptr::null_mut(), &mut env);
            let mut conn: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<B>(HandleType::Dbc as i16, env, &mut conn);

            connect(conn);

            let seen = crate::test_utils::with_handle::<B, ConnectionHandle<B>, _>(conn, |c| {
                let c = c.connection.as_ref().expect("connected");
                (c.saw_prompter, c.presented.clone())
            });

            let _ = sql_disconnect::<B>(conn);
            let _ = sql_free_handle::<B>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<B>(HandleType::Env as i16, env);
            seen
        }
    }

    /// `SQLDriverConnectW` with the given raw *DriverCompletion*.
    unsafe fn prompter_seen_by_driver_connect(completion: u16) -> (bool, Option<String>) {
        unsafe {
            prompter_seen_by::<crate::test_utils::MockPrompterBackend, _>(|conn| {
                let wide: Vec<u16> = "Host=localhost".encode_utf16().collect();
                assert_eq!(
                    sql_driver_connect_w::<crate::test_utils::MockPrompterBackend>(
                        conn,
                        std::ptr::null_mut(),
                        wide.as_ptr(),
                        wide.len() as i16,
                        std::ptr::null_mut(),
                        0,
                        std::ptr::null_mut(),
                        completion,
                    ),
                    SqlReturn::SUCCESS,
                );
            })
        }
    }

    #[test]
    fn sql_driver_noprompt_withholds_the_prompter_even_though_the_backend_has_one() {
        // The point of the whole mechanism. `MockPrompterBackend::prompter`
        // returns `Some` unconditionally, so a `None` here can only be core's
        // gate.
        unsafe {
            assert_eq!(
                prompter_seen_by_driver_connect(DriverConnectOption::NoPrompt as u16),
                (false, None),
            );
        }
    }

    #[test]
    fn the_three_prompting_completions_pass_the_prompter_through() {
        // `SQL_DRIVER_PROMPT`, `SQL_DRIVER_COMPLETE` and
        // `SQL_DRIVER_COMPLETE_REQUIRED` all permit the driver to prompt; only
        // `SQL_DRIVER_NOPROMPT` forbids it.
        for completion in [
            DriverConnectOption::Complete,
            DriverConnectOption::Prompt,
            DriverConnectOption::CompleteRequired,
        ] {
            unsafe {
                assert_eq!(
                    prompter_seen_by_driver_connect(completion as u16),
                    (true, Some("https://example.invalid/oauth".to_owned())),
                    "{completion:?} must permit prompting",
                );
            }
        }
    }

    #[test]
    fn an_unrecognised_driver_completion_is_accepted_and_permits_prompting() {
        // `HY110` carries `(DM)` on both of its clauses, so the check is the
        // Driver Manager's and core adds none. Reaching the driver at all means
        // no DM validated it; the permissive reading is the one that does not
        // silently disable interactive authentication.
        unsafe {
            assert_eq!(
                prompter_seen_by_driver_connect(99),
                (true, Some("https://example.invalid/oauth".to_owned())),
            );
        }
    }

    // `sql_connect_w` always resolves the DSN through the installer library, so
    // this is the one entry point of the three that Miri cannot reach: it
    // interprets rather than links, and `SQLGetPrivateProfileStringW` is a
    // foreign function it refuses outright. The gate itself is pure Rust with no
    // `unsafe` for Miri to check, and the other two entry points cover the same
    // `prompter_for` call under Miri, so nothing is lost.
    #[cfg_attr(miri, ignore = "calls the foreign SQLGetPrivateProfileStringW")]
    #[test]
    fn sql_connect_has_no_completion_argument_and_so_permits_prompting() {
        // `SQLConnect` is the DSN path — `isql` and Excel reach it, and those
        // are the likeliest interactive callers. Reading the absent argument as
        // `SQL_DRIVER_NOPROMPT` would lock DSN connections out of interactive
        // authentication entirely.
        unsafe {
            let seen = prompter_seen_by::<crate::test_utils::MockPrompterBackend, _>(|conn| {
                let dsn: Vec<u16> = "nonexistent-dsn".encode_utf16().collect();
                assert_eq!(
                    sql_connect_w::<crate::test_utils::MockPrompterBackend>(
                        conn,
                        dsn.as_ptr(),
                        dsn.len() as i16,
                        std::ptr::null(),
                        0,
                        std::ptr::null(),
                        0,
                    ),
                    SqlReturn::SUCCESS,
                );
            });
            assert_eq!(
                seen,
                (true, Some("https://example.invalid/oauth".to_owned()))
            );
        }
    }

    /// The DSN name reaches the backend on the `SQLConnect` path.
    ///
    /// `read_dsn_keys` enumerates the odbc.ini section, and a section lists the
    /// keywords inside it rather than its own heading, so the DSN name is the
    /// one connection parameter that is *not* among the keys it returns. Until
    /// core inserted it, `ConnectParams::dsn()` was `None` here while being
    /// `Some` for the same DSN through `SQLDriverConnectW` — and the spec makes
    /// this the authoritative source for `SQL_DATA_SOURCE_NAME`, which "is the
    /// value of the *ServerName* argument in SQLConnect".
    ///
    /// A nonexistent DSN is deliberate: it proves the name arrives on its own
    /// rather than by being echoed back out of a section that happened to
    /// contain it.
    #[cfg_attr(miri, ignore = "calls the foreign SQLGetPrivateProfileStringW")]
    #[test]
    fn sql_connect_hands_the_backend_the_dsn_name_it_connected_with() {
        type B = crate::test_utils::MockNoQueryTimeoutBackend;
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<B>(HandleType::Env as i16, std::ptr::null_mut(), &mut env);
            let mut conn: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<B>(HandleType::Dbc as i16, env, &mut conn);

            let dsn: Vec<u16> = "nonexistent-dsn".encode_utf16().collect();
            assert_eq!(
                sql_connect_w::<B>(
                    conn,
                    dsn.as_ptr(),
                    dsn.len() as i16,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    0,
                ),
                SqlReturn::SUCCESS,
            );

            crate::test_utils::with_handle::<B, ConnectionHandle<B>, _>(conn, |c| {
                assert_eq!(
                    c.connection
                        .as_ref()
                        .expect("connected")
                        .seen_dsn
                        .as_deref(),
                    Some("nonexistent-dsn"),
                    "SQLConnect's ServerName argument never reached the backend",
                );
            });

            let _ = sql_disconnect::<B>(conn);
            let _ = sql_free_handle::<B>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<B>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn sql_browse_connect_has_no_completion_argument_and_so_permits_prompting() {
        // Same reasoning as `SQLConnect`, and the same entry point's
        // `B::connect`. `MockPrompterBackend` declares no
        // `browse_connect_attrs`, so the first call has everything it needs and
        // connects immediately.
        unsafe {
            let seen = prompter_seen_by::<crate::test_utils::MockPrompterBackend, _>(|conn| {
                let wide: Vec<u16> = "Host=localhost".encode_utf16().collect();
                assert_eq!(
                    sql_browse_connect_w::<crate::test_utils::MockPrompterBackend>(
                        conn,
                        wide.as_ptr(),
                        wide.len() as i16,
                        std::ptr::null_mut(),
                        0,
                        std::ptr::null_mut(),
                    ),
                    SqlReturn::SUCCESS,
                );
            });
            assert_eq!(
                seen,
                (true, Some("https://example.invalid/oauth".to_owned()))
            );
        }
    }

    #[test]
    fn a_backend_that_declares_no_prompter_never_receives_one() {
        // The defaulted `Backend::prompter` is what keeps every existing driver
        // compiling and behaving exactly as before.
        assert!(MockBackend::prompter().is_none());
    }

    #[test]
    fn driver_connect_and_disconnect() {
        unsafe {
            let (env, conn) = alloc_env_and_conn();

            let input = "Host=localhost;Port=8080;Database=test;User=me";
            let wide: Vec<u16> = input.encode_utf16().collect();

            let mut out_buf = [0u16; 128];
            let mut out_len: i16 = 0;

            let ret = sql_driver_connect_w::<MockBackend>(
                conn,
                std::ptr::null_mut(),
                wide.as_ptr(),
                wide.len() as i16,
                out_buf.as_mut_ptr(),
                128,
                &mut out_len,
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(out_len as usize, wide.len());

            // Verify connection is established by disconnecting successfully
            let ret = sql_disconnect::<MockBackend>(conn);
            assert_eq!(ret, SqlReturn::SUCCESS);

            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn driver_connect_output_truncation() {
        unsafe {
            let (env, conn) = alloc_env_and_conn();

            let input = "Host=localhost;Port=8080;Database=test;User=me";
            let wide: Vec<u16> = input.encode_utf16().collect();

            let mut out_buf = [0u16; 6]; // small buffer: room for 5 chars + null
            let mut out_len: i16 = 0;

            let ret = sql_driver_connect_w::<MockBackend>(
                conn,
                std::ptr::null_mut(),
                wide.as_ptr(),
                wide.len() as i16,
                out_buf.as_mut_ptr(),
                6,
                &mut out_len,
                0,
            );
            // Always returns SUCCESS: returning SUCCESS_WITH_INFO for
            // truncated output strings triggers a crash in the Windows DM.
            assert_eq!(ret, SqlReturn::SUCCESS);
            // out_len still reports the full length needed
            assert_eq!(out_len as usize, wide.len());

            let _ = sql_disconnect::<MockBackend>(conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn disconnect_when_not_connected_returns_error() {
        unsafe {
            let (env, conn) = alloc_env_and_conn();

            // Spec 08003: Disconnect without connecting should return ERROR.
            let ret = sql_disconnect::<MockBackend>(conn);
            assert_eq!(ret, SqlReturn::ERROR);

            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn driver_connect_null_output_buffer() {
        unsafe {
            let (env, conn) = alloc_env_and_conn();

            let input = "Host=localhost;Port=8080;Database=test;User=me";
            let wide: Vec<u16> = input.encode_utf16().collect();

            let mut out_len: i16 = 0;

            let ret = sql_driver_connect_w::<MockBackend>(
                conn,
                std::ptr::null_mut(),
                wide.as_ptr(),
                wide.len() as i16,
                std::ptr::null_mut(),
                0,
                &mut out_len,
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(out_len as usize, wide.len());

            let _ = sql_disconnect::<MockBackend>(conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    /// Helper: connect a handle using a valid connection string.
    unsafe fn connect_handle(conn: *mut c_void) -> SqlReturn {
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

    #[test]
    fn connect_when_already_connected_returns_error() {
        // Spec 08002: Connection name in use.
        unsafe {
            let (env, conn) = alloc_env_and_conn();

            let ret = connect_handle(conn);
            assert_eq!(ret, SqlReturn::SUCCESS);

            // Second connect should fail with 08002.
            let ret = connect_handle(conn);
            assert_eq!(ret, SqlReturn::ERROR);

            let _ = sql_disconnect::<MockBackend>(conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn connect_negative_string_length_returns_error() {
        // Spec HY090: StringLength1 < 0 and != SQL_NTS (-3).
        unsafe {
            let (env, conn) = alloc_env_and_conn();
            let input = "Database=test";
            let wide: Vec<u16> = input.encode_utf16().collect();

            let ret = sql_driver_connect_w::<MockBackend>(
                conn,
                std::ptr::null_mut(),
                wide.as_ptr(),
                -5, // invalid: not SQL_NTS (-3) and not >= 0
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                0,
            );
            assert_eq!(ret, SqlReturn::ERROR);

            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn connect_sql_nts_string_length_succeeds() {
        // SQL_NTS (-3) is valid for null-terminated strings.
        unsafe {
            let (env, conn) = alloc_env_and_conn();
            let input = "Host=localhost;Port=8080;Database=test;User=me\0";
            let wide: Vec<u16> = input.encode_utf16().collect();

            let ret = sql_driver_connect_w::<MockBackend>(
                conn,
                std::ptr::null_mut(),
                wide.as_ptr(),
                SQL_NTS as i16,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let _ = sql_disconnect::<MockBackend>(conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn driver_connect_null_connection_string_returns_hy009() {
        // Spec HY009: InConnectionString must not be null.
        unsafe {
            let (env, conn) = alloc_env_and_conn();

            let ret = sql_driver_connect_w::<MockBackend>(
                conn,
                std::ptr::null_mut(),
                std::ptr::null(), // null connection string
                0,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                0,
            );
            assert_eq!(ret, SqlReturn::ERROR);

            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn connect_negative_buffer_length_returns_error() {
        // Spec HY090: BufferLength < 0.
        unsafe {
            let (env, conn) = alloc_env_and_conn();
            let input = "Database=test";
            let wide: Vec<u16> = input.encode_utf16().collect();
            let mut out_buf = [0u16; 128];
            let mut out_len: i16 = 0;

            let ret = sql_driver_connect_w::<MockBackend>(
                conn,
                std::ptr::null_mut(),
                wide.as_ptr(),
                wide.len() as i16,
                out_buf.as_mut_ptr(),
                -1, // invalid buffer length
                &mut out_len,
                0,
            );
            assert_eq!(ret, SqlReturn::ERROR);

            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn disconnect_frees_statements() {
        // Spec: "the driver frees those statements" on successful disconnect.
        unsafe {
            let (env, conn) = alloc_env_and_conn();

            let ret = connect_handle(conn);
            assert_eq!(ret, SqlReturn::SUCCESS);

            // Allocate a statement on this connection.
            let mut stmt: *mut c_void = std::ptr::null_mut();
            let ret = sql_alloc_handle::<MockBackend>(HandleType::Stmt as i16, conn, &mut stmt);
            assert_eq!(ret, SqlReturn::SUCCESS);

            // Disconnect should succeed and free the statement.
            let ret = sql_disconnect::<MockBackend>(conn);
            assert_eq!(ret, SqlReturn::SUCCESS);

            // Connection handle should now have no statements.
            assert!(
                crate::handles::registry::registry()
                    .children_of(conn)
                    .is_empty()
            );

            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn disconnect_then_free_succeeds() {
        // Full lifecycle: connect → disconnect → free.
        unsafe {
            let (env, conn) = alloc_env_and_conn();

            let ret = connect_handle(conn);
            assert_eq!(ret, SqlReturn::SUCCESS);

            let ret = sql_disconnect::<MockBackend>(conn);
            assert_eq!(ret, SqlReturn::SUCCESS);

            // Free should now succeed (connection is None, no statements).
            let ret = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            assert_eq!(ret, SqlReturn::SUCCESS);

            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn connect_w_invalid_name_length_returns_hy090() {
        // Spec HY090: NameLength1 < 0 and != SQL_NTS (-3).
        unsafe {
            let (env, conn) = alloc_env_and_conn();
            let server_name: Vec<u16> = "testdsn".encode_utf16().collect();

            let ret = sql_connect_w::<MockBackend>(
                conn,
                server_name.as_ptr(),
                -1i16, // invalid: negative and not SQL_NTS (-3)
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
            );
            assert_eq!(ret, SqlReturn::ERROR);

            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    // -----------------------------------------------------------------------
    // SQLNativeSqlW
    // -----------------------------------------------------------------------

    #[test]
    fn native_sql_w_translates_escapes() {
        // MockBackend uses the default EscapeDialect::ansi_default():
        // {d '..'} -> DATE '..'.
        unsafe {
            let (env, conn) = alloc_env_and_conn();
            assert_eq!(connect_handle(conn), SqlReturn::SUCCESS);

            let sql = "SELECT * FROM t WHERE d = {d '2020-01-01'}";
            let expected = "SELECT * FROM t WHERE d = DATE '2020-01-01'";
            let in_wide: Vec<u16> = sql.encode_utf16().collect();
            let mut out_buf = [0u16; 128];
            let mut out_len: i32 = 0;

            let ret = sql_native_sql_w::<MockBackend>(
                conn,
                in_wide.as_ptr(),
                in_wide.len() as i32,
                out_buf.as_mut_ptr(),
                128,
                &mut out_len,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            let expected_wide: Vec<u16> = expected.encode_utf16().collect();
            assert_eq!(out_len as usize, expected_wide.len());
            let result = String::from_utf16_lossy(&out_buf[..out_len as usize]);
            assert_eq!(result, expected);

            let _ = sql_disconnect::<MockBackend>(conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn native_sql_w_call_escape_fails_with_hy000() {
        // SQLNativeSql's diagnostics table has no 42000/HYC00 entries, so a
        // `{call}` escape (unsupported by translate_escapes) must surface as
        // HY000 here rather than the HYC00 that translate_escapes itself uses.
        use crate::ffi::diag::sql_get_diag_rec_w;

        unsafe {
            let (env, conn) = alloc_env_and_conn();
            assert_eq!(connect_handle(conn), SqlReturn::SUCCESS);

            let sql = "{call foo()}";
            let in_wide: Vec<u16> = sql.encode_utf16().collect();
            let mut out_buf = [0u16; 128];
            let mut out_len: i32 = 0;

            let ret = sql_native_sql_w::<MockBackend>(
                conn,
                in_wide.as_ptr(),
                in_wide.len() as i32,
                out_buf.as_mut_ptr(),
                128,
                &mut out_len,
            );
            assert_eq!(ret, SqlReturn::ERROR);

            let mut state = [0u16; 6];
            let mut native_err: i32 = 0;
            let mut msg_buf = [0u16; 256];
            let mut msg_len: i16 = 0;
            let diag_ret = sql_get_diag_rec_w::<MockBackend>(
                HandleType::Dbc as i16,
                conn,
                1,
                state.as_mut_ptr(),
                &mut native_err,
                msg_buf.as_mut_ptr(),
                msg_buf.len() as i16,
                &mut msg_len,
            );
            assert_eq!(diag_ret, SqlReturn::SUCCESS);
            let sqlstate = String::from_utf16_lossy(&state[..5]);
            assert_eq!(sqlstate, "HY000");

            let _ = sql_disconnect::<MockBackend>(conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn native_sql_w_truncates_and_returns_success_with_info() {
        unsafe {
            let (env, conn) = alloc_env_and_conn();
            assert_eq!(connect_handle(conn), SqlReturn::SUCCESS);

            let sql = "SELECT 1";
            let in_wide: Vec<u16> = sql.encode_utf16().collect();
            let mut out_buf = [0u16; 4]; // fits only 3 chars + null
            let mut out_len: i32 = 0;

            let ret = sql_native_sql_w::<MockBackend>(
                conn,
                in_wide.as_ptr(),
                in_wide.len() as i32,
                out_buf.as_mut_ptr(),
                4,
                &mut out_len,
            );
            assert_eq!(ret, SqlReturn::SUCCESS_WITH_INFO);
            // out_len always reports the full length
            assert_eq!(out_len as usize, in_wide.len());
            // output is null-terminated after 3 chars
            assert_eq!(out_buf[3], 0);

            let _ = sql_disconnect::<MockBackend>(conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    /// Spec 01004: "The buffer \*OutStatementText was not large enough to return
    /// the entire SQL string, so truncation occurred. (Function returns
    /// SQL_SUCCESS_WITH_INFO.)" A buffer the application declared empty is the
    /// extreme case of not large enough, and it used to report plain
    /// SQL_SUCCESS with no diagnostic at all.
    #[test]
    fn native_sql_w_zero_length_buffer_reports_truncation() {
        use crate::ffi::diag::sql_get_diag_rec_w;

        unsafe {
            let (env, conn) = alloc_env_and_conn();
            assert_eq!(connect_handle(conn), SqlReturn::SUCCESS);

            let sql = "SELECT 1";
            let in_wide: Vec<u16> = sql.encode_utf16().collect();
            let mut out_buf = [0u16; 1];
            let mut out_len: i32 = 0;

            let ret = sql_native_sql_w::<MockBackend>(
                conn,
                in_wide.as_ptr(),
                in_wide.len() as i32,
                out_buf.as_mut_ptr(),
                0, // a real buffer the application declared empty
                &mut out_len,
            );
            assert_eq!(ret, SqlReturn::SUCCESS_WITH_INFO);
            assert_eq!(
                out_len as usize,
                in_wide.len(),
                "the required length must still be reported",
            );
            assert_eq!(
                out_buf[0], 0,
                "nothing may be written into a zero-length buffer",
            );

            let mut state = [0u16; 6];
            let mut native_err: i32 = 0;
            let mut msg_buf = [0u16; 256];
            let mut msg_len: i16 = 0;
            assert_eq!(
                sql_get_diag_rec_w::<MockBackend>(
                    HandleType::Dbc as i16,
                    conn,
                    1,
                    state.as_mut_ptr(),
                    &mut native_err,
                    msg_buf.as_mut_ptr(),
                    msg_buf.len() as i16,
                    &mut msg_len,
                ),
                SqlReturn::SUCCESS,
                "SQL_SUCCESS_WITH_INFO with no record leaves the application \
                 unable to tell truncation from anything else",
            );
            assert_eq!(String::from_utf16_lossy(&state[..5]), "01004");

            let _ = sql_disconnect::<MockBackend>(conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    /// Nothing was truncated when there was nothing to write, so this stays
    /// plain SQL_SUCCESS. Without it, reporting 01004 for every zero-length
    /// buffer would pass the test above.
    #[test]
    fn native_sql_w_zero_length_buffer_and_empty_statement_is_plain_success() {
        unsafe {
            let (env, conn) = alloc_env_and_conn();
            assert_eq!(connect_handle(conn), SqlReturn::SUCCESS);

            let in_wide: Vec<u16> = vec![0u16];
            let mut out_buf = [0u16; 1];
            let mut out_len: i32 = 0;

            let ret = sql_native_sql_w::<MockBackend>(
                conn,
                in_wide.as_ptr(),
                0, // an empty statement
                out_buf.as_mut_ptr(),
                0,
                &mut out_len,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(out_len, 0);

            let _ = sql_disconnect::<MockBackend>(conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn native_sql_w_null_out_ptr_still_reports_length() {
        unsafe {
            let (env, conn) = alloc_env_and_conn();
            assert_eq!(connect_handle(conn), SqlReturn::SUCCESS);

            let sql = "SELECT 1";
            let in_wide: Vec<u16> = sql.encode_utf16().collect();
            let mut out_len: i32 = 0;

            let ret = sql_native_sql_w::<MockBackend>(
                conn,
                in_wide.as_ptr(),
                in_wide.len() as i32,
                std::ptr::null_mut(), // no output buffer
                0,
                &mut out_len,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(out_len as usize, in_wide.len());

            let _ = sql_disconnect::<MockBackend>(conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn native_sql_w_not_connected_returns_error() {
        unsafe {
            let (env, conn) = alloc_env_and_conn();

            let sql = "SELECT 1";
            let in_wide: Vec<u16> = sql.encode_utf16().collect();
            let ret = sql_native_sql_w::<MockBackend>(
                conn,
                in_wide.as_ptr(),
                in_wide.len() as i32,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::ERROR);

            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn native_sql_w_null_in_sql_returns_error() {
        unsafe {
            let (env, conn) = alloc_env_and_conn();
            assert_eq!(connect_handle(conn), SqlReturn::SUCCESS);

            let ret = sql_native_sql_w::<MockBackend>(
                conn,
                std::ptr::null(), // null input
                0,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::ERROR);

            let _ = sql_disconnect::<MockBackend>(conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn native_sql_w_null_handle_returns_invalid() {
        unsafe {
            let ret = sql_native_sql_w::<MockBackend>(
                std::ptr::null_mut(),
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::INVALID_HANDLE);
        }
    }

    #[test]
    fn free_connection_while_connected_fails() {
        // Spec HY010: Must disconnect before freeing.
        unsafe {
            let (env, conn) = alloc_env_and_conn();

            let ret = connect_handle(conn);
            assert_eq!(ret, SqlReturn::SUCCESS);

            // Free without disconnect should fail.
            let ret = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            assert_eq!(ret, SqlReturn::ERROR);

            // Clean up properly.
            let _ = sql_disconnect::<MockBackend>(conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn browse_connect_null_handle() {
        // Null handle must return INVALID_HANDLE.
        unsafe {
            let ret = sql_browse_connect_w::<MockBackend>(
                std::ptr::null_mut(),
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::INVALID_HANDLE);
        }
    }

    #[test]
    fn browse_connect_already_connected() {
        // Spec 08002: Connection already in use.
        unsafe {
            let (env, conn) = alloc_env_and_conn();

            let ret = connect_handle(conn);
            assert_eq!(ret, SqlReturn::SUCCESS);

            let input = "Host=localhost";
            let wide: Vec<u16> = input.encode_utf16().collect();
            let ret = sql_browse_connect_w::<MockBackend>(
                conn,
                wide.as_ptr(),
                wide.len() as i16,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::ERROR);

            let _ = sql_disconnect::<MockBackend>(conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn browse_connect_null_input_string() {
        // Spec HY009: InConnectionString must not be null.
        unsafe {
            let (env, conn) = alloc_env_and_conn();

            let ret = sql_browse_connect_w::<MockBackend>(
                conn,
                std::ptr::null(), // null input string
                0,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::ERROR);

            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn browse_connect_success_with_no_required_attrs() {
        // MockBackend::browse_connect_attrs() returns &[] so any string should connect directly.
        unsafe {
            let (env, conn) = alloc_env_and_conn();

            let input = "Host=localhost;Port=8080;User=me";
            let wide: Vec<u16> = input.encode_utf16().collect();
            let mut out_buf = [0u16; 256];
            let mut out_len: i16 = 0;

            let ret = sql_browse_connect_w::<MockBackend>(
                conn,
                wide.as_ptr(),
                wide.len() as i16,
                out_buf.as_mut_ptr(),
                256,
                &mut out_len,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert!(out_len > 0);

            let _ = sql_disconnect::<MockBackend>(conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    /// Spec (`SQLDisconnect`, Comments): "If an application calls
    /// **SQLDisconnect** after **SQLBrowseConnect** returns SQL_NEED_DATA and
    /// before it returns a different return code, the driver cancels the
    /// connection browsing process and returns the connection to an unconnected
    /// state." `SQLBrowseConnect` states the other half: "an application must
    /// use **SQLDisconnect** to cancel the browse process."
    ///
    /// `handle.connection` is `None` for the whole of a browse, so the `08003`
    /// guard used to answer this call and an application had no way to abandon
    /// a browse at all.
    #[test]
    fn disconnect_during_a_browse_cancels_it_instead_of_reporting_08003() {
        unsafe {
            let (env, conn) = alloc_env_and_conn_for::<MockBrowseBackend>();

            let wide: Vec<u16> = "Host=localhost".encode_utf16().collect();
            assert_eq!(
                sql_browse_connect_w::<MockBrowseBackend>(
                    conn,
                    wide.as_ptr(),
                    wide.len() as i16,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                ),
                SqlReturn::NEED_DATA,
                "precondition: the backend requires UID, so the first call asks for it",
            );

            assert_eq!(
                sql_disconnect::<MockBrowseBackend>(conn),
                SqlReturn::SUCCESS,
                "SQLDisconnect must cancel the browse, not report 08003",
            );

            crate::test_utils::with_handle::<
                MockBrowseBackend,
                ConnectionHandle<MockBrowseBackend>,
                _,
            >(conn, |h| {
                assert!(
                    h.browse_request.is_none(),
                    "the accumulated browse attributes must not survive the cancel",
                );
            });

            let _ = sql_free_handle::<MockBrowseBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBrowseBackend>(HandleType::Env as i16, env);
        }
    }

    /// Every connect entry point must name what it was asked to connect *to*,
    /// not just the handle it was called on. An entry log carrying only the
    /// handle shows which call happened and not what it asked for, which is
    /// precisely what is needed when a connection fails against a DSN nobody
    /// can reproduce.
    ///
    /// The parsed `ConnectParams`, never the raw string: its `Debug` redacts
    /// credential keywords and the raw string does not.
    #[test]
    fn every_connect_entry_point_logs_its_redacted_params() {
        let source = include_str!("connect.rs");
        for name in ["SQLDriverConnectW", "SQLConnectW", "SQLBrowseConnectW"] {
            assert!(
                source.contains(&format!("\"{name}: params = {{:?}}\"")),
                "{name} does not log the ConnectParams it parsed",
            );
        }
    }

    /// An abandoned browse must not contaminate a later one.
    /// `SQLBrowseConnectW` clears the accumulated attributes on its own success,
    /// but an ordinary connect used to leave them in place, so the next browse
    /// on that handle started from the attributes of the one before it.
    #[test]
    fn sql_driver_connect_clears_abandoned_browse_state() {
        unsafe {
            let (env, conn) = alloc_env_and_conn_for::<MockBrowseBackend>();

            let wide: Vec<u16> = "Host=localhost".encode_utf16().collect();
            assert_eq!(
                sql_browse_connect_w::<MockBrowseBackend>(
                    conn,
                    wide.as_ptr(),
                    wide.len() as i16,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                ),
                SqlReturn::NEED_DATA,
                "precondition: a browse is in progress",
            );

            assert_eq!(
                sql_driver_connect_w::<MockBrowseBackend>(
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

            crate::test_utils::with_handle::<
                MockBrowseBackend,
                ConnectionHandle<MockBrowseBackend>,
                _,
            >(conn, |h| {
                assert!(
                    h.browse_request.is_none(),
                    "the abandoned browse attributes survived the connect",
                );
            });

            let _ = sql_disconnect::<MockBrowseBackend>(conn);
            let _ = sql_free_handle::<MockBrowseBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBrowseBackend>(HandleType::Env as i16, env);
        }
    }

    /// The same rule for the DSN entry point. Ignored under Miri for the reason
    /// `sql_connect_has_no_completion_argument_and_so_permits_prompting` is:
    /// `SQLConnectW` always resolves the DSN through the installer library, and
    /// Miri interprets rather than links.
    #[cfg_attr(miri, ignore = "calls the foreign SQLGetPrivateProfileStringW")]
    #[test]
    fn sql_connect_clears_abandoned_browse_state() {
        unsafe {
            let (env, conn) = alloc_env_and_conn_for::<MockBrowseBackend>();

            let wide: Vec<u16> = "Host=localhost".encode_utf16().collect();
            assert_eq!(
                sql_browse_connect_w::<MockBrowseBackend>(
                    conn,
                    wide.as_ptr(),
                    wide.len() as i16,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                ),
                SqlReturn::NEED_DATA,
                "precondition: a browse is in progress",
            );

            let dsn: Vec<u16> = "nonexistent-dsn".encode_utf16().collect();
            assert_eq!(
                sql_connect_w::<MockBrowseBackend>(
                    conn,
                    dsn.as_ptr(),
                    dsn.len() as i16,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    0,
                ),
                SqlReturn::SUCCESS,
            );

            crate::test_utils::with_handle::<
                MockBrowseBackend,
                ConnectionHandle<MockBrowseBackend>,
                _,
            >(conn, |h| {
                assert!(
                    h.browse_request.is_none(),
                    "the abandoned browse attributes survived the connect",
                );
            });

            let _ = sql_disconnect::<MockBrowseBackend>(conn);
            let _ = sql_free_handle::<MockBrowseBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBrowseBackend>(HandleType::Env as i16, env);
        }
    }

    // -----------------------------------------------------------------------
    // The MAX_NTS_SCAN limit on SQL_NTS arguments
    //
    // Four call sites in this module resolve SQL_NTS, and all four used to take
    // the scanned prefix when the scan reached the bound. A truncated
    // connection string is not a syntax error the way a truncated statement is:
    // `ConnectParams` parses whatever it is given, so the connect proceeded
    // against a *different data source* than the application named.
    // -----------------------------------------------------------------------

    /// The `HY090` a scan overrun posts on a connection handle.
    unsafe fn dbc_sqlstate(conn: *mut c_void) -> String {
        use crate::ffi::diag::sql_get_diag_rec_w;
        let mut state = [0u16; 6];
        let mut native_err: i32 = 0;
        let mut msg_buf = [0u16; 256];
        let mut msg_len: i16 = 0;
        // SAFETY: every buffer is a live local of the declared size.
        let ret = unsafe {
            sql_get_diag_rec_w::<MockBackend>(
                HandleType::Dbc as i16,
                conn,
                1,
                state.as_mut_ptr(),
                &mut native_err,
                msg_buf.as_mut_ptr(),
                msg_buf.len() as i16,
                &mut msg_len,
            )
        };
        assert_eq!(ret, SqlReturn::SUCCESS, "no diagnostic record was posted");
        String::from_utf16_lossy(&state[..5])
    }

    /// A buffer of exactly `MAX_NTS_SCAN` units with no terminator: the scan
    /// must stop on its last unit, so an over-read is a heap overflow Miri sees
    /// rather than a longer prefix.
    fn unterminated_at_the_cap() -> Vec<u16> {
        vec![b'a' as u16; crate::utf16::MAX_NTS_SCAN]
    }

    #[test]
    fn driver_connect_refuses_an_nts_connection_string_that_runs_to_the_scan_cap() {
        unsafe {
            let (env, conn) = alloc_env_and_conn();
            let wide = unterminated_at_the_cap();

            assert_eq!(
                sql_driver_connect_w::<MockBackend>(
                    conn,
                    std::ptr::null_mut(),
                    wide.as_ptr(),
                    SQL_NTS as i16,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    0,
                ),
                SqlReturn::ERROR,
            );
            assert_eq!(
                dbc_sqlstate(conn),
                crate::types::sql_state::INVALID_STRING_OR_BUFFER_LENGTH
            );

            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn browse_connect_refuses_an_nts_connection_string_that_runs_to_the_scan_cap() {
        unsafe {
            let (env, conn) = alloc_env_and_conn();
            let wide = unterminated_at_the_cap();

            assert_eq!(
                sql_browse_connect_w::<MockBackend>(
                    conn,
                    wide.as_ptr(),
                    SQL_NTS as i16,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                ),
                SqlReturn::ERROR,
            );
            assert_eq!(
                dbc_sqlstate(conn),
                crate::types::sql_state::INVALID_STRING_OR_BUFFER_LENGTH
            );

            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn connect_refuses_an_nts_server_name_that_runs_to_the_scan_cap() {
        unsafe {
            let (env, conn) = alloc_env_and_conn();
            let wide = unterminated_at_the_cap();

            assert_eq!(
                sql_connect_w::<MockBackend>(
                    conn,
                    wide.as_ptr(),
                    SQL_NTS as i16,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    0,
                ),
                SqlReturn::ERROR,
            );
            assert_eq!(
                dbc_sqlstate(conn),
                crate::types::sql_state::INVALID_STRING_OR_BUFFER_LENGTH
            );

            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    /// The credential arguments, which were read inside `if let Ok(..)` and so
    /// discarded the error rather than propagating it. A swallowed `HY090` here
    /// connects with whatever credentials the DSN supplies, under a *UserName*
    /// the application believes it passed — a silent authorisation change, not a
    /// missing diagnostic. Delete either `?` in `sql_connect_w`'s credential
    /// block and this test fails.
    #[test]
    fn connect_refuses_an_nts_credential_that_runs_to_the_scan_cap() {
        unsafe {
            let dsn: Vec<u16> = "prod\0".encode_utf16().collect();
            let wide = unterminated_at_the_cap();

            for (user, auth) in [
                (wide.as_ptr(), std::ptr::null()),
                (std::ptr::null(), wide.as_ptr()),
            ] {
                let (env, conn) = alloc_env_and_conn();
                assert_eq!(
                    sql_connect_w::<MockBackend>(
                        conn,
                        dsn.as_ptr(),
                        SQL_NTS as i16,
                        user,
                        SQL_NTS as i16,
                        auth,
                        SQL_NTS as i16,
                    ),
                    SqlReturn::ERROR,
                );
                assert_eq!(
                    dbc_sqlstate(conn),
                    crate::types::sql_state::INVALID_STRING_OR_BUFFER_LENGTH
                );

                let _ = sql_disconnect::<MockBackend>(conn);
                let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
                let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
            }
        }
    }

    #[test]
    fn native_sql_refuses_an_nts_statement_that_runs_to_the_scan_cap() {
        unsafe {
            let (env, conn) = alloc_env_and_conn();
            assert_eq!(connect_handle(conn), SqlReturn::SUCCESS);
            let wide = unterminated_at_the_cap();

            assert_eq!(
                sql_native_sql_w::<MockBackend>(
                    conn,
                    wide.as_ptr(),
                    SQL_NTS,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                ),
                SqlReturn::ERROR,
            );
            assert_eq!(
                dbc_sqlstate(conn),
                crate::types::sql_state::INVALID_STRING_OR_BUFFER_LENGTH
            );

            let _ = sql_disconnect::<MockBackend>(conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    /// A 100 000-character `SQL_NTS` statement is translated in full, and the
    /// length reported back proves it — `SQLNativeSql` is the one function in
    /// the crate that echoes an `SQL_NTS` argument's resolved length to the
    /// application, so it can distinguish "accepted" from "accepted a prefix".
    ///
    /// 100 000 is above `i16::MAX` (32 767), which `MAX_NTS_SCAN` used to be, so
    /// this call was `HY090` until the cap was raised. A literal rather than a
    /// fraction of `MAX_NTS_SCAN`, because a test written against the constant
    /// passes at every value of it.
    #[test]
    fn native_sql_translates_an_nts_statement_of_a_hundred_thousand_characters_in_full() {
        const STATEMENT_UNITS: usize = 100_000;

        unsafe {
            let (env, conn) = alloc_env_and_conn();
            assert_eq!(connect_handle(conn), SqlReturn::SUCCESS);

            let mut wide = vec![b'a' as u16; STATEMENT_UNITS + 1];
            wide[STATEMENT_UNITS] = 0;
            let mut out = vec![0u16; STATEMENT_UNITS + 1];
            let mut out_len: i32 = 0;

            assert_eq!(
                sql_native_sql_w::<MockBackend>(
                    conn,
                    wide.as_ptr(),
                    SQL_NTS,
                    out.as_mut_ptr(),
                    i32::try_from(out.len()).expect("fits in i32"),
                    &mut out_len,
                ),
                SqlReturn::SUCCESS,
            );
            assert_eq!(
                usize::try_from(out_len).expect("a length is not negative"),
                STATEMENT_UNITS,
                "the whole statement was translated, not the prefix a capped scan would take",
            );

            let _ = sql_disconnect::<MockBackend>(conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    /// The accepting side, for the whole family: a terminator in the last
    /// position the scan may read is a complete connection string, not an
    /// error. Without it, every test above is satisfied by refusing all long
    /// input.
    #[test]
    fn driver_connect_accepts_an_nts_string_terminated_at_the_last_scannable_position() {
        unsafe {
            let (env, conn) = alloc_env_and_conn();
            // A valid connection string, padded with a keyword long enough to
            // reach the cap, terminated on the last unit the scan may read.
            let head = "Host=localhost;Port=8080;Database=test;User=me;Pad=";
            let mut wide: Vec<u16> = head.encode_utf16().collect();
            wide.resize(crate::utf16::MAX_NTS_SCAN, b'x' as u16);
            wide[crate::utf16::MAX_NTS_SCAN - 1] = 0;

            assert_eq!(
                sql_driver_connect_w::<MockBackend>(
                    conn,
                    std::ptr::null_mut(),
                    wide.as_ptr(),
                    SQL_NTS as i16,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    0,
                ),
                SqlReturn::SUCCESS,
            );

            let _ = sql_disconnect::<MockBackend>(conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    /// The other side of the same line. A handle that never connected and never
    /// browsed is genuinely not open, and `08003` is still the answer — the
    /// browse carve-out must not swallow it.
    #[test]
    fn disconnect_with_no_connection_and_no_browse_is_still_08003() {
        use crate::ffi::diag::sql_get_diag_rec_w;

        unsafe {
            let (env, conn) = alloc_env_and_conn();

            assert_eq!(sql_disconnect::<MockBackend>(conn), SqlReturn::ERROR);

            let mut state = [0u16; 6];
            let mut native_err: i32 = 0;
            let mut msg_buf = [0u16; 256];
            let mut msg_len: i16 = 0;
            assert_eq!(
                sql_get_diag_rec_w::<MockBackend>(
                    HandleType::Dbc as i16,
                    conn,
                    1,
                    state.as_mut_ptr(),
                    &mut native_err,
                    msg_buf.as_mut_ptr(),
                    msg_buf.len() as i16,
                    &mut msg_len,
                ),
                SqlReturn::SUCCESS,
            );
            assert_eq!(String::from_utf16_lossy(&state[..5]), "08003");

            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }
}
