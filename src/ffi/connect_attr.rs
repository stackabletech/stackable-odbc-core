//! Generic implementations of SQLSetConnectAttrW and SQLGetConnectAttrW.

use std::borrow::Cow;
use std::ffi::c_void;

use odbc_sys::ConnectionAttribute;

use crate::backend::Backend;
use crate::errors::{IntoOdbc, OdbcError};
use crate::handles::ConnectionHandle;
use crate::panic::panic_safe;
use crate::types::{
    SQL_AUTOCOMMIT_OFF, SQL_AUTOCOMMIT_ON, SQL_CD_FALSE, SQL_FALSE, SQL_NTS, SqlReturn, SqlState,
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
    apply_pending_current_catalog::<B>(handle)
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
/// Two things make a value invalid. It must name exactly one level — the
/// attribute selects *a* level, so a value with several `SQL_TXN_*` bits set
/// is meaningless even when each bit is individually supported — and that
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
/// - 24000 Invalid cursor state: not returned. Connection-level result-set state
///   is not currently tracked. Deferred.
/// - 25000 Illegal operation while in a local transaction: not applicable;
///   distributed transactions (DTC) are not supported, and
///   `SQL_ATTR_ENLIST_IN_DTC` reports HYC00 before a transaction can be
///   enlisted in one.
/// - 3D000 Invalid catalog name: not returned; the catalog string is stored
///   verbatim without validation.
/// - HY000 General error: returned for unexpected internal errors.
/// - HY001 Memory allocation error: not returned; Rust panics on allocation
///   failure, which is caught by `panic_safe` and converted to `SQL_ERROR`/HY000.
/// - HY008: Operation canceled; not returned here. Cancelling a connection-level call needs
///   `SQLCancelHandle` on a connection handle, which this driver does not export, so no cancel
///   token exists for this call to observe — `SQLCancel` takes a statement handle and cannot
///   reach one. The asynchronous clause is likewise inapplicable: core never returns
///   `SQL_STILL_EXECUTING`.
/// - HY009 Invalid use of null pointer: HY009 is not applicable here:
///   `SQL_ATTR_CURRENT_CATALOG` is the only string attribute handled, and null
///   means "clear the catalog" (a valid operation).
/// - HY010 Function sequence error: (driver-manager-handled; not returned here).
/// - HY011 Attribute cannot be set now: returned for `SQL_ATTR_PACKET_SIZE`
///   once the connection is open, which the spec states directly — "if the
///   application sets packet size after a connection has already been made,
///   the driver will return SQLSTATE HY011". Not returned for
///   `SQL_ATTR_TXN_ISOLATION` with an open transaction: that needs a
///   transaction-state flag on `ConnectionHandle`, which is not tracked.
///   Deferred.
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
/// - HY090 Invalid string or buffer length: (driver-manager-handled; not returned
///   here). A `string_length < 0` check for `SQL_ATTR_CURRENT_CATALOG` is
///   performed as a defensive measure.
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
///   *value* — an isolation level the data source cannot run at is HY024
///   above, not HYC00.
/// - HYT01 Connection timeout expired: not returned; this function does not wait
///   on the data source.
/// - IM001 Driver does not support this function: (driver-manager-handled; not
///   returned here).
/// - IM009 Unable to load translation DLL: not applicable; translation DLLs are
///   not supported (`SQL_ATTR_TRANSLATE_LIB` is accepted silently).
/// - IM017 Polling is disabled in asynchronous notification mode:
///   (driver-manager-handled; not returned here).
/// - IM018 SQLCompleteAsync has not been called:
///   (driver-manager-handled; not returned here).
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
            let conn = scope.get::<ConnectionHandle<B>>(connection_handle)?;
            conn.diagnostics.clear();

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

                // String-valued: SQL_ATTR_CURRENT_CATALOG — decode UTF-16, then
                // ask the backend to switch. Storing without switching would
                // tell an application its unqualified names now resolve
                // somewhere they do not; a backend that cannot switch reports
                // HYC00 from the default `set_current_catalog`, and nothing is
                // stored.
                _ if attr == ConnectionAttribute::CURRENT_CATALOG => {
                    if value_ptr.is_null() {
                        conn.attr_strings.remove(&attribute);
                    } else {
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
                        // spec lists this attribute as settable either side of
                        // one.
                        if let Some(connection) = conn.connection.as_ref() {
                            B::set_current_catalog(connection, &s).into_odbc()?;
                        }
                        conn.attr_strings.insert(attribute, s);
                    }
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
///   communicate with the data source.
/// - HY000 General error: returned for unexpected internal errors.
/// - HY001 Memory allocation error: not returned; Rust panics on allocation
///   failure, which is caught by `panic_safe` and converted to `SQL_ERROR`/HY000.
/// - HY010 Function sequence error: (driver-manager-handled; not returned here).
/// - HY013 Memory management error: not returned; Rust panics on memory errors,
///   caught by `panic_safe` and converted to `SQL_ERROR`/HY000.
/// - HY090 Invalid string or buffer length: (driver-manager-handled; not returned
///   here except for negative `buffer_length`). A `buffer_length < 0` check for
///   `SQL_ATTR_CURRENT_CATALOG` is performed as a defensive measure and returns
///   HY090. However, when `buffer_length` is valid but exceeds the maximum
///   (32767 chars), HY000 (general error) is returned instead, as the buffer
///   is not "invalid" but unsupported.
/// - HY092 Invalid attribute/option identifier: returned for unrecognised
///   attribute identifiers in the catch-all branch.
/// - HY114 Driver does not support connection-level asynchronous function execution:
///   (driver-manager-handled; not returned here).
/// - HY117 Connection is suspended due to unknown transaction state:
///   (driver-manager-handled; not returned here).
/// - HYC00 Optional feature not implemented: returned for valid ODBC connection
///   attributes that are not supported.
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
            // that applies to statement attributes — where *no* non-pointer
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

                // SQL_ATTR_CONNECTION_DEAD: always report connection is alive.
                _ if attr == ConnectionAttribute::CONNECTION_DEAD => {
                    write_u32(SQL_CD_FALSE as u32);
                    Ok(SqlReturn::SUCCESS)
                }

                // SQL_ATTR_CURRENT_CATALOG: what the application set, else what
                // the session is actually using. Without the second half this
                // attribute is write-only — it answers "" while
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
                    let buf_u16 = i16::try_from(buffer_length / 2).map_err(|_| {
                        OdbcError::general(
                            format!("Buffer length {buffer_length} exceeds driver maximum (32767 chars for string attributes)"),
                            SqlState::general_error(),
                        )
                    })?;
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

                _ => Err(OdbcError::general(
                    format!("SQLGetConnectAttrW: unsupported attribute {attr:?}"),
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
        MockAltBackend, MockBackend, MockConnection, MockIsolationBackend, MockIsolationConnection,
        MockUnappliedIsolationBackend, with_handle,
    };
    use odbc_sys::HandleType;

    /// Allocate an environment and connection handle **for the backend the
    /// test then calls through**.
    ///
    /// The type parameter is load-bearing, not decoration. A handle is a
    /// `ConnectionHandle<B>`, whose `connection: Option<B::Connection>` field
    /// has a different layout for every backend, while validation is a
    /// registry slot, generation and kind compare that never dereferences the
    /// caller's value and so has no way to know which backend allocated the
    /// handle. Allocating as one backend and calling as another therefore
    /// passes that compare and then reads memory laid out for a different
    /// type — undefined behaviour that Miri catches and a plain `cargo test`
    /// does not.
    unsafe fn alloc_env_conn_for<B: Backend>() -> (*mut c_void, *mut c_void) {
        let mut env: *mut c_void = std::ptr::null_mut();
        let _ = unsafe {
            sql_alloc_handle::<B>(HandleType::Env as i16, std::ptr::null_mut(), &mut env)
        };
        let mut conn: *mut c_void = std::ptr::null_mut();
        let _ = unsafe { sql_alloc_handle::<B>(HandleType::Dbc as i16, env, &mut conn) };
        (env, conn)
    }

    unsafe fn alloc_env_conn() -> (*mut c_void, *mut c_void) {
        unsafe { alloc_env_conn_for::<MockBackend>() }
    }

    unsafe fn cleanup_for<B: Backend>(env: *mut c_void, conn: *mut c_void) {
        unsafe {
            // A connected handle cannot be freed; disconnect first so the
            // connection is not leaked. Harmless when never connected.
            let _ = crate::ffi::connect::sql_disconnect::<B>(conn);
            let _ = sql_free_handle::<B>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<B>(HandleType::Env as i16, env);
        }
    }

    unsafe fn cleanup(env: *mut c_void, conn: *mut c_void) {
        unsafe { cleanup_for::<MockBackend>(env, conn) }
    }

    #[test]
    fn set_and_get_autocommit() {
        unsafe {
            let (env, conn) = alloc_env_conn();

            // Set autocommit OFF (0)
            let ret = sql_set_connect_attr_w::<MockBackend>(
                conn,
                ConnectionAttribute::AUTOCOMMIT.0,
                std::ptr::null_mut::<c_void>(),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            // Get autocommit — should be 0
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

            cleanup(env, conn);
        }
    }

    #[test]
    fn get_autocommit_default_is_on() {
        unsafe {
            let (env, conn) = alloc_env_conn();
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
            cleanup(env, conn);
        }
    }

    #[test]
    fn get_connection_dead_always_false() {
        unsafe {
            let (env, conn) = alloc_env_conn();
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
            cleanup(env, conn);
        }
    }

    #[test]
    fn set_unknown_attr_returns_success() {
        unsafe {
            let (env, conn) = alloc_env_conn();
            // Driver-specific attribute — must not fail
            let ret = sql_set_connect_attr_w::<MockBackend>(conn, 9999, std::ptr::null_mut(), 0);
            assert_eq!(ret, SqlReturn::SUCCESS);
            cleanup(env, conn);
        }
    }

    #[test]
    fn set_connection_dead_is_error() {
        unsafe {
            let (env, conn) = alloc_env_conn();
            let ret = sql_set_connect_attr_w::<MockBackend>(
                conn,
                ConnectionAttribute::CONNECTION_DEAD.0,
                std::ptr::null_mut::<c_void>(),
                0,
            );
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn);
        }
    }

    /// Spec, `SQL_ATTR_PACKET_SIZE`: "If the application sets packet size after
    /// a connection has already been made, the driver will return SQLSTATE
    /// HY011 (Attribute cannot be set now)." The attribute table lists it as
    /// settable "Before" only.
    #[test]
    fn packet_size_after_connect_reports_hy011() {
        unsafe {
            let (env, conn) = alloc_env_conn();

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

            cleanup(env, conn);
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
                let (env, conn) = alloc_env_conn();
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
                cleanup(env, conn);
            }
        }
    }

    /// `SQL_ATTR_CURRENT_CATALOG` and `SQL_DATABASE_NAME` are one value under
    /// two names, so they must read the same two sources in the same order:
    /// what the application set, else what the session is actually using. With
    /// only the first, the attribute is write-only — it answers `""` while the
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

            cleanup_for::<MockAltBackend>(env, conn);
        }
    }

    /// `SQL_ATTR_ASYNC_ENABLE` and `SQL_ATTR_ODBC_CURSORS` are the two
    /// connection attributes the spec declares `SQLULEN` — "A SQLULEN value
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
                let (env, conn) = alloc_env_conn();
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
                cleanup(env, conn);
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
                let (env, conn) = alloc_env_conn();
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
                cleanup(env, conn);
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
            let (env, conn) = alloc_env_conn();
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

            cleanup(env, conn);
        }
    }

    #[test]
    fn set_autocommit_on_is_accepted() {
        unsafe {
            let (env, conn) = alloc_env_conn();
            connect(conn);
            let ret = sql_set_connect_attr_w::<MockBackend>(
                conn,
                ConnectionAttribute::AUTOCOMMIT.0,
                std::ptr::without_provenance_mut(SQL_AUTOCOMMIT_ON),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            cleanup(env, conn);
        }
    }

    #[test]
    fn autocommit_off_set_before_connect_is_deferred_then_applied() {
        // The spec lists SQL_ATTR_AUTOCOMMIT as settable before connecting, so
        // the pre-connect call must succeed, but the value still has to reach
        // the backend, so the connect itself fails for a backend that cannot
        // honour manual-commit.
        unsafe {
            let (env, conn) = alloc_env_conn();
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

            cleanup(env, conn);
        }
    }

    #[test]
    fn set_autocommit_invalid_value_returns_hy024() {
        unsafe {
            let (env, conn) = alloc_env_conn();
            let ret = sql_set_connect_attr_w::<MockBackend>(
                conn,
                ConnectionAttribute::AUTOCOMMIT.0,
                99usize as *mut c_void,
                0,
            );
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn);
        }
    }

    #[test]
    fn set_access_mode_invalid_value_returns_hy024() {
        unsafe {
            let (env, conn) = alloc_env_conn();
            let ret = sql_set_connect_attr_w::<MockBackend>(
                conn,
                ConnectionAttribute::ACCESS_MODE.0,
                5usize as *mut c_void,
                0,
            );
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn);
        }
    }

    #[test]
    fn set_trace_invalid_value_returns_hy024() {
        unsafe {
            let (env, conn) = alloc_env_conn();
            let ret = sql_set_connect_attr_w::<MockBackend>(
                conn,
                ConnectionAttribute::TRACE.0,
                2usize as *mut c_void,
                0,
            );
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn);
        }
    }

    #[test]
    fn set_odbc_cursors_invalid_value_returns_hy024() {
        unsafe {
            let (env, conn) = alloc_env_conn();
            let ret = sql_set_connect_attr_w::<MockBackend>(
                conn,
                ConnectionAttribute::ODBC_CURSORS.0,
                99usize as *mut c_void,
                0,
            );
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn);
        }
    }

    #[test]
    fn set_discrete_attrs_valid_values_succeed() {
        unsafe {
            let (env, conn) = alloc_env_conn();
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
            cleanup(env, conn);
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
            let (env, conn) = alloc_env_conn();
            let mut val: u32 = 0;
            let ret = sql_get_connect_attr_w::<MockBackend>(
                conn,
                99999, // unknown attribute
                &mut val as *mut u32 as *mut c_void,
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn);
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

    /// C2: an unset `SQL_ATTR_TXN_ISOLATION` must report the level the backend
    /// declares, not a constant. Answering one unconditionally — say
    /// `SQL_TXN_READ_COMMITTED` — makes a backend that reports
    /// `SQL_TXN_SERIALIZABLE` for `SQL_DEFAULT_TXN_ISOLATION` contradict itself
    /// on the same connection.
    ///
    /// Connected first, because the declared default is the data source's and
    /// [`Backend::default_txn_isolation`] takes the connection to read it from.
    #[test]
    fn get_txn_isolation_default_comes_from_the_backend() {
        unsafe {
            let (env, conn) = alloc_env_conn();
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
            cleanup(env, conn);
        }
    }

    /// C3: the spec's HY024 row makes validating this the driver's job --
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
            let (env, conn) = alloc_env_conn();
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
            cleanup(env, conn);
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
            cleanup_for::<MockIsolationBackend>(env, conn);
        }
    }

    /// A supported level is accepted and reads back unchanged.
    #[test]
    fn set_txn_isolation_accepts_a_supported_level() {
        unsafe {
            let (env, conn) = alloc_env_conn();
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
            cleanup(env, conn);
        }
    }

    /// C3's worse half: the stored value has to *reach the data source*. An
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

            cleanup_for::<MockIsolationBackend>(env, conn);
        }
    }

    /// The spec lists SQL_ATTR_TXN_ISOLATION as settable "Either" side of the
    /// connection, so a level set before connecting must still be applied once
    /// the connection exists — the same contract as SQL_ATTR_AUTOCOMMIT.
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

            cleanup_for::<MockIsolationBackend>(env, conn);
        }
    }

    /// A backend declaring more than one level but not implementing
    /// `set_txn_isolation` must fail loudly rather than accept a level it
    /// cannot apply — the exact silent lie C3 is about.
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

            cleanup_for::<MockUnappliedIsolationBackend>(env, conn);
        }
    }
}
