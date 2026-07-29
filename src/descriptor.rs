//! The descriptor data model: one record type, the roles that give its fields
//! meaning, and the consistency check the spec requires before a binding is
//! used.
//!
//! ODBC has one record shape, not three. A descriptor record carries every
//! `SQL_DESC_*` record field, and each descriptor *role* uses a subset — which
//! is why `SQLSetDescField` accepts any field identifier against any descriptor
//! and decides validity from the role. Modelling that directly is what lets an
//! explicitly allocated descriptor exist at all: the spec says "it is not known
//! whether an explicitly allocated application descriptor is an APD or ARD
//! until execute time", so its records cannot be typed by role.

// The field tables and the record accessors below are written against the
// spec's tables rather than against a caller, and their callers are the four
// `SQLxxxDesc` entry points, which land after them. Until those arrive nothing
// outside this module's own tests reads them. The allow comes off with the last
// of the four; if it is still here once they are all in, something is genuinely
// unreachable.
#![allow(dead_code)]

use std::ffi::c_void;

use odbc_sys::{CDataType, Desc, ParamType, SqlDataType, StatementAttribute};

use crate::errors::OdbcError;
use crate::types::{SqlState, ULen, c_data_type_from_raw};

/// Which of ODBC's four descriptors a [`Descriptor`] is.
///
/// The field tables are indexed by this: a field defined for an ARD may be
/// `HY091` on an IPD, and the same identifier can name a C type on one and a
/// SQL type on another.
///
/// [`Descriptor`]: crate::handles::Descriptor
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DescriptorRole {
    /// Application row descriptor — where `SQLBindCol` writes.
    Ard,
    /// Application parameter descriptor — `SQLBindParameter`'s C-side half.
    Apd,
    /// Implementation row descriptor — result metadata. Core stores no records
    /// here; reads are computed from `ColumnDescriptor`.
    Ird,
    /// Implementation parameter descriptor — `SQLBindParameter`'s declared-type
    /// half.
    Ipd,
}

/// One descriptor record: every `SQL_DESC_*` record field, whatever the role.
///
/// Fields the spec initialises "ND" (no default) still have a Rust value here,
/// because a Rust struct has no third state — the spec's own answer for reading
/// one before it is set is that the value is undefined, not that the read
/// fails, so any value is conforming. The `Default` impl uses the spec's stated
/// defaults where it gives one and a zero otherwise.
#[derive(Debug)]
pub struct DescriptorRecord {
    /// `SQL_DESC_CONCISE_TYPE`. A C type on an ARD or APD, a SQL type on an
    /// IPD. Stored raw because it is one field with two readings; see
    /// [`Self::c_type`] and [`Self::sql_type`].
    pub concise_type: i16,
    /// `SQL_DESC_TYPE` — the *verbose* type, which differs from the concise one
    /// only for the datetime and interval families.
    pub verbose_type: i16,
    /// `SQL_DESC_DATETIME_INTERVAL_CODE`.
    pub datetime_interval_code: i16,
    /// `SQL_DESC_DATETIME_INTERVAL_PRECISION`.
    pub datetime_interval_precision: i32,
    /// `SQL_DESC_LENGTH` — the declared column size.
    pub length: ULen,
    /// `SQL_DESC_OCTET_LENGTH` — the buffer length in bytes.
    pub octet_length: isize,
    /// `SQL_DESC_PRECISION`.
    pub precision: i16,
    /// `SQL_DESC_SCALE`.
    pub scale: i16,
    /// `SQL_DESC_NUM_PREC_RADIX`.
    pub num_prec_radix: i32,
    /// `SQL_DESC_DATA_PTR`. Null means unbound — see [`Self::is_bound`].
    pub data_ptr: *mut c_void,
    /// `SQL_DESC_INDICATOR_PTR`.
    pub indicator_ptr: *mut isize,
    /// `SQL_DESC_OCTET_LENGTH_PTR`.
    pub octet_length_ptr: *mut isize,
    /// `SQL_DESC_PARAMETER_TYPE` — IPD only.
    pub parameter_type: ParamType,
    /// `SQL_DESC_NAME` — IPD writable, IRD read-only.
    pub name: String,
    /// `SQL_DESC_UNNAMED` — `SQL_NAMED` or `SQL_UNNAMED`.
    pub unnamed: isize,
}

// SAFETY: DescriptorRecord holds raw pointers into application-owned buffers.
// The ODBC contract guarantees they stay valid until the binding is changed or
// the statement is freed. Same reasoning as the ColumnBinding it replaces.
unsafe impl Send for DescriptorRecord {}
unsafe impl Sync for DescriptorRecord {}

impl Default for DescriptorRecord {
    fn default() -> Self {
        Self {
            concise_type: CDataType::Default as i16,
            verbose_type: CDataType::Default as i16,
            datetime_interval_code: 0,
            datetime_interval_precision: 0,
            length: 0,
            octet_length: 0,
            precision: 0,
            scale: 0,
            num_prec_radix: 0,
            data_ptr: std::ptr::null_mut(),
            indicator_ptr: std::ptr::null_mut(),
            octet_length_ptr: std::ptr::null_mut(),
            parameter_type: ParamType::Input,
            name: String::new(),
            unnamed: crate::types::SQL_UNNAMED,
        }
    }
}

impl DescriptorRecord {
    /// [`Self::concise_type`] read as a C data type, for an ARD or APD.
    ///
    /// Fallible because `SQLSetDescField` can store any `i16` here, unlike
    /// `SQLBindCol`, which validated it at the boundary. `HY003` is the code
    /// `SQLBindCol` already uses for an unrecognised C type.
    pub fn c_type(&self) -> Result<CDataType, OdbcError> {
        c_data_type_from_raw(self.concise_type).ok_or_else(|| {
            OdbcError::general(
                format!("Unknown C data type: {}", self.concise_type),
                SqlState::invalid_application_buffer_type(),
            )
        })
    }

