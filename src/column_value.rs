//! `write_column_value` marshals a [`crate::types::ColumnValue`] into an
//! application buffer for `SQLGetData` (NULL, truncation, type coercion).

use std::ffi::c_void;

use odbc_sys::{Date, NULL_DATA, Time, Timestamp};

use crate::errors::OdbcError;
use crate::types::{CDataType, ColumnValue, SqlReturn, SqlState};

// ---------------------------------------------------------------------------
// Core marshalling function
// ---------------------------------------------------------------------------

/// Write a [`ColumnValue`] into a caller-provided C buffer.
///
/// This is the core data marshalling for `SQLGetData`. Handles NULL values,
/// type conversion, truncation detection, and length/indicator reporting.
///
/// # Arguments
/// - `value`: The column value to write
/// - `target_type`: The ODBC C data type the caller wants
/// - `target_ptr`: Pointer to the caller's buffer (may be null for length-only queries)
/// - `buf_len`: Buffer size in bytes
/// - `len_ind_ptr`: Output pointer for actual data length (bytes) or NULL_DATA (-1)
///
/// # Returns
/// - `SqlReturn::SUCCESS` if the value was written completely
/// - `SqlReturn::SUCCESS_WITH_INFO` if the value was truncated (SQLSTATE 01004)
/// - `SqlReturn::ERROR` on invalid conversion
///
/// # Safety
/// `target_ptr` and `len_ind_ptr` must be valid writable pointers (or null where documented).
pub unsafe fn write_column_value(
    value: &ColumnValue,
    target_type: CDataType,
    target_ptr: *mut c_void,
    buf_len: isize,
    len_ind_ptr: *mut isize,
) -> Result<SqlReturn, OdbcError> {
    // NULL handling
    if matches!(value, ColumnValue::Null) {
        if !len_ind_ptr.is_null() {
            unsafe { std::ptr::write_unaligned(len_ind_ptr, NULL_DATA) };
        }
        return Ok(SqlReturn::SUCCESS);
    }

    // Default type: infer the natural C type from the ColumnValue variant
    if target_type == CDataType::Default {
        let inferred = match value {
            ColumnValue::String(_) => CDataType::WChar,
            ColumnValue::I8(_) => CDataType::STinyInt,
            ColumnValue::I16(_) => CDataType::SShort,
            ColumnValue::I32(_) => CDataType::SLong,
            ColumnValue::I64(_) => CDataType::SBigInt,
            ColumnValue::F32(_) => CDataType::Float,
            ColumnValue::F64(_) => CDataType::Double,
            ColumnValue::Bool(_) => CDataType::Bit,
            ColumnValue::Date { .. } => CDataType::TypeDate,
            ColumnValue::Time { .. } => CDataType::TypeTime,
            ColumnValue::Timestamp { .. } => CDataType::TypeTimestamp,
            // No SQL_C_TYPE_TIMESTAMP_TZ in ODBC — map to TypeTimestamp (offset is dropped).
            ColumnValue::TimestampTz { .. } => CDataType::TypeTimestamp,
            ColumnValue::Bytes(_) => CDataType::Binary,
            ColumnValue::Guid(_) => CDataType::Binary,
            // ColumnValue::Null is handled by the early return above and never
            // reaches this match; it falls into the catch-all harmlessly.
            // New complex variants: default to string serialization via WChar.
            // The (_, CDataType::WChar) arm will call column_value_to_string.
            _ => CDataType::WChar,
        };

        // For an explicitly named fixed C type the spec has the driver ignore
        // BufferLength, because naming the type is itself a statement of the
        // buffer's size. SQL_C_DEFAULT inverts that: the driver chooses, and it
        // chooses from the runtime `ColumnValue` variant rather than from the
        // `sql_type` that `SQLDescribeCol` reported and the application sized
        // its buffer from. Nothing cross-checks those two, so a backend
        // yielding a wider variant than it described would otherwise write past
        // the application's buffer — 16 bytes of `Timestamp` into the four an
        // application allocated for a declared `SQL_INTEGER`.
        //
        // A positive `buf_len` is the only evidence of the real buffer size
        // core has here, so honour it. Zero is exempt: it is the idiomatic way
        // to say "not applicable" for a fixed C type, so it carries no size
        // information and cannot be used as a bound. Variable-length targets
        // are not checked because `write_wchar` / `write_char` / `write_binary`
        // already bound themselves by `buf_len`.
        if let Some(needed) = default_target_width(inferred)
            && buf_len > 0
            && buf_len < needed as isize
        {
            return Err(OdbcError::general(
                format!(
                    "SQL_C_DEFAULT for {value:?} selects {inferred:?}, which needs {needed} bytes, \
                     but the application supplied a {buf_len}-byte buffer"
                ),
                SqlState::restricted_data_type_attribute_violation(),
            ));
        }

        return unsafe { write_column_value(value, inferred, target_ptr, buf_len, len_ind_ptr) };
    }

    // Type coercion: if the value doesn't match the requested type, convert
    // through a string representation for string targets.
    //
    // SAFETY: All unsafe helper calls below operate on the same raw pointers
    // passed by the caller, whose validity is guaranteed by the function's
    // safety contract.
    match (value, target_type) {
        // --- String to WChar (UTF-16) ---
        (ColumnValue::String(s), CDataType::WChar) => unsafe {
            write_wchar(s, target_ptr, buf_len, len_ind_ptr)
        },

        // --- String to Char (UTF-8) ---
        (ColumnValue::String(s), CDataType::Char) => unsafe {
            write_char(s, target_ptr, buf_len, len_ind_ptr)
        },

        // --- String to datetime C types ---
        // Required by the ODBC conversion matrix: SQL_CHAR / SQL_VARCHAR
        // convert to every C type. Backends whose data source has no native
        // date type deliver datetimes as character data.
        (ColumnValue::String(s), CDataType::TypeDate) => {
            let d = parse_sql_date(s)?;
            unsafe { write_fixed(target_ptr, len_ind_ptr, d) }
        }
        (ColumnValue::String(s), CDataType::TypeTime) => {
            let (t, fraction) = parse_sql_time(s)?;
            unsafe {
                let _ = write_fixed(target_ptr, len_ind_ptr, t)?;
            }
            if fraction != 0 {
                return Err(OdbcError::FractionalTruncation);
            }
            Ok(SqlReturn::SUCCESS)
        }
        (ColumnValue::String(s), CDataType::TypeTimestamp) => {
            let ts = parse_sql_timestamp(s)?;
            unsafe { write_fixed(target_ptr, len_ind_ptr, ts) }
        }

        // --- Numeric coercion: any numeric source → any numeric C target ---
        // ODBC requires drivers to support conversions between compatible numeric C types.
        // Applications (e.g. LibreOffice Base) routinely request SQL_C_SLONG for columns
        // that happen to hold i16 values, so all cross-type numeric casts must work.
        //
        // The pivot (column_value_as_numeric) maps every ColumnValue to either Int(i64) or
        // Float(f64) without intermediate precision loss. write_numeric_pivot then narrows
        // to the requested C type at the last possible moment.
        //
        // column_value_as_numeric uses an exhaustive match (no wildcard), so adding a new
        // ColumnValue variant causes a compile error there, forcing an explicit decision.
        (
            _,
            CDataType::STinyInt
            | CDataType::SShort
            | CDataType::SLong
            | CDataType::SBigInt
            | CDataType::UTinyInt
            | CDataType::UShort
            | CDataType::ULong
            | CDataType::UBigInt
            | CDataType::Float
            | CDataType::Double
            | CDataType::Bit,
        ) => match column_value_as_numeric(value) {
            Some(pivot) => unsafe {
                write_numeric_pivot(pivot, target_type, target_ptr, len_ind_ptr)
            },
            None => Err(match value {
                // Text that should have been numeric but was not parseable.
                ColumnValue::String(_) | ColumnValue::Decimal(_) => OdbcError::general(
                    format!("Invalid character value for cast: {value:?}"),
                    SqlState::invalid_character_value_for_cast(),
                ),
                // The column value's type has no defined conversion to the
                // requested C type (e.g. a Bytes/Guid/structured value asked
                // to become a numeric target). Spec 07006: "The data value of
                // a column in the result set could not be converted to the
                // data type specified by the TargetType argument."
                _ => OdbcError::general(
                    format!("Unsupported conversion from {value:?} to {target_type:?}"),
                    SqlState::restricted_data_type_attribute_violation(),
                ),
            }),
        },

        // --- Date ---
        (ColumnValue::Date { year, month, day }, CDataType::TypeDate) => {
            let ds = Date {
                year: *year,
                month: *month,
                day: *day,
            };
            unsafe { write_fixed(target_ptr, len_ind_ptr, ds) }
        }

        // SQL_TYPE_DATE -> SQL_C_TYPE_TIMESTAMP. Legal per the spec's SQL-to-C
        // table: "The driver sets the time fields of the timestamp structure to
        // zero." No SQLSTATE — nothing is lost.
        (ColumnValue::Date { year, month, day }, CDataType::TypeTimestamp) => {
            let ts = Timestamp {
                year: *year,
                month: *month,
                day: *day,
                hour: 0,
                minute: 0,
                second: 0,
                fraction: 0,
            };
            unsafe { write_fixed(target_ptr, len_ind_ptr, ts) }
        }

        // --- Time ---
        // SQL_TIME_STRUCT has no fraction field: write the whole-second parts,
        // then report 01S07 if a non-zero fraction had to be dropped to fit.
        (
            ColumnValue::Time {
                hour,
                minute,
                second,
                fraction,
            },
            CDataType::TypeTime,
        ) => {
            let ts = Time {
                hour: *hour,
                minute: *minute,
                second: *second,
            };
            unsafe {
                let _ = write_fixed(target_ptr, len_ind_ptr, ts)?;
            }
            if *fraction != 0 {
                return Err(OdbcError::FractionalTruncation);
            }
            Ok(SqlReturn::SUCCESS)
        }

        // SQL_TYPE_TIME -> SQL_C_TYPE_TIMESTAMP. Legal per the spec's SQL-to-C
        // table: "The date fields of the timestamp structure are set to the
        // current date, and the fractional seconds field of the timestamp
        // structure is set to zero."
        //
        // The spec lists no SQLSTATE for this row, so a dropped fraction is not
        // reported here — unlike the SQL_C_TYPE_TIME row above, where the target
        // has nowhere to put one. Here the target *has* a fraction field and the
        // spec still says to zero it, which makes it a defined part of the
        // conversion rather than a truncation.
        (
            ColumnValue::Time {
                hour,
                minute,
                second,
                ..
            },
            CDataType::TypeTimestamp,
        ) => {
            let (year, month, day) = current_utc_date();
            let ts = Timestamp {
                year,
                month,
                day,
                hour: *hour,
                minute: *minute,
                second: *second,
                fraction: 0,
            };
            unsafe { write_fixed(target_ptr, len_ind_ptr, ts) }
        }

        // --- Timestamp ---
        (
            ColumnValue::Timestamp {
                year,
                month,
                day,
                hour,
                minute,
                second,
                fraction,
            },
            CDataType::TypeTimestamp,
        ) => {
            let ts = Timestamp {
                year: *year,
                month: *month,
                day: *day,
                hour: *hour,
                minute: *minute,
                second: *second,
                fraction: *fraction,
            };
            unsafe { write_fixed(target_ptr, len_ind_ptr, ts) }
        }

        // SQL_TYPE_TIMESTAMP -> SQL_C_TYPE_DATE. Legal per the spec's SQL-to-C
        // table, which splits on the time portion: zero is `n/a`, non-zero is
        // `01S07` with "The time portion of the timestamp is truncated."
        (
            ColumnValue::Timestamp {
                year,
                month,
                day,
                hour,
                minute,
                second,
                fraction,
            },
            CDataType::TypeDate,
        ) => {
            let ds = Date {
                year: *year,
                month: *month,
                day: *day,
            };
            unsafe {
                let _ = write_fixed(target_ptr, len_ind_ptr, ds)?;
            }
            if *hour != 0 || *minute != 0 || *second != 0 || *fraction != 0 {
                return Err(OdbcError::FractionalTruncation);
            }
            Ok(SqlReturn::SUCCESS)
        }

        // SQL_TYPE_TIMESTAMP -> SQL_C_TYPE_TIME. Legal per the spec's SQL-to-C
        // table: "The date portion of the timestamp is ignored", and the split
        // is on the *fractional seconds* alone — a discarded date is not a
        // truncation, so only a non-zero fraction reports `01S07`.
        (
            ColumnValue::Timestamp {
                hour,
                minute,
                second,
                fraction,
                ..
            },
            CDataType::TypeTime,
        ) => {
            let ts = Time {
                hour: *hour,
                minute: *minute,
                second: *second,
            };
            unsafe {
                let _ = write_fixed(target_ptr, len_ind_ptr, ts)?;
            }
            if *fraction != 0 {
                return Err(OdbcError::FractionalTruncation);
            }
            Ok(SqlReturn::SUCCESS)
        }

        // --- TimestampTz → TypeTimestamp ---
        // SQL_TIMESTAMP_STRUCT has no timezone field, so the offset is discarded.
        // A backend that normalizes to UTC before reaching this point returns
        // ColumnValue::Timestamp instead, so this arm is a safety net for any
        // backend that produces TimestampTz directly.
        (
            ColumnValue::TimestampTz {
                year,
                month,
                day,
                hour,
                minute,
                second,
                fraction,
                ..
            },
            CDataType::TypeTimestamp,
        ) => {
            let ts = Timestamp {
                year: *year,
                month: *month,
                day: *day,
                hour: *hour,
                minute: *minute,
                second: *second,
                fraction: *fraction,
            };
            unsafe { write_fixed(target_ptr, len_ind_ptr, ts) }
        }

        // TimestampTz narrowed to SQL_C_TYPE_DATE / SQL_C_TYPE_TIME, by analogy
        // with the `Timestamp` arms above rather than from the SQL-to-C table,
        // which has no row for a zoned timestamp. Supporting only
        // SQL_C_TYPE_TIMESTAMP for `TimestampTz` while `Timestamp` supports all
        // three would leave the same hole this arm family was added to close: an
        // application asking a zoned column for a plain date would get 07006
        // where an unzoned one succeeds. The offset is discarded, as it already
        // is for SQL_C_TYPE_TIMESTAMP.
        (
            ColumnValue::TimestampTz {
                year,
                month,
                day,
                hour,
                minute,
                second,
                fraction,
                ..
            },
            CDataType::TypeDate,
        ) => {
            let ds = Date {
                year: *year,
                month: *month,
                day: *day,
            };
            unsafe {
                let _ = write_fixed(target_ptr, len_ind_ptr, ds)?;
            }
            if *hour != 0 || *minute != 0 || *second != 0 || *fraction != 0 {
                return Err(OdbcError::FractionalTruncation);
            }
            Ok(SqlReturn::SUCCESS)
        }
        (
            ColumnValue::TimestampTz {
                hour,
                minute,
                second,
                fraction,
                ..
            },
            CDataType::TypeTime,
        ) => {
            let ts = Time {
                hour: *hour,
                minute: *minute,
                second: *second,
            };
            unsafe {
                let _ = write_fixed(target_ptr, len_ind_ptr, ts)?;
            }
            if *fraction != 0 {
                return Err(OdbcError::FractionalTruncation);
            }
            Ok(SqlReturn::SUCCESS)
        }

        // --- Coercion: any value → Binary ---
        // Mirrors the WChar/Char catch-alls: a single arm backed by column_value_to_binary
        // so that adding a new ColumnValue variant never requires a new Binary arm here.
        (_, CDataType::Binary) => unsafe {
            let bytes = column_value_to_binary(value);
            write_binary(&bytes, target_ptr, buf_len, len_ind_ptr)
        },

        // --- Coercion: any value → WChar ---
        (_, CDataType::WChar) => {
            let s = column_value_to_string(value);
            unsafe { write_wchar(&s, target_ptr, buf_len, len_ind_ptr) }
        }

        // --- Coercion: numeric/bool to Char ---
        (_, CDataType::Char) => {
            let s = column_value_to_string(value);
            unsafe { write_char(&s, target_ptr, buf_len, len_ind_ptr) }
        }

        // Unsupported conversion. Spec 07006: "The data value of a column in
        // the result set could not be converted to the data type specified
        // by the TargetType argument."
        _ => Err(OdbcError::general(
            format!(
                "Unsupported conversion from {:?} to {:?}",
                value, target_type
            ),
            SqlState::restricted_data_type_attribute_violation(),
        )),
    }
}

