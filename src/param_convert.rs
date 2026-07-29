//! Conversion of character parameter data to the SQL type the application
//! declared at `SQLBindParameter`.
//!
//! `SQLBindParameter` takes two types: `ValueType`, the C type the value is
//! delivered in, and `ParameterType`, the SQL type the data source is to
//! receive. ODBC makes the driver convert between them — "If necessary, the
//! driver converts the data from the data type specified by the *ValueType*
//! argument in **SQLBindParameter** to the data type specified by the
//! *ParameterType* argument, and then sends the data to the data source"
//! ([Converting Data from C to SQL Data Types]).
//!
//! For every C type except the two character ones, `ValueType` already fixes
//! the value's shape and [`crate::ffi::params::read_param_value`] can read it
//! directly. `SQL_C_CHAR` and `SQL_C_WCHAR` are the exception: the value
//! arrives as text and `ParameterType` is the only statement of what it *is*.
//! Dropping it turns a `DECIMAL` parameter into a string, and a backend that
//! renders its parameters into SQL then emits `WHERE amount = '12.34'` against
//! a decimal column.
//!
//! This module is the [C to SQL: Character] table, transcribed. The table's
//! first column gives the legal `ParameterType` values, and its third gives the
//! SQLSTATE for each outcome — which is why the failures here are 22018 /
//! 22001 / 22003 / 22008 rather than a single general error.
//!
//! [Converting Data from C to SQL Data Types]: https://learn.microsoft.com/en-us/sql/odbc/reference/appendixes/converting-data-from-c-to-sql-data-types
//! [C to SQL: Character]: https://learn.microsoft.com/en-us/sql/odbc/reference/appendixes/c-to-sql-character

use odbc_sys::SqlDataType;

use crate::{
    column_value::{current_utc_date, parse_sql_time, parse_sql_timestamp},
    errors::OdbcError,
    types::{ColumnValue, SqlState, ULen},
};

// -- the four SQLSTATEs the table's third column names ----------------------

/// "Data value is not a *numeric-literal*" / "not a hexadecimal value" / "not a
/// valid *ODBC-date-literal*…" — the text does not denote a value of the
/// declared type at all.
fn not_a_literal(text: &str, sql_type: &str) -> OdbcError {
    OdbcError::general(
        format!("Parameter value {text:?} is not a valid {sql_type} literal"),
        SqlState::invalid_character_value_for_cast(),
    )
}

/// "Data converted with truncation of fractional digits" / "loss of whole …
/// digits" — the value is well formed but does not survive the conversion
/// intact, so it is not sent.
fn truncation(text: &str, what: &str) -> OdbcError {
    OdbcError::general(
        format!("Converting parameter value {text:?} would {what}"),
        SqlState::string_data_right_truncation(),
    )
}

/// "Data is outside the range of the data type to which the number is being
/// converted".
fn out_of_range(text: &str, sql_type: &str) -> OdbcError {
    OdbcError::general(
        format!("Parameter value {text:?} is outside the range of {sql_type}"),
        SqlState::numeric_value_out_of_range(),
    )
}

/// The datetime truncations the table calls out: a `SQL_TYPE_DATE` target given
/// a non-zero time, or a `SQL_TYPE_TIME` target given a non-zero fraction.
fn datetime_overflow(text: &str, what: &str) -> OdbcError {
    OdbcError::general(
        format!("Converting parameter value {text:?} would {what}"),
        SqlState::datetime_field_overflow(),
    )
}

