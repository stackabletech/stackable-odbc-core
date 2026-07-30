//! `SQLGetDescFieldW`, `SQLSetDescFieldW`, `SQLGetDescRecW` and `SQLSetDescRec`
//! — the four accessors over a statement's own descriptors.
//!
//! # How a call reaches a field
//!
//! Every one of the four takes a descriptor handle and nothing that says which
//! of the four descriptors it is. The descriptor itself answers that: each is its
//! own registered allocation carrying its own `role`, so a token names exactly
//! one `Descriptor` and that struct says which role it is. `resolve_target`
//! pairs it with the statement it was allocated with — through
//! `HandleScope::stmt_with_desc`, which is sound for the same reason
//! `stmt_with_parent` is — or with no statement at all, for a descriptor the
//! application allocated against a connection.
//!
//! From that pair the caller reaches whichever of three sources owns the field:
//!
//! - the **record map** on the descriptor, for the ARD, APD and IPD;
//! - the **header storage**, which is the descriptor's own `attrs` for the six
//!   ARD/APD header fields `SQLGetStmtAttr` also reads, and the statement's
//!   attribute bag for the four IRD/IPD pairs that deliberately stay there;
//! - the **IRD's computed view** over the current result set's
//!   `ColumnDescriptor`s, which stores nothing and delegates to the same
//!   `get_column_attribute` that implements `SQLColAttributeW`.
//!
//! `descriptor::field_access` decides `HY091` for all of them, from the spec's
//! "Initialization of Descriptor Fields" tables.
//!
//! # What is still missing
//!
//! `SQLCopyDesc` and `SQLAllocHandle(SQL_HANDLE_DESC)`. Until they land,
//! **`SQL_OIC_CORE` is not satisfied**: Core-level conformance requires working
//! descriptors, and these four make a statement's own descriptors real without
//! making an application's own possible.

use std::ffi::c_void;

use odbc_sys::Desc;

use crate::backend::{Backend, StatementBackend};
use crate::descriptor::{
    DescFieldValue, DescriptorRole, FieldAccess, consistency_check, field_access, get_record_field,
    header_attribute, header_default, set_record_field,
};
use crate::errors::OdbcError;
use crate::handles::registry::HandleKind;
use crate::handles::scope::HandleScope;
use crate::handles::{Descriptor, HeaderOwner, StatementHandle};
use crate::panic::panic_safe;
use crate::types::col_attr::{ColAttrValue, get_column_attribute};
use crate::types::{SqlReturn, SqlState};

/// What one of the four accessors is addressing: a descriptor, and the statement
/// it was allocated with if it has one.
///
/// Both borrows at once, which [`HandleScope::stmt_with_desc`] makes sound now
/// that a descriptor is its own allocation. `stmt` is what the IRD's computed
/// view and the four IRD/IPD header pairs need; everything else is on `desc`.
///
/// `stmt` is `None` for a descriptor the application allocated against a
/// connection. That is never an IRD — only application descriptors can be
/// explicit — so the IRD paths treat `None` as an error rather than substituting
/// an answer.
struct DescTarget<'a, B: Backend> {
    stmt: Option<&'a mut StatementHandle<B>>,
    desc: &'a mut Descriptor,
    role: DescriptorRole,
}