// ---------------------------------------------------------------------------
// Helper: today's date, for SQL_TYPE_TIME -> SQL_C_TYPE_TIMESTAMP
// ---------------------------------------------------------------------------

/// Convert a count of days since 1970-01-01 to a proleptic Gregorian date.
///
/// Howard Hinnant's `civil_from_days`, which is exact for the whole range this
/// can produce and needs no calendar dependency. Kept separate from
/// [`current_utc_date`] so the arithmetic can be tested against known dates
/// without a clock in the way.
fn civil_from_days(days: i64) -> (i64, u16, u16) {
    // Shift the epoch to 0000-03-01, which puts the leap day at the end of the
    // 400-year era and makes the month arithmetic below branch-free.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146_096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11], March = 0
    let day = (doy - (153 * mp + 2) / 5 + 1) as u16; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u16; // [1, 12]
    let year = yoe as i64 + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Today's date in UTC.
///
/// The `SQL_TYPE_TIME` -> `SQL_C_TYPE_TIMESTAMP` conversion requires it: "The
/// date fields of the timestamp structure are set to the current date, and the
/// fractional seconds field of the timestamp structure is set to zero."
///
/// UTC, not local time. The spec says "the current date" without saying whose,
/// and the standard library offers no timezone database, so local time is not
/// implementable here without a dependency. UTC is at least well defined and
/// the same for every driver built on this crate.
///
/// This is the only wall-clock read in the crate, and it makes
/// [`write_column_value`] impure for exactly one `(value, target_type)` pair.
/// `clippy.toml` disallows `SystemTime::now` so that a second one has to be
/// argued for rather than appearing by accident.
///
/// A clock set before 1970 is reported truthfully rather than clamped:
/// `duration_since` fails in that case, but the error carries the distance
/// backwards, so the date is still recoverable. There is no SQLSTATE for "no
/// clock" and the conversion owes a date either way, so the only alternative
/// would be to substitute one — and a wrong date presented as correct is worse
/// than an unusual one.
// The single sanctioned wall-clock read in the crate. `clippy.toml` disallows
// `SystemTime::now` so that a second one has to be argued for rather than
// appearing by accident; this one is forced by the spec sentence quoted above,
// which cannot be satisfied from the column value alone.
#[allow(
    clippy::disallowed_methods,
    reason = "SQL_TYPE_TIME -> SQL_C_TYPE_TIMESTAMP is specified as using the current date"
)]
fn current_utc_date() -> (i16, u16, u16) {
    // `try_from` rather than `as`: a clock far enough out to exceed i64 seconds
    // is nonsense either way, but wrapping it into a negative would turn a date
    // in the far future into one in the distant past.
    let secs = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(since_epoch) => i64::try_from(since_epoch.as_secs()).unwrap_or(i64::MAX),
        // Before 1970. The error carries how far back, so negate it rather than
        // discarding it and claiming 1970-01-01.
        Err(before_epoch) => -i64::try_from(before_epoch.duration().as_secs()).unwrap_or(i64::MAX),
    };
    let (year, month, day) = civil_from_days(secs.div_euclid(86_400));
    // `SQL_TIMESTAMP_STRUCT::year` is an i16, so a year outside it cannot be
    // represented at all; saturate rather than wrap into a plausible-looking
    // one. Unreachable for any clock that is merely wrong rather than absurd.
    (i16::try_from(year).unwrap_or(i16::MAX), month, day)
}

// ---------------------------------------------------------------------------
// Helper: write a fixed-size value to a raw pointer
// ---------------------------------------------------------------------------

/// How many bytes `write_fixed` will write for a C type that
/// [`write_column_value`]'s `SQL_C_DEFAULT` inference can select, or `None` for
/// the variable-length targets, which bound themselves by `buf_len`.
///
/// Deliberately covers only the types that inference can produce. A wider match
/// would invite the impression that this is a general size table for
/// `CDataType`, which it is not — it exists solely to bound the one path where
/// the driver, not the application, picks the C type.
fn default_target_width(c_type: CDataType) -> Option<usize> {
    Some(match c_type {
        CDataType::Bit | CDataType::STinyInt => 1,
        CDataType::SShort => 2,
        CDataType::SLong | CDataType::Float => 4,
        CDataType::SBigInt | CDataType::Double => 8,
        CDataType::TypeDate => size_of::<Date>(),
        CDataType::TypeTime => size_of::<Time>(),
        CDataType::TypeTimestamp => size_of::<Timestamp>(),
        // WChar and Binary are the other two inference results; both are
        // variable-length and already respect buf_len.
        _ => return None,
    })
}

unsafe fn write_fixed<T: Copy>(
    target_ptr: *mut c_void,
    len_ind_ptr: *mut isize,
    value: T,
) -> Result<SqlReturn, OdbcError> {
    if !target_ptr.is_null() {
        unsafe { std::ptr::write_unaligned(target_ptr.cast::<T>(), value) };
    }
    if !len_ind_ptr.is_null() {
        unsafe { std::ptr::write_unaligned(len_ind_ptr, std::mem::size_of::<T>() as isize) };
    }
    Ok(SqlReturn::SUCCESS)
}

// ---------------------------------------------------------------------------
// Helper: parse ODBC datetime literals from character data
// ---------------------------------------------------------------------------
//
// The ODBC conversion matrix requires SQL_CHAR / SQL_VARCHAR to convert to the
// datetime C types. Accepted forms are the ODBC literal formats, which is what
// backends are expected to emit:
//
//   date       yyyy-mm-dd
//   time       hh:mm[:ss[.f...]]
//   timestamp  yyyy-mm-dd[ T]hh:mm[:ss[.f...]]
//
// Unparseable text is 22018; text that parses but carries an out-of-range field
// is 22007. Both codes are scoped by the spec to a character column source
// (see the SQLGetData diagnostics table), which is exactly the case handled
// in this module -- stackable-odbc-core has no numeric datetime encodings left to
// decode (see the Backend-side decoding note on write_column_value above).

fn cast_error(s: &str) -> OdbcError {
    OdbcError::general(
        format!("Invalid character value for cast: {s:?}"),
        SqlState::invalid_character_value_for_cast(),
    )
}

fn invalid_datetime_format(s: &str) -> OdbcError {
    OdbcError::general(
        format!("Invalid datetime format: {s:?}"),
        SqlState::invalid_datetime_format(),
    )
}

/// Map a numeric-field [`std::num::ParseIntError`] to the right SQLSTATE.
///
/// A string that fails to parse purely because it does not fit the target
/// integer type (`PosOverflow` / `NegOverflow`, e.g. year `"99999"` or hour
/// `"700000"`) is syntactically valid but out of range: 22007. Anything else
/// (empty, non-digit characters, ...) is a syntax problem: 22018.
fn field_parse_error(s: &str, e: std::num::ParseIntError) -> OdbcError {
    use std::num::IntErrorKind;
    match e.kind() {
        IntErrorKind::PosOverflow | IntErrorKind::NegOverflow => invalid_datetime_format(s),
        _ => cast_error(s),
    }
}

/// Parse `yyyy-mm-dd` into its three numeric fields.
fn parse_date_fields(s: &str) -> Result<(i16, u16, u16), OdbcError> {
    let mut parts = s.split('-');
    let (y, m, d) = match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(y), Some(m), Some(d), None) => (y, m, d),
        _ => return Err(cast_error(s)),
    };
    let year: i16 = y.parse().map_err(|e| field_parse_error(s, e))?;
    let month: u16 = m.parse().map_err(|e| field_parse_error(s, e))?;
    let day: u16 = d.parse().map_err(|e| field_parse_error(s, e))?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return Err(invalid_datetime_format(s));
    }
    Ok((year, month, day))
}

/// Parse `hh:mm[:ss[.f...]]` into hour, minute, second and nanoseconds.
fn parse_time_fields(s: &str) -> Result<(u16, u16, u16, u32), OdbcError> {
    let mut parts = s.split(':');
    let (h, m) = match (parts.next(), parts.next()) {
        (Some(h), Some(m)) => (h, m),
        _ => return Err(cast_error(s)),
    };
    let sec_part = parts.next().unwrap_or("0");
    if parts.next().is_some() {
        return Err(cast_error(s));
    }

    let hour: u16 = h.parse().map_err(|e| field_parse_error(s, e))?;
    let minute: u16 = m.parse().map_err(|e| field_parse_error(s, e))?;

    // Distinguish "no dot at all" (None -> fraction 0) from "a dot with
    // nothing after it" (Some((_, "")) -> malformed, e.g. "10:30:15.").
    let (sec_text, frac_text) = match sec_part.split_once('.') {
        Some((sec, frac)) => (sec, Some(frac)),
        None => (sec_part, None),
    };
    let second: u16 = sec_text.parse().map_err(|e| field_parse_error(s, e))?;

    // Both SQL_TIMESTAMP_STRUCT.fraction and ColumnValue::Time's fraction are
    // nanoseconds. Pad or truncate to 9 digits.
    let fraction: u32 = match frac_text {
        None => 0,
        Some(frac_text) => {
            if frac_text.is_empty() || !frac_text.bytes().all(|b| b.is_ascii_digit()) {
                return Err(cast_error(s));
            }
            let mut digits = frac_text.to_string();
            digits.truncate(9);
            while digits.len() < 9 {
                digits.push('0');
            }
            digits.parse().map_err(|_| cast_error(s))?
        }
    };

    // 60 is permitted for a leap second.
    if hour > 23 || minute > 59 || second > 60 {
        return Err(invalid_datetime_format(s));
    }
    Ok((hour, minute, second, fraction))
}

fn parse_sql_date(s: &str) -> Result<Date, OdbcError> {
    let (year, month, day) = parse_date_fields(s.trim())?;
    Ok(Date { year, month, day })
}

/// Parse ODBC time literal text into a [`Time`] struct plus the fractional
/// seconds (nanoseconds) that `SQL_TIME_STRUCT` cannot carry. Callers writing
/// to `SQL_C_TYPE_TIME` must check the returned fraction themselves and report
/// 01S07 if it is non-zero -- this function only parses, it does not decide
/// whether the drop is acceptable for the caller's target type.
fn parse_sql_time(s: &str) -> Result<(Time, u32), OdbcError> {
    let (hour, minute, second, fraction) = parse_time_fields(s.trim())?;
    Ok((
        Time {
            hour,
            minute,
            second,
        },
        fraction,
    ))
}

fn parse_sql_timestamp(s: &str) -> Result<Timestamp, OdbcError> {
    let t = s.trim();
    // Accept either the ODBC space separator or the ISO 8601 'T'.
    let (date_part, time_part) = match t.split_once([' ', 'T']) {
        Some((d, rest)) => (d, rest.trim()),
        // A bare date is a valid timestamp at midnight.
        None => (t, ""),
    };
    let (year, month, day) = parse_date_fields(date_part)?;
    let (hour, minute, second, fraction) = if time_part.is_empty() {
        (0, 0, 0, 0)
    } else {
        parse_time_fields(time_part)?
    };
    Ok(Timestamp {
        year,
        month,
        day,
        hour,
        minute,
        second,
        fraction,
    })
}

// ---------------------------------------------------------------------------
// Helper: write UTF-16 string
// ---------------------------------------------------------------------------

unsafe fn write_wchar(
    s: &str,
    target_ptr: *mut c_void,
    buf_len: isize,
    len_ind_ptr: *mut isize,
) -> Result<SqlReturn, OdbcError> {
    // Pre-size to UTF-8 byte length, which is always >= UTF-16 code unit count.
    let mut wide = Vec::with_capacity(s.len());
    wide.extend(s.encode_utf16());
    let total_bytes = (wide.len() * 2) as isize;

    // Always report the total byte length needed.
    if !len_ind_ptr.is_null() {
        unsafe { std::ptr::write_unaligned(len_ind_ptr, total_bytes) };
    }

    if target_ptr.is_null() || buf_len <= 0 {
        return Ok(SqlReturn::SUCCESS);
    }

    // The null terminator is one UTF-16 code unit, so a buffer of fewer than
    // two bytes cannot hold it. Writing one anyway would overrun the caller's
    // buffer. Spec: "If the data buffer supplied is too small to hold the
    // null-termination character, SQLGetData returns SQL_SUCCESS_WITH_INFO
    // and SQLSTATE 01004."
    if buf_len < 2 {
        return Ok(SqlReturn::SUCCESS_WITH_INFO);
    }

    let out_ptr = target_ptr.cast::<u16>();
    // buf_len is in bytes; capacity in u16 code units (reserve one for null terminator)
    let capacity_units = ((buf_len as usize) / 2).saturating_sub(1);
    let copy_count = wide.len().min(capacity_units);

    unsafe {
        let out_bytes = out_ptr.cast::<u8>();
        std::ptr::copy_nonoverlapping(wide.as_ptr().cast::<u8>(), out_bytes, copy_count * 2);
        // null terminator
        std::ptr::write_unaligned(out_bytes.add(copy_count * 2).cast::<u16>(), 0u16);
    }

    if copy_count < wide.len() {
        Ok(SqlReturn::SUCCESS_WITH_INFO)
    } else {
        Ok(SqlReturn::SUCCESS)
    }
}

// ---------------------------------------------------------------------------
// Helper: write UTF-8 string
// ---------------------------------------------------------------------------

