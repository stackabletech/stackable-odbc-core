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

use odbc_sys::Desc;

use crate::backend::{Backend, StatementBackend};
use crate::descriptor::{
    DescFieldValue, DescriptorRole, FieldAccess, field_access, get_record_field, header_attribute,
    header_default,
};
use crate::errors::OdbcError;
use crate::handles::StatementHandle;
use crate::panic::panic_safe;
use crate::types::col_attr::{ColAttrValue, get_column_attribute};
use crate::types::{SqlReturn, SqlState};

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
/// Three sources feed one function, and which one answers is decided by the
/// descriptor's role and the field:
///
/// - **The IRD** is a computed view over the current result set's
///   `ColumnDescriptor`s, delegating to the same
///   [`get_column_attribute`] that implements `SQLColAttributeW`. `SQLColAttribute` and
///   `SQLGetDescField` on the IRD are two spellings of one question, and
///   answering them from two places is how they come to differ. Nothing is
///   stored, so there is no second copy to disagree.
/// - **The header fields** come from the descriptor's own header storage, which
///   is the same storage `SQLGetStmtAttr` reads for the eight statement
///   attributes ODBC defines as header fields.
/// - **Everything else** comes from the addressed record.
///
/// # Parameters
///
/// - `descriptor_handle`: Descriptor handle.
/// - `record_number`: The descriptor record to read from. Ignored for a header field.
/// - `field_identifier`: The `SQL_DESC_*` field to read.
/// - `value_ptr`: Buffer for the returned value. Not written when null.
/// - `buffer_length`: Length of `value_ptr` in bytes, for a character field.
/// - `string_length_ptr`: Receives the available byte count, for a character field.
///
/// # Spec compliance
///
/// Diagnostics from the ODBC spec Diagnostics table:
///
/// - `01000` General warning — (not returned here; core raises no general warning)
/// - `01004` String data, right truncated — returned when a character field does not fit
///   `value_ptr`
/// - `07009` Invalid descriptor index — returned for a negative `record_number` on an ARD or
///   an APD. Read the row's `(DM)` markers clause by clause: they precede only the bookmark
///   clause, so the negative-`RecNumber` clause is the driver's
/// - `08S01` Communication link failure — (not returned here; this function performs no I/O)
/// - `HY000` General error — returned only for an internal panic caught by `panic_safe`
/// - `HY001` Memory allocation error — (not returned here; nothing is allocated)
/// - `HY007` Associated statement is not prepared — returned for any IRD field read before
///   the statement has produced column metadata. The spec: "Until the IRD has been populated,
///   any attempt to gain access to a field of an IRD will return an error"
/// - `HY010` Function sequence error — **(DM)** on all three of its clauses (asynchronous
///   execution and `SQL_NEED_DATA`); not returned here
/// - `HY013` Memory management error — (not returned here)
/// - `HY021` Inconsistent descriptor information — (not returned here; the consistency check
///   runs on a write, and this function writes nothing)
/// - `HY090` Invalid string or buffer length — **(DM)**; not returned here
/// - `HY091` Invalid descriptor field identifier — returned for an unrecognised
///   `field_identifier`, and for one that is not defined on this descriptor's role
/// - `HY117` Connection is suspended — **(DM)**; not returned here
/// - `HYT01` Connection timeout expired — (not returned here; this function performs no I/O)
/// - `IM001` Driver does not support this function — **(DM)**; not returned here
///
/// Also from the Returns section: `SQL_NO_DATA` when `record_number` exceeds
/// `SQL_DESC_COUNT`, which is not an error and not a defaulted record.
///
/// # Safety
///
/// `descriptor_handle` must be null or a token issued by one of the `alloc_*`
/// functions in `handles`. `value_ptr` and `string_length_ptr` must be null or
/// point to writable memory of the size `buffer_length` declares.
pub unsafe fn sql_get_desc_field_w<B: Backend>(
    descriptor_handle: *mut c_void,
    record_number: i16,
    field_identifier: i16,
    value_ptr: *mut c_void,
    buffer_length: i32,
    string_length_ptr: *mut i32,
) -> SqlReturn {
    tracing::trace!(
        "SQLGetDescFieldW(desc={:?}, rec={}, field={}, value_ptr={:?}, buf_len={}, str_len_ptr={:?})",
        descriptor_handle,
        record_number,
        field_identifier,
        value_ptr,
        buffer_length,
        string_length_ptr,
    );
    // SAFETY: `descriptor_handle` is null or a token, which `descriptor_owner`
    // validates without dereferencing. `value_ptr` and `string_length_ptr` are
    // null-checked before every write, and written unaligned because row-wise
    // binding may place either at an arbitrary offset.
    let ret = unsafe {
        panic_safe::<B, _>(descriptor_handle, |scope| {
            let (stmt, role) = scope.descriptor_owner::<B>(descriptor_handle)?;
            // Spec: clear diagnostics at the start of each ODBC call — before
            // the field parse, so an unrecognised identifier reports `HY091`
            // onto an empty queue rather than behind the previous call's
            // records.
            stmt.descriptor_mut(role).diagnostics.clear();

            let field = field_from_raw(field_identifier)?;
            tracing::debug!(
                "SQLGetDescFieldW(desc={:?}, role={:?}, rec={}, field={:?})",
                descriptor_handle,
                role,
                record_number,
                field,
            );

            let Some(value) = read_desc_field::<B>(stmt, role, record_number, field)? else {
                return Ok(SqlReturn::NO_DATA);
            };

            match value {
                DescFieldValue::Numeric(n) => {
                    if !value_ptr.is_null() {
                        std::ptr::write_unaligned(value_ptr.cast::<isize>(), n);
                    }
                    Ok(SqlReturn::SUCCESS)
                }
                DescFieldValue::String(s) => {
                    // As `sql_col_attribute_w`: BufferLength is in bytes and
                    // must be even for the W variant, and StringLengthPtr is
                    // likewise "the total number of bytes". `write_utf16`
                    // counts UTF-16 code units, so convert on the way out.
                    let mut units: i16 = 0;
                    let ret = crate::utf16::note_truncation(
                        crate::utf16::write_utf16(
                            &s,
                            value_ptr.cast::<u16>(),
                            i16::try_from(buffer_length / 2).unwrap_or(i16::MAX),
                            &mut units,
                        ),
                        &mut stmt.descriptor_mut(role).diagnostics,
                    );
                    if !string_length_ptr.is_null() {
                        std::ptr::write_unaligned(string_length_ptr, i32::from(units) * 2);
                    }
                    Ok(ret)
                }
            }
        })
    };
    tracing::debug!("SQLGetDescFieldW -> {:?}", ret);
    ret
}