    /// [`Self::concise_type`] read as a SQL data type, for an IPD.
    ///
    /// Infallible, because `SqlDataType` is a newtype over `i16` with no closed
    /// set — a driver-specific SQL type is a legal value the spec's own
    /// consistency check allows.
    pub fn sql_type(&self) -> SqlDataType {
        SqlDataType(self.concise_type)
    }

    /// Whether this record is a *binding*, as opposed to a record that merely
    /// exists.
    ///
    /// The distinction did not exist before descriptors were writable: a record
    /// was created whole by `SQLBindCol` or `SQLBindParameter` and removed
    /// entirely by the unbind form, so "the key is present" answered the
    /// question. `SQLSetDescField` can create a record by setting any one
    /// field, so the spec's own answer is the only one left — a null
    /// `SQL_DESC_DATA_PTR` means unbound, and setting it to null is how
    /// `SQLSetDescRec` unbinds a column.
    pub fn is_bound(&self) -> bool {
        !self.data_ptr.is_null()
    }

    /// Set `SQL_DESC_CONCISE_TYPE`, and with it the two fields the spec makes
    /// follow from it.
    ///
    /// "When the SQL_DESC_CONCISE_TYPE field is set, the SQL_DESC_TYPE field is
    /// set to the corresponding verbose type, and the
    /// SQL_DESC_DATETIME_INTERVAL_CODE field is set to the corresponding
    /// subcode." One act, not three, so every writer of a concise type goes
    /// through here — `SQLBindCol`, `SQLBindParameter`, `SQLSetDescField` and
    /// `SQLSetDescRec` alike. A site that set the three itself would be one
    /// datetime bind away from failing its own consistency check.
    pub fn set_concise_type(&mut self, concise: i16) {
        self.concise_type = concise;
        self.verbose_type = crate::types::col_attr::verbose_type(SqlDataType(concise));
        self.datetime_interval_code = crate::types::col_attr::datetime_interval_subcode(concise);
    }
}

/// The spec's consistency check, returning `HY021`.
///
/// `SQLSetDescRec`'s "Consistency Checks" section, which states when it runs:
/// "This check is always performed when **SQLBindParameter** or **SQLBindCol**
/// is called or when **SQLSetDescRec** is called for an APD, ARD, or IPD" —
/// and `SQLSetDescField` adds a fourth site, when it sets
/// `SQL_DESC_DATA_PTR`. All four call this.
///
/// The five clauses, and what core does with each:
///
/// 1. **"The SQL_DESC_TYPE field must be one of the valid ODBC C or SQL types
///    or a driver-specific SQL type."** Checked for an ARD and APD, where the
///    type is a C type and the set is closed. Not checked for an IPD: "or a
///    driver-specific SQL type" leaves no value to reject, since a driver may
///    define any of them.
/// 2. **"If the SQL_DESC_TYPE field is SQL_DATETIME or SQL_INTERVAL, the
///    SQL_DESC_DATETIME_INTERVAL_CODE field must be one of the valid datetime
///    or interval codes."** Checked for datetime. **Reduced for interval:**
///    core supports no interval types, so it cannot enumerate their codes, and
///    the half of the clause it can act on is the converse — a datetime or
///    interval subcode on a type that has neither is rejected.
/// 3. **"If the type is numeric, the SQL_DESC_PRECISION and SQL_DESC_SCALE
///    fields are verified to be valid."** Checked for the exact numeric types,
///    where both fields are defined: a negative precision or scale, or a scale
///    exceeding the precision.
/// 4. **"If SQL_DESC_CONCISE_TYPE is a time or timestamp type, or an interval
///    with a seconds component, SQL_DESC_PRECISION is a valid seconds
///    precision."** Checked for time and timestamp. **Reduced for interval,**
///    as clause 2.
/// 5. **"If SQL_DESC_CONCISE_TYPE is an interval type,
///    SQL_DESC_DATETIME_INTERVAL_PRECISION is a valid interval leading
///    precision."** **Not checked:** core supports no interval types, so there
///    is no leading precision it could validate against.
pub fn consistency_check(record: &DescriptorRecord, role: DescriptorRole) -> Result<(), OdbcError> {
    use crate::types::{
        SQL_CODE_DATE, SQL_CODE_TIME, SQL_CODE_TIMESTAMP, SQL_DATETIME, SQL_INTERVAL,
    };

    let concise = record.concise_type;

    // Clause 1.
    if matches!(role, DescriptorRole::Ard | DescriptorRole::Apd)
        && c_data_type_from_raw(concise).is_none()
    {
        return Err(inconsistent(format!(
            "SQL_DESC_CONCISE_TYPE {concise} is not an ODBC C data type"
        )));
    }

    // Clause 2.
    let verbose = crate::types::col_attr::verbose_type(SqlDataType(concise));
    if verbose == SQL_DATETIME {
        if !matches!(
            record.datetime_interval_code,
            SQL_CODE_DATE | SQL_CODE_TIME | SQL_CODE_TIMESTAMP
        ) {
            return Err(inconsistent(format!(
                "SQL_DESC_DATETIME_INTERVAL_CODE {} is not a valid datetime code",
                record.datetime_interval_code
            )));
        }
    } else if verbose != SQL_INTERVAL && record.datetime_interval_code != 0 {
        return Err(inconsistent(format!(
            "SQL_DESC_DATETIME_INTERVAL_CODE is {} on type {concise}, which is neither \
             a datetime nor an interval type",
            record.datetime_interval_code
        )));
    }

    // Clause 3. The exact numerics are the types for which the spec defines
    // both fields; for every other type SQL_DESC_SCALE is undefined, so there
    // is nothing to verify it against.
    if concise == SqlDataType::DECIMAL.0 || concise == SqlDataType::NUMERIC.0 {
        if record.precision < 0 || record.scale < 0 {
            return Err(inconsistent(format!(
                "SQL_DESC_PRECISION {} and SQL_DESC_SCALE {} must not be negative",
                record.precision, record.scale
            )));
        }
        if record.scale > record.precision {
            return Err(inconsistent(format!(
                "SQL_DESC_SCALE {} exceeds SQL_DESC_PRECISION {}",
                record.scale, record.precision
            )));
        }
    }

    // Clause 4. A seconds precision is a count of fractional-second digits,
    // which ODBC bounds at nine.
    if verbose == SQL_DATETIME
        && matches!(
            record.datetime_interval_code,
            SQL_CODE_TIME | SQL_CODE_TIMESTAMP
        )
        && !(0..=MAX_SECONDS_PRECISION).contains(&record.precision)
    {
        return Err(inconsistent(format!(
            "SQL_DESC_PRECISION {} is not a valid seconds precision",
            record.precision
        )));
    }

    Ok(())
}