unsafe fn write_char(
    s: &str,
    target_ptr: *mut c_void,
    buf_len: isize,
    len_ind_ptr: *mut isize,
) -> Result<SqlReturn, OdbcError> {
    let bytes = s.as_bytes();
    let total_bytes = bytes.len() as isize;

    if !len_ind_ptr.is_null() {
        unsafe { std::ptr::write_unaligned(len_ind_ptr, total_bytes) };
    }

    if target_ptr.is_null() || buf_len <= 0 {
        return Ok(SqlReturn::SUCCESS);
    }

    let out_ptr = target_ptr.cast::<u8>();
    let capacity = (buf_len as usize).saturating_sub(1); // reserve one for null terminator
    let copy_count = bytes.len().min(capacity);

    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), out_ptr, copy_count);
        *out_ptr.add(copy_count) = 0u8; // null terminator
    }

    if copy_count < bytes.len() {
        Ok(SqlReturn::SUCCESS_WITH_INFO)
    } else {
        Ok(SqlReturn::SUCCESS)
    }
}

// ---------------------------------------------------------------------------
// Helper: write raw binary bytes
// ---------------------------------------------------------------------------

unsafe fn write_binary(
    data: &[u8],
    target_ptr: *mut c_void,
    buf_len: isize,
    len_ind_ptr: *mut isize,
) -> Result<SqlReturn, OdbcError> {
    let total_bytes = data.len() as isize;

    if !len_ind_ptr.is_null() {
        unsafe { std::ptr::write_unaligned(len_ind_ptr, total_bytes) };
    }

    if target_ptr.is_null() || buf_len <= 0 {
        return Ok(SqlReturn::SUCCESS);
    }

    let out_ptr = target_ptr.cast::<u8>();
    let copy_count = data.len().min(buf_len as usize);

    unsafe {
        std::ptr::copy_nonoverlapping(data.as_ptr(), out_ptr, copy_count);
    }

    if copy_count < data.len() {
        Ok(SqlReturn::SUCCESS_WITH_INFO)
    } else {
        Ok(SqlReturn::SUCCESS)
    }
}

// ---------------------------------------------------------------------------
// Numeric pivot: intermediate representation for numeric cross-type coercion
// ---------------------------------------------------------------------------

/// Intermediate representation for numeric cross-type coercion.
///
/// Keeping integers as `i64` and floats as `f64` avoids any intermediate precision
/// loss: the final narrowing cast (`i64 as i8`, `i64 as f64`, etc.) happens once,
/// at write time, only when the requested C target type requires it.
enum NumericPivot {
    Int(i64),
    Float(f64),
}

/// Parse numeric text into a [`NumericPivot`].
///
/// Integer is attempted first so that values beyond `f64`'s 53-bit exact range
/// (an exact numeric such as `DECIMAL(19,0)`) reach `SQL_C_SBIGINT` intact.
/// Falls back to `f64` for anything with a fractional part or exponent.
fn parse_numeric_text(s: &str) -> Option<NumericPivot> {
    let t = s.trim();
    if let Ok(i) = t.parse::<i64>() {
        return Some(NumericPivot::Int(i));
    }
    t.parse::<f64>().ok().map(NumericPivot::Float)
}

/// Map a [`ColumnValue`] to a [`NumericPivot`], or `None` if the variant is not numeric.
///
/// This match is intentionally exhaustive (no wildcard) so that adding a new
/// `ColumnValue` variant causes a compile error here, forcing an explicit decision
/// about whether the new type is numeric.
fn column_value_as_numeric(value: &ColumnValue) -> Option<NumericPivot> {
    match value {
        ColumnValue::I8(v) => Some(NumericPivot::Int(i64::from(*v))),
        ColumnValue::I16(v) => Some(NumericPivot::Int(i64::from(*v))),
        ColumnValue::I32(v) => Some(NumericPivot::Int(i64::from(*v))),
        ColumnValue::I64(v) => Some(NumericPivot::Int(*v)),
        ColumnValue::F32(v) => Some(NumericPivot::Float(f64::from(*v))),
        ColumnValue::F64(v) => Some(NumericPivot::Float(*v)),
        ColumnValue::Bool(v) => Some(NumericPivot::Int(*v as i64)),
        ColumnValue::Decimal(s) | ColumnValue::String(s) => parse_numeric_text(s),
        // Non-numeric variants: explicitly listed so the compiler flags any new variant
        ColumnValue::Null
        | ColumnValue::Date { .. }
        | ColumnValue::Time { .. }
        | ColumnValue::Timestamp { .. }
        | ColumnValue::Bytes(_)
        | ColumnValue::Guid(_)
        | ColumnValue::TimestampTz { .. }
        | ColumnValue::Json(_)
        | ColumnValue::Array(_)
        | ColumnValue::Map(_)
        | ColumnValue::Row(_)
        | ColumnValue::IntervalYearMonth { .. }
        | ColumnValue::IntervalDayTime { .. } => None,
    }
}

/// Write a numeric pivot value into a C buffer for the given target type.
///
/// Handles all signed and unsigned integer C types, `SQL_C_FLOAT`, `SQL_C_DOUBLE`,
/// and `SQL_C_BIT`. Returns `SQL_ERROR` with SQLSTATE `22003` (numeric value out of
/// range) when the value does not fit the target type, and `SQL_SUCCESS_WITH_INFO`
/// with SQLSTATE `01S07` (fractional truncation) when an `i64` or an `f64` is
/// narrowed to `f32` with precision loss. Any `CDataType` not covered by the
/// numeric arms returns `SQL_ERROR` with SQLSTATE `HY003` (invalid application
/// buffer type).
unsafe fn write_numeric_pivot(
    pivot: NumericPivot,
    target_type: CDataType,
    target_ptr: *mut c_void,
    len_ind_ptr: *mut isize,
) -> Result<SqlReturn, OdbcError> {
    match (pivot, target_type) {
        // --- Int pivot → signed integer targets ---
        (NumericPivot::Int(v), CDataType::STinyInt) => {
            let n = i8::try_from(v).map_err(|_| {
                OdbcError::general(
                    format!("Numeric value out of range: {v}"),
                    SqlState::numeric_value_out_of_range(),
                )
            })?;
            unsafe { write_fixed(target_ptr, len_ind_ptr, n) }
        }
        (NumericPivot::Int(v), CDataType::SShort) => {
            let n = i16::try_from(v).map_err(|_| {
                OdbcError::general(
                    format!("Numeric value out of range: {v}"),
                    SqlState::numeric_value_out_of_range(),
                )
            })?;
            unsafe { write_fixed(target_ptr, len_ind_ptr, n) }
        }
        (NumericPivot::Int(v), CDataType::SLong) => {
            let n = i32::try_from(v).map_err(|_| {
                OdbcError::general(
                    format!("Numeric value out of range: {v}"),
                    SqlState::numeric_value_out_of_range(),
                )
            })?;
            unsafe { write_fixed(target_ptr, len_ind_ptr, n) }
        }
        (NumericPivot::Int(v), CDataType::SBigInt) => unsafe {
            write_fixed(target_ptr, len_ind_ptr, v)
        },
        // --- Int pivot → unsigned integer targets ---
        (NumericPivot::Int(v), CDataType::UTinyInt) => {
            let n = u8::try_from(v).map_err(|_| {
                OdbcError::general(
                    format!("Numeric value out of range: {v}"),
                    SqlState::numeric_value_out_of_range(),
                )
            })?;
            unsafe { write_fixed(target_ptr, len_ind_ptr, n) }
        }
        (NumericPivot::Int(v), CDataType::UShort) => {
            let n = u16::try_from(v).map_err(|_| {
                OdbcError::general(
                    format!("Numeric value out of range: {v}"),
                    SqlState::numeric_value_out_of_range(),
                )
            })?;
            unsafe { write_fixed(target_ptr, len_ind_ptr, n) }
        }
        (NumericPivot::Int(v), CDataType::ULong) => {
            let n = u32::try_from(v).map_err(|_| {
                OdbcError::general(
                    format!("Numeric value out of range: {v}"),
                    SqlState::numeric_value_out_of_range(),
                )
            })?;
            unsafe { write_fixed(target_ptr, len_ind_ptr, n) }
        }
        (NumericPivot::Int(v), CDataType::UBigInt) => {
            let n = u64::try_from(v).map_err(|_| {
                OdbcError::general(
                    format!("Numeric value out of range: {v}"),
                    SqlState::numeric_value_out_of_range(),
                )
            })?;
            unsafe { write_fixed(target_ptr, len_ind_ptr, n) }
        }
        // --- Int pivot → float targets ---
        // i64 → f32: values with |v| > 2^24 lose precision; return 01S07 after writing.
        (NumericPivot::Int(v), CDataType::Float) => {
            let f = v as f32;
            unsafe {
                let _ = write_fixed(target_ptr, len_ind_ptr, f)?;
            };
            if f as i64 != v {
                return Err(OdbcError::FractionalTruncation);
            }
            Ok(SqlReturn::SUCCESS)
        }
        (NumericPivot::Int(v), CDataType::Double) => unsafe {
            write_fixed(target_ptr, len_ind_ptr, v as f64)
        },
        (NumericPivot::Int(v), CDataType::Bit) => unsafe {
            write_fixed(target_ptr, len_ind_ptr, u8::from(v != 0))
        },
        // --- Float pivot → signed integer targets ---
        (NumericPivot::Float(v), CDataType::STinyInt) => {
            if !v.is_finite() || v < i8::MIN as f64 || v > i8::MAX as f64 {
                return Err(OdbcError::general(
                    format!("Numeric value out of range: {v}"),
                    SqlState::numeric_value_out_of_range(),
                ));
            }
            unsafe { write_fixed(target_ptr, len_ind_ptr, v as i8) }
        }
        (NumericPivot::Float(v), CDataType::SShort) => {
            if !v.is_finite() || v < i16::MIN as f64 || v > i16::MAX as f64 {
                return Err(OdbcError::general(
                    format!("Numeric value out of range: {v}"),
                    SqlState::numeric_value_out_of_range(),
                ));
            }
            unsafe { write_fixed(target_ptr, len_ind_ptr, v as i16) }
        }
        (NumericPivot::Float(v), CDataType::SLong) => {
            if !v.is_finite() || v < i32::MIN as f64 || v > i32::MAX as f64 {
                return Err(OdbcError::general(
                    format!("Numeric value out of range: {v}"),
                    SqlState::numeric_value_out_of_range(),
                ));
            }
            unsafe { write_fixed(target_ptr, len_ind_ptr, v as i32) }
        }
        (NumericPivot::Float(v), CDataType::SBigInt) => {
            // i64::MAX is not representable in f64: `i64::MAX as f64` rounds up
            // to 2^63. Compare against 2^63 exclusively so that value is
            // rejected rather than saturated.
            const I64_MAX_EXCLUSIVE: f64 = 9_223_372_036_854_775_808.0; // 2^63
            const I64_MIN_INCLUSIVE: f64 = -9_223_372_036_854_775_808.0; // -2^63
            if !v.is_finite() || !(I64_MIN_INCLUSIVE..I64_MAX_EXCLUSIVE).contains(&v) {
                return Err(OdbcError::general(
                    format!("Numeric value out of range: {v}"),
                    SqlState::numeric_value_out_of_range(),
                ));
            }
            unsafe { write_fixed(target_ptr, len_ind_ptr, v as i64) }
        }
        // --- Float pivot → unsigned integer targets ---
        (NumericPivot::Float(v), CDataType::UTinyInt) => {
            if !v.is_finite() || v < 0.0 || v > u8::MAX as f64 {
                return Err(OdbcError::general(
                    format!("Numeric value out of range: {v}"),
                    SqlState::numeric_value_out_of_range(),
                ));
            }
            unsafe { write_fixed(target_ptr, len_ind_ptr, v as u8) }
        }
        (NumericPivot::Float(v), CDataType::UShort) => {
            if !v.is_finite() || v < 0.0 || v > u16::MAX as f64 {
                return Err(OdbcError::general(
                    format!("Numeric value out of range: {v}"),
                    SqlState::numeric_value_out_of_range(),
                ));
            }
            unsafe { write_fixed(target_ptr, len_ind_ptr, v as u16) }
        }
        (NumericPivot::Float(v), CDataType::ULong) => {
            if !v.is_finite() || v < 0.0 || v > u32::MAX as f64 {
                return Err(OdbcError::general(
                    format!("Numeric value out of range: {v}"),
                    SqlState::numeric_value_out_of_range(),
                ));
            }
            unsafe { write_fixed(target_ptr, len_ind_ptr, v as u32) }
        }
        (NumericPivot::Float(v), CDataType::UBigInt) => {
            // u64::MAX is not representable in f64: `u64::MAX as f64` rounds up
            // to 2^64.
            const U64_MAX_EXCLUSIVE: f64 = 18_446_744_073_709_551_616.0; // 2^64
            if !v.is_finite() || !(0.0..U64_MAX_EXCLUSIVE).contains(&v) {
                return Err(OdbcError::general(
                    format!("Numeric value out of range: {v}"),
                    SqlState::numeric_value_out_of_range(),
                ));
            }
            unsafe { write_fixed(target_ptr, len_ind_ptr, v as u64) }
        }
        // --- Float pivot → float targets ---
        // f64 → f32: narrowing loses precision for most values, and overflows
        // to ±inf beyond f32::MAX. Write the value, then report 01S07 when the
        // round trip is not exact, matching the Int → Float arm above.
        (NumericPivot::Float(v), CDataType::Float) => {
            let f = v as f32;
            unsafe {
                let _ = write_fixed(target_ptr, len_ind_ptr, f)?;
            };
            if f64::from(f) != v {
                return Err(OdbcError::FractionalTruncation);
            }
            Ok(SqlReturn::SUCCESS)
        }
        (NumericPivot::Float(v), CDataType::Double) => unsafe {
            write_fixed(target_ptr, len_ind_ptr, v)
        },
        // --- Float pivot → Bit ---
        // NaN is not a valid bit value; return 22003 rather than silently mapping to 1.
        (NumericPivot::Float(v), CDataType::Bit) => {
            if v.is_nan() {
                return Err(OdbcError::general(
                    "Numeric value out of range: NaN cannot be converted to SQL_C_BIT",
                    SqlState::numeric_value_out_of_range(),
                ));
            }
            unsafe { write_fixed(target_ptr, len_ind_ptr, u8::from(v != 0.0)) }
        }
        (_, _) => Err(OdbcError::general(
            format!("Unsupported numeric target type: {target_type:?}"),
            SqlState::invalid_application_buffer_type(),
        )),
    }
}

// ---------------------------------------------------------------------------
// Helper: convert ColumnValue to string for coercion
// ---------------------------------------------------------------------------

