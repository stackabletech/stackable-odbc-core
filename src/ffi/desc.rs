//! `SQLGetDescFieldW`, `SQLSetDescFieldW`, `SQLGetDescRecW` and `SQLSetDescRec`
//! — the four accessors over a statement's own descriptors.
//!
//! # How a call reaches a field
//!
//! Every one of the four takes a descriptor handle and nothing that says which
//! of the four descriptors it is. `HandleScope::descriptor_owner` answers that:
//! it resolves the token to the *statement* that owns it and to the role, by
//! comparing the token against the statement's four fields. Casting the address
//! the registry stored could not, because `HandleKind::Desc` is one kind
//! covering four roles — see that function, and `Descriptor`'s note on why it
//! deliberately has no `HasKind` impl.
//!
//! From that one `&mut` the caller reaches whichever of three sources owns the
//! field:
//!
//! - the **record map**, for the ARD, APD and IPD;
//! - the **header storage**, which is the same storage `SQLGetStmtAttr` reads
//!   for the eight statement attributes ODBC defines as descriptor header
//!   fields;
//! - the **IRD's computed view** over the current result set's
//!   `ColumnDescriptor`s, which stores nothing and delegates to the same
//!   `get_column_attribute` that implements `SQLColAttributeW`.
//!
//! `descriptor::field_access` decides `HY091` for all of them, from the spec's
//! "Initialization of Descriptor Fields" tables.
//!
//! # What is still missing
//!
//! `SQLCopyDesc` and `SQLAllocHandle(SQL_HANDLE_DESC)` — explicitly allocated
//! descriptors, which belong to a *connection* rather than a statement and so
//! do not resolve through `descriptor_owner`'s parent routing at all. Until
//! they land, **`SQL_OIC_CORE` is not satisfied**: Core-level conformance
//! requires working descriptors, and these four make a statement's own
//! descriptors real without making an application's own possible.

use std::ffi::c_void;

use odbc_sys::Desc;

use crate::backend::{Backend, StatementBackend};
use crate::descriptor::{
    DescFieldValue, DescriptorRole, FieldAccess, consistency_check, field_access, get_record_field,
    header_attribute, header_default, set_record_field,
};
use crate::errors::OdbcError;
use crate::handles::StatementHandle;
use crate::panic::panic_safe;
use crate::types::col_attr::{ColAttrValue, get_column_attribute};
use crate::types::{SqlReturn, SqlState};

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
            // `header_attribute` still answers "is this field stored as a
            // statement attribute on this role", which is what keeps the four
            // IRD/IPD pairs on `StatementHandle::attrs`; `attr_get` then routes
            // to the descriptor header or to that bag, keyed by the field in
            // the first case.
            let attr = header_attribute(role, field)?;
            let stored = stmt
                .attr_get(attr as i32)
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
/// The write counterpart of [`sql_get_desc_field_w`], routed the same three
/// ways — except that the IRD is not writable at all beyond the two header
/// fields the spec's `HY016` row names, because it is a computed view and has
/// nothing to write into.
///
/// # Parameters
///
/// - `descriptor_handle`: Descriptor handle.
/// - `record_number`: The descriptor record to write to. Ignored for a header field.
/// - `field_identifier`: The `SQL_DESC_*` field to write.
/// - `value_ptr`: The value to write, either as an integer or as a pointer, per the field.
/// - `buffer_length`: Length of `value_ptr` in bytes for a character field, or an
///   `SQL_IS_*` marker.
///
/// # Spec compliance
///
/// Diagnostics from the ODBC spec Diagnostics table:
///
/// - `01000` General warning — (not returned here; core raises no general warning)
/// - `01S02` Option value changed — returned for `SQL_DESC_ARRAY_SIZE` above 1, which core
///   substitutes back to 1. It is the same value and the same substitution
///   `SQLSetStmtAttr(SQL_ATTR_ROW_ARRAY_SIZE)` applies, through the other door
/// - `07009` Invalid descriptor index — returned for a negative `record_number` on an ARD or
///   an APD. The `(DM)` markers on this row precede the `SQL_DESC_COUNT` and
///   implicitly-allocated-APD clauses only; read them clause by clause
/// - `08S01` Communication link failure — (not returned here; this function performs no I/O)
/// - `22001` String data, right truncated — (not returned here; `SQL_DESC_NAME` is stored as
///   given, and core imposes no length limit of its own on it)
/// - `HY000` General error — returned only for an internal panic caught by `panic_safe`
/// - `HY001` Memory allocation error — (not returned here; nothing is allocated)
/// - `HY010` Function sequence error — **(DM)** on all four of its clauses; not returned here
/// - `HY013` Memory management error — (not returned here)
/// - `HY016` Cannot modify an implementation row descriptor — returned for any IRD write
///   other than `SQL_DESC_ARRAY_STATUS_PTR` and `SQL_DESC_ROWS_PROCESSED_PTR`, which the
///   row exempts by name
/// - `HY021` Inconsistent descriptor information — returned when setting
///   `SQL_DESC_DATA_PTR` leaves the record failing the spec's consistency check
/// - `HY090` Invalid string or buffer length — **(DM)** on both of its clauses; not returned
///   here
/// - `HY091` Invalid descriptor field identifier — returned for an unrecognised
///   `field_identifier`, for one that is not defined on this descriptor's role, and for one
///   that is defined but read-only there
/// - `HY092` Invalid attribute/option identifier — returned when the value is not valid for
///   the field: a numeric handed to `SQL_DESC_NAME`, a value too wide for the field, or
///   `SQL_NAMED` written to `SQL_DESC_UNNAMED`, which the row names explicitly
/// - `HY105` Invalid parameter type — **(DM)**; not returned here
/// - `HY117` Connection is suspended — **(DM)**; not returned here
/// - `HYT01` Connection timeout expired — (not returned here; this function performs no I/O)
/// - `IM001` Driver does not support this function — **(DM)**; not returned here
///
/// # Safety
///
/// `descriptor_handle` must be null or a token issued by one of the `alloc_*`
/// functions in `handles`. `value_ptr` is dereferenced only for a character
/// field, where it must point to `buffer_length` readable bytes; for every
/// other field it is the value itself and is never read through.
pub unsafe fn sql_set_desc_field_w<B: Backend>(
    descriptor_handle: *mut c_void,
    record_number: i16,
    field_identifier: i16,
    value_ptr: *mut c_void,
    buffer_length: i32,
) -> SqlReturn {
    tracing::trace!(
        "SQLSetDescFieldW(desc={:?}, rec={}, field={}, value_ptr={:?}, buf_len={})",
        descriptor_handle,
        record_number,
        field_identifier,
        value_ptr,
        buffer_length,
    );
    // SAFETY: `descriptor_handle` is null or a token, which `descriptor_owner`
    // validates without dereferencing. `value_ptr` is read through only for a
    // character field, where the caller guarantees `buffer_length` readable
    // bytes.
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
                "SQLSetDescFieldW(desc={:?}, role={:?}, rec={}, field={:?})",
                descriptor_handle,
                role,
                record_number,
                field,
            );

            write_desc_field(stmt, role, record_number, field, value_ptr, buffer_length)
        })
    };
    tracing::debug!("SQLSetDescFieldW -> {:?}", ret);
    ret
}

