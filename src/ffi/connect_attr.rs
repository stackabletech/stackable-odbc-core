//! Generic implementations of SQLSetConnectAttrW and SQLGetConnectAttrW.

use std::borrow::Cow;
use std::ffi::c_void;

use odbc_sys::ConnectionAttribute;

use crate::backend::Backend;
use crate::errors::{IntoOdbc, OdbcError};
use crate::handles::ConnectionHandle;
use crate::panic::panic_safe;
use crate::types::{
    ConnectParams, SQL_AUTOCOMMIT_OFF, SQL_AUTOCOMMIT_ON, SQL_CD_FALSE, SQL_CD_TRUE, SQL_FALSE,
    SQL_NTS, SqlReturn, SqlState,
};
#[cfg(test)]
use crate::types::{SQL_TXN_READ_COMMITTED, SQL_TXN_REPEATABLE_READ, SQL_TXN_SERIALIZABLE};
use crate::utf16::{utf16_to_string, write_utf16};

// SQL_ATTR_ASYNC_ENABLE values
const SQL_ASYNC_ENABLE_OFF: usize = 0;
// SQL_ATTR_ACCESS_MODE values
const SQL_MODE_READ_WRITE: usize = 0;
const SQL_MODE_READ_ONLY: usize = 1;
// SQL_ATTR_TRACE values
const SQL_OPT_TRACE_OFF: usize = 0;
const SQL_OPT_TRACE_ON: usize = 1;
// SQL_ATTR_ODBC_CURSORS values
const SQL_CUR_USE_IF_NEEDED: usize = 0;
const SQL_CUR_USE_ODBC: usize = 1;
const SQL_CUR_USE_DRIVER: usize = 2;

/// Apply every connection attribute that was set before the connection was
/// open and has to reach the backend.
///
/// The ODBC spec lists both `SQL_ATTR_AUTOCOMMIT` and
/// `SQL_ATTR_TXN_ISOLATION` as settable either side of the connection, so a
/// value stored pre-connect must still be applied once the connection exists;
/// otherwise the application is told the setting took effect while the data
/// source runs at the old one.
///
/// Called by `SQLDriverConnectW`, `SQLConnectW` and `SQLBrowseConnectW` on the
/// success path. An error here fails the connect, and the caller tears the
/// connection down.
pub(crate) fn apply_pending_connect_attrs<B: Backend>(
    handle: &mut ConnectionHandle<B>,
) -> Result<(), OdbcError> {
    apply_pending_autocommit::<B>(handle)?;
    apply_pending_txn_isolation::<B>(handle)?;
    apply_pending_access_mode::<B>(handle)?;
    apply_pending_current_catalog::<B>(handle)
}

/// Copy the connection attributes `Backend::connect` may need into the
/// `ConnectParams` it is about to be given.
///
/// `SQL_ATTR_LOGIN_TIMEOUT` and `SQL_ATTR_CONNECTION_TIMEOUT` are set through
/// `SQLSetConnectAttr`, not through the connection string, so without this a
/// backend has no way to see them: `connect` receives only `ConnectParams`.
/// The login timeout in particular is useless anywhere else: the spec lists it
/// as settable "Before" only, because it bounds the very call that establishes
/// the connection.
///
/// Core does not enforce either one. `Backend::connect` is synchronous and
/// there is no cancel token before a connection exists, so a backend that wants
/// these honoured passes them to its own client library.
///
/// A value that does not fit in `u32` is dropped rather than truncated: both
/// attributes are `SQLUINTEGER`, so a larger one is a caller error, and
/// silently wrapping it to a small number would impose a timeout far shorter
/// than anything the application asked for.
pub(crate) fn carry_connect_timeouts<B: Backend>(
    handle: &ConnectionHandle<B>,
    params: &mut ConnectParams,
) {
    let read = |attr: i32| {
        handle
            .attrs
            .get(&attr)
            .copied()
            .and_then(|v| u32::try_from(v).ok())
    };
    let login = read(ConnectionAttribute::LOGIN_TIMEOUT.0);
    let connection = read(ConnectionAttribute::CONNECTION_TIMEOUT.0);
    if login.is_some() || connection.is_some() {
        tracing::debug!(
            "connect: login_timeout={:?}s, connection_timeout={:?}s",
            login,
            connection
        );
    }
    params.set_timeouts(login, connection);
}

/// Apply a `SQL_ATTR_ACCESS_MODE` value that was set before the connection was
/// open. See [`apply_pending_connect_attrs`].
///
/// The spec's attribute table marks this one "Either", with footnote [1]:
/// "SQL_ATTR_ACCESS_MODE and SQL_ATTR_CURRENT_CATALOG can be set before or
/// after connecting, depending on the driver. However, interoperable
/// applications set them before connecting because some drivers do not support
/// changing these after connecting." Setting it before is therefore the
/// *recommended* usage, which makes this the path that matters most.
fn apply_pending_access_mode<B: Backend>(
    handle: &mut ConnectionHandle<B>,
) -> Result<(), OdbcError> {
    let Some(&val) = handle.attrs.get(&ConnectionAttribute::ACCESS_MODE.0) else {
        return Ok(());
    };
    let Some(connection) = handle.connection.as_ref() else {
        return Ok(());
    };
    B::set_access_mode(connection, val == SQL_MODE_READ_ONLY).into_odbc()
}

/// Apply a `SQL_ATTR_CURRENT_CATALOG` value that was set before the connection
/// was open. See [`apply_pending_connect_attrs`].
fn apply_pending_current_catalog<B: Backend>(
    handle: &mut ConnectionHandle<B>,
) -> Result<(), OdbcError> {
    let Some(catalog) = handle
        .attr_strings
        .get(&ConnectionAttribute::CURRENT_CATALOG.0)
        .cloned()
    else {
        return Ok(());
    };
    let Some(connection) = handle.connection.as_ref() else {
        return Ok(());
    };
    B::set_current_catalog(connection, &catalog).into_odbc()
}

/// Apply a `SQL_ATTR_AUTOCOMMIT` value that was set before the connection was
/// open. See [`apply_pending_connect_attrs`].
fn apply_pending_autocommit<B: Backend>(handle: &mut ConnectionHandle<B>) -> Result<(), OdbcError> {
    let Some(&val) = handle.attrs.get(&ConnectionAttribute::AUTOCOMMIT.0) else {
        return Ok(());
    };
    let Some(connection) = handle.connection.as_ref() else {
        return Ok(());
    };
    B::set_autocommit(connection, val == SQL_AUTOCOMMIT_ON).into_odbc()
}

/// Apply a `SQL_ATTR_TXN_ISOLATION` value that was set before the connection
/// was open. See [`apply_pending_connect_attrs`].
///
/// A value set before the connection existed was checked only for naming
/// exactly one level: [`Backend::txn_isolation_options`] is the data source's
/// answer and could not be consulted yet. That check happens here, against the
/// connection this is about to apply the level to, so an unsupported level
/// fails the connect rather than being applied silently.
fn apply_pending_txn_isolation<B: Backend>(
    handle: &mut ConnectionHandle<B>,
) -> Result<(), OdbcError> {
    let Some(&val) = handle.attrs.get(&ConnectionAttribute::TXN_ISOLATION.0) else {
        return Ok(());
    };
    let Some(connection) = handle.connection.as_ref() else {
        return Ok(());
    };
    validate_txn_isolation::<B>(Some(connection), val as u32)?;
    B::set_txn_isolation(connection, val as u32).into_odbc()
}

/// Reject a `SQL_ATTR_TXN_ISOLATION` value the data source cannot run at.
///
/// Two things make a value invalid. It must name exactly one level, because
/// the attribute selects *a* level and a value with several `SQL_TXN_*` bits
/// set is meaningless even when each bit is individually supported. And that
/// level must appear in [`Backend::txn_isolation_options`], which is what
/// `SQLGetInfo(SQL_TXN_ISOLATION_OPTION)` reports to the application as the
/// menu to choose from.
fn validate_txn_isolation<B: Backend>(
    conn: Option<&B::Connection>,
    level: u32,
) -> Result<(), OdbcError> {
    // Structural check first, because it holds with or without a connection:
    // the spec lets this attribute be set on either side of one.
    if level == 0 || !level.is_power_of_two() {
        return Err(OdbcError::general(
            format!("SQL_ATTR_TXN_ISOLATION: {level:#x} does not name exactly one isolation level"),
            SqlState::invalid_attribute_value(),
        ));
    }
    // The supported set is the data source's, so it cannot be consulted before
    // a connection exists. A level set early is checked against it by
    // `apply_pending_txn_isolation` once the connection is up.
    let Some(conn) = conn else {
        return Ok(());
    };
    let supported = B::txn_isolation_options(conn);
    if level & supported == 0 {
        return Err(OdbcError::general(
            format!(
                "SQL_ATTR_TXN_ISOLATION: isolation level {level:#x} is not supported by this data \
                 source (SQL_TXN_ISOLATION_OPTION = {supported:#x})"
            ),
            SqlState::invalid_attribute_value(),
        ));
    }
    Ok(())
}

/// Whether any statement on this connection currently has a result set open.
///
/// Backs the spec's `24000` row for `SQLSetConnectAttr`: "The *Attribute*
/// argument was SQL_ATTR_CURRENT_CATALOG, and a result set was pending." The
/// row carries no `(DM)` marker, so it is the driver's to return, and a pending
/// result set is exactly an open cursor on one of the connection's statements.
///
/// **This is not transaction state.** The neighbouring `HY011` row ("the
/// *Attribute* argument was SQL_ATTR_TXN_ISOLATION, and a transaction was
/// open") is a different condition over different state. Answering either with
/// the other's fact makes both wrong: a `SELECT` under autocommit leaves a
/// cursor open with no transaction, and a committed-but-uncleared transaction
/// has no cursor.
///
/// Reads `cursor_open` rather than `statement.is_some()` for the reason that
/// field exists: `SQLPrepareW` stores a backend statement without opening a
/// cursor, so a merely-prepared statement has no result set pending.
///
/// Walking the registry is sound without any further locking because statements
/// share their connection's group lock, which the caller's scope already holds.
fn connection_has_result_set_pending<B: Backend>(
    scope: &mut crate::handles::scope::HandleScope<'_>,
    conn_token: *mut c_void,
) -> bool {
    crate::handles::registry::registry()
        .children_of(conn_token)
        .into_iter()
        .any(|stmt_ptr| {
            scope
                .get::<crate::handles::StatementHandle<B>>(stmt_ptr)
                .is_ok_and(|stmt| stmt.cursor_open)
        })
}