fn column_value_to_string(value: &ColumnValue) -> String {
    match value {
        ColumnValue::Null => String::new(),
        ColumnValue::String(s) => s.clone(),
        ColumnValue::I8(v) => v.to_string(),
        ColumnValue::I16(v) => v.to_string(),
        ColumnValue::I32(v) => v.to_string(),
        ColumnValue::I64(v) => v.to_string(),
        ColumnValue::F32(v) => v.to_string(),
        ColumnValue::F64(v) => v.to_string(),
        ColumnValue::Bool(v) => {
            if *v {
                "1".to_string()
            } else {
                "0".to_string()
            }
        }
        ColumnValue::Date { year, month, day } => {
            format!("{year:04}-{month:02}-{day:02}")
        }
        ColumnValue::Time {
            hour,
            minute,
            second,
            fraction,
        } => {
            if *fraction == 0 {
                format!("{hour:02}:{minute:02}:{second:02}")
            } else {
                // Trailing zeros are stripped here: a `time(3)` value's
                // fraction is stored padded out to nanoseconds, and rendering
                // all 9 digits would print fabricated trailing zeros for
                // every genuine digit received, misrepresenting the source
                // column's actual precision. Trimming recovers exactly the
                // digits that were received, since the padding this function
                // strips back off is the same zero-padding the parser added
                // on the way in. `Timestamp` below does the same, for the
                // same reason.
                format!(
                    "{hour:02}:{minute:02}:{second:02}.{}",
                    format!("{fraction:09}").trim_end_matches('0')
                )
            }
        }
        ColumnValue::Timestamp {
            year,
            month,
            day,
            hour,
            minute,
            second,
            fraction,
        } => {
            if *fraction == 0 {
                format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}")
            } else {
                // See the `Time` arm above: trailing zeros are stripped so a
                // `timestamp(3)` value renders 3 fractional digits, not the
                // 9-digit nanosecond storage padded out with 6 fabricated
                // zeros. Emitting all 9 would render 29 characters against a
                // reported DISPLAY_SIZE of 23 (see the ODBC "Column Size"/
                // "Display Size" appendices' `20 + s` formula, `s` = declared
                // fractional-seconds scale).
                format!(
                    "{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}.{}",
                    format!("{fraction:09}").trim_end_matches('0')
                )
            }
        }
        ColumnValue::Bytes(data) => {
            // Hex-encode
            data.iter().map(|b| format!("{b:02X}")).collect()
        }
        ColumnValue::Guid(data) => {
            // UUID format: 8-4-4-4-12
            format!(
                "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
                data[0],
                data[1],
                data[2],
                data[3],
                data[4],
                data[5],
                data[6],
                data[7],
                data[8],
                data[9],
                data[10],
                data[11],
                data[12],
                data[13],
                data[14],
                data[15],
            )
        }
        ColumnValue::Decimal(s) => s.clone(),
        ColumnValue::TimestampTz {
            year,
            month,
            day,
            hour,
            minute,
            second,
            fraction,
            timezone_offset_minutes,
        } => {
            let sign = if *timezone_offset_minutes < 0 {
                '-'
            } else {
                '+'
            };
            let abs_offset = timezone_offset_minutes.unsigned_abs();
            let tz_h = abs_offset / 60;
            let tz_m = abs_offset % 60;
            if *fraction == 0 {
                format!(
                    "{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}{sign}{tz_h:02}:{tz_m:02}"
                )
            } else {
                format!(
                    "{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}.{fraction:09}{sign}{tz_h:02}:{tz_m:02}"
                )
            }
        }
        ColumnValue::Json(s) => s.clone(),
        ColumnValue::Array(items) => {
            let parts: Vec<String> = items.iter().map(column_value_to_string).collect();
            format!("[{}]", parts.join(", "))
        }
        ColumnValue::Map(pairs) => {
            let parts: Vec<String> = pairs
                .iter()
                .map(|(k, v)| {
                    format!(
                        "{}={}",
                        column_value_to_string(k),
                        column_value_to_string(v)
                    )
                })
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
        ColumnValue::Row(fields) => {
            let parts: Vec<String> = fields.iter().map(column_value_to_string).collect();
            format!("({})", parts.join(", "))
        }
        ColumnValue::IntervalYearMonth { years, months } => {
            // Both fields carry the same sign (see the parser that produces
            // this variant), so either is a valid sign source; render the
            // sign once up front rather than letting each field print its
            // own, which would otherwise yield something like "-1--6".
            let negative = *years < 0 || *months < 0;
            let sign = if negative { "-" } else { "" };
            format!("{sign}{}-{}", years.unsigned_abs(), months.unsigned_abs())
        }
        ColumnValue::IntervalDayTime { total_milliseconds } => {
            let negative = *total_milliseconds < 0;
            let sign = if negative { "-" } else { "" };
            let total_ms = total_milliseconds.unsigned_abs();
            let ms = total_ms % 1000;
            let total_s = total_ms / 1000;
            let s = total_s % 60;
            let total_m = total_s / 60;
            let m = total_m % 60;
            let total_h = total_m / 60;
            let h = total_h % 24;
            let days = total_h / 24;
            format!("{sign}{days} {h:02}:{m:02}:{s:02}.{ms:03}")
        }
    }
}

// ---------------------------------------------------------------------------
// Helper: convert ColumnValue to raw bytes for SQL_C_BINARY targets
// ---------------------------------------------------------------------------