/// The body of [`sql_set_desc_field_w`], once the handle and field are known.
///
/// # Safety
///
/// As [`sql_set_desc_field_w`]: `value_ptr` must point to `buffer_length`
/// readable bytes when `field` is a character field.
unsafe fn write_desc_field<B: Backend>(
    stmt: &mut StatementHandle<B>,
    role: DescriptorRole,
    record_number: i16,
    field: Desc,
    value_ptr: *mut c_void,
    buffer_length: i32,
) -> Result<SqlReturn, OdbcError> {
    // Spec 07009, as in the read direction.
    if record_number < 0 && matches!(role, DescriptorRole::Ard | DescriptorRole::Apd) {
        return Err(OdbcError::general(
            format!("Record number {record_number} is negative"),
            SqlState::invalid_descriptor_index(),
        ));
    }

    // Spec HY016: "The DescriptorHandle argument was associated with an IRD,
    // and the FieldIdentifier argument was not SQL_DESC_ARRAY_STATUS_PTR or
    // SQL_DESC_ROWS_PROCESSED_PTR." Checked before HY091 because it is the
    // more specific answer: the field may be perfectly well defined on an IRD
    // and still not writable there.
    if role == DescriptorRole::Ird
        && !matches!(field, Desc::ArrayStatusPtr | Desc::RowsProcessedPtr)
    {
        return Err(OdbcError::general(
            format!("Descriptor field {field:?} cannot be written on an IRD"),
            SqlState::cannot_modify_ird(),
        ));
    }

    match field_access(role, field) {
        FieldAccess::ReadWrite => {}
        FieldAccess::ReadOnly | FieldAccess::Undefined => {
            return Err(OdbcError::general(
                format!("Descriptor field {field:?} is not writable on {role:?}"),
                SqlState::invalid_descriptor_field_identifier(),
            ));
        }
    }

    // A header field: `RecNumber` is ignored, and the value goes to the same
    // storage `SQLSetStmtAttr` writes.
    if field == Desc::Count {
        return Ok(truncate_records(stmt, role, value_ptr as usize));
    }
    if field == Desc::AllocType {
        // Read-only on every role, so `field_access` has already refused it.
        // Unreachable, and cheaper to state than to leave as a silent fall
        // through into the record path.
        return Err(OdbcError::general(
            "SQL_DESC_ALLOC_TYPE is read-only",
            SqlState::invalid_descriptor_field_identifier(),
        ));
    }
    if let Some(attr) = header_attribute(role, field) {
        return Ok(write_header_field(
            stmt,
            role,
            field,
            attr,
            value_ptr as usize,
        ));
    }

    // A record field. `or_default` is what makes a record exist as soon as any
    // one field is set, which is the whole reason boundness moved to the data
    // pointer.
    let record_number = u16::try_from(record_number).unwrap_or(0);
    let value = if field == Desc::Name {
        // SAFETY: forwarded from this function's contract.
        DescFieldValue::String(unsafe {
            crate::utf16::utf16_to_string(value_ptr.cast::<u16>(), buffer_length / 2)?
        })
    } else {
        DescFieldValue::Numeric(value_ptr as isize)
    };

    let record = stmt
        .descriptor_mut(role)
        .records
        .entry(record_number)
        .or_default();
    set_record_field(record, role, field, value)?;

    // Spec: the consistency check runs "when SQLSetDescField is called to set
    // the SQL_DESC_DATA_PTR field". Not on every field, because a record is
    // built one field at a time and would be inconsistent for most of that.
    //
    // A failure leaves the record as it is: "If a call to SQLSetDescRec fails,
    // the contents of the descriptor record identified by the RecNumber
    // argument are undefined", so no rollback is owed — only a report before
    // anything reads it.
    if field == Desc::DataPtr {
        consistency_check(record, role)?;
    }
    Ok(SqlReturn::SUCCESS)
}