/// Convert character parameter data to the declared SQL type.
///
/// `text` is the value read out of an `SQL_C_CHAR` or `SQL_C_WCHAR` parameter
/// buffer; `sql_type` is `SQLBindParameter`'s `ParameterType`.
///
/// SQL types the [C to SQL: Character] table does not list are returned as
/// [`ColumnValue::String`] unchanged. That is not a silent fallback: the spec
/// puts the rejection at bind time rather than here — "If the *ParameterType*
/// argument in **SQLBindParameter** contains the identifier of an ODBC SQL data
/// type that is not shown in the table for a given C data type,
/// **SQLBindParameter** returns SQLSTATE 07006" — so by the time a value
/// reaches this function the pairing has already been accepted, and text is the
/// only faithful thing left to send. It also covers the character SQL types,
/// where text *is* the answer, and driver-specific type identifiers, which only
/// the backend can interpret.
///
/// # Declared size
///
/// `col_size` and `decimal_digits` are `SQLBindParameter`'s `ColumnSize` and
/// `DecimalDigits`. They are enforced for `SQL_DECIMAL` and `SQL_NUMERIC` only
/// — see [`check_declared_decimal_size`], which also records why the numeric
/// and integer types need no such check.
///
/// Two rows of the table still ask for a size check that is not made here, and
/// both are deliberate rather than forgotten:
///
/// - **Character targets.** "Byte length of data > Column length" is 22001 for
///   `SQL_CHAR`/`SQL_VARCHAR`/`SQL_LONGVARCHAR`, and the `SQL_W*` row states
///   the same test in *characters*. The wide row is implementable — UTF-16 code
///   units are well defined — but the narrow one is not: "byte length" is in
///   the data source's own encoding, which core does not know and which differs
///   between a UTF-8 backend and a Shift-JIS one. Answering it needs a
///   `Backend` hook, so it waits for a driver that needs one.
/// - **Binary targets.** "(Byte length of data) / 2 > column byte length" is
///   22001, and unlike the character case it has no encoding question. It is
///   simply not done yet.
///
/// Until then the declared size for those targets is unenforced, and the effect
/// is a missing diagnostic rather than wrong data: the value reaches the
/// backend as the application wrote it, and the data source applies its own
/// column constraints. An application relying on the driver to police its
/// declared parameter size is not told; one relying on the data source is
/// unaffected.
///
/// [C to SQL: Character]: https://learn.microsoft.com/en-us/sql/odbc/reference/appendixes/c-to-sql-character
pub(crate) fn text_to_sql_type(
    text: &str,
    sql_type: SqlDataType,
    col_size: ULen,
    decimal_digits: i16,
) -> Result<ColumnValue, OdbcError> {
    // The declared type, not the value: a parameter's contents can be anything
    // the application is querying on. Which SQL type it was bound as is the
    // fact a "why did my decimal filter come back empty" report needs, and the
    // one nothing else in the log records.
    tracing::trace!(
        "text_to_sql_type: {} characters declared as {:?}",
        text.len(),
        sql_type
    );

    // Exact numerics. The spec puts DECIMAL/NUMERIC and the four integer types
    // in one row: the test is the same ("is this a numeric-literal"), and only
    // what survives the conversion differs.
    if sql_type == SqlDataType::DECIMAL || sql_type == SqlDataType::NUMERIC {
        let literal = decimal_literal(text, "decimal")?;
        check_declared_decimal_size(&literal, text, col_size, decimal_digits)?;
        return Ok(ColumnValue::Decimal(literal.to_decimal_string()));
    }
    if sql_type == SqlDataType::EXT_TINY_INT {
        return Ok(ColumnValue::I8(to_integer(text, "TINYINT")?));
    }
    if sql_type == SqlDataType::SMALLINT {
        return Ok(ColumnValue::I16(to_integer(text, "SMALLINT")?));
    }
    if sql_type == SqlDataType::INTEGER {
        return Ok(ColumnValue::I32(to_integer(text, "INTEGER")?));
    }
    if sql_type == SqlDataType::EXT_BIG_INT {
        return Ok(ColumnValue::I64(to_integer(text, "BIGINT")?));
    }

    // Approximate numerics. `ColumnValue::F32` is SQL_REAL and `F64` is
    // SQL_FLOAT / SQL_DOUBLE, matching the variants' own documentation.
    if sql_type == SqlDataType::REAL {
        let v = to_double(text, "REAL")?;
        if v.abs() > f64::from(f32::MAX) {
            return Err(out_of_range(text, "REAL"));
        }
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the f32 range check above is the conversion's own precondition"
        )]
        return Ok(ColumnValue::F32(v as f32));
    }
    if sql_type == SqlDataType::FLOAT || sql_type == SqlDataType::DOUBLE {
        return Ok(ColumnValue::F64(to_double(text, "DOUBLE")?));
    }

    if sql_type == SqlDataType::EXT_BIT {
        return to_bit(text);
    }

    if sql_type == SqlDataType::EXT_BINARY
        || sql_type == SqlDataType::EXT_VAR_BINARY
        || sql_type == SqlDataType::EXT_LONG_VAR_BINARY
    {
        return to_binary(text);
    }

    // Datetimes. The ODBC 2.0 spellings (`SQL_DATE` 9, `SQL_TIME` 10,
    // `SQL_TIMESTAMP` 11) are grouped with their 3.x counterparts exactly as
    // `types::col_attr` already groups them, so a parameter bound by an ODBC
    // 2.x application does not lose its type for using the older number.
    if sql_type == SqlDataType::DATE {
        return to_date(text);
    }
    if sql_type == SqlDataType::TIME || sql_type == SqlDataType::EXT_TIME_OR_INTERVAL {
        return to_time(text);
    }
    if sql_type == SqlDataType::TIMESTAMP
        || sql_type == SqlDataType::DATETIME
        || sql_type == SqlDataType::EXT_TIMESTAMP
    {
        return to_timestamp(text);
    }

    // Character targets. The table gives the narrow row "Byte length of data >
    // Column length" and the wide row the same test in characters; both are
    // measured here in characters, and the narrow row's deviation from its own
    // wording is deliberate — see this function's "Declared size" note.
    if sql_type == SqlDataType::CHAR
        || sql_type == SqlDataType::VARCHAR
        || sql_type == SqlDataType::EXT_LONG_VARCHAR
    {
        check_declared_char_size(text.chars().count(), col_size)?;
        return Ok(ColumnValue::String(text.to_owned()));
    }
    if sql_type == SqlDataType::EXT_W_CHAR
        || sql_type == SqlDataType::EXT_W_VARCHAR
        || sql_type == SqlDataType::EXT_W_LONG_VARCHAR
    {
        check_declared_char_size(text.encode_utf16().count(), col_size)?;
        return Ok(ColumnValue::String(text.to_owned()));
    }

    // The interval types (which `ColumnValue` cannot carry), `SQL_GUID` (absent
    // from the table, so 07006 at bind time) and any driver-specific
    // identifier. None of these carries a declared length this function can
    // test, so none is size-checked.
    Ok(ColumnValue::String(text.to_owned()))
}

// -- numeric-literal parsing -------------------------------------------------

/// A parsed *numeric-literal*, as `±digits × 10⁻ˢᶜᵃˡᵉ`.
///
/// Keeping the significant digits as text rather than as a float is what lets a
/// `DECIMAL(38,10)` parameter reach the backend without passing through
/// `f64`'s 53 bits of mantissa. `scale` may be negative, which is how an
/// exponent larger than the fraction's length is carried (`1.5e2` is digits
/// `15` at scale `-1`).
struct DecimalLiteral {
    negative: bool,
    digits: String,
    scale: i32,
}

/// Parse the SQL grammar's *numeric-literal*: optional sign, digits with an
/// optional decimal point, optional `E` exponent. Blanks around it are ignored,
/// per "When character C data is converted to numeric, date, time, or timestamp
/// SQL data, leading and trailing blanks are ignored."
///
/// Deliberately stricter than `str::parse::<f64>()`, which also accepts `inf`
/// and `NaN` — neither is a numeric-literal, and neither is something to send
/// to a data source as one.
fn parse_numeric_literal(s: &str) -> Option<DecimalLiteral> {
    let t = s.trim();
    let (negative, rest) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };

    let (mantissa, exponent) = match rest.split_once(['e', 'E']) {
        Some((mantissa, exp)) => (mantissa, exp.parse::<i32>().ok()?),
        None => (rest, 0),
    };

    let (int_part, frac_part) = match mantissa.split_once('.') {
        Some((int_part, frac_part)) => (int_part, frac_part),
        None => (mantissa, ""),
    };

    // At least one digit, and nothing but digits on either side of the point.
    if int_part.is_empty() && frac_part.is_empty() {
        return None;
    }
    if !int_part
        .bytes()
        .chain(frac_part.bytes())
        .all(|b| b.is_ascii_digit())
    {
        return None;
    }

    let scale = i32::try_from(frac_part.len()).ok()?.checked_sub(exponent)?;
    Some(DecimalLiteral {
        negative,
        digits: format!("{int_part}{frac_part}"),
        scale,
    })
}

impl DecimalLiteral {
    /// The significant digits with leading zeros removed, never empty.
    fn significant(&self) -> &str {
        let trimmed = self.digits.trim_start_matches('0');
        if trimmed.is_empty() { "0" } else { trimmed }
    }

