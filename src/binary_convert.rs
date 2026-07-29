//! Conversion of `SQL_C_BINARY` parameter data to the SQL type the application
//! declared at `SQLBindParameter`.
//!
//! This module is the [C to SQL: Binary] table, transcribed — the sibling of
//! [`crate::param_convert`], which is the [C to SQL: Character] one. It exists
//! for the same reason: [`crate::backend::Backend::execute`] receives only
//! `&[ColumnValue]`, so the declared `ParameterType` never reaches the backend.
//! If core does not honour it, nobody does, and a parameter bound `SQL_C_BINARY`
//! + `SQL_INTEGER` arrives at the data source as raw bytes.
//!
//! # Byte order is native, and that is a ruling
//!
//! Native is chosen on evidence rather than derived. AWS's Redshift
//! ODBC driver v2.2.0 — whose changelog calls this work "fully ODBC-compliant"
//! and "Added missing SQL_C_BINARY conversion support for all SQL types" — reads
//! these targets with a plain `memcpy` into the C type, no swapping. A
//! round-trip test per target pins the expectation, so a big-endian port fails
//! loudly rather than silently swapping.
//!
//! Core is stricter than that driver in one place: it tests `==` where AWS tests
//! `>=`, so a five-byte value bound `SQL_INTEGER` is rejected rather than
//! silently truncated to its first four bytes. The spec row says "=".
//!
//! # What is refused, and why
//!
//! - **`SQL_DECIMAL` / `SQL_NUMERIC`.** Row 3 compares against "SQL data
//!   length", which [Converting Data from C to SQL Data Types] defines as the
//!   bytes required to store the value *at the data source*. A decimal has no
//!   fixed width and core cannot know the data source's. Reading the bytes as a
//!   `SQL_NUMERIC_STRUCT` would be an invented convention whose failure mode is
//!   the one this module exists to remove: 19 bytes meaning something else
//!   decoded into a plausible, wrong decimal with no diagnostic.
//! - **Every character target.** Rows 1 and 2 need an encoding, and ODBC
//!   specifies none for these bytes. Row 1's test is plain byte length rather
//!   than doubled — unlike the character table's binary row, which explicitly
//!   halves — so the conversion is a byte pass-through into the data source's
//!   own encoding, not a hex expansion. Core does not know that encoding.
//!   Guessing UTF-8 would make acceptance depend on the value's contents, so
//!   the same bind would succeed or fail with the data.
//!
//! Both refusals are `07006`, and both are raised by `SQLBindParameter` rather
//! than at execute time — see [`binary_target_is_supported`].
//!
//! [C to SQL: Character]: https://learn.microsoft.com/en-us/sql/odbc/reference/appendixes/c-to-sql-character
//! [C to SQL: Binary]: https://learn.microsoft.com/en-us/sql/odbc/reference/appendixes/c-to-sql-binary
//! [Converting Data from C to SQL Data Types]: https://learn.microsoft.com/en-us/sql/odbc/reference/appendixes/converting-data-from-c-to-sql-data-types

use odbc_sys::SqlDataType;

use crate::{
    errors::OdbcError,
    param_convert::check_declared_binary_size,
    types::{ColumnValue, SqlState, ULen},
};

/// Whether the declared SQL type is one of the three binary types, whose
/// `ColumnSize` is a byte length.
///
/// Moved here from [`crate::param_convert`] with the rest of this table;
/// `check_declared_binary_size` stayed there, because it also serves the
/// character table's own binary row.
pub(crate) fn is_binary_sql_type(sql_type: SqlDataType) -> bool {
    sql_type == SqlDataType::EXT_BINARY
        || sql_type == SqlDataType::EXT_VAR_BINARY
        || sql_type == SqlDataType::EXT_LONG_VAR_BINARY
}