/// `SQL_DESC_COUNT`, which is a write to the record *map* rather than to a
/// stored field.
///
/// The spec: "the driver deallocates the descriptor records for all records
/// whose number is greater than the value in SQL_DESC_COUNT". Deriving the
/// count from the map on read is what makes the two agree; discarding here is
/// what keeps them agreeing after a write.
fn truncate_records<B: Backend>(
    stmt: &mut StatementHandle<B>,
    role: DescriptorRole,
    count: usize,
) -> SqlReturn {
    let count = u16::try_from(count).unwrap_or(u16::MAX);
    stmt.descriptor_mut(role)
        .records
        .retain(|&number, _| number <= count);
    SqlReturn::SUCCESS
}

/// A header field write, with the one value core cannot honour substituted.
///
/// `SQL_DESC_ARRAY_SIZE` is `SQL_ATTR_ROW_ARRAY_SIZE` on the ARD and
/// `SQL_ATTR_PARAMSET_SIZE` on the APD, and `SQLSetStmtAttr` substitutes any
/// value but 1 with `01S02` because core rejects a multi-row rowset. The same
/// value reached through the descriptor gets the same answer, or the two views
/// disagree — which is the defect this milestone exists to remove. The
/// warning lands on the *descriptor's* queue, since that is the handle the
/// application named.
fn write_header_field<B: Backend>(
    stmt: &mut StatementHandle<B>,
    role: DescriptorRole,
    field: Desc,
    attr: odbc_sys::StatementAttribute,
    value: usize,
) -> SqlReturn {
    let substituted = field == Desc::ArraySize && value != SQL_DESC_ARRAY_SIZE_SUPPORTED;
    let stored = if substituted {
        SQL_DESC_ARRAY_SIZE_SUPPORTED
    } else {
        value
    };
    stmt.attr_set(attr as i32, stored);
    if substituted {
        return crate::ffi::stmt_attr::substitution_warning(
            &mut stmt.descriptor_mut(role).diagnostics,
            "SQL_DESC_ARRAY_SIZE",
            value,
            "1",
        );
    }
    SqlReturn::SUCCESS
}

/// The only `SQL_DESC_ARRAY_SIZE` core can honour.
///
/// A rowset larger than one row would need block cursors, which
/// `SQLSetStmtAttrW` already refuses for the same reason under the attribute's
/// other name.
const SQL_DESC_ARRAY_SIZE_SUPPORTED: usize = 1;

