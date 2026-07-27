//! Generic implementations of SQLSetStmtAttrW and SQLGetStmtAttrW.

use std::ffi::c_void;

use odbc_sys::StatementAttribute;

use crate::backend::Backend;
use crate::errors::OdbcError;
use crate::handles::{StatementHandle, as_handle_ref};
use crate::panic::panic_safe;
use crate::types::{
    SQL_CURSOR_FORWARD_ONLY, SQL_FALSE, SqlReturn, SqlState, statement_attribute_from_raw,
};

// SQL_ATTR_CURSOR_SCROLLABLE values
const SQL_NONSCROLLABLE: usize = 0;

// SQL_ATTR_CONCURRENCY values
const SQL_CONCUR_READ_ONLY: usize = 1;
// SQL_ATTR_RETRIEVE_DATA values
const SQL_RD_ON: usize = 1;
// SQL_ATTR_USE_BOOKMARKS values
const SQL_UB_OFF: usize = 0;
// SQL_ATTR_NOSCAN values
const SQL_NOSCAN_OFF: usize = 0;
// SQL_ATTR_ROW_BIND_TYPE values
const SQL_BIND_BY_COLUMN: usize = 0;
// SQL_ATTR_CURSOR_SENSITIVITY values
const SQL_INSENSITIVE: usize = 2;
// SQL_ATTR_ROW_ARRAY_SIZE default
const SQL_ROW_ARRAY_SIZE_DEFAULT: usize = 1;
// SQL_ATTR_PARAMSET_SIZE default
const SQL_PARAMSET_SIZE_DEFAULT: usize = 1;

