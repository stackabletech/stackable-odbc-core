//! Conversion of numeric parameter data to the SQL type the application
//! declared at `SQLBindParameter`.
//!
//! This module is the [C to SQL: Numeric] table, transcribed — the third
//! sibling of [`crate::param_convert`] (Character) and
//! [`crate::binary_convert`] (Binary). It exists for the reason they do:
//! [`crate::backend::Backend::execute`] receives only `&[ColumnValue]`, so the
//! declared `ParameterType` never reaches the backend. If core does not honour
//! it, nobody does, and a value bound `SQL_C_DOUBLE` + `SQL_VARCHAR(3)` reaches
//! the data source unchecked.
//!
//! # Not a model: what AWS's driver implements
//!
//! [`crate::binary_convert`] cites AWS's Redshift driver for byte order, a
//! question the spec does not answer. It is *not* a guide to coverage here: of
//! this table's six rows that driver implements the integer half of one, plus a
//! declared-precision check for `SQL_C_NUMERIC` to `DECIMAL`. Its `22015` sites
//! belong to the interval *source* table, not to this one. Core follows the
//! spec where the spec is clear, which for this table is everywhere.
//!
//! # `SQL_C_BIT` is not a source here
//!
//! It has its own *C to SQL: Bit* table. `SQL_BIT` appears below only as a
//! *target*, which is this table's fifth row.
//!
//! # Non-finite values are decided per target
//!
//! Row 4's test is "data is within the range of the data type to which the
//! number is being converted", and an IEEE-754 `f32`/`f64` represents NaN and
//! both infinities — so a float target accepts them. An integer, `SQL_BIT`,
//! `DECIMAL` or interval target cannot hold either, so those are `22003`. NaN
//! needs an explicit test wherever it is rejected: it compares false against
//! every bound, so `value < MIN || value > MAX` would let it through.
//!
//! A `SQL_C_FLOAT` or `SQL_C_DOUBLE` source is the only way a non-finite value
//! reaches this module: [`crate::param_convert::parse_numeric_literal`] rejects
//! `inf` and `NaN` outright, so a [`DecimalLiteral`] can never carry one.
//!
//! [C to SQL: Numeric]: https://learn.microsoft.com/en-us/sql/odbc/reference/appendixes/c-to-sql-numeric

// The table is built row by row, and `read_param_value` routes into it only once
// every row is present — a half-wired conversion would send some numeric
// parameters through the table and others past it, which is worse than none.
// Until that wiring lands nothing outside the tests calls in here.
//
// This allow must go away with that wiring. It is scoped to the module rather
// than to each item so that removing it fails loudly if anything is genuinely
// unreachable, instead of leaving a per-item allow behind to be inherited.
#![allow(dead_code)]

use odbc_sys::SqlDataType;

use crate::{
    errors::OdbcError,
    param_convert::{DecimalLiteral, check_declared_char_size},
    types::{ColumnValue, SqlState, ULen},
};

/// A numeric parameter, canonicalised from the table's fourteen C types.
///
/// The split is the spec's own: its interval footnote is stated in terms of the
/// "exact numeric" C types as against the "approximate" ones, and only exact
/// values convert to an interval at all.
pub(crate) enum NumericParam {
    /// Every integer C type and `SQL_C_NUMERIC`.
    ///
    /// Carried as a [`DecimalLiteral`] rather than an `i128` so a
    /// `DECIMAL(38,10)` parameter reaches the backend without passing through
    /// `f64`'s 53 bits of mantissa.
    Exact(DecimalLiteral),
    /// `SQL_C_FLOAT` and `SQL_C_DOUBLE`.
    Approx {
        /// The value, widened to `f64` if it arrived as `f32`.
        value: f64,
        /// Whether the source was `SQL_C_FLOAT` rather than `SQL_C_DOUBLE`.
        ///
        /// Rows 1 and 2 are what need this, not row 4. Rendering differs by
        /// source width: `1.15f32` widened to `f64` prints as
        /// `1.1499999761581421`, so a `SQL_C_FLOAT` bound to a `VARCHAR(6)`
        /// would be rejected for digits the application never supplied. The
        /// renderer must format at the source's own precision.
        single: bool,
    },
}

