//! `SQLGetDescFieldW`, `SQLSetDescFieldW` and `SQLSetDescRec` — the descriptor
//! entry points, which report that descriptors are not implemented.
//!
//! # Why these exist at all, and why they answer `HYC00`
//!
//! The three symbols stay exported. Removing them would put a NULL in the
//! Windows Driver Manager's dispatch table, which is a crash rather than an
//! error — the same reason `AGENTS.md`'s Windows checklist exists. What
//! changes is what they *say*: [`crate::function_id::CORE_UNEXPORTED_FUNCTIONS`]
//! lists all five descriptor functions, so `SQLGetFunctions` reports them
//! unsupported and a Driver Manager acting on that answers `IM001` without ever
//! reaching this module.
//!
//! When one is called anyway, it returns `HYC00` ("optional feature not
//! implemented") **with a diagnostic record posted on the descriptor handle**.
//! The diagnostic is the point: these entry points previously returned a bare
//! `SQL_ERROR` and posted nothing, so an application learned that something had
//! failed and could not learn what.
//!
//! `HYC00` does not appear in any of the three functions' diagnostics tables,
//! and that is a deliberate choice rather than an oversight:
//!
//! - The tables list what a driver that *implements* descriptors returns. The
//!   spec's own wording is "the SQLSTATE values **commonly** returned", not an
//!   exhaustive set.
//! - `IM001` ("driver does not support this function") is the exact meaning,
//!   and every one of the three tables marks it **(DM)**. The project's
//!   non-negotiable rule forbids a driver-side return of a Driver-Manager code,
//!   and the DM produces it from `SQLGetFunctions` regardless.
//! - `HY000` is in all three tables and un-annotated, but it is the "no
//!   specific SQLSTATE" catch-all. An application distinguishing "unimplemented
//!   feature" from "something went wrong" gets nothing from it.
//! - `HYC00` is what `SQLAllocHandle` already returns for
//!   `SQL_HANDLE_DESC` (`crate::ffi::handle`), so an application that tried to
//!   allocate a descriptor and an application that tried to use one get the
//!   same answer.
//!
//! None of this makes `SQL_OIC_CORE` true. Core-level conformance requires
//! working descriptors; these functions only stop the driver claiming it has
//! them.

use std::ffi::c_void;

use crate::backend::Backend;
use crate::errors::OdbcError;
use crate::panic::panic_safe;
use crate::types::SqlReturn;

/// Post `HYC00` on `descriptor_handle` and return it, for a descriptor entry
/// point that is not implemented.
///
/// The three functions differ only in their arguments and their logging, all of
/// which happens before this is called, so the body they share is written once.
///
/// # Safety
///
/// `descriptor_handle` must be null or a token issued by one of the `alloc_*`
/// functions in `handles`.
unsafe fn not_implemented<B: Backend>(descriptor_handle: *mut c_void, feature: &str) -> SqlReturn {
    unsafe {
        panic_safe::<B, _>(descriptor_handle, |scope| {
            // Validates that the token really names a descriptor in this
            // scope's group, and clears the queue as the spec requires at the
            // start of every call. `panic_safe` posts the error below through
            // the same accessor.
            scope
                .descriptor_diagnostics::<B>(descriptor_handle)
                .ok_or(OdbcError::InvalidHandle)?
                .clear();
            Err(OdbcError::NotImplemented {
                feature: feature.into(),
            })
        })
    }
}