/// Generic implementation of SQLSetStmtAttrW.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlsetstmtattr-function>
///
/// Accepts and stores all recognised integer/pointer attributes.
/// Returns `HYC00` (optional feature not implemented) when an application
/// requests a scrollable cursor or a non-read-only concurrency mode, since
/// only forward-only, read-only cursors are supported.
///
/// # Parameters
///
/// - `statement_handle`: statement handle (SQL_HANDLE_STMT).
/// - `attribute`: the statement attribute to set (e.g. `SQL_ATTR_CURSOR_TYPE`).
/// - `value_ptr`: the value to associate with `attribute`. Either an integer
///   cast to a pointer, or a pointer to a null-terminated UTF-16 string, or a
///   descriptor handle — depending on the attribute.
/// - `_string_length`: byte length of `*value_ptr` when it is a string;
///   ignored for integer-valued attributes.
///
/// # Spec compliance
///
/// - 01000 General warning: not currently returned here.
/// - 01S02 Option value changed: not currently returned; the driver stores
///   whatever integer value the application provides without substitution.
///   Returning 01S02 would require defining what values are "similar" for each
///   attribute. Deferred.
/// - 24000 Invalid cursor state: returned when setting `SQL_ATTR_CONCURRENCY`,
///   `SQL_ATTR_CURSOR_TYPE`, `SQL_ATTR_SIMULATE_CURSOR`, or
///   `SQL_ATTR_USE_BOOKMARKS` while a cursor is open (`stmt.cursor_open`). A
///   statement that is only prepared, or whose cursor `SQLEndTran` closed under
///   `SQL_CB_CLOSE`, has no open cursor and is not rejected here (a prepared one
///   is rejected by the HY011 check below instead).
/// - HY000 General error: returned for unexpected internal errors.
/// - HY001 Memory allocation error: not returned; Rust panics on allocation
///   failure, which is caught by `panic_safe` and converted to `SQL_ERROR`/HY000.
/// - HY009 Invalid use of null pointer: not currently checked; the spec
///   requires HY009 when an attribute that requires a string value receives a
///   null `value_ptr`. No string-valued set-attributes exist, so the
///   condition does not arise in practice.
/// - HY010 Function sequence error: (driver-manager-handled; not returned here).
/// - HY011 Attribute cannot be set now: returned when setting the above
///   attributes after the statement has been prepared (`stmt.prepared_sql` is
///   `Some`).
/// - HY013 Memory management error: not returned; Rust panics on memory errors,
///   caught by `panic_safe` and converted to `SQL_ERROR`/HY000.
/// - HY017 Invalid use of an automatically allocated descriptor handle:
///   (driver-manager-handled; not returned here). Descriptor
///   attributes are accepted silently because descriptors are not yet
///   fully implemented.
/// - HY024 Invalid attribute value: not returned. Unsupported cursor-type values
///   are rejected with HYC00 (Optional feature not implemented) via
///   `OdbcError::NotImplemented`. Per-attribute range validation for other
///   discrete-valued attributes is not implemented. Deferred.
/// - HY090 Invalid string or buffer length: (driver-manager-handled; not
///   returned here).
/// - HY092 Invalid attribute/option identifier: (driver-manager-handled; not
///   returned here). Unknown attributes are accepted silently.
/// - HY117 Connection is suspended due to unknown transaction state:
///   (driver-manager-handled; not returned here).
/// - HYC00 Optional feature not implemented: returned when
///   `SQL_ATTR_CURSOR_TYPE` is not `SQL_CURSOR_FORWARD_ONLY` or
///   `SQL_ATTR_CURSOR_SCROLLABLE` is not `SQL_NONSCROLLABLE`.
/// - HYT01 Connection timeout expired: not returned; this function does not
///   communicate with the data source.
/// - IM001 Driver does not support this function: (driver-manager-handled; not
///   returned here).
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
pub unsafe fn sql_set_stmt_attr_w<B: Backend>(
    statement_handle: *mut c_void,
    attribute: i32,
    value_ptr: *mut c_void,
    _string_length: i32,
) -> SqlReturn {
    tracing::trace!(
        "SQLSetStmtAttrW(stmt={:?}, attr_raw={}, value={:?})",
        statement_handle,
        attribute,
        value_ptr
    );
    let attr = statement_attribute_from_raw(attribute);
    tracing::debug!("SQLSetStmtAttrW: attr={:?}", attr);
    // SAFETY: statement_handle is null or a valid StatementHandle<B> allocated by
    // sql_alloc_handle; tag is validated by as_handle_ref inside the closure.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, || {
            let stmt = as_handle_ref::<StatementHandle<B>>(statement_handle)?;
            stmt.diagnostics.clear();

            let int_val = value_ptr as usize;

            match attr {
                // Spec 24000 + HY011: these attributes cannot be set while a cursor is
                // open or after the statement has been prepared.
                Some(
                    StatementAttribute::CursorType
                    | StatementAttribute::Concurrency
                    | StatementAttribute::SimulateCursor
                    | StatementAttribute::UseBookmarks,
                ) => {
                    // Spec 24000: cursor is open.
                    if stmt.cursor_open {
                        return Err(OdbcError::general(
                            format!("Cannot set {attr:?} while a cursor is open"),
                            SqlState::invalid_cursor_state(),
                        ));
                    }
                    // Spec HY011: statement has been prepared.
                    if stmt.prepared_sql.is_some() {
                        return Err(OdbcError::general(
                            format!("Cannot set {attr:?} after the statement has been prepared"),
                            SqlState::attribute_cannot_be_set_now(),
                        ));
                    }
                    // Cursor-type-specific validation: only forward-only is
                    // supported. If a driver ever needs scrollable cursors,
                    // this check would have to move from stackable-odbc-core into the
                    // backend trait so each driver could decide independently.
                    if matches!(attr, Some(StatementAttribute::CursorType))
                        && int_val != SQL_CURSOR_FORWARD_ONLY
                    {
                        return Err(OdbcError::NotImplemented {
                            feature: format!(
                                "SQL_ATTR_CURSOR_TYPE value {int_val} (only SQL_CURSOR_FORWARD_ONLY=0 is supported)"
                            ),
                        });
                    }
                    stmt.attrs.insert(attribute, int_val);
                    Ok(SqlReturn::SUCCESS)
                }

                // SQL_ATTR_CURSOR_SCROLLABLE: only non-scrollable is supported.
                Some(StatementAttribute::CursorScrollable) => {
                    if int_val != SQL_NONSCROLLABLE {
                        return Err(OdbcError::NotImplemented {
                            feature: "SQL_ATTR_CURSOR_SCROLLABLE = SQL_SCROLLABLE".into(),
                        });
                    }
                    stmt.attrs.insert(attribute, int_val);
                    Ok(SqlReturn::SUCCESS)
                }

                // Descriptor handle attrs (AppRowDesc, AppParamDesc, ImpRowDesc,
                // ImpParamDesc): accept silently; we don't implement descriptors yet.
                Some(
                    StatementAttribute::AppRowDesc
                    | StatementAttribute::AppParamDesc
                    | StatementAttribute::ImpRowDesc
                    | StatementAttribute::ImpParamDesc,
                ) => {
                    tracing::warn!(
                        "SQLSetStmtAttrW: descriptor attribute {} ({:?}) ignored \
                         (descriptors not yet implemented)",
                        attribute,
                        attr
                    );
                    Ok(SqlReturn::SUCCESS)
                }

                // Only single-row rowsets are implemented: SQLFetch reads one
                // row and does not write through SQL_ATTR_ROWS_FETCHED_PTR or
                // the row status array. Accepting a larger size verbatim would
                // leave the application reading uninitialised buffer elements
                // as though they were rows.
                //
                // Spec 01S02: the driver substitutes a similar value and
                // returns SQL_SUCCESS_WITH_INFO; SQL_ATTR_ROW_ARRAY_SIZE is
                // explicitly listed as substitutable. SQLGetStmtAttr then
                // reports the substituted value.
                Some(StatementAttribute::RowArraySize) if int_val != SQL_ROW_ARRAY_SIZE_DEFAULT => {
                    tracing::warn!(
                        "SQLSetStmtAttrW: SQL_ATTR_ROW_ARRAY_SIZE={} not supported, substituting {} (01S02)",
                        int_val,
                        SQL_ROW_ARRAY_SIZE_DEFAULT
                    );
                    stmt.attrs.insert(attribute, SQL_ROW_ARRAY_SIZE_DEFAULT);
                    stmt.diagnostics.push(&OdbcError::General {
                        message: format!(
                            "SQL_ATTR_ROW_ARRAY_SIZE {int_val} is not supported; substituted {SQL_ROW_ARRAY_SIZE_DEFAULT}"
                        ),
                        sqlstate: SqlState::option_value_changed(),
                    });
                    Ok(SqlReturn::SUCCESS_WITH_INFO)
                }

                // Only a single parameter set is executed: SQLExecute binds and
                // runs the parameter buffers once and does not iterate over an
                // array. Accepting a larger size verbatim would silently drop
                // every parameter set past the first and cause an undetectable
                // batch-insert data loss. As with SQL_ATTR_ROW_ARRAY_SIZE, the
                // spec (01S02) lets the driver substitute a similar value and
                // return SQL_SUCCESS_WITH_INFO; SQLGetStmtAttr then reports the
                // substituted value.
                Some(StatementAttribute::ParamsetSize) if int_val != SQL_PARAMSET_SIZE_DEFAULT => {
                    tracing::warn!(
                        "SQLSetStmtAttrW: SQL_ATTR_PARAMSET_SIZE={} not supported, substituting {} (01S02)",
                        int_val,
                        SQL_PARAMSET_SIZE_DEFAULT
                    );
                    stmt.attrs.insert(attribute, SQL_PARAMSET_SIZE_DEFAULT);
                    stmt.diagnostics.push(&OdbcError::General {
                        message: format!(
                            "SQL_ATTR_PARAMSET_SIZE {int_val} is not supported; substituted {SQL_PARAMSET_SIZE_DEFAULT}"
                        ),
                        sqlstate: SqlState::option_value_changed(),
                    });
                    Ok(SqlReturn::SUCCESS_WITH_INFO)
                }

                // All other recognised attributes: store value.
                Some(_) => {
                    stmt.attrs.insert(attribute, int_val);
                    Ok(SqlReturn::SUCCESS)
                }

                None => {
                    tracing::warn!(
                        "SQLSetStmtAttrW: unrecognized attribute {} accepted silently \
                         (spec requires HYC00; relaxed for DM/tool compatibility)",
                        attribute
                    );
                    Ok(SqlReturn::SUCCESS)
                }
            }
        })
    };
    tracing::debug!("SQLSetStmtAttrW -> {:?}", ret);
    ret
}