/// Resolve a descriptor handle to what the accessors operate on.
///
/// The role comes from the descriptor itself, which is why this needs no
/// comparison against a statement's fields: a token names exactly one
/// `Descriptor`, and that struct says which of the roles it is.
fn resolve_target<'s, B: Backend>(
    scope: &'s mut HandleScope<'_>,
    token: *mut c_void,
) -> Result<DescTarget<'s, B>, OdbcError> {
    let role = scope.descriptor(token)?.role;
    match scope.descriptor_stmt(token) {
        Some(stmt_token) => {
            let (stmt, desc) = scope.stmt_with_desc::<B>(stmt_token, token)?;
            Ok(DescTarget {
                stmt: Some(stmt),
                desc,
                role,
            })
        }
        None => Ok(DescTarget {
            stmt: None,
            desc: scope.descriptor(token)?,
            role,
        }),
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
///   an APD, and for a record field at `record_number` 0 on an IPD. Read the row's `(DM)`
///   markers clause by clause: they precede only the bookmark clause, so both of these are
///   the driver's. The IPD clause is not the bookmark record — bookmarks are an ARD and IRD
///   concept, and record 0 on an IPD addresses no parameter
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
    // SAFETY: `descriptor_handle` is null or a token, which `resolve_target`
    // validates without dereferencing. `value_ptr` and `string_length_ptr` are
    // null-checked before every write, and written unaligned because row-wise
    // binding may place either at an arbitrary offset.
    let ret = unsafe {
        panic_safe::<B, _>(descriptor_handle, |scope| {
            let mut target = resolve_target::<B>(scope, descriptor_handle)?;
            let role = target.role;
            // Spec: clear diagnostics at the start of each ODBC call — before
            // the field parse, so an unrecognised identifier reports `HY091`
            // onto an empty queue rather than behind the previous call's
            // records.
            target.desc.diagnostics.clear();

            let field = field_from_raw(field_identifier)?;
            tracing::debug!(
                "SQLGetDescFieldW(desc={:?}, role={:?}, rec={}, field={:?})",
                descriptor_handle,
                role,
                record_number,
                field,
            );

            let Some(value) = read_desc_field(&mut target, record_number, field)? else {
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
                        &mut target.desc.diagnostics,
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
    target: &mut DescTarget<'_, B>,
    record_number: i16,
    field: Desc,
) -> Result<Option<DescFieldValue>, OdbcError> {
    let role = target.role;
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
    if let Some(value) = read_header_field(target, field) {
        return Ok(Some(DescFieldValue::Numeric(value)));
    }

    if role == DescriptorRole::Ird {
        return read_ird_field(target, record_number, field).map(Some);
    }

    // Spec 07009: "The FieldIdentifier argument was a record field, the
    // RecNumber argument was 0, and the DescriptorHandle argument was an IPD
    // handle." Unmarked, so it is the driver's. Placed *after* the header and
    // IRD routing because the clause says "a record field" and a header field
    // ignores `RecNumber` altogether.
    //
    // Not the bookmark record, which is an ARD and IRD concept: an IPD has no
    // bookmark, so record 0 there addresses no parameter.
    if record_number == 0 && role == DescriptorRole::Ipd {
        return Err(OdbcError::general(
            "Record number 0 does not address a parameter on an IPD",
            SqlState::invalid_descriptor_index(),
        ));
    }

    let record_number = u16::try_from(record_number).unwrap_or(0);
    // Spec, Returns: `SQL_NO_DATA` when `RecNumber` is greater than
    // `SQL_DESC_COUNT`. Derived from the map, as everywhere.
    let Some(record) = target.desc.records.get(&record_number) else {
        return Ok(None);
    };
    get_record_field(record, role, field).map(Some)
}

/// The header half of [`read_desc_field`], or `None` if `field` is not a header
/// field.
fn read_header_field<B: Backend>(target: &mut DescTarget<'_, B>, field: Desc) -> Option<isize> {
    let role = target.role;
    match field {
        // Follows the allocation: `SQL_DESC_ALLOC_USER` for a descriptor the
        // application allocated, `SQL_DESC_ALLOC_AUTO` for the four a statement
        // owns. Read-only on every role, and the one field `SQLCopyDesc` never
        // copies.
        Desc::AllocType => Some(target.desc.alloc_type.as_sql()),
        // Derived rather than stored, so it cannot disagree with the map. The
        // IRD's records are the result set's columns, which it does not store.
        Desc::Count => Some(match role {
            DescriptorRole::Ird => target
                .stmt
                .as_ref()
                .and_then(|s| s.statement.as_ref())
                .map_or(0, |s| s.column_count()) as isize,
            _ => target.desc.records.len() as isize,
        }),
        _ => {
            // `header_attribute` answers "is this field stored as a statement
            // attribute on this role", and [`HeaderOwner::of`] then says which of
            // the two storages holds it: the descriptor's own header for the
            // ARD/APD fields, and the statement's attribute bag for the four
            // IRD/IPD pairs that deliberately stay there.
            let attr = header_attribute(role, field)?;
            let stored = match HeaderOwner::of(Some(attr)) {
                Some((_, key)) => target.desc.attrs.get(&(key as u16)).copied(),
                None => target
                    .stmt
                    .as_ref()
                    .and_then(|s| s.plain_attr_get(attr as i32)),
            };
            Some(stored.unwrap_or_else(|| header_default(field)) as isize)
        }
    }
}

/// The IRD half of [`read_desc_field`]: a computed view, never stored state.
fn read_ird_field<B: Backend>(
    target: &mut DescTarget<'_, B>,
    record_number: i16,
    field: Desc,
) -> Result<DescFieldValue, OdbcError> {
    // An IRD is always one of a statement's four, so a missing statement here is
    // unreachable rather than a case to answer for. Erroring says so instead of
    // inventing column metadata that belongs to nothing.
    let Some(stmt) = target.stmt.as_deref_mut() else {
        return Err(OdbcError::general(
            "An implementation row descriptor has no owning statement",
            SqlState::general_error(),
        ));
    };
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
///   an APD, and for a record field at `record_number` 0 on an IPD. The `(DM)` markers on
///   this row precede the `SQL_DESC_COUNT` and implicitly-allocated-APD clauses only; read
///   them clause by clause
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
    // SAFETY: `descriptor_handle` is null or a token, which `resolve_target`
    // validates without dereferencing. `value_ptr` is read through only for a
    // character field, where the caller guarantees `buffer_length` readable
    // bytes.
    let ret = unsafe {
        panic_safe::<B, _>(descriptor_handle, |scope| {
            let mut target = resolve_target::<B>(scope, descriptor_handle)?;
            let role = target.role;
            // Spec: clear diagnostics at the start of each ODBC call — before
            // the field parse, so an unrecognised identifier reports `HY091`
            // onto an empty queue rather than behind the previous call's
            // records.
            target.desc.diagnostics.clear();

            let field = field_from_raw(field_identifier)?;
            tracing::debug!(
                "SQLSetDescFieldW(desc={:?}, role={:?}, rec={}, field={:?})",
                descriptor_handle,
                role,
                record_number,
                field,
            );

            write_desc_field(&mut target, record_number, field, value_ptr, buffer_length)
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
    target: &mut DescTarget<'_, B>,
    record_number: i16,
    field: Desc,
    value_ptr: *mut c_void,
    buffer_length: i32,
) -> Result<SqlReturn, OdbcError> {
    let role = target.role;
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
        return Ok(truncate_records(target, value_ptr as usize));
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
        return Ok(write_header_field(target, field, attr, value_ptr as usize));
    }

    // Spec 07009, as in the read direction: a record field, `RecNumber` of 0,
    // and an IPD. After the header routing for the same reason.
    if record_number == 0 && role == DescriptorRole::Ipd {
        return Err(OdbcError::general(
            "Record number 0 does not address a parameter on an IPD",
            SqlState::invalid_descriptor_index(),
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

    let record = target.desc.records.entry(record_number).or_default();
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
fn truncate_records<B: Backend>(target: &mut DescTarget<'_, B>, count: usize) -> SqlReturn {
    let count = u16::try_from(count).unwrap_or(u16::MAX);
    target.desc.records.retain(|&number, _| number <= count);
    SqlReturn::SUCCESS
}

/// A header field write, with the one value core cannot honour substituted.
///
/// `SQL_DESC_ARRAY_SIZE` is `SQL_ATTR_ROW_ARRAY_SIZE` on the ARD and
/// `SQL_ATTR_PARAMSET_SIZE` on the APD, and `SQLSetStmtAttr` substitutes any
/// value but 1 with `01S02` because core rejects a multi-row rowset. The same
/// value reached through the descriptor gets the same answer, or the two views
/// disagree — which single storage exists to prevent. The warning lands on the
/// *descriptor's* queue, since that is the handle the application named.
fn write_header_field<B: Backend>(
    target: &mut DescTarget<'_, B>,
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
    // The same two storages [`read_header_field`] reads, so the pair cannot
    // disagree about where the value lives.
    match HeaderOwner::of(Some(attr)) {
        Some((_, key)) => {
            target.desc.attrs.insert(key as u16, stored);
        }
        None => {
            if let Some(stmt) = target.stmt.as_deref_mut() {
                stmt.plain_attr_set(attr as i32, stored);
            }
        }
    }
    if substituted {
        return crate::ffi::stmt_attr::substitution_warning(
            &mut target.desc.diagnostics,
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

/// The value [`sql_get_desc_rec_w`] reports for a field this descriptor's role
/// leaves undefined.
///
/// The spec permits leaving the application's buffer as it was — the value "is
/// undefined". A defined value is written instead: an output the application
/// can read is strictly more useful than one it cannot, and it is the half of
/// the behaviour a test can assert on.
fn undefined_placeholder(field: Desc) -> DescFieldValue {
    match field {
        // The one character field of the seven. An empty name writes a
        // terminator and a length of zero rather than leaving the buffer as it
        // was found.
        Desc::Name => DescFieldValue::String(String::new()),
        // "Nullability is unknown" — the same answer `get_record_field` gives
        // for the IPD's `SQL_DESC_NULLABLE`, which core never auto-populates.
        Desc::Nullable => {
            DescFieldValue::Numeric(crate::types::Nullable::SqlNullableUnknown as isize)
        }
        _ => DescFieldValue::Numeric(0),
    }
}

/// [`read_desc_field`] as `SQLGetDescRec` reads it.
///
/// The one difference is the [`FieldAccess::Undefined`] cell, and it is a
/// difference between two spec pages rather than a correction to one.
/// `SQLGetDescField` lists `HY091` in its diagnostics table and answers with it
/// for a field its role does not define. `SQLGetDescRec`'s table has **no
/// `HY091` row at all**, and its Comments section says what happens instead,
/// naming this exact case: "calling SQLGetDescRec for the SQL_DESC_NAME or
/// SQL_DESC_NULLABLE field of an APD or ARD will return SQL_SUCCESS but an
/// undefined value for the field."
///
/// So the divergence lives here, at the one call site the spec exempts, and
/// [`read_desc_field`]'s contract is unchanged for every other caller.
///
/// `SQL_DESC_TYPE` is defined on all four roles, so the placeholder can never
/// stand in for the `None` that [`sql_get_desc_rec_w`] reads as `SQL_NO_DATA`.
fn read_desc_rec_field<B: Backend>(
    target: &mut DescTarget<'_, B>,
    record_number: i16,
    field: Desc,
) -> Result<Option<DescFieldValue>, OdbcError> {
    if field_access(target.role, field) == FieldAccess::Undefined {
        return Ok(Some(undefined_placeholder(field)));
    }
    read_desc_field(target, record_number, field)
}

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
/// The missing `HY091` row is load-bearing rather than incidental. Two of the
/// seven fields — `SQL_DESC_NAME` and `SQL_DESC_NULLABLE` — are undefined on an
/// ARD and an APD, and the Comments section states what this function does
/// about it: "When an application calls SQLGetDescRec to retrieve the value of
/// a field that is undefined for a particular descriptor type, the function
/// returns SQL_SUCCESS but the value returned for the field is undefined."
/// `SQLGetDescField` on the same field still answers `HY091`, because that is a
/// different function whose table lists it. `read_desc_rec_field` is where the
/// two part company.
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
    // SAFETY: `descriptor_handle` is null or a token, which `resolve_target`
    // validates without dereferencing. Every output pointer is null-checked
    // before its write, and written unaligned because row-wise binding may
    // place any of them at an arbitrary offset.
    let ret = unsafe {
        panic_safe::<B, _>(descriptor_handle, |scope| {
            let mut target = resolve_target::<B>(scope, descriptor_handle)?;
            // Spec: clear diagnostics at the start of each ODBC call.
            target.desc.diagnostics.clear();

            // The six numeric fields, each read through the same
            // `get_record_field` `SQLGetDescField` uses — one mapping, two
            // callers, so the two functions cannot report a record differently.
            // `SQL_NO_DATA` from any one of them is `SQL_NO_DATA` for the call:
            // they all address the same record.
            let mut numeric = |field: Desc| -> Result<Option<isize>, OdbcError> {
                Ok(
                    match read_desc_rec_field(&mut target, record_number, field)? {
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
                read_desc_rec_field(&mut target, record_number, Desc::Name)?
            else {
                return Ok(SqlReturn::SUCCESS);
            };
            let mut units: i16 = 0;
            let ret = crate::utf16::note_truncation(
                crate::utf16::write_utf16(&field_name, name, buffer_length / 2, &mut units),
                &mut target.desc.diagnostics,
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
/// - `07009` Invalid descriptor index — returned for a negative `record_number` on **any**
///   role, and for `record_number` 0 on an IPD. No clause of this row is annotated `(DM)`,
///   and the negative clause names no descriptor role — so it is reported ahead of the
///   `HY016` an IRD would otherwise get
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
    // SAFETY: `descriptor_handle` is null or a token, which `resolve_target`
    // validates without dereferencing. The three pointer arguments are stored
    // as values and never read through.
    let ret = unsafe {
        panic_safe::<B, _>(descriptor_handle, |scope| {
            let target = resolve_target::<B>(scope, descriptor_handle)?;
            let role = target.role;
            // Spec: clear diagnostics at the start of each ODBC call.
            target.desc.diagnostics.clear();

            // Spec 07009, both clauses, neither annotated `(DM)`: "The
            // RecNumber argument was set to 0, and the DescriptorHandle
            // referred to an IPD handle. The RecNumber argument was less than
            // 0."
            //
            // The negative clause names no descriptor role, unlike
            // `SQLGetDescField`'s, so it is checked for all four — including an
            // IRD, ahead of that role's `HY016`. The argument is wrong whichever
            // descriptor it names, and "this index does not exist" is the more
            // specific of the two answers.
            //
            // The record-0 clause is not the bookmark record: bookmarks are an
            // ARD and IRD concept, and an IPD has none, so record 0 there is an
            // index no parameter number can address.
            if record_number < 0 {
                return Err(OdbcError::general(
                    format!("Record number {record_number} is negative"),
                    SqlState::invalid_descriptor_index(),
                ));
            }
            if record_number == 0 && role == DescriptorRole::Ipd {
                return Err(OdbcError::general(
                    "Record number 0 does not address a parameter on an IPD",
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
            let record = target.desc.records.entry(record_key).or_default();

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

/// Generic implementation of SQLCopyDesc.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlcopydesc-function>
///
/// Copies one descriptor's contents onto another, in two phases that never hold
/// two group locks at once. The spec permits a copy across connections and even
/// across environments — "Descriptor handles can be copied across connections
/// even if the connections are under different environments" — so source and
/// target may be in two different lock groups, and a call holding both would be a
/// second nested-lock site where the crate has exactly one (environment before
/// connection). Instead:
///
/// 1. **Source group alone.** Materialise an owned `DescriptorSnapshot`. An IRD
///    is materialised here, so `HY007` is decided here; the lock is released
///    before phase 2 begins.
/// 2. **Target group alone**, under an ordinary `panic_safe`, where every
///    diagnostic this call posts belongs.
///
/// Two phases cannot deadlock in either direction by construction, which is the
/// whole reason for the shape.
///
/// `SQL_DESC_ALLOC_TYPE` is never copied: the spec exempts it by name, and it
/// belongs to the allocation rather than to its contents.
///
/// The source's diagnostic queue is deliberately **not** cleared. The spec's
/// sentence about that describes the Driver Manager's cross-driver implementation
/// via `SQLGetDescField`/`SQLSetDescField`, not an obligation on a driver that
/// copies directly.
///
/// # Parameters
///
/// - `source_desc_handle`: the descriptor to copy from. May be any role,
///   including an IRD.
/// - `target_desc_handle`: the descriptor to copy onto. May not be an IRD.
///
/// # Spec compliance
///
/// Every SQLSTATE from this function's Diagnostics table:
///
/// - `01000` General warning — not produced here.
/// - `08S01` Communication link failure — not returned; this call performs no
///   I/O.
/// - `HY000` General error — returned for a failure with no more specific code,
///   including an internal panic caught by `panic_safe`.
/// - `HY001` Memory allocation error — not returned; allocation here is an
///   infallible `Box`/`HashMap` clone.
/// - `HY007` Associated statement is not prepared — returned when the source is
///   an IRD whose statement has produced no column metadata. Determined in phase
///   one and posted in phase two, against the target, which is the handle the
///   spec says to read this call's diagnostics with.
/// - `HY010` Function sequence error — **(DM)** on all four of its clauses; not
///   returned here.
/// - `HY013` Memory management error — not returned.
/// - `HY016` Cannot modify an implementation row descriptor — returned when
///   `target_desc_handle` names an IRD: "TargetDescHandle cannot be set to a
///   handle to an IRD."
/// - `HY021` Inconsistent descriptor information — returned when a copied record
///   fails the consistency check under the target's role. Core checks the
///   *snapshot* before writing anything, so the target is left untouched; the
///   spec permits it to be undefined, and an owned snapshot makes doing better
///   free.
/// - `HY092` Invalid attribute/option identifier — not returned. Core copies
///   wholesale rather than field by field, as the spec's own "All fields of the
///   descriptor, except SQL_DESC_ALLOC_TYPE ..., are copied, whether or not the
///   field is defined for the destination descriptor" describes; this row
///   describes the Driver Manager's field-by-field implementation.
/// - `HY117` Connection is suspended — **(DM)**; not returned here.
/// - `HYT01` Connection timeout expired — not returned; no I/O.
/// - `IM001` Driver does not support this function — **(DM)**; not returned here.
///
/// An invalid `source_desc_handle` is `SQL_INVALID_HANDLE` with **no** diagnostic
/// record, per the page's own sentence: "If an invalid SourceDescHandle was
/// passed in the call, SQL_INVALID_HANDLE will be returned but no SQLSTATE will be
/// returned." No queue is in reach at that point, which is exactly right.
///
/// # Safety
///
/// Both arguments must be null or tokens issued by `SQLAllocHandle` or by the
/// implicit allocation a statement performs.
pub unsafe fn sql_copy_desc<B: Backend>(
    source_desc_handle: *mut c_void,
    target_desc_handle: *mut c_void,
) -> SqlReturn {
    tracing::debug!(
        "SQLCopyDesc(source={:?}, target={:?})",
        source_desc_handle,
        target_desc_handle
    );
    // Phase one: the source's group, alone. Released before phase two takes the
    // target's, so this call never holds two peer groups — the crate's one
    // ordering rule (environment before connection) stays the only one, and a
    // copy in the opposite direction on another thread cannot deadlock.
    //
    // `HandleScope::with_group` is the acquisition, and its return type carries no
    // guard — so that the lock is released before phase two is a fact its
    // signature states rather than something this function has to remember.
    let Some(snapshot) = HandleScope::with_group(source_desc_handle, HandleKind::Desc, |scope| {
        scope.snapshot_descriptor::<B>(source_desc_handle)
    }) else {
        tracing::debug!("SQLCopyDesc -> INVALID_HANDLE (source)");
        return SqlReturn::INVALID_HANDLE;
    };

    // Phase two: the target's group. Every diagnostic this call posts goes to the
    // target's queue, per the spec's own instruction to read them with a `Handle`
    // of `TargetDescHandle` — including `HY007`, which phase one decided.
    let ret = unsafe {
        panic_safe::<B, _>(target_desc_handle, |scope| {
            let mut target = resolve_target::<B>(scope, target_desc_handle)?;
            target.desc.diagnostics.clear();
            let target_role = target.role;

            // Spec HY016: "TargetDescHandle cannot be set to a handle to an IRD."
            // Before the snapshot is unwrapped, so an IRD target is reported as
            // such even when the source also failed.
            if target_role == DescriptorRole::Ird {
                return Err(OdbcError::general(
                    "SQLCopyDesc: the target is an implementation row descriptor",
                    SqlState::cannot_modify_ird(),
                ));
            }

            let mut snapshot = snapshot?;

            // Spec: "When the SQL_DESC_DATA_PTR field is copied, a consistency
            // check is performed on the target descriptor." Run against the
            // snapshot, before anything is written, so HY021 leaves the target
            // untouched.
            for record in snapshot.records.values() {
                if record.is_bound() {
                    consistency_check(record, target_role)?;
                }
            }

            // The header fields this role keeps on its owning statement rather
            // than in its own header — `SQL_ATTR_PARAM_STATUS_PTR` and
            // `SQL_ATTR_PARAMS_PROCESSED_PTR` for an IPD target. Written where
            // this role *reads* them, or the copy would land in a map nothing
            // consults and the spec's "all fields ... are copied" would hold
            // only for the roles whose header lives on the descriptor.
            for field in [Desc::ArrayStatusPtr, Desc::RowsProcessedPtr] {
                let Some(attr) = header_attribute(target_role, field) else {
                    continue;
                };
                // `Some` here means the field is on the descriptor's own header
                // after all — the ARD and APD pairs — which the wholesale
                // assignment below already carries.
                if HeaderOwner::of(Some(attr)).is_some() {
                    continue;
                }
                let Some(value) = snapshot.attrs.remove(&(field as u16)) else {
                    continue;
                };
                let Some(stmt) = target.stmt.as_deref_mut() else {
                    continue;
                };
                stmt.plain_attr_set(attr as i32, value);
            }

            // Spec: "All fields of the descriptor, except SQL_DESC_ALLOC_TYPE
            // ..., are copied, whether or not the field is defined for the
            // destination descriptor." So this is a wholesale replace rather than
            // a field-by-field validated set — which is also why HY091 cannot
            // arise here and HY092 is not core's.
            target.desc.records = snapshot.records;
            target.desc.attrs = snapshot.attrs;
            tracing::debug!(
                "SQLCopyDesc: copied {} record(s) onto the {:?}",
                target.desc.records.len(),
                target_role
            );
            Ok(SqlReturn::SUCCESS)
        })
    };
    tracing::debug!("SQLCopyDesc -> {:?}", ret);
    ret
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffi::diag::sql_get_diag_rec_w;
    use crate::ffi::stmt_attr::sql_get_stmt_attr_w;
    use crate::test_utils::{
        MockBackend, MockLongDataBackend, MockRecordingBackend, MockTypeInfoBackend,
        alloc_env_conn_stmt, cleanup_env_conn_stmt, with_descriptor,
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
    /// `SQLSetDescFieldW`, so that a test of the *read* path does not depend on
    /// the write path being right. The round-trip tests below drive both.
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

    /// Spec 07009, `SQLSetDescRec`: "The RecNumber argument was set to 0, and
    /// the DescriptorHandle referred to an IPD handle." No clause of that row
    /// is annotated `(DM)`. Without the check, record 0 is created like any
    /// other and the parameter set silently gains a record no parameter number
    /// can address.
    #[test]
    fn set_desc_rec_at_record_zero_on_an_ipd_reports_07009() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ipd = ipd_of(stmt);

            let ret = sql_set_desc_rec::<MockBackend>(
                ipd,
                0,
                SqlDataType::INTEGER.0,
                0,
                4,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );

            assert_eq!(ret, SqlReturn::ERROR);
            assert_eq!(first_sqlstate(ipd), sql_state::INVALID_DESCRIPTOR_INDEX);

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// The second clause of the same row: "The RecNumber argument was less than
    /// 0." It names no descriptor role, unlike `SQLGetDescField`'s, so it holds
    /// for an IPD too — where a negative number previously wrapped to record 0
    /// through `u16::try_from(...).unwrap_or(0)` and wrote a record the caller
    /// never asked for.
    #[test]
    fn set_desc_rec_with_a_negative_record_number_on_an_ipd_reports_07009() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ipd = ipd_of(stmt);

            let ret = sql_set_desc_rec::<MockBackend>(
                ipd,
                -1,
                SqlDataType::INTEGER.0,
                0,
                4,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );

            assert_eq!(ret, SqlReturn::ERROR);
            assert_eq!(first_sqlstate(ipd), sql_state::INVALID_DESCRIPTOR_INDEX);

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// The negative clause names no role, so it reaches an IRD as well, and it
    /// is reported ahead of `HY016`: the argument is wrong whichever descriptor
    /// it names, and "this index does not exist" is the more specific of the
    /// two answers.
    #[test]
    fn set_desc_rec_with_a_negative_record_number_on_an_ird_reports_07009() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ird = ird_of(stmt);

            let ret = sql_set_desc_rec::<MockBackend>(
                ird,
                -1,
                SqlDataType::INTEGER.0,
                0,
                4,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );

            assert_eq!(ret, SqlReturn::ERROR);
            assert_eq!(first_sqlstate(ird), sql_state::INVALID_DESCRIPTOR_INDEX);

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// Spec 07009, `SQLGetDescField` and `SQLSetDescField` alike: "The
    /// FieldIdentifier argument was a record field, the RecNumber argument was
    /// 0, and the DescriptorHandle argument was an IPD handle." Unmarked on
    /// both pages, so it is the driver's.
    #[test]
    fn a_record_field_at_record_zero_on_an_ipd_reports_07009_both_ways() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ipd = ipd_of(stmt);

            let mut value: isize = 0;
            let ret = sql_get_desc_field_w::<MockBackend>(
                ipd,
                0,
                Desc::ConciseType as i16,
                std::ptr::from_mut(&mut value).cast::<c_void>(),
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::ERROR, "the read direction");
            assert_eq!(first_sqlstate(ipd), sql_state::INVALID_DESCRIPTOR_INDEX);

            let ret = sql_set_desc_field_w::<MockBackend>(
                ipd,
                0,
                Desc::ConciseType as i16,
                std::ptr::without_provenance_mut(SqlDataType::INTEGER.0 as usize),
                0,
            );
            assert_eq!(ret, SqlReturn::ERROR, "the write direction");
            assert_eq!(first_sqlstate(ipd), sql_state::INVALID_DESCRIPTOR_INDEX);

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// The clause says "a **record** field", and a header field ignores
    /// `RecNumber` entirely — the page: `SQLGetDescField` "does not retrieve
    /// the values for header fields" by record. So record 0 must still answer
    /// `SQL_DESC_COUNT` on an IPD, and a check placed ahead of the header
    /// routing would break it.
    #[test]
    fn a_header_field_at_record_zero_on_an_ipd_still_answers() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ipd = ipd_of(stmt);

            let mut value: isize = -1;
            let ret = sql_get_desc_field_w::<MockBackend>(
                ipd,
                0,
                Desc::Count as i16,
                std::ptr::from_mut(&mut value).cast::<c_void>(),
                0,
                std::ptr::null_mut(),
            );

            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(value, 0, "no parameters are bound");

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
    /// a door that accepts what the other refuses is the disagreement single
    /// storage exists to prevent.
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

    /// The payoff of one storage: a binding built entirely through the
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

            with_descriptor::<MockBackend, _>(stmt, DescriptorRole::Ard, |ard| {
                let record = ard.records.get(&1).expect("SQLSetDescRec wrote no record");
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

            with_descriptor::<MockBackend, _>(stmt, DescriptorRole::Ard, |ard| {
                let record = ard.records.get(&1).expect("the record itself still exists");
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

    /// `SQLGetDescRec` on an ARD must succeed, even though `SQL_DESC_NULLABLE`
    /// and `SQL_DESC_NAME` are undefined there.
    ///
    /// Spec, Comments: "When an application calls SQLGetDescRec to retrieve the
    /// value of a field that is undefined for a particular descriptor type, the
    /// function returns SQL_SUCCESS but the value returned for the field is
    /// undefined. For example, calling SQLGetDescRec for the SQL_DESC_NAME or
    /// SQL_DESC_NULLABLE field of an APD or ARD will return SQL_SUCCESS but an
    /// undefined value for the field."
    ///
    /// Every other test of this function drives the IPD, which defines both.
    /// That is why an unconditional `HY091` here survived: the one descriptor
    /// role it cannot happen on was the only one under test.
    #[test]
    fn get_desc_rec_on_an_ard_reports_the_undefined_fields_without_failing() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ard = ard_of(stmt);

            let mut buf = [0u8; 8];
            assert_eq!(
                crate::ffi::bind::sql_bind_col::<MockBackend>(
                    stmt,
                    1,
                    CDataType::SBigInt as i16,
                    buf.as_mut_ptr().cast(),
                    8,
                    std::ptr::null_mut(),
                ),
                SqlReturn::SUCCESS
            );

            // A sentinel the call must overwrite: "undefined" in the spec's
            // sense would permit leaving these as they are, and core writes a
            // defined value instead so the application has something to read.
            let mut name = [0xFFFFu16; 32];
            let mut name_len: i16 = -1;
            let mut type_out: i16 = -1;
            let mut sub_type: i16 = -1;
            let mut length: isize = -1;
            let mut precision: i16 = -1;
            let mut scale: i16 = -1;
            let mut nullable: i16 = -1;
            let ret = sql_get_desc_rec_w::<MockBackend>(
                ard,
                1,
                name.as_mut_ptr(),
                i16::try_from(name.len() * 2).expect("short"),
                &mut name_len,
                &mut type_out,
                &mut sub_type,
                &mut length,
                &mut precision,
                &mut scale,
                &mut nullable,
            );

            assert_eq!(ret, SqlReturn::SUCCESS, "an ARD read must not fail");
            assert_eq!(type_out, CDataType::SBigInt as i16, "SQL_DESC_TYPE");
            assert_eq!(length, 8, "SQL_DESC_OCTET_LENGTH, from SQLBindCol");
            assert_eq!(
                nullable,
                crate::types::Nullable::SqlNullableUnknown as i16,
                "SQL_DESC_NULLABLE is undefined on an ARD, and must still be written"
            );
            assert_eq!(name_len, 0, "SQL_DESC_NAME is undefined on an ARD");
            assert_eq!(name[0], 0, "the name buffer must be left terminated");
            assert_eq!(sub_type, 0, "SQL_DESC_DATETIME_INTERVAL_CODE");
            assert_eq!(precision, 0, "SQL_DESC_PRECISION");
            assert_eq!(scale, 0, "SQL_DESC_SCALE");

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// The same for an APD, reached through `SQLBindParameter` rather than
    /// `SQLBindCol`. The two application descriptors share the undefined cells,
    /// so a fix that special-cased only the ARD would leave this one failing.
    #[test]
    fn get_desc_rec_on_an_apd_reports_the_undefined_fields_without_failing() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let apd = apd_of(stmt);

            let mut val: i32 = 0;
            assert_eq!(
                crate::ffi::params::sql_bind_parameter::<MockBackend>(
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
                ),
                SqlReturn::SUCCESS
            );

            let mut type_out: i16 = -1;
            let mut nullable: i16 = -1;
            let ret = sql_get_desc_rec_w::<MockBackend>(
                apd,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut type_out,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut nullable,
            );

            assert_eq!(ret, SqlReturn::SUCCESS, "an APD read must not fail");
            assert_eq!(
                type_out,
                CDataType::SLong as i16,
                "SQL_DESC_TYPE on an APD is the C type"
            );
            assert_eq!(
                nullable,
                crate::types::Nullable::SqlNullableUnknown as i16,
                "SQL_DESC_NULLABLE is undefined on an APD"
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

    // -----------------------------------------------------------------------
    // SQLCopyDesc
    // -----------------------------------------------------------------------

    /// The documented use: copy one statement's ARD onto another's APD, so the
    /// rows fetched by the first become the parameters of the second.
    #[test]
    fn copy_desc_copies_an_ard_onto_an_apd() {
        unsafe {
            let (env, conn, stmt_a) = alloc_env_conn_stmt();
            let mut stmt_b: *mut c_void = std::ptr::null_mut();
            assert_eq!(
                crate::ffi::handle::sql_alloc_handle::<MockBackend>(
                    HandleType::Stmt as i16,
                    conn,
                    &mut stmt_b
                ),
                SqlReturn::SUCCESS
            );

            let mut buf = [0u8; 8];
            assert_eq!(
                crate::ffi::bind::sql_bind_col::<MockBackend>(
                    stmt_a,
                    1,
                    CDataType::SBigInt as i16,
                    buf.as_mut_ptr().cast(),
                    8,
                    std::ptr::null_mut(),
                ),
                SqlReturn::SUCCESS
            );

            let source = ard_of(stmt_a);
            let target = apd_of(stmt_b);
            assert_eq!(
                sql_copy_desc::<MockBackend>(source, target),
                SqlReturn::SUCCESS
            );

            assert_eq!(read_numeric(target, 0, Desc::Count), 1);
            assert_eq!(
                read_numeric(target, 1, Desc::ConciseType),
                CDataType::SBigInt as isize,
                "the copied record does not describe what was bound"
            );

            let _ =
                crate::ffi::handle::sql_free_handle::<MockBackend>(HandleType::Stmt as i16, stmt_b);
            cleanup_env_conn_stmt(env, conn, stmt_a);
        }
    }

    /// Across two connections under one environment, which the spec permits and
    /// which is the case that puts source and target in different lock groups —
    /// the whole reason for the two-phase shape.
    #[test]
    fn copy_desc_spans_two_connections() {
        unsafe {
            let (env, conn_a, stmt_a) = alloc_env_conn_stmt();
            let mut conn_b: *mut c_void = std::ptr::null_mut();
            assert_eq!(
                crate::ffi::handle::sql_alloc_handle::<MockBackend>(
                    HandleType::Dbc as i16,
                    env,
                    &mut conn_b
                ),
                SqlReturn::SUCCESS
            );
            let mut stmt_b: *mut c_void = std::ptr::null_mut();
            assert_eq!(
                crate::ffi::handle::sql_alloc_handle::<MockBackend>(
                    HandleType::Stmt as i16,
                    conn_b,
                    &mut stmt_b
                ),
                SqlReturn::SUCCESS
            );

            let mut buf = [0u8; 4];
            assert_eq!(
                crate::ffi::bind::sql_bind_col::<MockBackend>(
                    stmt_a,
                    1,
                    CDataType::SLong as i16,
                    buf.as_mut_ptr().cast(),
                    4,
                    std::ptr::null_mut(),
                ),
                SqlReturn::SUCCESS
            );

            assert_eq!(
                sql_copy_desc::<MockBackend>(ard_of(stmt_a), ard_of(stmt_b)),
                SqlReturn::SUCCESS
            );
            assert_eq!(read_numeric(ard_of(stmt_b), 0, Desc::Count), 1);
            assert_eq!(
                read_numeric(ard_of(stmt_b), 1, Desc::ConciseType),
                CDataType::SLong as isize
            );

            let _ =
                crate::ffi::handle::sql_free_handle::<MockBackend>(HandleType::Stmt as i16, stmt_b);
            let _ =
                crate::ffi::handle::sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn_b);
            cleanup_env_conn_stmt(env, conn_a, stmt_a);
        }
    }

    /// Spec HY016: "TargetDescHandle cannot be set to a handle to an IRD."
    #[test]
    fn copy_desc_refuses_an_ird_target() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let target = ird_of(stmt);

            assert_eq!(
                sql_copy_desc::<MockBackend>(ard_of(stmt), target),
                SqlReturn::ERROR
            );
            assert_eq!(
                first_sqlstate(target),
                sql_state::CANNOT_MODIFY_IRD,
                "the diagnostic must be read off the target"
            );

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// Spec HY007: an IRD source whose statement has not been prepared or
    /// executed. Decided in phase one, posted against the target in phase two.
    #[test]
    fn copy_desc_refuses_an_unpopulated_ird_source() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let target = apd_of(stmt);

            assert_eq!(
                sql_copy_desc::<MockBackend>(ird_of(stmt), target),
                SqlReturn::ERROR
            );
            assert_eq!(
                first_sqlstate(target),
                sql_state::ASSOCIATED_STATEMENT_NOT_PREPARED
            );

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// An invalid source is `SQL_INVALID_HANDLE` with **no** SQLSTATE, per the
    /// page's own sentence — phase one fails before any queue is in reach.
    #[test]
    fn copy_desc_refuses_an_invalid_source_without_a_diagnostic() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let target = apd_of(stmt);

            assert_eq!(
                sql_copy_desc::<MockBackend>(std::ptr::without_provenance_mut(0x1234usize), target),
                SqlReturn::INVALID_HANDLE
            );
            // Nothing was posted anywhere: the target's queue is untouched.
            let mut state = [0u16; 6];
            let mut native: i32 = 0;
            let mut message = [0u16; 256];
            let mut message_len: i16 = 0;
            assert_eq!(
                sql_get_diag_rec_w::<MockBackend>(
                    HandleType::Desc as i16,
                    target,
                    1,
                    state.as_mut_ptr(),
                    &mut native,
                    message.as_mut_ptr(),
                    256,
                    &mut message_len,
                ),
                SqlReturn::NO_DATA,
                "an invalid SourceDescHandle must post no SQLSTATE"
            );

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// `HY021` leaves the target untouched: core checks the snapshot before it
    /// writes, which is stricter than the spec's "contents are undefined".
    #[test]
    fn copy_desc_leaves_the_target_unchanged_when_the_check_fails() {
        unsafe {
            let (env, conn, stmt_a) = alloc_env_conn_stmt();
            let mut stmt_b: *mut c_void = std::ptr::null_mut();
            assert_eq!(
                crate::ffi::handle::sql_alloc_handle::<MockBackend>(
                    HandleType::Stmt as i16,
                    conn,
                    &mut stmt_b
                ),
                SqlReturn::SUCCESS
            );

            // A record that is consistent where it is written and inconsistent
            // under the target's role. Clause 1 of the consistency check is the
            // only role-dependent one — "SQL_DESC_CONCISE_TYPE is a valid ODBC C
            // data type" applies to an ARD or APD and not to an IPD — so a concise
            // type that is a valid SQL type and *not* a valid C type is exactly
            // the case. `SQL_VARCHAR` (12) is one: `c_data_type_from_raw` has no
            // entry for it.
            //
            // Written in this order deliberately. `SQLSetDescField` runs the check
            // only when `SQL_DESC_DATA_PTR` is set, so setting the pointer first —
            // while the record still carries the default C type — passes, and
            // setting the concise type afterwards runs no check. The result is a
            // *bound* record whose type only an IPD would accept, which is what
            // makes the copy's own check the thing under test.
            let source = ard_of(stmt_a);
            let mut val: i64 = 0;
            assert_eq!(
                sql_set_desc_field_w::<MockBackend>(
                    source,
                    1,
                    Desc::DataPtr as i16,
                    std::ptr::from_mut(&mut val).cast(),
                    0,
                ),
                SqlReturn::SUCCESS,
                "a default record with a data pointer is consistent"
            );
            assert_eq!(
                sql_set_desc_field_w::<MockBackend>(
                    source,
                    1,
                    Desc::ConciseType as i16,
                    std::ptr::without_provenance_mut(SqlDataType::VARCHAR.0 as usize),
                    0,
                ),
                SqlReturn::SUCCESS,
                "setting the concise type runs no check of its own"
            );

            // The target starts with one binding of its own, which must survive.
            let mut buf = [0u8; 4];
            assert_eq!(
                crate::ffi::bind::sql_bind_col::<MockBackend>(
                    stmt_b,
                    7,
                    CDataType::SLong as i16,
                    buf.as_mut_ptr().cast(),
                    4,
                    std::ptr::null_mut(),
                ),
                SqlReturn::SUCCESS
            );
            let target = ard_of(stmt_b);
            let before = read_numeric(target, 0, Desc::Count);

            assert_eq!(
                sql_copy_desc::<MockBackend>(source, target),
                SqlReturn::ERROR
            );
            assert_eq!(
                first_sqlstate(target),
                sql_state::INCONSISTENT_DESCRIPTOR_INFORMATION
            );
            assert_eq!(
                read_numeric(target, 0, Desc::Count),
                before,
                "a refused copy must leave the target untouched"
            );
            assert_eq!(
                read_numeric(target, 7, Desc::ConciseType),
                CDataType::SLong as isize,
                "the target's own binding was overwritten by a refused copy"
            );

            let _ =
                crate::ffi::handle::sql_free_handle::<MockBackend>(HandleType::Stmt as i16, stmt_b);
            cleanup_env_conn_stmt(env, conn, stmt_a);
        }
    }

    /// `SQL_DESC_ALLOC_TYPE` is never copied: the spec exempts it by name, and it
    /// belongs to the allocation rather than to its contents.
    #[test]
    fn copy_desc_does_not_copy_the_alloc_type() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let mut target: *mut c_void = std::ptr::null_mut();
            assert_eq!(
                crate::ffi::handle::sql_alloc_handle::<MockBackend>(
                    HandleType::Desc as i16,
                    conn,
                    &mut target
                ),
                SqlReturn::SUCCESS
            );

            assert_eq!(
                sql_copy_desc::<MockBackend>(ard_of(stmt), target),
                SqlReturn::SUCCESS
            );
            assert_eq!(
                read_numeric(target, 0, Desc::AllocType),
                crate::types::SQL_DESC_ALLOC_USER,
                "the source's SQL_DESC_ALLOC_AUTO was copied onto an explicit descriptor"
            );

            let _ =
                crate::ffi::handle::sql_free_handle::<MockBackend>(HandleType::Desc as i16, target);
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// An IRD keeps `SQL_DESC_ARRAY_STATUS_PTR` on its owning statement, as
    /// `SQL_ATTR_ROW_STATUS_PTR`, rather than in its own header. A snapshot
    /// built from the descriptor's own map alone drops it, and the spec forbids
    /// that: "All fields of the descriptor, except SQL_DESC_ALLOC_TYPE ..., are
    /// copied, whether or not the field is defined for the destination
    /// descriptor."
    ///
    /// `MockTypeInfoBackend` and `SQLGetTypeInfo` because the source IRD has to
    /// be *populated*: an unpopulated one is `HY007` and the copy would fail
    /// before reaching anything this test is about.
    #[test]
    fn copy_desc_carries_the_irds_statement_held_header_fields() {
        unsafe {
            let (env, conn, stmt_a) =
                crate::test_utils::alloc_connected_env_conn_stmt::<MockTypeInfoBackend>();
            let mut stmt_b: *mut c_void = std::ptr::null_mut();
            assert_eq!(
                crate::ffi::handle::sql_alloc_handle::<MockTypeInfoBackend>(
                    HandleType::Stmt as i16,
                    conn,
                    &mut stmt_b
                ),
                SqlReturn::SUCCESS
            );

            assert_eq!(
                crate::ffi::info::sql_get_type_info::<MockTypeInfoBackend>(
                    stmt_a,
                    crate::types::SqlDataType::UNKNOWN_TYPE.0,
                ),
                SqlReturn::SUCCESS,
                "precondition: the IRD is populated"
            );

            let mut row_status: u16 = 0;
            let status_ptr = std::ptr::from_mut(&mut row_status).cast::<c_void>();
            assert_eq!(
                crate::ffi::stmt_attr::sql_set_stmt_attr_w::<MockTypeInfoBackend>(
                    stmt_a,
                    StatementAttribute::RowStatusPtr as i32,
                    status_ptr,
                    0,
                ),
                SqlReturn::SUCCESS
            );

            let source =
                desc_token_of::<MockTypeInfoBackend>(stmt_a, StatementAttribute::ImpRowDesc);
            let target =
                desc_token_of::<MockTypeInfoBackend>(stmt_b, StatementAttribute::AppParamDesc);
            assert_eq!(
                sql_copy_desc::<MockTypeInfoBackend>(source, target),
                SqlReturn::SUCCESS
            );

            assert_eq!(
                read_numeric_of::<MockTypeInfoBackend>(target, 0, Desc::ArrayStatusPtr),
                status_ptr as isize,
                "SQL_DESC_ARRAY_STATUS_PTR was not carried off the IRD"
            );

            let _ = crate::ffi::handle::sql_free_handle::<MockTypeInfoBackend>(
                HandleType::Stmt as i16,
                stmt_b,
            );
            crate::test_utils::cleanup_connected_env_conn_stmt::<MockTypeInfoBackend>(
                env, conn, stmt_a,
            );
        }
    }

    /// The other direction: an IPD **target**. `SQL_DESC_ARRAY_STATUS_PTR` is
    /// `SQL_ATTR_PARAM_STATUS_PTR` there and lives on the statement, so a copy
    /// that wrote it into the descriptor's own map would land it where nothing
    /// reads it — the value would be silently unreadable through either door.
    #[test]
    fn copy_desc_routes_the_ipds_statement_held_header_fields() {
        unsafe {
            let (env, conn, stmt_a) = alloc_env_conn_stmt();
            let mut stmt_b: *mut c_void = std::ptr::null_mut();
            assert_eq!(
                crate::ffi::handle::sql_alloc_handle::<MockBackend>(
                    HandleType::Stmt as i16,
                    conn,
                    &mut stmt_b
                ),
                SqlReturn::SUCCESS
            );

            // On an ARD this attribute *is* `SQL_DESC_ARRAY_STATUS_PTR`, stored
            // on the descriptor's own header — so the source side needs no fix
            // and only the target's routing is under test here.
            let mut row_operation: u16 = 0;
            let operation_ptr = std::ptr::from_mut(&mut row_operation).cast::<c_void>();
            assert_eq!(
                crate::ffi::stmt_attr::sql_set_stmt_attr_w::<MockBackend>(
                    stmt_a,
                    StatementAttribute::RowOperationPtr as i32,
                    operation_ptr,
                    0,
                ),
                SqlReturn::SUCCESS
            );

            let source = ard_of(stmt_a);
            let target = desc_token_of::<MockBackend>(stmt_b, StatementAttribute::ImpParamDesc);
            assert_eq!(
                sql_copy_desc::<MockBackend>(source, target),
                SqlReturn::SUCCESS
            );

            assert_eq!(
                read_numeric(target, 0, Desc::ArrayStatusPtr),
                operation_ptr as isize,
                "SQL_DESC_ARRAY_STATUS_PTR is unreadable through the IPD"
            );

            // The same storage through the other door, which is the half that
            // proves the routing rather than merely the copy.
            let mut readback: *mut c_void = std::ptr::null_mut();
            assert_eq!(
                sql_get_stmt_attr_w::<MockBackend>(
                    stmt_b,
                    StatementAttribute::ParamStatusPtr as i32,
                    std::ptr::from_mut(&mut readback).cast::<c_void>(),
                    0,
                    std::ptr::null_mut(),
                ),
                SqlReturn::SUCCESS
            );
            assert_eq!(
                readback, operation_ptr,
                "SQL_ATTR_PARAM_STATUS_PTR and SQL_DESC_ARRAY_STATUS_PTR are one value"
            );

            let _ =
                crate::ffi::handle::sql_free_handle::<MockBackend>(HandleType::Stmt as i16, stmt_b);
            cleanup_env_conn_stmt(env, conn, stmt_a);
        }
    }

    /// The APD's token, as the application receives it.
    unsafe fn apd_of(stmt: *mut c_void) -> *mut c_void {
        let mut token: *mut c_void = std::ptr::null_mut();
        let ret = unsafe {
            sql_get_stmt_attr_w::<MockBackend>(
                stmt,
                StatementAttribute::AppParamDesc as i32,
                std::ptr::from_mut(&mut token).cast::<c_void>(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SqlReturn::SUCCESS, "SQLGetStmtAttrW(APP_PARAM_DESC)");
        assert!(!token.is_null(), "the APD token must not be null");
        token
    }

    /// One numeric descriptor field, through `SQLGetDescFieldW`.
    unsafe fn read_numeric(desc: *mut c_void, record: i16, field: Desc) -> isize {
        let mut value: isize = -1;
        let ret = unsafe {
            sql_get_desc_field_w::<MockBackend>(
                desc,
                record,
                field as i16,
                std::ptr::from_mut(&mut value).cast(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SqlReturn::SUCCESS, "SQLGetDescFieldW({field:?})");
        value
    }

    /// One of a statement's four descriptor tokens, for a test driving a
    /// backend other than `MockBackend`.
    ///
    /// The type parameter is load-bearing for the same reason
    /// [`first_sqlstate_of`]'s is: the statement is resolved as
    /// `StatementHandle<B>`, so `MockBackend` here against another backend's
    /// handle would be a type confusion rather than a failed assertion.
    unsafe fn desc_token_of<B: Backend>(
        stmt: *mut c_void,
        attribute: StatementAttribute,
    ) -> *mut c_void {
        let mut token: *mut c_void = std::ptr::null_mut();
        let ret = unsafe {
            sql_get_stmt_attr_w::<B>(
                stmt,
                attribute as i32,
                std::ptr::from_mut(&mut token).cast::<c_void>(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SqlReturn::SUCCESS, "SQLGetStmtAttrW({attribute:?})");
        assert!(!token.is_null(), "the descriptor token must not be null");
        token
    }

    /// [`read_numeric`] for a test driving a backend other than `MockBackend`.
    unsafe fn read_numeric_of<B: Backend>(desc: *mut c_void, record: i16, field: Desc) -> isize {
        let mut value: isize = -1;
        let ret = unsafe {
            sql_get_desc_field_w::<B>(
                desc,
                record,
                field as i16,
                std::ptr::from_mut(&mut value).cast(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SqlReturn::SUCCESS, "SQLGetDescFieldW({field:?})");
        value
    }
}