/// Generic implementation of SQLGetDescFieldW.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlgetdescfield-function>
///
/// Descriptors are not implemented, so this reports `HYC00` on the descriptor
/// handle. See the [module docs](self) for why that code and not one from the
/// table below.
///
/// # Parameters
///
/// - `descriptor_handle`: Descriptor handle.
/// - `record_number`: The descriptor record to read from.
/// - `field_identifier`: The `SQL_DESC_*` field to read.
/// - `value_ptr`: Buffer for the returned value. Never written.
/// - `buffer_length`: Length of `value_ptr` in bytes.
/// - `string_length_ptr`: Receives the available byte count. Never written.
///
/// # Spec compliance
///
/// Diagnostics from the ODBC spec Diagnostics table:
///
/// - `01000` General warning — (not returned here; nothing succeeds with info)
/// - `01004` String data, right truncated — (not returned here; no field is ever read, so
///   nothing can be truncated into `value_ptr`)
/// - `07009` Invalid descriptor index — **(DM)** on the clause that names an IRD bookmark
///   record; the remaining clauses are the driver's, but no record is reachable to index
///   into, so `record_number` is never validated
/// - `08S01` Communication link failure — (not returned here; this function performs no I/O)
/// - `HY000` General error — returned only for an internal panic caught by `panic_safe`
/// - `HY001` Memory allocation error — (not returned here; nothing is allocated)
/// - `HY007` Associated statement is not prepared — (not returned here; the IRD is not backed,
///   so there is no prepared-state check to make)
/// - `HY010` Function sequence error — **(DM)** on all three of its clauses (asynchronous
///   execution and `SQL_NEED_DATA`); not returned here
/// - `HY013` Memory management error — (not returned here)
/// - `HY021` Inconsistent descriptor information — (not returned here; no field is read)
/// - `HY090` Invalid string or buffer length — **(DM)**; not returned here
/// - `HY091` Invalid descriptor field identifier — (not returned here; `field_identifier` is
///   not validated, because the unimplemented-feature answer is the same for every field and
///   a driver-side field check would report the wrong problem)
/// - `HY117` Connection is suspended — **(DM)**; not returned here
/// - `HYT01` Connection timeout expired — (not returned here; this function performs no I/O)
/// - `IM001` Driver does not support this function — **(DM)**; the Driver Manager returns it
///   from the `SQLGetFunctions` answer, which reports this function unsupported
///
/// Not in the table, but what this function returns:
///
/// - `HYC00` Optional feature not implemented — descriptors are not implemented. See the
///   [module docs](self).
///
/// # Safety
///
/// `descriptor_handle` must be null or a token issued by one of the `alloc_*`
/// functions in `handles`. `value_ptr` and `string_length_ptr` are
/// never dereferenced.
pub unsafe fn sql_get_desc_field_w<B: Backend>(
    descriptor_handle: *mut c_void,
    record_number: i16,
    field_identifier: i16,
    value_ptr: *mut c_void,
    buffer_length: i32,
    string_length_ptr: *mut i32,
) -> SqlReturn {
    tracing::debug!(
        "SQLGetDescFieldW(desc={:?}, rec={}, field={}, value_ptr={:?}, buf_len={}, str_len_ptr={:?})",
        descriptor_handle,
        record_number,
        field_identifier,
        value_ptr,
        buffer_length,
        string_length_ptr,
    );
    let ret = unsafe {
        not_implemented::<B>(
            descriptor_handle,
            "SQLGetDescField: descriptors are not implemented",
        )
    };
    tracing::debug!("SQLGetDescFieldW -> {:?}", ret);
    ret
}