/// The largest fractional-seconds precision ODBC defines, in digits.
const MAX_SECONDS_PRECISION: i16 = 9;

/// `HY021` — "inconsistent descriptor information".
fn inconsistent(detail: String) -> OdbcError {
    OdbcError::general(detail, SqlState::inconsistent_descriptor_information())
}

/// Whether a field is readable, writable or undefined for a descriptor role.
///
/// Transcribed cell by cell from `SQLSetDescField`'s "Initialization of
/// Descriptor Fields" tables, and deliberately not derived from anything: it is
/// a matrix the spec states, and the only way to check it is to read it against
/// that page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldAccess {
    /// Readable and writable.
    ReadWrite,
    /// Readable; a write is `HY091`.
    ReadOnly,
    /// Not defined for this role; both directions are `HY091`.
    Undefined,
}

/// How `field` may be accessed on a descriptor of this `role`.
///
/// One arm per row of the spec's two initialization tables, in the order they
/// give them: the header fields, then the record fields, then the IRD's
/// read-only set. Reviewing this function means reading it against that page
/// cell by cell — nothing about it is inferable from the rest of the crate.
///
/// This is the sole authority on `HY091`. It covers the header fields as well
/// as the record ones, because the caller decides *where* a field is stored and
/// this decides whether it may be touched at all.
pub fn field_access(role: DescriptorRole, field: Desc) -> FieldAccess {
    use DescriptorRole::{Apd, Ard, Ipd, Ird};
    use FieldAccess::{ReadOnly, ReadWrite, Undefined};

    match field {
        // ------------------------------------------------------------------
        // Header fields
        // ------------------------------------------------------------------

        // R on all four. `SQL_DESC_ALLOC_AUTO` for every descriptor core has,
        // since all four are implicitly allocated; D4 makes the value vary.
        Desc::AllocType => ReadOnly,
        Desc::ArraySize => match role {
            Ard | Apd => ReadWrite,
            Ird | Ipd => Undefined,
        },
        Desc::ArrayStatusPtr => match role {
            Ard | Apd | Ird | Ipd => ReadWrite,
        },
        Desc::BindOffsetPtr | Desc::BindType => match role {
            Ard | Apd => ReadWrite,
            Ird | Ipd => Undefined,
        },
        // Writing it lower deletes the higher-numbered records; the IRD's is
        // the column count, which the application does not get to choose.
        Desc::Count => match role {
            Ard | Apd | Ipd => ReadWrite,
            Ird => ReadOnly,
        },
        Desc::RowsProcessedPtr => match role {
            Ird | Ipd => ReadWrite,
            Ard | Apd => Undefined,
        },

        // ------------------------------------------------------------------
        // Record fields
        // ------------------------------------------------------------------
        Desc::ConciseType
        | Desc::Type
        | Desc::OctetLength
        | Desc::Length
        | Desc::Precision
        | Desc::Scale
        | Desc::DatetimeIntervalCode
        | Desc::DatetimeIntervalPrecision
        | Desc::NumPrecRadix => match role {
            Ard | Apd | Ipd => ReadWrite,
            Ird => ReadOnly,
        },
        Desc::IndicatorPtr | Desc::OctetLengthPtr => match role {
            Ard | Apd => ReadWrite,
            Ird | Ipd => Undefined,
        },
        // The IPD's is the documented oddity. The initialization table marks it
        // "Unused", but `SQLSetDescField`'s own prose overrides that for the
        // write direction: "The SQL_DESC_DATA_PTR field of the IPD can be set
        // to force a consistency check", and "the value ... is not actually
        // stored and cannot be retrieved". So a write is legal and discarded
        // (see [`set_record_field`]), and a read gets back the null that was
        // never overwritten — which is conforming, since the spec only says a
        // read is not *required* to return what was set.
        Desc::DataPtr => match role {
            Ard | Apd | Ipd => ReadWrite,
            Ird => Undefined,
        },
        Desc::ParameterType => match role {
            Ipd => ReadWrite,
            Ard | Apd | Ird => Undefined,
        },
        Desc::Name | Desc::Unnamed => match role {
            Ipd => ReadWrite,
            Ird => ReadOnly,
            Ard | Apd => Undefined,
        },
        Desc::Nullable | Desc::RowVer => match role {
            Ird | Ipd => ReadOnly,
            Ard | Apd => Undefined,
        },

        // ------------------------------------------------------------------
        // The IRD's read-only set: result metadata, which only the IRD has.
        // ------------------------------------------------------------------

        // The last five of these are footnote [1]'s: `SQL_DESC_CASE_SENSITIVE`,
        // `SQL_DESC_FIXED_PREC_SCALE`, `SQL_DESC_LOCAL_TYPE_NAME`,
        // `SQL_DESC_TYPE_NAME` and `SQL_DESC_UNSIGNED` "are defined only when
        // the IPD is automatically populated by the driver. If not, they are
        // undefined." Core answers `SQL_ATTR_AUTO_IPD` with `SQL_FALSE` and
        // refuses `SQL_ATTR_ENABLE_AUTO_IPD = SQL_TRUE`, so its IPD is not
        // auto-populated and they land here with the rest rather than in the
        // row above. That is a consequence of an answer core already gives.
        Desc::AutoUniqueValue
        | Desc::BaseColumnName
        | Desc::BaseTableName
        | Desc::CatalogName
        | Desc::DisplaySize
        | Desc::Label
        | Desc::LiteralPrefix
        | Desc::LiteralSuffix
        | Desc::SchemaName
        | Desc::Searchable
        | Desc::TableName
        | Desc::Updatable
        | Desc::CaseSensitive
        | Desc::FixedPrecScale
        | Desc::LocalTypeName
        | Desc::TypeName
        | Desc::Unsigned => match role {
            Ird => ReadOnly,
            Ard | Apd | Ipd => Undefined,
        },

        // `SQL_DESC_MAXIMUM_SCALE` and `SQL_DESC_MINIMUM_SCALE` appear in
        // `sqlext.h`, and therefore in `odbc-sys`, but in neither of
        // `SQLSetDescField`'s tables — they describe a *type*, which is
        // `SQLGetTypeInfo`'s subject, not a descriptor record's.
        //
        // The catch-all beside them exists only for the identifiers `odbc-sys`
        // adds behind its `odbc_version_4` feature, which a driver can turn on
        // through feature unification. Every ODBC 3.x identifier is named
        // above, so nothing else reaches it.
        _ => Undefined,
    }
}