/// Generic implementation of SQLGetStmtAttrW.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlgetstmtattr-function>
///
/// Returns integer attributes as `u32` written to `*value_ptr`.
/// Pointer attributes are returned as pointer-sized values.
///
/// # Parameters
///
/// - `statement_handle`: statement handle (SQL_HANDLE_STMT).
/// - `attribute`: the statement attribute to retrieve (e.g. `SQL_ATTR_CURSOR_TYPE`).
/// - `value_ptr`: output buffer to receive the attribute value. For integer
///   attributes this receives a `u32`; for pointer attributes a pointer-sized
///   value; for descriptor-handle attributes a pointer to the descriptor handle.
///   May be null — the driver still writes `*string_length_ptr` in that case.
/// - `_buffer_length`: maximum byte length of `*value_ptr` when it is a string;
///   ignored for integer and pointer attributes.
/// - `string_length_ptr`: output pointer that receives the number of bytes
///   written to `*value_ptr`. May be null.
///
/// # Spec compliance
///
/// - 01000 General warning: not currently returned here.
/// - 01004 String data, right truncated: not applicable; no string-valued
///   statement attributes are returned (all returned values are integer or
///   pointer types).
/// - 24000 Invalid cursor state: returned when `SQL_ATTR_ROW_NUMBER` is
///   requested and no cursor is open (`stmt.cursor_open` is `false`), which
///   includes a statement that is only prepared and one whose cursor
///   `SQLEndTran` closed under `SQL_CB_CLOSE`.
/// - HY000 General error: returned for unexpected internal errors.
/// - HY001 Memory allocation error: not returned; Rust panics on allocation
///   failure, which is caught by `panic_safe` and converted to `SQL_ERROR`/HY000.
/// - HY010 Function sequence error: (driver-manager-handled; not returned here).
/// - HY013 Memory management error: not returned; Rust panics on memory errors,
///   caught by `panic_safe` and converted to `SQL_ERROR`/HY000.
/// - HY090 Invalid string or buffer length: (driver-manager-handled; not
///   returned here).
/// - HY092 Invalid attribute/option identifier: returning HYC00 rather than
///   HY092 for unrecognised identifiers is a deliberate DM-compatibility
///   choice. HYC00 is accepted by all common Driver Managers.
/// - HY109 Invalid cursor position: detecting deleted or unfetchable rows
///   requires backend support for tracking row validity. Not currently
///   implemented. Deferred.
/// - HY117 Connection is suspended due to unknown transaction state:
///   (driver-manager-handled; not returned here).
/// - HYC00 Optional feature not implemented: returned for unrecognised or
///   unsupported attribute identifiers.
/// - HYT01 Connection timeout expired: not returned; this function does not
///   communicate with the data source.
/// - IM001 Driver does not support this function: (driver-manager-handled; not
///   returned here).
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
/// `value_ptr` must be writable for the appropriate size.
pub unsafe fn sql_get_stmt_attr_w<B: Backend>(
    statement_handle: *mut c_void,
    attribute: i32,
    value_ptr: *mut c_void,
    _buffer_length: i32,
    string_length_ptr: *mut i32,
) -> SqlReturn {
    tracing::trace!(
        "SQLGetStmtAttrW(stmt={:?}, attr_raw={})",
        statement_handle,
        attribute
    );
    let attr = statement_attribute_from_raw(attribute);
    tracing::debug!("SQLGetStmtAttrW: attr={:?}", attr);
    // SAFETY: statement_handle is null or a valid StatementHandle<B> allocated by
    // sql_alloc_handle; tag is validated by as_handle_ref inside the closure.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, || {
            let stmt = as_handle_ref::<StatementHandle<B>>(statement_handle)?;
            stmt.diagnostics.clear();

            // Helper: write a u32 to value_ptr and report size.
            // SAFETY: value_ptr is non-null (checked); caller guarantees it points to
            // writable memory for at least a u32. string_length_ptr likewise. Alignment is
            // not guaranteed (row-wise binding may place the buffer at an arbitrary offset),
            // so use unaligned writes.
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

            // Helper: write a pointer-sized value to value_ptr.
            // SAFETY: value_ptr is non-null (checked); caller guarantees it points to
            // writable memory for at least a usize. string_length_ptr likewise. Alignment is
            // not guaranteed (row-wise binding may place the buffer at an arbitrary offset),
            // so use unaligned writes.
            let write_ptr = |v: usize| {
                if !value_ptr.is_null() {
                    // SAFETY: non-null checked above; caller guarantees writable usize
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
                Some(StatementAttribute::QueryTimeout) => {
                    write_u32(stmt.attrs.get(&attribute).copied().unwrap_or(0) as u32);
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::MaxRows) => {
                    write_u32(stmt.attrs.get(&attribute).copied().unwrap_or(0) as u32);
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::MaxLength) => {
                    write_u32(stmt.attrs.get(&attribute).copied().unwrap_or(0) as u32);
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::NoScan) => {
                    write_u32(
                        stmt.attrs
                            .get(&attribute)
                            .copied()
                            .unwrap_or(SQL_NOSCAN_OFF) as u32,
                    );
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::RowBindType) => {
                    write_u32(
                        stmt.attrs
                            .get(&attribute)
                            .copied()
                            .unwrap_or(SQL_BIND_BY_COLUMN) as u32,
                    );
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::CursorType) => {
                    write_u32(
                        stmt.attrs
                            .get(&attribute)
                            .copied()
                            .unwrap_or(SQL_CURSOR_FORWARD_ONLY) as u32,
                    );
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::Concurrency) => {
                    write_u32(
                        stmt.attrs
                            .get(&attribute)
                            .copied()
                            .unwrap_or(SQL_CONCUR_READ_ONLY) as u32,
                    );
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::RetrieveData) => {
                    write_u32(stmt.attrs.get(&attribute).copied().unwrap_or(SQL_RD_ON) as u32);
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::UseBookmarks) => {
                    write_u32(stmt.attrs.get(&attribute).copied().unwrap_or(SQL_UB_OFF) as u32);
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::RowArraySize) => {
                    write_u32(
                        stmt.attrs
                            .get(&attribute)
                            .copied()
                            .unwrap_or(SQL_ROW_ARRAY_SIZE_DEFAULT) as u32,
                    );
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::ParamsetSize) => {
                    write_u32(
                        stmt.attrs
                            .get(&attribute)
                            .copied()
                            .unwrap_or(SQL_PARAMSET_SIZE_DEFAULT) as u32,
                    );
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::RowNumber) => {
                    // Spec 24000: no cursor is open.
                    if !stmt.cursor_open {
                        return Err(OdbcError::general(
                            "SQL_ATTR_ROW_NUMBER requires an open cursor",
                            SqlState::invalid_cursor_state(),
                        ));
                    }
                    let v = stmt.attrs.get(&attribute).copied().unwrap_or(0);
                    write_u32(v as u32);
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::EnableAutoIpd) => {
                    write_u32(SQL_FALSE); // not supported
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::AsyncEnable) => {
                    write_u32(stmt.attrs.get(&attribute).copied().unwrap_or(0) as u32);
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::MetadataId) => {
                    write_u32(
                        stmt.attrs
                            .get(&attribute)
                            .copied()
                            .unwrap_or(SQL_FALSE as usize) as u32,
                    );
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::CursorScrollable) => {
                    write_u32(
                        stmt.attrs
                            .get(&attribute)
                            .copied()
                            .unwrap_or(SQL_NONSCROLLABLE) as u32,
                    );
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::CursorSensitivity) => {
                    write_u32(
                        stmt.attrs
                            .get(&attribute)
                            .copied()
                            .unwrap_or(SQL_INSENSITIVE) as u32,
                    );
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::SimulateCursor) => {
                    write_u32(stmt.attrs.get(&attribute).copied().unwrap_or(0) as u32);
                    Ok(SqlReturn::SUCCESS)
                }

                // Pointer-valued attributes.
                Some(StatementAttribute::RowStatusPtr) => {
                    write_ptr(stmt.attrs.get(&attribute).copied().unwrap_or(0));
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::RowsFetchedPtr) => {
                    write_ptr(stmt.attrs.get(&attribute).copied().unwrap_or(0));
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::RowBindOffsetPtr) => {
                    write_ptr(stmt.attrs.get(&attribute).copied().unwrap_or(0));
                    Ok(SqlReturn::SUCCESS)
                }

                // Descriptor handle attrs: return the allocated descriptor handles.
                // The Windows DM requires these to build its CLI dispatch table.
                Some(StatementAttribute::AppRowDesc) => {
                    write_ptr(stmt.app_row_desc.token() as usize);
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::AppParamDesc) => {
                    write_ptr(stmt.app_param_desc.token() as usize);
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::ImpRowDesc) => {
                    write_ptr(stmt.imp_row_desc.token() as usize);
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::ImpParamDesc) => {
                    write_ptr(stmt.imp_param_desc.token() as usize);
                    Ok(SqlReturn::SUCCESS)
                }

                _ => Err(OdbcError::general(
                    format!("SQLGetStmtAttrW: unsupported attribute {attribute}"),
                    SqlState::optional_feature_not_implemented(),
                )),
            }
        })
    };
    tracing::debug!("SQLGetStmtAttrW -> {:?}", ret);
    ret
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffi::handle::{sql_alloc_handle, sql_free_handle};
    use crate::test_utils::MockBackend;
    use odbc_sys::HandleType;

    unsafe fn alloc_env_conn_stmt() -> (*mut c_void, *mut c_void, *mut c_void) {
        let mut env: *mut c_void = std::ptr::null_mut();
        let _ = unsafe {
            sql_alloc_handle::<MockBackend>(HandleType::Env as i16, std::ptr::null_mut(), &mut env)
        };
        let mut conn: *mut c_void = std::ptr::null_mut();
        let _ = unsafe { sql_alloc_handle::<MockBackend>(HandleType::Dbc as i16, env, &mut conn) };
        let mut stmt: *mut c_void = std::ptr::null_mut();
        let _ =
            unsafe { sql_alloc_handle::<MockBackend>(HandleType::Stmt as i16, conn, &mut stmt) };
        (env, conn, stmt)
    }

    unsafe fn cleanup(env: *mut c_void, conn: *mut c_void, stmt: *mut c_void) {
        unsafe {
            let _ = sql_free_handle::<MockBackend>(HandleType::Stmt as i16, stmt);
            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn row_array_size_greater_than_one_is_substituted_with_01s02() {
        // Only single-row fetch is implemented. Accepting a larger rowset with
        // plain SQL_SUCCESS would let the application read uninitialised
        // buffer elements as rows.
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();

            let ret = sql_set_stmt_attr_w::<MockBackend>(
                stmt,
                StatementAttribute::RowArraySize as i32,
                std::ptr::without_provenance_mut(10usize),
                0,
            );
            assert_eq!(
                ret,
                SqlReturn::SUCCESS_WITH_INFO,
                "oversized rowset was accepted silently"
            );

            // Checked before any other call: every FFI function clears the
            // handle's diagnostics on entry.
            {
                let handle = as_handle_ref::<StatementHandle<MockBackend>>(stmt).unwrap();
                assert_eq!(
                    handle.diagnostics.len(),
                    1,
                    "no 01S02 diagnostic was recorded"
                );
            }

            // SQLGetStmtAttr must report the substituted value, per 01S02.
            let mut out: u32 = 0;
            assert_eq!(
                sql_get_stmt_attr_w::<MockBackend>(
                    stmt,
                    StatementAttribute::RowArraySize as i32,
                    std::ptr::from_mut(&mut out).cast(),
                    0,
                    std::ptr::null_mut(),
                ),
                SqlReturn::SUCCESS
            );
            assert_eq!(out, 1, "substituted value not reported back");

            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn row_array_size_of_one_is_accepted_without_warning() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = sql_set_stmt_attr_w::<MockBackend>(
                stmt,
                StatementAttribute::RowArraySize as i32,
                std::ptr::without_provenance_mut(1usize),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            let handle = as_handle_ref::<StatementHandle<MockBackend>>(stmt).unwrap();
            assert_eq!(handle.diagnostics.len(), 0);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn paramset_size_greater_than_one_is_substituted_with_01s02() {
        // Only a single parameter set is executed. Accepting a larger
        // SQL_ATTR_PARAMSET_SIZE with plain SQL_SUCCESS silently drops every
        // parameter set past the first, an undetectable batch-insert data
        // loss. Mirror SQL_ATTR_ROW_ARRAY_SIZE: substitute 1 with 01S02.
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();

            let ret = sql_set_stmt_attr_w::<MockBackend>(
                stmt,
                StatementAttribute::ParamsetSize as i32,
                std::ptr::without_provenance_mut(10usize),
                0,
            );
            assert_eq!(
                ret,
                SqlReturn::SUCCESS_WITH_INFO,
                "oversized parameter set was accepted silently"
            );

            {
                let handle = as_handle_ref::<StatementHandle<MockBackend>>(stmt).unwrap();
                assert_eq!(
                    handle.diagnostics.len(),
                    1,
                    "no 01S02 diagnostic was recorded"
                );
            }

            // SQLGetStmtAttr must report the substituted value, per 01S02.
            let mut out: u32 = 0;
            assert_eq!(
                sql_get_stmt_attr_w::<MockBackend>(
                    stmt,
                    StatementAttribute::ParamsetSize as i32,
                    std::ptr::from_mut(&mut out).cast(),
                    0,
                    std::ptr::null_mut(),
                ),
                SqlReturn::SUCCESS
            );
            assert_eq!(out, 1, "substituted value not reported back");

            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn paramset_size_of_one_is_accepted_without_warning() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = sql_set_stmt_attr_w::<MockBackend>(
                stmt,
                StatementAttribute::ParamsetSize as i32,
                std::ptr::without_provenance_mut(1usize),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            let handle = as_handle_ref::<StatementHandle<MockBackend>>(stmt).unwrap();
            assert_eq!(handle.diagnostics.len(), 0);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn descriptor_attrs_return_four_distinct_handles() {
        // The Windows DM reads these four attributes to build its CLI dispatch
        // table, so each must be a distinct, non-null address.
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();

            let mut addrs = Vec::new();
            for attr in [
                StatementAttribute::AppRowDesc,
                StatementAttribute::AppParamDesc,
                StatementAttribute::ImpRowDesc,
                StatementAttribute::ImpParamDesc,
            ] {
                let mut out: usize = 0;
                let ret = sql_get_stmt_attr_w::<MockBackend>(
                    stmt,
                    attr as i32,
                    std::ptr::from_mut(&mut out).cast(),
                    0,
                    std::ptr::null_mut(),
                );
                assert_eq!(ret, SqlReturn::SUCCESS, "{attr:?}");
                assert_ne!(out, 0, "{attr:?} returned a null descriptor handle");
                addrs.push(out);
            }

            addrs.sort_unstable();
            let distinct = addrs.len();
            addrs.dedup();
            assert_eq!(
                addrs.len(),
                distinct,
                "descriptor handles were not distinct"
            );

            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn set_and_get_cursor_type_forward_only() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = sql_set_stmt_attr_w::<MockBackend>(
                stmt,
                StatementAttribute::CursorType as i32,
                SQL_CURSOR_FORWARD_ONLY as *mut c_void,
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let mut val: u32 = 99;
            let ret = sql_get_stmt_attr_w::<MockBackend>(
                stmt,
                StatementAttribute::CursorType as i32,
                &mut val as *mut u32 as *mut c_void,
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(val, SQL_CURSOR_FORWARD_ONLY as u32);

            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn set_cursor_type_static_returns_error() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = sql_set_stmt_attr_w::<MockBackend>(
                stmt,
                StatementAttribute::CursorType as i32,
                3usize as *mut c_void, // SQL_CURSOR_STATIC
                0,
            );
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn get_cursor_type_default_is_forward_only() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let mut val: u32 = 99;
            let ret = sql_get_stmt_attr_w::<MockBackend>(
                stmt,
                StatementAttribute::CursorType as i32,
                &mut val as *mut u32 as *mut c_void,
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(val, 0);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn set_scrollable_cursor_returns_error() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = sql_set_stmt_attr_w::<MockBackend>(
                stmt,
                StatementAttribute::CursorScrollable as i32,
                std::ptr::dangling_mut::<c_void>(), // SQL_SCROLLABLE (non-null)
                0,
            );
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn set_and_get_query_timeout() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = sql_set_stmt_attr_w::<MockBackend>(
                stmt,
                StatementAttribute::QueryTimeout as i32,
                30usize as *mut c_void,
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let mut val: u32 = 0;
            let ret = sql_get_stmt_attr_w::<MockBackend>(
                stmt,
                StatementAttribute::QueryTimeout as i32,
                &mut val as *mut u32 as *mut c_void,
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(val, 30);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn set_unknown_attr_returns_success() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = sql_set_stmt_attr_w::<MockBackend>(stmt, 9999, std::ptr::null_mut(), 0);
            assert_eq!(ret, SqlReturn::SUCCESS);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn null_handle_returns_invalid() {
        unsafe {
            let ret = sql_set_stmt_attr_w::<MockBackend>(
                std::ptr::null_mut(),
                StatementAttribute::CursorType as i32,
                std::ptr::null_mut(),
                0,
            );
            assert_eq!(ret, SqlReturn::INVALID_HANDLE);
        }
    }

    #[test]
    fn set_cursor_type_with_cursor_open_returns_24000() {
        // 24000: cannot set SQL_ATTR_CURSOR_TYPE when a cursor is open.
        unsafe {
            use crate::handles::StatementData;

            let (env, conn, stmt) = alloc_env_conn_stmt();
            // Inject a synthetic cursor to simulate "cursor open".
            let handle = as_handle_ref::<StatementHandle<MockBackend>>(stmt).unwrap();
            handle.set_result_set(StatementData::Synthetic(
                crate::test_utils::synthetic_result_set(vec![]),
            ));
            let ret = sql_set_stmt_attr_w::<MockBackend>(
                stmt,
                StatementAttribute::CursorType as i32,
                std::ptr::null_mut::<c_void>(), // SQL_CURSOR_FORWARD_ONLY
                0,
            );
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn set_cursor_type_after_prepare_returns_hy011() {
        // HY011: cannot set SQL_ATTR_CURSOR_TYPE after SQLPrepare.
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let handle = as_handle_ref::<StatementHandle<MockBackend>>(stmt).unwrap();
            handle.prepared_sql = Some("SELECT 1".into());
            let ret = sql_set_stmt_attr_w::<MockBackend>(
                stmt,
                StatementAttribute::CursorType as i32,
                std::ptr::null_mut::<c_void>(),
                0,
            );
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn get_stmt_attr_null_handle_returns_invalid() {
        unsafe {
            let ret = sql_get_stmt_attr_w::<MockBackend>(
                std::ptr::null_mut(),
                StatementAttribute::CursorType as i32,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::INVALID_HANDLE);
        }
    }

    #[test]
    fn get_stmt_attr_unsupported_returns_error() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let mut val: u32 = 0;
            // Use a known-unsupported attribute to cover the error branch
            let ret = sql_get_stmt_attr_w::<MockBackend>(
                stmt,
                9999,
                &mut val as *mut u32 as *mut c_void,
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn, stmt);
        }
    }
}
