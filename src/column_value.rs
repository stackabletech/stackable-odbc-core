//! `write_column_value` marshals a [`crate::types::ColumnValue`] into an
//! application buffer for `SQLGetData` (NULL, truncation, type coercion).

use std::ffi::c_void;

use odbc_sys::{Date, NULL_DATA, Time, Timestamp};

use crate::errors::OdbcError;
use crate::param_convert::{DecimalLiteral, parse_numeric_literal};
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
    unsafe { write_column_value_at(value, target_type, target_ptr, buf_len, len_ind_ptr, 0) }
        .map(|w| w.ret)
}

/// What one marshalling call delivered, for `SQLGetData`'s chunking loop.
///
/// Only `SQLGetData` needs this; the bound-column and `SQLParamData` paths call
/// [`write_column_value`], which discards it.
///
/// The fields are crate-private and the type is `#[non_exhaustive]`, so a field
/// added here is a source-compatible change for every driver. Read a field back
/// through its accessor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ChunkWrite {
    pub(crate) ret: SqlReturn,
    pub(crate) delivered: usize,
    pub(crate) chunkable: bool,
}

/// Field accessors for [`ChunkWrite`].
///
/// The fields themselves are crate-private: this type is `#[non_exhaustive]`,
/// and public fields would have made that advisory. Reading goes through these
/// instead.
impl ChunkWrite {
    /// The value to return from the FFI function.
    ///
    /// No `#[must_use]`: [`SqlReturn`] already carries one, and clippy's
    /// `double_must_use` rejects the pair.
    pub fn ret(&self) -> SqlReturn {
        self.ret
    }

    /// Units delivered by this call — UTF-16 code units for `SQL_C_WCHAR`,
    /// bytes for `SQL_C_CHAR` and `SQL_C_BINARY`, `0` for a fixed-width target.
    /// The caller adds this to its running offset.
    #[must_use]
    pub fn delivered(&self) -> usize {
        self.delivered
    }

    /// Whether this target type can be read in parts at all.
    ///
    /// `false` for every fixed-width target, which the spec forbids chunking:
    /// "SQLGetData cannot be used to return fixed-length data in parts. If
    /// SQLGetData is called more than one time in a row for a column containing
    /// fixed-length data, it returns SQL_NO_DATA for all calls after the first."
    #[must_use]
    pub fn chunkable(&self) -> bool {
        self.chunkable
    }
}

/// [`write_column_value`], resuming `offset` units into the value.
///
/// Character and binary targets are the only ones that can be read in parts, and
/// all three of them funnel through `write_wchar` / `write_char` /
/// `write_binary`, so the chunking is handled in one place here rather than
/// spread across the fixed-width arms below — none of which can chunk.
///
/// # Safety
/// Same contract as [`write_column_value`].
pub unsafe fn write_column_value_at(
    value: &ColumnValue,
    target_type: CDataType,
    target_ptr: *mut c_void,
    buf_len: isize,
    len_ind_ptr: *mut isize,
    offset: usize,
) -> Result<ChunkWrite, OdbcError> {
    unsafe { write_fixed_or_chunked(value, target_type, target_ptr, buf_len, len_ind_ptr, offset) }
}

/// A non-chunkable outcome: the whole value in one call.
fn whole(ret: SqlReturn) -> ChunkWrite {
    ChunkWrite {
        ret,
        delivered: 0,
        chunkable: false,
    }
}