/// Generic implementation of SQLGetDescRecW.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlgetdescrec-function>
///
/// Seven fixed record fields in one call, where [`sql_get_desc_field_w`] reads
/// one named field. Two properties from the page shape the implementation:
///
/// - It **"does not retrieve the values for header fields"**. `SQL_DESC_COUNT`
///   and the rest are `SQLGetDescField`'s alone, so a `record_number` here
///   always addresses a record.
/// - **Every output pointer is independently optional:** "an application can
///   prevent the return of a field's setting by setting the argument that
///   corresponds to the field to a null pointer." So each is null-checked on
///   its own rather than as a group.
///
/// The `W` suffix is required by the project's rule that every string-bearing
/// function is exported in its Wide form only; `Name` is that string.
/// `SQLSetDescRec` takes none and keeps its unsuffixed spelling.
///
/// # Parameters
///
/// - `descriptor_handle`: Descriptor handle.
/// - `record_number`: The descriptor record to read.
/// - `name`: Buffer for `SQL_DESC_NAME`.
/// - `buffer_length`: Length of `name` in bytes.
/// - `string_length_ptr`: Receives the available byte count for `name`.
/// - `type_ptr`: Receives `SQL_DESC_TYPE`.
/// - `sub_type_ptr`: Receives `SQL_DESC_DATETIME_INTERVAL_CODE`, for a datetime
///   or interval type.
/// - `length_ptr`: Receives `SQL_DESC_OCTET_LENGTH`.
/// - `precision_ptr`: Receives `SQL_DESC_PRECISION`.
/// - `scale_ptr`: Receives `SQL_DESC_SCALE`.
/// - `nullable_ptr`: Receives `SQL_DESC_NULLABLE`.
///
/// # Spec compliance
///
/// This is the narrowest diagnostics table of the four descriptor functions,
/// and its own page says why: it reads seven fixed fields and takes no field
/// identifier to be wrong about, so there is no `HY091`, `HY092`, `HY016` or
/// `HY021` row at all.
///
/// - `01000` General warning — (not returned here; core raises no general warning)
/// - `01004` String data, right truncated — returned when `SQL_DESC_NAME` does not fit `name`
/// - `07009` Invalid descriptor index — returned for a negative `record_number` on an ARD or
///   an APD, and for a record number outside the current result set on an IRD
/// - `08S01` Communication link failure — (not returned here; this function performs no I/O)
/// - `HY000` General error — returned only for an internal panic caught by `panic_safe`
/// - `HY001` Memory allocation error — (not returned here; nothing is allocated)
/// - `HY007` Associated statement is not prepared — returned for an IRD read before the
///   statement has produced column metadata
/// - `HY010` Function sequence error — **(DM)**; not returned here
/// - `HY013` Memory management error — (not returned here)
/// - `HY117` Connection is suspended — **(DM)**; not returned here
/// - `HYT01` Connection timeout expired — (not returned here; this function performs no I/O)
/// - `IM001` Driver does not support this function — **(DM)**; not returned here
///
/// Also from the Returns section: `SQL_NO_DATA` when `record_number` exceeds
/// `SQL_DESC_COUNT`.
///
/// # Safety
///
/// `descriptor_handle` must be null or a token issued by one of the `alloc_*`
/// functions in `handles`. Every output pointer must be null or point to
/// writable memory of its own type; `name` must be null or point to
/// `buffer_length` writable bytes.
#[allow(clippy::too_many_arguments)]
pub unsafe fn sql_get_desc_rec_w<B: Backend>(
    descriptor_handle: *mut c_void,
    record_number: i16,
    name: *mut u16,
    buffer_length: i16,
    string_length_ptr: *mut i16,
    type_ptr: *mut i16,
    sub_type_ptr: *mut i16,
    length_ptr: *mut isize,
    precision_ptr: *mut i16,
    scale_ptr: *mut i16,
    nullable_ptr: *mut i16,
) -> SqlReturn {
    tracing::debug!(
        "SQLGetDescRecW(desc={:?}, rec={}, name={:?}, buf_len={})",
        descriptor_handle,
        record_number,
        name,
        buffer_length,
    );
    // SAFETY: `descriptor_handle` is null or a token, which `descriptor_owner`
    // validates without dereferencing. Every output pointer is null-checked
    // before its write, and written unaligned because row-wise binding may
    // place any of them at an arbitrary offset.
    let ret = unsafe {
        panic_safe::<B, _>(descriptor_handle, |scope| {
            let (stmt, role) = scope.descriptor_owner::<B>(descriptor_handle)?;
            // Spec: clear diagnostics at the start of each ODBC call.
            stmt.descriptor_mut(role).diagnostics.clear();

            // The six numeric fields, each read through the same
            // `get_record_field` `SQLGetDescField` uses — one mapping, two
            // callers, so the two functions cannot report a record differently.
            // `SQL_NO_DATA` from any one of them is `SQL_NO_DATA` for the call:
            // they all address the same record.
            let mut numeric = |field: Desc| -> Result<Option<isize>, OdbcError> {
                Ok(
                    match read_desc_field::<B>(stmt, role, record_number, field)? {
                        Some(DescFieldValue::Numeric(n)) => Some(n),
                        // A string where a number was expected cannot happen
                        // for these six, and `None` is `SQL_NO_DATA`.
                        Some(DescFieldValue::String(_)) | None => None,
                    },
                )
            };

            let Some(verbose_type) = numeric(Desc::Type)? else {
                return Ok(SqlReturn::NO_DATA);
            };
            let sub_type = numeric(Desc::DatetimeIntervalCode)?.unwrap_or(0);
            let octet_length = numeric(Desc::OctetLength)?.unwrap_or(0);
            let precision = numeric(Desc::Precision)?.unwrap_or(0);
            let scale = numeric(Desc::Scale)?.unwrap_or(0);
            let nullable = numeric(Desc::Nullable)?
                .unwrap_or(crate::types::Nullable::SqlNullableUnknown as isize);

            write_small_int(type_ptr, verbose_type);
            write_small_int(sub_type_ptr, sub_type);
            write_small_int(precision_ptr, precision);
            write_small_int(scale_ptr, scale);
            write_small_int(nullable_ptr, nullable);
            if !length_ptr.is_null() {
                std::ptr::write_unaligned(length_ptr, octet_length);
            }

            // `SQL_DESC_NAME` is a character field, and its buffer is optional
            // like every other output here.
            let Some(DescFieldValue::String(field_name)) =
                read_desc_field::<B>(stmt, role, record_number, Desc::Name)?
            else {
                return Ok(SqlReturn::SUCCESS);
            };
            let mut units: i16 = 0;
            let ret = crate::utf16::note_truncation(
                crate::utf16::write_utf16(&field_name, name, buffer_length / 2, &mut units),
                &mut stmt.descriptor_mut(role).diagnostics,
            );
            if !string_length_ptr.is_null() {
                std::ptr::write_unaligned(string_length_ptr, units.saturating_mul(2));
            }
            Ok(ret)
        })
    };
    tracing::debug!("SQLGetDescRecW -> {:?}", ret);
    ret
}

/// Write one of `SQLGetDescRecW`'s `SQLSMALLINT` outputs, if the application
/// asked for it.
///
/// Saturating rather than truncating: a value too wide for the ABI's own type
/// is not something the application can act on, and silently wrapping it would
/// report a different field value than the descriptor holds.
///
/// # Safety
///
/// `ptr` must be null or point to a writable `i16`.
unsafe fn write_small_int(ptr: *mut i16, value: isize) {
    if ptr.is_null() {
        return;
    }
    let narrowed = i16::try_from(value).unwrap_or_else(|_| {
        tracing::warn!("SQLGetDescRecW: value {value} does not fit an SQLSMALLINT, saturating");
        i16::MAX
    });
    // SAFETY: `ptr` is non-null and the caller guarantees it is writable;
    // unaligned because row-wise binding may place it at any offset.
    unsafe { std::ptr::write_unaligned(ptr, narrowed) };
}