/// A converted parameter, and the optional warning the conversion raised.
///
/// The warning is the table's footnote [b]: "a driver may optionally return
/// SQL_SUCCESS_WITH_INFO and 01S07 when there is a fractional truncation". The
/// value is still sent — that is what makes it a warning rather than an error,
/// and why it cannot simply be an `Err`.
pub(crate) struct Converted {
    /// The value to hand the backend.
    pub value: ColumnValue,
    /// A diagnostic to post alongside it, without failing the call.
    pub warning: Option<OdbcError>,
}

impl Converted {
    /// A conversion that raised nothing.
    fn clean(value: ColumnValue) -> Self {
        Self {
            value,
            warning: None,
        }
    }
}

impl NumericParam {
    /// Render at the source's own precision.
    ///
    /// Rows 1 and 2 measure "number of digits ... including the minus sign,
    /// decimal point, and exponent", which is the length of this string.
    fn render(&self) -> String {
        match self {
            NumericParam::Exact(literal) => literal.to_decimal_string(),
            NumericParam::Approx {
                value,
                single: true,
            } => (*value as f32).to_string(),
            NumericParam::Approx {
                value,
                single: false,
            } => value.to_string(),
        }
    }
}

/// A target this table does not convert the given numeric C type to.
///
/// `pub(crate)` so `SQLBindParameter`'s refusal and this module's own carry the
/// same message; an application should not be able to tell the two apart.
pub(crate) fn unsupported_target(sql_type: SqlDataType) -> OdbcError {
    OdbcError::general(
        format!("A numeric parameter cannot be converted to {sql_type:?}"),
        SqlState::restricted_data_type_attribute_violation(),
    )
}