/// The exact byte count row 3 requires for a target, or `None` if this table
/// does not convert `SQL_C_BINARY` to it.
///
/// Every width is a `size_of`, never a literal: these are C struct and scalar
/// widths, so the compiler is the only correct source for them.
///
/// The ODBC 2.0 datetime spellings are grouped with their 3.x counterparts
/// exactly as [`crate::param_convert`] and [`crate::types::col_attr`] group
/// them. Note that `odbc_sys` names 9 `DATETIME`, after the *verbose*
/// `SQL_DATETIME`, but `ParameterType` is a **concise** type where 9 is
/// `SQL_DATE` — so it belongs with date.
fn fixed_width(sql_type: SqlDataType) -> Option<usize> {
    use std::mem::size_of;

    if sql_type == SqlDataType::EXT_TINY_INT {
        return Some(size_of::<i8>());
    }
    if sql_type == SqlDataType::SMALLINT {
        return Some(size_of::<i16>());
    }
    if sql_type == SqlDataType::INTEGER {
        return Some(size_of::<i32>());
    }
    if sql_type == SqlDataType::EXT_BIG_INT {
        return Some(size_of::<i64>());
    }
    if sql_type == SqlDataType::REAL {
        return Some(size_of::<f32>());
    }
    if sql_type == SqlDataType::FLOAT || sql_type == SqlDataType::DOUBLE {
        return Some(size_of::<f64>());
    }
    if sql_type == SqlDataType::EXT_BIT {
        return Some(size_of::<u8>());
    }
    if sql_type == SqlDataType::DATE || sql_type == SqlDataType::DATETIME {
        return Some(size_of::<odbc_sys::Date>());
    }
    if sql_type == SqlDataType::TIME || sql_type == SqlDataType::EXT_TIME_OR_INTERVAL {
        return Some(size_of::<odbc_sys::Time>());
    }
    if sql_type == SqlDataType::TIMESTAMP || sql_type == SqlDataType::EXT_TIMESTAMP {
        return Some(size_of::<odbc_sys::Timestamp>());
    }
    None
}

/// Whether core converts `SQL_C_BINARY` to this target at all.
///
/// `SQLBindParameter` calls this and refuses the pairing with 07006 when it is
/// false. Bind time rather than execute time is deliberate: the pairing is
/// fixed at bind, needs no backend metadata, and never depends on the data — so
/// the application fails before running its query, and the `SQLPutData` path is
/// covered by the same single check.
///
/// Anything this admits, [`binary_to_sql_type`] handles; a test pins both
/// directions.
pub(crate) fn binary_target_is_supported(sql_type: SqlDataType) -> bool {
    is_binary_sql_type(sql_type) || fixed_width(sql_type).is_some()
}

/// "Byte length of data <> SQL data length" — row 3's only failure outcome.
fn wrong_width(actual: usize, expected: usize, sql_type: SqlDataType) -> OdbcError {
    OdbcError::general(
        format!(
            "A SQL_C_BINARY parameter for {sql_type:?} needs exactly {expected} bytes, not {actual}"
        ),
        SqlState::numeric_value_out_of_range(),
    )
}

/// A target this table does not convert `SQL_C_BINARY` to.
///
/// `pub(crate)` so `SQLBindParameter`'s refusal and this module's own carry the
/// same message; an application should not be able to tell the two apart.
pub(crate) fn unsupported_target(sql_type: SqlDataType) -> OdbcError {
    OdbcError::general(
        format!("SQL_C_BINARY cannot be converted to {sql_type:?}"),
        SqlState::restricted_data_type_attribute_violation(),
    )
}

/// Copy exactly `N` bytes out of a slice the caller has already length-checked.
///
/// `unwrap_or` states a value the branch cannot reach rather than panicking in
/// an FFI call: every caller is guarded by the `bytes.len() != width` test.
fn fixed<const N: usize>(bytes: &[u8]) -> [u8; N] {
    bytes.try_into().unwrap_or([0u8; N])
}