/// The statement attribute a descriptor **header** field aliases on this role,
/// or `None` if the field is not stored as one.
///
/// `SQLSetStmtAttr`'s mapping table read in the other direction.
/// [`HeaderOwner::of`] answers "which descriptor does this statement attribute
/// live on"; this answers "which statement attribute is this header field of
/// this descriptor". They must name the same storage, or the two doors onto one
/// value disagree — which is the defect this whole milestone exists to remove.
///
/// The two header fields with no entry are the ones core computes rather than
/// stores: `SQL_DESC_COUNT` is derived from the record map, and
/// `SQL_DESC_ALLOC_TYPE` is [`SQL_DESC_ALLOC_AUTO`] for every descriptor core
/// owns.
///
/// The IRD and IPD rows are the four pairs D2 deliberately left on the
/// statement rather than re-homing onto a descriptor header; `attr_store`
/// routes them there, so a caller of this needs to know nothing about the
/// split.
///
/// [`HeaderOwner::of`]: crate::handles::HeaderOwner::of
/// [`SQL_DESC_ALLOC_AUTO`]: crate::types::SQL_DESC_ALLOC_AUTO
pub fn header_attribute(role: DescriptorRole, field: Desc) -> Option<StatementAttribute> {
    use DescriptorRole::{Apd, Ard, Ipd, Ird};
    use StatementAttribute as A;

    match (field, role) {
        (Desc::ArraySize, Ard) => Some(A::RowArraySize),
        (Desc::ArraySize, Apd) => Some(A::ParamsetSize),
        (Desc::BindType, Ard) => Some(A::RowBindType),
        (Desc::BindType, Apd) => Some(A::ParamBindType),
        (Desc::BindOffsetPtr, Ard) => Some(A::RowBindOffsetPtr),
        (Desc::BindOffsetPtr, Apd) => Some(A::ParamBindOffsetPtr),
        // `odbc-sys` spells `SQL_ATTR_PARAM_OPERATION_PTR`
        // `ParamOpterationPtr` — transposed letters, upstream.
        (Desc::ArrayStatusPtr, Ard) => Some(A::RowOperationPtr),
        (Desc::ArrayStatusPtr, Apd) => Some(A::ParamOpterationPtr),
        (Desc::ArrayStatusPtr, Ird) => Some(A::RowStatusPtr),
        (Desc::ArrayStatusPtr, Ipd) => Some(A::ParamStatusPtr),
        (Desc::RowsProcessedPtr, Ird) => Some(A::RowsFetchedPtr),
        (Desc::RowsProcessedPtr, Ipd) => Some(A::ParamsProcessedPtr),
        _ => None,
    }
}

/// What a header field reads as before anything has set it.
///
/// The "Default" column of the spec's header table: `1` for
/// `SQL_DESC_ARRAY_SIZE`, `SQL_BIND_BY_COLUMN` for `SQL_DESC_BIND_TYPE` and a
/// null pointer for the rest — all of which are `0` except the first.
pub fn header_default(field: Desc) -> usize {
    match field {
        Desc::ArraySize => 1,
        _ => 0,
    }
}

/// One descriptor field's value, in the two shapes the ABI has.
///
/// Deliberately the same shape as [`ColAttrValue`], because `SQLGetDescField`
/// marshals both through one code path: the IRD's values arrive as
/// `ColAttrValue` from the view and every other role's as this.
///
/// [`ColAttrValue`]: crate::types::col_attr::ColAttrValue
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DescFieldValue {
    /// A numeric field, including the pointer-valued ones.
    Numeric(isize),
    /// A character field.
    String(String),
}