/// The `SQL_DESC_*` identifier `field_identifier` names.
///
/// `HY091` for an unrecognised value: the spec's row is "Invalid descriptor
/// field identifier", which is exactly what an unparseable one is. Converted
/// here rather than passed on raw, so nothing below this line handles an
/// integer that names no field.
fn field_from_raw(field_identifier: i16) -> Result<Desc, OdbcError> {
    crate::types::desc_from_raw(field_identifier as u16).ok_or_else(|| {
        OdbcError::general(
            format!("Unknown descriptor field identifier: {field_identifier}"),
            SqlState::invalid_descriptor_field_identifier(),
        )
    })
}

/// Read one field of one descriptor, from whichever of the three sources owns
/// it.
///
/// `Ok(None)` means `SQL_NO_DATA`: the record number is past `SQL_DESC_COUNT`,
/// which the Returns section makes a distinct answer from both an error and a
/// defaulted record.
///
/// Shared with `SQLGetDescRecW`, which reads seven fixed record fields and must
/// give the same answers as this for each of them.
fn read_desc_field<B: Backend>(
    stmt: &mut StatementHandle<B>,
    role: DescriptorRole,
    record_number: i16,
    field: Desc,
) -> Result<Option<DescFieldValue>, OdbcError> {
    // Spec 07009: "the RecNumber argument was less than 0, and the
    // DescriptorHandle argument referred to an ARD or an APD". No `(DM)` on
    // that clause, so it is core's.
    if record_number < 0 && matches!(role, DescriptorRole::Ard | DescriptorRole::Apd) {
        return Err(OdbcError::general(
            format!("Record number {record_number} is negative"),
            SqlState::invalid_descriptor_index(),
        ));
    }

    if field_access(role, field) == FieldAccess::Undefined {
        return Err(OdbcError::general(
            format!("Descriptor field {field:?} is not defined on {role:?}"),
            SqlState::invalid_descriptor_field_identifier(),
        ));
    }

    // Header fields first: `RecNumber` is ignored for them, and
    // `SQL_DESC_COUNT` in particular is answered even when there are no
    // records at all.
    if let Some(value) = read_header_field(stmt, role, field) {
        return Ok(Some(DescFieldValue::Numeric(value)));
    }

    if role == DescriptorRole::Ird {
        return read_ird_field(stmt, record_number, field).map(Some);
    }

    let record_number = u16::try_from(record_number).unwrap_or(0);
    let desc = stmt.descriptor_mut(role);
    // Spec, Returns: `SQL_NO_DATA` when `RecNumber` is greater than
    // `SQL_DESC_COUNT`. Derived from the map, as everywhere.
    let Some(record) = desc.records.get(&record_number) else {
        return Ok(None);
    };
    get_record_field(record, role, field).map(Some)
}