    fn is_zero(&self) -> bool {
        self.digits.bytes().all(|b| b == b'0')
    }

    /// Render as a plain decimal literal, expanding any exponent. A backend
    /// renders `ColumnValue::Decimal` into SQL verbatim, and `1.5e2` is not
    /// something every data source accepts where `150` is.
    fn to_decimal_string(&self) -> String {
        let sign = if self.negative && !self.is_zero() {
            "-"
        } else {
            ""
        };
        let digits = self.significant();
        if self.scale <= 0 {
            if self.is_zero() {
                return format!("{sign}0");
            }
            let zeros = "0".repeat(usize::try_from(-self.scale).unwrap_or(0));
            return format!("{sign}{digits}{zeros}");
        }
        let scale = usize::try_from(self.scale).unwrap_or(0);
        if digits.len() > scale {
            let point = digits.len() - scale;
            format!("{sign}{}.{}", &digits[..point], &digits[point..])
        } else {
            format!("{sign}0.{digits:0>scale$}")
        }
    }

    /// The smallest scale that represents this value exactly.
    ///
    /// A declared scale is compared against this rather than against however
    /// many fractional digits were typed, so `12.3400` fits `DECIMAL(10,2)`:
    /// dropping those two trailing zeros loses nothing, and the spec's
    /// truncation test is about what the conversion would *lose*.
    fn required_scale(&self) -> usize {
        if self.is_zero() || self.scale <= 0 {
            return 0;
        }
        let trailing_zeros = self.digits.len() - self.digits.trim_end_matches('0').len();
        usize::try_from(self.scale)
            .unwrap_or(0)
            .saturating_sub(trailing_zeros)
    }

    /// The number of digits to the left of the decimal point. Zero has none, so
    /// it fits a `DECIMAL(2,2)` that has room for no whole digits at all.
    fn whole_digits(&self) -> usize {
        if self.is_zero() {
            return 0;
        }
        let significant = self.significant().len();
        if self.scale <= 0 {
            // A negative scale is trailing zeros the literal did not spell out.
            return significant.saturating_add(usize::try_from(-self.scale).unwrap_or(0));
        }
        significant.saturating_sub(usize::try_from(self.scale).unwrap_or(0))
    }

    /// Whether every digit to the right of the decimal point is a zero, i.e.
    /// whether an integer target would lose anything.
    fn fraction_is_zero(&self) -> bool {
        let Ok(scale) = usize::try_from(self.scale) else {
            // A negative scale is a whole number with trailing zeros appended.
            return true;
        };
        let fraction = self
            .digits
            .len()
            .checked_sub(scale)
            .map_or(self.digits.as_str(), |point| &self.digits[point..]);
        fraction.bytes().all(|b| b == b'0')
    }

    /// The value truncated toward zero, or `None` if it does not fit `i128`.
    fn to_integer(&self) -> Option<i128> {
        let whole = if self.scale <= 0 {
            let zeros = "0".repeat(usize::try_from(-self.scale).ok()?);
            format!("{}{zeros}", self.significant())
        } else {
            let scale = usize::try_from(self.scale).ok()?;
            match self.digits.len().checked_sub(scale) {
                Some(point) if point > 0 => self.digits[..point].to_owned(),
                // Every digit is to the right of the point: |value| < 1.
                _ => "0".to_owned(),
            }
        };
        let magnitude = whole.parse::<i128>().ok()?;
        if self.negative {
            magnitude.checked_neg()
        } else {
            Some(magnitude)
        }
    }
}

fn decimal_literal(text: &str, sql_type: &str) -> Result<DecimalLiteral, OdbcError> {
    parse_numeric_literal(text).ok_or_else(|| not_a_literal(text, sql_type))
}

/// The declared column size was exceeded — the [C to SQL: Character] table's
/// "Byte length of data > Column length" row and its siblings.
///
/// The message names the measured length and the declared size rather than the
/// value, unlike [`truncation`] and [`out_of_range`] next door. These are
/// precisely the rows with large values: a 10 MB parameter's contents in a
/// diagnostic record helps nobody, and the two numbers are the whole of what an
/// application needs to fix its bind.
///
/// [C to SQL: Character]: https://learn.microsoft.com/en-us/sql/odbc/reference/appendixes/c-to-sql-character
fn oversized(actual: usize, unit: &str, declared: ULen) -> OdbcError {
    OdbcError::general(
        format!(
            "Parameter value of {actual} {unit} exceeds the declared column size of {declared}"
        ),
        SqlState::string_data_right_truncation(),
    )
}

/// Apply the declared `ColumnSize` to a character target.
///
/// `measured` is the value's length in the unit its row names — characters for
/// `SQL_CHAR` and its siblings, UTF-16 code units for the `SQL_W*` ones. The
/// caller picks the unit, because it is fixed by the target SQL type and not by
/// the C type the value arrived in.
///
/// A `col_size` of 0 states that the application declared no size rather than a
/// zero-length column, exactly as in [`check_declared_decimal_size`]: ODBC
/// defines no sentinel for "size unknown", and reading 0 literally would reject
/// every value an application that omits `ColumnSize` ever binds.
fn check_declared_char_size(measured: usize, col_size: ULen) -> Result<(), OdbcError> {
    if col_size == 0 {
        return Ok(());
    }
    if measured > col_size {
        return Err(oversized(measured, "characters", col_size));
    }
    Ok(())
}