/// Read `field` out of a record.
///
/// Only the fields a [`DescriptorRecord`] actually stores are answered here.
/// The two families that are not are the caller's to route before it gets this
/// far, and both return `HY091` rather than a wrong value if it does not:
///
/// - **The header fields**, which live on the descriptor rather than on any
///   record.
/// - **The IRD's read-only metadata**, which is computed from the result set's
///   `ColumnDescriptor` and never stored — that is the whole point of the IRD
///   being a view.
pub fn get_record_field(
    record: &DescriptorRecord,
    role: DescriptorRole,
    field: Desc,
) -> Result<DescFieldValue, OdbcError> {
    if field_access(role, field) == FieldAccess::Undefined {
        return Err(undefined_field(role, field));
    }

    let numeric = match field {
        Desc::ConciseType => isize::from(record.concise_type),
        Desc::Type => isize::from(record.verbose_type),
        Desc::DatetimeIntervalCode => isize::from(record.datetime_interval_code),
        Desc::DatetimeIntervalPrecision => record.datetime_interval_precision as isize,
        Desc::Length => record.length as isize,
        Desc::OctetLength => record.octet_length,
        Desc::Precision => isize::from(record.precision),
        Desc::Scale => isize::from(record.scale),
        Desc::NumPrecRadix => record.num_prec_radix as isize,
        Desc::DataPtr => record.data_ptr as isize,
        Desc::IndicatorPtr => record.indicator_ptr as isize,
        Desc::OctetLengthPtr => record.octet_length_ptr as isize,
        Desc::ParameterType => record.parameter_type as isize,
        Desc::Unnamed => record.unnamed,
        Desc::Name => return Ok(DescFieldValue::String(record.name.clone())),
        // Both are "R, ND" on the IPD: defined only once the driver has
        // populated it, which core never does. The values below are the spec's
        // own "not known" answers rather than invented ones.
        Desc::Nullable => crate::types::Nullable::SqlNullableUnknown as isize,
        Desc::RowVer => crate::types::SQL_FALSE as isize,
        _ => return Err(undefined_field(role, field)),
    };
    Ok(DescFieldValue::Numeric(numeric))
}

/// Write `field` into a record.
///
/// `HY091` if the role does not define the field or defines it read-only;
/// `HY092` if the value is of the wrong shape for it. Routing is as
/// [`get_record_field`] describes.
pub fn set_record_field(
    record: &mut DescriptorRecord,
    role: DescriptorRole,
    field: Desc,
    value: DescFieldValue,
) -> Result<(), OdbcError> {
    match field_access(role, field) {
        FieldAccess::ReadWrite => {}
        FieldAccess::ReadOnly | FieldAccess::Undefined => return Err(undefined_field(role, field)),
    }

    // The one string-valued record field. Taken first so every arm below can
    // assume a number.
    if field == Desc::Name {
        return match value {
            DescFieldValue::String(name) => {
                record.name = name;
                Ok(())
            }
            DescFieldValue::Numeric(_) => Err(wrong_value(field)),
        };
    }

    let DescFieldValue::Numeric(n) = value else {
        return Err(wrong_value(field));
    };

    match field {
        // Setting the concise type sets the verbose type and the
        // datetime/interval code with it — the spec makes them one act. See
        // [`DescriptorRecord::set_concise_type`], which every writer shares.
        Desc::ConciseType => record.set_concise_type(narrow_i16(n, field)?),
        // The other direction. For a non-datetime type the two are equal, so
        // the concise type follows; for `SQL_DATETIME` or `SQL_INTERVAL` the
        // concise type is only determined once
        // `SQL_DESC_DATETIME_INTERVAL_CODE` is also set, so it is left alone.
        Desc::Type => {
            let verbose = narrow_i16(n, field)?;
            record.verbose_type = verbose;
            if crate::types::col_attr::verbose_type(SqlDataType(verbose)) == verbose {
                record.concise_type = verbose;
                record.datetime_interval_code = 0;
            }
        }
        Desc::DatetimeIntervalCode => record.datetime_interval_code = narrow_i16(n, field)?,
        Desc::DatetimeIntervalPrecision => {
            record.datetime_interval_precision = narrow_i32(n, field)?;
        }
        Desc::Length => {
            record.length = ULen::try_from(n).map_err(|_| wrong_value(field))?;
        }
        Desc::OctetLength => record.octet_length = n,
        Desc::Precision => record.precision = narrow_i16(n, field)?,
        Desc::Scale => record.scale = narrow_i16(n, field)?,
        Desc::NumPrecRadix => record.num_prec_radix = narrow_i32(n, field)?,
        // The IPD's is set to force the consistency check and deliberately not
        // stored — see [`field_access`]. The check itself is the caller's, at
        // all four of the sites the spec names.
        Desc::DataPtr => {
            if role != DescriptorRole::Ipd {
                record.data_ptr = n as *mut c_void;
            }
        }
        Desc::IndicatorPtr => record.indicator_ptr = n as *mut isize,
        Desc::OctetLengthPtr => record.octet_length_ptr = n as *mut isize,
        Desc::ParameterType => {
            record.parameter_type = crate::types::param_type_from_raw(narrow_i16(n, field)?)
                .ok_or_else(|| {
                    OdbcError::general(
                        format!("Invalid SQL_DESC_PARAMETER_TYPE value: {n}"),
                        SqlState::invalid_attribute_option_identifier(),
                    )
                })?;
        }
        // `SQLSetDescField`'s `HY092` row names this case on its own: "The
        // FieldIdentifier argument was SQL_DESC_UNNAMED, and ValuePtr was
        // SQL_NAMED." Only `SQL_UNNAMED` may be written; a name is what makes a
        // record named, not this field.
        Desc::Unnamed => {
            if n != crate::types::SQL_UNNAMED {
                return Err(wrong_value(field));
            }
            record.unnamed = n;
        }
        _ => return Err(undefined_field(role, field)),
    }
    Ok(())
}