unsafe fn write_fixed_or_chunked(
    value: &ColumnValue,
    target_type: CDataType,
    target_ptr: *mut c_void,
    buf_len: isize,
    len_ind_ptr: *mut isize,
    offset: usize,
) -> Result<ChunkWrite, OdbcError> {
    // NULL handling
    if matches!(value, ColumnValue::Null) {
        if !len_ind_ptr.is_null() {
            unsafe { std::ptr::write_unaligned(len_ind_ptr, NULL_DATA) };
        }
        return Ok(whole(SqlReturn::SUCCESS));
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

        return unsafe {
            write_fixed_or_chunked(value, inferred, target_ptr, buf_len, len_ind_ptr, offset)
        };
    }

    // The three chunkable targets are handled here, ahead of the coercion match,
    // because every character and binary conversion below funnelled into these
    // same three writers anyway — the arms differed only in how they produced
    // the string or byte form. Resuming at `offset` therefore belongs in one
    // place rather than in each arm, and no fixed-width arm can chunk at all.
    //
    // For `ColumnValue::String` the string form *is* the value, which is why
    // this borrows instead of going through `column_value_to_string` (that
    // returns `s.clone()` for the `String` variant, so the two agree).
    match target_type {
        CDataType::WChar | CDataType::Char => {
            let owned;
            let s: &str = match value {
                ColumnValue::String(s) => s,
                _ => {
                    owned = column_value_to_string(value);
                    &owned
                }
            };
            let (ret, delivered) = unsafe {
                if target_type == CDataType::WChar {
                    write_wchar(s, target_ptr, buf_len, len_ind_ptr, offset)?
                } else {
                    write_char(s, target_ptr, buf_len, len_ind_ptr, offset)?
                }
            };
            return Ok(ChunkWrite {
                ret,
                delivered,
                chunkable: true,
            });
        }
        CDataType::Binary => {
            let bytes = column_value_to_binary(value);
            let (ret, delivered) =
                unsafe { write_binary(&bytes, target_ptr, buf_len, len_ind_ptr, offset)? };
            return Ok(ChunkWrite {
                ret,
                delivered,
                chunkable: true,
            });
        }
        _ => {}
    }

    // Type coercion: if the value doesn't match the requested type, convert
    // through a string representation for string targets.
    //
    // SAFETY: All unsafe helper calls below operate on the same raw pointers
    // passed by the caller, whose validity is guaranteed by the function's
    // safety contract.
    let fixed = match (value, target_type) {
        // --- String to datetime C types ---
        // Required by the ODBC conversion matrix: SQL_CHAR / SQL_VARCHAR
        // convert to every C type. Backends whose data source has no native
        // date type deliver datetimes as character data.
        //
        // Each of the three C structs accepts more than its own literal form,
        // and the SQL to C: Character rows say which and at what cost. The
        // cascades below are the same ones `param_convert`'s `to_date`,
        // `to_time` and `to_timestamp` run in the C-to-SQL direction, over the
        // same two parsers; only the SQLSTATE conventions differ, which is why
        // they are not one function.

        // "Data value is a valid date-value" / "a valid timestamp-value; time
        // portion is zero" — data written, no SQLSTATE. "a valid
        // timestamp-value; time portion is nonzero" — truncated data written
        // with 01S07, footnote [c]: "The time portion of the timestamp-value is
        // truncated." Anything else is the row's 22018 with nothing written —
        // or this module's 22007, which `parse_sql_timestamp`'s `?` propagates
        // for a literal it recognises whose field is out of range.
        //
        // One branch covers all three: `parse_sql_timestamp` reads a bare
        // `yyyy-mm-dd` as that date at midnight, so a date-value arrives here
        // with a zero time portion and takes the clean path by construction.
        (ColumnValue::String(s), CDataType::TypeDate) => {
            let ts = parse_sql_timestamp(s)?;
            let d = Date {
                year: ts.year,
                month: ts.month,
                day: ts.day,
            };
            unsafe {
                let _ = write_fixed(target_ptr, len_ind_ptr, d)?;
            }
            // "Nonzero" is any of the four time fields, the fraction included.
            if (ts.hour, ts.minute, ts.second, ts.fraction) != (0, 0, 0, 0) {
                return Err(OdbcError::FractionalTruncation);
            }
            Ok(SqlReturn::SUCCESS)
        }

        // "Data value is a valid time-value and the fractional seconds value is
        // 0" / "a valid timestamp-value or a valid time-value; fractional
        // seconds portion is zero" — data written, no SQLSTATE, footnote [d]:
        // "The date portion of the timestamp-value is ignored", so a discarded
        // date is not a truncation and reports nothing. "a valid
        // timestamp-value; fractional seconds portion is nonzero" — truncated
        // data written with 01S07, footnote [e]. Anything else is 22018.
        (ColumnValue::String(s), CDataType::TypeTime) => {
            let (t, fraction) = match parse_sql_time(s) {
                Ok(parsed) => parsed,
                // Load-bearing, not an optimisation: falling through on *every*
                // error would hand "25:00:00" to `parse_sql_timestamp`, which
                // does not recognise it as a datetime at all and answers 22018,
                // discarding the 22007 this module gives a recognised literal
                // with an out-of-range field. Deleting it fails
                // `hour_that_overflows_u16_returns_22007_not_22018`, and only
                // that test — the newer
                // `timestamp_text_with_out_of_range_minute_to_time_stays_22007`
                // takes the fall-through and gets its 22007 from the timestamp
                // parser, so it stays green either way.
                Err(e) if !is_wrong_literal_shape(&e) => return Err(e),
                Err(_) => {
                    let ts = parse_sql_timestamp(s)?;
                    (
                        Time {
                            hour: ts.hour,
                            minute: ts.minute,
                            second: ts.second,
                        },
                        ts.fraction,
                    )
                }
            };
            unsafe {
                let _ = write_fixed(target_ptr, len_ind_ptr, t)?;
            }
            if fraction != 0 {
                return Err(OdbcError::FractionalTruncation);
            }
            Ok(SqlReturn::SUCCESS)
        }

        // "Data value is a valid timestamp-value or a valid time-value;
        // fractional seconds portion not truncated" / "a valid date-value",
        // footnote [f]: "The time fields of the timestamp structure are set to
        // zero" — both are what `parse_sql_timestamp` already produces. "a
        // valid time-value", footnote [g]: "The date fields of the timestamp
        // structure are set to the current date" — the branch below. Anything
        // else is 22018 with nothing written.
        //
        // Footnote [g] speaks only of the date fields, and the row above it
        // makes a time-value's fractional seconds something that can be
        // "truncated" — so the literal's fraction is carried into the target's
        // own fraction field rather than zeroed. That is the opposite of the
        // typed `ColumnValue::Time` arm below, whose row (SQL to C: Time) says
        // in as many words that the fraction is set to zero. Two source types,
        // two tables, two answers.
        //
        // Known limitation, recorded rather than fixed: the "fractional seconds
        // portion truncated" row's 01S07 is not reported. `parse_time_fields`
        // truncates a literal carrying more than nine fractional digits to
        // nanoseconds silently, on this path and on the timestamp-value path
        // alike, so the two cannot disagree and the data still arrives — only
        // the warning is missing. Ruled 2026-08-01 and listed under "Known
        // limitations" in docs/superpowers/plans/2026-07-31-audit-remediation.md.
        // Not an open intention: changing it needs that ruling revisited first.
        (ColumnValue::String(s), CDataType::TypeTimestamp) => {
            let ts = match parse_sql_timestamp(s) {
                Ok(ts) => ts,
                // A local invariant, *not* load-bearing, unlike its counterpart
                // in the arm above: no known input both fails
                // `parse_sql_timestamp` with 22007 and parses as a time-value,
                // and where neither form parses the terminal arm below already
                // returns this same `e`. It states "only a 'this is not a
                // timestamp at all' failure may try the time-value form" so a
                // later change to either parser cannot quietly break it.
                Err(e) if !is_wrong_literal_shape(&e) => return Err(e),
                Err(e) => match parse_sql_time(s) {
                    Ok((t, fraction)) => {
                        let (year, month, day) = current_utc_date();
                        Timestamp {
                            year,
                            month,
                            day,
                            hour: t.hour,
                            minute: t.minute,
                            second: t.second,
                            fraction,
                        }
                    }
                    // Neither form parsed. A 22007 from the time parser names a
                    // real defect in the text and is kept over the timestamp
                    // parser's blanket 22018.
                    Err(time_err) if !is_wrong_literal_shape(&time_err) => return Err(time_err),
                    Err(_) => return Err(e),
                },
            };
            unsafe { write_fixed(target_ptr, len_ind_ptr, ts) }
        }

        // --- Numeric coercion: any numeric source → any numeric C target ---
        // ODBC requires drivers to support conversions between compatible numeric C types.
        // Applications (e.g. LibreOffice Base) routinely request SQL_C_SLONG for columns
        // that happen to hold i16 values, so all cross-type numeric casts must work.
        //
        // The pivot (column_value_as_numeric) maps every ColumnValue to Int(i64), Float(f64)
        // or, for text bound for an integer target, exact decimal digits — none of which
        // loses precision on the way in. write_numeric_pivot then narrows to the requested
        // C type at the last possible moment.
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
        ) => match column_value_as_numeric(value, target_type) {
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

        // The `Binary`, `WChar` and `Char` catch-alls that used to close this
        // match now sit ahead of it, where the chunking offset is applied; every
        // target reaching here is fixed-width.

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
    };

    fixed.map(whole)
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
pub(crate) fn current_utc_date() -> (i16, u16, u16) {
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
// in this module — stackable-odbc-core has no numeric datetime encodings left to
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

/// Is `year` a leap year in the proleptic Gregorian calendar?
///
/// Divisible by 4, except centuries, except every fourth century — so 2000 is a
/// leap year and 1900 is not. The same calendar [`civil_from_days`] implements,
/// which is what keeps this module's two date computations from disagreeing —
/// and `days_in_month_agrees_with_civil_from_days` checks that rather than
/// leaving it to this sentence, by walking every day of nine chosen years
/// through both.
///
/// `%` is correct for a negative year here because every arm compares against
/// zero, and `-100 % 100` is 0 in Rust as it is in mathematics. No negative year
/// reaches this function today — [`parse_date_fields`] splits on `-`, so a
/// leading minus sign produces a fourth part and is refused as malformed — but
/// the rule is written to be right either way rather than to depend on that.
fn is_leap_year(year: i32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

/// How many days `month` (1-12) has in `year`.
///
/// February is 29 in a leap year and 28 otherwise; April, June, September and
/// November have 30; January, March, May, July, August, October and December
/// have 31. Callers must have validated `month` first: an out-of-range month
/// answers 31, the widest length, so a bad month is refused by the month check
/// and never by this one reporting an unrelated day error.
fn days_in_month(year: i32, month: u16) -> u16 {
    match month {
        2 => {
            if is_leap_year(year) {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        _ => 31,
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
    if !(1..=12).contains(&month) {
        return Err(invalid_datetime_format(s));
    }
    // "Data value is not a valid date-value or timestamp-value" — SQL to C:
    // Character. ODBC's grammar says only `days-value ::= digit digit`, so what
    // makes a day valid is the calendar, not the syntax: 2024-02-30 is
    // well-formed and does not exist. Covered by `feb_30_is_rejected` and its
    // neighbours.
    if !(1..=days_in_month(i32::from(year), month)).contains(&day) {
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

/// Does this parse failure mean "the text is not this literal form at all"?
///
/// The two literal parsers answer with one of two SQLSTATEs, and the difference
/// decides whether a caller may try the other form: 22018 says the text does not
/// have this shape, so the other parser deserves a look; 22007 says the shape
/// was recognised and a field is out of range, which the other parser will not
/// improve on and will usually report as the weaker 22018.
fn is_wrong_literal_shape(e: &OdbcError) -> bool {
    e.sqlstate() == SqlState::invalid_character_value_for_cast()
}

/// Parse ODBC time literal text into a [`Time`] struct plus the fractional
/// seconds (nanoseconds) that `SQL_TIME_STRUCT` cannot carry. Callers writing
/// to `SQL_C_TYPE_TIME` must check the returned fraction themselves and report
/// 01S07 if it is non-zero — this function only parses, it does not decide
/// whether the drop is acceptable for the caller's target type.
pub(crate) fn parse_sql_time(s: &str) -> Result<(Time, u32), OdbcError> {
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

pub(crate) fn parse_sql_timestamp(s: &str) -> Result<Timestamp, OdbcError> {
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

/// Writes `s` as UTF-16, starting `offset` code units in.
///
/// `offset` is how many UTF-16 code units earlier `SQLGetData` calls already
/// delivered for this column; `0` is the whole value. Returns the code units
/// written this call, which is what the caller adds to its running offset.
///
/// The length written to `len_ind_ptr` is the length *remaining at the start of
/// this call*, not the length of the whole value, per the spec's step 7: "When
/// `SQLGetData` is called multiple times in succession for the same column,
/// this is the length of the data available at the start of the current call;
/// that is, the length decreases with each subsequent call."
unsafe fn write_wchar(
    s: &str,
    target_ptr: *mut c_void,
    buf_len: isize,
    len_ind_ptr: *mut isize,
    offset: usize,
) -> Result<(SqlReturn, usize), OdbcError> {
    // Pre-size to UTF-8 byte length, which is always >= UTF-16 code unit count.
    let mut wide = Vec::with_capacity(s.len());
    wide.extend(s.encode_utf16());
    // An offset past the end yields an empty remainder rather than panicking:
    // the caller stops at `done`, but a truncating write that lands exactly on
    // the end would otherwise index one past it.
    let remaining = wide.get(offset.min(wide.len())..).unwrap_or(&[]);
    let total_bytes = (remaining.len() * 2) as isize;

    // Always report the byte length still to come.
    if !len_ind_ptr.is_null() {
        unsafe { std::ptr::write_unaligned(len_ind_ptr, total_bytes) };
    }

    // A null target is not something SQLGetData's own spec sanctions — its
    // Arguments section is explicit that "TargetValuePtr cannot be NULL."
    // The case that actually reaches this branch comes from this function's
    // *other* caller: `sql_fetch`'s bound-column loop (`ffi/fetch.rs`)
    // legitimately passes a null data pointer when `SQL_DESC_DATA_PTR` is
    // null but `SQL_DESC_INDICATOR_PTR` is not — the indicator-only binding
    // the spec allows ("An application can unbind the data buffer for a
    // column but still have a length/indicator buffer bound for the
    // column"), which `collect_bindings` deliberately keeps and
    // `fetch_writes_the_indicator_of_an_indicator_only_binding` pins. That
    // caller still wants the length written to `len_ind_ptr` above, with
    // nothing written to a buffer that does not exist, so this returns
    // SUCCESS rather than treating the null pointer as an error this deep in
    // the call stack; `SQLGetData`'s own `target_value_ptr` null case is
    // `HY009`, deliberately left unchecked at the FFI boundary (see
    // `sql_get_data`'s doc comment) rather than enforced here.
    if target_ptr.is_null() {
        return Ok((SqlReturn::SUCCESS, 0));
    }

    // A non-null target with fewer than two bytes of room — including
    // exactly zero, the standard "how big a buffer do I need" probe — cannot
    // hold even the one-UTF-16-code-unit null terminator. That is total
    // truncation, not a length query: the application supplied somewhere to
    // write and nothing was written there. Spec: "If the data buffer
    // supplied is too small to hold the null-termination character,
    // SQLGetData returns SQL_SUCCESS_WITH_INFO and SQLSTATE 01004." Reporting
    // plain SUCCESS here (as a shared branch with the null-target case above
    // used to) made SQLGetData indistinguishable from "this column is fully
    // delivered," which permanently stranded the data behind a `buf_len == 0`
    // probe: `cursor.done` is derived from this return value.
    if buf_len < 2 {
        return Ok((SqlReturn::SUCCESS_WITH_INFO, 0));
    }

    let out_ptr = target_ptr.cast::<u16>();
    // buf_len is in bytes; capacity in u16 code units (reserve one for null terminator)
    let capacity_units = ((buf_len as usize) / 2).saturating_sub(1);
    let copy_count = remaining.len().min(capacity_units);

    unsafe {
        let out_bytes = out_ptr.cast::<u8>();
        std::ptr::copy_nonoverlapping(remaining.as_ptr().cast::<u8>(), out_bytes, copy_count * 2);
        // null terminator
        std::ptr::write_unaligned(out_bytes.add(copy_count * 2).cast::<u16>(), 0u16);
    }

    if copy_count < remaining.len() {
        Ok((SqlReturn::SUCCESS_WITH_INFO, copy_count))
    } else {
        Ok((SqlReturn::SUCCESS, copy_count))
    }
}

// ---------------------------------------------------------------------------
// Helper: write UTF-8 string
// ---------------------------------------------------------------------------

/// Writes `s` as UTF-8, starting `offset` bytes in. See [`write_wchar`] for what
/// `offset`, the return value and the indicator length mean.
///
/// The offset is a byte count, so a chunk boundary can fall inside a multi-byte
/// UTF-8 sequence and split it. That is the same split `SQL_C_CHAR` truncation
/// already performs at the buffer edge, and it is what the ODBC contract asks
/// for: the application is reassembling a byte stream and is told to concatenate
/// the parts, so the sequence is whole again once it does. Deliberately not
/// "fixed" by backing up to a character boundary — that would deliver fewer
/// bytes than the buffer holds and, on a buffer smaller than one character,
/// would deliver nothing and never terminate.
unsafe fn write_char(
    s: &str,
    target_ptr: *mut c_void,
    buf_len: isize,
    len_ind_ptr: *mut isize,
    offset: usize,
) -> Result<(SqlReturn, usize), OdbcError> {
    let all = s.as_bytes();
    let bytes = all.get(offset.min(all.len())..).unwrap_or(&[]);
    let total_bytes = bytes.len() as isize;

    if !len_ind_ptr.is_null() {
        unsafe { std::ptr::write_unaligned(len_ind_ptr, total_bytes) };
    }

    // A null target here is the bound-column caller's indicator-only
    // binding, not something SQLGetData's own spec permits — see
    // `write_wchar`'s full reasoning.
    if target_ptr.is_null() {
        return Ok((SqlReturn::SUCCESS, 0));
    }

    // A non-null target with no room in it — including exactly zero, the
    // standard length-probe — cannot hold even the one-byte null terminator,
    // which is total truncation (SUCCESS_WITH_INFO / 01004), not a length
    // query. See `write_wchar`'s identical split.
    if buf_len <= 0 {
        return Ok((SqlReturn::SUCCESS_WITH_INFO, 0));
    }

    let out_ptr = target_ptr.cast::<u8>();
    let capacity = (buf_len as usize).saturating_sub(1); // reserve one for null terminator
    let copy_count = bytes.len().min(capacity);

    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), out_ptr, copy_count);
        *out_ptr.add(copy_count) = 0u8; // null terminator
    }

    if copy_count < bytes.len() {
        Ok((SqlReturn::SUCCESS_WITH_INFO, copy_count))
    } else {
        Ok((SqlReturn::SUCCESS, copy_count))
    }
}

// ---------------------------------------------------------------------------
// Helper: write raw binary bytes
// ---------------------------------------------------------------------------

/// Writes raw bytes starting `offset` bytes in. See [`write_wchar`] for what
/// `offset`, the return value and the indicator length mean.
///
/// Unlike the two character writers this reserves no terminator, per the spec's
/// step 5: "If the length of binary data exceeds the length of the data buffer,
/// `SQLGetData` truncates it to `BufferLength` bytes."
unsafe fn write_binary(
    data: &[u8],
    target_ptr: *mut c_void,
    buf_len: isize,
    len_ind_ptr: *mut isize,
    offset: usize,
) -> Result<(SqlReturn, usize), OdbcError> {
    let data = data.get(offset.min(data.len())..).unwrap_or(&[]);
    let total_bytes = data.len() as isize;

    if !len_ind_ptr.is_null() {
        unsafe { std::ptr::write_unaligned(len_ind_ptr, total_bytes) };
    }

    // A null target here is the bound-column caller's indicator-only
    // binding, not something SQLGetData's own spec permits — see
    // `write_wchar`'s full reasoning.
    if target_ptr.is_null() {
        return Ok((SqlReturn::SUCCESS, 0));
    }

    // A non-null target with no room in it — including exactly zero, the
    // standard length-probe — cannot hold any of the data, which is total
    // truncation (SUCCESS_WITH_INFO / 01004), not a length query. Binary
    // reserves no terminator, so unlike the two character writers there is no
    // extra "room for one more byte" boundary; `buf_len <= 0` is the whole
    // condition. See `write_wchar`'s identical split.
    if buf_len <= 0 {
        return Ok((SqlReturn::SUCCESS_WITH_INFO, 0));
    }

    let out_ptr = target_ptr.cast::<u8>();
    let copy_count = data.len().min(buf_len as usize);

    unsafe {
        std::ptr::copy_nonoverlapping(data.as_ptr(), out_ptr, copy_count);
    }

    if copy_count < data.len() {
        Ok((SqlReturn::SUCCESS_WITH_INFO, copy_count))
    } else {
        Ok((SqlReturn::SUCCESS, copy_count))
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
enum NumericPivot<'a> {
    Int(i64),
    Float(f64),
    /// `ColumnValue::String` or `ColumnValue::Decimal` text on its way to one of
    /// the eight integer C targets, carried as exact digits rather than as an
    /// `f64`. `text` is the trimmed source, used only in the `22003` diagnostic.
    Exact {
        literal: DecimalLiteral,
        text: &'a str,
    },
}

/// The C targets whose value is an exact integer, so that text reaching one of
/// them must not pass through `f64`.
///
/// `SQL_C_BIT` is deliberately absent: it has its own row in both of the tables
/// [`write_exact_integer`] cites, and its three-way rule turns on a fraction
/// rather than discarding one. It is implemented on the `Float` arm below.
const fn is_exact_integer_target(target_type: CDataType) -> bool {
    matches!(
        target_type,
        CDataType::STinyInt
            | CDataType::SShort
            | CDataType::SLong
            | CDataType::SBigInt
            | CDataType::UTinyInt
            | CDataType::UShort
            | CDataType::ULong
            | CDataType::UBigInt
    )
}

/// Parse numeric text into a [`NumericPivot`] for a given C target.
///
/// The target decides the representation, because the exact-numeric row of the
/// two tables [`write_exact_integer`] cites and their float row want different
/// things. An integer target gets a [`DecimalLiteral`], whose digits survive
/// both `f64`'s 53-bit mantissa and the rounding an `f64` would apply where the
/// row says "truncation of fractional digits". A float target keeps the `f64`
/// path, since `f64` is where the value is going anyway.
///
/// The `i64`-then-`f64` fallback still runs for the float targets, and for text
/// that is not a *numeric-literal* at all — `inf` and `NaN`, which
/// [`parse_numeric_literal`] rejects and Rust's float parser accepts.
fn parse_numeric_text(s: &str, target_type: CDataType) -> Option<NumericPivot<'_>> {
    let t = s.trim();
    if is_exact_integer_target(target_type)
        && let Some(literal) = parse_numeric_literal(t)
    {
        return Some(NumericPivot::Exact { literal, text: t });
    }
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
fn column_value_as_numeric(
    value: &ColumnValue,
    target_type: CDataType,
) -> Option<NumericPivot<'_>> {
    match value {
        ColumnValue::I8(v) => Some(NumericPivot::Int(i64::from(*v))),
        ColumnValue::I16(v) => Some(NumericPivot::Int(i64::from(*v))),
        ColumnValue::I32(v) => Some(NumericPivot::Int(i64::from(*v))),
        ColumnValue::I64(v) => Some(NumericPivot::Int(*v)),
        ColumnValue::F32(v) => Some(NumericPivot::Float(f64::from(*v))),
        ColumnValue::F64(v) => Some(NumericPivot::Float(*v)),
        ColumnValue::Bool(v) => Some(NumericPivot::Int(*v as i64)),
        ColumnValue::Decimal(s) | ColumnValue::String(s) => parse_numeric_text(s, target_type),
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
/// with SQLSTATE `01S07` (fractional truncation) in the four cases that drop
/// something: an `i64` or an `f64` narrowed to `f32` with precision loss, a
/// fraction between 0 and 2 losing its fractional part to reach `SQL_C_BIT`, an
/// exact decimal losing a non-zero fraction to reach an integer target (see
/// [`write_exact_integer`]), and an `f64` losing a non-zero fraction to reach
/// an integer target (see [`write_truncated_float`]). Any `CDataType` not
/// covered by the numeric arms returns `SQL_ERROR` with SQLSTATE `HY003`
/// (invalid application buffer type).
unsafe fn write_numeric_pivot(
    pivot: NumericPivot<'_>,
    target_type: CDataType,
    target_ptr: *mut c_void,
    len_ind_ptr: *mut isize,
) -> Result<SqlReturn, OdbcError> {
    match (pivot, target_type) {
        // --- Exact pivot → integer targets ---
        // `parse_numeric_text` builds this variant only for the eight targets
        // `is_exact_integer_target` names, so the arms below cover every
        // combination it can produce.
        (NumericPivot::Exact { literal, text }, CDataType::STinyInt) => unsafe {
            write_exact_integer::<i8>(&literal, text, target_ptr, len_ind_ptr)
        },
        (NumericPivot::Exact { literal, text }, CDataType::SShort) => unsafe {
            write_exact_integer::<i16>(&literal, text, target_ptr, len_ind_ptr)
        },
        (NumericPivot::Exact { literal, text }, CDataType::SLong) => unsafe {
            write_exact_integer::<i32>(&literal, text, target_ptr, len_ind_ptr)
        },
        (NumericPivot::Exact { literal, text }, CDataType::SBigInt) => unsafe {
            write_exact_integer::<i64>(&literal, text, target_ptr, len_ind_ptr)
        },
        (NumericPivot::Exact { literal, text }, CDataType::UTinyInt) => unsafe {
            write_exact_integer::<u8>(&literal, text, target_ptr, len_ind_ptr)
        },
        (NumericPivot::Exact { literal, text }, CDataType::UShort) => unsafe {
            write_exact_integer::<u16>(&literal, text, target_ptr, len_ind_ptr)
        },
        (NumericPivot::Exact { literal, text }, CDataType::ULong) => unsafe {
            write_exact_integer::<u32>(&literal, text, target_ptr, len_ind_ptr)
        },
        (NumericPivot::Exact { literal, text }, CDataType::UBigInt) => unsafe {
            write_exact_integer::<u64>(&literal, text, target_ptr, len_ind_ptr)
        },
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
        // --- Int pivot → Bit ---
        // "Data is less than 0 or greater than or equal to 2" is 22003; the
        // table's 01S07 row ("greater than 0, less than 2, and not equal to 1")
        // needs a fractional part, which an integer pivot cannot carry, so what
        // survives the range test here is exactly the "Data is 0 or 1" row.
        (NumericPivot::Int(v), CDataType::Bit) => {
            if !(0..2).contains(&v) {
                return Err(OdbcError::general(
                    format!("Numeric value out of range: {v} is not 0 or 1"),
                    SqlState::numeric_value_out_of_range(),
                ));
            }
            unsafe { write_fixed(target_ptr, len_ind_ptr, u8::from(v == 1)) }
        }
        // --- Float pivot → signed integer targets ---
        (NumericPivot::Float(v), CDataType::STinyInt) => unsafe {
            write_truncated_float::<i8>(v, target_ptr, len_ind_ptr)
        },
        (NumericPivot::Float(v), CDataType::SShort) => unsafe {
            write_truncated_float::<i16>(v, target_ptr, len_ind_ptr)
        },
        (NumericPivot::Float(v), CDataType::SLong) => unsafe {
            write_truncated_float::<i32>(v, target_ptr, len_ind_ptr)
        },
        (NumericPivot::Float(v), CDataType::SBigInt) => unsafe {
            write_truncated_float::<i64>(v, target_ptr, len_ind_ptr)
        },
        // --- Float pivot → unsigned integer targets ---
        (NumericPivot::Float(v), CDataType::UTinyInt) => unsafe {
            write_truncated_float::<u8>(v, target_ptr, len_ind_ptr)
        },
        (NumericPivot::Float(v), CDataType::UShort) => unsafe {
            write_truncated_float::<u16>(v, target_ptr, len_ind_ptr)
        },
        (NumericPivot::Float(v), CDataType::ULong) => unsafe {
            write_truncated_float::<u32>(v, target_ptr, len_ind_ptr)
        },
        (NumericPivot::Float(v), CDataType::UBigInt) => unsafe {
            write_truncated_float::<u64>(v, target_ptr, len_ind_ptr)
        },
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
        // The table's three rows in order: 0 or 1 converts with no diagnostic;
        // greater than 0, less than 2 and not equal to 1 converts to "Truncated
        // data" with 01S07; less than 0 or at least 2 is 22003 with nothing
        // written. The range test below already answers 22003 for both
        // infinities (`+inf` fails the upper bound, `-inf` the lower) and for
        // NaN (every IEEE comparison against NaN is false, so `contains` is
        // false), so neither needs a branch of its own to reach the right
        // SQLSTATE. The explicit NaN branch is kept for its more specific
        // message, not for its state; `float_nan_to_bit_returns_22003` covers
        // it, and deleting it would change what an application reads in the
        // diagnostic rather than what it returns.
        (NumericPivot::Float(v), CDataType::Bit) => {
            if v.is_nan() {
                return Err(OdbcError::general(
                    "Numeric value out of range: NaN cannot be converted to SQL_C_BIT",
                    SqlState::numeric_value_out_of_range(),
                ));
            }
            if !(0.0..2.0).contains(&v) {
                return Err(OdbcError::general(
                    format!("Numeric value out of range: {v} is not in [0, 2)"),
                    SqlState::numeric_value_out_of_range(),
                ));
            }
            // Truncation towards zero: 0.5 delivers 0 and 1.5 delivers 1.
            let bit = v as u8;
            unsafe {
                let _ = write_fixed(target_ptr, len_ind_ptr, bit)?;
            };
            if f64::from(bit) == v {
                Ok(SqlReturn::SUCCESS)
            } else {
                Err(OdbcError::FractionalTruncation)
            }
        }
        (_, _) => Err(OdbcError::general(
            format!("Unsupported numeric target type: {target_type:?}"),
            SqlState::invalid_application_buffer_type(),
        )),
    }
}

/// Write an `f64` into an integer C target, truncated toward zero.
///
/// The governing row is the same one [`write_exact_integer`] cites — [SQL to C:
/// Numeric]'s row for `SQL_C_STINYINT`, `SQL_C_UTINYINT`, `SQL_C_TINYINT`,
/// `SQL_C_SBIGINT`, `SQL_C_UBIGINT`, `SQL_C_SSHORT`, `SQL_C_USHORT`,
/// `SQL_C_SHORT`, `SQL_C_SLONG`, `SQL_C_ULONG`, `SQL_C_LONG` and
/// `SQL_C_NUMERIC`. That table covers the approximate numeric SQL types
/// (`SQL_REAL`, `SQL_FLOAT`, `SQL_DOUBLE`) alongside the exact ones — its
/// identifier list names all nine — so a float source gets the same three
/// outcomes. Eight of the row's C types reach here, one per caller in
/// [`write_numeric_pivot`]: `odbc-sys` models the deprecated `SQL_C_TINYINT`,
/// `SQL_C_SHORT` and `SQL_C_LONG` only as commented-out entries, and
/// `SQL_C_NUMERIC` has no arm in that function at all. The three outcomes:
///
/// - "Data converted without truncation" — `SQL_SUCCESS`.
/// - "Data converted with truncation of fractional digits" — the truncated
///   value is written and `01S07` returned.
/// - "Conversion of data would result in loss of whole (as opposed to
///   fractional) digits" — `22003`, with `*TargetValuePtr` left alone, which the
///   table's "Undefined" requires.
///
/// **The order matters: truncate first, then range-check the truncated value.**
/// It is whole digits the third outcome protects, so `127.5` into
/// `SQL_C_STINYINT` is the second outcome and writes `127`, and `-0.5` into any
/// unsigned target writes `0` — the same reading [`write_exact_integer`] gives
/// the text path, so the two agree on that value.
///
/// Two notes on the arithmetic:
///
/// - **The finiteness test is load-bearing, not defensive.** Rust's
///   float-to-integer `as` cast is saturating and maps `NaN` to `0`, so without
///   it a `NaN` would write `0` and report success. An infinity would be caught
///   anyway, by saturating to `i128::MIN`/`i128::MAX` and failing
///   `T::try_from`; the test covers it too, and gives it a clearer diagnostic.
/// - **`i128` is the exact intermediary for every one of the eight targets.**
///   A finite `f64` carries at most 53 significant bits, so its truncation is
///   representable in `i128` exactly whenever `|v| < 2^127`, and the eight
///   target widths are all far below that — `T::try_from` therefore decides the
///   bound at the target's own width rather than at an `f64`-rounded
///   approximation of it. That is what makes `2^63` reject for `SQL_C_SBIGINT`
///   where `v > i64::MAX as f64` admitted it: `i64::MAX as f64` rounds *up* to
///   `2^63`. A magnitude at or beyond `2^127` saturates to `i128::MIN`/`MAX`
///   and fails `T::try_from`, which is the same `22003` it deserves.
///
/// [SQL to C: Numeric]: https://learn.microsoft.com/en-us/sql/odbc/reference/appendixes/sql-to-c-numeric
unsafe fn write_truncated_float<T: Copy + TryFrom<i128>>(
    v: f64,
    target_ptr: *mut c_void,
    len_ind_ptr: *mut isize,
) -> Result<SqlReturn, OdbcError> {
    let truncated = v.trunc();
    let n = v
        .is_finite()
        .then(|| T::try_from(truncated as i128).ok())
        .flatten()
        .ok_or_else(|| {
            OdbcError::general(
                format!("Numeric value out of range: {v}"),
                SqlState::numeric_value_out_of_range(),
            )
        })?;
    unsafe {
        let _ = write_fixed(target_ptr, len_ind_ptr, n)?;
    };
    if truncated == v {
        Ok(SqlReturn::SUCCESS)
    } else {
        Err(OdbcError::FractionalTruncation)
    }
}

/// Write an exact decimal into an integer C target, truncated toward zero.
///
/// **Two tables govern this arm, and they agree.** A `ColumnValue::String` comes
/// from `SQL_CHAR`/`SQL_VARCHAR`, so [SQL to C: Character] applies; a
/// `ColumnValue::Decimal` comes from `SQL_DECIMAL`/`SQL_NUMERIC`, so [SQL to C:
/// Numeric] does. Their exact-numeric rows list the same twelve C types and the
/// same three outcomes, in the same order and with the same SQLSTATEs. The one
/// difference is a fourth row that only the character table has — "Data is not a
/// *numeric-literal*" → `22018` — which a numeric SQL source cannot reach, and
/// which is handled by [`column_value_as_numeric`] returning `None` rather than
/// here.
///
/// The three shared outcomes, in the tables' own order:
///
/// - "Data converted without truncation" — `SQL_SUCCESS`, nothing to report.
/// - "Data converted with truncation of fractional digits" — the truncated
///   value is written and `01S07` returned. Truncation is toward zero, which is
///   what [`DecimalLiteral::to_integer`] does, so `-3.9` delivers `-3`.
/// - "Conversion of data would result in loss of whole (as opposed to
///   fractional) digits" — `22003`, and the range test runs before the write so
///   `*TargetValuePtr` is left alone. A magnitude beyond `i128` reaches the same
///   branch: no integer C type holds it either.
///
/// A fraction of zeros loses nothing, so `42.000` is the first row and not the
/// second. Note what the third row does *not* say: it is whole digits that must
/// survive, so `-0.5` into an unsigned target is the second row and writes `0`,
/// not the third.
///
/// [SQL to C: Character]: https://learn.microsoft.com/en-us/sql/odbc/reference/appendixes/sql-to-c-character
/// [SQL to C: Numeric]: https://learn.microsoft.com/en-us/sql/odbc/reference/appendixes/sql-to-c-numeric
unsafe fn write_exact_integer<T: Copy + TryFrom<i128>>(
    literal: &DecimalLiteral,
    text: &str,
    target_ptr: *mut c_void,
    len_ind_ptr: *mut isize,
) -> Result<SqlReturn, OdbcError> {
    let n = literal
        .to_integer()
        .and_then(|v| T::try_from(v).ok())
        .ok_or_else(|| {
            OdbcError::general(
                format!("Numeric value out of range: {text}"),
                SqlState::numeric_value_out_of_range(),
            )
        })?;
    unsafe {
        let _ = write_fixed(target_ptr, len_ind_ptr, n)?;
    };
    if literal.fraction_is_zero() {
        Ok(SqlReturn::SUCCESS)
    } else {
        Err(OdbcError::FractionalTruncation)
    }
}

// ---------------------------------------------------------------------------
// Helper: convert ColumnValue to string for coercion
// ---------------------------------------------------------------------------

/// The textual form of an infinite float for `SQL_C_CHAR`/`SQL_C_WCHAR`.
///
/// The ODBC spec defines no textual form for a non-finite float, so this is
/// decided by ecosystem fit rather than by conformance. Every relevant
/// neighbour agrees against Rust: Trino renders `Infinity`, the Trino JDBC
/// driver renders `Infinity`, and PostgreSQL — the other major data source with
/// infinite floats — emits `Infinity` in its own text output. Rust's `Display`
/// gives `inf`/`-inf`, which arrived here by default rather than by decision.
///
/// The deciding argument is that this is core's *shared* coercion path and a
/// driver cannot override it, so core should not impose a Rust-ism on every
/// backend. `NaN` already agrees between Rust and Java and is deliberately left
/// alone, so only the two infinities differ from `Display`.
const fn infinity_text(negative: bool) -> &'static str {
    if negative { "-Infinity" } else { "Infinity" }
}

fn column_value_to_string(value: &ColumnValue) -> String {
    match value {
        ColumnValue::Null => String::new(),
        ColumnValue::String(s) => s.clone(),
        ColumnValue::I8(v) => v.to_string(),
        ColumnValue::I16(v) => v.to_string(),
        ColumnValue::I32(v) => v.to_string(),
        ColumnValue::I64(v) => v.to_string(),
        // Match guards rather than one shared `f64` helper: widening `f32`
        // would change every *finite* value too, since `0.1f32` prints as "0.1"
        // but `0.1f32 as f64` prints as "0.10000000149011612".
        ColumnValue::F32(v) if v.is_infinite() => infinity_text(v.is_sign_negative()).to_string(),
        ColumnValue::F32(v) => v.to_string(),
        ColumnValue::F64(v) if v.is_infinite() => infinity_text(v.is_sign_negative()).to_string(),
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

    /// The three fields are reachable through accessors rather than directly, so
    /// the struct can gain a field, or an invariant, without a driver having to
    /// change. A driver reads a `ChunkWrite` and never builds one, so there is no
    /// constructor to cover here.
    #[test]
    fn chunk_write_exposes_every_field_through_an_accessor() {
        let write = ChunkWrite {
            ret: SqlReturn::SUCCESS_WITH_INFO,
            delivered: 7,
            chunkable: true,
        };

        assert_eq!(write.ret(), SqlReturn::SUCCESS_WITH_INFO);
        assert_eq!(write.delivered(), 7);
        assert!(write.chunkable());
    }

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
        // A non-null target with buf_len == 0 has no room for even the null
        // terminator, which is total truncation (SUCCESS_WITH_INFO / 01004),
        // not a length query. Only a null target is a length query
        // (SUCCESS) — see `write_utf16`'s identical split.
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
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS_WITH_INFO);
        assert_eq!(ind, 10); // 5 chars * 2 bytes, still reported
        assert_eq!(buf, [0xAA; 4], "wrote into a zero-length buffer");
    }

    #[test]
    fn wchar_null_target_with_zero_length_is_a_pure_length_query() {
        // A null target pointer stays SUCCESS regardless of buf_len. Not
        // something SQLGetData's own spec sanctions directly — its Arguments
        // section says "TargetValuePtr cannot be NULL" — but this writer is
        // shared with `sql_fetch`'s bound-column loop, where a null
        // `SQL_DESC_DATA_PTR` paired with a live indicator pointer is the
        // spec-legal indicator-only binding (see `write_wchar`'s doc
        // comment on this branch for the full reasoning).
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
        assert_eq!(ind, 10);
    }

    #[test]
    fn char_zero_length_buffer_reports_size_and_writes_nothing() {
        // The write_char sibling of the wchar case above.
        let mut buf = [0xAAu8; 4];
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::String("hello".into()),
                CDataType::Char,
                buf.as_mut_ptr() as *mut c_void,
                0,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS_WITH_INFO);
        assert_eq!(ind, 5);
        assert_eq!(buf, [0xAA; 4], "wrote into a zero-length buffer");
    }

    #[test]
    fn binary_zero_length_buffer_reports_size_and_writes_nothing() {
        // The write_binary sibling of the wchar case above. Binary has no
        // null terminator, so the "no room to make progress" condition is
        // simply buf_len <= 0 rather than needing 2 bytes.
        let mut buf = [0xAAu8; 4];
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF]),
                CDataType::Binary,
                buf.as_mut_ptr() as *mut c_void,
                0,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS_WITH_INFO);
        assert_eq!(ind, 4);
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

    /// Read a `ColumnValue` back as the text `SQL_C_CHAR` would deliver.
    fn char_text_of(value: &ColumnValue) -> String {
        let mut buf = [0u8; 64];
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                value,
                CDataType::Char,
                buf.as_mut_ptr() as *mut c_void,
                64,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        let len = usize::try_from(ind).expect("indicator is a byte count");
        String::from_utf8(buf[..len].to_vec()).expect("ASCII")
    }

    #[test]
    fn infinity_renders_as_infinity_not_inf() {
        // The ODBC spec defines no textual form for a non-finite float, so this
        // is decided by ecosystem fit: Trino, its JDBC driver and PostgreSQL all
        // spell it `Infinity`/`-Infinity`, while Rust's `Display` gives
        // `inf`/`-inf`. This is core's shared coercion path and a driver cannot
        // override it, so core does not impose a Rust-ism on every backend.
        assert_eq!(char_text_of(&ColumnValue::F64(f64::INFINITY)), "Infinity");
        assert_eq!(
            char_text_of(&ColumnValue::F64(f64::NEG_INFINITY)),
            "-Infinity"
        );
        assert_eq!(char_text_of(&ColumnValue::F32(f32::INFINITY)), "Infinity");
        assert_eq!(
            char_text_of(&ColumnValue::F32(f32::NEG_INFINITY)),
            "-Infinity"
        );
    }

    #[test]
    fn nan_still_renders_as_nan() {
        // Rust and Java already agree on this one, so it must NOT move with the
        // infinities.
        assert_eq!(char_text_of(&ColumnValue::F64(f64::NAN)), "NaN");
        assert_eq!(char_text_of(&ColumnValue::F32(f32::NAN)), "NaN");
    }

    #[test]
    fn finite_floats_are_not_widened_when_rendered() {
        // Guards the obvious wrong fix: routing `f32` through `f64` to share one
        // helper. `0.1f32` prints as "0.1", but `0.1f32 as f64` prints as
        // "0.10000000149011612".
        assert_eq!(char_text_of(&ColumnValue::F32(0.1)), "0.1");
        assert_eq!(char_text_of(&ColumnValue::F64(1.5)), "1.5");
    }

    #[test]
    fn the_infinity_spelling_parses_back_into_a_float() {
        // The round trip: a backend returning "Infinity" as *text* that an
        // application then requests as a numeric type goes through
        // `parse_numeric_text`. Rust's float parser is ASCII-case-insensitive
        // over `inf`/`infinity`, so both spellings survive — but nothing pinned
        // that before, and emitting a spelling core cannot read back would be a
        // one-way door.
        //
        // An integer target is the interesting one: none of these is a
        // *numeric-literal*, so the exact path declines them and the `f64`
        // fallback is what answers.
        for text in ["Infinity", "-Infinity", "inf", "-inf"] {
            assert!(
                matches!(
                    parse_numeric_text(text, CDataType::SBigInt),
                    Some(NumericPivot::Float(f)) if f.is_infinite()
                ),
                "{text} should parse back to an infinite f64"
            );
        }
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
        // "Data converted without truncation": a whole float loses nothing, so
        // this is the exact-numeric row's first outcome and carries no
        // diagnostic. The fractional case is
        // `f64_3_9_to_slong_is_3_with_01s07`, below.
        let mut buf: i32 = 0;
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::F64(3.0),
                CDataType::SLong,
                &mut buf as *mut i32 as *mut c_void,
                4,
                &mut ind,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(buf, 3);
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
    // Tests for the exact-numeric row of "SQL to C: Numeric" reached from a
    // float source: truncate toward zero, then range-check the *truncated*
    // value. A dropped non-zero fraction is 01S07 with the truncated data
    // written; only a truncation that still does not fit is 22003 with nothing
    // written.
    // -----------------------------------------------------------------------

    /// Assert the 01S07 outcome: `expected` written, `SQL_SUCCESS_WITH_INFO`.
    fn assert_truncated_to<T: Copy + PartialEq + std::fmt::Debug + Default>(
        v: f64,
        target_type: CDataType,
        expected: T,
    ) {
        let mut out = T::default();
        let mut ind: isize = 0;
        let err = unsafe {
            write_column_value(
                &ColumnValue::F64(v),
                target_type,
                std::ptr::from_mut(&mut out).cast(),
                size_of::<T>() as isize,
                &mut ind,
            )
        }
        .expect_err("a dropped fraction is reported, not silent");
        assert_eq!(
            sqlstate_of_err(&err),
            SqlState::fractional_truncation().as_str()
        );
        assert_eq!(err.sql_return(), SqlReturn::SUCCESS_WITH_INFO);
        // "Truncated data" in the table's *TargetValuePtr* column.
        assert_eq!(out, expected);
        assert_eq!(ind, size_of::<T>() as isize);
    }

    /// Assert the 22003 outcome: nothing written, `sentinel` still in place.
    fn assert_out_of_range_leaves<T: Copy + PartialEq + std::fmt::Debug>(
        v: f64,
        target_type: CDataType,
        sentinel: T,
    ) {
        let mut out = sentinel;
        let err = unsafe {
            write_column_value(
                &ColumnValue::F64(v),
                target_type,
                std::ptr::from_mut(&mut out).cast(),
                size_of::<T>() as isize,
                std::ptr::null_mut(),
            )
        }
        .expect_err("whole digits would be lost");
        assert_eq!(
            sqlstate_of_err(&err),
            SqlState::numeric_value_out_of_range().as_str()
        );
        // "Undefined" in the table's *TargetValuePtr* column: nothing written.
        assert_eq!(out, sentinel);
    }

    #[test]
    fn f64_3_9_to_slong_is_3_with_01s07() {
        assert_truncated_to(3.9, CDataType::SLong, 3i32);
    }

    #[test]
    fn f64_minus_3_9_to_slong_truncates_toward_zero_with_01s07() {
        // Toward zero, not toward negative infinity: -3, never -4.
        assert_truncated_to(-3.9, CDataType::SLong, -3i32);
    }

    #[test]
    fn f64_127_5_to_stinyint_is_127_with_01s07() {
        assert_truncated_to(127.5, CDataType::STinyInt, 127i8);
    }

    #[test]
    fn f64_128_0_to_stinyint_is_22003() {
        assert_out_of_range_leaves(128.0, CDataType::STinyInt, 9i8);
    }

    #[test]
    fn f64_minus_128_9_to_stinyint_is_minus_128_with_01s07() {
        assert_truncated_to(-128.9, CDataType::STinyInt, -128i8);
    }

    #[test]
    fn f64_minus_129_0_to_stinyint_is_22003() {
        assert_out_of_range_leaves(-129.0, CDataType::STinyInt, 9i8);
    }

    #[test]
    fn f64_minus_0_5_to_utinyint_is_0_with_01s07() {
        // It is whole digits that must survive, and -0.5 has none: truncation
        // gives 0, which fits. The same reading governs the text path, in
        // `write_exact_integer`.
        assert_truncated_to(-0.5, CDataType::UTinyInt, 0u8);
    }

    #[test]
    fn f64_minus_1_5_to_utinyint_is_22003() {
        // -1.5 truncates to -1, a whole digit an unsigned target cannot hold.
        assert_out_of_range_leaves(-1.5, CDataType::UTinyInt, 9u8);
    }

    #[test]
    fn f64_32767_5_to_sshort_is_i16_max_with_01s07() {
        assert_truncated_to(32_767.5, CDataType::SShort, i16::MAX);
    }

    #[test]
    fn f64_65535_5_to_ushort_is_u16_max_with_01s07() {
        assert_truncated_to(65_535.5, CDataType::UShort, u16::MAX);
    }

    #[test]
    fn f64_2147483647_5_to_slong_is_i32_max_with_01s07() {
        assert_truncated_to(2_147_483_647.5, CDataType::SLong, i32::MAX);
    }

    #[test]
    fn f64_4294967295_5_to_ulong_is_u32_max_with_01s07() {
        assert_truncated_to(4_294_967_295.5, CDataType::ULong, u32::MAX);
    }

    #[test]
    fn f64_just_below_two_pow_63_to_sbigint_is_written() {
        // The largest f64 below 2^63: spacing in [2^62, 2^63) is 1024, so this
        // is 2^63 - 1024. It fits i64, and the sibling test at 2^63 itself does
        // not — together they pin the bound as exact for the target's width.
        let v = 9_223_372_036_854_774_784.0_f64;
        let mut out = 0i64;
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::F64(v),
                CDataType::SBigInt,
                std::ptr::from_mut(&mut out).cast(),
                size_of::<i64>() as isize,
                &mut ind,
            )
        }
        .expect("2^63 - 1024 fits in i64");
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!(out, 9_223_372_036_854_774_784);
    }

    #[test]
    fn f64_just_below_two_pow_64_to_ubigint_is_written() {
        // The largest f64 below 2^64: spacing in [2^63, 2^64) is 2048.
        let v = 18_446_744_073_709_549_568.0_f64;
        let mut out = 0u64;
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::F64(v),
                CDataType::UBigInt,
                std::ptr::from_mut(&mut out).cast(),
                size_of::<u64>() as isize,
                &mut ind,
            )
        }
        .expect("2^64 - 2048 fits in u64");
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!(out, 18_446_744_073_709_549_568);
    }

    #[test]
    fn f64_beyond_i128_range_is_22003_with_nothing_written() {
        // 1e300 is finite, so it passes the finiteness test and reaches the
        // cast: `as i128` saturates it to `i128::MAX`, which `i64::try_from`
        // then rejects. The negative counterpart saturates to `i128::MIN`, so
        // both ends of the saturation are pinned rather than argued.
        assert_out_of_range_leaves(1e300, CDataType::SBigInt, 9i64);
        assert_out_of_range_leaves(-1e300, CDataType::SBigInt, 9i64);
    }

    #[test]
    fn f64_nan_to_utinyint_is_22003_with_nothing_written() {
        // NaN has no truncation, so it stays 22003. Load-bearing: Rust's
        // saturating float-to-integer cast maps NaN to 0, so without the
        // finiteness test a NaN would write 0 and report success.
        assert_out_of_range_leaves(f64::NAN, CDataType::UTinyInt, 9u8);
    }

    #[test]
    fn f64_infinity_to_ubigint_is_22003_with_nothing_written() {
        assert_out_of_range_leaves(f64::INFINITY, CDataType::UBigInt, 9u64);
    }

    // -----------------------------------------------------------------------
    // Tests for the SQL_C_BIT row of "SQL to C: Numeric": 0 or 1 converts, a
    // fraction in (0, 2) converts with 01S07, anything else is 22003.
    // -----------------------------------------------------------------------

    #[test]
    fn i64_five_to_bit_is_22003() {
        let mut buf: u8 = 9;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::I64(5),
                CDataType::Bit,
                &mut buf as *mut u8 as *mut c_void,
                1,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(
            sqlstate_of_err(&ret.unwrap_err()),
            SqlState::numeric_value_out_of_range().as_str()
        );
        // "Undefined" in the table's *TargetValuePtr* column: nothing written.
        assert_eq!(buf, 9);
    }

    #[test]
    fn i64_minus_one_to_bit_is_22003() {
        let mut buf: u8 = 9;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::I64(-1),
                CDataType::Bit,
                &mut buf as *mut u8 as *mut c_void,
                1,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(
            sqlstate_of_err(&ret.unwrap_err()),
            SqlState::numeric_value_out_of_range().as_str()
        );
        assert_eq!(buf, 9);
    }

    #[test]
    fn f64_half_to_bit_writes_zero_with_01s07() {
        let mut buf: u8 = 9;
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::F64(0.5),
                CDataType::Bit,
                &mut buf as *mut u8 as *mut c_void,
                1,
                &mut ind,
            )
        };
        let err = ret.unwrap_err();
        assert_eq!(err.sql_return(), SqlReturn::SUCCESS_WITH_INFO);
        assert_eq!(
            sqlstate_of_err(&err),
            SqlState::fractional_truncation().as_str()
        );
        // "Truncated data": the fractional part is dropped, so 0.5 delivers 0.
        assert_eq!(buf, 0);
        assert_eq!(ind, 1);
    }

    #[test]
    fn f64_one_point_five_to_bit_writes_one_with_01s07() {
        let mut buf: u8 = 9;
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::F64(1.5),
                CDataType::Bit,
                &mut buf as *mut u8 as *mut c_void,
                1,
                &mut ind,
            )
        };
        let err = ret.unwrap_err();
        assert_eq!(err.sql_return(), SqlReturn::SUCCESS_WITH_INFO);
        assert_eq!(
            sqlstate_of_err(&err),
            SqlState::fractional_truncation().as_str()
        );
        assert_eq!(buf, 1);
        assert_eq!(ind, 1);
    }

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

    /// `-0.0` is the "Data is 0 or 1" row, not the "less than 0" one: IEEE makes
    /// it equal to `0.0`, so it is inside the range and truncates to 0.
    /// Tightening the lower bound to `v > 0.0` would break `float_zero_to_bit_is_zero`
    /// too, since `0.0 > 0.0` is false; what this test adds is the *signed*-zero
    /// half of that boundary, which nothing covered before and which a reader is
    /// likelier to reason about incorrectly.
    #[test]
    fn float_negative_zero_to_bit_is_zero() {
        let mut buf: u8 = 9;
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::F64(-0.0),
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

    /// `2.0` is the lower edge of the table's "greater than or equal to 2" row,
    /// so it is 22003 and not the 1 an earlier revision wrote for every non-zero
    /// float.
    #[test]
    fn float_two_to_bit_is_22003() {
        let mut buf: u8 = 9;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::F64(2.0),
                CDataType::Bit,
                &mut buf as *mut u8 as *mut c_void,
                1,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(
            sqlstate_of_err(&ret.unwrap_err()),
            SqlState::numeric_value_out_of_range().as_str()
        );
        assert_eq!(buf, 9);
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
    // Decimal/String text → integer C targets: the exact-numeric row of *SQL to
    // C: Character* (a String source) and of *SQL to C: Numeric* (a Decimal
    // source), which agree. "Data converted with truncation of fractional
    // digits" is 01S07 with the truncated data written; "loss of whole (as
    // opposed to fractional) digits" is 22003 with nothing written.
    // -----------------------------------------------------------------------

    #[test]
    fn decimal_text_above_2_pow_53_to_sbigint_is_exact() {
        let mut out = 0i64;
        let mut ind = 0isize;
        let err = unsafe {
            write_column_value(
                &ColumnValue::Decimal("9007199254740993.5".into()),
                CDataType::SBigInt,
                std::ptr::from_mut(&mut out).cast(),
                size_of::<i64>() as isize,
                &mut ind,
            )
        }
        .expect_err("a dropped fraction is 01S07, not success");
        // The nearest f64 to 9007199254740993.5 is 9007199254740994.0, so a
        // conversion routed through f64 delivers ...94 here.
        assert_eq!(out, 9_007_199_254_740_993);
        assert_eq!(
            err.sqlstate().as_str(),
            crate::types::sql_state::FRACTIONAL_TRUNCATION
        );
    }

    #[test]
    fn decimal_text_fraction_to_slong_truncates_toward_zero_with_01s07() {
        let mut out = 0i32;
        let mut ind = 0isize;
        let err = unsafe {
            write_column_value(
                &ColumnValue::Decimal("-3.9".into()),
                CDataType::SLong,
                std::ptr::from_mut(&mut out).cast(),
                size_of::<i32>() as isize,
                &mut ind,
            )
        }
        .expect_err("a dropped fraction is 01S07, not success");
        assert_eq!(out, -3, "truncation is toward zero, not to -4");
        assert_eq!(
            err.sqlstate().as_str(),
            crate::types::sql_state::FRACTIONAL_TRUNCATION
        );
    }

    #[test]
    fn decimal_text_at_u64_max_with_fraction_to_ubigint_is_exact() {
        let mut out = 0u64;
        let mut ind = 0isize;
        let err = unsafe {
            write_column_value(
                &ColumnValue::Decimal("18446744073709551615.9".into()),
                CDataType::UBigInt,
                std::ptr::from_mut(&mut out).cast(),
                size_of::<u64>() as isize,
                &mut ind,
            )
        }
        .expect_err("a dropped fraction is 01S07, not success");
        // The nearest f64 is 2^64 exactly, which the range test rejects, so a
        // conversion routed through f64 answers 22003 for a value that fits.
        assert_eq!(out, u64::MAX);
        assert_eq!(
            err.sqlstate().as_str(),
            crate::types::sql_state::FRACTIONAL_TRUNCATION
        );
    }

    #[test]
    fn decimal_text_with_an_all_zero_fraction_to_slong_is_not_a_truncation() {
        let mut out = 0i32;
        let mut ind = 0isize;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::Decimal("42.000".into()),
                CDataType::SLong,
                std::ptr::from_mut(&mut out).cast(),
                size_of::<i32>() as isize,
                &mut ind,
            )
        }
        .expect("a fraction of zeros loses nothing");
        assert_eq!(out, 42);
        assert_eq!(ret, SqlReturn::SUCCESS);
    }

    #[test]
    fn decimal_text_in_exponent_form_to_slong_converts() {
        let mut out = 0i32;
        let mut ind = 0isize;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::Decimal("1.5e2".into()),
                CDataType::SLong,
                std::ptr::from_mut(&mut out).cast(),
                size_of::<i32>() as isize,
                &mut ind,
            )
        }
        .expect("an exponent that consumes the fraction loses nothing");
        assert_eq!(out, 150);
        assert_eq!(ret, SqlReturn::SUCCESS);
    }

    #[test]
    fn decimal_text_below_one_to_slong_writes_zero_with_01s07() {
        // The whole part is zero and the fraction is all there is, so the row is
        // "converted with truncation of fractional digits" and not "loss of
        // whole digits". The two spellings take different branches of
        // `DecimalLiteral::to_integer`: "0.5" has a digit left of the point to
        // slice, ".5" has none and falls to its |value| < 1 arm.
        for text in ["0.5", ".5"] {
            let mut out = 7i32;
            let mut ind = 0isize;
            let err = unsafe {
                write_column_value(
                    &ColumnValue::Decimal(text.into()),
                    CDataType::SLong,
                    std::ptr::from_mut(&mut out).cast(),
                    size_of::<i32>() as isize,
                    &mut ind,
                )
            }
            .expect_err("a dropped fraction is 01S07, not success");
            assert_eq!(out, 0, "{text} truncates to zero");
            assert_eq!(
                err.sqlstate().as_str(),
                crate::types::sql_state::FRACTIONAL_TRUNCATION,
                "{text} dropped a fraction"
            );
        }
    }

    #[test]
    fn negative_decimal_text_above_minus_one_to_utinyint_writes_zero_with_01s07() {
        // The unsigned targets are where truncating first changes the answer:
        // -0.5 loses no *whole* digits, so the truncated value is 0 and it fits.
        // Reading the sign before the truncation makes this 22003 instead.
        let mut out = 7u8;
        let mut ind = 0isize;
        let err = unsafe {
            write_column_value(
                &ColumnValue::Decimal("-0.5".into()),
                CDataType::UTinyInt,
                std::ptr::from_mut(&mut out).cast(),
                size_of::<u8>() as isize,
                &mut ind,
            )
        }
        .expect_err("a dropped fraction is 01S07, not success");
        assert_eq!(out, 0);
        assert_eq!(
            err.sqlstate().as_str(),
            crate::types::sql_state::FRACTIONAL_TRUNCATION
        );
    }

    #[test]
    fn decimal_text_with_fraction_beyond_target_range_returns_22003_and_writes_nothing() {
        let mut out = 7i32;
        let mut ind = 0isize;
        let err = unsafe {
            write_column_value(
                &ColumnValue::Decimal("99999999999.5".into()),
                CDataType::SLong,
                std::ptr::from_mut(&mut out).cast(),
                size_of::<i32>() as isize,
                &mut ind,
            )
        }
        .expect_err("losing whole digits must not convert");
        assert_eq!(
            out, 7,
            "22003 leaves *TargetValuePtr undefined, so unwritten"
        );
        assert_eq!(
            err.sqlstate().as_str(),
            crate::types::sql_state::NUMERIC_VALUE_OUT_OF_RANGE
        );
    }

    #[test]
    fn decimal_text_beyond_i128_to_sbigint_returns_22003() {
        let mut out = 7i64;
        let mut ind = 0isize;
        let err = unsafe {
            write_column_value(
                &ColumnValue::Decimal("1e40".into()),
                CDataType::SBigInt,
                std::ptr::from_mut(&mut out).cast(),
                size_of::<i64>() as isize,
                &mut ind,
            )
        }
        .expect_err("losing whole digits must not convert");
        assert_eq!(out, 7);
        assert_eq!(
            err.sqlstate().as_str(),
            crate::types::sql_state::NUMERIC_VALUE_OUT_OF_RANGE
        );
    }

    /// A column value is whatever the data source sent, so its exponent is
    /// attacker-controlled on a compromised or hostile source. Before
    /// `param_convert::MAX_DECIMAL_EXPANSION_DIGITS`, this reached
    /// `"0".repeat(2_147_483_646)` inside `DecimalLiteral::to_integer` — a
    /// ~2 GB allocation, with a second copy in the `format!` that follows —
    /// and an allocation failure aborts the process rather than unwinding, so
    /// `panic_safe` could not contain it.
    ///
    /// The SQLSTATE is the one the exact-numeric row of both *SQL to C:
    /// Character* and *SQL to C: Numeric* gives for "Conversion of data would
    /// result in loss of whole (as opposed to fractional) digits", and that
    /// row's `TargetValuePtr` column reads "Undefined", so nothing is written.
    #[test]
    fn a_hostile_exponent_from_the_data_source_is_22003_without_expanding() {
        let mut out = 7i64;
        let mut ind = 0isize;
        let err = unsafe {
            write_column_value(
                &ColumnValue::Decimal("1e2147483646".into()),
                CDataType::SBigInt,
                std::ptr::from_mut(&mut out).cast(),
                size_of::<i64>() as isize,
                &mut ind,
            )
        }
        .expect_err("an exponent no integer target can hold must not convert");
        assert_eq!(out, 7);
        assert_eq!(
            err.sqlstate().as_str(),
            crate::types::sql_state::NUMERIC_VALUE_OUT_OF_RANGE
        );
    }

    /// A magnitude far below the target's resolution truncates toward zero at
    /// no cost — `to_integer`'s positive-scale branch slices digits the source
    /// supplied rather than expanding anything — so the expansion bound must
    /// not reach it. `01S07` because a non-zero fraction was dropped.
    #[test]
    fn a_pathologically_small_column_value_truncates_to_zero_with_01s07() {
        let mut out = 7i64;
        let mut ind = 0isize;
        let err = unsafe {
            write_column_value(
                &ColumnValue::Decimal("1e-2000000".into()),
                CDataType::SBigInt,
                std::ptr::from_mut(&mut out).cast(),
                size_of::<i64>() as isize,
                &mut ind,
            )
        }
        .expect_err("a dropped fraction is 01S07, not success");
        assert_eq!(out, 0);
        assert_eq!(
            err.sqlstate().as_str(),
            crate::types::sql_state::FRACTIONAL_TRUNCATION
        );
    }

    /// The same, one synthesised digit past the bound rather than at the
    /// extreme, so the test pins the bound and not merely `i128`'s range.
    #[test]
    fn a_column_value_one_digit_past_the_expansion_limit_is_22003() {
        let text = format!(
            "1e{}",
            crate::param_convert::MAX_DECIMAL_EXPANSION_DIGITS + 1
        );
        let mut out = 7i64;
        let mut ind = 0isize;
        let err = unsafe {
            write_column_value(
                &ColumnValue::Decimal(text),
                CDataType::SBigInt,
                std::ptr::from_mut(&mut out).cast(),
                size_of::<i64>() as isize,
                &mut ind,
            )
        }
        .expect_err("an unexpandable exponent must not convert");
        assert_eq!(out, 7);
        assert_eq!(
            err.sqlstate().as_str(),
            crate::types::sql_state::NUMERIC_VALUE_OUT_OF_RANGE
        );
    }

    #[test]
    fn decimal_text_above_2_pow_53_to_double_still_goes_through_f64() {
        // The float targets keep the f64 path: their own row of the table asks
        // only that the value be within range, and f64 is the destination.
        let mut out = 0f64;
        let mut ind = 0isize;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::Decimal("9007199254740993.5".into()),
                CDataType::Double,
                std::ptr::from_mut(&mut out).cast(),
                size_of::<f64>() as isize,
                &mut ind,
            )
        }
        .expect("a decimal within f64's range converts");
        assert_eq!(out, 9_007_199_254_740_994.0);
        assert_eq!(ret, SqlReturn::SUCCESS);
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
    // The cross-form rows of SQL to C: Character — a character column whose
    // literal is one datetime form read into a C struct of another.
    //
    // Spec: https://learn.microsoft.com/en-us/sql/odbc/reference/appendixes/sql-to-c-character
    // -----------------------------------------------------------------------

    /// Write `text` into a `T`-shaped buffer as `target`, returning the buffer
    /// and the `SqlReturn`. The buffer starts as `sentinel` so a caller
    /// checking an error path can prove nothing was written.
    unsafe fn convert_text<T: Copy>(
        text: &str,
        target: CDataType,
        sentinel: T,
    ) -> (T, Result<SqlReturn, OdbcError>) {
        let mut out = sentinel;
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::String(text.into()),
                target,
                std::ptr::from_mut(&mut out).cast(),
                size_of::<T>() as isize,
                &mut ind,
            )
        };
        (out, ret)
    }

    /// A `Date` value no conversion can produce, for the error-path canaries.
    fn date_sentinel() -> Date {
        Date {
            year: -9,
            month: 99,
            day: 99,
        }
    }

    /// A `Time` value no conversion can produce (hour 99 > 23).
    fn time_sentinel() -> Time {
        Time {
            hour: 99,
            minute: 99,
            second: 99,
        }
    }

    /// A `Timestamp` value no conversion can produce.
    fn timestamp_sentinel() -> Timestamp {
        Timestamp {
            year: -9,
            month: 99,
            day: 99,
            hour: 99,
            minute: 99,
            second: 99,
            fraction: 99,
        }
    }

    // --- SQL_C_TYPE_DATE, rows 2 and 3: the source is a timestamp-value ---

    #[test]
    fn timestamp_text_with_zero_time_converts_to_date() {
        // "Data value is a valid timestamp-value; time portion is zero" — the
        // date is written and the SQLSTATE column is "n/a".
        let (out, ret) =
            unsafe { convert_text("2026-07-21 00:00:00", CDataType::TypeDate, date_sentinel()) };
        let ret = ret.expect("a timestamp whose time is zero converts to a date cleanly");
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!((out.year, out.month, out.day), (2026, 7, 21));
    }

    #[test]
    fn timestamp_text_with_time_to_date_is_01s07() {
        // "Data value is a valid timestamp-value; time portion is nonzero" —
        // "Truncated data" is written and the SQLSTATE is 01S07, footnote [c]:
        // "The time portion of the timestamp-value is truncated."
        let (out, ret) =
            unsafe { convert_text("2026-07-21 10:30:15", CDataType::TypeDate, date_sentinel()) };
        let err = ret.expect_err("a dropped time portion is 01S07, not success");
        assert_eq!(
            sqlstate_of_err(&err),
            crate::types::sql_state::FRACTIONAL_TRUNCATION
        );
        // "Truncated data": the date is written even though 01S07 is reported.
        assert_eq!((out.year, out.month, out.day), (2026, 7, 21));
    }

    #[test]
    fn timestamp_text_with_only_a_fraction_to_date_is_01s07() {
        // The time portion is "nonzero" if any of its four fields is, and the
        // fraction is one of them.
        let (out, ret) = unsafe {
            convert_text(
                "2026-07-21 00:00:00.5",
                CDataType::TypeDate,
                date_sentinel(),
            )
        };
        let err = ret.expect_err("a dropped fractional second is still a dropped time portion");
        assert_eq!(
            sqlstate_of_err(&err),
            crate::types::sql_state::FRACTIONAL_TRUNCATION
        );
        assert_eq!((out.year, out.month, out.day), (2026, 7, 21));
    }

    #[test]
    fn text_that_is_no_date_or_timestamp_to_date_is_22018() {
        // The row's last line: "Data value is not a valid date-value or
        // timestamp-value" — 22018, *TargetValuePtr* undefined.
        let (out, ret) = unsafe { convert_text("10:30:15", CDataType::TypeDate, date_sentinel()) };
        let err = ret.expect_err("a time-only literal is not a date or a timestamp");
        assert_eq!(
            sqlstate_of_err(&err),
            crate::types::sql_state::INVALID_CHARACTER_VALUE_FOR_CAST
        );
        assert_eq!(out, date_sentinel());
    }

    // --- SQL_C_TYPE_TIME, rows 2 and 3: the source is a timestamp-value ---

    #[test]
    fn timestamp_text_with_zero_fraction_converts_to_time() {
        // "Data value is a valid timestamp-value or a valid time-value;
        // fractional seconds portion is zero", footnote [d]: "The date portion
        // of the timestamp-value is ignored." No SQLSTATE.
        let (out, ret) =
            unsafe { convert_text("2026-07-21 10:30:15", CDataType::TypeTime, time_sentinel()) };
        let ret = ret.expect("a timestamp with no fraction converts to a time cleanly");
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!((out.hour, out.minute, out.second), (10, 30, 15));
    }

    #[test]
    fn timestamp_text_with_fraction_to_time_is_01s07() {
        // "Data value is a valid timestamp-value; fractional seconds portion is
        // nonzero" — 01S07 with the truncated data written. Only the *fraction*
        // provokes it; the discarded date does not, per footnote [d].
        let (out, ret) = unsafe {
            convert_text(
                "2026-07-21 10:30:15.5",
                CDataType::TypeTime,
                time_sentinel(),
            )
        };
        let err = ret.expect_err("a dropped fractional second is 01S07, not success");
        assert_eq!(
            sqlstate_of_err(&err),
            crate::types::sql_state::FRACTIONAL_TRUNCATION
        );
        assert_eq!((out.hour, out.minute, out.second), (10, 30, 15));
    }

    #[test]
    fn text_that_is_no_time_or_timestamp_to_time_is_22018() {
        let (out, ret) =
            unsafe { convert_text("not a time", CDataType::TypeTime, time_sentinel()) };
        let err = ret.expect_err("unparseable text is not a time or a timestamp");
        assert_eq!(
            sqlstate_of_err(&err),
            crate::types::sql_state::INVALID_CHARACTER_VALUE_FOR_CAST
        );
        assert_eq!(out, time_sentinel());
    }

    // --- SQL_C_TYPE_TIMESTAMP, row 4: the source is a time-value ---

    #[test]
    fn time_text_to_timestamp_gets_current_date() {
        // "Data value is a valid time-value", footnote [g]: "The date fields of
        // the timestamp structure are set to the current date." No SQLSTATE.
        //
        // Not flaky across a midnight rollover: the expectation is read from the
        // same clock source the conversion uses, on both sides of the call, and
        // either reading is accepted.
        let before = current_utc_date();
        let (out, ret) =
            unsafe { convert_text("10:30:15", CDataType::TypeTimestamp, timestamp_sentinel()) };
        let after = current_utc_date();
        let ret = ret.expect("a time-only literal converts to a timestamp on the current date");
        assert_eq!(ret, SqlReturn::SUCCESS);
        let written = (out.year, out.month, out.day);
        assert!(
            written == before || written == after,
            "date fields {written:?} are neither {before:?} nor {after:?}",
        );
        assert_eq!((out.hour, out.minute, out.second), (10, 30, 15));
        assert_eq!(out.fraction, 0);
    }

    #[test]
    fn time_text_with_fraction_to_timestamp_carries_nanoseconds() {
        // SQL_TIMESTAMP_STRUCT.fraction is in billionths of a second, so ".5"
        // is 500_000_000 and not 500 or 500_000. The target has a fraction
        // field, so nothing is truncated and the SQLSTATE stays "n/a".
        let (out, ret) =
            unsafe { convert_text("10:30:15.5", CDataType::TypeTimestamp, timestamp_sentinel()) };
        let ret = ret.expect("a fractional time-only literal converts");
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!((out.hour, out.minute, out.second), (10, 30, 15));
        assert_eq!(out.fraction, 500_000_000);
    }

    #[test]
    fn text_that_is_no_datetime_at_all_to_timestamp_is_22018() {
        let (out, ret) = unsafe {
            convert_text(
                "half past ten",
                CDataType::TypeTimestamp,
                timestamp_sentinel(),
            )
        };
        let err = ret.expect_err("unparseable text is not a date, time or timestamp");
        assert_eq!(
            sqlstate_of_err(&err),
            crate::types::sql_state::INVALID_CHARACTER_VALUE_FOR_CAST
        );
        assert_eq!(out, timestamp_sentinel());
    }

    #[test]
    fn time_text_with_out_of_range_hour_to_timestamp_stays_22007() {
        // The fall-through to the time-value form must not relabel this
        // module's 22007 as the row's blanket 22018: "25:00:00" is recognisably
        // a time literal whose hour is out of range.
        let (out, ret) =
            unsafe { convert_text("25:00:00", CDataType::TypeTimestamp, timestamp_sentinel()) };
        let err = ret.expect_err("hour 25 must not convert");
        assert_eq!(
            sqlstate_of_err(&err),
            crate::types::sql_state::INVALID_DATETIME_FORMAT
        );
        assert_eq!(out, timestamp_sentinel());
    }

    #[test]
    fn timestamp_text_with_out_of_range_minute_to_time_stays_22007() {
        // The mirror image on the SQL_C_TYPE_TIME cascade.
        let (out, ret) =
            unsafe { convert_text("2026-07-21 10:99:15", CDataType::TypeTime, time_sentinel()) };
        let err = ret.expect_err("minute 99 must not convert");
        assert_eq!(
            sqlstate_of_err(&err),
            crate::types::sql_state::INVALID_DATETIME_FORMAT
        );
        assert_eq!(out, time_sentinel());
    }

    // -----------------------------------------------------------------------
    // Impossible calendar days.
    //
    // "Data value is not a valid date-value or timestamp-value" — the last row
    // of SQL to C: Character's SQL_C_TYPE_DATE cell. ODBC's own grammar defines
    // `days-value ::= digit digit` and says nothing about which day numbers a
    // month has, so "valid date-value" is validity against the calendar, and the
    // calendar is the proleptic Gregorian one `civil_from_days` already uses in
    // this module. The failures below therefore carry this module's 22007 for a
    // recognised literal with an out-of-range field, not the row's blanket
    // 22018.
    //
    // Spec: https://learn.microsoft.com/en-us/sql/odbc/reference/appendixes/sql-to-c-character
    // Grammar: https://learn.microsoft.com/en-us/sql/odbc/reference/appendixes/date-time-and-timestamp-escape-sequences
    // -----------------------------------------------------------------------

    /// Read `text` as a date and assert it was refused with 22007, writing
    /// nothing.
    fn assert_date_rejected(text: &str) {
        let (out, ret) = unsafe { convert_text(text, CDataType::TypeDate, date_sentinel()) };
        let err = ret.expect_err("an impossible calendar day must not convert");
        assert_eq!(
            sqlstate_of_err(&err),
            crate::types::sql_state::INVALID_DATETIME_FORMAT,
            "{text}"
        );
        assert_eq!(out, date_sentinel(), "{text}");
    }

    /// Read `text` as a date and assert the three fields it produced.
    fn assert_date_accepted(text: &str, expected: (i16, u16, u16)) {
        let (out, ret) = unsafe { convert_text(text, CDataType::TypeDate, date_sentinel()) };
        let ret = ret.expect("a real calendar day must convert");
        assert_eq!(ret, SqlReturn::SUCCESS, "{text}");
        assert_eq!((out.year, out.month, out.day), expected, "{text}");
    }

    #[test]
    fn feb_30_is_rejected() {
        assert_date_rejected("2024-02-30");
    }

    #[test]
    fn feb_29_2024_is_accepted() {
        // Divisible by 4 and not by 100: a leap year.
        assert_date_accepted("2024-02-29", (2024, 2, 29));
    }

    #[test]
    fn feb_29_2023_is_rejected() {
        // Not divisible by 4.
        assert_date_rejected("2023-02-29");
    }

    #[test]
    fn feb_29_1900_is_rejected() {
        // Divisible by 100 and not by 400: not a leap year.
        assert_date_rejected("1900-02-29");
    }

    #[test]
    fn feb_29_2000_is_accepted() {
        // Divisible by 400: a leap year, the exception to the century rule.
        assert_date_accepted("2000-02-29", (2000, 2, 29));
    }

    #[test]
    fn feb_28_is_accepted_in_a_non_leap_year() {
        assert_date_accepted("2023-02-28", (2023, 2, 28));
    }

    #[test]
    fn apr_31_is_rejected() {
        assert_date_rejected("2024-04-31");
    }

    #[test]
    fn day_31_is_rejected_in_every_30_day_month() {
        // The full set of 30-day months: April, June, September, November.
        for month in ["04", "06", "09", "11"] {
            assert_date_rejected(&format!("2024-{month}-31"));
        }
    }

    #[test]
    fn day_30_is_accepted_in_every_30_day_month() {
        for (month, number) in [("04", 4), ("06", 6), ("09", 9), ("11", 11)] {
            assert_date_accepted(&format!("2024-{month}-30"), (2024, number, 30));
        }
    }

    #[test]
    fn day_31_is_accepted_in_every_31_day_month() {
        // The full set of 31-day months: January, March, May, July, August,
        // October, December.
        for (month, number) in [
            ("01", 1),
            ("03", 3),
            ("05", 5),
            ("07", 7),
            ("08", 8),
            ("10", 10),
            ("12", 12),
        ] {
            assert_date_accepted(&format!("2024-{month}-31"), (2024, number, 31));
        }
    }

    #[test]
    fn impossible_day_in_timestamp_text_is_rejected() {
        // The timestamp path shares `parse_date_fields`, so the check reaches it
        // too — a well-formed time does not rescue an impossible date.
        let (out, ret) = unsafe {
            convert_text(
                "2024-02-30 10:00:00",
                CDataType::TypeTimestamp,
                timestamp_sentinel(),
            )
        };
        let err = ret.expect_err("February 30th is not a timestamp either");
        assert_eq!(
            sqlstate_of_err(&err),
            crate::types::sql_state::INVALID_DATETIME_FORMAT
        );
        assert_eq!(out, timestamp_sentinel());
    }

    #[test]
    fn impossible_day_in_timestamp_text_to_time_is_rejected() {
        // The third C target the change reaches. SQL to C: Character's
        // SQL_C_TYPE_TIME cell ignores the *date portion* of a
        // timestamp-value — but only of a valid one, and the row's last line
        // covers text that is not a valid timestamp-value at all.
        let (out, ret) =
            unsafe { convert_text("2024-02-30 10:00:00", CDataType::TypeTime, time_sentinel()) };
        let err = ret.expect_err("an impossible date is not a valid timestamp-value");
        assert_eq!(
            sqlstate_of_err(&err),
            crate::types::sql_state::INVALID_DATETIME_FORMAT
        );
        assert_eq!(out, time_sentinel());
    }

    #[test]
    fn year_zero_stays_accepted_and_is_a_leap_year() {
        // Not a change: `years-value ::= digit digit digit digit` admits "0000"
        // and the parser has always accepted it, so the day check must agree
        // with the proleptic Gregorian calendar the rest of this module uses
        // rather than treat year 0 as a special case. 0 is divisible by 400.
        assert_date_accepted("0000-02-29", (0, 2, 29));
    }

    #[test]
    fn impossible_day_is_refused_on_the_bind_path_too() {
        // `param_convert::to_date` parses through the same
        // `parse_sql_timestamp`, so an impossible day is refused before it can
        // reach a backend as a `ColumnValue::Date`. The 22007 propagates
        // unchanged: `retype_datetime_error` relabels only 22018.
        let err = crate::param_convert::text_to_sql_type(
            "2024-02-30",
            crate::types::SqlDataType::DATE,
            0,
            0,
        )
        .expect_err("February 30th must not be bound as a date");
        assert_eq!(
            sqlstate_of_err(&err),
            crate::types::sql_state::INVALID_DATETIME_FORMAT
        );
    }

    #[test]
    fn impossible_day_is_refused_when_bound_as_a_time() {
        // A separate code route from the `SQL_TYPE_DATE` test above, not a
        // corollary of it: `to_date` propagates the parser's error directly,
        // while `to_time` first tries `parse_sql_time`, and only the failure of
        // *that* reaches `parse_sql_timestamp`.
        //
        // The date is validated even though the conversion discards it. C to
        // SQL: Character's footnote is "the date portion of the timestamp is
        // ignored", but the row it annotates admits a *valid* timestamp-value,
        // and 2024-02-30 is not one. Ignoring a field is not the same as
        // accepting any contents in it.
        let err = crate::param_convert::text_to_sql_type(
            "2024-02-30 10:00:00",
            crate::types::SqlDataType::TIME,
            0,
            0,
        )
        .expect_err("February 30th must not be bound as a time");
        assert_eq!(
            sqlstate_of_err(&err),
            crate::types::sql_state::INVALID_DATETIME_FORMAT
        );
    }

    #[test]
    fn impossible_day_is_refused_when_bound_as_a_timestamp() {
        // The third route: `to_timestamp` falls back to `parse_sql_time` when
        // the timestamp parse fails, and keeps the timestamp parser's error
        // when that fallback fails too.
        let err = crate::param_convert::text_to_sql_type(
            "2024-02-30 10:00:00",
            crate::types::SqlDataType::TIMESTAMP,
            0,
            0,
        )
        .expect_err("February 30th must not be bound as a timestamp");
        assert_eq!(
            sqlstate_of_err(&err),
            crate::types::sql_state::INVALID_DATETIME_FORMAT
        );
    }

    // -----------------------------------------------------------------------
    // SQL-to-C conversions for the temporal types.
    //
    // These walk the spec's SQL-to-C conversion table rather than the pairs a
    // particular backend happens to emit. That distinction matters here: a
    // temporal value read as SQL_C_CHAR or SQL_C_WCHAR takes the
    // string-coercion catch-all and never reaches a struct target, so a suite
    // built from what a backend produces can cover every temporal type and
    // still leave the whole struct half of the table unexercised.
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

    /// The day number [`civil_from_days`] maps to 1 January of `year`.
    ///
    /// `civil_from_days` is monotonic, so a binary search inverts it. That is
    /// the point: a second forward implementation of the calendar would be a
    /// second thing to get wrong, and the test below is about the two existing
    /// ones agreeing.
    fn january_first(year: i64) -> i64 {
        let (mut lo, mut hi) = (-800_000_i64, 800_000_i64);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if civil_from_days(mid) < (year, 1, 1) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    #[test]
    fn days_in_month_agrees_with_civil_from_days() {
        // `is_leap_year`'s doc claims it implements the same calendar
        // `civil_from_days` does. This makes that a checked fact rather than a
        // stated one, so an edit to either side cannot silently break the
        // agreement the comment relies on.
        //
        // The years are the ones where the two could differ: both century
        // rules (1900 not a leap year, 1600 and 2000 leap), an ordinary leap
        // year and its neighbour, year 0 — which is divisible by 400 and
        // therefore leap in the proleptic Gregorian calendar both sides use —
        // and 2100, the next century non-leap year.
        for year in [0_i64, 1600, 1700, 1900, 1996, 2000, 2023, 2024, 2100] {
            let year_i32 = i32::try_from(year).expect("year fits i32");
            let start = january_first(year);
            let end = january_first(year + 1);

            let expected_length = if is_leap_year(year_i32) { 366 } else { 365 };
            assert_eq!(end - start, expected_length, "length of year {year}");

            for days in start..end {
                let (y, month, day) = civil_from_days(days);
                assert_eq!(y, year, "day {days} should fall in {year}");
                let length = days_in_month(year_i32, month);
                assert!(day <= length, "{year}-{month}-{day} exceeds {length}");
                // The last day of a month is the one the next day rolls over.
                let (_, next_month, _) = civil_from_days(days + 1);
                if next_month != month {
                    assert_eq!(day, length, "last day of {year}-{month}");
                }
            }
        }
    }

    #[test]
    fn civil_from_days_handles_a_pre_epoch_clock() {
        // `current_utc_date` cannot be pointed at a fake clock without a clock
        // abstraction, but the part worth testing is the arithmetic rather than
        // the read. `duration_since(UNIX_EPOCH)` fails for a clock set before
        // 1970 and carries the distance backwards in the error, so that branch
        // produces negative day counts; these are the ones it can reach.
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