/// Convert a [`ColumnValue`] to raw bytes for writing into a `SQL_C_BINARY` buffer.
///
/// Mirrors [`column_value_to_string`]: a single function that handles all variants,
/// so the `(_, CDataType::Binary)` catch-all in [`write_column_value`] never needs
/// per-variant arms.
///
/// Numeric types are written as little-endian machine bytes (ODBC permits any
/// representation for numeric→binary; LE matches what most clients expect and what
/// `struct.unpack` in Python decodes by default with `<`).
/// String types are returned as UTF-8 bytes.
/// Structured types fall back to their string representation as UTF-8 bytes.
fn column_value_to_binary(value: &ColumnValue) -> Vec<u8> {
    match value {
        // Raw byte types: return as-is
        ColumnValue::Bytes(b) => b.clone(),
        ColumnValue::Guid(data) => data.to_vec(),
        // Integer types: little-endian machine representation
        ColumnValue::I8(v) => v.to_le_bytes().to_vec(),
        ColumnValue::I16(v) => v.to_le_bytes().to_vec(),
        ColumnValue::I32(v) => v.to_le_bytes().to_vec(),
        ColumnValue::I64(v) => v.to_le_bytes().to_vec(),
        // Float types: little-endian IEEE 754 representation
        ColumnValue::F32(v) => v.to_le_bytes().to_vec(),
        ColumnValue::F64(v) => v.to_le_bytes().to_vec(),
        // Bool: single byte (0 or 1)
        ColumnValue::Bool(v) => vec![*v as u8],
        // Null is handled before the match in write_column_value and never reaches here.
        // Strings and all structured types: fall back to UTF-8 string representation.
        _ => column_value_to_string(value).into_bytes(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_value_writes_null_indicator() {
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::Null,
                CDataType::SLong,
                std::ptr::null_mut(),
                0,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(ind, -1); // NULL_DATA
    }

    // -----------------------------------------------------------------------
    // SQL_C_DEFAULT and the application's buffer
    //
    // For an explicitly named fixed C type the spec says BufferLength is
    // ignored, because the application named the type and therefore knows its
    // size. SQL_C_DEFAULT inverts that: the *driver* picks the C type, and it
    // picks from the runtime ColumnValue variant rather than the sql_type the
    // application sized its buffer from. Nothing cross-checks the two, so the
    // buffer length is the only bound available.
    // -----------------------------------------------------------------------

    /// A canary either side of the target buffer catches an overrun even when
    /// the allocator happens to leave slack after it.
    fn with_guarded_buffer(
        len: usize,
        f: impl FnOnce(*mut c_void) -> Result<SqlReturn, OdbcError>,
    ) {
        let mut arena = vec![0xAAu8; len + 32];
        let target = unsafe { arena.as_mut_ptr().add(16) };
        let result = f(target.cast());

        assert!(
            result.is_err(),
            "expected an error rather than a write past the buffer, got {result:?}"
        );
        assert!(
            arena[..16].iter().all(|&b| b == 0xAA),
            "wrote before the buffer: {:?}",
            &arena[..16]
        );
        assert!(
            arena[16 + len..].iter().all(|&b| b == 0xAA),
            "wrote past the end of a {len}-byte buffer: {:?}",
            &arena[16 + len..]
        );
    }

    #[test]
    fn default_c_type_will_not_write_a_timestamp_into_a_four_byte_buffer() {
        // An application that saw SQL_INTEGER from SQLDescribeCol binds four
        // bytes with SQL_C_DEFAULT. A backend that then yields a Timestamp must
        // not cause a 16-byte write.
        let value = ColumnValue::Timestamp {
            year: 2026,
            month: 7,
            day: 27,
            hour: 12,
            minute: 30,
            second: 15,
            fraction: 0,
        };
        let mut ind: isize = 0;
        with_guarded_buffer(4, |target| unsafe {
            write_column_value(&value, CDataType::Default, target, 4, &mut ind)
        });
    }

    #[test]
    fn default_c_type_will_not_write_an_i64_into_a_four_byte_buffer() {
        let mut ind: isize = 0;
        with_guarded_buffer(4, |target| unsafe {
            write_column_value(
                &ColumnValue::I64(i64::MAX),
                CDataType::Default,
                target,
                4,
                &mut ind,
            )
        });
    }

    #[test]
    fn default_c_type_still_writes_when_the_buffer_is_large_enough() {
        let mut buf: i64 = 0;
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::I64(42),
                CDataType::Default,
                &mut buf as *mut i64 as *mut c_void,
                8,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(buf, 42);
        assert_eq!(ind, 8);
    }

    #[test]
    fn an_explicitly_named_fixed_c_type_still_ignores_buffer_length() {
        // The spec says so, and applications rely on it: naming SQL_C_SBIGINT
        // is a statement that the buffer is eight bytes, whatever is passed as
        // BufferLength. Only the SQL_C_DEFAULT path gains a bound.
        let mut buf: i64 = 0;
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::I64(42),
                CDataType::SBigInt,
                &mut buf as *mut i64 as *mut c_void,
                0,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(buf, 42);
    }

    #[test]
    fn i32_value_to_slong_buffer() {
        let mut buf: i32 = 0;
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::I32(42),
                CDataType::SLong,
                &mut buf as *mut i32 as *mut c_void,
                4,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(buf, 42);
        assert_eq!(ind, 4);
    }

    #[test]
    fn i64_value_to_sbigint_buffer() {
        let mut buf: i64 = 0;
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::I64(123_456_789),
                CDataType::SBigInt,
                &mut buf as *mut i64 as *mut c_void,
                8,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(buf, 123_456_789);
        assert_eq!(ind, 8);
    }

    #[test]
    fn i8_value_to_stinyint_buffer() {
        let mut buf: i8 = 0;
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::I8(-42),
                CDataType::STinyInt,
                &mut buf as *mut i8 as *mut c_void,
                1,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(buf, -42);
        assert_eq!(ind, 1);
    }

    #[test]
    fn i16_value_to_sshort_buffer() {
        let mut buf: i16 = 0;
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::I16(1234),
                CDataType::SShort,
                &mut buf as *mut i16 as *mut c_void,
                2,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(buf, 1234);
        assert_eq!(ind, 2);
    }

    #[test]
    fn f32_value_to_float_buffer() {
        let mut buf: f32 = 0.0;
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::F32(2.5),
                CDataType::Float,
                &mut buf as *mut f32 as *mut c_void,
                4,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert!((buf - 2.5).abs() < f32::EPSILON);
        assert_eq!(ind, 4);
    }

    #[test]
    fn f64_value_to_double_buffer() {
        let mut buf: f64 = 0.0;
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::F64(std::f64::consts::PI),
                CDataType::Double,
                &mut buf as *mut f64 as *mut c_void,
                8,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert!((buf - std::f64::consts::PI).abs() < f64::EPSILON);
        assert_eq!(ind, 8);
    }

    #[test]
    fn bool_value_to_bit_buffer() {
        let mut buf: u8 = 0;
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::Bool(true),
                CDataType::Bit,
                &mut buf as *mut u8 as *mut c_void,
                1,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(buf, 1);
        assert_eq!(ind, 1);
    }

    #[test]
    fn bool_false_value_to_bit_buffer() {
        let mut buf: u8 = 1;
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::Bool(false),
                CDataType::Bit,
                &mut buf as *mut u8 as *mut c_void,
                1,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(buf, 0);
        assert_eq!(ind, 1);
    }

    #[test]
    fn string_value_to_wchar_buffer() {
        let mut buf = [0u16; 20];
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::String("hello".into()),
                CDataType::WChar,
                buf.as_mut_ptr() as *mut c_void,
                40,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(ind, 10); // 5 chars * 2 bytes
        let s = String::from_utf16_lossy(&buf[..5]);
        assert_eq!(s, "hello");
    }

    #[test]
    fn string_truncation_returns_success_with_info() {
        let mut buf = [0u16; 4]; // room for 3 chars + null
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::String("hello".into()),
                CDataType::WChar,
                buf.as_mut_ptr() as *mut c_void,
                8, // 4 u16 slots = 8 bytes
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS_WITH_INFO);
        assert_eq!(ind, 10); // reports full size needed
    }

    #[test]
    fn wchar_buffer_too_small_for_null_terminator_writes_nothing() {
        // A 1-byte buffer cannot hold the 2-byte UTF-16 null terminator.
        // Spec: "If the data buffer supplied is too small to hold the
        // null-termination character, SQLGetData returns SQL_SUCCESS_WITH_INFO
        // and SQLSTATE 01004."
        let mut buf = [0xAAu8; 4];
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::String("hello".into()),
                CDataType::WChar,
                buf.as_mut_ptr() as *mut c_void,
                1, // only one byte is available
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS_WITH_INFO);
        assert_eq!(ind, 10); // the full size needed is still reported
        assert_eq!(
            buf, [0xAA; 4],
            "wrote past the end of the caller's 1-byte buffer"
        );
    }

    #[test]
    fn wchar_zero_length_buffer_reports_size_and_writes_nothing() {
        // buf_len == 0 is a length query: report the byte count, write nothing.
        let mut buf = [0xAAu8; 4];
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::String("hello".into()),
                CDataType::WChar,
                buf.as_mut_ptr() as *mut c_void,
                0,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(ind, 10); // 5 chars * 2 bytes, still reported
        assert_eq!(buf, [0xAA; 4], "wrote into a zero-length buffer");
    }

    #[test]
    fn wchar_buffer_holding_only_the_null_terminator_is_written() {
        // Two bytes is exactly one UTF-16 code unit: room for the terminator
        // and nothing else. This pins the boundary of the guard above.
        let mut buf = [0xAAu8; 4];
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::String("hello".into()),
                CDataType::WChar,
                buf.as_mut_ptr() as *mut c_void,
                2,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS_WITH_INFO);
        assert_eq!(ind, 10);
        assert_eq!(&buf[..2], &[0x00, 0x00], "null terminator not written");
        assert_eq!(&buf[2..], &[0xAA, 0xAA], "wrote past the 2-byte buffer");
    }

    #[test]
    fn default_type_infers_correctly() {
        let mut buf: i32 = 0;
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::I32(99),
                CDataType::Default,
                &mut buf as *mut i32 as *mut c_void,
                4,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(buf, 99);
    }

    #[test]
    fn string_to_char_buffer_utf8() {
        let mut buf = [0u8; 20];
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::String("hello".into()),
                CDataType::Char,
                buf.as_mut_ptr() as *mut c_void,
                20,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(ind, 5); // 5 UTF-8 bytes
        assert_eq!(&buf[..5], b"hello");
        assert_eq!(buf[5], 0); // null terminator
    }

    #[test]
    fn string_to_char_truncation() {
        let mut buf = [0u8; 4]; // room for 3 chars + null
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::String("hello".into()),
                CDataType::Char,
                buf.as_mut_ptr() as *mut c_void,
                4,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS_WITH_INFO);
        assert_eq!(ind, 5); // reports full size
        assert_eq!(&buf[..3], b"hel");
        assert_eq!(buf[3], 0); // null terminator
    }

    #[test]
    fn bytes_to_binary_buffer() {
        let mut buf = [0u8; 10];
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF]),
                CDataType::Binary,
                buf.as_mut_ptr() as *mut c_void,
                10,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(ind, 4);
        assert_eq!(&buf[..4], &[0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn bytes_truncation() {
        let mut buf = [0u8; 2];
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF]),
                CDataType::Binary,
                buf.as_mut_ptr() as *mut c_void,
                2,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS_WITH_INFO);
        assert_eq!(ind, 4); // reports full size
        assert_eq!(&buf[..2], &[0xDE, 0xAD]);
    }

    #[test]
    fn date_value_to_type_date() {
        let mut buf = [0u8; std::mem::size_of::<Date>()];
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::Date {
                    year: 2025,
                    month: 6,
                    day: 15,
                },
                CDataType::TypeDate,
                buf.as_mut_ptr() as *mut c_void,
                std::mem::size_of::<Date>() as isize,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(ind, std::mem::size_of::<Date>() as isize);
        let ds = unsafe { std::ptr::read_unaligned(buf.as_ptr().cast::<Date>()) };
        assert_eq!(ds.year, 2025);
        assert_eq!(ds.month, 6);
        assert_eq!(ds.day, 15);
    }

    #[test]
    fn time_value_to_type_time() {
        let mut buf = [0u8; std::mem::size_of::<Time>()];
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::Time {
                    hour: 14,
                    minute: 30,
                    second: 45,
                    fraction: 0,
                },
                CDataType::TypeTime,
                buf.as_mut_ptr() as *mut c_void,
                std::mem::size_of::<Time>() as isize,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        let ts = unsafe { std::ptr::read_unaligned(buf.as_ptr().cast::<Time>()) };
        assert_eq!(ts.hour, 14);
        assert_eq!(ts.minute, 30);
        assert_eq!(ts.second, 45);
    }

    #[test]
    fn time_with_fraction_to_type_time_reports_01s07() {
        // SQL_TIME_STRUCT cannot carry the fraction: the whole-second parts
        // must still be written, and 01S07 reported for the dropped part.
        let mut buf = [0u8; std::mem::size_of::<Time>()];
        let mut ind: isize = 0;
        let err = unsafe {
            write_column_value(
                &ColumnValue::Time {
                    hour: 14,
                    minute: 30,
                    second: 45,
                    fraction: 123_000_000,
                },
                CDataType::TypeTime,
                buf.as_mut_ptr() as *mut c_void,
                std::mem::size_of::<Time>() as isize,
                &mut ind,
            )
        }
        .expect_err("non-zero fraction dropped to SQL_TIME_STRUCT must report 01S07");
        assert_eq!(err.sqlstate().as_str(), "01S07");
        let ts = unsafe { std::ptr::read_unaligned(buf.as_ptr().cast::<Time>()) };
        assert_eq!((ts.hour, ts.minute, ts.second), (14, 30, 45));
    }

    #[test]
    fn time_zero_fraction_to_type_time_is_plain_success() {
        let mut buf = [0u8; std::mem::size_of::<Time>()];
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::Time {
                    hour: 1,
                    minute: 2,
                    second: 3,
                    fraction: 0,
                },
                CDataType::TypeTime,
                buf.as_mut_ptr() as *mut c_void,
                std::mem::size_of::<Time>() as isize,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
    }

    #[test]
    fn time_with_fraction_coerced_to_wchar_keeps_milliseconds() {
        // A time(3) value's milliseconds must survive the string rendering
        // used for SQL_C_CHAR / SQL_C_WCHAR, even though the same value would
        // lose them writing to SQL_C_TYPE_TIME (see the 01S07 test above).
        let mut buf = [0u16; 20];
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::Time {
                    hour: 14,
                    minute: 30,
                    second: 45,
                    fraction: 123_000_000,
                },
                CDataType::WChar,
                buf.as_mut_ptr() as *mut c_void,
                40,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        let char_count = (ind / 2) as usize;
        let s = String::from_utf16_lossy(&buf[..char_count]);
        assert_eq!(s, "14:30:45.123");
    }

    #[test]
    fn timestamp_value_to_type_timestamp() {
        let mut buf = [0u8; std::mem::size_of::<Timestamp>()];
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::Timestamp {
                    year: 2025,
                    month: 6,
                    day: 15,
                    hour: 14,
                    minute: 30,
                    second: 45,
                    fraction: 123_000_000,
                },
                CDataType::TypeTimestamp,
                buf.as_mut_ptr() as *mut c_void,
                std::mem::size_of::<Timestamp>() as isize,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        let ts = unsafe { std::ptr::read_unaligned(buf.as_ptr().cast::<Timestamp>()) };
        assert_eq!(ts.year, 2025);
        assert_eq!(ts.month, 6);
        assert_eq!(ts.day, 15);
        assert_eq!(ts.hour, 14);
        assert_eq!(ts.minute, 30);
        assert_eq!(ts.second, 45);
        assert_eq!(ts.fraction, 123_000_000);
    }

    /// TimestampTz writes date/time fields to SQL_TIMESTAMP_STRUCT and drops
    /// the offset; ODBC has no timezone-aware timestamp C type.
    #[test]
    fn timestamp_tz_value_to_type_timestamp() {
        let mut buf = [0u8; std::mem::size_of::<Timestamp>()];
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::TimestampTz {
                    year: 2025,
                    month: 6,
                    day: 15,
                    hour: 14,
                    minute: 30,
                    second: 45,
                    fraction: 123_000_000,
                    timezone_offset_minutes: 330, // +05:30 — dropped by write_column_value
                },
                CDataType::TypeTimestamp,
                buf.as_mut_ptr() as *mut c_void,
                std::mem::size_of::<Timestamp>() as isize,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        let ts = unsafe { std::ptr::read_unaligned(buf.as_ptr().cast::<Timestamp>()) };
        assert_eq!(ts.year, 2025);
        assert_eq!(ts.month, 6);
        assert_eq!(ts.day, 15);
        assert_eq!(ts.hour, 14);
        assert_eq!(ts.minute, 30);
        assert_eq!(ts.second, 45);
        assert_eq!(ts.fraction, 123_000_000);
    }

    // -----------------------------------------------------------------------
    // Numeric ColumnValue requested as a datetime C type
    // -----------------------------------------------------------------------
    //
    // stackable-odbc-core has no backend-specific knowledge of numeric datetime
    // encodings some data sources use for DATE/TIME/TIMESTAMP storage:
    // decoding those, if a backend's data source has any, is that backend's
    // job, done at fetch time before the value ever reaches
    // `write_column_value`. A genuinely numeric column value (one the
    // backend did NOT recognise as an encoded datetime) has no defined
    // conversion to a datetime C type per the ODBC conversion matrix, so it
    // must fall through to the generic 07006 case rather than being
    // reinterpreted as a timestamp.

    #[test]
    fn integer_value_requested_as_type_timestamp_returns_07006() {
        let mut buf = [0u8; std::mem::size_of::<Timestamp>()];
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::I64(1_700_000_000),
                CDataType::TypeTimestamp,
                buf.as_mut_ptr() as *mut c_void,
                std::mem::size_of::<Timestamp>() as isize,
                &mut ind,
            )
        };
        let err = ret.expect_err("a plain integer column has no datetime conversion");
        assert_eq!(
            err.sqlstate().as_str(),
            crate::types::sql_state::RESTRICTED_DATA_TYPE_ATTRIBUTE_VIOLATION
        );
    }

    #[test]
    fn float_value_requested_as_type_date_returns_07006() {
        let mut buf = [0u8; std::mem::size_of::<Date>()];
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::F64(2_451_545.0),
                CDataType::TypeDate,
                buf.as_mut_ptr() as *mut c_void,
                std::mem::size_of::<Date>() as isize,
                &mut ind,
            )
        };
        let err = ret.expect_err("a plain float column has no datetime conversion");
        assert_eq!(
            err.sqlstate().as_str(),
            crate::types::sql_state::RESTRICTED_DATA_TYPE_ATTRIBUTE_VIOLATION
        );
    }

    #[test]
    fn i32_coerced_to_wchar() {
        let mut buf = [0u16; 20];
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::I32(42),
                CDataType::WChar,
                buf.as_mut_ptr() as *mut c_void,
                40,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(ind, 4); // "42" = 2 chars * 2 bytes
        let s = String::from_utf16_lossy(&buf[..2]);
        assert_eq!(s, "42");
    }

    #[test]
    fn i32_coerced_to_char() {
        let mut buf = [0u8; 20];
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::I32(42),
                CDataType::Char,
                buf.as_mut_ptr() as *mut c_void,
                20,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(ind, 2); // "42" = 2 bytes
        assert_eq!(&buf[..2], b"42");
    }

    #[test]
    fn unsupported_conversion_returns_error() {
        let mut buf: f64 = 0.0;
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::String("hello".into()),
                CDataType::Double,
                &mut buf as *mut f64 as *mut c_void,
                8,
                &mut ind,
            )
        };
        assert!(ret.is_err());
    }

    #[test]
    fn null_target_ptr_just_reports_length() {
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::String("hello".into()),
                CDataType::WChar,
                std::ptr::null_mut(),
                0,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(ind, 10); // 5 chars * 2 bytes
    }

    #[test]
    fn default_type_for_string_infers_wchar() {
        let mut buf = [0u16; 20];
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::String("hi".into()),
                CDataType::Default,
                buf.as_mut_ptr() as *mut c_void,
                40,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(ind, 4); // 2 chars * 2 bytes
        let s = String::from_utf16_lossy(&buf[..2]);
        assert_eq!(s, "hi");
    }

    #[test]
    fn default_type_for_bool_infers_bit() {
        let mut buf: u8 = 0;
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::Bool(true),
                CDataType::Default,
                &mut buf as *mut u8 as *mut c_void,
                1,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(buf, 1);
    }

    // -----------------------------------------------------------------------
    // Guid → Binary
    // -----------------------------------------------------------------------

    #[test]
    fn guid_to_binary_buffer() {
        let guid: [u8; 16] = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E,
            0x0F, 0x10,
        ];
        let mut buf = [0u8; 20];
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::Guid(guid),
                CDataType::Binary,
                buf.as_mut_ptr() as *mut c_void,
                20,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(ind, 16);
        assert_eq!(&buf[..16], &guid);
    }

    // -----------------------------------------------------------------------
    // Default type inference for remaining variants
    // -----------------------------------------------------------------------

    #[test]
    fn default_type_for_i8_infers_stinyint() {
        let mut buf: i8 = 0;
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::I8(77),
                CDataType::Default,
                &mut buf as *mut i8 as *mut c_void,
                1,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(buf, 77);
    }

    #[test]
    fn default_type_for_i16_infers_sshort() {
        let mut buf: i16 = 0;
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::I16(999),
                CDataType::Default,
                &mut buf as *mut i16 as *mut c_void,
                2,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(buf, 999);
    }

    #[test]
    fn default_type_for_i64_infers_sbigint() {
        let mut buf: i64 = 0;
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::I64(987_654_321),
                CDataType::Default,
                &mut buf as *mut i64 as *mut c_void,
                8,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(buf, 987_654_321);
    }

    #[test]
    fn default_type_for_f32_infers_float() {
        let mut buf: f32 = 0.0;
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::F32(1.5),
                CDataType::Default,
                &mut buf as *mut f32 as *mut c_void,
                4,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert!((buf - 1.5).abs() < f32::EPSILON);
    }

    #[test]
    fn default_type_for_f64_infers_double() {
        let mut buf: f64 = 0.0;
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::F64(std::f64::consts::E),
                CDataType::Default,
                &mut buf as *mut f64 as *mut c_void,
                8,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert!((buf - std::f64::consts::E).abs() < f64::EPSILON);
    }

    #[test]
    fn default_type_for_date_infers_type_date() {
        let mut buf = [0u8; std::mem::size_of::<Date>()];
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::Date {
                    year: 2026,
                    month: 3,
                    day: 27,
                },
                CDataType::Default,
                buf.as_mut_ptr() as *mut c_void,
                std::mem::size_of::<Date>() as isize,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        let ds = unsafe { std::ptr::read_unaligned(buf.as_ptr().cast::<Date>()) };
        assert_eq!(ds.year, 2026);
        assert_eq!(ds.month, 3);
        assert_eq!(ds.day, 27);
    }

    #[test]
    fn default_type_for_time_infers_type_time() {
        let mut buf = [0u8; std::mem::size_of::<Time>()];
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::Time {
                    hour: 9,
                    minute: 15,
                    second: 30,
                    fraction: 0,
                },
                CDataType::Default,
                buf.as_mut_ptr() as *mut c_void,
                std::mem::size_of::<Time>() as isize,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        let ts = unsafe { std::ptr::read_unaligned(buf.as_ptr().cast::<Time>()) };
        assert_eq!(ts.hour, 9);
        assert_eq!(ts.minute, 15);
        assert_eq!(ts.second, 30);
    }

    #[test]
    fn default_type_for_timestamp_infers_type_timestamp() {
        let mut buf = [0u8; std::mem::size_of::<Timestamp>()];
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::Timestamp {
                    year: 2026,
                    month: 3,
                    day: 27,
                    hour: 9,
                    minute: 15,
                    second: 30,
                    fraction: 0,
                },
                CDataType::Default,
                buf.as_mut_ptr() as *mut c_void,
                std::mem::size_of::<Timestamp>() as isize,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        let ts = unsafe { std::ptr::read_unaligned(buf.as_ptr().cast::<Timestamp>()) };
        assert_eq!(ts.year, 2026);
    }

    #[test]
    fn default_type_for_bytes_infers_binary() {
        let mut buf = [0u8; 10];
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::Bytes(vec![0xAB, 0xCD]),
                CDataType::Default,
                buf.as_mut_ptr() as *mut c_void,
                10,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(ind, 2);
        assert_eq!(&buf[..2], &[0xAB, 0xCD]);
    }

    #[test]
    fn default_type_for_guid_infers_binary() {
        let guid: [u8; 16] = [0xAA; 16];
        let mut buf = [0u8; 20];
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::Guid(guid),
                CDataType::Default,
                buf.as_mut_ptr() as *mut c_void,
                20,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(ind, 16);
        assert_eq!(&buf[..16], &[0xAA; 16]);
    }

    // -----------------------------------------------------------------------
    // Coercion to WChar/Char for non-I32 types
    // -----------------------------------------------------------------------

    #[test]
    fn bool_true_coerced_to_wchar() {
        let mut buf = [0u16; 10];
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::Bool(true),
                CDataType::WChar,
                buf.as_mut_ptr() as *mut c_void,
                20,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(ind, 2); // "1" = 1 char * 2 bytes
        let s = String::from_utf16_lossy(&buf[..1]);
        assert_eq!(s, "1");
    }

    #[test]
    fn bool_false_coerced_to_char() {
        let mut buf = [0u8; 10];
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::Bool(false),
                CDataType::Char,
                buf.as_mut_ptr() as *mut c_void,
                10,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(ind, 1); // "0" = 1 byte
        assert_eq!(&buf[..1], b"0");
    }

    #[test]
    fn f64_coerced_to_wchar() {
        let mut buf = [0u16; 30];
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::F64(1.5),
                CDataType::WChar,
                buf.as_mut_ptr() as *mut c_void,
                60,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        let char_count = (ind / 2) as usize;
        let s = String::from_utf16_lossy(&buf[..char_count]);
        assert_eq!(s, "1.5");
    }

    #[test]
    fn date_coerced_to_wchar() {
        let mut buf = [0u16; 20];
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::Date {
                    year: 2025,
                    month: 6,
                    day: 15,
                },
                CDataType::WChar,
                buf.as_mut_ptr() as *mut c_void,
                40,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        let char_count = (ind / 2) as usize;
        let s = String::from_utf16_lossy(&buf[..char_count]);
        assert_eq!(s, "2025-06-15");
    }

    #[test]
    fn time_coerced_to_char() {
        let mut buf = [0u8; 20];
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::Time {
                    hour: 14,
                    minute: 30,
                    second: 45,
                    fraction: 0,
                },
                CDataType::Char,
                buf.as_mut_ptr() as *mut c_void,
                20,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(&buf[..ind as usize], b"14:30:45");
    }

    #[test]
    fn timestamp_with_fraction_coerced_to_wchar() {
        let mut buf = [0u16; 40];
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::Timestamp {
                    year: 2025,
                    month: 6,
                    day: 15,
                    hour: 14,
                    minute: 30,
                    second: 45,
                    fraction: 123_000_000,
                },
                CDataType::WChar,
                buf.as_mut_ptr() as *mut c_void,
                80,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        let char_count = (ind / 2) as usize;
        let s = String::from_utf16_lossy(&buf[..char_count]);
        // Trailing zeros trimmed (matching the `Time` rendering below), not
        // the fixed 9-digit nanosecond padding: a `timestamp(3)` value's
        // fraction is stored zero-padded internally, and printing all 9
        // digits would over-report the column's actual precision.
        assert_eq!(s, "2025-06-15 14:30:45.123");
    }

    #[test]
    fn timestamp_zero_fraction_coerced_to_wchar() {
        let mut buf = [0u16; 40];
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::Timestamp {
                    year: 2025,
                    month: 6,
                    day: 15,
                    hour: 14,
                    minute: 30,
                    second: 45,
                    fraction: 0,
                },
                CDataType::WChar,
                buf.as_mut_ptr() as *mut c_void,
                80,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        let char_count = (ind / 2) as usize;
        let s = String::from_utf16_lossy(&buf[..char_count]);
        assert_eq!(s, "2025-06-15 14:30:45");
    }

    #[test]
    fn bytes_coerced_to_wchar_hex() {
        let mut buf = [0u16; 20];
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF]),
                CDataType::WChar,
                buf.as_mut_ptr() as *mut c_void,
                40,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        let char_count = (ind / 2) as usize;
        let s = String::from_utf16_lossy(&buf[..char_count]);
        assert_eq!(s, "DEADBEEF");
    }

    #[test]
    fn guid_coerced_to_wchar_uuid_format() {
        let guid: [u8; 16] = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E,
            0x0F, 0x10,
        ];
        let mut buf = [0u16; 40];
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::Guid(guid),
                CDataType::WChar,
                buf.as_mut_ptr() as *mut c_void,
                80,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        let char_count = (ind / 2) as usize;
        let s = String::from_utf16_lossy(&buf[..char_count]);
        assert_eq!(s, "01020304-0506-0708-090A-0B0C0D0E0F10");
    }

    #[test]
    fn guid_coerced_to_char_uuid_format() {
        let guid: [u8; 16] = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E,
            0x0F, 0x10,
        ];
        let mut buf = [0u8; 40];
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::Guid(guid),
                CDataType::Char,
                buf.as_mut_ptr() as *mut c_void,
                40,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(
            &buf[..ind as usize],
            b"01020304-0506-0708-090A-0B0C0D0E0F10"
        );
    }

    // -----------------------------------------------------------------------
    // Null len_ind_ptr
    // -----------------------------------------------------------------------

    #[test]
    fn null_len_ind_ptr_with_fixed_type() {
        let mut buf: i32 = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::I32(42),
                CDataType::SLong,
                &mut buf as *mut i32 as *mut c_void,
                4,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(buf, 42);
    }

    #[test]
    fn null_len_ind_ptr_with_wchar() {
        let mut buf = [0u16; 20];
        let ret = unsafe {
            write_column_value(
                &ColumnValue::String("hi".into()),
                CDataType::WChar,
                buf.as_mut_ptr() as *mut c_void,
                40,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        let s = String::from_utf16_lossy(&buf[..2]);
        assert_eq!(s, "hi");
    }

    #[test]
    fn null_len_ind_ptr_with_char() {
        let mut buf = [0u8; 20];
        let ret = unsafe {
            write_column_value(
                &ColumnValue::String("hi".into()),
                CDataType::Char,
                buf.as_mut_ptr() as *mut c_void,
                20,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(&buf[..2], b"hi");
    }

    #[test]
    fn null_len_ind_ptr_with_binary() {
        let mut buf = [0u8; 10];
        let ret = unsafe {
            write_column_value(
                &ColumnValue::Bytes(vec![0xAB]),
                CDataType::Binary,
                buf.as_mut_ptr() as *mut c_void,
                10,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(buf[0], 0xAB);
    }

    #[test]
    fn null_len_ind_ptr_with_null_value() {
        let ret = unsafe {
            write_column_value(
                &ColumnValue::Null,
                CDataType::SLong,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
    }

    // --- column_value_to_string: new variant coverage ---

    #[test]
    fn decimal_to_string() {
        let v = ColumnValue::Decimal("123.456".to_string());
        assert_eq!(column_value_to_string(&v), "123.456");
    }

    #[test]
    fn timestamp_tz_no_fraction() {
        let v = ColumnValue::TimestampTz {
            year: 2024,
            month: 3,
            day: 15,
            hour: 10,
            minute: 30,
            second: 0,
            fraction: 0,
            timezone_offset_minutes: 60, // +01:00
        };
        assert_eq!(column_value_to_string(&v), "2024-03-15 10:30:00+01:00");
    }

    #[test]
    fn timestamp_tz_with_fraction_and_negative_offset() {
        let v = ColumnValue::TimestampTz {
            year: 2024,
            month: 1,
            day: 1,
            hour: 0,
            minute: 0,
            second: 0,
            fraction: 123_000_000,
            timezone_offset_minutes: -330, // -05:30
        };
        assert_eq!(
            column_value_to_string(&v),
            "2024-01-01 00:00:00.123000000-05:30"
        );
    }

    #[test]
    fn timestamp_tz_utc_zero_offset() {
        let v = ColumnValue::TimestampTz {
            year: 2000,
            month: 6,
            day: 15,
            hour: 12,
            minute: 0,
            second: 0,
            fraction: 0,
            timezone_offset_minutes: 0,
        };
        assert_eq!(column_value_to_string(&v), "2000-06-15 12:00:00+00:00");
    }

    #[test]
    fn json_to_string() {
        let v = ColumnValue::Json(r#"{"key":"value"}"#.to_string());
        assert_eq!(column_value_to_string(&v), r#"{"key":"value"}"#);
    }

    #[test]
    fn array_to_string() {
        let v = ColumnValue::Array(vec![ColumnValue::I32(1), ColumnValue::I32(2)]);
        assert_eq!(column_value_to_string(&v), "[1, 2]");
    }

    #[test]
    fn map_to_string() {
        let v = ColumnValue::Map(vec![(
            ColumnValue::String("a".to_string()),
            ColumnValue::I32(1),
        )]);
        assert_eq!(column_value_to_string(&v), "{a=1}");
    }

    #[test]
    fn row_to_string() {
        let v = ColumnValue::Row(vec![
            ColumnValue::I32(42),
            ColumnValue::String("hello".to_string()),
        ]);
        assert_eq!(column_value_to_string(&v), "(42, hello)");
    }

    #[test]
    fn interval_year_month_positive() {
        let v = ColumnValue::IntervalYearMonth {
            years: 2,
            months: 6,
        };
        assert_eq!(column_value_to_string(&v), "2-6");
    }

    #[test]
    fn interval_year_month_negative() {
        let v = ColumnValue::IntervalYearMonth {
            years: -2,
            months: -6,
        };
        assert_eq!(column_value_to_string(&v), "-2-6");
    }

    #[test]
    fn interval_day_time_positive() {
        let v = ColumnValue::IntervalDayTime {
            // 1 day + 1h 1m 1s 0ms
            total_milliseconds: 86_400_000 + 3_661_000,
        };
        assert_eq!(column_value_to_string(&v), "1 01:01:01.000");
    }

    #[test]
    fn interval_day_time_negative_days() {
        let v = ColumnValue::IntervalDayTime {
            total_milliseconds: -86_400_000,
        };
        assert_eq!(column_value_to_string(&v), "-1 00:00:00.000");
    }

    #[test]
    fn interval_day_time_negative_sub_day() {
        // Less than one full day, still negative: the split days/milliseconds
        // representation could not express this state at all.
        let v = ColumnValue::IntervalDayTime {
            total_milliseconds: -500,
        };
        assert_eq!(column_value_to_string(&v), "-0 00:00:00.500");
    }

    // -----------------------------------------------------------------------
    // Numeric cross-cast via pivot
    // -----------------------------------------------------------------------

    #[test]
    fn i8_widened_to_slong() {
        let mut buf: i32 = 0;
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::I8(-5),
                CDataType::SLong,
                &mut buf as *mut i32 as *mut c_void,
                4,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(buf, -5);
        assert_eq!(ind, 4);
    }

    #[test]
    fn i64_narrowed_to_stinyint() {
        let mut buf: i8 = 0;
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::I64(42),
                CDataType::STinyInt,
                &mut buf as *mut i8 as *mut c_void,
                1,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(buf, 42);
        assert_eq!(ind, 1);
    }

    #[test]
    fn i32_to_double() {
        let mut buf: f64 = 0.0;
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::I32(100),
                CDataType::Double,
                &mut buf as *mut f64 as *mut c_void,
                8,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert!((buf - 100.0).abs() < f64::EPSILON);
        assert_eq!(ind, 8);
    }

    #[test]
    fn f64_to_slong() {
        let mut buf: i32 = 0;
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::F64(3.9),
                CDataType::SLong,
                &mut buf as *mut i32 as *mut c_void,
                4,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(buf, 3); // truncation
        assert_eq!(ind, 4);
    }

    #[test]
    fn f64_narrowed_to_float() {
        let mut buf: f32 = 0.0;
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::F64(1.5),
                CDataType::Float,
                &mut buf as *mut f32 as *mut c_void,
                4,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert!((buf - 1.5_f32).abs() < f32::EPSILON);
        assert_eq!(ind, 4);
    }

    #[test]
    fn bool_true_to_slong() {
        let mut buf: i32 = 0;
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::Bool(true),
                CDataType::SLong,
                &mut buf as *mut i32 as *mut c_void,
                4,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(buf, 1);
        assert_eq!(ind, 4);
    }

    #[cfg(test)]
    mod proptest_column_value {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn string_variant_defaults_to_wchar(s in ".*") {
                let value = ColumnValue::String(s);
                let mut buf = [0u16; 256];
                let mut ind: isize = 0;
                let ret = unsafe {
                    write_column_value(
                        &value,
                        CDataType::Default,
                        buf.as_mut_ptr().cast(),
                        (buf.len() * 2) as isize,
                        &mut ind,
                    )
                };
                let r = ret.unwrap();
                assert!(
                    r == SqlReturn::SUCCESS || r == SqlReturn::SUCCESS_WITH_INFO,
                    "unexpected result {r:?}"
                );
            }

            #[test]
            fn i32_variant_defaults_to_slong(v in any::<i32>()) {
                let value = ColumnValue::I32(v);
                let mut out: i32 = 0;
                let mut ind: isize = 0;
                let ret = unsafe {
                    write_column_value(
                        &value,
                        CDataType::Default,
                        (&mut out as *mut i32).cast(),
                        4,
                        &mut ind,
                    )
                };
                assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
                assert_eq!(out, v);
            }

            #[test]
            fn f64_variant_defaults_to_double(v in any::<f64>()) {
                let value = ColumnValue::F64(v);
                let mut out: f64 = 0.0;
                let mut ind: isize = 0;
                let ret = unsafe {
                    write_column_value(
                        &value,
                        CDataType::Default,
                        (&mut out as *mut f64).cast(),
                        8,
                        &mut ind,
                    )
                };
                assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
                assert_eq!(out.to_bits(), v.to_bits());
            }

            #[test]
            fn decimal_variant_defaults_to_wchar_string(s in "[0-9]{1,10}\\.[0-9]{1,5}") {
                let value = ColumnValue::Decimal(s.clone());
                let mut buf = [0u16; 64];
                let mut ind: isize = 0;
                let ret = unsafe {
                    write_column_value(
                        &value,
                        CDataType::Default,
                        buf.as_mut_ptr().cast(),
                        (buf.len() * 2) as isize,
                        &mut ind,
                    )
                };
                assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
                let code_units = (ind as usize) / 2;
                let decoded = String::from_utf16_lossy(&buf[..code_units]);
                assert_eq!(decoded, s);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Tests for numeric overflow detection (SQLSTATE 22003)
    // -----------------------------------------------------------------------

    fn sqlstate_of_err(err: &OdbcError) -> String {
        err.sqlstate().as_str().to_owned()
    }

    #[test]
    fn int_to_stinyint_overflow_returns_22003() {
        let mut buf: i8 = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::I64(128), // i8::MAX + 1
                CDataType::STinyInt,
                &mut buf as *mut i8 as *mut c_void,
                1,
                std::ptr::null_mut(),
            )
        };
        let err = ret.unwrap_err();
        assert_eq!(sqlstate_of_err(&err), "22003");
    }

    #[test]
    fn int_to_stinyint_boundary_succeeds() {
        let mut buf: i8 = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::I64(i64::from(i8::MAX)),
                CDataType::STinyInt,
                &mut buf as *mut i8 as *mut c_void,
                1,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(buf, i8::MAX);
    }

    #[test]
    fn int_to_sshort_overflow_returns_22003() {
        let mut buf: i16 = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::I64(i64::from(i16::MAX) + 1),
                CDataType::SShort,
                &mut buf as *mut i16 as *mut c_void,
                2,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(sqlstate_of_err(&ret.unwrap_err()), "22003");
    }

    #[test]
    fn int_to_slong_overflow_returns_22003() {
        let mut buf: i32 = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::I64(i64::from(i32::MAX) + 1),
                CDataType::SLong,
                &mut buf as *mut i32 as *mut c_void,
                4,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(sqlstate_of_err(&ret.unwrap_err()), "22003");
    }

    #[test]
    fn int_to_utinyint_negative_returns_22003() {
        let mut buf: u8 = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::I64(-1),
                CDataType::UTinyInt,
                &mut buf as *mut u8 as *mut c_void,
                1,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(sqlstate_of_err(&ret.unwrap_err()), "22003");
    }

    #[test]
    fn int_to_utinyint_overflow_returns_22003() {
        let mut buf: u8 = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::I64(256),
                CDataType::UTinyInt,
                &mut buf as *mut u8 as *mut c_void,
                1,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(sqlstate_of_err(&ret.unwrap_err()), "22003");
    }

    #[test]
    fn int_to_ubigint_negative_returns_22003() {
        let mut buf: u64 = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::I64(-1),
                CDataType::UBigInt,
                &mut buf as *mut u64 as *mut c_void,
                8,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(sqlstate_of_err(&ret.unwrap_err()), "22003");
    }

    #[test]
    fn int_to_unsigned_types_succeed() {
        // UTinyInt
        let mut buf: u8 = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::I64(200),
                CDataType::UTinyInt,
                &mut buf as *mut u8 as *mut c_void,
                1,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(buf, 200);

        // UShort
        let mut buf16: u16 = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::I64(60000),
                CDataType::UShort,
                &mut buf16 as *mut u16 as *mut c_void,
                2,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(buf16, 60000);

        // ULong
        let mut buf32: u32 = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::I64(3_000_000_000),
                CDataType::ULong,
                &mut buf32 as *mut u32 as *mut c_void,
                4,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(buf32, 3_000_000_000);

        // UBigInt
        let mut buf64: u64 = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::I64(i64::MAX),
                CDataType::UBigInt,
                &mut buf64 as *mut u64 as *mut c_void,
                8,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(buf64, i64::MAX as u64);
    }

    // -----------------------------------------------------------------------
    // Tests for float-to-integer overflow (SQLSTATE 22003)
    // -----------------------------------------------------------------------

    #[test]
    fn float_to_stinyint_overflow_returns_22003() {
        let mut buf: i8 = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::F64(200.0),
                CDataType::STinyInt,
                &mut buf as *mut i8 as *mut c_void,
                1,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(sqlstate_of_err(&ret.unwrap_err()), "22003");
    }

    #[test]
    fn float_infinity_to_slong_returns_22003() {
        let mut buf: i32 = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::F64(f64::INFINITY),
                CDataType::SLong,
                &mut buf as *mut i32 as *mut c_void,
                4,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(sqlstate_of_err(&ret.unwrap_err()), "22003");
    }

    #[test]
    fn float_nan_to_sbigint_returns_22003() {
        let mut buf: i64 = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::F64(f64::NAN),
                CDataType::SBigInt,
                &mut buf as *mut i64 as *mut c_void,
                8,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(sqlstate_of_err(&ret.unwrap_err()), "22003");
    }

    #[test]
    fn float_negative_to_utinyint_returns_22003() {
        let mut buf: u8 = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::F64(-1.0),
                CDataType::UTinyInt,
                &mut buf as *mut u8 as *mut c_void,
                1,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(sqlstate_of_err(&ret.unwrap_err()), "22003");
    }

    #[test]
    fn float_to_unsigned_types_succeed() {
        let mut buf: u8 = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::F64(200.0),
                CDataType::UTinyInt,
                &mut buf as *mut u8 as *mut c_void,
                1,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(buf, 200);
    }

    // -----------------------------------------------------------------------
    // Tests for NaN → Bit (SQLSTATE 22003)
    // -----------------------------------------------------------------------

    #[test]
    fn float_nan_to_bit_returns_22003() {
        let mut buf: u8 = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::F64(f64::NAN),
                CDataType::Bit,
                &mut buf as *mut u8 as *mut c_void,
                1,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(sqlstate_of_err(&ret.unwrap_err()), "22003");
    }

    #[test]
    fn float_zero_to_bit_is_zero() {
        let mut buf: u8 = 1;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::F64(0.0),
                CDataType::Bit,
                &mut buf as *mut u8 as *mut c_void,
                1,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(buf, 0);
    }

    #[test]
    fn float_nonzero_to_bit_is_one() {
        let mut buf: u8 = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::F64(1.5),
                CDataType::Bit,
                &mut buf as *mut u8 as *mut c_void,
                1,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(buf, 1);
    }

    // -----------------------------------------------------------------------
    // Tests for Int → f32 precision loss (SQLSTATE 01S07)
    // -----------------------------------------------------------------------

    #[test]
    fn int_to_float_precision_loss_returns_01s07() {
        // 2^24 + 1 = 16_777_217 cannot be represented exactly as f32
        let mut buf: f32 = 0.0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::I64(16_777_217),
                CDataType::Float,
                &mut buf as *mut f32 as *mut c_void,
                4,
                std::ptr::null_mut(),
            )
        };
        let err = ret.unwrap_err();
        assert_eq!(err.sql_return(), SqlReturn::SUCCESS_WITH_INFO);
        assert_eq!(sqlstate_of_err(&err), "01S07");
        // Value is still written (truncated)
        assert_eq!(buf, 16_777_216.0_f32);
    }

    #[test]
    fn int_to_float_exact_value_succeeds() {
        // 2^24 = 16_777_216 is exactly representable as f32
        let mut buf: f32 = 0.0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::I64(16_777_216),
                CDataType::Float,
                &mut buf as *mut f32 as *mut c_void,
                4,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(buf, 16_777_216.0_f32);
    }

    // -----------------------------------------------------------------------
    // Tests for f64 → i64/u64 boundary saturation and f64 → f32 truncation
    // -----------------------------------------------------------------------

    #[test]
    fn f64_at_i64_max_boundary_returns_22003() {
        // i64::MAX as f64 rounds UP to 9223372036854775808.0, which is one
        // greater than i64::MAX. A `v > i64::MAX as f64` guard lets it through
        // and `v as i64` then saturates silently.
        let mut out = 0i64;
        let mut ind = 0isize;
        let err = unsafe {
            write_column_value(
                &ColumnValue::F64(9_223_372_036_854_775_808.0),
                CDataType::SBigInt,
                std::ptr::from_mut(&mut out).cast(),
                size_of::<i64>() as isize,
                &mut ind,
            )
        }
        .expect_err("2^63 does not fit in i64");
        assert_eq!(
            err.sqlstate().as_str(),
            crate::types::sql_state::NUMERIC_VALUE_OUT_OF_RANGE
        );
    }

    #[test]
    fn f64_at_u64_max_boundary_returns_22003() {
        let mut out = 0u64;
        let mut ind = 0isize;
        let err = unsafe {
            write_column_value(
                &ColumnValue::F64(18_446_744_073_709_551_616.0),
                CDataType::UBigInt,
                std::ptr::from_mut(&mut out).cast(),
                size_of::<u64>() as isize,
                &mut ind,
            )
        }
        .expect_err("2^64 does not fit in u64");
        assert_eq!(
            err.sqlstate().as_str(),
            crate::types::sql_state::NUMERIC_VALUE_OUT_OF_RANGE
        );
    }

    #[test]
    fn f64_narrowed_to_f32_with_precision_loss_warns() {
        let mut out = 0f32;
        let mut ind = 0isize;
        let err = unsafe {
            write_column_value(
                &ColumnValue::F64(0.1),
                CDataType::Float,
                std::ptr::from_mut(&mut out).cast(),
                size_of::<f32>() as isize,
                &mut ind,
            )
        }
        .expect_err("0.1 is not exactly representable in f32");
        assert_eq!(
            err.sqlstate().as_str(),
            crate::types::sql_state::FRACTIONAL_TRUNCATION
        );
        assert_eq!(err.sql_return(), SqlReturn::SUCCESS_WITH_INFO);
        // The value must still be written despite the warning.
        assert_eq!(out, 0.1f32);
    }

    #[test]
    fn f64_exactly_representable_in_f32_does_not_warn() {
        let mut out = 0f32;
        let mut ind = 0isize;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::F64(0.5),
                CDataType::Float,
                std::ptr::from_mut(&mut out).cast(),
                size_of::<f32>() as isize,
                &mut ind,
            )
        }
        .expect("0.5 is exact in f32");
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!(out, 0.5f32);
    }

    // -----------------------------------------------------------------------
    // Tests for Decimal/String → numeric C type conversion
    // -----------------------------------------------------------------------

    #[test]
    fn decimal_converts_to_double() {
        let mut out = 0f64;
        let mut ind = 0isize;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::Decimal("123.456".into()),
                CDataType::Double,
                std::ptr::from_mut(&mut out).cast(),
                size_of::<f64>() as isize,
                &mut ind,
            )
        }
        .expect("decimal should convert to SQL_C_DOUBLE");
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert!((out - 123.456).abs() < 1e-9);
    }

    #[test]
    fn decimal_integral_converts_to_bigint_without_precision_loss() {
        let mut out = 0i64;
        let mut ind = 0isize;
        let _ = unsafe {
            write_column_value(
                &ColumnValue::Decimal("9007199254740993".into()),
                CDataType::SBigInt,
                std::ptr::from_mut(&mut out).cast(),
                size_of::<i64>() as isize,
                &mut ind,
            )
        }
        .expect("integral decimal should convert to SQL_C_SBIGINT");
        // 2^53 + 1 is not representable in f64, so this fails if the parse
        // goes through f64 rather than trying i64 first.
        assert_eq!(out, 9_007_199_254_740_993);
    }

    #[test]
    fn string_converts_to_integer() {
        let mut out = 0i32;
        let mut ind = 0isize;
        let _ = unsafe {
            write_column_value(
                &ColumnValue::String("42".into()),
                CDataType::SLong,
                std::ptr::from_mut(&mut out).cast(),
                size_of::<i32>() as isize,
                &mut ind,
            )
        }
        .expect("numeric string should convert to SQL_C_SLONG");
        assert_eq!(out, 42);
    }

    #[test]
    fn non_numeric_string_returns_22018() {
        let mut out = 0i32;
        let mut ind = 0isize;
        let err = unsafe {
            write_column_value(
                &ColumnValue::String("not a number".into()),
                CDataType::SLong,
                std::ptr::from_mut(&mut out).cast(),
                size_of::<i32>() as isize,
                &mut ind,
            )
        }
        .expect_err("non-numeric string must not convert");
        assert_eq!(
            err.sqlstate().as_str(),
            crate::types::sql_state::INVALID_CHARACTER_VALUE_FOR_CAST
        );
    }

    #[test]
    fn out_of_range_string_returns_22003() {
        let mut out = 0i32;
        let mut ind = 0isize;
        let err = unsafe {
            write_column_value(
                &ColumnValue::String("99999999999".into()),
                CDataType::SLong,
                std::ptr::from_mut(&mut out).cast(),
                size_of::<i32>() as isize,
                &mut ind,
            )
        }
        .expect_err("out-of-range value must not convert");
        assert_eq!(
            err.sqlstate().as_str(),
            crate::types::sql_state::NUMERIC_VALUE_OUT_OF_RANGE
        );
    }

    // -----------------------------------------------------------------------
    // String → datetime C types
    // -----------------------------------------------------------------------

    #[test]
    fn string_converts_to_timestamp() {
        let mut out = Timestamp::default();
        let mut ind = 0isize;
        let _ = unsafe {
            write_column_value(
                &ColumnValue::String("2026-07-21 10:30:00.500".into()),
                CDataType::TypeTimestamp,
                std::ptr::from_mut(&mut out).cast(),
                size_of::<Timestamp>() as isize,
                &mut ind,
            )
        }
        .expect("ISO timestamp text should convert");
        assert_eq!((out.year, out.month, out.day), (2026, 7, 21));
        assert_eq!((out.hour, out.minute, out.second), (10, 30, 0));
        // SQL_TIMESTAMP_STRUCT.fraction is in nanoseconds.
        assert_eq!(out.fraction, 500_000_000);
    }

    #[test]
    fn string_converts_to_date() {
        let mut out = Date::default();
        let mut ind = 0isize;
        let _ = unsafe {
            write_column_value(
                &ColumnValue::String("2026-07-21".into()),
                CDataType::TypeDate,
                std::ptr::from_mut(&mut out).cast(),
                size_of::<Date>() as isize,
                &mut ind,
            )
        }
        .expect("ISO date text should convert");
        assert_eq!((out.year, out.month, out.day), (2026, 7, 21));
    }

    #[test]
    fn string_converts_to_time() {
        let mut out = Time::default();
        let mut ind = 0isize;
        let _ = unsafe {
            write_column_value(
                &ColumnValue::String("10:30:15".into()),
                CDataType::TypeTime,
                std::ptr::from_mut(&mut out).cast(),
                size_of::<Time>() as isize,
                &mut ind,
            )
        }
        .expect("ISO time text should convert");
        assert_eq!((out.hour, out.minute, out.second), (10, 30, 15));
    }

    #[test]
    fn timestamp_with_t_separator_converts() {
        let mut out = Timestamp::default();
        let mut ind = 0isize;
        let _ = unsafe {
            write_column_value(
                &ColumnValue::String("2026-07-21T10:30:00".into()),
                CDataType::TypeTimestamp,
                std::ptr::from_mut(&mut out).cast(),
                size_of::<Timestamp>() as isize,
                &mut ind,
            )
        }
        .expect("T-separated timestamp should convert");
        assert_eq!((out.year, out.month, out.day), (2026, 7, 21));
        assert_eq!(out.hour, 10);
    }

    #[test]
    fn date_only_string_converts_to_timestamp_at_midnight() {
        let mut out = Timestamp::default();
        let mut ind = 0isize;
        let _ = unsafe {
            write_column_value(
                &ColumnValue::String("2026-07-21".into()),
                CDataType::TypeTimestamp,
                std::ptr::from_mut(&mut out).cast(),
                size_of::<Timestamp>() as isize,
                &mut ind,
            )
        }
        .expect("date-only text should convert to midnight");
        assert_eq!((out.year, out.month, out.day), (2026, 7, 21));
        assert_eq!(
            (out.hour, out.minute, out.second, out.fraction),
            (0, 0, 0, 0)
        );
    }

    #[test]
    fn malformed_timestamp_returns_22018() {
        let mut out = Timestamp::default();
        let mut ind = 0isize;
        let err = unsafe {
            write_column_value(
                &ColumnValue::String("not a timestamp".into()),
                CDataType::TypeTimestamp,
                std::ptr::from_mut(&mut out).cast(),
                size_of::<Timestamp>() as isize,
                &mut ind,
            )
        }
        .expect_err("malformed text must not convert");
        assert_eq!(
            err.sqlstate().as_str(),
            crate::types::sql_state::INVALID_CHARACTER_VALUE_FOR_CAST
        );
    }

    #[test]
    fn out_of_range_month_returns_22007() {
        let mut out = Timestamp::default();
        let mut ind = 0isize;
        let err = unsafe {
            write_column_value(
                &ColumnValue::String("2026-13-01 00:00:00".into()),
                CDataType::TypeTimestamp,
                std::ptr::from_mut(&mut out).cast(),
                size_of::<Timestamp>() as isize,
                &mut ind,
            )
        }
        .expect_err("month 13 must not convert");
        assert_eq!(
            err.sqlstate().as_str(),
            crate::types::sql_state::INVALID_DATETIME_FORMAT
        );
    }

    #[test]
    fn time_with_trailing_dot_and_no_fraction_digits_returns_22018() {
        // "15." splits to sec_text="15", frac_text=Some(""), which must not be
        // conflated with "no dot at all" (frac_text=None, meaning fraction 0).
        let mut out = Time::default();
        let mut ind = 0isize;
        let err = unsafe {
            write_column_value(
                &ColumnValue::String("10:30:15.".into()),
                CDataType::TypeTime,
                std::ptr::from_mut(&mut out).cast(),
                size_of::<Time>() as isize,
                &mut ind,
            )
        }
        .expect_err("trailing dot with no fraction digits must not convert");
        assert_eq!(
            err.sqlstate().as_str(),
            crate::types::sql_state::INVALID_CHARACTER_VALUE_FOR_CAST
        );
    }

    #[test]
    fn timestamp_with_trailing_dot_and_no_fraction_digits_returns_22018() {
        let mut out = Timestamp::default();
        let mut ind = 0isize;
        let err = unsafe {
            write_column_value(
                &ColumnValue::String("2026-07-21 10:30:15.".into()),
                CDataType::TypeTimestamp,
                std::ptr::from_mut(&mut out).cast(),
                size_of::<Timestamp>() as isize,
                &mut ind,
            )
        }
        .expect_err("trailing dot with no fraction digits must not convert");
        assert_eq!(
            err.sqlstate().as_str(),
            crate::types::sql_state::INVALID_CHARACTER_VALUE_FOR_CAST
        );
    }

    #[test]
    fn year_that_overflows_i16_returns_22007_not_22018() {
        // "99999" is syntactically all-digits but does not fit i16: a range
        // problem (22007), not a syntax problem (22018).
        let mut out = Date::default();
        let mut ind = 0isize;
        let err = unsafe {
            write_column_value(
                &ColumnValue::String("99999-01-01".into()),
                CDataType::TypeDate,
                std::ptr::from_mut(&mut out).cast(),
                size_of::<Date>() as isize,
                &mut ind,
            )
        }
        .expect_err("year 99999 must not convert");
        assert_eq!(
            err.sqlstate().as_str(),
            crate::types::sql_state::INVALID_DATETIME_FORMAT
        );
    }

    #[test]
    fn hour_that_overflows_u16_returns_22007_not_22018() {
        // "700000" is syntactically all-digits but does not fit u16: a range
        // problem (22007), not a syntax problem (22018).
        let mut out = Time::default();
        let mut ind = 0isize;
        let err = unsafe {
            write_column_value(
                &ColumnValue::String("700000:30:15".into()),
                CDataType::TypeTime,
                std::ptr::from_mut(&mut out).cast(),
                size_of::<Time>() as isize,
                &mut ind,
            )
        }
        .expect_err("hour 700000 must not convert");
        assert_eq!(
            err.sqlstate().as_str(),
            crate::types::sql_state::INVALID_DATETIME_FORMAT
        );
    }

    // -----------------------------------------------------------------------
    // SQL-to-C conversions for the temporal types.
    //
    // These walk the spec's SQL-to-C conversion table rather than the pairs a
    // particular backend happens to emit. Every earlier temporal test read its
    // value as SQL_C_CHAR or SQL_C_WCHAR, which take the string-coercion
    // catch-all, so the whole struct-target half of the table was untested and
    // four legal conversions were missing without anything noticing.
    //
    // Spec: https://learn.microsoft.com/en-us/sql/odbc/reference/appendixes/converting-data-from-sql-to-c-data-types
    // -----------------------------------------------------------------------

    fn a_date() -> ColumnValue {
        ColumnValue::Date {
            year: 2026,
            month: 7,
            day: 27,
        }
    }

    fn a_time(fraction: u32) -> ColumnValue {
        ColumnValue::Time {
            hour: 14,
            minute: 30,
            second: 45,
            fraction,
        }
    }

    fn a_timestamp(hour: u16, minute: u16, second: u16, fraction: u32) -> ColumnValue {
        ColumnValue::Timestamp {
            year: 2026,
            month: 7,
            day: 27,
            hour,
            minute,
            second,
            fraction,
        }
    }

    fn a_timestamp_tz(hour: u16, minute: u16, second: u16, fraction: u32) -> ColumnValue {
        ColumnValue::TimestampTz {
            year: 2026,
            month: 7,
            day: 27,
            hour,
            minute,
            second,
            fraction,
            timezone_offset_minutes: 120,
        }
    }

    /// Write `value` as `target_type` into a buffer big enough for any of the
    /// temporal C structs, and hand back the raw bytes with the outcome.
    unsafe fn convert(
        value: &ColumnValue,
        target_type: CDataType,
    ) -> (Result<SqlReturn, OdbcError>, [u8; 32], isize) {
        let mut buf = [0u8; 32];
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                value,
                target_type,
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as isize,
                &mut ind,
            )
        };
        (ret, buf, ind)
    }

    #[test]
    fn temporal_conversion_table_matches_the_spec() {
        // (SQL value, C target, legal?) for every temporal struct target the
        // spec's three tables define. The illegal pairs matter as much as the
        // legal ones: a driver that accepted them would silently invent data.
        let cases: &[(&str, ColumnValue, CDataType, bool)] = &[
            // SQL to C: Date — SQL_C_TYPE_DATE and SQL_C_TYPE_TIMESTAMP only.
            ("DATE -> DATE", a_date(), CDataType::TypeDate, true),
            (
                "DATE -> TIMESTAMP",
                a_date(),
                CDataType::TypeTimestamp,
                true,
            ),
            ("DATE -> TIME", a_date(), CDataType::TypeTime, false),
            // SQL to C: Time — SQL_C_TYPE_TIME and SQL_C_TYPE_TIMESTAMP only.
            ("TIME -> TIME", a_time(0), CDataType::TypeTime, true),
            (
                "TIME -> TIMESTAMP",
                a_time(0),
                CDataType::TypeTimestamp,
                true,
            ),
            ("TIME -> DATE", a_time(0), CDataType::TypeDate, false),
            // SQL to C: Timestamp — all three.
            (
                "TIMESTAMP -> TIMESTAMP",
                a_timestamp(1, 2, 3, 0),
                CDataType::TypeTimestamp,
                true,
            ),
            (
                "TIMESTAMP -> DATE",
                a_timestamp(0, 0, 0, 0),
                CDataType::TypeDate,
                true,
            ),
            (
                "TIMESTAMP -> TIME",
                a_timestamp(1, 2, 3, 0),
                CDataType::TypeTime,
                true,
            ),
            // TimestampTz is core's own variant, with no row in the spec's
            // table. It must still support the same three targets as
            // Timestamp: a zoned column that refused a plain date where an
            // unzoned one succeeded would be the same defect again.
            (
                "TIMESTAMPTZ -> TIMESTAMP",
                a_timestamp_tz(1, 2, 3, 0),
                CDataType::TypeTimestamp,
                true,
            ),
            (
                "TIMESTAMPTZ -> DATE",
                a_timestamp_tz(0, 0, 0, 0),
                CDataType::TypeDate,
                true,
            ),
            (
                "TIMESTAMPTZ -> TIME",
                a_timestamp_tz(1, 2, 3, 0),
                CDataType::TypeTime,
                true,
            ),
        ];

        for (name, value, target, legal) in cases {
            let (ret, _, _) = unsafe { convert(value, *target) };
            if *legal {
                assert!(
                    ret.is_ok(),
                    "{name} is legal per the spec but returned {:?}",
                    ret.as_ref()
                        .err()
                        .map(|e| e.sqlstate().as_str().to_string())
                );
            } else {
                let err = ret.expect_err(&format!("{name} is not in the spec's table"));
                assert_eq!(
                    err.sqlstate().as_str(),
                    crate::types::sql_state::RESTRICTED_DATA_TYPE_ATTRIBUTE_VIOLATION,
                    "{name} must be refused with 07006"
                );
            }
        }
    }

    #[test]
    fn date_to_timestamp_zeroes_the_time_fields() {
        // Spec: "The driver sets the time fields of the timestamp structure to
        // zero." No SQLSTATE — nothing is lost.
        let (ret, buf, ind) = unsafe { convert(&a_date(), CDataType::TypeTimestamp) };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(ind, std::mem::size_of::<Timestamp>() as isize);

        let ts = unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const Timestamp) };
        assert_eq!(
            ts,
            Timestamp {
                year: 2026,
                month: 7,
                day: 27,
                hour: 0,
                minute: 0,
                second: 0,
                fraction: 0,
            }
        );
    }

    #[test]
    fn time_to_timestamp_uses_todays_date_and_zeroes_the_fraction() {
        // Spec: "The date fields of the timestamp structure are set to the
        // current date, and the fractional seconds field of the timestamp
        // structure is set to zero."
        //
        // A non-zero fraction on the way in must not produce 01S07: the spec
        // lists no SQLSTATE for this row, so zeroing it is part of the defined
        // conversion rather than a truncation.
        let (ret, buf, ind) = unsafe { convert(&a_time(123_456_789), CDataType::TypeTimestamp) };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(ind, std::mem::size_of::<Timestamp>() as isize);

        let ts = unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const Timestamp) };
        assert_eq!((ts.hour, ts.minute, ts.second), (14, 30, 45));
        assert_eq!(ts.fraction, 0, "the fraction must be zeroed");

        let (year, month, day) = current_utc_date();
        assert_eq!((ts.year, ts.month, ts.day), (year, month, day));
    }

    #[test]
    fn timestamp_to_date_reports_01s07_only_when_a_time_is_dropped() {
        // Spec splits this row on the time portion: zero is n/a, non-zero is
        // 01S07 with "The time portion of the timestamp is truncated."
        let (ret, buf, ind) = unsafe { convert(&a_timestamp(0, 0, 0, 0), CDataType::TypeDate) };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(ind, std::mem::size_of::<Date>() as isize);
        let d = unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const Date) };
        assert_eq!(
            d,
            Date {
                year: 2026,
                month: 7,
                day: 27
            }
        );

        // A dropped time, however small, is a truncation.
        for value in [
            a_timestamp(1, 0, 0, 0),
            a_timestamp(0, 1, 0, 0),
            a_timestamp(0, 0, 1, 0),
            a_timestamp(0, 0, 0, 1),
        ] {
            let (ret, _, _) = unsafe { convert(&value, CDataType::TypeDate) };
            let err = ret.expect_err("a dropped time portion must be reported");
            assert_eq!(
                err.sqlstate().as_str(),
                crate::types::sql_state::FRACTIONAL_TRUNCATION
            );
        }
    }

    #[test]
    fn timestamp_to_time_ignores_the_date_but_reports_a_dropped_fraction() {
        // Spec: "The date portion of the timestamp is ignored", and the row
        // splits on the fractional seconds alone — so a discarded date is not a
        // truncation, but a discarded fraction is.
        let (ret, buf, ind) = unsafe { convert(&a_timestamp(14, 30, 45, 0), CDataType::TypeTime) };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(ind, std::mem::size_of::<Time>() as isize);
        let t = unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const Time) };
        assert_eq!(
            t,
            Time {
                hour: 14,
                minute: 30,
                second: 45
            }
        );

        let (ret, _, _) = unsafe { convert(&a_timestamp(14, 30, 45, 1), CDataType::TypeTime) };
        let err = ret.expect_err("a dropped fraction must be reported");
        assert_eq!(
            err.sqlstate().as_str(),
            crate::types::sql_state::FRACTIONAL_TRUNCATION
        );
    }

    #[test]
    fn civil_from_days_matches_known_dates() {
        // Pins the calendar arithmetic without a clock in the way: epoch,
        // both sides of a leap day, a century non-leap year, and a date before
        // the epoch to exercise the negative-era branch.
        let cases = [
            (0_i64, (1970_i64, 1_u16, 1_u16)),
            (-1, (1969, 12, 31)),
            (59, (1970, 3, 1)),
            (365, (1971, 1, 1)),
            // 2000 was a leap year (divisible by 400); 1900 was not.
            (11_016, (2000, 2, 29)),
            (11_017, (2000, 3, 1)),
            (-25_508, (1900, 3, 1)),
            (20_661, (2026, 7, 27)),
        ];
        for (days, expected) in cases {
            assert_eq!(civil_from_days(days), expected, "days = {days}");
        }
    }

    #[test]
    fn civil_from_days_handles_a_pre_epoch_clock() {
        // `current_utc_date` cannot be pointed at a fake clock without a clock
        // abstraction, but the branch that was wrong is the arithmetic, not the
        // read: a machine whose clock is set before 1970 used to be given
        // 1970-01-01 because the error carrying the distance backwards was
        // discarded. These are the day counts that branch produces.
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(civil_from_days(-365), (1969, 1, 1));
        assert_eq!(civil_from_days(-719_468), (0, 3, 1));
        // A negative second count must floor, not truncate toward zero: any
        // moment on 1969-12-31 is that date, not 1970-01-01.
        assert_eq!((-1_i64).div_euclid(86_400), -1);
        assert_eq!((-86_400_i64).div_euclid(86_400), -1);
        assert_eq!((-86_401_i64).div_euclid(86_400), -2);
    }

    #[test]
    fn current_utc_date_is_plausible() {
        // The clock itself cannot be asserted, but a wrong epoch or a broken
        // era branch shows up as an absurd year or an out-of-range field.
        let (year, month, day) = current_utc_date();
        assert!((2020..=2200).contains(&year), "implausible year {year}");
        assert!((1..=12).contains(&month), "month out of range: {month}");
        assert!((1..=31).contains(&day), "day out of range: {day}");
    }
}