/// `HY091` — the field is not defined for this role, or is read-only on it.
fn undefined_field(role: DescriptorRole, field: Desc) -> OdbcError {
    OdbcError::general(
        format!("Descriptor field {field:?} is not writable on {role:?}"),
        SqlState::invalid_descriptor_field_identifier(),
    )
}

/// `HY092` — "the value in *ValuePtr was not valid for the FieldIdentifier
/// argument".
fn wrong_value(field: Desc) -> OdbcError {
    OdbcError::general(
        format!("Invalid value for descriptor field {field:?}"),
        SqlState::invalid_attribute_option_identifier(),
    )
}

/// An `isize` from the ABI narrowed to the field's own width.
///
/// A value that does not fit was never a legal setting for the field, so it is
/// `HY092` rather than a silent truncation.
fn narrow_i16(value: isize, field: Desc) -> Result<i16, OdbcError> {
    i16::try_from(value).map_err(|_| wrong_value(field))
}

/// [`narrow_i16`], for the two `SQLINTEGER` fields.
fn narrow_i32(value: isize, field: Desc) -> Result<i32, OdbcError> {
    i32::try_from(value).map_err(|_| wrong_value(field))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The spec's "Initialization of Descriptor Fields" table, for the fields
    /// it gives a concrete default rather than "ND". A record that starts life
    /// with anything else would hand `SQLGetDescField` a value the application
    /// never set and the spec never promised.
    #[test]
    fn a_new_record_carries_the_specs_defaults() {
        let record = DescriptorRecord::default();

        assert_eq!(
            record.concise_type,
            CDataType::Default as i16,
            "SQL_DESC_CONCISE_TYPE defaults to SQL_C_DEFAULT for an ARD and APD"
        );
        assert_eq!(
            record.verbose_type,
            CDataType::Default as i16,
            "SQL_DESC_TYPE defaults to SQL_C_DEFAULT for an ARD and APD"
        );
        assert!(
            record.data_ptr.is_null(),
            "SQL_DESC_DATA_PTR defaults to null"
        );
        assert!(
            record.indicator_ptr.is_null(),
            "SQL_DESC_INDICATOR_PTR defaults to null"
        );
        assert!(
            record.octet_length_ptr.is_null(),
            "SQL_DESC_OCTET_LENGTH_PTR defaults to null"
        );
        assert_eq!(
            record.parameter_type,
            ParamType::Input,
            "SQL_DESC_PARAMETER_TYPE defaults to SQL_PARAM_INPUT"
        );
    }

    /// A record exists as soon as any field is set, so "the key is present"
    /// stops meaning "bound". The spec makes a null `SQL_DESC_DATA_PTR` the
    /// unbind, and that is the only test of boundness Task 2 leaves standing.
    #[test]
    fn a_record_is_bound_only_when_its_data_pointer_is_set() {
        let mut record = DescriptorRecord::default();
        assert!(!record.is_bound(), "a defaulted record is not a binding");

        let mut buf: i64 = 0;
        record.data_ptr = std::ptr::from_mut(&mut buf).cast::<c_void>();
        assert!(
            record.is_bound(),
            "a record with a data pointer is a binding"
        );

        record.data_ptr = std::ptr::null_mut();
        assert!(!record.is_bound(), "a null data pointer unbinds");
    }

    /// `SQL_DESC_CONCISE_TYPE` is one field serving two readings: a C type on
    /// the ARD and APD, a SQL type on the IPD. Storing it raw and converting at
    /// the point of use is what lets one record type serve both roles.
    #[test]
    fn concise_type_reads_as_a_c_type_or_a_sql_type() {
        let as_c = DescriptorRecord {
            concise_type: CDataType::SBigInt as i16,
            ..Default::default()
        };
        assert_eq!(
            as_c.c_type().expect("SQL_C_SBIGINT is a C type"),
            CDataType::SBigInt
        );

        let as_sql = DescriptorRecord {
            concise_type: SqlDataType::INTEGER.0,
            ..Default::default()
        };
        assert_eq!(as_sql.sql_type(), SqlDataType::INTEGER);
    }

    /// An unrecognised concise type is `HY003`, the same code `SQLBindCol`
    /// already returns for one — not a panic and not a silent default, because
    /// `SQLSetDescField` can put an arbitrary i16 here.
    #[test]
    fn an_unknown_concise_type_reports_hy003() {
        let record = DescriptorRecord {
            concise_type: 31337,
            ..Default::default()
        };

        let err = record.c_type().expect_err("31337 is not a C data type");
        assert_eq!(
            err.sqlstate().as_str(),
            crate::types::sql_state::INVALID_APPLICATION_BUFFER_TYPE
        );
    }

    /// The spec's initialization table is a matrix, not a list: the same
    /// identifier is read/write on one descriptor, read-only on another and
    /// undefined on a third. `HY091` is decided from this, so a wrong cell is a
    /// wrong SQLSTATE.
    #[test]
    fn field_access_follows_the_specs_initialization_table() {
        use DescriptorRole::{Apd, Ard, Ipd, Ird};

        // SQL_DESC_DATA_PTR: R/W on the application descriptors, undefined on
        // the implementation ones.
        assert_eq!(field_access(Ard, Desc::DataPtr), FieldAccess::ReadWrite);
        assert_eq!(field_access(Apd, Desc::DataPtr), FieldAccess::ReadWrite);
        assert_eq!(field_access(Ird, Desc::DataPtr), FieldAccess::Undefined);

        // SQL_DESC_PARAMETER_TYPE: the IPD's alone.
        assert_eq!(
            field_access(Ipd, Desc::ParameterType),
            FieldAccess::ReadWrite
        );
        assert_eq!(
            field_access(Ard, Desc::ParameterType),
            FieldAccess::Undefined
        );

        // SQL_DESC_NAME: read-only on the IRD, writable on the IPD, undefined
        // on the application descriptors.
        assert_eq!(field_access(Ird, Desc::Name), FieldAccess::ReadOnly);
        assert_eq!(field_access(Ipd, Desc::Name), FieldAccess::ReadWrite);
        assert_eq!(field_access(Ard, Desc::Name), FieldAccess::Undefined);

        // The IRD's record fields are read-only across the board.
        assert_eq!(field_access(Ird, Desc::Nullable), FieldAccess::ReadOnly);
        assert_eq!(field_access(Ird, Desc::TypeName), FieldAccess::ReadOnly);
    }

    /// The whole matrix, in the shape the spec prints it: one row per field,
    /// four columns for ARD, APD, IRD and IPD.
    ///
    /// [`field_access`] is a `match` whose arms group fields that share a row,
    /// which is compact but is *not* the shape of the page it was transcribed
    /// from. This is, so a reviewer can read the two side by side. A cell that
    /// disagrees is a wrong SQLSTATE for that field on that descriptor.
    #[test]
    fn every_cell_of_the_initialization_table_is_transcribed() {
        use DescriptorRole::{Apd, Ard, Ipd, Ird};
        use FieldAccess::{ReadOnly as R, ReadWrite as RW, Undefined as U};

        // (field, [ARD, APD, IRD, IPD])
        let table: &[(Desc, [FieldAccess; 4])] = &[
            // Header fields.
            (Desc::AllocType, [R, R, R, R]),
            (Desc::ArraySize, [RW, RW, U, U]),
            (Desc::ArrayStatusPtr, [RW, RW, RW, RW]),
            (Desc::BindOffsetPtr, [RW, RW, U, U]),
            (Desc::BindType, [RW, RW, U, U]),
            (Desc::Count, [RW, RW, R, RW]),
            (Desc::RowsProcessedPtr, [U, U, RW, RW]),
            // Record fields.
            (Desc::ConciseType, [RW, RW, R, RW]),
            (Desc::Type, [RW, RW, R, RW]),
            // The IPD's is writable only as a consistency-check trigger; see
            // `field_access`'s note on the arm.
            (Desc::DataPtr, [RW, RW, U, RW]),
            (Desc::IndicatorPtr, [RW, RW, U, U]),
            (Desc::OctetLengthPtr, [RW, RW, U, U]),
            (Desc::OctetLength, [RW, RW, R, RW]),
            (Desc::Length, [RW, RW, R, RW]),
            (Desc::Precision, [RW, RW, R, RW]),
            (Desc::Scale, [RW, RW, R, RW]),
            (Desc::DatetimeIntervalCode, [RW, RW, R, RW]),
            (Desc::DatetimeIntervalPrecision, [RW, RW, R, RW]),
            (Desc::NumPrecRadix, [RW, RW, R, RW]),
            (Desc::ParameterType, [U, U, U, RW]),
            (Desc::Name, [U, U, R, RW]),
            (Desc::Unnamed, [U, U, R, RW]),
            (Desc::Nullable, [U, U, R, R]),
            (Desc::RowVer, [U, U, R, R]),
            // The IRD's read-only metadata. The last five are footnote [1]'s,
            // undefined on the IPD because core does not auto-populate it.
            (Desc::AutoUniqueValue, [U, U, R, U]),
            (Desc::BaseColumnName, [U, U, R, U]),
            (Desc::BaseTableName, [U, U, R, U]),
            (Desc::CatalogName, [U, U, R, U]),
            (Desc::DisplaySize, [U, U, R, U]),
            (Desc::Label, [U, U, R, U]),
            (Desc::LiteralPrefix, [U, U, R, U]),
            (Desc::LiteralSuffix, [U, U, R, U]),
            (Desc::SchemaName, [U, U, R, U]),
            (Desc::Searchable, [U, U, R, U]),
            (Desc::TableName, [U, U, R, U]),
            (Desc::Updatable, [U, U, R, U]),
            (Desc::CaseSensitive, [U, U, R, U]),
            (Desc::FixedPrecScale, [U, U, R, U]),
            (Desc::LocalTypeName, [U, U, R, U]),
            (Desc::TypeName, [U, U, R, U]),
            (Desc::Unsigned, [U, U, R, U]),
            // In `sqlext.h` and so in `odbc-sys`, but in neither of the spec's
            // tables: they describe a type, not a descriptor record.
            (Desc::MaximumScale, [U, U, U, U]),
            (Desc::MinimumScale, [U, U, U, U]),
        ];

        for (field, expected) in table {
            for (role, expected) in [Ard, Apd, Ird, Ipd].into_iter().zip(expected) {
                assert_eq!(
                    field_access(role, *field),
                    *expected,
                    "{field:?} on the {role:?}"
                );
            }
        }
    }

    /// Footnote [1] of the initialization table: these five "are defined only
    /// when the IPD is automatically populated by the driver. If not, they are
    /// undefined. If an application attempts to set these fields, SQLSTATE
    /// HY091 ... will be returned."
    ///
    /// Core reports `SQL_ATTR_AUTO_IPD` as `SQL_FALSE` and refuses
    /// `SQL_ATTR_ENABLE_AUTO_IPD = SQL_TRUE` with `HYC00`, so its IPD is not
    /// automatically populated and the footnote applies. This is a consequence
    /// of an answer core already gives, not a choice.
    #[test]
    fn the_auto_populated_ipd_fields_are_undefined_because_core_does_not_populate_it() {
        for field in [
            Desc::CaseSensitive,
            Desc::FixedPrecScale,
            Desc::LocalTypeName,
            Desc::TypeName,
            Desc::Unsigned,
        ] {
            assert_eq!(
                field_access(DescriptorRole::Ipd, field),
                FieldAccess::Undefined,
                "{field:?} is defined on the IPD only when the driver auto-populates it"
            );
        }
    }

    /// Round-trip: what `SQLSetDescField` writes, `SQLGetDescField` reads back.
    #[test]
    fn a_written_field_reads_back() {
        let mut record = DescriptorRecord::default();

        set_record_field(
            &mut record,
            DescriptorRole::Ard,
            Desc::ConciseType,
            DescFieldValue::Numeric(CDataType::SBigInt as isize),
        )
        .expect("SQL_DESC_CONCISE_TYPE is writable on an ARD");

        assert_eq!(
            get_record_field(&record, DescriptorRole::Ard, Desc::ConciseType)
                .expect("and readable"),
            DescFieldValue::Numeric(CDataType::SBigInt as isize)
        );
        assert_eq!(record.concise_type, CDataType::SBigInt as i16);
    }

    /// A write to a field this role does not define is `HY091`, and so is a
    /// write to a read-only one. The spec's `HY091` row names both: "The
    /// FieldIdentifier argument was invalid for the DescriptorHandle argument.
    /// The FieldIdentifier argument was a read-only, ODBC-defined field."
    #[test]
    fn setting_an_undefined_or_read_only_field_reports_hy091() {
        let mut record = DescriptorRecord::default();

        let err = set_record_field(
            &mut record,
            DescriptorRole::Ard,
            Desc::ParameterType,
            DescFieldValue::Numeric(ParamType::Input as isize),
        )
        .expect_err("SQL_DESC_PARAMETER_TYPE is undefined on an ARD");
        assert_eq!(
            err.sqlstate().as_str(),
            crate::types::sql_state::INVALID_DESCRIPTOR_FIELD_IDENTIFIER
        );

        let err = set_record_field(
            &mut record,
            DescriptorRole::Ird,
            Desc::Nullable,
            DescFieldValue::Numeric(0),
        )
        .expect_err("SQL_DESC_NULLABLE is read-only on an IRD");
        assert_eq!(
            err.sqlstate().as_str(),
            crate::types::sql_state::INVALID_DESCRIPTOR_FIELD_IDENTIFIER
        );
    }

    /// `SQLSetDescRec`'s "Consistency Checks": "If the SQL_DESC_TYPE field
    /// indicates a numeric type, the SQL_DESC_PRECISION and SQL_DESC_SCALE
    /// fields are verified to be valid."
    #[test]
    fn a_scale_larger_than_the_precision_is_inconsistent() {
        let record = DescriptorRecord {
            concise_type: SqlDataType::DECIMAL.0,
            precision: 5,
            scale: 9,
            data_ptr: std::ptr::dangling_mut(),
            ..Default::default()
        };

        let err = consistency_check(&record, DescriptorRole::Ipd)
            .expect_err("DECIMAL(5,9) has more scale than precision");

        assert_eq!(
            err.sqlstate().as_str(),
            crate::types::sql_state::INCONSISTENT_DESCRIPTOR_INFORMATION
        );
    }

    /// "The SQL_DESC_TYPE field must be one of the valid ODBC C or SQL types or
    /// a driver-specific SQL type." An ARD's is a C type, and 31337 is not one.
    #[test]
    fn an_unknown_c_type_on_an_ard_is_inconsistent() {
        let record = DescriptorRecord {
            concise_type: 31337,
            data_ptr: std::ptr::dangling_mut(),
            ..Default::default()
        };

        let err = consistency_check(&record, DescriptorRole::Ard)
            .expect_err("31337 is not a C data type");

        assert_eq!(
            err.sqlstate().as_str(),
            crate::types::sql_state::INCONSISTENT_DESCRIPTOR_INFORMATION
        );
    }

    /// "If the SQL_DESC_TYPE record field is SQL_DATETIME or SQL_INTERVAL, the
    /// SQL_DESC_DATETIME_INTERVAL_CODE field must be one of the valid datetime
    /// or interval codes." Core supports no interval types, so the reduction
    /// this check makes is to reject an interval code on a type that is not an
    /// interval — which is the half of the clause core can act on.
    #[test]
    fn a_datetime_interval_code_on_a_plain_type_is_inconsistent() {
        let record = DescriptorRecord {
            concise_type: SqlDataType::INTEGER.0,
            datetime_interval_code: crate::types::SQL_CODE_DATE,
            data_ptr: std::ptr::dangling_mut(),
            ..Default::default()
        };

        let err = consistency_check(&record, DescriptorRole::Ipd)
            .expect_err("an INTEGER has no datetime/interval code");

        assert_eq!(
            err.sqlstate().as_str(),
            crate::types::sql_state::INCONSISTENT_DESCRIPTOR_INFORMATION
        );
    }

    /// A consistent record passes. Without this the check could reject
    /// everything and the three tests above would still be green.
    #[test]
    fn a_consistent_record_passes() {
        let record = DescriptorRecord {
            concise_type: SqlDataType::DECIMAL.0,
            precision: 9,
            scale: 2,
            data_ptr: std::ptr::dangling_mut(),
            ..Default::default()
        };

        consistency_check(&record, DescriptorRole::Ipd)
            .expect("DECIMAL(9,2) is a consistent record");
    }

    /// A numeric value handed to a string field, or the reverse, is `HY092`
    /// ("the value in *ValuePtr was not valid for the FieldIdentifier
    /// argument") rather than a silent coercion.
    #[test]
    fn a_value_of_the_wrong_shape_reports_hy092() {
        let mut record = DescriptorRecord::default();

        let err = set_record_field(
            &mut record,
            DescriptorRole::Ipd,
            Desc::Name,
            DescFieldValue::Numeric(7),
        )
        .expect_err("SQL_DESC_NAME is a string field");

        assert_eq!(
            err.sqlstate().as_str(),
            crate::types::sql_state::INVALID_ATTRIBUTE_OPTION_IDENTIFIER
        );
    }
}