/// Convert a numeric parameter to the declared SQL type.
///
/// `col_size` is `SQLBindParameter`'s `ColumnSize`, `decimal_digits` its
/// `DecimalDigits`, and `interval_precision` the IPD's
/// `SQL_DESC_DATETIME_INTERVAL_PRECISION`.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/appendixes/c-to-sql-numeric>
pub(crate) fn numeric_to_sql_type(
    value: NumericParam,
    sql_type: SqlDataType,
    col_size: ULen,
    decimal_digits: i16,
    interval_precision: i32,
) -> Result<Converted, OdbcError> {
    tracing::trace!(
        "numeric_to_sql_type: declared as {:?}, column size {}, scale {}",
        sql_type,
        col_size,
        decimal_digits
    );

    // Rows 1 and 2. Both measure the rendered number; the narrow row counts
    // characters and the wide row UTF-16 code units, which is the only
    // difference between them.
    //
    // Core renders rather than passing the numeric value through, so the length
    // it checks is the value that is sent. Counting a canonical rendering and
    // then handing the backend the original number would let a backend that
    // renders `3.14` as `3.140000` put eight characters on the wire after a
    // check that passed four.
    if sql_type == SqlDataType::CHAR
        || sql_type == SqlDataType::VARCHAR
        || sql_type == SqlDataType::EXT_LONG_VARCHAR
    {
        let text = value.render();
        check_declared_char_size(text.chars().count(), col_size)?;
        return Ok(Converted::clean(ColumnValue::String(text)));
    }
    if sql_type == SqlDataType::EXT_W_CHAR
        || sql_type == SqlDataType::EXT_W_VARCHAR
        || sql_type == SqlDataType::EXT_W_LONG_VARCHAR
    {
        let text = value.render();
        check_declared_char_size(text.encode_utf16().count(), col_size)?;
        return Ok(Converted::clean(ColumnValue::String(text)));
    }

    // Rows 3 to 6 arrive in the following commits. Until then any other target
    // is refused rather than silently passed through.
    let _ = (decimal_digits, interval_precision);
    Err(unsupported_target(sql_type))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::param_convert::parse_numeric_literal;

    fn exact(text: &str) -> NumericParam {
        NumericParam::Exact(parse_numeric_literal(text).expect("a numeric literal"))
    }

    fn convert(
        v: NumericParam,
        sql_type: SqlDataType,
        col_size: ULen,
    ) -> Result<Converted, OdbcError> {
        numeric_to_sql_type(v, sql_type, col_size, 0, 0)
    }

    fn state_of(result: Result<Converted, OdbcError>) -> String {
        result
            .err()
            .expect("expected an error")
            .sqlstate()
            .as_str()
            .to_owned()
    }

    // -- row 1: narrow character targets ------------------------------------

    #[test]
    fn a_number_that_fits_a_varchar_is_rendered_as_a_string() {
        let out = convert(exact("3.14"), SqlDataType::VARCHAR, 10).expect("four characters fit");
        assert_eq!(out.value, ColumnValue::String("3.14".to_owned()));
        assert!(out.warning.is_none());
    }

    #[test]
    fn a_number_longer_than_the_declared_varchar_is_22001() {
        assert_eq!(
            state_of(convert(exact("3.14"), SqlDataType::VARCHAR, 3)),
            "22001"
        );
    }

    #[test]
    fn a_zero_column_size_disables_the_character_check() {
        assert!(convert(exact("123456789"), SqlDataType::CHAR, 0).is_ok());
    }

    /// The row counts "number of digits ... including the minus sign, decimal
    /// point, and exponent", so the sign is part of the length.
    #[test]
    fn the_minus_sign_counts_toward_the_declared_size() {
        assert_eq!(
            state_of(convert(exact("-1234"), SqlDataType::VARCHAR, 4)),
            "22001"
        );
        assert!(convert(exact("-1234"), SqlDataType::VARCHAR, 5).is_ok());
    }

    #[test]
    fn a_long_varchar_target_is_checked_like_a_varchar() {
        assert_eq!(
            state_of(convert(exact("12345"), SqlDataType::EXT_LONG_VARCHAR, 4)),
            "22001"
        );
    }

    // -- row 2: wide character targets --------------------------------------

    #[test]
    fn a_wide_character_target_counts_utf16_code_units() {
        let out = convert(exact("42"), SqlDataType::EXT_W_VARCHAR, 2).expect("two code units fit");
        assert_eq!(out.value, ColumnValue::String("42".to_owned()));
    }

    #[test]
    fn a_number_longer_than_the_declared_wvarchar_is_22001() {
        assert_eq!(
            state_of(convert(exact("12345"), SqlDataType::EXT_W_VARCHAR, 4)),
            "22001"
        );
    }

    // -- rendering: the source's own precision ------------------------------

    /// A `SQL_C_FLOAT` widened to `f64` prints as 1.1499999761581421. Rendering
    /// at the source's precision is what keeps a `VARCHAR(4)` bind working.
    #[test]
    fn a_single_precision_source_renders_at_single_precision() {
        let out = numeric_to_sql_type(
            NumericParam::Approx {
                value: f64::from(1.15_f32),
                single: true,
            },
            SqlDataType::VARCHAR,
            4,
            0,
            0,
        )
        .expect("1.15 is four characters");
        assert_eq!(out.value, ColumnValue::String("1.15".to_owned()));
    }

    /// The same value read as `SQL_C_DOUBLE` really is the longer number, and
    /// is reported as too long rather than quietly shortened.
    #[test]
    fn a_double_precision_source_renders_every_digit_it_has() {
        assert_eq!(
            state_of(numeric_to_sql_type(
                NumericParam::Approx {
                    value: f64::from(1.15_f32),
                    single: false,
                },
                SqlDataType::VARCHAR,
                4,
                0,
                0,
            )),
            "22001"
        );
    }

    // -- targets the following commits implement ----------------------------

    #[test]
    fn a_target_this_table_does_not_convert_to_is_07006() {
        assert_eq!(
            state_of(convert(exact("1"), SqlDataType::EXT_GUID, 0)),
            "07006"
        );
    }
}