/// Generic implementation of SQLSetDescFieldW.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlsetdescfield-function>
///
/// Descriptors are not implemented, so this reports `HYC00` on the descriptor
/// handle. See the [module docs](self) for why that code and not one from the
/// table below.
///
/// # Parameters
///
/// - `descriptor_handle`: Descriptor handle.
/// - `record_number`: The descriptor record to write to.
/// - `field_identifier`: The `SQL_DESC_*` field to write.
/// - `value_ptr`: The value to write. Never dereferenced.
/// - `buffer_length`: Length of `value_ptr` in bytes, or an `SQL_IS_*` marker.
///
/// # Spec compliance
///
/// Diagnostics from the ODBC spec Diagnostics table:
///
/// - `01000` General warning — (not returned here; nothing succeeds with info)
/// - `01S02` Option value changed — (not returned here; no value is accepted, so none is
///   substituted)
/// - `07009` Invalid descriptor index — **(DM)** on the `SQL_DESC_COUNT` clause; the
///   remaining clauses are the driver's, but no record is reachable to index into, so
///   `record_number` is never validated
/// - `08S01` Communication link failure — (not returned here; this function performs no I/O)
/// - `22001` String data, right truncated — (not returned here; `SQL_DESC_NAME` is never
///   written)
/// - `HY000` General error — returned only for an internal panic caught by `panic_safe`
/// - `HY001` Memory allocation error — (not returned here; nothing is allocated)
/// - `HY010` Function sequence error — **(DM)** on all four of its clauses; not returned here
/// - `HY013` Memory management error — (not returned here)
/// - `HY016` Cannot modify an implementation row descriptor — (not returned here; the IRD is
///   deliberately unbacked, so every descriptor gets the same unimplemented answer and
///   singling the IRD out would assert a distinction core does not yet make)
/// - `HY021` Inconsistent descriptor information — (not returned here; no consistency check
///   runs, because no field is stored to be inconsistent with another)
/// - `HY090` Invalid string or buffer length — **(DM)** on both of its clauses; not returned
///   here
/// - `HY091` Invalid descriptor field identifier — (not returned here; `field_identifier` is
///   not validated, because the unimplemented-feature answer is the same for every field)
/// - `HY092` Invalid attribute/option identifier — (not returned here; no value is inspected)
/// - `HY105` Invalid parameter type — **(DM)**; not returned here
/// - `HY117` Connection is suspended — **(DM)**; not returned here
/// - `HYT01` Connection timeout expired — (not returned here; this function performs no I/O)
/// - `IM001` Driver does not support this function — **(DM)**; the Driver Manager returns it
///   from the `SQLGetFunctions` answer, which reports this function unsupported
///
/// Not in the table, but what this function returns:
///
/// - `HYC00` Optional feature not implemented — descriptors are not implemented. See the
///   [module docs](self).
///
/// # Safety
///
/// `descriptor_handle` must be null or a token issued by one of the `alloc_*`
/// functions in `handles`. `value_ptr` is never dereferenced.
pub unsafe fn sql_set_desc_field_w<B: Backend>(
    descriptor_handle: *mut c_void,
    record_number: i16,
    field_identifier: i16,
    value_ptr: *mut c_void,
    buffer_length: i32,
) -> SqlReturn {
    tracing::debug!(
        "SQLSetDescFieldW(desc={:?}, rec={}, field={}, value_ptr={:?}, buf_len={})",
        descriptor_handle,
        record_number,
        field_identifier,
        value_ptr,
        buffer_length,
    );
    let ret = unsafe {
        not_implemented::<B>(
            descriptor_handle,
            "SQLSetDescField: descriptors are not implemented",
        )
    };
    tracing::debug!("SQLSetDescFieldW -> {:?}", ret);
    ret
}