/// Generic implementation of SQLSetDescRec.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlsetdescrec-function>
///
/// Eight record fields in one call, where [`sql_set_desc_field_w`] sets one
/// named field — and the consistency check runs once at the end rather than on
/// every field, because a record assembled field by field is inconsistent for
/// most of that.
///
/// Two differences from [`sql_get_desc_rec_w`], both from the page's own
/// argument descriptions:
///
/// - **A `record_number` above `SQL_DESC_COUNT` raises the count** rather than
///   answering `SQL_NO_DATA`. This function grows the record set; the read one
///   reports the end of it.
/// - **`HY016` is unconditional on an IRD.** `SQLSetDescField` exempts
///   `SQL_DESC_ARRAY_STATUS_PTR` and `SQL_DESC_ROWS_PROCESSED_PTR`, but those
///   are header fields and this function sets record fields only, so no
///   exemption can apply.
///
/// No `W` suffix: every argument is numeric or a deferred data pointer, so
/// there is one spelling of this function rather than an ANSI and a Wide form.
///
/// # Parameters
///
/// - `descriptor_handle`: Descriptor handle.
/// - `record_number`: The descriptor record to write to.
/// - `value_type`: The value for `SQL_DESC_TYPE`.
/// - `sub_type`: The value for `SQL_DESC_DATETIME_INTERVAL_CODE`, for a datetime or
///   interval `value_type`.
/// - `length`: The value for `SQL_DESC_OCTET_LENGTH`.
/// - `precision`: The value for `SQL_DESC_PRECISION`.
/// - `scale`: The value for `SQL_DESC_SCALE`.
/// - `data_ptr`: The value for `SQL_DESC_DATA_PTR`. A deferred pointer, never dereferenced
///   here; null unbinds.
/// - `string_length_ptr`: The value for `SQL_DESC_OCTET_LENGTH_PTR`. Never dereferenced.
/// - `indicator_ptr`: The value for `SQL_DESC_INDICATOR_PTR`. Never dereferenced.
///
/// # Spec compliance
///
/// Diagnostics from the ODBC spec Diagnostics table:
///
/// - `01000` General warning — (not returned here; core raises no general warning)
/// - `07009` Invalid descriptor index — returned for a negative `record_number` on an ARD or
///   an APD. No clause of this row is annotated `(DM)`
/// - `08S01` Communication link failure — (not returned here; this function performs no I/O)
/// - `HY000` General error — returned only for an internal panic caught by `panic_safe`
/// - `HY001` Memory allocation error — (not returned here; nothing is allocated)
/// - `HY010` Function sequence error — **(DM)** on all four of its clauses; not returned here
/// - `HY013` Memory management error — (not returned here)
/// - `HY016` Cannot modify an implementation row descriptor — returned for any IRD write
/// - `HY021` Inconsistent descriptor information — returned when the assembled record fails
///   the spec's consistency check
/// - `HY090` Invalid string or buffer length — **(DM)**; not returned here
/// - `HY117` Connection is suspended — **(DM)**; not returned here
/// - `HYT01` Connection timeout expired — (not returned here; this function performs no I/O)
/// - `IM001` Driver does not support this function — **(DM)**; not returned here
///
/// # Safety
///
/// `descriptor_handle` must be null or a token issued by one of the `alloc_*`
/// functions in `handles`. `data_ptr`, `string_length_ptr` and
/// `indicator_ptr` are stored as given and never dereferenced.
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
    // SAFETY: `descriptor_handle` is null or a token, which `descriptor_owner`
    // validates without dereferencing. The three pointer arguments are stored
    // as values and never read through.
    let ret = unsafe {
        panic_safe::<B, _>(descriptor_handle, |scope| {
            let (stmt, role) = scope.descriptor_owner::<B>(descriptor_handle)?;
            // Spec: clear diagnostics at the start of each ODBC call.
            stmt.descriptor_mut(role).diagnostics.clear();

            // Spec 07009, as in the other three. No clause of this function's
            // row is annotated `(DM)`.
            if record_number < 0 && matches!(role, DescriptorRole::Ard | DescriptorRole::Apd) {
                return Err(OdbcError::general(
                    format!("Record number {record_number} is negative"),
                    SqlState::invalid_descriptor_index(),
                ));
            }

            // Spec HY016: "The DescriptorHandle argument was associated with an
            // IRD." Unconditional, because the two fields `SQLSetDescField`'s
            // HY016 row exempts are header fields and this function sets record
            // fields only.
            if role == DescriptorRole::Ird {
                return Err(OdbcError::general(
                    "An implementation row descriptor cannot be modified",
                    SqlState::cannot_modify_ird(),
                ));
            }

            // The spec's own list, in its order. Each goes through
            // `set_record_field` so the `HY091` rules are identical to
            // `SQLSetDescField`'s — a field this role does not define is
            // refused the same way through both doors.
            //
            // `SQL_DESC_TYPE` is set before the subcode, because setting the
            // type resets the subcode for a non-datetime type and would
            // otherwise discard what this call was given.
            let fields = [
                (Desc::Type, isize::from(value_type)),
                (Desc::DatetimeIntervalCode, isize::from(sub_type)),
                (Desc::OctetLength, length),
                (Desc::Precision, isize::from(precision)),
                (Desc::Scale, isize::from(scale)),
                (Desc::DataPtr, data_ptr as isize),
                (Desc::OctetLengthPtr, string_length_ptr as isize),
                (Desc::IndicatorPtr, indicator_ptr as isize),
            ];

            // `or_default` creates the record if `record_number` is above the
            // current `SQL_DESC_COUNT`: the *RecNumber* description makes this
            // function grow the record set, which is the difference from
            // `SQLGetDescRec` answering `SQL_NO_DATA` there.
            let record_key = u16::try_from(record_number).unwrap_or(0);
            let record = stmt
                .descriptor_mut(role)
                .records
                .entry(record_key)
                .or_default();

            for (field, value) in fields {
                // The IPD defines neither pointer field, and the spec expects
                // this call to work against one — its `SQL_DESC_DATA_PTR` is
                // the documented oddity below. Skipping them here rather than
                // reporting `HY091` is what lets an application set an IPD
                // record with the same call it uses for an APD.
                if field_access(role, field) != FieldAccess::ReadWrite {
                    continue;
                }
                set_record_field(record, role, field, DescFieldValue::Numeric(value))?;
            }

            // Spec: "The SQL_DESC_DATA_PTR field of the IPD can be set to force
            // a consistency check ... the value that the SQL_DESC_DATA_PTR
            // field of the IPD is set to is not actually stored and cannot be
            // retrieved by a call to SQLGetDescField or SQLGetDescRec; the
            // setting is made only to force the consistency check."
            // `set_record_field` already discards it for the IPD; the check
            // below is the part that is not discarded.
            //
            // Once, at the end. `SQLSetDescField` runs it when
            // `SQL_DESC_DATA_PTR` is set because that is the field the spec
            // names there; here the whole record arrives at once, so there is
            // one consistent moment to check and this is it.
            consistency_check(record, role)?;
            Ok(SqlReturn::SUCCESS)
        })
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
        MockBackend, MockLongDataBackend, MockRecordingBackend, MockTypeInfoBackend,
        alloc_env_conn_stmt, cleanup_env_conn_stmt, with_descriptor, with_handle,
    };
    use crate::types::sql_state;
    use odbc_sys::{CDataType, HandleType, ParamType, SqlDataType, StatementAttribute};

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

    /// The IPD's token, as the application receives it.
    unsafe fn ipd_of(stmt: *mut c_void) -> *mut c_void {
        let mut token: *mut c_void = std::ptr::null_mut();
        let ret = unsafe {
            sql_get_stmt_attr_w::<MockBackend>(
                stmt,
                StatementAttribute::ImpParamDesc as i32,
                std::ptr::from_mut(&mut token).cast::<c_void>(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SqlReturn::SUCCESS, "SQLGetStmtAttrW(IMP_PARAM_DESC)");
        assert!(!token.is_null(), "the IPD token must not be null");
        token
    }

    /// Put `count` records into the ARD without going through
    /// `SQLSetDescFieldW`, which is still unimplemented at this point. Task 7's
    /// tests drive the real round trip.
    unsafe fn seed_ard_records(stmt: *mut c_void, count: u16, concise_type: i16) {
        with_descriptor::<MockBackend, _>(stmt, DescriptorRole::Ard, |ard| {
            for column in 1..=count {
                let mut record = crate::descriptor::DescriptorRecord::default();
                record.set_concise_type(concise_type);
                ard.records.insert(column, record);
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

    /// The spec's `HY016` row: a write to the IRD is refused unless the field
    /// is `SQL_DESC_ARRAY_STATUS_PTR` or `SQL_DESC_ROWS_PROCESSED_PTR`, which
    /// it names as the exceptions.
    #[test]
    fn writing_the_ird_reports_hy016() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ird = ird_of(stmt);

            let ret = sql_set_desc_field_w::<MockBackend>(
                ird,
                1,
                Desc::ConciseType as i16,
                std::ptr::without_provenance_mut(CDataType::SLong as usize),
                0,
            );

            assert_eq!(ret, SqlReturn::ERROR);
            assert_eq!(first_sqlstate(ird), sql_state::CANNOT_MODIFY_IRD);

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// The two the `HY016` row exempts by name.
    #[test]
    fn the_two_exempt_ird_header_fields_are_writable() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ird = ird_of(stmt);

            for field in [Desc::ArrayStatusPtr, Desc::RowsProcessedPtr] {
                let ret = sql_set_desc_field_w::<MockBackend>(
                    ird,
                    0,
                    field as i16,
                    std::ptr::null_mut(),
                    0,
                );
                assert_eq!(ret, SqlReturn::SUCCESS, "{field:?} is exempt from HY016");
            }

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// `SQL_DESC_ARRAY_SIZE` is the ARD header field `SQL_ATTR_ROW_ARRAY_SIZE`
    /// aliases. Core substitutes any value but 1 with `01S02` through
    /// `SQLSetStmtAttr`, and must do the same here — the two are one value, and
    /// a door that accepts what the other refuses is the disagreement this
    /// milestone exists to remove.
    #[test]
    fn an_oversized_array_size_is_substituted_through_the_descriptor_too() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ard = ard_of(stmt);

            let ret = sql_set_desc_field_w::<MockBackend>(
                ard,
                0,
                Desc::ArraySize as i16,
                std::ptr::without_provenance_mut(10usize),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS_WITH_INFO);
            assert_eq!(first_sqlstate(ard), sql_state::OPTION_VALUE_CHANGED);

            // And both views report the substituted value.
            let mut via_desc: isize = 0;
            let ret = sql_get_desc_field_w::<MockBackend>(
                ard,
                0,
                Desc::ArraySize as i16,
                std::ptr::from_mut(&mut via_desc).cast::<c_void>(),
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(via_desc, 1);

            let mut via_attr: usize = 0;
            let ret = crate::ffi::stmt_attr::sql_get_stmt_attr_w::<MockBackend>(
                stmt,
                StatementAttribute::RowArraySize as i32,
                std::ptr::from_mut(&mut via_attr).cast::<c_void>(),
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(via_attr, 1, "the two views of one value disagree");

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// Setting `SQL_DESC_COUNT` lower discards the records above it. Leaving
    /// them would make the count and the map disagree, which is what deriving
    /// the count from the map exists to prevent.
    #[test]
    fn lowering_desc_count_discards_the_higher_records() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ard = ard_of(stmt);

            for column in 1..=3i16 {
                let ret = sql_set_desc_field_w::<MockBackend>(
                    ard,
                    column,
                    Desc::ConciseType as i16,
                    std::ptr::without_provenance_mut(CDataType::SLong as usize),
                    0,
                );
                assert_eq!(ret, SqlReturn::SUCCESS);
            }

            let ret = sql_set_desc_field_w::<MockBackend>(
                ard,
                0,
                Desc::Count as i16,
                std::ptr::without_provenance_mut(1usize),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            with_descriptor::<MockBackend, _>(stmt, DescriptorRole::Ard, |ard| {
                assert!(ard.records.contains_key(&1));
                assert!(!ard.records.contains_key(&2));
                assert!(!ard.records.contains_key(&3));
            });

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// The payoff of the whole milestone: a binding built entirely through the
    /// descriptor *is* a binding. If this passes, `SQLBindCol` and
    /// `SQLSetDescField` really are two doors onto one state rather than two
    /// states that happen to agree.
    ///
    /// Column 2 of `MockLongDataBackend`'s row is the fixed-width 4242, so the
    /// value itself is asserted and not merely the indicator — an indicator
    /// alone would pass for a column that was bound but never written into.
    #[test]
    fn a_binding_built_through_the_descriptor_receives_fetched_data() {
        unsafe {
            let (env, conn, stmt) =
                crate::test_utils::alloc_connected_env_conn_stmt::<MockLongDataBackend>();

            let mut ard: *mut c_void = std::ptr::null_mut();
            let ret = sql_get_stmt_attr_w::<MockLongDataBackend>(
                stmt,
                StatementAttribute::AppRowDesc as i32,
                std::ptr::from_mut(&mut ard).cast::<c_void>(),
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let mut buf: i32 = 0;
            let mut indicator: isize = 0;

            for (field, value) in [
                (Desc::ConciseType, CDataType::SLong as usize),
                (Desc::OctetLength, std::mem::size_of::<i32>()),
            ] {
                let ret = sql_set_desc_field_w::<MockLongDataBackend>(
                    ard,
                    2,
                    field as i16,
                    std::ptr::without_provenance_mut(value),
                    0,
                );
                assert_eq!(ret, SqlReturn::SUCCESS, "{field:?}");
            }
            let ret = sql_set_desc_field_w::<MockLongDataBackend>(
                ard,
                2,
                Desc::IndicatorPtr as i16,
                std::ptr::from_mut(&mut indicator).cast::<c_void>(),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            // DATA_PTR last: it is what runs the consistency check.
            let ret = sql_set_desc_field_w::<MockLongDataBackend>(
                ard,
                2,
                Desc::DataPtr as i16,
                std::ptr::from_mut(&mut buf).cast::<c_void>(),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let sql: Vec<u16> = "SELECT a, b, c FROM t".encode_utf16().collect();
            let ret = crate::ffi::execute::sql_exec_direct_w::<MockLongDataBackend>(
                stmt,
                sql.as_ptr(),
                i32::try_from(sql.len()).expect("short"),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            let ret = crate::ffi::fetch::sql_fetch::<MockLongDataBackend>(stmt);
            assert_eq!(ret, SqlReturn::SUCCESS);

            assert_eq!(
                buf, 4242,
                "SQLFetch did not write through a descriptor-built binding"
            );
            assert_eq!(indicator, std::mem::size_of::<i32>() as isize);

            crate::test_utils::cleanup_connected_env_conn_stmt::<MockLongDataBackend>(
                env, conn, stmt,
            );
        }
    }

    /// One call sets eight fields, and the consistency check runs once at the
    /// end — `SQL_DESC_DATA_PTR` is among them.
    #[test]
    fn set_desc_rec_sets_the_record_in_one_call() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ard = ard_of(stmt);
            let mut buf: i64 = 0;

            let ret = sql_set_desc_rec::<MockBackend>(
                ard,
                1,
                CDataType::SBigInt as i16,
                0,
                std::mem::size_of::<i64>() as isize,
                0,
                0,
                std::ptr::from_mut(&mut buf).cast::<c_void>(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                let record = handle
                    .app_row_desc
                    .records
                    .get(&1)
                    .expect("SQLSetDescRec wrote no record");
                assert_eq!(record.concise_type, CDataType::SBigInt as i16);
                assert_eq!(record.octet_length, std::mem::size_of::<i64>() as isize);
                assert!(record.is_bound());
            });

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// "The DataPtr argument can be set to a null pointer ... If the handle in
    /// the DescriptorHandle argument is associated with an ARD, this unbinds
    /// the column."
    #[test]
    fn set_desc_rec_with_a_null_data_pointer_unbinds() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ard = ard_of(stmt);
            let mut buf: i64 = 0;

            let ret = sql_set_desc_rec::<MockBackend>(
                ard,
                1,
                CDataType::SBigInt as i16,
                0,
                std::mem::size_of::<i64>() as isize,
                0,
                0,
                std::ptr::from_mut(&mut buf).cast::<c_void>(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let ret = sql_set_desc_rec::<MockBackend>(
                ard,
                1,
                CDataType::SBigInt as i16,
                0,
                std::mem::size_of::<i64>() as isize,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                let record = handle
                    .app_row_desc
                    .records
                    .get(&1)
                    .expect("the record itself still exists");
                assert!(!record.is_bound(), "a null DataPtr did not unbind");
            });

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// `HY016`: "The DescriptorHandle argument was associated with an IRD."
    /// Unconditional here — unlike `SQLSetDescField`, this function has no
    /// exempt header fields, because it sets record fields only.
    #[test]
    fn set_desc_rec_on_the_ird_reports_hy016() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ird = ird_of(stmt);

            let ret = sql_set_desc_rec::<MockBackend>(
                ird,
                1,
                CDataType::SBigInt as i16,
                0,
                8,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );

            assert_eq!(ret, SqlReturn::ERROR);
            assert_eq!(first_sqlstate(ird), sql_state::CANNOT_MODIFY_IRD);

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// The consistency check runs here too, and this is the test that proves
    /// it: the three above all set consistent records, so removing the check
    /// entirely would leave every one of them green.
    ///
    /// `DECIMAL(5,9)` has more scale than precision, which is the clause
    /// "if the SQL_DESC_TYPE field indicates a numeric type, the
    /// SQL_DESC_PRECISION and SQL_DESC_SCALE fields are verified to be valid".
    #[test]
    fn set_desc_rec_runs_the_consistency_check() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ipd = ipd_of(stmt);

            let ret = sql_set_desc_rec::<MockBackend>(
                ipd,
                1,
                SqlDataType::DECIMAL.0,
                0,
                8,
                5, // precision
                9, // scale
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );

            assert_eq!(ret, SqlReturn::ERROR);
            assert_eq!(
                first_sqlstate(ipd),
                sql_state::INCONSISTENT_DESCRIPTOR_INFORMATION
            );

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// The seven fields the spec lists, read in one call. Each output pointer
    /// is independently optional: "an application can prevent the return of a
    /// field's setting by setting the argument that corresponds to the field to
    /// a null pointer."
    #[test]
    fn get_desc_rec_reads_the_seven_record_fields() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ipd = ipd_of(stmt);

            let mut val: i32 = 0;
            let ret = crate::ffi::params::sql_bind_parameter::<MockBackend>(
                stmt,
                1,
                ParamType::Input as i16,
                CDataType::SLong as i16,
                SqlDataType::DECIMAL.0,
                9,
                2,
                std::ptr::from_mut(&mut val).cast::<c_void>(),
                4,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let mut name = [0u16; 32];
            let mut name_len: i16 = -1;
            let mut type_out: i16 = -1;
            let mut sub_type: i16 = -1;
            let mut length: isize = -1;
            let mut precision: i16 = -1;
            let mut scale: i16 = -1;
            let mut nullable: i16 = -1;
            let ret = sql_get_desc_rec_w::<MockBackend>(
                ipd,
                1,
                name.as_mut_ptr(),
                (name.len() * 2) as i16,
                &mut name_len,
                &mut type_out,
                &mut sub_type,
                &mut length,
                &mut precision,
                &mut scale,
                &mut nullable,
            );

            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(type_out, SqlDataType::DECIMAL.0, "SQL_DESC_TYPE");
            assert_eq!(
                sub_type, 0,
                "SQL_DESC_DATETIME_INTERVAL_CODE: not a datetime"
            );
            assert_eq!(precision, 9, "SQL_DESC_PRECISION, from ColumnSize");
            assert_eq!(scale, 2, "SQL_DESC_SCALE, from DecimalDigits");
            assert_eq!(
                length, 0,
                "SQL_DESC_OCTET_LENGTH: SQLBindParameter's BufferLength is the APD's, not the IPD's"
            );
            assert_eq!(
                nullable,
                crate::types::Nullable::SqlNullableUnknown as i16,
                "SQL_DESC_NULLABLE is \"R, ND\" on an IPD core does not auto-populate"
            );
            assert_eq!(name_len, 0, "an unnamed parameter");

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// Every output pointer is optional on its own: "an application can prevent
    /// the return of a field's setting by setting the argument that corresponds
    /// to the field to a null pointer." A group null-check would either write
    /// through one of these or refuse to write any.
    #[test]
    fn get_desc_rec_writes_only_the_outputs_the_application_asked_for() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ipd = ipd_of(stmt);

            let mut val: i32 = 0;
            let ret = crate::ffi::params::sql_bind_parameter::<MockBackend>(
                stmt,
                1,
                ParamType::Input as i16,
                CDataType::SLong as i16,
                SqlDataType::DECIMAL.0,
                9,
                2,
                std::ptr::from_mut(&mut val).cast::<c_void>(),
                4,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            // Only `scale` is asked for; every other output stays untouched.
            const SENTINEL: i16 = -99;
            let mut scale: i16 = SENTINEL;
            let mut untouched: i16 = SENTINEL;
            let ret = sql_get_desc_rec_w::<MockBackend>(
                ipd,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut scale,
                std::ptr::null_mut(),
            );

            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(scale, 2);
            assert_eq!(untouched, SENTINEL, "a null output pointer was written to");
            // Read it back so the compiler cannot fold the local away.
            untouched = untouched.wrapping_add(0);
            assert_eq!(untouched, SENTINEL);

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// `SQL_NO_DATA` past the count, per the Returns section.
    #[test]
    fn get_desc_rec_past_the_count_returns_no_data() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ard = ard_of(stmt);

            let ret = sql_get_desc_rec_w::<MockBackend>(
                ard,
                4,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );

            assert_eq!(ret, SqlReturn::NO_DATA);

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

            // `SQL_DESC_PARAMETER_TYPE` is the IPD's alone, so this is `HY091`
            // on the APD. Any failing call would do; what the test watches is
            // *where* the record lands.
            let ret = sql_set_desc_field_w::<MockBackend>(
                apd,
                1,
                Desc::ParameterType as i16,
                std::ptr::without_provenance_mut(ParamType::Input as usize),
                0,
            );
            assert_eq!(ret, SqlReturn::ERROR);

            assert_eq!(
                first_sqlstate(apd),
                sql_state::INVALID_DESCRIPTOR_FIELD_IDENTIFIER
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