/// Apply the declared `DECIMAL(col_size, decimal_digits)` to a parsed literal.
///
/// These are `SQLBindParameter`'s `ColumnSize` and `DecimalDigits`, which for
/// `SQL_DECIMAL` and `SQL_NUMERIC` set the IPD's `SQL_DESC_PRECISION` and
/// `SQL_DESC_SCALE`. Both of the row's truncation outcomes are 22001 —
/// "data converted with truncation of fractional digits" and "conversion of
/// data would result in loss of whole (as opposed to fractional) digits".
/// (`SQLExecute`'s own 22003 row is a different check: it is about assignment
/// to the *table column*, which is the data source's to make, not the
/// driver's.)
///
/// Only these two SQL types are checked. `ColumnSize` sets `SQL_DESC_PRECISION`
/// for `SQL_FLOAT`, `SQL_REAL` and `SQL_DOUBLE` too, but there it is a count of
/// mantissa bits and the row's own test is range, which [`to_double`] already
/// applies. For every other type — the integers included — the spec says
/// plainly: "For other data types, the *ColumnSize* argument is ignored."
///
/// The two declared-size checks the spec's table still asks for and this does
/// not are noted on [`text_to_sql_type`].
fn check_declared_decimal_size(
    literal: &DecimalLiteral,
    text: &str,
    col_size: ULen,
    decimal_digits: i16,
) -> Result<(), OdbcError> {
    // No decimal has zero digits of precision, so a `ColumnSize` of 0 states
    // that the application declared no size rather than a zero-digit column.
    // The spec defines no sentinel for "size unknown", and reading 0 literally
    // would reject every value an application that omits it ever binds.
    if col_size == 0 {
        return Ok(());
    }

    // A negative scale is legal on some data sources and rounds the value to
    // tens or hundreds. Core has no rounding to apply and will not guess at
    // one, so a negative scale disables the check rather than being read as 0.
    let Ok(scale) = usize::try_from(decimal_digits) else {
        return Ok(());
    };

    if literal.required_scale() > scale {
        return Err(truncation(text, "truncate its fractional digits"));
    }

    // A scale exceeding the precision is a contradictory declaration; it leaves
    // room for no whole digits rather than underflowing.
    let whole_allowed = col_size.saturating_sub(scale);
    if literal.whole_digits() > whole_allowed {
        return Err(truncation(text, "lose whole digits"));
    }

    Ok(())
}

/// Convert to an exact integer target, applying the row's two truncation tests.
fn to_integer<T: TryFrom<i128>>(text: &str, sql_type: &str) -> Result<T, OdbcError> {
    let literal = decimal_literal(text, sql_type)?;
    if !literal.fraction_is_zero() {
        return Err(truncation(text, "truncate its fractional digits"));
    }
    literal
        .to_integer()
        .and_then(|v| T::try_from(v).ok())
        .ok_or_else(|| truncation(text, format!("lose whole digits of {sql_type}").as_str()))
}

/// Validate as a numeric-literal, then convert to `f64`. An exponent beyond
/// `f64`'s range parses to an infinity, which is the row's out-of-range case
/// rather than a malformed literal.
fn to_double(text: &str, sql_type: &str) -> Result<f64, OdbcError> {
    decimal_literal(text, sql_type)?;
    let value = text
        .trim()
        .parse::<f64>()
        .map_err(|_| not_a_literal(text, sql_type))?;
    if value.is_finite() {
        Ok(value)
    } else {
        Err(out_of_range(text, sql_type))
    }
}

/// `SQL_BIT`'s four outcomes, in the order the table lists them.
fn to_bit(text: &str) -> Result<ColumnValue, OdbcError> {
    let literal = decimal_literal(text, "BIT")?;
    let value = to_double(text, "BIT")?;
    if value == 0.0 && literal.is_zero() {
        return Ok(ColumnValue::Bool(false));
    }
    if !(0.0..2.0).contains(&value) {
        return Err(out_of_range(text, "BIT"));
    }
    if value == 1.0 && literal.fraction_is_zero() {
        return Ok(ColumnValue::Bool(true));
    }
    // Greater than 0, less than 2, not equal to 1: representable only by
    // dropping the fraction.
    Err(truncation(text, "truncate its fractional digits"))
}

/// Decode hexadecimal digit pairs. "The driver always converts pairs of
/// hexadecimal digits to individual bytes … if the length of the character
/// string is odd, the last byte of the string … is not converted."
fn to_binary(text: &str) -> Result<ColumnValue, OdbcError> {
    if !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(not_a_literal(text, "hexadecimal"));
    }
    let digits = text.as_bytes();
    let bytes = digits
        .chunks_exact(2)
        .map(|pair| {
            // SAFETY-of-logic: every byte passed `is_ascii_hexdigit` above, so
            // both conversions are infallible; `unwrap_or` states a value the
            // branch cannot reach rather than panicking in an FFI call.
            let hi = char::from(pair[0]).to_digit(16).unwrap_or(0);
            let lo = char::from(pair[1]).to_digit(16).unwrap_or(0);
            u8::try_from(hi * 16 + lo).unwrap_or(0)
        })
        .collect();
    Ok(ColumnValue::Bytes(bytes))
}

// -- datetime targets --------------------------------------------------------

/// `SQL_TYPE_DATE`: a date literal, or a timestamp literal whose time is zero.
fn to_date(text: &str) -> Result<ColumnValue, OdbcError> {
    let ts = parse_sql_timestamp(text).map_err(|e| retype_datetime_error(e, text, "date"))?;
    if (ts.hour, ts.minute, ts.second, ts.fraction) != (0, 0, 0, 0) {
        return Err(datetime_overflow(text, "drop the time portion"));
    }
    Ok(ColumnValue::Date {
        year: ts.year,
        month: ts.month,
        day: ts.day,
    })
}

/// `SQL_TYPE_TIME`: a time literal, or a timestamp literal whose fractional
/// seconds are zero — "[b] The date portion of the timestamp is ignored."
fn to_time(text: &str) -> Result<ColumnValue, OdbcError> {
    if let Ok((time, fraction)) = parse_sql_time(text) {
        return Ok(ColumnValue::Time {
            hour: time.hour,
            minute: time.minute,
            second: time.second,
            fraction,
        });
    }
    let ts = parse_sql_timestamp(text).map_err(|e| retype_datetime_error(e, text, "time"))?;
    if ts.fraction != 0 {
        return Err(datetime_overflow(text, "drop the fractional seconds"));
    }
    Ok(ColumnValue::Time {
        hour: ts.hour,
        minute: ts.minute,
        second: ts.second,
        fraction: 0,
    })
}

/// `SQL_TYPE_TIMESTAMP`: a timestamp literal, a date literal ("[c] The time
/// portion of the timestamp is set to zero", which `parse_sql_timestamp`
/// already does), or a time literal ("[d] The date portion of the timestamp is
/// set to the current date").
fn to_timestamp(text: &str) -> Result<ColumnValue, OdbcError> {
    match parse_sql_timestamp(text) {
        Ok(ts) => Ok(ColumnValue::Timestamp {
            year: ts.year,
            month: ts.month,
            day: ts.day,
            hour: ts.hour,
            minute: ts.minute,
            second: ts.second,
            fraction: ts.fraction,
        }),
        Err(e) => {
            let Ok((time, fraction)) = parse_sql_time(text) else {
                return Err(retype_datetime_error(e, text, "timestamp"));
            };
            let (year, month, day) = current_utc_date();
            Ok(ColumnValue::Timestamp {
                year,
                month,
                day,
                hour: time.hour,
                minute: time.minute,
                second: time.second,
                fraction,
            })
        }
    }
}