/// Generic implementation of SQLSetDescRec.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlsetdescrec-function>
///
/// Descriptors are not implemented, so this reports `HYC00` on the descriptor
/// handle. See the [module docs](self) for why that code and not one from the
/// table below.
///
/// No `W` suffix: every argument is numeric or a deferred data pointer, so
/// there is one spelling of this function rather than an ANSI and a Wide form.
///
/// # Parameters
///
/// - `descriptor_handle`: Descriptor handle.
/// - `record_number`: The descriptor record to write to.
/// - `value_type`: The value for `SQL_DESC_TYPE`.
/// - `sub_type`: The value for `SQL_DESC_DATETIME_INTERVAL_CODE`.
/// - `length`: The value for `SQL_DESC_OCTET_LENGTH`.
/// - `precision`: The value for `SQL_DESC_PRECISION`.
/// - `scale`: The value for `SQL_DESC_SCALE`.
/// - `data_ptr`: The value for `SQL_DESC_DATA_PTR`. Never dereferenced.
/// - `string_length_ptr`: The value for `SQL_DESC_OCTET_LENGTH_PTR`. Never dereferenced.
/// - `indicator_ptr`: The value for `SQL_DESC_INDICATOR_PTR`. Never dereferenced.
///
/// # Spec compliance
///
/// Diagnostics from the ODBC spec Diagnostics table:
///
/// - `01000` General warning — (not returned here; nothing succeeds with info)
/// - `07009` Invalid descriptor index — (not returned here; no clause of this row is
///   annotated `(DM)`, but no record is reachable to index into, so `record_number` is never
///   validated)
/// - `08S01` Communication link failure — (not returned here; this function performs no I/O)
/// - `HY000` General error — returned only for an internal panic caught by `panic_safe`
/// - `HY001` Memory allocation error — (not returned here; nothing is allocated)
/// - `HY010` Function sequence error — **(DM)** on all four of its clauses; not returned here
/// - `HY013` Memory management error — (not returned here)
/// - `HY016` Cannot modify an implementation row descriptor — (not returned here; the IRD is
///   deliberately unbacked, so every descriptor gets the same unimplemented answer)
/// - `HY021` Inconsistent descriptor information — (not returned here; the consistency check
///   the spec describes has no stored fields to check)
/// - `HY090` Invalid string or buffer length — **(DM)**; not returned here
/// - `HY117` Connection is suspended — **(DM)**; not returned here
/// - `HYT01` Connection timeout expired — (not returned here; this function performs no I/O)
/// - `IM001` Driver does not support this function — **(DM)**; the Driver Manager returns it
///   from the `SQLGetFunctions` answer, which reports this function unsupported
///
/// Not in the table, but what this function returns:
///
/// - `HYC00` Optional feature not implemented — descriptors are not implemented. See the
///   [module docs](self).
///
/// # Safety
///
/// `descriptor_handle` must be null or a token issued by one of the `alloc_*`
/// functions in `handles`. `data_ptr`, `string_length_ptr` and
/// `indicator_ptr` are never dereferenced.
#[allow(clippy::too_many_arguments)]
pub unsafe fn sql_set_desc_rec<B: Backend>(
    descriptor_handle: *mut c_void,
    record_number: i16,
    value_type: i16,
    sub_type: i16,
    length: isize,
    precision: i16,
    scale: i16,
    data_ptr: *mut c_void,
    string_length_ptr: *mut isize,
    indicator_ptr: *mut isize,
) -> SqlReturn {
    tracing::debug!(
        "SQLSetDescRec(desc={:?}, rec={}, type={}, subtype={}, length={}, precision={}, \
         scale={}, data_ptr={:?}, str_len_ptr={:?}, indicator_ptr={:?})",
        descriptor_handle,
        record_number,
        value_type,
        sub_type,
        length,
        precision,
        scale,
        data_ptr,
        string_length_ptr,
        indicator_ptr,
    );
    let ret = unsafe {
        not_implemented::<B>(
            descriptor_handle,
            "SQLSetDescRec: descriptors are not implemented",
        )
    };
    tracing::debug!("SQLSetDescRec -> {:?}", ret);
    ret
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffi::diag::sql_get_diag_rec_w;
    use crate::ffi::stmt_attr::sql_get_stmt_attr_w;
    use crate::test_utils::{MockBackend, alloc_env_conn_stmt, cleanup_env_conn_stmt};
    use crate::types::sql_state;
    use odbc_sys::{HandleType, StatementAttribute};

    /// The ARD's token, as the application receives it: through
    /// `SQLGetStmtAttrW(SQL_ATTR_APP_ROW_DESC)`. Building one any other way
    /// would test a value no application can hold.
    unsafe fn ard_of(stmt: *mut c_void) -> *mut c_void {
        let mut token: *mut c_void = std::ptr::null_mut();
        let ret = unsafe {
            sql_get_stmt_attr_w::<MockBackend>(
                stmt,
                StatementAttribute::AppRowDesc as i32,
                std::ptr::from_mut(&mut token).cast::<c_void>(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SqlReturn::SUCCESS, "SQLGetStmtAttrW(APP_ROW_DESC)");
        assert!(!token.is_null(), "the ARD token must not be null");
        token
    }

    /// Read back the first diagnostic record posted on `handle`, as
    /// `SQLGetDiagRecW` would — which is the half of this that the old bare
    /// `SQL_ERROR` failed: asserting the return code alone passes against it.
    unsafe fn first_sqlstate(handle: *mut c_void) -> String {
        let mut state = [0u16; 6];
        let mut native: i32 = 0;
        let mut message = [0u16; 256];
        let mut message_len: i16 = 0;
        let ret = unsafe {
            sql_get_diag_rec_w::<MockBackend>(
                HandleType::Desc as i16,
                handle,
                1,
                state.as_mut_ptr(),
                &mut native,
                message.as_mut_ptr(),
                256,
                &mut message_len,
            )
        };
        assert_eq!(ret, SqlReturn::SUCCESS, "no diagnostic record was posted");
        String::from_utf16_lossy(&state[..5])
    }

    #[test]
    fn get_desc_field_reports_hyc00_and_posts_a_diagnostic() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ard = ard_of(stmt);

            let ret = sql_get_desc_field_w::<MockBackend>(
                ard,
                1,
                odbc_sys::Desc::ConciseType as i16,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
            );

            assert_eq!(ret, SqlReturn::ERROR);
            assert_eq!(
                first_sqlstate(ard),
                sql_state::OPTIONAL_FEATURE_NOT_IMPLEMENTED
            );

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    #[test]
    fn set_desc_field_reports_hyc00_and_posts_a_diagnostic() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ard = ard_of(stmt);

            let ret = sql_set_desc_field_w::<MockBackend>(
                ard,
                1,
                odbc_sys::Desc::ConciseType as i16,
                std::ptr::null_mut(),
                0,
            );

            assert_eq!(ret, SqlReturn::ERROR);
            assert_eq!(
                first_sqlstate(ard),
                sql_state::OPTIONAL_FEATURE_NOT_IMPLEMENTED
            );

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    #[test]
    fn set_desc_rec_reports_hyc00_and_posts_a_diagnostic() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ard = ard_of(stmt);

            let ret = sql_set_desc_rec::<MockBackend>(
                ard,
                1,
                odbc_sys::SqlDataType::INTEGER.0,
                0,
                4,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );

            assert_eq!(ret, SqlReturn::ERROR);
            assert_eq!(
                first_sqlstate(ard),
                sql_state::OPTIONAL_FEATURE_NOT_IMPLEMENTED
            );

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// Each descriptor owns its queue. Posting to whichever descriptor the
    /// token names is the whole reason the lookup goes through the statement
    /// rather than casting the address the registry stored, so a diagnostic
    /// landing on a *sibling* descriptor would be the failure that lookup
    /// exists to prevent.
    #[test]
    fn the_diagnostic_lands_on_the_descriptor_that_was_called() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();

            let mut apd: *mut c_void = std::ptr::null_mut();
            let ret = sql_get_stmt_attr_w::<MockBackend>(
                stmt,
                StatementAttribute::AppParamDesc as i32,
                std::ptr::from_mut(&mut apd).cast::<c_void>(),
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let ard = ard_of(stmt);
            assert_ne!(ard, apd, "the ARD and APD must have distinct tokens");

            let _ = sql_set_desc_field_w::<MockBackend>(
                apd,
                1,
                odbc_sys::Desc::ConciseType as i16,
                std::ptr::null_mut(),
                0,
            );

            assert_eq!(
                first_sqlstate(apd),
                sql_state::OPTIONAL_FEATURE_NOT_IMPLEMENTED
            );

            // The ARD was never called, so its queue must still be empty.
            let mut state = [0u16; 6];
            let mut native: i32 = 0;
            let mut message = [0u16; 256];
            let mut message_len: i16 = 0;
            let ret = sql_get_diag_rec_w::<MockBackend>(
                HandleType::Desc as i16,
                ard,
                1,
                state.as_mut_ptr(),
                &mut native,
                message.as_mut_ptr(),
                256,
                &mut message_len,
            );
            assert_eq!(
                ret,
                SqlReturn::NO_DATA,
                "a diagnostic leaked onto a descriptor that was never called"
            );

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// A statement token is a live handle in the same group, so the group check
    /// alone would let it through. It is not a descriptor, and answering
    /// anything but `SQL_INVALID_HANDLE` would mean writing a diagnostic onto a
    /// handle the caller did not name.
    #[test]
    fn a_non_descriptor_handle_is_rejected() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();

            let ret = sql_set_desc_field_w::<MockBackend>(
                stmt,
                1,
                odbc_sys::Desc::ConciseType as i16,
                std::ptr::null_mut(),
                0,
            );
            assert_eq!(ret, SqlReturn::INVALID_HANDLE);

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }
}