/// Generic implementation of SQLSetConnectAttrW.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlsetconnectattr-function>
///
/// Stores known integer/pointer attributes in the handle's `attrs` map, and
/// string attributes in `attr_strings`. Unknown attributes are accepted silently
/// so that tools setting driver-specific or DM-layer attributes do not fail.
///
/// # Parameters
///
/// - `connection_handle`: connection handle (SQL_HANDLE_DBC).
/// - `attribute`: the connection attribute to set (e.g. `SQL_ATTR_AUTOCOMMIT`).
/// - `value_ptr`: pointer to the value to associate with `attribute`. Either an
///   integer cast to a pointer, or a pointer to a null-terminated UTF-16 string.
/// - `string_length`: byte length of `*value_ptr` when it is a string; ignored
///   for integer-valued attributes. `SQL_NTS` (-3) means null-terminated.
///
/// # Spec compliance
///
/// - 01000 General warning: not currently returned here.
/// - 01S02 Option value changed: not currently returned. The driver stores
///   whatever value the application provides. Returning 01S02 would require
///   defining "similar" values per attribute. Deferred.
/// - 08002 Connection name in use: not returned; the driver does not reject
///   `SQL_ATTR_ODBC_CURSORS` changes after connect (this is a DM-managed
///   cursor-library setting the driver does not enforce).
/// - 08003 Connection not open: (driver-manager-handled; not returned here).
/// - 08S01 Communication link failure: not applicable; this function does not
///   communicate with the data source.
/// - 24000 Invalid cursor state: returned for `SQL_ATTR_CURRENT_CATALOG` when
///   any statement on the connection has an open cursor. The spec's row is
///   "the *Attribute* argument was SQL_ATTR_CURRENT_CATALOG, and a result set
///   was pending", and it carries no `(DM)` marker, so the driver owes it. See
///   `connection_has_result_set_pending`; note in particular that this is a
///   *cursor* condition, not the transaction condition HY011 describes.
/// - 25000 Illegal operation while in a local transaction: not applicable;
///   distributed transactions (DTC) are not supported, and
///   `SQL_ATTR_ENLIST_IN_DTC` reports HYC00 before a transaction can be
///   enlisted in one.
/// - 3D000 Invalid catalog name: **returned by this driver**, propagated
///   unchanged from [`Backend::set_current_catalog`]. The row ("the *Attribute*
///   argument was SQL_CURRENT_CATALOG, and the specified catalog name was
///   invalid") carries no `(DM)` marker, so it is the driver's to return, and
///   core cannot produce it: only the data source knows which catalogs exist,
///   and the attribute's own description has the driver send something to it
///   ("the driver sends a **USE** *database* statement"). Core asks the hook and
///   stores the value only if it succeeds, so a rejected catalog is never
///   recorded as the current one. A backend reports this with
///   [`SqlState::invalid_catalog_name`].
/// - HY000 General error: returned for unexpected internal errors.
/// - HY001 Memory allocation error: not returned; Rust panics on allocation
///   failure, which is caught by `panic_safe` and converted to `SQL_ERROR`/HY000.
/// - HY008: Operation canceled; not returned here. Cancelling a connection-level call needs
///   `SQLCancelHandle` on a connection handle, which this driver does not export, so no cancel
///   token exists for this call to observe: `SQLCancel` takes a statement handle and cannot
///   reach one. The asynchronous clause is likewise inapplicable: core never returns
///   `SQL_STILL_EXECUTING`.
/// - HY009 Invalid use of null pointer: returned when the *Attribute* argument
///   is `SQL_ATTR_CURRENT_CATALOG` (the one string-valued attribute this
///   function handles) and *ValuePtr* is null. The row carries no `(DM)`
///   marker, so the check is the driver's. A null is not "clear the catalog":
///   the spec defines no such operation, and the session's catalog is not
///   something the driver can unset by forgetting a string.
/// - HY010 Function sequence error: (driver-manager-handled; not returned here).
/// - HY011 Attribute cannot be set now: returned for `SQL_ATTR_PACKET_SIZE`
///   once the connection is open, which the spec states directly: "if the
///   application sets packet size after a connection has already been made,
///   the driver will return SQLSTATE HY011". Also returned for
///   `SQL_ATTR_TXN_ISOLATION` when a transaction is open, which the spec's own
///   HY011 row names. That is tracked as `ConnectionHandle::txn_dirty`, set when a
///   statement-producing call runs under manual commit and cleared by
///   `SQLEndTran` or by switching autocommit back on. Note this is *not* the
///   cursor condition 24000 describes.
/// - HY013 Memory management error: not returned; Rust panics on memory errors,
///   caught by `panic_safe` and converted to `SQL_ERROR`/HY000.
/// - HY024 Invalid attribute value: returned for `SQL_ATTR_ACCESS_MODE` (not 0 or 1),
///   `SQL_ATTR_AUTOCOMMIT` (not 0 or 1), `SQL_ATTR_TRACE` (not 0 or 1), and
///   `SQL_ATTR_ODBC_CURSORS` (not 0, 1, or 2). Also returned when
///   `SQL_ATTR_CONNECTION_DEAD` is set (read-only attribute), and for
///   `SQL_ATTR_TXN_ISOLATION` when the value does not name exactly one
///   isolation level or names one outside [`Backend::txn_isolation_options`].
///   The spec's own HY024 row assigns this check to the driver: the Driver
///   Manager only validates attributes "that accept a discrete set of values",
///   and "for all other connection and statement attributes, the driver must
///   verify the value specified in ValuePtr".
/// - HY090 Invalid string or buffer length: the spec annotates every clause of
///   this row `(DM)`; the `string_length < 0` check for
///   `SQL_ATTR_CURRENT_CATALOG` is guarded defensively here, so the row's own
///   condition **is** answered by core when no Driver Manager caught it first.
///
///   **Also returned here**, for a condition the row does not state:
///   `SQL_ATTR_CURRENT_CATALOG` (the one attribute of this function whose value
///   is a string, and so the whole set) passed with `StringLength` of `SQL_NTS`
///   and no null terminator within `MAX_NTS_SCAN` (1 048 576) code units. That is a
///   length the driver cannot determine, and storing the scanned prefix would hand
///   `Backend::set_current_catalog` a truncated name, which selects a *different
///   catalog* rather than failing. See
///   `set_current_catalog_refuses_an_nts_value_that_runs_to_the_scan_cap`.
/// - HY092 Invalid attribute/option identifier: (driver-manager-handled; not
///   returned here). Unknown attributes are accepted silently.
/// - HY114 Driver does not support connection-level asynchronous function execution:
///   (driver-manager-handled; not returned here).
/// - HY117 Connection is suspended due to unknown transaction state:
///   (driver-manager-handled; not returned here).
/// - HY121 Cursor Library and Driver-Aware Pooling cannot be enabled simultaneously:
///   not applicable; the cursor library and connection pooling are not used.
/// - HYC00 Optional feature not implemented: returned for
///   `SQL_ATTR_ASYNC_ENABLE` = `SQL_ASYNC_ENABLE_ON`, since
///   `SQLGetInfo(SQL_ASYNC_MODE)` reports `SQL_AM_NONE`, and for
///   `SQL_ATTR_ENLIST_IN_DTC`, since core enlists in no distributed
///   transaction. An *unrecognized* attribute is still accepted silently for
///   DM/tool compatibility (a warning is logged instead). Note the distinction
///   this row draws between an unsupported *attribute* and an unsupported
///   *value*: an isolation level the data source cannot run at is HY024
///   above, not HYC00. A backend may also produce this from
///   [`Backend::set_current_catalog`] or [`Backend::set_access_mode`]; note
///   that the latter defaults to accepting, because the spec makes read-only a
///   hint the driver "is not required to" enforce.
/// - HYT01 Connection timeout expired: not returned by core, but a backend hook
///   this function calls (`set_autocommit`, `set_txn_isolation`,
///   `set_access_mode` or `set_current_catalog`) may reach the data source and
///   report it.
/// - IM001 Driver does not support this function: (driver-manager-handled; not
///   returned here).
/// - IM009 Unable to load translation DLL: not applicable; translation DLLs are
///   not supported (`SQL_ATTR_TRANSLATE_LIB` is accepted silently).
/// - IM017 Polling is disabled in asynchronous notification mode: not returned here
///   (the asynchronous notification model is not supported; not DM-annotated in the spec).
/// - IM018 SQLCompleteAsync has not been called: not returned here (the asynchronous
///   notification model is not supported; not DM-annotated in the spec).
/// - S1118 Driver does not support asynchronous notification: not applicable;
///   asynchronous notification is not supported.
///
/// # Safety
///
/// `connection_handle` must point to a valid `ConnectionHandle<B>`.
pub unsafe fn sql_set_connect_attr_w<B: Backend>(
    connection_handle: *mut c_void,
    attribute: i32,
    value_ptr: *mut c_void,
    string_length: i32,
) -> SqlReturn {
    tracing::trace!(
        "SQLSetConnectAttrW(conn={:?}, attr_raw={}, value={:?})",
        connection_handle,
        attribute,
        value_ptr
    );
    let attr = ConnectionAttribute(attribute);
    tracing::debug!("SQLSetConnectAttrW: attr={:?}", attr);
    // SAFETY: connection_handle is null or a valid ConnectionHandle<B> allocated by
    // sql_alloc_handle; kind and group are validated by scope.get inside the closure.
    let ret = unsafe {
        panic_safe::<B, _>(connection_handle, |scope| {
            {
                let conn = scope.get::<ConnectionHandle<B>>(connection_handle)?;
                conn.diagnostics.clear();
            }

            // Spec 24000, checked before the connection borrow below because
            // both need the scope and that borrow lasts the whole match.
            // Gated on the one attribute the spec's row names, so no other
            // attribute pays for the walk.
            if attr == ConnectionAttribute::CURRENT_CATALOG
                && connection_has_result_set_pending::<B>(scope, connection_handle)
            {
                return Err(OdbcError::general(
                    "SQL_ATTR_CURRENT_CATALOG cannot be set while a result set is pending",
                    SqlState::invalid_cursor_state(),
                ));
            }

            let conn = scope.get::<ConnectionHandle<B>>(connection_handle)?;

            match attr {
                // Discrete-valued attributes: validate before storing (HY024).
                _ if attr == ConnectionAttribute::ACCESS_MODE => {
                    let val = value_ptr as usize;
                    if val != SQL_MODE_READ_WRITE && val != SQL_MODE_READ_ONLY {
                        return Err(OdbcError::general(
                            format!(
                                "SQL_ATTR_ACCESS_MODE: invalid value {val} (expected 0=read/write or 1=read-only)"
                            ),
                            SqlState::invalid_attribute_value(),
                        ));
                    }
                    // Applied here when connected; deferred to
                    // `apply_pending_access_mode` at connect otherwise, since
                    // the spec lists this attribute as settable either side of
                    // one. Stored only once the backend accepted it, so
                    // `SQLGetConnectAttr` cannot report a mode the data source
                    // refused.
                    if let Some(connection) = conn.connection.as_ref() {
                        B::set_access_mode(connection, val == SQL_MODE_READ_ONLY).into_odbc()?;
                    }
                    conn.attrs.insert(attribute, val);
                    Ok(SqlReturn::SUCCESS)
                }
                _ if attr == ConnectionAttribute::AUTOCOMMIT => {
                    let val = value_ptr as usize;
                    if val != SQL_AUTOCOMMIT_OFF && val != SQL_AUTOCOMMIT_ON {
                        return Err(OdbcError::general(
                            format!(
                                "SQL_ATTR_AUTOCOMMIT: invalid value {val} (expected 0=off or 1=on)"
                            ),
                            SqlState::invalid_attribute_value(),
                        ));
                    }
                    // The attribute must reach the backend. Storing it alone
                    // would let an application turn autocommit off, run several
                    // statements, call SQLEndTran(SQL_ROLLBACK), and have every
                    // statement already committed, with success reported at
                    // every step. Backends that cannot honour manual-commit
                    // mode report HYC00 from the default `set_autocommit`.
                    // The spec lists SQL_ATTR_AUTOCOMMIT as settable either
                    // before or after connecting. When set before, the value is
                    // stored and applied by `apply_pending_autocommit` once the
                    // connection is open.
                    if let Some(connection) = conn.connection.as_ref() {
                        B::set_autocommit(connection, val == SQL_AUTOCOMMIT_ON).into_odbc()?;
                    }

                    // Spec: "Any open transactions on the connection are
                    // committed when SQL_ATTR_AUTOCOMMIT is set to
                    // SQL_AUTOCOMMIT_ON to change from manual-commit mode to
                    // autocommit mode." That commit ends the transaction, so
                    // SQL_ATTR_TXN_ISOLATION becomes settable again. Only after
                    // `set_autocommit` succeeded, because a backend that refused
                    // the switch committed nothing.
                    if val == SQL_AUTOCOMMIT_ON {
                        conn.txn_dirty = false;
                    }

                    conn.attrs.insert(attribute, val);
                    Ok(SqlReturn::SUCCESS)
                }
                _ if attr == ConnectionAttribute::TRACE => {
                    let val = value_ptr as usize;
                    if val != SQL_OPT_TRACE_OFF && val != SQL_OPT_TRACE_ON {
                        return Err(OdbcError::general(
                            format!("SQL_ATTR_TRACE: invalid value {val} (expected 0=off or 1=on)"),
                            SqlState::invalid_attribute_value(),
                        ));
                    }
                    conn.attrs.insert(attribute, val);
                    Ok(SqlReturn::SUCCESS)
                }
                _ if attr == ConnectionAttribute::ODBC_CURSORS => {
                    let val = value_ptr as usize;
                    if val != SQL_CUR_USE_IF_NEEDED
                        && val != SQL_CUR_USE_ODBC
                        && val != SQL_CUR_USE_DRIVER
                    {
                        return Err(OdbcError::general(
                            format!(
                                "SQL_ATTR_ODBC_CURSORS: invalid value {val} (expected 0, 1, or 2)"
                            ),
                            SqlState::invalid_attribute_value(),
                        ));
                    }
                    conn.attrs.insert(attribute, val);
                    Ok(SqlReturn::SUCCESS)
                }

                // SQL_ATTR_TXN_ISOLATION selects *one* isolation level, and the
                // spec's HY024 row makes verifying it the driver's job: "For
                // all other connection and statement attributes, the driver
                // must verify the value specified in ValuePtr". A level the
                // data source cannot run at must be rejected rather than
                // stored and echoed back, and a level it can must actually
                // reach the data source.
                _ if attr == ConnectionAttribute::TXN_ISOLATION => {
                    // Spec HY011, verbatim: "The Attribute argument was
                    // SQL_ATTR_TXN_ISOLATION, and a transaction was open." The
                    // attribute's own description says the same thing from the
                    // application's side: "an application must call SQLEndTran
                    // to commit or roll back all open transactions on a
                    // connection, before calling SQLSetConnectAttr with this
                    // option". Footnote [3] says it a third time.
                    //
                    // Checked before the value itself: an open transaction
                    // makes the call illegal whatever level was asked for, so
                    // reporting HY024 for a bad value here would tell the
                    // application to fix the wrong thing.
                    if conn.txn_dirty {
                        return Err(OdbcError::general(
                            "SQL_ATTR_TXN_ISOLATION cannot be set while a transaction is open",
                            SqlState::attribute_cannot_be_set_now(),
                        ));
                    }
                    let level = u32::try_from(value_ptr as usize).map_err(|_| {
                        OdbcError::general(
                            format!("SQL_ATTR_TXN_ISOLATION: invalid value {:?}", value_ptr),
                            SqlState::invalid_attribute_value(),
                        )
                    })?;
                    validate_txn_isolation::<B>(conn.connection.as_ref(), level)?;
                    // Applied here when connected; deferred to
                    // `apply_pending_txn_isolation` at connect otherwise, since
                    // the spec lists this attribute as settable either side of
                    // the connection.
                    if let Some(c) = conn.connection.as_ref() {
                        B::set_txn_isolation(c, level).into_odbc()?;
                    }
                    conn.attrs.insert(attribute, level as usize);
                    Ok(SqlReturn::SUCCESS)
                }

                // Spec: "If the application sets packet size after a connection
                // has already been made, the driver will return SQLSTATE HY011
                // (Attribute cannot be set now)." The attribute table lists it
                // as settable "Before" only.
                _ if attr == ConnectionAttribute::PACKET_SIZE && conn.connection.is_some() => {
                    Err(OdbcError::general(
                        "SQL_ATTR_PACKET_SIZE cannot be set once the connection is open",
                        SqlState::attribute_cannot_be_set_now(),
                    ))
                }

                // `SQLGetInfo(SQL_ASYNC_MODE)` reports SQL_AM_NONE and the
                // `Backend` trait is synchronous, so there is no asynchronous
                // execution to enable for the statements on this connection.
                _ if attr == ConnectionAttribute::ASYNC_ENABLE
                    && value_ptr as usize != SQL_ASYNC_ENABLE_OFF =>
                {
                    Err(OdbcError::NotImplemented {
                        feature: "SQL_ATTR_ASYNC_ENABLE = SQL_ASYNC_ENABLE_ON".into(),
                    })
                }

                // Enlisting in an MS DTC distributed transaction requires a
                // transaction object core does nothing with. Accepting it
                // silently would leave an application believing its work is
                // under the protection of that transaction.
                _ if attr == ConnectionAttribute::ENLIST_IN_DTC => Err(OdbcError::NotImplemented {
                    feature: "SQL_ATTR_ENLIST_IN_DTC (distributed transactions)".into(),
                }),

                // Non-discrete integer-valued attributes: store value directly.
                _ if attr == ConnectionAttribute::LOGIN_TIMEOUT
                    || attr == ConnectionAttribute::TRANSLATE_OPTION
                    || attr == ConnectionAttribute::PACKET_SIZE
                    || attr == ConnectionAttribute::CONNECTION_TIMEOUT
                    || attr == ConnectionAttribute::ASYNC_ENABLE
                    || attr == ConnectionAttribute::METADATA_ID =>
                {
                    conn.attrs.insert(attribute, value_ptr as usize);
                    Ok(SqlReturn::SUCCESS)
                }

                // String-valued: SQL_ATTR_CURRENT_CATALOG. Decode UTF-16, then
                // ask the backend to switch. Storing without switching would
                // tell an application its unqualified names now resolve
                // somewhere they do not; a backend that cannot switch reports
                // HYC00 from the default `set_current_catalog`, and nothing is
                // stored.
                _ if attr == ConnectionAttribute::CURRENT_CATALOG => {
                    // Spec HY009, on a row with no (DM) marker: "The Attribute
                    // argument identified a connection attribute that required a
                    // string value, and the ValuePtr argument was a null
                    // pointer." Removing the stored override and reporting
                    // success is an operation the spec does not define: the
                    // session's catalog would be untouched while the
                    // application was told it had been cleared. Neither
                    // psqlODBC (which ignores SQL_CURRENT_QUALIFIER on set) nor
                    // MySQL Connector/ODBC (which measures the value with
                    // strlen before looking at it) implements null-as-clear.
                    if value_ptr.is_null() {
                        return Err(OdbcError::general(
                            "SQL_ATTR_CURRENT_CATALOG requires a string value, \
                             and ValuePtr was null",
                            SqlState::invalid_use_of_null_pointer(),
                        ));
                    }
                    // string_length is in bytes; convert to UTF-16 code units.
                    // SQL_NTS passes through; other negatives are invalid.
                    let len_code_units = if string_length == SQL_NTS {
                        SQL_NTS
                    } else if string_length < 0 {
                        return Err(OdbcError::general(
                            format!("Invalid string length: {string_length}"),
                            SqlState::invalid_string_or_buffer_length(),
                        ));
                    } else {
                        string_length / 2
                    };
                    let s = utf16_to_string(value_ptr as *const u16, len_code_units)?;
                    // Applied here when connected; a value set before the
                    // connection exists is stored and applied by
                    // `apply_pending_current_catalog` at connect, since the
                    // spec lists this attribute as settable either side of one.
                    if let Some(connection) = conn.connection.as_ref() {
                        B::set_current_catalog(connection, &s).into_odbc()?;
                    }
                    conn.attr_strings.insert(attribute, s);
                    Ok(SqlReturn::SUCCESS)
                }

                // SQL_ATTR_CONNECTION_DEAD is read-only; reject writes.
                _ if attr == ConnectionAttribute::CONNECTION_DEAD => Err(OdbcError::general(
                    "SQL_ATTR_CONNECTION_DEAD is read-only",
                    SqlState::invalid_attribute_value(),
                )),

                // All other attributes (including DM-only and driver-specific) are accepted
                // silently. Returning ERROR here would break tools that set attrs we don't know.
                _ => {
                    tracing::warn!(
                        "SQLSetConnectAttrW: unrecognized attribute {attr:?} accepted silently \
                         (spec requires SQL_ERROR; relaxed for DM/tool compatibility)"
                    );
                    Ok(SqlReturn::SUCCESS)
                }
            }
        })
    };
    tracing::debug!("SQLSetConnectAttrW -> {:?}", ret);
    ret
}