/// The header half of [`read_desc_field`], or `None` if `field` is not a header
/// field.
fn read_header_field<B: Backend>(
    stmt: &mut StatementHandle<B>,
    role: DescriptorRole,
    field: Desc,
) -> Option<isize> {
    match field {
        // Every descriptor core owns is implicitly allocated; D4's explicit
        // ones are what make this vary.
        Desc::AllocType => Some(crate::types::SQL_DESC_ALLOC_AUTO),
        // Derived rather than stored, so it cannot disagree with the map. The
        // IRD's records are the result set's columns, which it does not store.
        Desc::Count => Some(match role {
            DescriptorRole::Ird => stmt.statement.as_ref().map_or(0, |s| s.column_count()) as isize,
            _ => stmt.descriptor_mut(role).records.len() as isize,
        }),
        _ => {
            let attr = header_attribute(role, field)?;
            let stored = stmt
                .attr_store(Some(attr))
                .get(&(attr as i32))
                .copied()
                .unwrap_or_else(|| header_default(field));
            Some(stored as isize)
        }
    }
}

/// The IRD half of [`read_desc_field`]: a computed view, never stored state.
fn read_ird_field<B: Backend>(
    stmt: &mut StatementHandle<B>,
    record_number: i16,
    field: Desc,
) -> Result<DescFieldValue, OdbcError> {
    // Spec HY007: "The fields of an IRD have a default value only after the
    // statement has been prepared or executed and the IRD has been populated
    // ... Until the IRD has been populated, any attempt to gain access to a
    // field of an IRD will return an error." Populated is exactly "the backend
    // produced column metadata".
    let statement = stmt.statement.as_mut().filter(|s| s.column_count() > 0);
    let Some(statement) = statement else {
        return Err(OdbcError::general(
            "The IRD is not populated: the statement has not been prepared or executed",
            SqlState::associated_statement_not_prepared(),
        ));
    };
    let column_count = statement.column_count();

    let column = u16::try_from(record_number).unwrap_or(0);
    if column == 0 || record_number > column_count {
        return Err(OdbcError::general(
            format!("Record number {record_number} out of range (have {column_count} columns)"),
            SqlState::invalid_descriptor_index(),
        ));
    }

    let desc = statement.describe_col(column)?;
    Ok(match get_column_attribute(&desc, column_count, field)? {
        ColAttrValue::Numeric(n) => DescFieldValue::Numeric(n),
        ColAttrValue::String(s) => DescFieldValue::String(s),
    })
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
    use crate::handles::StatementHandle;
    use crate::test_utils::{
        MockBackend, MockRecordingBackend, MockTypeInfoBackend, alloc_env_conn_stmt,
        cleanup_env_conn_stmt, with_handle,
    };
    use crate::types::sql_state;
    use odbc_sys::{CDataType, HandleType, StatementAttribute};

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
        unsafe { first_sqlstate_of::<MockBackend>(handle) }
    }

    /// [`first_sqlstate`] for a test driving a backend other than
    /// `MockBackend`. The type parameter is load-bearing: the queue is reached
    /// through the owning statement, which the registry resolves as
    /// `StatementHandle<B>`.
    unsafe fn first_sqlstate_of<B: Backend>(handle: *mut c_void) -> String {
        let mut state = [0u16; 6];
        let mut native: i32 = 0;
        let mut message = [0u16; 256];
        let mut message_len: i16 = 0;
        let ret = unsafe {
            sql_get_diag_rec_w::<B>(
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

    /// The IRD's token, as the application receives it.
    unsafe fn ird_of(stmt: *mut c_void) -> *mut c_void {
        let mut token: *mut c_void = std::ptr::null_mut();
        let ret = unsafe {
            sql_get_stmt_attr_w::<MockBackend>(
                stmt,
                StatementAttribute::ImpRowDesc as i32,
                std::ptr::from_mut(&mut token).cast::<c_void>(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SqlReturn::SUCCESS, "SQLGetStmtAttrW(IMP_ROW_DESC)");
        assert!(!token.is_null(), "the IRD token must not be null");
        token
    }

    /// Put `count` records into the ARD without going through
    /// `SQLSetDescFieldW`, which is still unimplemented at this point. Task 7's
    /// tests drive the real round trip.
    unsafe fn seed_ard_records(stmt: *mut c_void, count: u16, concise_type: i16) {
        with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
            for column in 1..=count {
                let mut record = crate::descriptor::DescriptorRecord::default();
                record.set_concise_type(concise_type);
                handle.app_row_desc.records.insert(column, record);
            }
        });
    }

    /// A record field reads back through the descriptor.
    #[test]
    fn get_desc_field_reads_back_a_record_field() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ard = ard_of(stmt);
            seed_ard_records(stmt, 1, CDataType::SBigInt as i16);

            let mut value: isize = 0;
            let ret = sql_get_desc_field_w::<MockBackend>(
                ard,
                1,
                Desc::ConciseType as i16,
                std::ptr::from_mut(&mut value).cast::<c_void>(),
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(value, CDataType::SBigInt as isize);

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// The spec: "Until the IRD has been populated, any attempt to gain access
    /// to a field of an IRD will return an error." `HY007` is the row for it,
    /// un-annotated, and its description is this case verbatim.
    #[test]
    fn reading_the_ird_before_execution_reports_hy007() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ird = ird_of(stmt);

            let mut value: isize = 0;
            let ret = sql_get_desc_field_w::<MockBackend>(
                ird,
                1,
                Desc::ConciseType as i16,
                std::ptr::from_mut(&mut value).cast::<c_void>(),
                0,
                std::ptr::null_mut(),
            );

            assert_eq!(ret, SqlReturn::ERROR);
            assert_eq!(
                first_sqlstate(ird),
                sql_state::ASSOCIATED_STATEMENT_NOT_PREPARED
            );

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// An executed statement that produced no result set has an unpopulated
    /// IRD too, and reports the same `HY007`.
    ///
    /// The distinction matters because "not executed" and "executed, no
    /// columns" are different statement states — S2 versus S4 — and only the
    /// second has a `stmt.statement` to look at. A check that tested only for
    /// the absence of a statement would sail past this one and then ask a
    /// column-less result set to describe column 1.
    #[test]
    fn reading_the_ird_of_a_statement_with_no_result_set_reports_hy007() {
        unsafe {
            let (env, conn, stmt) =
                crate::test_utils::alloc_connected_env_conn_stmt::<MockRecordingBackend>();

            let sql: Vec<u16> = "UPDATE t SET a = 1".encode_utf16().collect();
            let ret = crate::ffi::execute::sql_exec_direct_w::<MockRecordingBackend>(
                stmt,
                sql.as_ptr(),
                i32::try_from(sql.len()).expect("short"),
            );
            assert_eq!(ret, SqlReturn::SUCCESS, "precondition: the statement ran");

            let mut ird: *mut c_void = std::ptr::null_mut();
            let ret = sql_get_stmt_attr_w::<MockRecordingBackend>(
                stmt,
                StatementAttribute::ImpRowDesc as i32,
                std::ptr::from_mut(&mut ird).cast::<c_void>(),
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let mut value: isize = 0;
            let ret = sql_get_desc_field_w::<MockRecordingBackend>(
                ird,
                1,
                Desc::ConciseType as i16,
                std::ptr::from_mut(&mut value).cast::<c_void>(),
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::ERROR);
            assert_eq!(
                first_sqlstate_of::<MockRecordingBackend>(ird),
                sql_state::ASSOCIATED_STATEMENT_NOT_PREPARED,
                "a result set with no columns is not a populated IRD"
            );

            crate::test_utils::cleanup_connected_env_conn_stmt::<MockRecordingBackend>(
                env, conn, stmt,
            );
        }
    }

    /// After execution the IRD answers from the same `ColumnDescriptor`
    /// `SQLColAttributeW` reads, so the two cannot disagree about a column.
    /// That is the whole reason the IRD is a computed view rather than stored
    /// state — a second copy is a second thing to be wrong.
    ///
    /// Driven through `SQLGetTypeInfo`, whose result set core owns and can
    /// describe; the plain mocks take `describe_col`'s `NotImplemented`
    /// default, so neither side would have an answer to agree on.
    #[test]
    fn the_ird_agrees_with_sqlcolattribute() {
        unsafe {
            let (env, conn, stmt) =
                crate::test_utils::alloc_connected_env_conn_stmt::<MockTypeInfoBackend>();
            let ret = crate::ffi::info::sql_get_type_info::<MockTypeInfoBackend>(
                stmt,
                crate::types::SqlDataType::UNKNOWN_TYPE.0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let mut ird: *mut c_void = std::ptr::null_mut();
            let ret = sql_get_stmt_attr_w::<MockTypeInfoBackend>(
                stmt,
                StatementAttribute::ImpRowDesc as i32,
                std::ptr::from_mut(&mut ird).cast::<c_void>(),
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            for field in [Desc::ConciseType, Desc::Nullable, Desc::Length] {
                let mut via_desc: isize = 0;
                let ret = sql_get_desc_field_w::<MockTypeInfoBackend>(
                    ird,
                    1,
                    field as i16,
                    std::ptr::from_mut(&mut via_desc).cast::<c_void>(),
                    0,
                    std::ptr::null_mut(),
                );
                assert_eq!(ret, SqlReturn::SUCCESS, "{field:?} through the IRD");

                let mut via_col_attr: isize = 0;
                let ret = crate::ffi::metadata::sql_col_attribute_w::<MockTypeInfoBackend>(
                    stmt,
                    1,
                    field as u16,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    &mut via_col_attr,
                );
                assert_eq!(
                    ret,
                    SqlReturn::SUCCESS,
                    "{field:?} through SQLColAttributeW"
                );

                assert_eq!(
                    via_desc, via_col_attr,
                    "the IRD and SQLColAttributeW disagree about column 1's {field:?}"
                );
            }

            crate::test_utils::cleanup_connected_env_conn_stmt::<MockTypeInfoBackend>(
                env, conn, stmt,
            );
        }
    }

    /// `SQL_DESC_COUNT` is derived from the record map, so it cannot disagree
    /// with it. `RecNumber` is ignored for a header field.
    #[test]
    fn desc_count_counts_the_records() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ard = ard_of(stmt);
            seed_ard_records(stmt, 3, CDataType::SLong as i16);

            let mut count: isize = 0;
            let ret = sql_get_desc_field_w::<MockBackend>(
                ard,
                0,
                Desc::Count as i16,
                std::ptr::from_mut(&mut count).cast::<c_void>(),
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(count, 3);

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// A negative record number is the driver's to reject: the `(DM)` on this
    /// row precedes its *other* clauses, not this one.
    #[test]
    fn a_negative_record_number_reports_07009() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ard = ard_of(stmt);

            let mut value: isize = 0;
            let ret = sql_get_desc_field_w::<MockBackend>(
                ard,
                -1,
                Desc::ConciseType as i16,
                std::ptr::from_mut(&mut value).cast::<c_void>(),
                0,
                std::ptr::null_mut(),
            );

            assert_eq!(ret, SqlReturn::ERROR);
            assert_eq!(first_sqlstate(ard), sql_state::INVALID_DESCRIPTOR_INDEX);

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// Diagnostics are cleared at the start of every call, and *before* the
    /// field identifier is parsed.
    ///
    /// The parse is the first thing that can fail, so clearing after it would
    /// leave `HY091` queued behind the previous call's records — and
    /// `SQLGetDiagRec` numbers from 1, so the application would read the stale
    /// one and never see the real answer.
    #[test]
    fn diagnostics_are_cleared_before_the_field_identifier_is_parsed() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ard = ard_of(stmt);
            const NOT_A_FIELD: i16 = 31337;

            // A first failure of a different kind, to leave a record behind.
            let mut value: isize = 0;
            let ret = sql_get_desc_field_w::<MockBackend>(
                ard,
                -1,
                Desc::ConciseType as i16,
                std::ptr::from_mut(&mut value).cast::<c_void>(),
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::ERROR);
            assert_eq!(
                first_sqlstate(ard),
                sql_state::INVALID_DESCRIPTOR_INDEX,
                "precondition: a record is queued"
            );

            let ret = sql_get_desc_field_w::<MockBackend>(
                ard,
                1,
                NOT_A_FIELD,
                std::ptr::from_mut(&mut value).cast::<c_void>(),
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::ERROR);
            assert_eq!(
                first_sqlstate(ard),
                sql_state::INVALID_DESCRIPTOR_FIELD_IDENTIFIER,
                "the queue was not cleared before the field parse"
            );

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// `SQL_NO_DATA` when the record number is past `SQL_DESC_COUNT`, per the
    /// Returns section — not an error, and not a defaulted record.
    #[test]
    fn a_record_number_past_the_count_returns_no_data() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ard = ard_of(stmt);

            let mut value: isize = 0;
            let ret = sql_get_desc_field_w::<MockBackend>(
                ard,
                7,
                Desc::ConciseType as i16,
                std::ptr::from_mut(&mut value).cast::<c_void>(),
                0,
                std::ptr::null_mut(),
            );

            assert_eq!(ret, SqlReturn::NO_DATA);

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