/// Re-label a parse failure from `column_value`'s literal parsers.
///
/// Those parsers serve the SQL-to-C direction, where an out-of-range field is
/// 22007 ("invalid datetime format"). That code is on the `SQLExecute`
/// diagnostics table too, and is more specific than the blanket 22018 this
/// row's last line would give, so it is kept; a malformed literal keeps the
/// 22018 the row names, with a message naming the target type.
fn retype_datetime_error(e: OdbcError, text: &str, sql_type: &str) -> OdbcError {
    if e.sqlstate() == SqlState::invalid_character_value_for_cast() {
        not_a_literal(text, sql_type)
    } else {
        e
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Convert with no declared size, which is how most of these tests want it:
    /// a precision of 0 is not a legal decimal precision, so it reads as "the
    /// application did not state one" and no size check runs.
    fn convert(text: &str, sql_type: SqlDataType) -> ColumnValue {
        text_to_sql_type(text, sql_type, 0, 0).expect("conversion should succeed")
    }

    fn sqlstate(text: &str, sql_type: SqlDataType) -> String {
        state_of(text_to_sql_type(text, sql_type, 0, 0))
    }

    /// Convert as `DECIMAL(col_size, decimal_digits)`.
    fn convert_decimal(text: &str, col_size: ULen, decimal_digits: i16) -> ColumnValue {
        text_to_sql_type(text, SqlDataType::DECIMAL, col_size, decimal_digits)
            .expect("conversion should succeed")
    }

    fn decimal_sqlstate(text: &str, col_size: ULen, decimal_digits: i16) -> String {
        state_of(text_to_sql_type(
            text,
            SqlDataType::DECIMAL,
            col_size,
            decimal_digits,
        ))
    }

    fn state_of(result: Result<ColumnValue, OdbcError>) -> String {
        result
            .expect_err("conversion should have failed")
            .sqlstate()
            .as_str()
            .to_owned()
    }

    // -- character targets: text stays text ---------------------------------

    #[test]
    fn a_varchar_parameter_stays_a_string() {
        assert_eq!(
            convert("hello", SqlDataType::VARCHAR),
            ColumnValue::String("hello".into())
        );
    }

    #[test]
    fn a_wvarchar_parameter_stays_a_string() {
        assert_eq!(
            convert("héllo", SqlDataType::EXT_W_VARCHAR),
            ColumnValue::String("héllo".into())
        );
    }

    /// Blanks are only ignored for the numeric and datetime targets. A
    /// character column is entitled to the spaces the application sent.
    #[test]
    fn a_character_target_keeps_surrounding_blanks() {
        assert_eq!(
            convert("  padded  ", SqlDataType::CHAR),
            ColumnValue::String("  padded  ".into())
        );
    }

    // -- DECIMAL / NUMERIC --------------------------------------------------

    /// The reported defect: pyodbc binds a `Decimal` as `SQL_C_CHAR` +
    /// `SQL_NUMERIC`, and the declared type is the only thing distinguishing
    /// it from a string.
    #[test]
    fn a_numeric_parameter_becomes_a_decimal() {
        assert_eq!(
            convert("12.34", SqlDataType::NUMERIC),
            ColumnValue::Decimal("12.34".into())
        );
    }

    #[test]
    fn a_decimal_parameter_becomes_a_decimal() {
        assert_eq!(
            convert("-0.05", SqlDataType::DECIMAL),
            ColumnValue::Decimal("-0.05".into())
        );
    }

    /// "When character C data is converted to numeric, date, time, or
    /// timestamp SQL data, leading and trailing blanks are ignored."
    #[test]
    fn a_decimal_parameter_ignores_surrounding_blanks() {
        assert_eq!(
            convert("  12.34  ", SqlDataType::DECIMAL),
            ColumnValue::Decimal("12.34".into())
        );
    }

    /// A decimal has to reach the backend as a decimal literal, so an
    /// exponent is expanded rather than passed through: `1.5e2` is `150`.
    #[test]
    fn a_decimal_parameter_expands_an_exponent() {
        assert_eq!(
            convert("1.5e2", SqlDataType::DECIMAL),
            ColumnValue::Decimal("150".into())
        );
    }

    #[test]
    fn a_decimal_parameter_expands_a_negative_exponent() {
        assert_eq!(
            convert("15e-2", SqlDataType::DECIMAL),
            ColumnValue::Decimal("0.15".into())
        );
    }

    /// Spec: "Data value is not a *numeric-literal*" → 22018.
    #[test]
    fn a_decimal_parameter_that_is_not_a_numeric_literal_is_22018() {
        assert_eq!(sqlstate("twelve", SqlDataType::DECIMAL), "22018");
    }

    #[test]
    fn an_empty_decimal_parameter_is_22018() {
        assert_eq!(sqlstate("", SqlDataType::DECIMAL), "22018");
    }

    // -- DECIMAL / NUMERIC: the declared precision and scale -----------------

    #[test]
    fn a_decimal_within_its_declared_precision_and_scale_is_accepted() {
        assert_eq!(
            convert_decimal("12.34", 10, 2),
            ColumnValue::Decimal("12.34".into())
        );
    }

    /// Spec: "Data converted with truncation of fractional digits" → 22001.
    /// `DECIMAL(10,2)` has no room for the third fractional digit.
    #[test]
    fn a_decimal_with_more_fractional_digits_than_its_declared_scale_is_22001() {
        assert_eq!(decimal_sqlstate("12.345", 10, 2), "22001");
    }

    /// Trailing zeros beyond the declared scale lose nothing, so they are not
    /// truncation. The check ignores them; the value is still passed on as the
    /// application wrote it, because `12.3400` and `12.34` are the same
    /// `DECIMAL(10,2)` and reshaping digits is not this check's job.
    #[test]
    fn a_decimal_whose_extra_fractional_digits_are_zeros_is_accepted() {
        assert_eq!(
            convert_decimal("12.3400", 10, 2),
            ColumnValue::Decimal("12.3400".into())
        );
    }

    /// Spec: "Conversion of data would result in loss of whole (as opposed to
    /// fractional) digits" → 22001. `DECIMAL(10,2)` leaves eight whole digits.
    #[test]
    fn a_decimal_with_more_whole_digits_than_its_declared_precision_allows_is_22001() {
        assert_eq!(
            decimal_sqlstate("123456789012345678901234567890", 10, 2),
            "22001"
        );
    }

    #[test]
    fn a_decimal_filling_its_declared_precision_exactly_is_accepted() {
        assert_eq!(
            convert_decimal("12345678.90", 10, 2),
            ColumnValue::Decimal("12345678.90".into())
        );
    }

    #[test]
    fn a_decimal_one_whole_digit_over_its_declared_precision_is_22001() {
        assert_eq!(decimal_sqlstate("123456789.01", 10, 2), "22001");
    }

    /// A precision of 0 is not a legal decimal precision, so it states that the
    /// application declared no size rather than a zero-digit column. Checking
    /// it literally would reject every value.
    #[test]
    fn a_decimal_with_an_unstated_precision_is_not_size_checked() {
        assert_eq!(
            convert_decimal("123456789012345678901234567890.99", 0, 0),
            ColumnValue::Decimal("123456789012345678901234567890.99".into())
        );
    }

    /// `DECIMAL(2,2)` holds only a fraction. A value below one has no whole
    /// digits to lose.
    #[test]
    fn a_decimal_smaller_than_one_has_no_whole_digits() {
        assert_eq!(
            convert_decimal("0.05", 2, 2),
            ColumnValue::Decimal("0.05".into())
        );
    }

    #[test]
    fn zero_fits_a_declared_decimal_with_no_whole_digits() {
        assert_eq!(convert_decimal("0", 2, 2), ColumnValue::Decimal("0".into()));
    }

    /// An exponent is expanded before the size check, so the declared size is
    /// measured against the value rather than against how it was spelled.
    #[test]
    fn a_decimal_exponent_is_expanded_before_the_size_check() {
        assert_eq!(decimal_sqlstate("1e9", 10, 2), "22001");
    }

    /// A negative scale is legal on some data sources and means the value is
    /// rounded to tens or hundreds. Core has no rounding to apply, so it
    /// declines to enforce rather than guessing.
    #[test]
    fn a_negative_declared_scale_is_not_enforced() {
        assert_eq!(
            convert_decimal("12.34", 10, -1),
            ColumnValue::Decimal("12.34".into())
        );
    }

    /// A scale larger than the precision is a contradictory declaration. It
    /// leaves no whole digits rather than underflowing the subtraction.
    #[test]
    fn a_declared_scale_larger_than_the_precision_leaves_no_whole_digits() {
        assert_eq!(decimal_sqlstate("1.5", 2, 4), "22001");
    }

    /// Spec, `SQLBindParameter`'s *ColumnSize* section: `ColumnSize` sets
    /// `SQL_DESC_PRECISION` only for `SQL_DECIMAL`, `SQL_NUMERIC`, `SQL_FLOAT`,
    /// `SQL_REAL` and `SQL_DOUBLE`; "for other data types, the *ColumnSize*
    /// argument is ignored". The integer types are other data types, so their
    /// range check stays the C type's own.
    #[test]
    fn an_integer_target_ignores_the_declared_column_size() {
        assert_eq!(
            text_to_sql_type("123456", SqlDataType::INTEGER, 2, 0).unwrap(),
            ColumnValue::I32(123_456)
        );
    }

    // -- exact integer targets ----------------------------------------------

    #[test]
    fn an_integer_parameter_becomes_an_i32() {
        assert_eq!(convert("42", SqlDataType::INTEGER), ColumnValue::I32(42));
    }

    #[test]
    fn a_smallint_parameter_becomes_an_i16() {
        assert_eq!(convert("-7", SqlDataType::SMALLINT), ColumnValue::I16(-7));
    }

    #[test]
    fn a_tinyint_parameter_becomes_an_i8() {
        assert_eq!(
            convert("100", SqlDataType::EXT_TINY_INT),
            ColumnValue::I8(100)
        );
    }

    #[test]
    fn a_bigint_parameter_becomes_an_i64() {
        assert_eq!(
            convert("9223372036854775807", SqlDataType::EXT_BIG_INT),
            ColumnValue::I64(i64::MAX)
        );
    }

    /// A fraction of zeros loses nothing, so it is not truncation.
    #[test]
    fn an_integer_parameter_accepts_a_zero_fraction() {
        assert_eq!(
            convert("42.000", SqlDataType::INTEGER),
            ColumnValue::I32(42)
        );
    }

    /// Spec: "Data converted with truncation of fractional digits" → 22001.
    #[test]
    fn an_integer_parameter_with_a_nonzero_fraction_is_22001() {
        assert_eq!(sqlstate("42.5", SqlDataType::INTEGER), "22001");
    }

    /// Spec: "Conversion of data would result in loss of whole (as opposed to
    /// fractional) digits" → 22001.
    #[test]
    fn an_integer_parameter_too_large_for_its_type_is_22001() {
        assert_eq!(sqlstate("2147483648", SqlDataType::INTEGER), "22001");
    }

    #[test]
    fn a_smallint_parameter_too_large_for_its_type_is_22001() {
        assert_eq!(sqlstate("40000", SqlDataType::SMALLINT), "22001");
    }

    #[test]
    fn an_integer_parameter_that_is_not_a_numeric_literal_is_22018() {
        assert_eq!(sqlstate("1,000", SqlDataType::INTEGER), "22018");
    }

    // -- approximate numeric targets ----------------------------------------

    #[test]
    fn a_double_parameter_becomes_an_f64() {
        assert_eq!(convert("1.5", SqlDataType::DOUBLE), ColumnValue::F64(1.5));
    }

    #[test]
    fn a_float_parameter_becomes_an_f64() {
        assert_eq!(
            convert("-2.25", SqlDataType::FLOAT),
            ColumnValue::F64(-2.25)
        );
    }

    #[test]
    fn a_real_parameter_becomes_an_f32() {
        assert_eq!(convert("1.5", SqlDataType::REAL), ColumnValue::F32(1.5));
    }

    /// Spec: "Data is outside the range of the data type to which the number
    /// is being converted" → 22003.
    #[test]
    fn a_real_parameter_outside_f32_range_is_22003() {
        assert_eq!(sqlstate("1e39", SqlDataType::REAL), "22003");
    }

    #[test]
    fn a_double_parameter_outside_f64_range_is_22003() {
        assert_eq!(sqlstate("1e400", SqlDataType::DOUBLE), "22003");
    }

    #[test]
    fn a_double_parameter_that_is_not_a_numeric_literal_is_22018() {
        assert_eq!(sqlstate("NaN-ish", SqlDataType::DOUBLE), "22018");
    }

    // -- SQL_BIT ------------------------------------------------------------

    #[test]
    fn a_bit_parameter_of_one_becomes_true() {
        assert_eq!(convert("1", SqlDataType::EXT_BIT), ColumnValue::Bool(true));
    }

    #[test]
    fn a_bit_parameter_of_zero_becomes_false() {
        assert_eq!(convert("0", SqlDataType::EXT_BIT), ColumnValue::Bool(false));
    }

    /// Spec: "Data is greater than 0, less than 2, and not equal to 1" →
    /// 22001. The value is rounded down to 1, losing the fraction.
    #[test]
    fn a_fractional_bit_parameter_between_zero_and_two_is_22001() {
        assert_eq!(sqlstate("0.5", SqlDataType::EXT_BIT), "22001");
    }

    /// Spec: "Data is less than 0 or greater than or equal to 2" → 22003.
    #[test]
    fn a_bit_parameter_of_two_is_22003() {
        assert_eq!(sqlstate("2", SqlDataType::EXT_BIT), "22003");
    }

    #[test]
    fn a_negative_bit_parameter_is_22003() {
        assert_eq!(sqlstate("-1", SqlDataType::EXT_BIT), "22003");
    }

    #[test]
    fn a_bit_parameter_that_is_not_a_numeric_literal_is_22018() {
        assert_eq!(sqlstate("true", SqlDataType::EXT_BIT), "22018");
    }

    // -- binary targets: hexadecimal pairs ----------------------------------

    /// Spec: "each two bytes of character data are converted to a single byte
    /// (8 bits) of binary data … '01' is converted to a binary 00000001 and
    /// 'FF' is converted to a binary 11111111."
    #[test]
    fn a_binary_parameter_decodes_hexadecimal_pairs() {
        assert_eq!(
            convert("01FF", SqlDataType::EXT_BINARY),
            ColumnValue::Bytes(vec![0x01, 0xFF])
        );
    }

    #[test]
    fn a_varbinary_parameter_accepts_lower_case_hexadecimal() {
        assert_eq!(
            convert("deadbeef", SqlDataType::EXT_VAR_BINARY),
            ColumnValue::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF])
        );
    }

    /// Spec: "if the length of the character string is odd, the last byte of
    /// the string … is not converted." An odd length is not an error.
    #[test]
    fn a_binary_parameter_of_odd_length_drops_the_last_character() {
        assert_eq!(
            convert("01FFA", SqlDataType::EXT_LONG_VAR_BINARY),
            ColumnValue::Bytes(vec![0x01, 0xFF])
        );
    }

    /// Spec: "Data value is not a hexadecimal value" → 22018.
    #[test]
    fn a_binary_parameter_that_is_not_hexadecimal_is_22018() {
        assert_eq!(sqlstate("zz", SqlDataType::EXT_BINARY), "22018");
    }

    // -- SQL_TYPE_DATE ------------------------------------------------------

    #[test]
    fn a_date_parameter_becomes_a_date() {
        assert_eq!(
            convert("2024-03-17", SqlDataType::DATE),
            ColumnValue::Date {
                year: 2024,
                month: 3,
                day: 17
            }
        );
    }

    /// Spec: "Data value is a valid *ODBC-timestamp-literal*; time portion is
    /// zero" → n/a, i.e. accepted.
    #[test]
    fn a_date_parameter_accepts_a_timestamp_with_a_zero_time() {
        assert_eq!(
            convert("2024-03-17 00:00:00", SqlDataType::DATE),
            ColumnValue::Date {
                year: 2024,
                month: 3,
                day: 17
            }
        );
    }

    /// Spec: "time portion is nonzero" → 22008. The time would be dropped.
    #[test]
    fn a_date_parameter_with_a_nonzero_time_is_22008() {
        assert_eq!(sqlstate("2024-03-17 10:30:00", SqlDataType::DATE), "22008");
    }

    #[test]
    fn a_date_parameter_that_is_not_a_date_literal_is_22018() {
        assert_eq!(sqlstate("17/03/2024", SqlDataType::DATE), "22018");
    }

    // -- SQL_TYPE_TIME ------------------------------------------------------

    #[test]
    fn a_time_parameter_becomes_a_time() {
        assert_eq!(
            convert("10:30:15", SqlDataType::TIME),
            ColumnValue::Time {
                hour: 10,
                minute: 30,
                second: 15,
                fraction: 0
            }
        );
    }

    /// Spec: "Data value is a valid *ODBC-timestamp-literal*; fractional
    /// seconds portion is zero" → accepted; "[b] The date portion of the
    /// timestamp is ignored."
    #[test]
    fn a_time_parameter_accepts_a_timestamp_and_ignores_its_date() {
        assert_eq!(
            convert("2024-03-17 10:30:15", SqlDataType::TIME),
            ColumnValue::Time {
                hour: 10,
                minute: 30,
                second: 15,
                fraction: 0
            }
        );
    }

    /// Spec: "fractional seconds portion is nonzero" → 22008.
    #[test]
    fn a_time_parameter_from_a_timestamp_with_a_fraction_is_22008() {
        assert_eq!(
            sqlstate("2024-03-17 10:30:15.5", SqlDataType::TIME),
            "22008"
        );
    }

    #[test]
    fn a_time_parameter_that_is_not_a_time_literal_is_22018() {
        assert_eq!(sqlstate("half past ten", SqlDataType::TIME), "22018");
    }

    // -- SQL_TYPE_TIMESTAMP -------------------------------------------------

    #[test]
    fn a_timestamp_parameter_becomes_a_timestamp() {
        assert_eq!(
            convert("2024-03-17 10:30:15.25", SqlDataType::TIMESTAMP),
            ColumnValue::Timestamp {
                year: 2024,
                month: 3,
                day: 17,
                hour: 10,
                minute: 30,
                second: 15,
                fraction: 250_000_000
            }
        );
    }

    /// Spec: "Data value is a valid *ODBC-date-literal*" → accepted; "[c] The
    /// time portion of the timestamp is set to zero."
    #[test]
    fn a_timestamp_parameter_accepts_a_bare_date_at_midnight() {
        assert_eq!(
            convert("2024-03-17", SqlDataType::TIMESTAMP),
            ColumnValue::Timestamp {
                year: 2024,
                month: 3,
                day: 17,
                hour: 0,
                minute: 0,
                second: 0,
                fraction: 0
            }
        );
    }

    /// Spec: "Data value is a valid *ODBC-time-literal*" → accepted; "[d] The
    /// date portion of the timestamp is set to the current date."
    #[test]
    fn a_timestamp_parameter_from_a_bare_time_uses_the_current_date() {
        let ColumnValue::Timestamp {
            year,
            month,
            day,
            hour,
            minute,
            second,
            fraction,
        } = convert("10:30:15", SqlDataType::TIMESTAMP)
        else {
            panic!("a time literal should convert to a timestamp");
        };
        assert_eq!((hour, minute, second, fraction), (10, 30, 15, 0));
        assert_eq!(
            (year, month, day),
            crate::column_value::current_utc_date(),
            "the date portion should be today"
        );
    }

    #[test]
    fn a_timestamp_parameter_that_is_not_a_datetime_literal_is_22018() {
        assert_eq!(sqlstate("yesterday", SqlDataType::TIMESTAMP), "22018");
    }

    // -- the ODBC 2.0 datetime type codes -----------------------------------

    /// `SQL_DATE`/`SQL_TIME`/`SQL_TIMESTAMP` (9/10/11) are the ODBC 2.0
    /// spellings of the 91/92/93 codes. `types::col_attr` already treats the
    /// two spellings alike, and a parameter bound by an ODBC 2.x application
    /// must not lose its type just for using the older number.
    #[test]
    fn the_odbc_2_0_timestamp_code_converts_like_the_3_0_one() {
        assert_eq!(
            convert("2024-03-17 10:30:15", SqlDataType::EXT_TIMESTAMP),
            convert("2024-03-17 10:30:15", SqlDataType::TIMESTAMP)
        );
    }

    #[test]
    fn the_odbc_2_0_time_code_converts_like_the_3_0_one() {
        assert_eq!(
            convert("10:30:15", SqlDataType::EXT_TIME_OR_INTERVAL),
            convert("10:30:15", SqlDataType::TIME)
        );
    }

    // -- types the table does not list --------------------------------------

    /// `SQL_GUID` is absent from the character C table, so the pairing is
    /// `SQLBindParameter`'s 07006 to reject, not this function's to convert.
    #[test]
    fn a_guid_parameter_stays_a_string() {
        assert_eq!(
            convert(
                "00112233-4455-6677-8899-aabbccddeeff",
                SqlDataType::EXT_GUID
            ),
            ColumnValue::String("00112233-4455-6677-8899-aabbccddeeff".into())
        );
    }

    /// A driver-specific `ParameterType` only the backend can interpret.
    #[test]
    fn an_unrecognised_sql_type_stays_a_string() {
        assert_eq!(
            convert("whatever", SqlDataType(4242)),
            ColumnValue::String("whatever".into())
        );
    }

    // -- character targets: the declared size -------------------------------

    /// Convert as `VARCHAR(col_size)` and friends. `decimal_digits` is
    /// irrelevant to a character target, so it is fixed at 0.
    fn sized(text: &str, sql_type: SqlDataType, col_size: ULen) -> Result<ColumnValue, OdbcError> {
        text_to_sql_type(text, sql_type, col_size, 0)
    }

    #[test]
    fn a_character_value_shorter_than_the_declared_size_is_accepted() {
        assert_eq!(
            sized("abcd", SqlDataType::VARCHAR, 5).expect("four characters fit VARCHAR(5)"),
            ColumnValue::String("abcd".into())
        );
    }

    #[test]
    fn a_character_value_exactly_the_declared_size_is_accepted() {
        assert_eq!(
            sized("abcde", SqlDataType::VARCHAR, 5).expect("five characters fit VARCHAR(5)"),
            ColumnValue::String("abcde".into())
        );
    }

    /// The reported defect: a ten-character string declared `VARCHAR(5)`
    /// reached the backend whole.
    #[test]
    fn a_character_value_over_the_declared_size_is_22001() {
        assert_eq!(
            state_of(sized("0123456789", SqlDataType::VARCHAR, 5)),
            "22001"
        );
    }

    #[test]
    fn the_declared_size_is_checked_for_char_and_longvarchar_too() {
        assert_eq!(state_of(sized("abcdef", SqlDataType::CHAR, 5)), "22001");
        assert_eq!(
            state_of(sized("abcdef", SqlDataType::EXT_LONG_VARCHAR, 5)),
            "22001"
        );
    }

    /// A `ColumnSize` of 0 is "the application declared no size", not a
    /// zero-length column.
    #[test]
    fn a_character_value_is_unchecked_when_no_size_was_declared() {
        assert_eq!(
            sized("0123456789", SqlDataType::VARCHAR, 0).expect("no declared size, no check"),
            ColumnValue::String("0123456789".into())
        );
    }

    /// The deliberate deviation from the narrow row's "byte length" wording:
    /// `ColumnSize` is declared in characters, and these five characters are
    /// nine UTF-8 bytes. Reading the row literally would reject a value bound
    /// at its column's own declared length.
    #[test]
    fn a_multibyte_character_value_is_measured_in_characters_not_bytes() {
        assert_eq!(
            sized("äöüßx", SqlDataType::VARCHAR, 5).expect("five characters fit VARCHAR(5)"),
            ColumnValue::String("äöüßx".into())
        );
    }

    #[test]
    fn a_wide_character_value_over_the_declared_size_is_22001() {
        assert_eq!(
            state_of(sized("abcdef", SqlDataType::EXT_W_VARCHAR, 5)),
            "22001"
        );
        assert_eq!(
            state_of(sized("abcdef", SqlDataType::EXT_W_CHAR, 5)),
            "22001"
        );
        assert_eq!(
            state_of(sized("abcdef", SqlDataType::EXT_W_LONG_VARCHAR, 5)),
            "22001"
        );
    }

    /// The two rows are genuinely different tests, and an astral character is
    /// where they part company: one character, two UTF-16 code units. Do not
    /// "fix" this into consistency.
    #[test]
    fn an_astral_character_counts_once_narrow_and_twice_wide() {
        assert_eq!(
            sized("😀", SqlDataType::VARCHAR, 1).expect("one character fits VARCHAR(1)"),
            ColumnValue::String("😀".into())
        );
        assert_eq!(
            state_of(sized("😀", SqlDataType::EXT_W_VARCHAR, 1)),
            "22001"
        );
        assert_eq!(
            sized("😀", SqlDataType::EXT_W_VARCHAR, 2).expect("two code units fit WVARCHAR(2)"),
            ColumnValue::String("😀".into())
        );
    }
}