/// Every connection attribute the ODBC specification defines, as `odbc-sys`
/// names them.
///
/// `SQLGetConnectAttr`'s diagnostics table draws a line this list is the whole
/// of: `HY092` is "the value specified for the argument *Attribute* was not
/// valid for the version of ODBC supported by the driver", while `HYC00` is
/// "...was a valid ODBC connection attribute for the version of ODBC supported
/// by the driver, but was not supported by the driver". A number absent from
/// this list is not an ODBC connection attribute at all and is the first; one
/// present but with no arm in the match below is the second.
///
/// Transcribed from `odbc_sys::ConnectionAttribute`'s own constants rather than
/// from `sqlext.h`, so it cannot name a value the crate does not.
const DEFINED_CONNECTION_ATTRIBUTES: &[ConnectionAttribute] = &[
    ConnectionAttribute::ASYNC_ENABLE,
    ConnectionAttribute::ACCESS_MODE,
    ConnectionAttribute::AUTOCOMMIT,
    ConnectionAttribute::LOGIN_TIMEOUT,
    ConnectionAttribute::TRACE,
    ConnectionAttribute::TRACEFILE,
    ConnectionAttribute::TRANSLATE_LIB,
    ConnectionAttribute::TRANSLATE_OPTION,
    ConnectionAttribute::TXN_ISOLATION,
    ConnectionAttribute::CURRENT_CATALOG,
    ConnectionAttribute::ODBC_CURSORS,
    ConnectionAttribute::QUIET_MODE,
    ConnectionAttribute::PACKET_SIZE,
    ConnectionAttribute::CONNECTION_TIMEOUT,
    ConnectionAttribute::DISCONNECT_BEHAVIOUR,
    ConnectionAttribute::RESET_CONNECTION,
    ConnectionAttribute::ASYNC_DBC_FUNCTIONS_ENABLE,
    ConnectionAttribute::DBC_INFO_TOKEN,
    ConnectionAttribute::ASYNC_DBC_EVENT,
    ConnectionAttribute::ENLIST_IN_DTC,
    ConnectionAttribute::ENLIST_IN_XA,
    ConnectionAttribute::CONNECTION_DEAD,
    ConnectionAttribute::AUTO_IPD,
    ConnectionAttribute::METADATA_ID,
];