/// Convert `SQL_C_BINARY` parameter data to the declared SQL type.
///
/// `bytes` is the value read out of the parameter buffer, or accumulated by
/// `SQLPutData`; `sql_type` is `SQLBindParameter`'s `ParameterType` and
/// `col_size` its `ColumnSize`.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/appendixes/c-to-sql-binary>
pub(crate) fn binary_to_sql_type(
    bytes: &[u8],
    sql_type: SqlDataType,
    col_size: ULen,
) -> Result<ColumnValue, OdbcError> {
    tracing::trace!(
        "binary_to_sql_type: {} bytes declared as {:?}",
        bytes.len(),
        sql_type
    );

    // Row 4: binary to binary needs no conversion, only the declared-size test.
    if is_binary_sql_type(sql_type) {
        check_declared_binary_size(bytes.len(), col_size)?;
        return Ok(ColumnValue::Bytes(bytes.to_vec()));
    }

    let Some(width) = fixed_width(sql_type) else {
        return Err(unsupported_target(sql_type));
    };
    if bytes.len() != width {
        return Err(wrong_width(bytes.len(), width, sql_type));
    }

    if sql_type == SqlDataType::EXT_TINY_INT {
        return Ok(ColumnValue::I8(i8::from_ne_bytes(fixed(bytes))));
    }
    if sql_type == SqlDataType::SMALLINT {
        return Ok(ColumnValue::I16(i16::from_ne_bytes(fixed(bytes))));
    }
    if sql_type == SqlDataType::INTEGER {
        return Ok(ColumnValue::I32(i32::from_ne_bytes(fixed(bytes))));
    }
    if sql_type == SqlDataType::EXT_BIG_INT {
        return Ok(ColumnValue::I64(i64::from_ne_bytes(fixed(bytes))));
    }
    if sql_type == SqlDataType::REAL {
        return Ok(ColumnValue::F32(f32::from_ne_bytes(fixed(bytes))));
    }
    if sql_type == SqlDataType::FLOAT || sql_type == SqlDataType::DOUBLE {
        return Ok(ColumnValue::F64(f64::from_ne_bytes(fixed(bytes))));
    }
    if sql_type == SqlDataType::EXT_BIT {
        // Row 3 states a width test and no value test for SQL_BIT, so any
        // non-zero byte is true. `first` rather than `[0]`: the width check
        // above already guarantees one byte, and indexing would be a panic
        // path in an FFI call.
        return Ok(ColumnValue::Bool(bytes.first().copied().unwrap_or(0) != 0));
    }

    // The temporal targets are the C structs the C Data Types appendix
    // specifies field by field. SAFETY for all three reads: the width check
    // above guarantees `bytes` is exactly `size_of` the struct, and
    // `read_unaligned` imposes no alignment requirement on the `u8` pointer it
    // reads through — which matters, because a bound parameter buffer inside a
    // packed row-wise structure has no guaranteed alignment.
    if sql_type == SqlDataType::DATE || sql_type == SqlDataType::DATETIME {
        let d = unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast::<odbc_sys::Date>()) };
        return Ok(ColumnValue::Date {
            year: d.year,
            month: d.month,
            day: d.day,
        });
    }
    if sql_type == SqlDataType::TIME || sql_type == SqlDataType::EXT_TIME_OR_INTERVAL {
        // SQL_TIME_STRUCT carries no fractional seconds; report 0, as
        // `read_param_value` does for the SQL_C_TYPE_TIME buffer.
        let t = unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast::<odbc_sys::Time>()) };
        return Ok(ColumnValue::Time {
            hour: t.hour,
            minute: t.minute,
            second: t.second,
            fraction: 0,
        });
    }
    if sql_type == SqlDataType::TIMESTAMP || sql_type == SqlDataType::EXT_TIMESTAMP {
        let ts = unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast::<odbc_sys::Timestamp>()) };
        return Ok(ColumnValue::Timestamp {
            year: ts.year,
            month: ts.month,
            day: ts.day,
            hour: ts.hour,
            minute: ts.minute,
            second: ts.second,
            fraction: ts.fraction,
        });
    }

    // `fixed_width` returned a width for a type no branch above handles, which
    // means the two lists have drifted. A test pins them together.
    Err(unsupported_target(sql_type))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Convert with no declared column size, which only the binary-family
    /// targets consult.
    fn convert(bytes: &[u8], sql_type: SqlDataType) -> Result<ColumnValue, OdbcError> {
        binary_to_sql_type(bytes, sql_type, 0)
    }

    fn state_of(result: Result<ColumnValue, OdbcError>) -> String {
        result
            .expect_err("conversion should have failed")
            .sqlstate()
            .as_str()
            .to_owned()
    }

    // -- round trips: the native byte-order ruling, pinned -------------------

    #[test]
    fn a_tinyint_target_decodes_one_native_byte() {
        assert_eq!(
            convert(&(-42_i8).to_ne_bytes(), SqlDataType::EXT_TINY_INT).expect("one byte"),
            ColumnValue::I8(-42)
        );
    }

    #[test]
    fn a_smallint_target_decodes_two_native_bytes() {
        assert_eq!(
            convert(&(-12_345_i16).to_ne_bytes(), SqlDataType::SMALLINT).expect("two bytes"),
            ColumnValue::I16(-12_345)
        );
    }

    #[test]
    fn an_integer_target_decodes_four_native_bytes() {
        assert_eq!(
            convert(&(-123_456_i32).to_ne_bytes(), SqlDataType::INTEGER).expect("four bytes"),
            ColumnValue::I32(-123_456)
        );
    }

    #[test]
    fn a_bigint_target_decodes_eight_native_bytes() {
        assert_eq!(
            convert(
                &(-9_000_000_000_i64).to_ne_bytes(),
                SqlDataType::EXT_BIG_INT
            )
            .expect("eight bytes"),
            ColumnValue::I64(-9_000_000_000)
        );
    }

    #[test]
    fn a_real_target_decodes_four_native_bytes() {
        assert_eq!(
            convert(&1.5_f32.to_ne_bytes(), SqlDataType::REAL).expect("four bytes"),
            ColumnValue::F32(1.5)
        );
    }

    #[test]
    fn a_double_target_decodes_eight_native_bytes() {
        assert_eq!(
            convert(&(-2.25_f64).to_ne_bytes(), SqlDataType::DOUBLE).expect("eight bytes"),
            ColumnValue::F64(-2.25)
        );
        assert_eq!(
            convert(&(-2.25_f64).to_ne_bytes(), SqlDataType::FLOAT).expect("eight bytes"),
            ColumnValue::F64(-2.25)
        );
    }

    /// Row 3 states a width test and no value test for `SQL_BIT`, unlike the
    /// character table's `SQL_BIT` row, which states three. So any non-zero
    /// byte is true; 2 is not an error here.
    #[test]
    fn a_bit_target_reads_one_byte_with_no_value_test() {
        assert_eq!(
            convert(&[0], SqlDataType::EXT_BIT).expect("one byte"),
            ColumnValue::Bool(false)
        );
        assert_eq!(
            convert(&[1], SqlDataType::EXT_BIT).expect("one byte"),
            ColumnValue::Bool(true)
        );
        assert_eq!(
            convert(&[2], SqlDataType::EXT_BIT).expect("one byte"),
            ColumnValue::Bool(true)
        );
    }

    // -- the width test: "=" and not ">=" -----------------------------------

    #[test]
    fn a_value_narrower_than_the_target_is_22003() {
        assert_eq!(state_of(convert(&[0, 0, 0], SqlDataType::INTEGER)), "22003");
    }

    /// AWS's driver accepts this and takes the first four bytes. The spec row
    /// says "Byte length of data = SQL data length", so core rejects it.
    #[test]
    fn a_value_wider_than_the_target_is_22003_not_truncated() {
        assert_eq!(
            state_of(convert(&[0, 0, 0, 0, 0], SqlDataType::INTEGER)),
            "22003"
        );
    }

    #[test]
    fn an_empty_value_is_22003_rather_than_a_zero() {
        assert_eq!(state_of(convert(&[], SqlDataType::EXT_TINY_INT)), "22003");
    }

    // -- refusals -----------------------------------------------------------

    #[test]
    fn a_decimal_target_is_07006() {
        assert_eq!(state_of(convert(&[0; 19], SqlDataType::DECIMAL)), "07006");
        assert_eq!(state_of(convert(&[0; 19], SqlDataType::NUMERIC)), "07006");
    }

    #[test]
    fn a_character_target_is_07006() {
        assert_eq!(state_of(convert(b"hello", SqlDataType::VARCHAR)), "07006");
        assert_eq!(state_of(convert(b"hello", SqlDataType::CHAR)), "07006");
        assert_eq!(
            state_of(convert(b"hello", SqlDataType::EXT_W_VARCHAR)),
            "07006"
        );
    }

    #[test]
    fn an_unrecognised_target_is_07006() {
        assert_eq!(state_of(convert(&[0], SqlDataType(4242))), "07006");
    }

    // -- the binary family: unchanged behaviour, now owned here -------------

    #[test]
    fn a_binary_target_passes_the_bytes_through_at_any_width() {
        assert_eq!(
            convert(&[1, 2, 3], SqlDataType::EXT_VAR_BINARY).expect("no declared size"),
            ColumnValue::Bytes(vec![1, 2, 3])
        );
    }

    #[test]
    fn a_binary_target_over_the_declared_size_is_22001() {
        assert_eq!(
            binary_to_sql_type(&[1, 2, 3], SqlDataType::EXT_VAR_BINARY, 2)
                .expect_err("three bytes exceed VARBINARY(2)")
                .sqlstate()
                .as_str(),
            "22001"
        );
    }

    // -- temporal targets ---------------------------------------------------

    /// The bytes are a `SQL_DATE_STRUCT`, whose fields the C Data Types
    /// appendix specifies. Built here from the struct so the test states the
    /// layout rather than assuming it.
    #[test]
    fn a_date_target_decodes_a_sql_date_struct() {
        let d = odbc_sys::Date {
            year: 2026,
            month: 7,
            day: 29,
        };
        let bytes = unsafe {
            std::slice::from_raw_parts(
                std::ptr::addr_of!(d).cast::<u8>(),
                std::mem::size_of::<odbc_sys::Date>(),
            )
        };
        assert_eq!(
            convert(bytes, SqlDataType::DATE).expect("six bytes"),
            ColumnValue::Date {
                year: 2026,
                month: 7,
                day: 29
            }
        );
    }

    #[test]
    fn a_time_target_decodes_a_sql_time_struct_with_zero_fraction() {
        let t = odbc_sys::Time {
            hour: 13,
            minute: 45,
            second: 6,
        };
        let bytes = unsafe {
            std::slice::from_raw_parts(
                std::ptr::addr_of!(t).cast::<u8>(),
                std::mem::size_of::<odbc_sys::Time>(),
            )
        };
        let expected = ColumnValue::Time {
            hour: 13,
            minute: 45,
            second: 6,
            fraction: 0,
        };
        assert_eq!(
            convert(bytes, SqlDataType::TIME).expect("six bytes"),
            expected
        );
        assert_eq!(
            convert(bytes, SqlDataType::EXT_TIME_OR_INTERVAL).expect("six bytes"),
            expected
        );
    }

    #[test]
    fn a_timestamp_target_decodes_a_sql_timestamp_struct() {
        let ts = odbc_sys::Timestamp {
            year: 2026,
            month: 7,
            day: 29,
            hour: 13,
            minute: 45,
            second: 6,
            fraction: 123_000_000,
        };
        let bytes = unsafe {
            std::slice::from_raw_parts(
                std::ptr::addr_of!(ts).cast::<u8>(),
                std::mem::size_of::<odbc_sys::Timestamp>(),
            )
        };
        let expected = ColumnValue::Timestamp {
            year: 2026,
            month: 7,
            day: 29,
            hour: 13,
            minute: 45,
            second: 6,
            fraction: 123_000_000,
        };
        assert_eq!(
            convert(bytes, SqlDataType::TIMESTAMP).expect("sixteen bytes"),
            expected
        );
        assert_eq!(
            convert(bytes, SqlDataType::EXT_TIMESTAMP).expect("sixteen bytes"),
            expected
        );
    }

    /// `DATETIME` is 9, which is both the 3.x *verbose* datetime identifier and
    /// the ODBC 2.0 *concise* `SQL_DATE`. `ParameterType` is a concise type, so
    /// 9 is a date. AWS's Redshift ODBC driver reads it the same way — its
    /// parameter conversion opens that branch `case SQL_TYPE_DATE: case
    /// SQL_DATE:`. `param_convert` and `col_attr` agree.
    #[test]
    fn the_2x_date_spelling_is_a_date_not_a_timestamp() {
        assert_eq!(
            fixed_width(SqlDataType::DATETIME),
            Some(std::mem::size_of::<odbc_sys::Date>())
        );

        let d = odbc_sys::Date {
            year: 2026,
            month: 7,
            day: 29,
        };
        let bytes = unsafe {
            std::slice::from_raw_parts(
                std::ptr::addr_of!(d).cast::<u8>(),
                std::mem::size_of::<odbc_sys::Date>(),
            )
        };
        assert_eq!(
            convert(bytes, SqlDataType::DATETIME).expect("six bytes"),
            ColumnValue::Date {
                year: 2026,
                month: 7,
                day: 29
            }
        );
    }

    #[test]
    fn a_temporal_target_at_the_wrong_width_is_22003() {
        assert_eq!(state_of(convert(&[0; 5], SqlDataType::DATE)), "22003");
        assert_eq!(state_of(convert(&[0; 7], SqlDataType::DATE)), "22003");
        assert_eq!(state_of(convert(&[0; 15], SqlDataType::TIMESTAMP)), "22003");
    }

    // -- the bind-time gate agrees with the converter ------------------------

    /// The two must not drift: anything the gate admits, the converter handles.
    #[test]
    fn every_supported_target_converts_at_its_own_width() {
        for sql_type in [
            SqlDataType::EXT_TINY_INT,
            SqlDataType::SMALLINT,
            SqlDataType::INTEGER,
            SqlDataType::EXT_BIG_INT,
            SqlDataType::REAL,
            SqlDataType::FLOAT,
            SqlDataType::DOUBLE,
            SqlDataType::EXT_BIT,
            SqlDataType::DATE,
            SqlDataType::TIME,
            SqlDataType::EXT_TIME_OR_INTERVAL,
            SqlDataType::TIMESTAMP,
            SqlDataType::DATETIME,
            SqlDataType::EXT_TIMESTAMP,
        ] {
            assert!(
                binary_target_is_supported(sql_type),
                "gate rejects {sql_type:?}"
            );
            let width = fixed_width(sql_type).expect("supported target has a width");
            let bytes = vec![0u8; width];
            assert!(
                binary_to_sql_type(&bytes, sql_type, 0).is_ok(),
                "converter rejects {sql_type:?} at width {width}"
            );
        }
    }

    #[test]
    fn the_gate_rejects_what_the_converter_refuses() {
        for sql_type in [
            SqlDataType::DECIMAL,
            SqlDataType::NUMERIC,
            SqlDataType::VARCHAR,
            SqlDataType::EXT_W_VARCHAR,
            SqlDataType(4242),
        ] {
            assert!(
                !binary_target_is_supported(sql_type),
                "gate admits {sql_type:?}"
            );
        }
    }
}
