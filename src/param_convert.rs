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
//! This module owns the size checks all three C-to-SQL tables share, because
//! their rows ask one question of one thing and answering it twice is how two
//! tables come to disagree about one bind.
//!
//! [`check_declared_binary_size`] is shared with [`crate::binary_convert`], the
//! [C to SQL: Binary] table: both tables have a binary-target row asking that
//! the byte string about to be sent not exceed the declared column length.
//! [`check_declared_char_size`], [`check_declared_decimal_size`],
//! [`parse_numeric_literal`] and [`DecimalLiteral`] are shared with
//! [`crate::numeric_convert`], the [C to SQL: Numeric] table, whose character
//! and exact-numeric rows are this module's own questions asked of a number
//! rather than of text.
//!
//! The *verdicts* are not shared along with the primitives, and the
//! exact-numeric row is where the two tables diverge most. This table refuses a
//! value that would lose fractional digits (`22001`); the numeric table's row
//! calls that its "n/a" case and sends the value truncated, with an optional
//! `01S07`. Whole-digit loss is `22001` here and `22003` there. So
//! [`check_declared_decimal_size`] is *this* table's composite answer and the
//! numeric one asks [`DecimalLiteral::whole_digits`] and
//! [`DecimalLiteral::required_scale`] separately, because it needs the two
//! halves to end differently. Sharing the primitives while diverging on what
//! they mean is the point; a shared verdict would have been wrong.
//!
//! [Converting Data from C to SQL Data Types]: https://learn.microsoft.com/en-us/sql/odbc/reference/appendixes/converting-data-from-c-to-sql-data-types
//! [C to SQL: Character]: https://learn.microsoft.com/en-us/sql/odbc/reference/appendixes/c-to-sql-character
//! [C to SQL: Binary]: https://learn.microsoft.com/en-us/sql/odbc/reference/appendixes/c-to-sql-binary
//! [C to SQL: Numeric]: https://learn.microsoft.com/en-us/sql/odbc/reference/appendixes/c-to-sql-numeric

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
pub(crate) fn truncation(text: &str, what: &str) -> OdbcError {
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

/// A well-formed *numeric-literal* whose exponent expands past
/// [`MAX_DECIMAL_EXPANSION_DIGITS`].
///
/// 22001, because the *C to SQL: Character* table's
/// `SQL_DECIMAL`/`SQL_NUMERIC`/integer row offers exactly four outcomes —
/// "Data converted without truncation" (n/a), "Data converted with truncation
/// of fractional digits" (22001), "Conversion of data would result in loss of
/// whole (as opposed to fractional) digits" (22001) and "Data value is not a
/// *numeric-literal*" (22018). Both of the lossy outcomes are 22001 and the row
/// lists no 22003, so a bound that refuses the value on either side of the
/// decimal point lands on the same state whichever way it leans. 22018 would be
/// wrong: the text *is* a numeric-literal.
///
/// The read direction disagrees, and correctly so — see
/// [`DecimalLiteral::to_integer`].
fn unexpandable(text: &str) -> OdbcError {
    OdbcError::general(
        format!(
            "Parameter value {text:?} has an exponent that expands past \
             {MAX_DECIMAL_EXPANSION_DIGITS} digits"
        ),
        SqlState::string_data_right_truncation(),
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
/// `DecimalDigits`. Every row of the table that tests a value against them is
/// enforced here:
///
/// - **Exact numerics.** `SQL_DECIMAL` and `SQL_NUMERIC` — see
///   [`check_declared_decimal_size`], which also records why the four integer
///   types need no such check.
/// - **Character targets.** "Byte length of data > Column length" is 22001 for
///   `SQL_CHAR`/`SQL_VARCHAR`/`SQL_LONGVARCHAR`, and the `SQL_W*` row states
///   the same test in characters. Both are measured **in characters**, and the
///   narrow row's deviation from its own wording is deliberate: `ColumnSize`
///   for a character column is declared in characters, and the row's byte
///   wording dates from when the two coincided. Under UTF-8 they do not —
///   `"äöüßx"` is five characters and nine bytes — so reading it literally
///   would reject a value bound at its column's own declared length, which the
///   data source would have accepted. A false 22001 is a worse outcome than
///   the missing diagnostic this check replaced. An astral character therefore
///   counts once against a `VARCHAR` and twice against a `WVARCHAR`; the two
///   rows are different tests and a test pins both.
/// - **Binary targets.** "(Byte length of data) / 2 > column byte length" is
///   22001, the halving being the hex-pair conversion — see
///   [`check_declared_binary_size`].
///
/// A `col_size` of 0 disables all of them, for the reason
/// [`check_declared_char_size`] records.
///
/// The `SQL_C_BINARY` side of the same question lives in
/// [`crate::binary_convert`], which is the [C to SQL: Binary] table.
///
/// [C to SQL: Character]: https://learn.microsoft.com/en-us/sql/odbc/reference/appendixes/c-to-sql-character
/// [C to SQL: Binary]: https://learn.microsoft.com/en-us/sql/odbc/reference/appendixes/c-to-sql-binary
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
        // Before the size checks, which are about the *declared* column and are
        // skipped entirely when `col_size` is 0. This one is about whether core
        // will materialise the expansion at all, so it applies either way.
        if !literal.expansion_is_bounded() {
            return Err(unexpandable(text));
        }
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

    if crate::binary_convert::is_binary_sql_type(sql_type) {
        return to_binary(text, col_size);
    }

    // Datetimes. The ODBC 2.0 spellings (`SQL_DATE` 9, `SQL_TIME` 10,
    // `SQL_TIMESTAMP` 11) are grouped with their 3.x counterparts, so a
    // parameter bound by an ODBC 2.x application does not lose its type for
    // using the older number.
    //
    // `odbc_sys` names 9 `DATETIME`, which is the *verbose* `SQL_DATETIME`
    // spelling of the same number — but `SQLBindParameter`'s `ParameterType` is
    // a **concise** type, where 9 is `SQL_DATE`. It therefore belongs with date
    // and not with timestamp, which is where it sat until this was checked
    // against AWS's Redshift ODBC driver: `convertCParamDataToSQLData` opens
    // that branch `case SQL_TYPE_DATE: case SQL_DATE:`.
    if sql_type == SqlDataType::DATE || sql_type == SqlDataType::DATETIME {
        return to_date(text);
    }
    if sql_type == SqlDataType::TIME || sql_type == SqlDataType::EXT_TIME_OR_INTERVAL {
        return to_time(text);
    }
    if sql_type == SqlDataType::TIMESTAMP || sql_type == SqlDataType::EXT_TIMESTAMP {
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

/// The maximum precision of an ODBC `SQL_DECIMAL` / `SQL_NUMERIC` value: a
/// `SQL_NUMERIC_STRUCT` carries `SQL_MAX_NUMERIC_LEN` (16) mantissa bytes, and
/// the spec caps its `precision` field at 38 decimal digits.
pub(crate) const MAX_ODBC_DECIMAL_PRECISION: usize = 38;

/// The most digits [`DecimalLiteral`] will *synthesise* while expanding an
/// exponent into plain decimal notation.
///
/// # What is bounded
///
/// The characters a rendering has to *materialise* — the three
/// `"0".repeat(…)` calls, two in [`DecimalLiteral::to_decimal_string`] (one per
/// scale sign) and one in [`DecimalLiteral::to_integer`]'s negative-scale
/// branch. Significant digits the caller spelled out are memory it has already
/// paid for and are not counted, which is what lets the bound sit far below any
/// real precision limit while still refusing only values that would allocate.
///
/// **Leading fractional zeros are the exception, and they are counted.**
/// [`DecimalLiteral::significant`] strips them at parse time, so
/// `to_decimal_string`'s positive-scale branch re-synthesises them with its own
/// `"0".repeat` — they really are materialised, whoever typed them. A literal
/// with every digit
/// written out, `"0."` followed by 1 048 577 zeros and a `1`, therefore reports
/// 1 048 577 synthesised digits and is refused. That is fail-closed and
/// deliberate: the measurement matches what the renderer allocates, which is
/// the property the bound needs. The alternative — counting supplied leading
/// zeros as free — would understate the allocation by exactly their number.
///
/// # Derivation
///
/// The bound has to clear the widest exponent a genuine numeric value can
/// carry. The candidates, largest plain-decimal rendering each:
///
/// | source | synthesised digits |
/// |---|---|
/// | ODBC `SQL_DECIMAL` / `SQL_NUMERIC` | 38 ([`MAX_ODBC_DECIMAL_PRECISION`]) |
/// | IEEE-754 binary64 (`SQL_DOUBLE`, `SQL_C_DOUBLE`) | 323 for the smallest subnormal `4.9e-324`; 307 for `1.8e308` |
/// | PostgreSQL `numeric`, the widest exact type among the mainstream data sources | [`WIDEST_REAL_DATA_SOURCE_EXPANSION`] — 131 072 whole digits and 16 383 fractional, per its own documented limits |
///
/// 2²⁰ clears the largest of those by more than seven times, so no value any of
/// them can hold is refused, while capping the transient *peak* at about
/// **2 MiB** — not 1 MiB: every `"0".repeat` is immediately consumed by a
/// `format!` that copies it, so the padding and the finished string are both
/// live for a moment. That is about three orders of magnitude below the ~2 GiB
/// a single unbounded `i32` exponent asks for, and small enough that exceeding
/// it could never be the difference between a diagnostic and an abort. The
/// `const` assertions below
/// hold the derivation from both sides: tightening the bound onto the ODBC
/// precision alone fails to compile, and so does loosening it back into a
/// hazard.
///
/// # What is actually refused
///
/// Not "an exponent past 2²⁰". The refusal set is per rendering branch, because
/// [`DecimalLiteral::synthesised_digits`] measures
/// [`DecimalLiteral::to_decimal_string`] branch for branch. In terms of
/// `scale`, which is `frac_len − exponent` and so runs *opposite* to the
/// exponent:
///
/// | literal | scale | refused? |
/// |---|---|---|
/// | non-zero mantissa, `scale < −2²⁰` (large non-negative exponent, `"1e2147483646"`) | very negative | **yes** — `"0".repeat(−scale)` |
/// | **zero** mantissa, any `scale ≤ 0` (`"0e2147483646"`) | any ≤ 0 | no — renders `"0"`, allocates nothing |
/// | any mantissa, `scale − significant digits > 2²⁰` (large *negative* exponent, `"0e-2147483647"`, `"1e-2147483647"`) | very positive | **yes** — the padding to `scale` characters happens whether or not the mantissa is zero |
/// | non-zero mantissa, `scale > 0` and enough digits to cover it | positive | no — a decimal point is inserted, nothing is synthesised |
///
/// So a zero mantissa is refused too, at a large negative exponent; only the
/// `scale ≤ 0` half of the zero case is free. And it is still not a range check
/// on the *number*: a value far below an integer target's resolution truncates
/// to zero at no cost through [`DecimalLiteral::to_integer`]'s positive-scale
/// branch, which slices rather than expands and is deliberately not guarded.
///
/// # The accepted range is total
///
/// Everything inside the bound renders; being accepted and rendering
/// successfully are the same thing. That is worth stating because it was once
/// not true: [`DecimalLiteral::to_decimal_string`]'s positive-scale branch used
/// `format!`'s `{digits:0>scale$}` width, which Rust caps at `u16::MAX`, so any
/// `scale` from 65 536 up panicked with "Formatting argument out of range"
/// instead of rendering — a range this bound accepts and PostgreSQL `numeric`
/// can reach. That branch now builds its padding with `"0".repeat`, and two
/// tests pin the old boundary from the far side.
///
/// The alternative — lowering the bound to `u16::MAX` — was rejected: it would
/// have traded a contained panic for wrong answers on legitimate data, since
/// the derivation table above requires accepting scales far beyond 65 535.
///
/// # Why a bound at all
///
/// A failed allocation aborts the process rather than unwinding, so
/// [`crate::panic::panic_safe`] cannot turn it into a `SQL_ERROR`. An exponent
/// arrives from two directions — an application's `SQL_C_CHAR` parameter, and a
/// `ColumnValue::Decimal`/`ColumnValue::String` the *data source* returned — and
/// the second is outside the driver's trust boundary.
pub(crate) const MAX_DECIMAL_EXPANSION_DIGITS: usize = 1 << 20;

/// The most decimal digits a `u128` can hold, and so the most a
/// `SQL_NUMERIC_STRUCT`'s 16-byte `val` can carry.
///
/// `u128::MAX` is 340282366920938463463374607431768211455 — 39 digits. Used by
/// [`DecimalLiteral::to_numeric_struct`] to reject a shift before expanding it
/// into a string, so a large exponent is `22003` rather than a multi-gigabyte
/// allocation.
const MAX_U128_DIGITS: usize = 39;

/// A plain-decimal rendering of the widest exact numeric type any mainstream
/// data source offers: PostgreSQL's `numeric`, documented at up to 131 072
/// digits before the decimal point and 16 383 after it. Every other candidate
/// in [`MAX_DECIMAL_EXPANSION_DIGITS`]' table is narrower by three orders of
/// magnitude.
const WIDEST_REAL_DATA_SOURCE_EXPANSION: usize = 131_072 + 16_383;

/// The number of digits a plain rendering of an IEEE-754 binary64 needs
/// synthesised: 323 leading zeros for the smallest subnormal, `4.9e-324`, which
/// is more than the 307 trailing zeros `1.8e308` needs (digits `18` at scale
/// −307, since one of the two is already left of the point).
const MAX_BINARY64_SYNTHESISED_DIGITS: usize = 323;

// [`MAX_DECIMAL_EXPANSION_DIGITS`]' derivation, checked rather than asserted in
// prose. The bound clears every source in that table with room to spare, so no
// value one of them can hold is refused.
const _: () = assert!(MAX_DECIMAL_EXPANSION_DIGITS > WIDEST_REAL_DATA_SOURCE_EXPANSION);
// Subsumed by the line above — 361 is far below 147 455 — and kept because it
// is the pairing a reader checks first, and because dropping it would leave
// both of its constants unreferenced. It documents; it cannot fail alone.
const _: () = assert!(
    MAX_DECIMAL_EXPANSION_DIGITS > MAX_ODBC_DECIMAL_PRECISION + MAX_BINARY64_SYNTHESISED_DIGITS
);
// And the direction the two above cannot see. They pin the bound from below
// only, so a later "let us be safer still" edit raising it to `1 << 40` would
// compile and silently restore the multi-gigabyte allocation this constant
// exists to prevent — the hazard is a bound that is too *loose*, and only this
// line fails when one is introduced. 2^22 is about 4 MiB, still an order of
// magnitude above the widest row of the table.
const _: () = assert!(MAX_DECIMAL_EXPANSION_DIGITS <= 1 << 22);

/// A parsed *numeric-literal*, as `±digits × 10⁻ˢᶜᵃˡᵉ`.
///
/// Keeping the significant digits as text rather than as a float is what lets a
/// `DECIMAL(38,10)` parameter reach the backend without passing through
/// `f64`'s 53 bits of mantissa. `scale` may be negative, which is how an
/// exponent larger than the fraction's length is carried (`1.5e2` is digits
/// `15` at scale `-1`).
#[derive(Clone)]
pub(crate) struct DecimalLiteral {
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
///
/// Shared with [`crate::numeric_convert`], which canonicalises every numeric C
/// type through a [`DecimalLiteral`] so its exact rows compare digits rather
/// than a value already rounded through `f64`.
pub(crate) fn parse_numeric_literal(s: &str) -> Option<DecimalLiteral> {
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
    /// An exact integer, with no fractional part.
    ///
    /// Built directly rather than by rendering to text and parsing it back,
    /// because [`crate::numeric_convert`] calls this for every bound integer
    /// parameter of every execution — the one place in this family that is on a
    /// hot path rather than a per-statement one.
    pub(crate) fn from_integer(value: i128) -> Self {
        Self {
            negative: value < 0,
            digits: value.unsigned_abs().to_string(),
            scale: 0,
        }
    }

    /// The significant digits with leading zeros removed, never empty.
    pub(crate) fn significant(&self) -> &str {
        let trimmed = self.digits.trim_start_matches('0');
        if trimmed.is_empty() { "0" } else { trimmed }
    }

    pub(crate) fn is_zero(&self) -> bool {
        self.digits.bytes().all(|b| b == b'0')
    }

    /// The digits an expansion to plain decimal notation would have to
    /// synthesise — zeros the source text did not contain.
    ///
    /// This measures [`Self::to_decimal_string`] branch for branch, which is
    /// the property that keeps the bound from refusing a value that costs
    /// nothing to render. In particular a zero at a non-positive scale is
    /// **not** measured by its exponent: that renderer returns `"0"` for it
    /// before reaching any `repeat`, so `"0e2147483646"` synthesises nothing.
    /// A zero at a *positive* scale is a different case and is measured, since
    /// `0.000…` really does pad out to `scale` characters.
    ///
    /// [`Self::to_integer`] expands on one branch only, and applies this there.
    /// See [`MAX_DECIMAL_EXPANSION_DIGITS`].
    fn synthesised_digits(&self) -> usize {
        // `unsigned_abs` rather than `-self.scale`, which overflows at
        // `i32::MIN`. `parse_numeric_literal`'s `checked_sub` cannot reach that
        // value, but nothing about the field's type says so.
        let magnitude = usize::try_from(self.scale.unsigned_abs()).unwrap_or(usize::MAX);
        if self.scale <= 0 {
            if self.is_zero() {
                return 0;
            }
            // Trailing zeros the literal did not spell out.
            magnitude
        } else {
            // The positive-scale branch pads the digits out to `scale`
            // characters; a literal with more digits than that synthesises
            // none, because it only gains a decimal point.
            magnitude.saturating_sub(self.significant().len())
        }
    }

    /// Whether this literal can be expanded within
    /// [`MAX_DECIMAL_EXPANSION_DIGITS`].
    pub(crate) fn expansion_is_bounded(&self) -> bool {
        self.synthesised_digits() <= MAX_DECIMAL_EXPANSION_DIGITS
    }

    /// Render as a plain decimal literal, expanding any exponent. A backend
    /// renders `ColumnValue::Decimal` into SQL verbatim, and `1.5e2` is not
    /// something every data source accepts where `150` is.
    ///
    /// Past [`MAX_DECIMAL_EXPANSION_DIGITS`] the plain form is not rendered and
    /// the exponent form is returned instead. That keeps this function total —
    /// seven call sites across two modules read it, and the exponent form is
    /// still an exact, syntactically valid *numeric-literal* — while removing
    /// the only unbounded allocation on the path. It is a fail-safe, not the
    /// answer: [`text_to_sql_type`] refuses such a value with [`unexpandable`]
    /// before it renders one, so nothing this branch produces is sent to a
    /// data source today. The other six call sites are in
    /// [`crate::numeric_convert`], whose literals come from
    /// [`Self::from_integer`] (scale 0), from `SQL_C_NUMERIC`'s `i8` scale, or
    /// from `f64::to_string`, which emits no exponent at all — so none of them
    /// can reach this branch.
    pub(crate) fn to_decimal_string(&self) -> String {
        let sign = if self.negative && !self.is_zero() {
            "-"
        } else {
            ""
        };
        let digits = self.significant();
        if !self.expansion_is_bounded() {
            return format!("{sign}{digits}e{}", -i64::from(self.scale));
        }
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
            // The padding is built explicitly rather than with `format!`'s
            // `{digits:0>scale$}` width, which Rust caps at `u16::MAX` and
            // which therefore *panicked* for any scale from 65 536 up — a range
            // `MAX_DECIMAL_EXPANSION_DIGITS` accepts and must render, since
            // PostgreSQL `numeric` alone reaches scale 16 383 and the bound is
            // sized for 147 455. Lowering the bound to `u16::MAX` instead would
            // trade a contained panic for a wrong answer on legitimate data.
            //
            // The repeat count is `synthesised_digits`' own expression for this
            // cell — `scale.saturating_sub(significant().len())`, where `digits`
            // *is* `significant()` — so the bound still measures this branch
            // exactly and the allocation is the one already accounted for.
            let zeros = "0".repeat(scale.saturating_sub(digits.len()));
            format!("{sign}0.{zeros}{digits}")
        }
    }

    /// The smallest scale that represents this value exactly.
    ///
    /// A declared scale is compared against this rather than against however
    /// many fractional digits were typed, so `12.3400` fits `DECIMAL(10,2)`:
    /// dropping those two trailing zeros loses nothing, and the spec's
    /// truncation test is about what the conversion would *lose*.
    pub(crate) fn required_scale(&self) -> usize {
        if self.is_zero() || self.scale <= 0 {
            return 0;
        }
        let trailing_zeros = self.digits.len() - self.digits.trim_end_matches('0').len();
        usize::try_from(self.scale)
            .unwrap_or(0)
            .saturating_sub(trailing_zeros)
    }

    /// The same value with its fraction truncated toward zero to `scale`
    /// digits.
    ///
    /// Truncated, not rounded: the *C to SQL: Numeric* table's exact-numeric row
    /// says "with truncated of fractional digits", and this is the conversion
    /// that row describes the driver performing. Used only by
    /// [`crate::numeric_convert`] — this table refuses such a value instead, so
    /// it has nothing to truncate.
    ///
    /// A literal already at or below `scale`, or one with a negative scale
    /// (a whole number with implied trailing zeros), is returned unchanged.
    pub(crate) fn truncated_to_scale(&self, scale: usize) -> DecimalLiteral {
        let Ok(current) = usize::try_from(self.scale) else {
            return self.clone();
        };
        let Some(drop) = current.checked_sub(scale).filter(|d| *d > 0) else {
            return self.clone();
        };
        // Every digit is fractional and all of them go: the value truncates to
        // zero rather than underflowing the slice.
        let keep = self.digits.len().saturating_sub(drop);
        DecimalLiteral {
            negative: self.negative,
            digits: self.digits[..keep].to_owned(),
            scale: i32::try_from(scale).unwrap_or(self.scale),
        }
    }

    /// The number of digits to the left of the decimal point. Zero has none, so
    /// it fits a `DECIMAL(2,2)` that has room for no whole digits at all.
    pub(crate) fn whole_digits(&self) -> usize {
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
    pub(crate) fn fraction_is_zero(&self) -> bool {
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

    /// Render as a `SQL_NUMERIC_STRUCT` for `SQL_C_NUMERIC`, plus whether
    /// fractional digits were dropped doing it.
    ///
    /// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/appendixes/sql-to-c-numeric>
    ///
    /// The struct is `±val × 10⁻ˢᶜᵃˡᵉ` with `val` a little-endian *unsigned*
    /// magnitude and no decimal point, which is the same shape a
    /// [`DecimalLiteral`] already has — so this is a rescale and a base
    /// conversion, not a numeric conversion, and the digits never pass through a
    /// float.
    ///
    /// `val` is exactly [`odbc_sys::MAX_NUMERIC_LEN`] = 16 bytes, so the
    /// magnitude is a `u128` and "does it fit" is `u128::try_from`. A magnitude
    /// that does not fit is the table's third outcome, `22003`; dropping a
    /// non-zero fractional digit is its second, `01S07`, reported through the
    /// returned flag because the row still writes the truncated data.
    ///
    /// Returning the flag rather than the SQLSTATE keeps this function about
    /// the *number*: the caller owns which of the row's three outcomes it is,
    /// and the caller is the one holding the output pointers.
    pub(crate) fn to_numeric_struct(
        &self,
        target: crate::column_value::NumericTarget,
    ) -> Result<(odbc_sys::Numeric, bool), OdbcError> {
        let out_of_range = || {
            OdbcError::general(
                format!(
                    "Numeric value out of range: {} does not fit a SQL_NUMERIC_STRUCT",
                    self.to_decimal_string()
                ),
                SqlState::numeric_value_out_of_range(),
            )
        };

        // A declared precision means the application dictated the layout; zero
        // means it declared nothing, and the value describes itself. Zero is
        // not a legal SQL_NUMERIC_STRUCT precision, which is what makes it
        // usable as the "unspecified" marker.
        let declared = target.precision != 0;
        let scale = if declared {
            i32::from(target.scale)
        } else {
            i32::try_from(self.required_scale()).map_err(|_| out_of_range())?
        };

        // `val` holds `value × 10^scale`, so the digit string shifts by the
        // difference between the scale asked for and the one this literal
        // carries.
        let shift = scale.checked_sub(self.scale).ok_or_else(out_of_range)?;
        let (digits, fraction_lost) = if shift >= 0 {
            let pad = usize::try_from(shift).map_err(|_| out_of_range())?;
            // A u128 holds 39 digits; anything past that cannot fit `val`, and
            // this is also what stops a large exponent expanding into a
            // multi-gigabyte string before the range check can reject it.
            if pad > MAX_U128_DIGITS {
                return Err(out_of_range());
            }
            let mut d = String::with_capacity(self.digits.len() + pad);
            d.push_str(&self.digits);
            d.extend(std::iter::repeat_n('0', pad));
            (d, false)
        } else {
            let drop = usize::try_from(-shift).map_err(|_| out_of_range())?;
            let keep = self.digits.len().saturating_sub(drop);
            // Truncated toward zero, per the row's "truncation of fractional
            // digits". Only a non-zero digit is a loss: `1.500` to scale 1 is
            // exact.
            let lost = self.digits[keep..].bytes().any(|b| b != b'0');
            (self.digits[..keep].to_owned(), lost)
        };

        let magnitude: u128 = if digits.is_empty() {
            0
        } else {
            digits.parse::<u128>().map_err(|_| out_of_range())?
        };

        // A declared precision is a hard limit on the digit count, not a hint:
        // the application sized its reading of `val` by it.
        let significant = digits.trim_start_matches('0').len();
        if declared && significant > usize::try_from(target.precision).unwrap_or(0) {
            return Err(out_of_range());
        }

        let precision = if declared {
            u8::try_from(target.precision).map_err(|_| out_of_range())?
        } else {
            u8::try_from(significant.max(1)).map_err(|_| out_of_range())?
        };

        Ok((
            odbc_sys::Numeric {
                precision,
                scale: i8::try_from(scale).map_err(|_| out_of_range())?,
                // odbc-sys: "1 if positive, 0 if negative". The opposite of a
                // sign bit, and the field most easily inverted by habit.
                sign: u8::from(!self.negative),
                val: magnitude.to_le_bytes(),
            },
            fraction_lost,
        ))
    }

    /// The value truncated toward zero, or `None` if it does not fit `i128`.
    ///
    /// A literal past [`MAX_DECIMAL_EXPANSION_DIGITS`] is `None` too, and
    /// deliberately reported the same way: it is a magnitude no integer target
    /// holds either, and every caller already maps `None` to the SQLSTATE its
    /// own conversion table names for that — 22001 in [`to_integer`] (*C to
    /// SQL: Character*), 22003 in `column_value::write_exact_integer` (*SQL to
    /// C: Character* and *SQL to C: Numeric*) and in
    /// [`crate::numeric_convert`] (*C to SQL: Numeric*). The two directions
    /// genuinely disagree about the state, so the bound must not name one.
    pub(crate) fn to_integer(&self) -> Option<i128> {
        // REQUIRED FOR THE BOUND TO HOLD — not an optimisation, however much it
        // reads like one. This is the only thing standing between a zero
        // mantissa and `"0".repeat(2_147_483_646)`.
        //
        // The cell it covers is `is_zero()` with `scale <= 0` — a zero at a
        // large *non-negative exponent*, such as `"0e2147483646"`.
        // [`Self::synthesised_digits`] reports **0** for that cell by design,
        // because [`Self::to_decimal_string`] renders it as `"0"` before
        // reaching any `repeat`. So `expansion_is_bounded()` is `true` there,
        // the guard below correctly does not fire, and without this early
        // return the negative-scale branch would expand the exponent in full.
        // The two guards are complementary; neither covers this cell alone.
        //
        // Deleting this reintroduces the original denial of service, and the
        // test that fails is
        // `tests::a_zero_mantissa_with_a_pathological_exponent_still_reaches_an_integer`
        // — by wrong result as well as by allocation, since the expansion ends
        // in a `parse::<i128>` failure and a `None`.
        if self.is_zero() {
            return Some(0);
        }
        let whole = if self.scale <= 0 {
            // The one branch that expands. The positive-scale branch below
            // slices digits the caller supplied and allocates nothing beyond
            // them, so the bound does not apply to it: `1e-2000000` truncates
            // toward zero at no cost and must keep doing so.
            if !self.expansion_is_bounded() {
                return None;
            }
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
pub(crate) fn oversized(actual: usize, unit: &str, declared: ULen) -> OdbcError {
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
///
/// Shared with [`crate::numeric_convert`], whose first two rows ask this of a
/// rendered number: "Number of digits > Column byte length" and its wide
/// counterpart. The caller picking the unit is what lets one function serve
/// both tables' narrow and wide rows alike.
pub(crate) fn check_declared_char_size(measured: usize, col_size: ULen) -> Result<(), OdbcError> {
    if col_size == 0 {
        return Ok(());
    }
    if measured > col_size {
        return Err(oversized(measured, "characters", col_size));
    }
    Ok(())
}

/// Apply the declared `ColumnSize` to a binary target.
///
/// This serves a row in each of the two C-to-SQL tables, because both ask the
/// same question of the same thing: the byte string about to be sent must not
/// exceed the declared column length. [C to SQL: Character] words it as
/// "(Byte length of data) / 2 > column byte length" — the halving is the
/// hex-pair conversion, so the test is the produced byte count — and
/// [C to SQL: Binary]'s binary row as "Length of data > column length".
///
/// `len` is therefore the number of bytes that will reach the backend, not the
/// length of whatever the application handed over. A `col_size` of 0 disables
/// the check, for the reason [`check_declared_char_size`] records.
///
/// [C to SQL: Character]: https://learn.microsoft.com/en-us/sql/odbc/reference/appendixes/c-to-sql-character
/// [C to SQL: Binary]: https://learn.microsoft.com/en-us/sql/odbc/reference/appendixes/c-to-sql-binary
pub(crate) fn check_declared_binary_size(len: usize, col_size: ULen) -> Result<(), OdbcError> {
    if col_size == 0 {
        return Ok(());
    }
    if len > col_size {
        return Err(oversized(len, "bytes", col_size));
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
///
/// **Not** shared with [`crate::numeric_convert`], though it looks as though it
/// should be. This function bundles two tests, and that table's exact-numeric
/// row disagrees with this one about both:
///
/// | | this table | *C to SQL: Numeric* |
/// |---|---|---|
/// | fractional truncation | `22001`, refused | its "n/a" case — sent truncated, optional `01S07` |
/// | whole-digit loss | `22001` | `22003` |
///
/// A shared verdict would therefore have been wrong in both halves, not merely
/// mislabelled. That table calls [`DecimalLiteral::required_scale`] and
/// [`DecimalLiteral::whole_digits`] directly, so the two primitives stay shared
/// and only the composite answer is per-table.
pub(crate) fn check_declared_decimal_size(
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
fn to_binary(text: &str, col_size: ULen) -> Result<ColumnValue, OdbcError> {
    if !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(not_a_literal(text, "hexadecimal"));
    }
    let digits = text.as_bytes();
    let bytes: Vec<u8> = digits
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
    check_declared_binary_size(bytes.len(), col_size)?;
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

    // ---------------------------------------------------------------------
    // to_numeric_struct — the SQL to C: Numeric exact row, as a number
    // ---------------------------------------------------------------------

    use crate::column_value::NumericTarget;

    /// Parse and convert in one step, for the tests below.
    fn to_numeric(text: &str, target: NumericTarget) -> (odbc_sys::Numeric, bool) {
        parse_numeric_literal(text)
            .expect("test input must be a numeric literal")
            .to_numeric_struct(target)
            .expect("test input must fit a SQL_NUMERIC_STRUCT")
    }

    /// `val` is documented as "a little-endian array of bytes ... an unsigned
    /// integer without any decimal point", and `odbc-sys` gives the canonical
    /// example: "-123.45 with precision 5 and scale 2 is stored as 12345".
    /// This is that example, verbatim.
    #[test]
    fn the_odbc_sys_worked_example_round_trips() {
        let (n, lost) = to_numeric(
            "-123.45",
            NumericTarget {
                precision: 5,
                scale: 2,
            },
        );
        assert_eq!(n.precision, 5);
        assert_eq!(n.scale, 2);
        assert_eq!(u128::from_le_bytes(n.val), 12345);
        assert!(!lost, "the declared scale holds every digit");
        assert_eq!(n.sign, 0, "negative");
    }

    /// The sign byte is the field most likely to be inverted by habit:
    /// `odbc-sys` documents it as "1 if positive, 0 if negative", which is the
    /// opposite of a sign *bit*. Both directions, so an inversion cannot pass.
    #[test]
    fn a_negative_value_sets_the_numeric_sign_byte_to_zero() {
        let unspecified = NumericTarget::UNSPECIFIED;
        assert_eq!(to_numeric("-1", unspecified).0.sign, 0, "negative is 0");
        assert_eq!(to_numeric("1", unspecified).0.sign, 1, "positive is 1");
        // Zero is not negative.
        assert_eq!(to_numeric("0", unspecified).0.sign, 1);
        // The magnitude is unsigned: -1 and 1 differ only in `sign`.
        assert_eq!(
            u128::from_le_bytes(to_numeric("-1", unspecified).0.val),
            u128::from_le_bytes(to_numeric("1", unspecified).0.val),
        );
    }

    /// With no declared precision the value describes itself, which is what the
    /// struct's own `precision`/`scale` fields are for. Zero is not a legal
    /// precision, so it is usable as the "application said nothing" marker.
    #[test]
    fn an_unspecified_target_takes_precision_and_scale_from_the_value() {
        let (n, lost) = to_numeric("12.345", NumericTarget::UNSPECIFIED);
        assert_eq!(n.scale, 3);
        assert_eq!(n.precision, 5);
        assert_eq!(u128::from_le_bytes(n.val), 12345);
        assert!(!lost);
    }

    /// The row's second outcome: "Data converted with truncation of fractional
    /// digits" → truncated data *and* `01S07`. The data is still written, so
    /// this reports a flag rather than an error, and the caller decides.
    #[test]
    fn dropping_a_non_zero_fractional_digit_reports_truncation() {
        let (n, lost) = to_numeric(
            "1.239",
            NumericTarget {
                precision: 5,
                scale: 2,
            },
        );
        assert_eq!(u128::from_le_bytes(n.val), 123, "truncated toward zero");
        assert_eq!(n.scale, 2);
        assert!(lost, "the 9 was dropped and 01S07 says so");
    }

    /// The guard against a false `01S07`: dropping a *zero* loses nothing.
    /// `1.500` at scale 1 is exactly `1.5`, and the spec's truncation test is
    /// about what the conversion would lose.
    #[test]
    fn dropping_only_zeros_is_not_truncation() {
        let (n, lost) = to_numeric(
            "1.500",
            NumericTarget {
                precision: 5,
                scale: 1,
            },
        );
        assert_eq!(u128::from_le_bytes(n.val), 15);
        assert!(!lost, "1.500 -> 1.5 loses nothing");
    }

    /// Scaling *up* pads with zeros and loses nothing: `1.5` at scale 4 is
    /// `15000 × 10⁻⁴`.
    #[test]
    fn a_larger_declared_scale_pads_rather_than_truncates() {
        let (n, lost) = to_numeric(
            "1.5",
            NumericTarget {
                precision: 10,
                scale: 4,
            },
        );
        assert_eq!(u128::from_le_bytes(n.val), 15000);
        assert_eq!(n.scale, 4);
        assert!(!lost);
    }

    /// A negative literal scale is how an exponent larger than the fraction is
    /// carried — `1.5e2` is digits `15` at scale `-1`. It must reach `val` as
    /// 150, not 15.
    #[test]
    fn an_exponent_literal_expands_into_the_magnitude() {
        let (n, _) = to_numeric("1.5e2", NumericTarget::UNSPECIFIED);
        assert_eq!(u128::from_le_bytes(n.val), 150);
        assert_eq!(n.scale, 0);
    }

    /// The row's third outcome, `22003`: a magnitude no `SQL_NUMERIC_STRUCT`
    /// holds. `val` is 16 bytes, so the bound is `u128::MAX`.
    ///
    /// The digit *count* and the bound are not the same test, which is the
    /// trap here. `u128::MAX` is 340282366920938463463374607431768211455 — 39
    /// digits — but only the first ~3.4 of every 10 such numbers fit, so 39
    /// nines (≈10³⁹) overflows while 38 nines (≈10³⁸) does not.
    /// [`MAX_U128_DIGITS`] is therefore only a cheap pre-expansion guard
    /// against a pathological exponent; the real bound is the `parse::<u128>`
    /// itself.
    #[test]
    fn a_magnitude_past_u128_is_22003() {
        for digits in [39, 40] {
            let err = parse_numeric_literal(&"9".repeat(digits))
                .expect("a numeric literal")
                .to_numeric_struct(NumericTarget::UNSPECIFIED)
                .expect_err("{digits} nines must not fit 16 bytes");
            assert_eq!(
                err.sqlstate().as_str(),
                crate::types::sql_state::NUMERIC_VALUE_OUT_OF_RANGE,
                "{digits} nines",
            );
        }
        // 38 nines is the widest all-nines magnitude that does fit, so the
        // boundary is exercised from both sides.
        let (n, _) = to_numeric(&"9".repeat(38), NumericTarget::UNSPECIFIED);
        assert_eq!(u128::from_le_bytes(n.val), 10u128.pow(38) - 1);
    }

    /// A declared precision is a limit the application sized its reading by,
    /// not a hint: more significant digits than it allows is `22003`.
    #[test]
    fn more_digits_than_the_declared_precision_is_22003() {
        let err = parse_numeric_literal("123456")
            .expect("a numeric literal")
            .to_numeric_struct(NumericTarget {
                precision: 3,
                scale: 0,
            })
            .expect_err("six digits do not fit a declared precision of three");
        assert_eq!(
            err.sqlstate().as_str(),
            crate::types::sql_state::NUMERIC_VALUE_OUT_OF_RANGE
        );
    }

    /// A pathological exponent must be rejected by the digit bound *before* it
    /// is expanded into a string, or the range check is reached only after a
    /// multi-gigabyte allocation. This is the same denial-of-service shape
    /// `to_integer`'s own guards exist for.
    #[test]
    fn a_pathological_exponent_is_rejected_without_expanding_it() {
        let err = parse_numeric_literal("1e2000000000")
            .expect("a numeric literal")
            .to_numeric_struct(NumericTarget::UNSPECIFIED)
            .expect_err("must not expand two billion digits");
        assert_eq!(
            err.sqlstate().as_str(),
            crate::types::sql_state::NUMERIC_VALUE_OUT_OF_RANGE
        );
    }

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

    // -- pathological exponents ---------------------------------------------

    /// The exponent the audit reported. Before [`MAX_DECIMAL_EXPANSION_DIGITS`]
    /// this reached `"0".repeat(2_147_483_646)` inside
    /// [`DecimalLiteral::to_integer`] — a ~2 GB allocation, and the `format!`
    /// on the next line copies it — where an allocation failure aborts the
    /// process rather than unwinding, so `panic_safe` cannot contain it.
    ///
    /// 22001 is the *C to SQL: Character* table's verdict for this row:
    /// "Conversion of data would result in loss of whole (as opposed to
    /// fractional) digits" → 22001. That row lists no 22003 at all.
    #[test]
    fn a_pathological_exponent_bound_as_bigint_is_refused() {
        assert_eq!(sqlstate("1e2147483646", SqlDataType::EXT_BIG_INT), "22001");
    }

    /// The same value against `SQL_DECIMAL` with no declared size — the one
    /// path into [`DecimalLiteral::to_decimal_string`] with no size check in
    /// front of it, since [`check_declared_decimal_size`] returns `Ok` when
    /// `col_size` is 0.
    #[test]
    fn a_pathological_exponent_bound_as_decimal_is_refused() {
        assert_eq!(decimal_sqlstate("1e2147483646", 0, 0), "22001");
    }

    /// A negative exponent expands the other way — the positive-scale branch of
    /// [`DecimalLiteral::to_decimal_string`] pads the digits out to `scale`
    /// characters — so it needs the same bound.
    #[test]
    fn a_pathological_negative_exponent_bound_as_decimal_is_refused() {
        assert_eq!(decimal_sqlstate("1e-2147483647", 0, 0), "22001");
    }

    /// One below that overflows `parse_numeric_literal`'s scale arithmetic, so
    /// it never becomes a literal at all and takes the row's last line instead.
    #[test]
    fn an_exponent_below_i32_min_is_not_a_numeric_literal() {
        assert_eq!(decimal_sqlstate("1e-2147483648", 0, 0), "22018");
    }

    /// And one above `i32::MAX` fails the exponent's own `parse::<i32>`.
    #[test]
    fn an_exponent_above_i32_max_is_not_a_numeric_literal() {
        assert_eq!(decimal_sqlstate("1e2147483648", 0, 0), "22018");
    }

    /// Exactly at the bound still renders, which is what makes the bound a
    /// resource limit rather than a range check on the value.
    #[test]
    fn a_literal_at_the_expansion_limit_still_converts() {
        let text = format!("1e{MAX_DECIMAL_EXPANSION_DIGITS}");
        match convert_decimal(&text, 0, 0) {
            ColumnValue::Decimal(rendered) => {
                assert_eq!(rendered.len(), MAX_DECIMAL_EXPANSION_DIGITS + 1);
                assert!(rendered.starts_with("10"));
            }
            other => panic!("a decimal target yields ColumnValue::Decimal, got {other:?}"),
        }
    }

    /// One synthesised digit past it is refused.
    #[test]
    fn a_literal_one_digit_past_the_expansion_limit_is_refused() {
        let text = format!("1e{}", MAX_DECIMAL_EXPANSION_DIGITS + 1);
        assert_eq!(decimal_sqlstate(&text, 0, 0), "22001");
    }

    #[test]
    fn an_ordinary_positive_exponent_is_unaffected() {
        assert_eq!(
            convert("1e18", SqlDataType::EXT_BIG_INT),
            ColumnValue::I64(1_000_000_000_000_000_000)
        );
    }

    #[test]
    fn an_ordinary_negative_exponent_is_unaffected() {
        assert_eq!(
            convert_decimal("1.5e-10", 0, 0),
            ColumnValue::Decimal("0.00000000015".into())
        );
    }

    /// The 38 significant digits `SQL_MAX_NUMERIC_LEN` allows, written out in
    /// full: nothing is synthesised, so the bound never sees it.
    #[test]
    fn a_full_precision_38_digit_decimal_is_unaffected() {
        let text = "1".repeat(MAX_ODBC_DECIMAL_PRECISION);
        assert_eq!(
            convert_decimal(&text, 0, 0),
            ColumnValue::Decimal(text.clone())
        );
    }

    /// Zero is zero at any exponent, and `to_decimal_string` returns `"0"` for
    /// it without reaching a `repeat`. The bound must not refuse what costs
    /// nothing, so it measures that branch as synthesising nothing.
    #[test]
    fn a_zero_mantissa_with_a_pathological_exponent_still_converts() {
        assert_eq!(
            convert_decimal("0e2147483646", 0, 0),
            ColumnValue::Decimal("0".into())
        );
    }

    #[test]
    fn a_zero_mantissa_with_a_pathological_exponent_still_reaches_an_integer() {
        assert_eq!(
            convert("0e2147483646", SqlDataType::EXT_BIG_INT),
            ColumnValue::I64(0)
        );
    }

    /// A zero at a *positive* scale is the opposite case and is bounded: that
    /// branch really does pad out to `scale` characters, so `0.000…` with two
    /// million places is refused where `"0e2147483646"` above is not.
    #[test]
    fn a_zero_mantissa_at_a_pathological_positive_scale_is_refused() {
        assert_eq!(decimal_sqlstate("0e-2147483647", 0, 0), "22001");
    }

    /// The security invariant, across all four rendering cells:
    /// `synthesised_digits` must never *understate* what `to_decimal_string`
    /// materialises, or the bound would measure less than the allocation it is
    /// there to cap. Everything the renderer emits is either a supplied
    /// significant digit, a synthesised zero, or one of at most three
    /// structural characters (a sign and `"0."`).
    ///
    /// Checked here rather than only reasoned about, because the two
    /// expressions live in different functions and only a test notices when one
    /// of them moves.
    #[test]
    fn synthesised_digits_never_understates_what_the_renderer_materialises() {
        const STRUCTURAL: usize = 3; // sign, and the "0." of a bare fraction
        for text in [
            "0e2147483646", // zero, scale <= 0: renders "0"
            "0",            // zero, scale 0
            "-0.000",       // negative zero at a positive scale
            "1e18",         // non-zero, scale < 0: trailing zeros synthesised
            "-1e18",
            "123.45", // scale > 0, digits cover it: a point is inserted
            "-123.45",
            "1e-65536", // scale > 0, padding synthesised — the tight case
            "-1.5e-10",
            "15e-200000",
            "5e-324",
        ] {
            let literal = parse_numeric_literal(text).expect("a numeric literal");
            let rendered = literal.to_decimal_string();
            let accounted = STRUCTURAL + literal.significant().len() + literal.synthesised_digits();
            assert!(
                rendered.len() <= accounted,
                "{text}: rendered {} characters but only {accounted} were accounted for",
                rendered.len(),
            );
        }
    }

    /// Rust caps a `format!` width at `u16::MAX`, so the positive-scale branch
    /// used to panic here rather than render. It builds its padding explicitly
    /// now, so the whole accepted range is total. This is the exact boundary:
    /// 65 535 always worked, 65 536 did not.
    #[test]
    fn a_scale_one_past_the_format_width_limit_renders() {
        let scale = usize::from(u16::MAX) + 1;
        assert_eq!(
            convert_decimal(&format!("1e-{scale}"), 0, 0),
            ColumnValue::Decimal(format!("0.{}1", "0".repeat(scale - 1)))
        );
    }

    /// And well past it, still inside [`MAX_DECIMAL_EXPANSION_DIGITS`].
    #[test]
    fn a_scale_far_past_the_format_width_limit_renders() {
        let scale = 200_000usize;
        assert_eq!(
            convert_decimal(&format!("15e-{scale}"), 0, 0),
            ColumnValue::Decimal(format!("0.{}15", "0".repeat(scale - 2)))
        );
    }

    /// The smallest positive `SQL_DOUBLE` subnormal, in exponent form: 324
    /// synthesised digits, which the bound must not refuse.
    #[test]
    fn the_smallest_binary64_subnormal_still_expands() {
        assert_eq!(
            convert_decimal("5e-324", 0, 0),
            ColumnValue::Decimal(format!("0.{}5", "0".repeat(323)))
        );
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

    /// `SqlDataType::DATETIME` is the number 9, which `odbc_sys` names after
    /// the *verbose* `SQL_DATETIME`. `SQLBindParameter`'s `ParameterType` is a
    /// **concise** type, where 9 is the ODBC 2.0 `SQL_DATE` — so an ODBC 2.x
    /// application binding a date parameter gets a date, not a timestamp. AWS's
    /// Redshift ODBC driver reads it the same way, opening that branch of
    /// `convertCParamDataToSQLData` with `case SQL_TYPE_DATE: case SQL_DATE:`.
    #[test]
    fn the_2x_date_spelling_converts_to_a_date_not_a_timestamp() {
        assert_eq!(
            convert("2026-07-29", SqlDataType::DATETIME),
            ColumnValue::Date {
                year: 2026,
                month: 7,
                day: 29
            }
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

    // -- binary targets: the declared size ----------------------------------

    /// Eight hex characters are four bytes, which is what `ColumnSize`
    /// counts for a binary column.
    #[test]
    fn a_binary_value_exactly_the_declared_size_is_accepted() {
        assert_eq!(
            sized("DEADBEEF", SqlDataType::EXT_VAR_BINARY, 4).expect("four bytes fit VARBINARY(4)"),
            ColumnValue::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF])
        );
    }

    #[test]
    fn a_binary_value_over_the_declared_size_is_22001() {
        assert_eq!(
            state_of(sized("DEADBEEF", SqlDataType::EXT_VAR_BINARY, 3)),
            "22001"
        );
    }

    #[test]
    fn the_declared_size_is_checked_for_binary_and_longvarbinary_too() {
        assert_eq!(
            state_of(sized("DEADBEEF", SqlDataType::EXT_BINARY, 3)),
            "22001"
        );
        assert_eq!(
            state_of(sized("DEADBEEF", SqlDataType::EXT_LONG_VAR_BINARY, 3)),
            "22001"
        );
    }

    #[test]
    fn a_binary_value_is_unchecked_when_no_size_was_declared() {
        assert_eq!(
            sized("DEADBEEF", SqlDataType::EXT_VAR_BINARY, 0).expect("no declared size, no check"),
            ColumnValue::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF])
        );
    }

    /// "if the length of the character string is odd, the last byte of the
    /// string ... is not converted" — so the dropped digit is not counted
    /// against the declared size either. Five hex characters are two bytes.
    #[test]
    fn an_odd_length_hex_value_counts_only_the_bytes_it_produces() {
        assert_eq!(
            sized("DEADB", SqlDataType::EXT_VAR_BINARY, 2).expect("two bytes fit VARBINARY(2)"),
            ColumnValue::Bytes(vec![0xDE, 0xAD])
        );
        assert_eq!(
            state_of(sized("DEADB", SqlDataType::EXT_VAR_BINARY, 1)),
            "22001"
        );
    }
}