/// Generic implementation of SQLGetConnectAttrW.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlgetconnectattr-function>
///
/// Returns integer attributes as a `u32` written to `*value_ptr`.
/// Returns string attributes as a null-terminated UTF-16 string written to `*value_ptr`.
///
/// # Parameters
///
/// - `connection_handle`: connection handle (SQL_HANDLE_DBC).
/// - `attribute`: the connection attribute to retrieve.
/// - `value_ptr`: output buffer. For integer attributes a `u32` is written;
///   for string attributes a null-terminated UTF-16 string is written.
///   May be null, in which case `string_length_ptr` still receives the byte count.
/// - `buffer_length`: size of `value_ptr` in bytes; ignored for integer attributes.
///   Must be even for Unicode strings per the spec.
/// - `string_length_ptr`: receives the byte count of the string written to
///   `*value_ptr` (excluding the null terminator), or `sizeof(u32)` for integer
///   attributes. May be null.
///
/// # Spec compliance
///
/// - 01000 General warning: not currently returned here.
/// - 01004 String data, right truncated: returned (as `SQL_SUCCESS_WITH_INFO`) by
///   `write_utf16` when the string value is truncated to fit `buffer_length`.
/// - 08003 Connection not open: (driver-manager-handled; not returned here).
/// - 08S01 Communication link failure: not applicable; this function does not
///   communicate with the data source. `SQL_ATTR_CONNECTION_DEAD` is not an
///   exception: [`Backend::connection_dead`] is documented to answer from state
///   the backend already holds rather than by probing the link, because a
///   connection pool may read it on every checkout.
/// - HY000 General error: returned for unexpected internal errors.
/// - HY001 Memory allocation error: not returned; Rust panics on allocation
///   failure, which is caught by `panic_safe` and converted to `SQL_ERROR`/HY000.
/// - HY010 Function sequence error: (driver-manager-handled; not returned here).
/// - HY013 Memory management error: not returned; Rust panics on memory errors,
///   caught by `panic_safe` and converted to `SQL_ERROR`/HY000.
/// - HY090 Invalid string or buffer length: (driver-manager-handled; not returned
///   here except for a negative `buffer_length`). A `buffer_length < 0` check for
///   `SQL_ATTR_CURRENT_CATALOG` is performed as a defensive measure and returns
///   HY090. A *large* `buffer_length` is not an error: the spec describes the
///   argument only as "the length of \**ValuePtr*", so a buffer past what the
///   shared string writer's `SQLSMALLINT` can express is clamped rather than
///   refused.
/// - HY092 Invalid attribute/option identifier: returned for an identifier that
///   is not an ODBC connection attribute at all; see
///   `DEFINED_CONNECTION_ATTRIBUTES`, which is the whole of the spec's list.
///   The spec's own wording for this row is "not valid for the version of ODBC
///   supported by the driver", which is a different claim from the HYC00 row
///   below.
/// - HY114 Driver does not support connection-level asynchronous function execution:
///   (driver-manager-handled; not returned here).
/// - HY117 Connection is suspended due to unknown transaction state:
///   (driver-manager-handled; not returned here).
/// - HYC00 Optional feature not implemented: returned for an attribute that is
///   on the spec's list but that this function has no answer for:
///   `SQL_ATTR_QUIET_MODE`, `SQL_ATTR_TRACEFILE`, `SQL_ATTR_TRANSLATE_LIB`,
///   `SQL_ATTR_ENLIST_IN_DTC`, `SQL_ATTR_ASYNC_DBC_FUNCTIONS_ENABLE` and the
///   rest. The spec's wording is "a valid ODBC connection attribute for the
///   version of ODBC supported by the driver, but was not supported by the
///   driver".
/// - HYT01 Connection timeout expired: not returned; this function does not wait
///   on the data source.
/// - IM001 Driver does not support this function: (driver-manager-handled; not
///   returned here).
///
/// # Safety
///
/// `connection_handle` must point to a valid `ConnectionHandle<B>`.
/// `value_ptr` must be writable for the appropriate size.
pub unsafe fn sql_get_connect_attr_w<B: Backend>(
    connection_handle: *mut c_void,
    attribute: i32,
    value_ptr: *mut c_void,
    buffer_length: i32,
    string_length_ptr: *mut i32,
) -> SqlReturn {
    tracing::trace!(
        "SQLGetConnectAttrW(conn={:?}, attr_raw={})",
        connection_handle,
        attribute
    );
    let attr = ConnectionAttribute(attribute);
    tracing::debug!("SQLGetConnectAttrW: attr={:?}", attr);
    // SAFETY: connection_handle is null or a valid ConnectionHandle<B> allocated by
    // sql_alloc_handle; kind and group are validated by scope.get inside the closure.
    let ret = unsafe {
        panic_safe::<B, _>(connection_handle, |scope| {
            let conn = scope.get::<ConnectionHandle<B>>(connection_handle)?;
            conn.diagnostics.clear();

            // Helper: write a u32 integer value to value_ptr.
            // SAFETY: value_ptr and string_length_ptr are non-null (checked) and
            // caller guarantees they point to writable memory of the appropriate size.
            let write_u32 = |v: u32| {
                if !value_ptr.is_null() {
                    // SAFETY: non-null checked above; caller guarantees writable u32
                    std::ptr::write_unaligned(value_ptr as *mut u32, v);
                }
                if !string_length_ptr.is_null() {
                    // SAFETY: non-null checked above; caller guarantees writable i32
                    std::ptr::write_unaligned(string_length_ptr, std::mem::size_of::<u32>() as i32);
                }
            };

            // The two connection attributes the spec declares `SQLULEN` rather
            // than `SQLUINTEGER`: `SQL_ATTR_ASYNC_ENABLE` ("A SQLULEN value
            // that specifies whether a function called with a statement on the
            // specified connection is executed asynchronously") and
            // `SQL_ATTR_ODBC_CURSORS` ("An SQLULEN value specifying how the
            // Driver Manager uses the ODBC cursor library"). Every other
            // integer-valued connection attribute really is `SQLUINTEGER`, so
            // this is a two-attribute exception rather than the blanket rule
            // that applies to statement attributes, where *no* non-pointer
            // attribute is `SQLUINTEGER`.
            //
            // SAFETY: as `write_u32` above, but the buffer is SQLULEN-wide.
            let write_ulen = |v: usize| {
                if !value_ptr.is_null() {
                    // SAFETY: non-null checked above; caller guarantees writable SQLULEN
                    std::ptr::write_unaligned(value_ptr as *mut usize, v);
                }
                if !string_length_ptr.is_null() {
                    // SAFETY: non-null checked above; caller guarantees writable i32
                    std::ptr::write_unaligned(
                        string_length_ptr,
                        std::mem::size_of::<usize>() as i32,
                    );
                }
            };

            match attr {
                _ if attr == ConnectionAttribute::ACCESS_MODE => {
                    let v = conn
                        .attrs
                        .get(&attribute)
                        .copied()
                        .unwrap_or(SQL_MODE_READ_WRITE);
                    write_u32(v as u32);
                    Ok(SqlReturn::SUCCESS)
                }
                _ if attr == ConnectionAttribute::AUTOCOMMIT => {
                    let v = conn
                        .attrs
                        .get(&attribute)
                        .copied()
                        .unwrap_or(SQL_AUTOCOMMIT_ON);
                    write_u32(v as u32);
                    Ok(SqlReturn::SUCCESS)
                }
                _ if attr == ConnectionAttribute::LOGIN_TIMEOUT => {
                    let v = conn.attrs.get(&attribute).copied().unwrap_or(0);
                    write_u32(v as u32);
                    Ok(SqlReturn::SUCCESS)
                }
                _ if attr == ConnectionAttribute::CONNECTION_TIMEOUT => {
                    let v = conn.attrs.get(&attribute).copied().unwrap_or(0);
                    write_u32(v as u32);
                    Ok(SqlReturn::SUCCESS)
                }
                _ if attr == ConnectionAttribute::TXN_ISOLATION => {
                    // Unset: report what the backend declares, not a constant.
                    // `SQL_DEFAULT_TXN_ISOLATION` is derived from the same hook
                    // (see `default_get_info`), so the info type and the
                    // connection attribute cannot disagree on one connection.
                    // The declared default needs the connection; with none
                    // there is no data source to have a default, so an unset
                    // attribute reads as 0 until one exists.
                    let declared = conn
                        .connection
                        .as_ref()
                        .map_or(0, |c| B::default_txn_isolation(c) as usize);
                    let v = conn.attrs.get(&attribute).copied().unwrap_or(declared);
                    write_u32(v as u32);
                    Ok(SqlReturn::SUCCESS)
                }
                _ if attr == ConnectionAttribute::TRACE => {
                    let v = conn
                        .attrs
                        .get(&attribute)
                        .copied()
                        .unwrap_or(SQL_OPT_TRACE_OFF);
                    write_u32(v as u32);
                    Ok(SqlReturn::SUCCESS)
                }
                _ if attr == ConnectionAttribute::ODBC_CURSORS => {
                    let v = conn
                        .attrs
                        .get(&attribute)
                        .copied()
                        .unwrap_or(SQL_CUR_USE_DRIVER);
                    write_ulen(v);
                    Ok(SqlReturn::SUCCESS)
                }
                _ if attr == ConnectionAttribute::PACKET_SIZE => {
                    let v = conn.attrs.get(&attribute).copied().unwrap_or(0);
                    write_u32(v as u32);
                    Ok(SqlReturn::SUCCESS)
                }
                _ if attr == ConnectionAttribute::METADATA_ID => {
                    let v = conn
                        .attrs
                        .get(&attribute)
                        .copied()
                        .unwrap_or(SQL_FALSE as usize);
                    write_u32(v as u32);
                    Ok(SqlReturn::SUCCESS)
                }
                _ if attr == ConnectionAttribute::AUTO_IPD => {
                    write_u32(SQL_FALSE);
                    Ok(SqlReturn::SUCCESS)
                }

                // The two remaining attributes `SQLSetConnectAttr` stores. An
                // attribute this driver holds a value for is one it can report:
                // `SQLGetConnectAttr` is how an application reads back what it
                // set.
                _ if attr == ConnectionAttribute::ASYNC_ENABLE => {
                    let v = conn
                        .attrs
                        .get(&attribute)
                        .copied()
                        .unwrap_or(SQL_ASYNC_ENABLE_OFF);
                    write_ulen(v);
                    Ok(SqlReturn::SUCCESS)
                }
                _ if attr == ConnectionAttribute::TRANSLATE_OPTION => {
                    let v = conn.attrs.get(&attribute).copied().unwrap_or(0);
                    write_u32(v as u32);
                    Ok(SqlReturn::SUCCESS)
                }

                // SQL_ATTR_CONNECTION_DEAD: whatever the backend knows about
                // its own liveness.
                //
                // A handle with no connection is not a *lost* connection: it
                // never had one, or `SQLDisconnect` closed it on request,
                // and SQL_CD_TRUE asserts the first. The Driver Manager's 08003
                // covers the not-connected case for the attributes that require
                // one, so core has nothing to add here beyond declining to call
                // a hook it has no connection to pass.
                _ if attr == ConnectionAttribute::CONNECTION_DEAD => {
                    let dead = conn.connection.as_ref().is_some_and(B::connection_dead);
                    tracing::debug!("SQLGetConnectAttrW: SQL_ATTR_CONNECTION_DEAD -> {}", dead);
                    write_u32(if dead { SQL_CD_TRUE } else { SQL_CD_FALSE } as u32);
                    Ok(SqlReturn::SUCCESS)
                }

                // SQL_ATTR_CURRENT_CATALOG: what the application set, else what
                // the session is actually using. Without the second half this
                // attribute is write-only: it answers "" while
                // `SQLGetInfo(SQL_DATABASE_NAME)`, which the spec makes the same
                // value, answers the real catalog.
                _ if attr == ConnectionAttribute::CURRENT_CATALOG => {
                    let s = conn
                        .attr_strings
                        .get(&attribute)
                        .cloned()
                        .or_else(|| {
                            conn.connection
                                .as_ref()
                                .and_then(|c| B::current_catalog(c))
                                .map(Cow::into_owned)
                        })
                        .unwrap_or_default();
                    // write_utf16 takes buf_len and len_ptr as i16 (SQLSMALLINT),
                    // but GetConnectAttrW uses i32 (SQLINTEGER). Validate the
                    // buffer length fits, then widen the output back to i32 bytes.
                    if buffer_length < 0 {
                        return Err(OdbcError::general(
                            format!("Invalid buffer length: {buffer_length}"),
                            SqlState::invalid_string_or_buffer_length(),
                        ));
                    }
                    // `write_utf16` takes an `i16` because most string-bearing
                    // entry points declare their buffer `SQLSMALLINT`;
                    // `SQLGetConnectAttr` is the odd one out and declares it
                    // `SQLINTEGER`. Clamping rather than failing is safe in the
                    // direction that matters: core writes at most `i16::MAX`
                    // code units into a buffer the application declared larger,
                    // and a value that genuinely does not fit is still reported
                    // as `01004` by the writer below. The spec describes this
                    // argument only as "the length of *ValuePtr" and defines no
                    // error for a large one.
                    let buf_u16 = i16::try_from(buffer_length / 2).unwrap_or(i16::MAX);
                    let mut len_u16: i16 = 0;
                    let ret = crate::utf16::note_truncation(
                        write_utf16(&s, value_ptr as *mut u16, buf_u16, &mut len_u16),
                        &mut conn.diagnostics,
                    );
                    if !string_length_ptr.is_null() {
                        // SAFETY: non-null checked above; caller guarantees writable i32
                        std::ptr::write_unaligned(string_length_ptr, i32::from(len_u16) * 2); // bytes
                    }
                    Ok(ret)
                }

                // Spec HYC00: "a valid ODBC connection attribute for the version
                // of ODBC supported by the driver, but was not supported by the
                // driver." MySQL Connector/ODBC answers HYC00 in this function
                // too, for the one attribute it cannot report
                // (`driver/options.cc` returns `MYERR_S1C00` when
                // SQL_ATTR_CURRENT_CATALOG is read before a connection exists).
                _ if DEFINED_CONNECTION_ATTRIBUTES.contains(&attr) => {
                    Err(OdbcError::NotImplemented {
                        feature: format!("SQLGetConnectAttrW attribute {attr:?}"),
                    })
                }

                // Spec HY092: not an ODBC connection attribute at all.
                _ => Err(OdbcError::general(
                    format!("SQLGetConnectAttrW: unknown attribute {attr:?}"),
                    SqlState::invalid_attribute_option_identifier(),
                )),
            }
        })
    };
    tracing::debug!("SQLGetConnectAttrW -> {:?}", ret);
    ret
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffi::handle::{sql_alloc_handle, sql_free_handle};
    use crate::test_utils::{
        MockAltBackend, MockBackend, MockCancelAwareBackend, MockCatalogRejectingBackend,
        MockConnection, MockIsolationBackend, MockIsolationConnection,
        MockUnappliedIsolationBackend, alloc_env_conn_for, cleanup_env_conn_for, with_handle,
    };
    use odbc_sys::HandleType;

    #[test]
    fn set_and_get_autocommit() {
        unsafe {
            let (env, conn) = alloc_env_conn_for::<MockBackend>();

            // Set autocommit OFF (0)
            let ret = sql_set_connect_attr_w::<MockBackend>(
                conn,
                ConnectionAttribute::AUTOCOMMIT.0,
                std::ptr::null_mut::<c_void>(),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            // Get autocommit, which should be 0
            let mut val: u32 = 99;
            let ret = sql_get_connect_attr_w::<MockBackend>(
                conn,
                ConnectionAttribute::AUTOCOMMIT.0,
                &mut val as *mut u32 as *mut c_void,
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(val, 0);

            cleanup_env_conn_for::<MockBackend>(env, conn);
        }
    }

    #[test]
    fn get_autocommit_default_is_on() {
        unsafe {
            let (env, conn) = alloc_env_conn_for::<MockBackend>();
            let mut val: u32 = 99;
            let ret = sql_get_connect_attr_w::<MockBackend>(
                conn,
                ConnectionAttribute::AUTOCOMMIT.0,
                &mut val as *mut u32 as *mut c_void,
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(val, SQL_AUTOCOMMIT_ON as u32);
            cleanup_env_conn_for::<MockBackend>(env, conn);
        }
    }

    /// Set login and connection timeouts before connecting, then report what
    /// `Backend::connect` actually saw.
    unsafe fn timeouts_seen_by_connect(
        login: Option<usize>,
        connection: Option<usize>,
    ) -> (Option<u32>, Option<u32>) {
        unsafe {
            type B = crate::test_utils::MockAccessModeBackend;
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<B>(HandleType::Env as i16, std::ptr::null_mut(), &mut env);
            let mut conn: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<B>(HandleType::Dbc as i16, env, &mut conn);

            for (attr, value) in [
                (ConnectionAttribute::LOGIN_TIMEOUT.0, login),
                (ConnectionAttribute::CONNECTION_TIMEOUT.0, connection),
            ] {
                if let Some(v) = value {
                    assert_eq!(
                        sql_set_connect_attr_w::<B>(
                            conn,
                            attr,
                            std::ptr::without_provenance_mut::<c_void>(v),
                            0,
                        ),
                        SqlReturn::SUCCESS,
                    );
                }
            }

            let wide: Vec<u16> = "Host=localhost".encode_utf16().collect();
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

            let seen = with_handle::<B, ConnectionHandle<B>, _>(conn, |c| {
                let c = c.connection.as_ref().expect("connected");
                (c.seen_login_timeout, c.seen_connection_timeout)
            });

            let _ = crate::ffi::connect::sql_disconnect::<B>(conn);
            let _ = sql_free_handle::<B>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<B>(HandleType::Env as i16, env);
            seen
        }
    }

    #[test]
    fn login_and_connection_timeouts_reach_the_backend() {
        // Both are set through SQLSetConnectAttr rather than the connection
        // string, so without core carrying them across, `Backend::connect` -
        // which receives only ConnectParams - could never see them. The login
        // timeout is settable "Before" only precisely because it bounds this
        // call.
        unsafe {
            assert_eq!(
                timeouts_seen_by_connect(Some(15), Some(45)),
                (Some(15), Some(45)),
            );
        }
    }

    #[test]
    fn an_unset_timeout_is_none_and_a_zero_timeout_is_some_zero() {
        // The distinction is the spec's, and collapsing it inverts the
        // behaviour: unset means "use the driver's own default", while 0 for
        // SQL_ATTR_LOGIN_TIMEOUT means "the timeout is disabled and a
        // connection attempt will wait indefinitely". A backend that read
        // Some(0) as unset would impose a default on an application that asked
        // for no limit at all.
        unsafe {
            assert_eq!(
                timeouts_seen_by_connect(None, None),
                (None, None),
                "nothing was set, so the backend must see nothing",
            );
            assert_eq!(
                timeouts_seen_by_connect(Some(0), Some(0)),
                (Some(0), Some(0)),
                "an explicit 0 is a value, not an absence",
            );
        }
    }

    /// Read back the access mode a backend actually had applied to it.
    fn applied_access_mode<B: Backend<Connection = crate::test_utils::MockAppliedConnection>>(
        conn: *mut c_void,
    ) -> Option<bool> {
        with_handle::<B, ConnectionHandle<B>, _>(conn, |c| {
            c.connection
                .as_ref()
                .expect("connected")
                .access_mode
                .lock()
                .ok()
                .and_then(|slot| *slot)
        })
    }

    #[test]
    fn setting_access_mode_reaches_the_backend() {
        // The attribute was validated and stored but never applied, so a data
        // source with a real read-only session mode never entered it.
        unsafe {
            type B = crate::test_utils::MockAccessModeBackend;
            let (env, conn, stmt) = crate::test_utils::alloc_connected_env_conn_stmt::<B>();

            assert_eq!(
                applied_access_mode::<B>(conn),
                None,
                "nothing has set the access mode yet",
            );

            let ret = sql_set_connect_attr_w::<B>(
                conn,
                ConnectionAttribute::ACCESS_MODE.0,
                std::ptr::without_provenance_mut::<c_void>(SQL_MODE_READ_ONLY),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(
                applied_access_mode::<B>(conn),
                Some(true),
                "Backend::set_access_mode was never called",
            );

            crate::test_utils::cleanup_connected_env_conn_stmt::<B>(env, conn, stmt);
        }
    }

    #[test]
    fn an_access_mode_set_before_connecting_is_applied_at_connect() {
        // The spec's footnote [1] calls setting this before connecting the
        // interoperable choice - "some drivers do not support changing these
        // after connecting" - so this is the path that matters most, and it
        // runs through `apply_pending_access_mode` rather than the set arm.
        unsafe {
            type B = crate::test_utils::MockAccessModeBackend;
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<B>(HandleType::Env as i16, std::ptr::null_mut(), &mut env);
            let mut conn: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<B>(HandleType::Dbc as i16, env, &mut conn);

            assert_eq!(
                sql_set_connect_attr_w::<B>(
                    conn,
                    ConnectionAttribute::ACCESS_MODE.0,
                    std::ptr::without_provenance_mut::<c_void>(SQL_MODE_READ_ONLY),
                    0,
                ),
                SqlReturn::SUCCESS,
                "the spec lists this attribute as settable before connecting",
            );

            let wide: Vec<u16> = "Host=localhost".encode_utf16().collect();
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

            assert_eq!(
                applied_access_mode::<B>(conn),
                Some(true),
                "a mode set before connecting must be applied once the connection exists",
            );

            let _ = crate::ffi::connect::sql_disconnect::<B>(conn);
            let _ = sql_free_handle::<B>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<B>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn a_backend_that_refuses_an_access_mode_keeps_it_out_of_the_stored_attributes() {
        // Storing a mode the data source refused would let SQLGetConnectAttr
        // report a read-only connection that is nothing of the kind.
        unsafe {
            type B = crate::test_utils::MockRefusingAccessModeBackend;
            let (env, conn, stmt) = crate::test_utils::alloc_connected_env_conn_stmt::<B>();

            let ret = sql_set_connect_attr_w::<B>(
                conn,
                ConnectionAttribute::ACCESS_MODE.0,
                std::ptr::without_provenance_mut::<c_void>(SQL_MODE_READ_ONLY),
                0,
            );
            assert_eq!(ret, SqlReturn::ERROR);

            let mut val: u32 = 99;
            let get = sql_get_connect_attr_w::<B>(
                conn,
                ConnectionAttribute::ACCESS_MODE.0,
                &mut val as *mut u32 as *mut c_void,
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(get, SqlReturn::SUCCESS);
            assert_eq!(
                val, SQL_MODE_READ_WRITE as u32,
                "a refused read-only mode must not be reported back as in force",
            );

            crate::test_utils::cleanup_connected_env_conn_stmt::<B>(env, conn, stmt);
        }
    }

    /// Set `SQL_ATTR_AUTOCOMMIT` to manual commit, optionally run a statement,
    /// then try to change the isolation level. Returns the first SQLSTATE.
    unsafe fn isolation_after<B: Backend>(execute_first: bool) -> String {
        unsafe {
            let (env, conn, stmt) = crate::test_utils::alloc_connected_env_conn_stmt::<B>();

            assert_eq!(
                sql_set_connect_attr_w::<B>(
                    conn,
                    ConnectionAttribute::AUTOCOMMIT.0,
                    std::ptr::without_provenance_mut::<c_void>(SQL_AUTOCOMMIT_OFF),
                    0,
                ),
                SqlReturn::SUCCESS,
                "the mock supports manual-commit mode",
            );

            if execute_first {
                let sql: Vec<u16> = "UPDATE t SET c = 1".encode_utf16().collect();
                assert_eq!(
                    crate::ffi::execute::sql_exec_direct_w::<B>(
                        stmt,
                        sql.as_ptr(),
                        sql.len() as i32,
                    ),
                    SqlReturn::SUCCESS,
                );
            }

            let _ = sql_set_connect_attr_w::<B>(
                conn,
                ConnectionAttribute::TXN_ISOLATION.0,
                std::ptr::without_provenance_mut::<c_void>(
                    crate::types::SQL_TXN_SERIALIZABLE as usize,
                ),
                0,
            );
            let state = with_handle::<B, ConnectionHandle<B>, _>(conn, |c| {
                c.diagnostics
                    .get(0)
                    .map(|r| r.sqlstate.as_str().to_owned())
                    .unwrap_or_default()
            });
            crate::test_utils::cleanup_connected_env_conn_stmt::<B>(env, conn, stmt);
            state
        }
    }

    /// Spec `SQLSetConnectAttr` HY011: "The *Attribute* argument was
    /// SQL_ATTR_TXN_ISOLATION, and a transaction was open." Unmarked, so the
    /// driver owes it.
    #[test]
    fn setting_txn_isolation_with_an_open_transaction_is_hy011() {
        unsafe {
            assert_eq!(
                isolation_after::<crate::test_utils::MockTxnPreserveBackend>(true),
                "HY011",
            );
        }
    }

    /// The control: manual-commit mode entered but no work done, so no
    /// transaction is open yet and HY011 must not fire. Without this, the test
    /// above would pass just as well if core rejected the attribute whenever
    /// autocommit was off.
    #[test]
    fn setting_txn_isolation_in_manual_commit_with_no_work_done_is_not_hy011() {
        unsafe {
            assert_ne!(
                isolation_after::<crate::test_utils::MockTxnPreserveBackend>(false),
                "HY011",
                "entering manual-commit mode does not by itself open a transaction",
            );
        }
    }

    /// `SQLEndTran` ends the transaction, so the attribute becomes settable
    /// again. Pins the clearing half: without it a connection would be locked
    /// out of isolation changes for the rest of its life after one statement.
    #[test]
    fn setting_txn_isolation_after_end_tran_is_not_hy011() {
        unsafe {
            type B = crate::test_utils::MockTxnPreserveBackend;
            let (env, conn, stmt) = crate::test_utils::alloc_connected_env_conn_stmt::<B>();

            assert_eq!(
                sql_set_connect_attr_w::<B>(
                    conn,
                    ConnectionAttribute::AUTOCOMMIT.0,
                    std::ptr::without_provenance_mut::<c_void>(SQL_AUTOCOMMIT_OFF),
                    0,
                ),
                SqlReturn::SUCCESS,
            );
            let sql: Vec<u16> = "UPDATE t SET c = 1".encode_utf16().collect();
            assert_eq!(
                crate::ffi::execute::sql_exec_direct_w::<B>(stmt, sql.as_ptr(), sql.len() as i32),
                SqlReturn::SUCCESS,
            );
            with_handle::<B, ConnectionHandle<B>, _>(conn, |c| {
                assert!(
                    c.txn_dirty,
                    "the execution was supposed to open a transaction"
                );
            });

            assert_eq!(
                crate::ffi::tran::sql_end_tran::<B>(
                    odbc_sys::HandleType::Dbc as i16,
                    conn,
                    odbc_sys::CompletionType::Commit as i16,
                ),
                SqlReturn::SUCCESS,
            );
            with_handle::<B, ConnectionHandle<B>, _>(conn, |c| {
                assert!(!c.txn_dirty, "a committed transaction is no longer open");
            });

            let _ = sql_set_connect_attr_w::<B>(
                conn,
                ConnectionAttribute::TXN_ISOLATION.0,
                std::ptr::without_provenance_mut::<c_void>(
                    crate::types::SQL_TXN_SERIALIZABLE as usize,
                ),
                0,
            );
            let state = with_handle::<B, ConnectionHandle<B>, _>(conn, |c| {
                c.diagnostics
                    .get(0)
                    .map(|r| r.sqlstate.as_str().to_owned())
                    .unwrap_or_default()
            });
            assert_ne!(
                state, "HY011",
                "SQLEndTran must clear the open-transaction state"
            );

            crate::test_utils::cleanup_connected_env_conn_stmt::<B>(env, conn, stmt);
        }
    }

    /// Spec `SQLSetConnectAttr` 24000: "The *Attribute* argument was
    /// SQL_ATTR_CURRENT_CATALOG, and a result set was pending." The row carries
    /// no `(DM)` marker, so the driver owes it.
    ///
    /// Driven through a real `SQLExecDirect` rather than by poking
    /// `cursor_open`, so it also pins that an ordinary execution is what makes a
    /// result set "pending": `MockCancelAwareBackend` reports one column, and
    /// core opens a cursor for any execution that has columns.
    #[test]
    fn setting_current_catalog_while_a_result_set_is_pending_is_24000() {
        unsafe {
            let (env, conn, stmt) =
                crate::test_utils::alloc_connected_env_conn_stmt::<MockCancelAwareBackend>();

            let sql: Vec<u16> = "SELECT 1".encode_utf16().collect();
            assert_eq!(
                crate::ffi::execute::sql_exec_direct_w::<MockCancelAwareBackend>(
                    stmt,
                    sql.as_ptr(),
                    sql.len() as i32,
                ),
                SqlReturn::SUCCESS,
            );
            with_handle::<
                MockCancelAwareBackend,
                crate::handles::StatementHandle<MockCancelAwareBackend>,
                _,
            >(stmt, |h| {
                assert!(h.cursor_open, "the execution was supposed to open a cursor");
            });

            let catalog: Vec<u16> = "other".encode_utf16().collect();
            let ret = sql_set_connect_attr_w::<MockCancelAwareBackend>(
                conn,
                ConnectionAttribute::CURRENT_CATALOG.0,
                catalog.as_ptr() as *mut c_void,
                (catalog.len() * 2) as i32,
            );
            assert_eq!(ret, SqlReturn::ERROR);

            let state = with_handle::<
                MockCancelAwareBackend,
                ConnectionHandle<MockCancelAwareBackend>,
                _,
            >(conn, |c| {
                c.diagnostics
                    .get(0)
                    .expect("a diagnostic record")
                    .sqlstate
                    .as_str()
                    .to_owned()
            });
            assert_eq!(state, "24000");

            crate::test_utils::cleanup_connected_env_conn_stmt::<MockCancelAwareBackend>(
                env, conn, stmt,
            );
        }
    }

    /// The control: the same connection with the same statement allocated on
    /// it, but no cursor open. Without this, the test above would still pass if
    /// the check rejected SQL_ATTR_CURRENT_CATALOG unconditionally.
    ///
    /// The backend leaves `set_current_catalog` defaulted, so the expected
    /// answer is that HYC00. The point is that it is *not* 24000, i.e. the
    /// request got past the cursor check and reached the backend.
    #[test]
    fn setting_current_catalog_with_no_cursor_open_is_not_24000() {
        unsafe {
            let (env, conn, stmt) =
                crate::test_utils::alloc_connected_env_conn_stmt::<MockCancelAwareBackend>();

            let catalog: Vec<u16> = "other".encode_utf16().collect();
            let ret = sql_set_connect_attr_w::<MockCancelAwareBackend>(
                conn,
                ConnectionAttribute::CURRENT_CATALOG.0,
                catalog.as_ptr() as *mut c_void,
                (catalog.len() * 2) as i32,
            );
            assert_eq!(
                ret,
                SqlReturn::ERROR,
                "the default set_current_catalog refuses"
            );

            let state = with_handle::<
                MockCancelAwareBackend,
                ConnectionHandle<MockCancelAwareBackend>,
                _,
            >(conn, |c| {
                c.diagnostics
                    .get(0)
                    .expect("a diagnostic record")
                    .sqlstate
                    .as_str()
                    .to_owned()
            });
            assert_ne!(
                state, "24000",
                "no cursor is open, so the pending-result-set check must not fire"
            );

            crate::test_utils::cleanup_connected_env_conn_stmt::<MockCancelAwareBackend>(
                env, conn, stmt,
            );
        }
    }

    #[test]
    fn get_connection_dead_on_an_unconnected_handle_is_false() {
        // A handle that never had a connection has not *lost* one, and
        // SQL_CD_TRUE asserts exactly that it was lost. `alloc_env_conn_for` does
        // not connect, so this is the no-connection branch specifically; the
        // two tests below cover the branches that reach the backend.
        unsafe {
            let (env, conn) = alloc_env_conn_for::<MockBackend>();
            let mut val: u32 = 99;
            let ret = sql_get_connect_attr_w::<MockBackend>(
                conn,
                ConnectionAttribute::CONNECTION_DEAD.0,
                &mut val as *mut u32 as *mut c_void,
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(val, SQL_CD_FALSE as u32);
            cleanup_env_conn_for::<MockBackend>(env, conn);
        }
    }

    /// Read `SQL_ATTR_CONNECTION_DEAD` over a genuinely connected handle of
    /// backend `B`.
    unsafe fn connection_dead_for<B: Backend>() -> u32 {
        unsafe {
            let (env, conn, stmt) = crate::test_utils::alloc_connected_env_conn_stmt::<B>();
            let mut val: u32 = 99;
            let ret = sql_get_connect_attr_w::<B>(
                conn,
                ConnectionAttribute::CONNECTION_DEAD.0,
                &mut val as *mut u32 as *mut c_void,
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            crate::test_utils::cleanup_connected_env_conn_stmt::<B>(env, conn, stmt);
            val
        }
    }

    #[test]
    fn a_backend_that_reports_a_lost_connection_is_read_as_sql_cd_true() {
        // What a connection pool reads before handing a connection out. Before
        // `Backend::connection_dead` existed this was hardcoded SQL_CD_FALSE,
        // so a pool would serve a connection whose socket had already closed.
        unsafe {
            assert_eq!(
                connection_dead_for::<crate::test_utils::MockDeadConnectionBackend>(),
                SQL_CD_TRUE as u32,
            );
        }
    }

    #[test]
    fn a_backend_with_no_liveness_signal_is_read_as_sql_cd_false() {
        // The control for the test above: same code path, same connected
        // handle, a backend that leaves `connection_dead` defaulted. Pins that
        // the answer moves with the backend rather than being hardcoded either
        // way.
        unsafe {
            assert_eq!(
                connection_dead_for::<crate::test_utils::MockNoQueryTimeoutBackend>(),
                SQL_CD_FALSE as u32,
            );
        }
    }

    #[test]
    fn set_unknown_attr_returns_success() {
        unsafe {
            let (env, conn) = alloc_env_conn_for::<MockBackend>();
            // Driver-specific attribute, must not fail
            let ret = sql_set_connect_attr_w::<MockBackend>(conn, 9999, std::ptr::null_mut(), 0);
            assert_eq!(ret, SqlReturn::SUCCESS);
            cleanup_env_conn_for::<MockBackend>(env, conn);
        }
    }

    /// `SQL_ATTR_CONNECTION_DEAD` is read-only, and the state core posts for a
    /// write is `HY024`: the identifier names a real connection attribute, so
    /// `HY092` would be the wrong half of the pair, and the objection is to the
    /// value having anywhere to go at all.
    #[test]
    fn set_connection_dead_is_error() {
        unsafe {
            let (env, conn) = alloc_env_conn_for::<MockBackend>();
            let ret = sql_set_connect_attr_w::<MockBackend>(
                conn,
                ConnectionAttribute::CONNECTION_DEAD.0,
                std::ptr::null_mut::<c_void>(),
                0,
            );
            assert_eq!(ret, SqlReturn::ERROR);
            assert_eq!(
                first_sqlstate::<MockBackend>(conn),
                crate::types::sql_state::INVALID_ATTRIBUTE_VALUE,
                "a write to a read-only attribute is HY024, not a generic HY000",
            );
            cleanup_env_conn_for::<MockBackend>(env, conn);
        }
    }

    /// Spec, `SQL_ATTR_PACKET_SIZE`: "If the application sets packet size after
    /// a connection has already been made, the driver will return SQLSTATE
    /// HY011 (Attribute cannot be set now)." The attribute table lists it as
    /// settable "Before" only.
    #[test]
    fn packet_size_after_connect_reports_hy011() {
        unsafe {
            let (env, conn) = alloc_env_conn_for::<MockBackend>();

            // Before connecting it is an ordinary stored attribute.
            assert_eq!(
                sql_set_connect_attr_w::<MockBackend>(
                    conn,
                    ConnectionAttribute::PACKET_SIZE.0,
                    std::ptr::without_provenance_mut(8192),
                    0,
                ),
                SqlReturn::SUCCESS
            );

            connect(conn);

            assert_eq!(
                sql_set_connect_attr_w::<MockBackend>(
                    conn,
                    ConnectionAttribute::PACKET_SIZE.0,
                    std::ptr::without_provenance_mut(4096),
                    0,
                ),
                SqlReturn::ERROR
            );
            with_handle::<MockBackend, ConnectionHandle<MockBackend>, _>(conn, |handle| {
                assert_eq!(
                    handle
                        .diagnostics
                        .get(0)
                        .expect("a HY011 record")
                        .sqlstate
                        .as_str(),
                    "HY011"
                );
            });

            cleanup_env_conn_for::<MockBackend>(env, conn);
        }
    }

    /// `SQLGetInfo(SQL_ASYNC_MODE)` reports `SQL_AM_NONE`, so there is no
    /// asynchronous execution to enable; and core enlists in no distributed
    /// transaction. Both are valid ODBC attributes this driver does not
    /// implement, which is the HYC00 row.
    #[test]
    fn attributes_core_does_not_implement_report_hyc00() {
        // (attribute, name, value)
        let cases: &[(i32, &str, usize)] = &[
            (
                ConnectionAttribute::ASYNC_ENABLE.0,
                "SQL_ATTR_ASYNC_ENABLE",
                1, // SQL_ASYNC_ENABLE_ON
            ),
            (
                ConnectionAttribute::ENLIST_IN_DTC.0,
                "SQL_ATTR_ENLIST_IN_DTC",
                1,
            ),
        ];
        for (attribute, name, value) in cases {
            unsafe {
                let (env, conn) = alloc_env_conn_for::<MockBackend>();
                let ret = sql_set_connect_attr_w::<MockBackend>(
                    conn,
                    *attribute,
                    std::ptr::without_provenance_mut(*value),
                    0,
                );
                assert_eq!(ret, SqlReturn::ERROR, "{name} was accepted");
                with_handle::<MockBackend, ConnectionHandle<MockBackend>, _>(conn, |handle| {
                    assert_eq!(
                        handle
                            .diagnostics
                            .get(0)
                            .unwrap_or_else(|| panic!("{name}: a HYC00 record"))
                            .sqlstate
                            .as_str(),
                        "HYC00",
                        "{name} posted the wrong SQLSTATE"
                    );
                });
                cleanup_env_conn_for::<MockBackend>(env, conn);
            }
        }
    }

    /// `SQL_ATTR_CURRENT_CATALOG` and `SQL_DATABASE_NAME` are one value under
    /// two names, so they must read the same two sources in the same order:
    /// what the application set, else what the session is actually using. With
    /// only the first, the attribute is write-only: it answers `""` while the
    /// info type answers the real catalog.
    #[test]
    fn current_catalog_falls_back_to_the_session_catalog() {
        unsafe {
            let (env, conn) = alloc_env_conn_for::<MockAltBackend>();
            assert_eq!(driver_connect::<MockAltBackend>(conn), SqlReturn::SUCCESS);

            let mut buf = [0u16; 64];
            let mut len: i32 = 0;
            assert_eq!(
                sql_get_connect_attr_w::<MockAltBackend>(
                    conn,
                    ConnectionAttribute::CURRENT_CATALOG.0,
                    buf.as_mut_ptr().cast(),
                    (buf.len() * 2) as i32,
                    &mut len,
                ),
                SqlReturn::SUCCESS
            );
            let attr = String::from_utf16_lossy(&buf[..(len / 2) as usize]);
            assert_eq!(
                attr, "alt_catalog",
                "the attribute must report the session catalog when the \
                 application has set none"
            );

            // The same value under its other name.
            let (_, info) = crate::conformance::observe_string_value::<MockAltBackend>(
                conn,
                crate::types::SQL_DATABASE_NAME,
            );
            assert_eq!(
                info, attr,
                "SQL_DATABASE_NAME and SQL_ATTR_CURRENT_CATALOG disagree"
            );

            cleanup_env_conn_for::<MockAltBackend>(env, conn);
        }
    }

    /// Spec, on a row carrying no `(DM)` marker: "The *Attribute* argument
    /// identified a connection attribute that required a string value, and the
    /// *ValuePtr* argument was a null pointer." So the check is the driver's.
    ///
    /// Treating a null as "remove the stored override and report success" would
    /// tell the application it had cleared a catalog the session was still
    /// using, and the spec defines no operation that unsets one. Neither
    /// psqlODBC nor MySQL Connector/ODBC implements null-as-clear either.
    #[test]
    fn a_null_value_ptr_for_current_catalog_is_hy009() {
        unsafe {
            let (env, conn) = alloc_env_conn_for::<MockBackend>();

            assert_eq!(
                sql_set_connect_attr_w::<MockBackend>(
                    conn,
                    ConnectionAttribute::CURRENT_CATALOG.0,
                    std::ptr::null_mut(),
                    SQL_NTS,
                ),
                SqlReturn::ERROR,
            );
            with_handle::<MockBackend, ConnectionHandle<MockBackend>, _>(conn, |h| {
                assert_eq!(
                    h.diagnostics
                        .get(0)
                        .expect("a diagnostic record")
                        .sqlstate
                        .as_str(),
                    "HY009",
                );
            });

            cleanup_env_conn_for::<MockBackend>(env, conn);
        }
    }

    /// `SQLGetConnectAttr`'s *BufferLength* is `SQLINTEGER`, so 64 KB is an
    /// ordinary thing for an application to offer for a catalog name and the
    /// spec defines no error for it: the argument is described only as "the
    /// length of \**ValuePtr*". Narrowing it to the `SQLSMALLINT` the shared
    /// string writer takes is what would turn it into an `HY000`.
    #[test]
    fn a_buffer_larger_than_i16_max_still_returns_the_catalog() {
        unsafe {
            let (env, conn) = alloc_env_conn_for::<MockAltBackend>();
            assert_eq!(driver_connect::<MockAltBackend>(conn), SqlReturn::SUCCESS);

            // 32768 code units is 65536 bytes: one past what an i16 can hold.
            let mut buf = vec![0u16; 32_768];
            let mut len: i32 = 0;
            assert_eq!(
                sql_get_connect_attr_w::<MockAltBackend>(
                    conn,
                    ConnectionAttribute::CURRENT_CATALOG.0,
                    buf.as_mut_ptr().cast(),
                    i32::try_from(buf.len() * 2).expect("length fits in i32"),
                    &mut len,
                ),
                SqlReturn::SUCCESS,
                "a large buffer is not a failure the spec defines",
            );
            assert_eq!(
                String::from_utf16_lossy(&buf[..(len / 2) as usize]),
                "alt_catalog",
            );

            cleanup_env_conn_for::<MockAltBackend>(env, conn);
        }
    }

    /// `SQL_ATTR_ASYNC_ENABLE` and `SQL_ATTR_ODBC_CURSORS` are the two
    /// connection attributes the spec declares `SQLULEN`: "A SQLULEN value
    /// that specifies whether a function called with a statement on the
    /// specified connection is executed asynchronously" and "An SQLULEN value
    /// specifying how the Driver Manager uses the ODBC cursor library". Every
    /// other integer-valued connection attribute is `SQLUINTEGER`, which is why
    /// this is a two-attribute exception here and a blanket rule for statement
    /// attributes.
    #[test]
    fn the_two_sqlulen_connection_attributes_are_written_at_full_width() {
        for (attribute, name, expected) in [
            (
                ConnectionAttribute::ODBC_CURSORS.0,
                "SQL_ATTR_ODBC_CURSORS",
                SQL_CUR_USE_DRIVER,
            ),
            (
                ConnectionAttribute::ASYNC_ENABLE.0,
                "SQL_ATTR_ASYNC_ENABLE",
                SQL_ASYNC_ENABLE_OFF,
            ),
        ] {
            unsafe {
                let (env, conn) = alloc_env_conn_for::<MockBackend>();
                let mut value: usize = usize::MAX;
                let ret = sql_get_connect_attr_w::<MockBackend>(
                    conn,
                    attribute,
                    std::ptr::from_mut(&mut value).cast(),
                    0,
                    std::ptr::null_mut(),
                );
                assert_eq!(ret, SqlReturn::SUCCESS);
                assert_eq!(
                    value, expected,
                    "{name}: the high half of the SQLULEN buffer kept its poison"
                );
                cleanup_env_conn_for::<MockBackend>(env, conn);
            }
        }
    }

    /// An attribute `SQLSetConnectAttr` stores is one `SQLGetConnectAttr` can
    /// report: the spec makes the getter the way an application reads back what
    /// it set, so a stored value that answers HY092 would be unreachable.
    #[test]
    fn every_stored_connection_attribute_is_readable() {
        // (attribute, name, a value the setter accepts)
        let cases: &[(i32, &str, usize)] = &[
            (
                ConnectionAttribute::ACCESS_MODE.0,
                "SQL_ATTR_ACCESS_MODE",
                1,
            ),
            (ConnectionAttribute::AUTOCOMMIT.0, "SQL_ATTR_AUTOCOMMIT", 1),
            (
                ConnectionAttribute::LOGIN_TIMEOUT.0,
                "SQL_ATTR_LOGIN_TIMEOUT",
                30,
            ),
            (
                ConnectionAttribute::CONNECTION_TIMEOUT.0,
                "SQL_ATTR_CONNECTION_TIMEOUT",
                30,
            ),
            (ConnectionAttribute::TRACE.0, "SQL_ATTR_TRACE", 0),
            (
                ConnectionAttribute::ODBC_CURSORS.0,
                "SQL_ATTR_ODBC_CURSORS",
                2,
            ),
            (
                ConnectionAttribute::PACKET_SIZE.0,
                "SQL_ATTR_PACKET_SIZE",
                8192,
            ),
            (
                ConnectionAttribute::METADATA_ID.0,
                "SQL_ATTR_METADATA_ID",
                1,
            ),
            (
                ConnectionAttribute::ASYNC_ENABLE.0,
                "SQL_ATTR_ASYNC_ENABLE",
                0,
            ),
            (
                ConnectionAttribute::TRANSLATE_OPTION.0,
                "SQL_ATTR_TRANSLATE_OPTION",
                7,
            ),
        ];
        for (attribute, name, value) in cases {
            unsafe {
                let (env, conn) = alloc_env_conn_for::<MockBackend>();
                assert_eq!(
                    sql_set_connect_attr_w::<MockBackend>(
                        conn,
                        *attribute,
                        std::ptr::without_provenance_mut(*value),
                        0,
                    ),
                    SqlReturn::SUCCESS,
                    "{name} was not accepted"
                );
                // An SQLULEN buffer, zeroed: two of these attributes are
                // SQLULEN and the rest SQLUINTEGER, and the spec tells
                // applications to "use a buffer of SQLULEN and initialize the
                // value to 0" precisely so one buffer serves both. A `u32`
                // here would be overflowed by the SQLULEN pair.
                let mut out: usize = 0;
                assert_eq!(
                    sql_get_connect_attr_w::<MockBackend>(
                        conn,
                        *attribute,
                        std::ptr::from_mut(&mut out).cast(),
                        0,
                        std::ptr::null_mut(),
                    ),
                    SqlReturn::SUCCESS,
                    "{name} was stored but cannot be read back"
                );
                assert_eq!(out, *value, "{name} read back a different value");
                cleanup_env_conn_for::<MockBackend>(env, conn);
            }
        }
    }

    /// Put the handle into a connected state so backend-dispatching attributes
    /// can be exercised. MockBackend's `connect` always succeeds.
    ///
    /// The connection string deliberately omits `DSN=`: that path calls into
    /// unixODBC's `SQLGetPrivateProfileStringW`, which Miri cannot execute.
    unsafe fn connect(conn: *mut c_void) {
        let cs: Vec<u16> = "Database=test"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let ret = unsafe {
            crate::ffi::connect::sql_driver_connect_w::<MockBackend>(
                conn,
                std::ptr::null_mut(),
                cs.as_ptr(),
                SQL_NTS as i16,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                0,
            )
        };
        assert_eq!(ret, SqlReturn::SUCCESS, "MockBackend connect failed");
    }

    #[test]
    fn set_autocommit_off_is_rejected_when_backend_cannot_honour_it() {
        // MockBackend uses the default `set_autocommit`, which reports HYC00 for
        // manual-commit mode. Accepting it would let an application believe a
        // rollback is available when every statement is already committed.
        unsafe {
            let (env, conn) = alloc_env_conn_for::<MockBackend>();
            connect(conn);
            let ret = sql_set_connect_attr_w::<MockBackend>(
                conn,
                ConnectionAttribute::AUTOCOMMIT.0,
                std::ptr::without_provenance_mut(SQL_AUTOCOMMIT_OFF),
                0,
            );
            assert_eq!(ret, SqlReturn::ERROR, "manual-commit was accepted silently");

            with_handle::<MockBackend, ConnectionHandle<MockBackend>, _>(conn, |handle| {
                assert!(
                    !handle.diagnostics.is_empty(),
                    "no HYC00 diagnostic was recorded"
                );
            });

            cleanup_env_conn_for::<MockBackend>(env, conn);
        }
    }

    #[test]
    fn set_autocommit_on_is_accepted() {
        unsafe {
            let (env, conn) = alloc_env_conn_for::<MockBackend>();
            connect(conn);
            let ret = sql_set_connect_attr_w::<MockBackend>(
                conn,
                ConnectionAttribute::AUTOCOMMIT.0,
                std::ptr::without_provenance_mut(SQL_AUTOCOMMIT_ON),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            cleanup_env_conn_for::<MockBackend>(env, conn);
        }
    }

    #[test]
    fn autocommit_off_set_before_connect_is_deferred_then_applied() {
        // The spec lists SQL_ATTR_AUTOCOMMIT as settable before connecting, so
        // the pre-connect call must succeed, but the value still has to reach
        // the backend, so the connect itself fails for a backend that cannot
        // honour manual-commit.
        unsafe {
            let (env, conn) = alloc_env_conn_for::<MockBackend>();
            assert_eq!(
                sql_set_connect_attr_w::<MockBackend>(
                    conn,
                    ConnectionAttribute::AUTOCOMMIT.0,
                    std::ptr::without_provenance_mut(SQL_AUTOCOMMIT_OFF),
                    0,
                ),
                SqlReturn::SUCCESS,
                "pre-connect set must be accepted and deferred"
            );

            let cs: Vec<u16> = "Database=test"
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let ret = crate::ffi::connect::sql_driver_connect_w::<MockBackend>(
                conn,
                std::ptr::null_mut(),
                cs.as_ptr(),
                SQL_NTS as i16,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                0,
            );
            assert_eq!(
                ret,
                SqlReturn::ERROR,
                "deferred manual-commit was silently dropped at connect"
            );

            cleanup_env_conn_for::<MockBackend>(env, conn);
        }
    }

    #[test]
    fn set_autocommit_invalid_value_returns_hy024() {
        unsafe {
            let (env, conn) = alloc_env_conn_for::<MockBackend>();
            let ret = sql_set_connect_attr_w::<MockBackend>(
                conn,
                ConnectionAttribute::AUTOCOMMIT.0,
                99usize as *mut c_void,
                0,
            );
            assert_eq!(ret, SqlReturn::ERROR);
            assert_eq!(
                first_sqlstate::<MockBackend>(conn),
                crate::types::sql_state::INVALID_ATTRIBUTE_VALUE,
                "the state this test's name claims"
            );
            cleanup_env_conn_for::<MockBackend>(env, conn);
        }
    }

    #[test]
    fn set_access_mode_invalid_value_returns_hy024() {
        unsafe {
            let (env, conn) = alloc_env_conn_for::<MockBackend>();
            let ret = sql_set_connect_attr_w::<MockBackend>(
                conn,
                ConnectionAttribute::ACCESS_MODE.0,
                5usize as *mut c_void,
                0,
            );
            assert_eq!(ret, SqlReturn::ERROR);
            assert_eq!(
                first_sqlstate::<MockBackend>(conn),
                crate::types::sql_state::INVALID_ATTRIBUTE_VALUE,
                "the state this test's name claims"
            );
            cleanup_env_conn_for::<MockBackend>(env, conn);
        }
    }

    #[test]
    fn set_trace_invalid_value_returns_hy024() {
        unsafe {
            let (env, conn) = alloc_env_conn_for::<MockBackend>();
            let ret = sql_set_connect_attr_w::<MockBackend>(
                conn,
                ConnectionAttribute::TRACE.0,
                2usize as *mut c_void,
                0,
            );
            assert_eq!(ret, SqlReturn::ERROR);
            assert_eq!(
                first_sqlstate::<MockBackend>(conn),
                crate::types::sql_state::INVALID_ATTRIBUTE_VALUE,
                "the state this test's name claims"
            );
            cleanup_env_conn_for::<MockBackend>(env, conn);
        }
    }

    #[test]
    fn set_odbc_cursors_invalid_value_returns_hy024() {
        unsafe {
            let (env, conn) = alloc_env_conn_for::<MockBackend>();
            let ret = sql_set_connect_attr_w::<MockBackend>(
                conn,
                ConnectionAttribute::ODBC_CURSORS.0,
                99usize as *mut c_void,
                0,
            );
            assert_eq!(ret, SqlReturn::ERROR);
            assert_eq!(
                first_sqlstate::<MockBackend>(conn),
                crate::types::sql_state::INVALID_ATTRIBUTE_VALUE,
                "the state this test's name claims"
            );
            cleanup_env_conn_for::<MockBackend>(env, conn);
        }
    }

    #[test]
    fn set_discrete_attrs_valid_values_succeed() {
        unsafe {
            let (env, conn) = alloc_env_conn_for::<MockBackend>();
            // ACCESS_MODE: 0 (read/write) and 1 (read-only) are valid.
            // Value 0: null_mut encodes integer zero (ODBC integer-as-pointer convention).
            let ret = sql_set_connect_attr_w::<MockBackend>(
                conn,
                ConnectionAttribute::ACCESS_MODE.0,
                std::ptr::null_mut::<c_void>(),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            // Value 1: dangling_mut encodes integer one.
            let ret = sql_set_connect_attr_w::<MockBackend>(
                conn,
                ConnectionAttribute::ACCESS_MODE.0,
                std::ptr::dangling_mut::<c_void>(),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            // AUTOCOMMIT: 0 (off) is valid.
            let ret = sql_set_connect_attr_w::<MockBackend>(
                conn,
                ConnectionAttribute::AUTOCOMMIT.0,
                std::ptr::null_mut::<c_void>(),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            // TRACE: 1 (on) is valid.
            let ret = sql_set_connect_attr_w::<MockBackend>(
                conn,
                ConnectionAttribute::TRACE.0,
                std::ptr::dangling_mut::<c_void>(),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            // ODBC_CURSORS: 0 (use-if-needed), 1 (use-odbc), and 2 (use-driver) are valid.
            let ret = sql_set_connect_attr_w::<MockBackend>(
                conn,
                ConnectionAttribute::ODBC_CURSORS.0,
                std::ptr::null_mut::<c_void>(), // 0
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS, "ODBC_CURSORS=0 should succeed");
            let ret = sql_set_connect_attr_w::<MockBackend>(
                conn,
                ConnectionAttribute::ODBC_CURSORS.0,
                std::ptr::dangling_mut::<c_void>(), // 1
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS, "ODBC_CURSORS=1 should succeed");
            let ret = sql_set_connect_attr_w::<MockBackend>(
                conn,
                ConnectionAttribute::ODBC_CURSORS.0,
                2usize as *mut c_void, // 2
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS, "ODBC_CURSORS=2 should succeed");
            cleanup_env_conn_for::<MockBackend>(env, conn);
        }
    }

    #[test]
    fn null_handle_returns_invalid() {
        unsafe {
            let ret = sql_set_connect_attr_w::<MockBackend>(
                std::ptr::null_mut(),
                ConnectionAttribute::AUTOCOMMIT.0,
                std::ptr::dangling_mut::<c_void>(),
                0,
            );
            assert_eq!(ret, SqlReturn::INVALID_HANDLE);
        }
    }

    #[test]
    fn get_unknown_attribute_returns_hy092() {
        // Spec HY092: unrecognised attribute identifiers return ERROR.
        unsafe {
            let (env, conn) = alloc_env_conn_for::<MockBackend>();
            let mut val: u32 = 0;
            let ret = sql_get_connect_attr_w::<MockBackend>(
                conn,
                99999, // unknown attribute
                &mut val as *mut u32 as *mut c_void,
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::ERROR);
            assert_eq!(
                first_sqlstate::<MockBackend>(conn),
                crate::types::sql_state::INVALID_ATTRIBUTE_OPTION_IDENTIFIER,
                "the state this test's name claims"
            );
            cleanup_env_conn_for::<MockBackend>(env, conn);
        }
    }

    /// The spec names both rows and distinguishes them precisely: `HY092` is
    /// "the value specified for the argument Attribute was not valid for the
    /// version of ODBC supported by the driver", while `HYC00` is "...was a
    /// valid ODBC connection attribute for the version of ODBC supported by the
    /// driver, but was not supported by the driver". These five are all spec
    /// attributes core does not answer, so they are the second.
    #[test]
    fn a_valid_but_unsupported_connection_attribute_is_hyc00() {
        unsafe {
            let (env, conn) = alloc_env_conn_for::<MockBackend>();
            for (attribute, name) in [
                (ConnectionAttribute::QUIET_MODE, "SQL_ATTR_QUIET_MODE"),
                (ConnectionAttribute::TRACEFILE, "SQL_ATTR_TRACEFILE"),
                (ConnectionAttribute::TRANSLATE_LIB, "SQL_ATTR_TRANSLATE_LIB"),
                (ConnectionAttribute::ENLIST_IN_DTC, "SQL_ATTR_ENLIST_IN_DTC"),
                (
                    ConnectionAttribute::ASYNC_DBC_FUNCTIONS_ENABLE,
                    "SQL_ATTR_ASYNC_DBC_FUNCTIONS_ENABLE",
                ),
            ] {
                let mut value: u32 = 0;
                assert_eq!(
                    sql_get_connect_attr_w::<MockBackend>(
                        conn,
                        attribute.0,
                        std::ptr::from_mut(&mut value).cast::<c_void>(),
                        0,
                        std::ptr::null_mut(),
                    ),
                    SqlReturn::ERROR,
                    "{name}"
                );
                with_handle::<MockBackend, ConnectionHandle<MockBackend>, _>(conn, |h| {
                    assert_eq!(
                        h.diagnostics
                            .get(0)
                            .expect("a diagnostic record")
                            .sqlstate
                            .as_str(),
                        "HYC00",
                        "{name} must report the unsupported-feature state, not the \
                         unknown-identifier one",
                    );
                });
            }
            cleanup_env_conn_for::<MockBackend>(env, conn);
        }
    }

    /// The other side of the line: a number that is not an ODBC connection
    /// attribute at all stays `HY092`. Without this, widening `HYC00` to
    /// everything would pass the test above.
    #[test]
    fn an_identifier_that_is_not_a_connection_attribute_is_still_hy092() {
        const NOT_A_CONNECTION_ATTRIBUTE: i32 = 424_242;

        unsafe {
            let (env, conn) = alloc_env_conn_for::<MockBackend>();
            let mut value: u32 = 0;
            assert_eq!(
                sql_get_connect_attr_w::<MockBackend>(
                    conn,
                    NOT_A_CONNECTION_ATTRIBUTE,
                    std::ptr::from_mut(&mut value).cast::<c_void>(),
                    0,
                    std::ptr::null_mut(),
                ),
                SqlReturn::ERROR,
            );
            with_handle::<MockBackend, ConnectionHandle<MockBackend>, _>(conn, |h| {
                assert_eq!(
                    h.diagnostics
                        .get(0)
                        .expect("a diagnostic record")
                        .sqlstate
                        .as_str(),
                    "HY092",
                );
            });
            cleanup_env_conn_for::<MockBackend>(env, conn);
        }
    }

    // -----------------------------------------------------------------------
    // SQL_ATTR_TXN_ISOLATION
    // -----------------------------------------------------------------------

    /// Reads the SQLSTATE of the connection's first diagnostic record.
    unsafe fn first_sqlstate<B: Backend>(conn: *mut c_void) -> String {
        let mut state = [0u16; 6];
        let mut native_err: i32 = 0;
        let mut msg = [0u16; 256];
        let mut msg_len: i16 = 0;
        let ret = unsafe {
            crate::ffi::diag::sql_get_diag_rec_w::<B>(
                odbc_sys::HandleType::Dbc as i16,
                conn,
                1,
                state.as_mut_ptr(),
                &mut native_err,
                msg.as_mut_ptr(),
                msg.len() as i16,
                &mut msg_len,
            )
        };
        assert_eq!(ret, SqlReturn::SUCCESS, "expected a diagnostic record");
        String::from_utf16_lossy(&state[..5])
    }

    /// The isolation level the backend actually had applied to it, read out of
    /// the connection the handle owns. This is what separates "core stored the
    /// value" from "the data source runs at it".
    unsafe fn applied_isolation(conn: *mut c_void) -> u32 {
        with_handle::<MockIsolationBackend, ConnectionHandle<MockIsolationBackend>, _>(
            conn,
            |handle| {
                let connection: &MockIsolationConnection =
                    handle.connection.as_ref().expect("not connected");
                connection.applied.load(std::sync::atomic::Ordering::SeqCst)
            },
        )
    }

    unsafe fn driver_connect<B: Backend>(conn: *mut c_void) -> SqlReturn {
        let cs: Vec<u16> = "Database=test"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            crate::ffi::connect::sql_driver_connect_w::<B>(
                conn,
                std::ptr::null_mut(),
                cs.as_ptr(),
                SQL_NTS as i16,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                0,
            )
        }
    }

    /// An unset `SQL_ATTR_TXN_ISOLATION` must report the level the backend
    /// declares, not a constant. Answering one unconditionally (say
    /// `SQL_TXN_READ_COMMITTED`) makes a backend that reports
    /// `SQL_TXN_SERIALIZABLE` for `SQL_DEFAULT_TXN_ISOLATION` contradict itself
    /// on the same connection.
    ///
    /// Connected first, because the declared default is the data source's and
    /// [`Backend::default_txn_isolation`] takes the connection to read it from.
    #[test]
    fn get_txn_isolation_default_comes_from_the_backend() {
        unsafe {
            let (env, conn) = alloc_env_conn_for::<MockBackend>();
            assert_eq!(driver_connect::<MockBackend>(conn), SqlReturn::SUCCESS);
            let mut val: u32 = 99;
            let ret = sql_get_connect_attr_w::<MockBackend>(
                conn,
                ConnectionAttribute::TXN_ISOLATION.0,
                &mut val as *mut u32 as *mut c_void,
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(
                val,
                MockBackend::default_txn_isolation(&MockConnection),
                "unset SQL_ATTR_TXN_ISOLATION ignored Backend::default_txn_isolation"
            );
            assert_ne!(
                val, SQL_TXN_READ_COMMITTED,
                "reported a constant rather than the backend's declared level"
            );
            cleanup_env_conn_for::<MockBackend>(env, conn);
        }
    }

    /// The spec's HY024 row makes validating this the driver's job:
    /// "For all other connection and statement attributes, the driver must
    /// verify the value specified in ValuePtr". A level outside
    /// `SQL_TXN_ISOLATION_OPTION` is not one the data source can run at.
    ///
    /// Connected first: the supported set is the data source's, so a level set
    /// before there is a connection is checked only for naming exactly one
    /// level, and against the set by `apply_pending_txn_isolation` at connect.
    #[test]
    fn set_txn_isolation_rejects_a_level_the_backend_does_not_support() {
        unsafe {
            let (env, conn) = alloc_env_conn_for::<MockBackend>();
            assert_eq!(driver_connect::<MockBackend>(conn), SqlReturn::SUCCESS);
            // MockBackend declares SERIALIZABLE only.
            let ret = sql_set_connect_attr_w::<MockBackend>(
                conn,
                ConnectionAttribute::TXN_ISOLATION.0,
                std::ptr::without_provenance_mut(SQL_TXN_READ_COMMITTED as usize),
                0,
            );
            assert_eq!(ret, SqlReturn::ERROR);
            assert_eq!(first_sqlstate::<MockBackend>(conn), "HY024");
            cleanup_env_conn_for::<MockBackend>(env, conn);
        }
    }

    /// The attribute sets *a* level, not a set of them, so a value with more
    /// than one `SQL_TXN_*` bit is invalid even when every bit is supported.
    #[test]
    fn set_txn_isolation_rejects_a_multi_bit_value() {
        unsafe {
            let (env, conn) = alloc_env_conn_for::<MockIsolationBackend>();
            let ret = sql_set_connect_attr_w::<MockIsolationBackend>(
                conn,
                ConnectionAttribute::TXN_ISOLATION.0,
                std::ptr::without_provenance_mut(
                    (SQL_TXN_READ_COMMITTED | SQL_TXN_SERIALIZABLE) as usize,
                ),
                0,
            );
            assert_eq!(ret, SqlReturn::ERROR);
            assert_eq!(first_sqlstate::<MockIsolationBackend>(conn), "HY024");
            cleanup_env_conn_for::<MockIsolationBackend>(env, conn);
        }
    }

    /// A supported level is accepted and reads back unchanged.
    #[test]
    fn set_txn_isolation_accepts_a_supported_level() {
        unsafe {
            let (env, conn) = alloc_env_conn_for::<MockBackend>();
            let ret = sql_set_connect_attr_w::<MockBackend>(
                conn,
                ConnectionAttribute::TXN_ISOLATION.0,
                std::ptr::without_provenance_mut(SQL_TXN_SERIALIZABLE as usize),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let mut val: u32 = 0;
            let ret = sql_get_connect_attr_w::<MockBackend>(
                conn,
                ConnectionAttribute::TXN_ISOLATION.0,
                &mut val as *mut u32 as *mut c_void,
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(val, SQL_TXN_SERIALIZABLE);
            cleanup_env_conn_for::<MockBackend>(env, conn);
        }
    }

    /// The stored value also has to *reach the data source*. An
    /// application that sets REPEATABLE READ, gets SQL_SUCCESS and reads its
    /// own value back must not be talking to a connection still running at
    /// whatever it always ran at.
    #[test]
    fn set_txn_isolation_reaches_the_backend() {
        unsafe {
            let (env, conn) = alloc_env_conn_for::<MockIsolationBackend>();
            assert_eq!(
                driver_connect::<MockIsolationBackend>(conn),
                SqlReturn::SUCCESS
            );

            let ret = sql_set_connect_attr_w::<MockIsolationBackend>(
                conn,
                ConnectionAttribute::TXN_ISOLATION.0,
                std::ptr::without_provenance_mut(SQL_TXN_REPEATABLE_READ as usize),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(
                applied_isolation(conn),
                SQL_TXN_REPEATABLE_READ,
                "the level never reached the backend"
            );

            cleanup_env_conn_for::<MockIsolationBackend>(env, conn);
        }
    }

    /// The spec lists SQL_ATTR_TXN_ISOLATION as settable "Either" side of the
    /// connection, so a level set before connecting must still be applied once
    /// the connection exists, the same contract as SQL_ATTR_AUTOCOMMIT.
    #[test]
    fn txn_isolation_set_before_connect_is_applied_on_connect() {
        unsafe {
            let (env, conn) = alloc_env_conn_for::<MockIsolationBackend>();
            assert_eq!(
                sql_set_connect_attr_w::<MockIsolationBackend>(
                    conn,
                    ConnectionAttribute::TXN_ISOLATION.0,
                    std::ptr::without_provenance_mut(SQL_TXN_REPEATABLE_READ as usize),
                    0,
                ),
                SqlReturn::SUCCESS,
                "pre-connect set must be accepted and deferred"
            );
            assert_eq!(
                driver_connect::<MockIsolationBackend>(conn),
                SqlReturn::SUCCESS
            );
            assert_eq!(
                applied_isolation(conn),
                SQL_TXN_REPEATABLE_READ,
                "deferred isolation level was silently dropped at connect"
            );

            cleanup_env_conn_for::<MockIsolationBackend>(env, conn);
        }
    }

    /// A backend declaring more than one level but not implementing
    /// `set_txn_isolation` must fail loudly rather than accept a level it
    /// cannot apply, rather than lie about the level in force.
    #[test]
    fn multi_level_backend_without_the_hook_cannot_switch_levels() {
        unsafe {
            let (env, conn) = alloc_env_conn_for::<MockUnappliedIsolationBackend>();
            assert_eq!(
                driver_connect::<MockUnappliedIsolationBackend>(conn),
                SqlReturn::SUCCESS
            );
            let ret = sql_set_connect_attr_w::<MockUnappliedIsolationBackend>(
                conn,
                ConnectionAttribute::TXN_ISOLATION.0,
                std::ptr::without_provenance_mut(SQL_TXN_SERIALIZABLE as usize),
                0,
            );
            assert_eq!(
                ret,
                SqlReturn::ERROR,
                "accepted a level the backend has no way to apply"
            );

            cleanup_env_conn_for::<MockUnappliedIsolationBackend>(env, conn);
        }
    }

    // -----------------------------------------------------------------------
    // 3D000 invalid catalog name
    // -----------------------------------------------------------------------

    /// Set `SQL_ATTR_CURRENT_CATALOG` to `catalog` on a connected handle and
    /// return the call's `SqlReturn` plus the first diagnostic's SQLSTATE.
    unsafe fn set_catalog<B: Backend>(
        conn: *mut c_void,
        catalog: &str,
    ) -> (SqlReturn, Option<String>) {
        unsafe {
            let wide: Vec<u16> = catalog.encode_utf16().collect();
            let ret = sql_set_connect_attr_w::<B>(
                conn,
                ConnectionAttribute::CURRENT_CATALOG.0,
                wide.as_ptr() as *mut c_void,
                i32::try_from(wide.len() * 2).expect("length fits in i32"),
            );
            let state = with_handle::<B, ConnectionHandle<B>, _>(conn, |h| {
                h.diagnostics.get(0).map(|d| d.sqlstate.as_str().to_owned())
            });
            (ret, state)
        }
    }

    /// `SQL_ATTR_CURRENT_CATALOG` is the one connection attribute whose value
    /// is a string, so it is the one that resolves `SQL_NTS`. A value running to
    /// `MAX_NTS_SCAN` is `HY090`, and no catalog is stored.
    ///
    /// The buffer is exactly the cap, so an over-read is a heap overflow Miri
    /// sees rather than a longer catalog name.
    #[test]
    fn set_current_catalog_refuses_an_nts_value_that_runs_to_the_scan_cap() {
        unsafe {
            let (env, conn) = alloc_env_conn_for::<MockBackend>();
            let wide = vec![b'a' as u16; crate::utf16::MAX_NTS_SCAN];

            assert_eq!(
                sql_set_connect_attr_w::<MockBackend>(
                    conn,
                    ConnectionAttribute::CURRENT_CATALOG.0,
                    wide.as_ptr().cast_mut().cast::<c_void>(),
                    SQL_NTS,
                ),
                SqlReturn::ERROR,
            );
            assert_eq!(
                first_sqlstate::<MockBackend>(conn),
                crate::types::sql_state::INVALID_STRING_OR_BUFFER_LENGTH
            );
            with_handle::<MockBackend, ConnectionHandle<MockBackend>, _>(conn, |h| {
                assert!(
                    !h.attr_strings
                        .contains_key(&ConnectionAttribute::CURRENT_CATALOG.0),
                    "a refused value must not be stored",
                );
            });

            cleanup_env_conn_for::<MockBackend>(env, conn);
        }
    }

    /// Core calls `Backend::set_current_catalog` and stores the value only if
    /// that succeeds, so the catalog string is never stored unvalidated. The
    /// spec's row carries no `(DM)` marker, so the state is the driver's, and
    /// core has no way to produce it: only the data source knows which catalogs
    /// exist. This pins the propagation the contract relies on.
    #[test]
    fn set_current_catalog_propagates_the_backends_3d000() {
        unsafe {
            let (env, conn) = alloc_env_conn_for::<MockCatalogRejectingBackend>();
            assert_eq!(
                driver_connect::<MockCatalogRejectingBackend>(conn),
                SqlReturn::SUCCESS,
                "precondition: connected, so the hook is reached",
            );

            let (ret, state) = set_catalog::<MockCatalogRejectingBackend>(conn, "nope");
            assert_eq!(ret, SqlReturn::ERROR);
            assert_eq!(
                state.as_deref(),
                Some("3D000"),
                "the data source's verdict on the catalog name, not a state core invented",
            );

            cleanup_env_conn_for::<MockCatalogRejectingBackend>(env, conn);
        }
    }

    /// The other half: a catalog the data source accepts is stored, so
    /// `SQLGetConnectAttr` reads it back. Without this, the test above could
    /// not tell "core propagated the backend's verdict" from "core rejects
    /// every catalog".
    #[test]
    fn set_current_catalog_stores_a_catalog_the_backend_accepted() {
        unsafe {
            let (env, conn) = alloc_env_conn_for::<MockCatalogRejectingBackend>();
            assert_eq!(
                driver_connect::<MockCatalogRejectingBackend>(conn),
                SqlReturn::SUCCESS,
                "precondition: connected, so the hook is reached",
            );

            let (ret, _) = set_catalog::<MockCatalogRejectingBackend>(conn, "good");
            assert_eq!(ret, SqlReturn::SUCCESS);

            let stored = with_handle::<
                MockCatalogRejectingBackend,
                ConnectionHandle<MockCatalogRejectingBackend>,
                _,
            >(conn, |h| {
                h.attr_strings
                    .get(&ConnectionAttribute::CURRENT_CATALOG.0)
                    .cloned()
            });
            assert_eq!(
                stored.as_deref(),
                Some("good"),
                "core stores the value only once the data source agreed to it",
            );

            cleanup_env_conn_for::<MockCatalogRejectingBackend>(env, conn);
        }
    }

    /// A catalog set *before* connecting is applied by
    /// `apply_pending_connect_attrs`, so its `3D000` surfaces from the connect
    /// function rather than from `SQLSetConnectAttr`.
    ///
    /// Worth pinning because `SQLDriverConnect`'s own diagnostics table has no
    /// `3D000` row: the state is real and reported anyway, since degrading it
    /// would tell the application its connection failed for some unrelated
    /// reason. The spec lists this attribute as settable either side of a
    /// connection and notes interoperable applications set it *before*, so this
    /// is the path that matters most.
    #[test]
    fn a_pre_connect_catalog_the_backend_rejects_fails_the_connect_with_3d000() {
        unsafe {
            let (env, conn) = alloc_env_conn_for::<MockCatalogRejectingBackend>();

            let (ret, _) = set_catalog::<MockCatalogRejectingBackend>(conn, "nope");
            assert_eq!(
                ret,
                SqlReturn::SUCCESS,
                "precondition: with no connection yet the value is only stored",
            );

            assert_eq!(
                driver_connect::<MockCatalogRejectingBackend>(conn),
                SqlReturn::ERROR,
                "a catalog the data source rejects must not be reported as applied",
            );
            let state = with_handle::<
                MockCatalogRejectingBackend,
                ConnectionHandle<MockCatalogRejectingBackend>,
                _,
            >(conn, |h| {
                h.diagnostics.get(0).map(|d| d.sqlstate.as_str().to_owned())
            });
            assert_eq!(state.as_deref(), Some("3D000"));

            cleanup_env_conn_for::<MockCatalogRejectingBackend>(env, conn);
        }
    }
}
