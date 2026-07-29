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

use std::ffi::c_void;

use odbc_sys::{CDataType, ParamType, SqlDataType};

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
// Six of these fields — the datetime/interval pair, `precision`,
// `num_prec_radix`, `name` and `unnamed` — have no reader until
// `SQLGetDescField` and the consistency check land. They are declared now
// because the field set is the spec's, not a subset of what today's callers
// happen to want, and a record built by `SQLSetDescField` can carry any of
// them. The allow comes off with the last of those readers.
#[allow(dead_code)]
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
    // Its callers are the boundness sites that still test `contains_key`; the
    // allow comes off when they move over.
    #[allow(dead_code)]
    pub fn is_bound(&self) -> bool {
        !self.data_ptr.is_null()
    }
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
}
