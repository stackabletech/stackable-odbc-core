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
/// - `numeric`: the ARD's precision and scale, read only by `SQL_C_NUMERIC`
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
    numeric: NumericTarget,
) -> Result<SqlReturn, OdbcError> {
    unsafe {
        write_column_value_at(
            value,
            target_type,
            target_ptr,
            buf_len,
            len_ind_ptr,
            0,
            numeric,
        )
    }
    .map(|w| w.ret)
}

/// The ARD's `SQL_DESC_PRECISION` and `SQL_DESC_SCALE` for the column being
/// written, which only `SQL_C_NUMERIC` reads.
///
/// The *SQL to C: Numeric* page is explicit that an application controls a
/// `SQL_NUMERIC_STRUCT`'s precision and scale through the descriptor:
/// "**SQLSetDescField** is required to perform manual binding with
/// SQL_C_NUMERIC values". So the conversion cannot be done from the
/// [`ColumnValue`] alone.
///
/// A struct rather than two `i16` arguments, for the reason
/// `SQLForeignKeys`' argument list is a standing warning about in AGENTS.md:
/// two adjacent same-typed parameters can be crossed at a call site and still
/// compile, and crossing these two produces a struct describing a different
/// number.
///
/// [`NumericTarget::UNSPECIFIED`] is what every caller but the bound-column
/// loop and `SQLGetData` passes, and what an ARD record that was never given
/// these fields yields. Zero is not a legal `SQL_NUMERIC_STRUCT` precision, so
/// it reads as "the application did not say" and the conversion derives both
/// from the value itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NumericTarget {
    /// `SQL_DESC_PRECISION`. `0` means unspecified.
    pub precision: i16,
    /// `SQL_DESC_SCALE`.
    pub scale: i16,
}

impl NumericTarget {
    /// The application declared neither field; derive both from the value.
    pub const UNSPECIFIED: Self = Self {
        precision: 0,
        scale: 0,
    };
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

    /// Units delivered by this call: UTF-16 code units for `SQL_C_WCHAR`,
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
/// spread across the fixed-width arms below, none of which can chunk.
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
    numeric: NumericTarget,
) -> Result<ChunkWrite, OdbcError> {
    unsafe {
        write_fixed_or_chunked(
            value,
            target_type,
            target_ptr,
            buf_len,
            len_ind_ptr,
            offset,
            numeric,
        )
    }
}

/// A chunkable value, converted once, for a `SQLGetData` read to drain in parts.
///
/// # Why this exists
///
/// `SQLGetData` is called repeatedly for one column, each call delivering the
/// next part. Asking the backend for the value and converting it again on every
/// call would drain an N-byte column through a K-byte buffer at O(N²/K): 128
/// materialisations of 64 KiB to deliver 64 KiB through the 512-byte buffer a
/// driver manager may pick. The chunk size is the application's own buffer, so
/// nothing it could do would avoid the amplification.
///
/// The variant records the shape the target C type needs, and the C type it was
/// built for is stored beside it, because an application may legally change
/// target type between parts, and that invalidates the conversion rather than
/// only the offset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CachedChunkSource {
    /// UTF-16 code units, for `SQL_C_WCHAR`.
    Utf16(Vec<u16>),
    /// Bytes: UTF-8 for `SQL_C_CHAR`, the value's own bytes for `SQL_C_BINARY`.
    /// Which writer applies is decided by the stored C type, since the character
    /// one reserves a null terminator and the binary one does not.
    Bytes(Vec<u8>),
}

/// The chunk source for a value whose string or byte form **is** the value, or
/// `None` for every other combination.
///
/// Narrow on purpose. `ColumnValue::String` and `ColumnValue::Bytes` are the
/// variants a data source makes long enough to chunk (a LOB), and they are the
/// two whose conversion is a borrow rather than a rendering. Everything else
/// keeps the uncached path unchanged, which matters for one reason beyond
/// caution: [`check_whole_digits_fit`] must be re-evaluated per call, because it
/// reads `buf_len`, and it applies only to numeric sources. Returning `None`
/// here for those keeps that check where it was.
pub(crate) fn cacheable_chunk_source(
    value: &ColumnValue,
    target_type: CDataType,
) -> Option<CachedChunkSource> {
    match (value, target_type) {
        (ColumnValue::String(s), CDataType::WChar) => {
            let mut wide = Vec::with_capacity(s.len());
            wide.extend(s.encode_utf16());
            Some(CachedChunkSource::Utf16(wide))
        }
        (ColumnValue::String(s), CDataType::Char) => {
            Some(CachedChunkSource::Bytes(s.as_bytes().to_vec()))
        }
        (ColumnValue::Bytes(b), CDataType::Binary) => Some(CachedChunkSource::Bytes(b.clone())),
        _ => None,
    }
}

/// Write one chunk from an already-converted source.
///
/// The same three writers the uncached path uses, entered past their conversion
/// step, so the chunking contract is one implementation rather than two: the
/// indicator reporting bytes *remaining*, the terminator, and the
/// `SUCCESS_WITH_INFO` that marks "more to come".
///
/// # Safety
///
/// Same as [`write_column_value_at`]: `target_ptr` must be null or writable for
/// `buf_len` bytes, and `len_ind_ptr` null or a writable `isize`.
pub(crate) unsafe fn write_cached_chunk(
    source: &CachedChunkSource,
    target_type: CDataType,
    target_ptr: *mut c_void,
    buf_len: isize,
    len_ind_ptr: *mut isize,
    offset: usize,
) -> Result<ChunkWrite, OdbcError> {
    let (ret, delivered) = unsafe {
        match (source, target_type) {
            (CachedChunkSource::Utf16(units), _) => {
                write_wchar_units(units, target_ptr, buf_len, len_ind_ptr, offset)?
            }
            (CachedChunkSource::Bytes(bytes), CDataType::Binary) => {
                write_binary(bytes, target_ptr, buf_len, len_ind_ptr, offset)?
            }
            (CachedChunkSource::Bytes(bytes), _) => {
                write_char_bytes(bytes, target_ptr, buf_len, len_ind_ptr, offset)?
            }
        }
    };
    Ok(ChunkWrite {
        ret,
        delivered,
        chunkable: true,
    })
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
    numeric: NumericTarget,
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
            // No SQL_C_TYPE_TIMESTAMP_TZ in ODBC, so map to TypeTimestamp
            // (the offset is dropped).
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
        // the application's buffer: 16 bytes of `Timestamp` into the four an
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
            write_fixed_or_chunked(
                value,
                inferred,
                target_ptr,
                buf_len,
                len_ind_ptr,
                offset,
                numeric,
            )
        };
    }

    // The three chunkable targets are handled here, ahead of the coercion match,
    // because every character and binary conversion below funnelled into these
    // same three writers, differing only in how they produce the string or
    // byte form. Resuming at `offset` therefore belongs in one place rather
    // than in each arm, and no fixed-width arm can chunk at all.
    //
    // For `ColumnValue::String` the string form *is* the value, which is why
    // this borrows instead of going through `column_value_to_string` (that
    // returns `s.clone()` for the `String` variant, so the two agree).
    match target_type {
        CDataType::WChar | CDataType::Char => {
            let owned;
            let s: &str = match value {
                // Both variants hold the string form already, so this borrows
                // instead of going through `column_value_to_string`, which
                // returns `s.clone()` for each of them, so the two agree. That
                // clone is a full copy of the value on every call, and for
                // `Decimal` that is every numeric column an application binds
                // as character data.
                ColumnValue::String(s) | ColumnValue::Decimal(s) => s,
                _ => {
                    owned = column_value_to_string(value);
                    &owned
                }
            };
            check_whole_digits_fit(value, s, target_type, target_ptr, buf_len)?;
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
        // `SQL_C_NUMERIC` sits here, beside the other targets that need the
        // value's *text* rather than a numeric pivot, and ahead of the coercion
        // match for the same reason they are: it reads the rendered decimal.
        // Not chunkable: `SQL_NUMERIC_STRUCT` is fixed-width.
        CDataType::Numeric => {
            return unsafe { write_numeric(value, target_ptr, len_ind_ptr, numeric) }.map(whole);
        }
        // The *SQL to C: GUID* table's own row, and the only row that table
        // gives for this C type: test "None", data written, indicator 16, no
        // SQLSTATE. There is no failure case.
        //
        // Only `ColumnValue::Guid` reaches it. `SQL_C_GUID` appears in exactly
        // one conversion table, whose single source type is `SQL_GUID`; the
        // *SQL to C: Character* table has no `SQL_C_GUID` row, so a character
        // column read as `SQL_C_GUID` is not a defined conversion and falls
        // through to the `07006` the overview page prescribes for "an
        // identifier for an ODBC C data type not shown in the table for a given
        // ODBC SQL data type". Adding a text-parsing arm here with a `22018`
        // for a bad parse would be inventing a cell the spec does not have.
        CDataType::Guid => {
            if let ColumnValue::Guid(bytes) = value {
                // `SQLGUID`'s first three groups are integers whose textual
                // form is the big-endian reading of the bytes, the same order
                // `column_value_to_string` renders, where `data[0]` is the
                // leading digit pair. Reading them natively would byte-swap the
                // GUID on every little-endian machine, silently.
                let out = odbc_sys::Guid {
                    d1: u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
                    d2: u16::from_be_bytes([bytes[4], bytes[5]]),
                    d3: u16::from_be_bytes([bytes[6], bytes[7]]),
                    d4: [
                        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14],
                        bytes[15],
                    ],
                };
                return unsafe { write_fixed(target_ptr, len_ind_ptr, out) }.map(whole);
            }
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
        // portion is zero": data written, no SQLSTATE. "a valid
        // timestamp-value; time portion is nonzero": truncated data written
        // with 01S07, footnote [c]: "The time portion of the timestamp-value is
        // truncated." Anything else is the row's 22018 with nothing written, or
        // this module's 22007, which `parse_sql_timestamp`'s `?` propagates
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
        // seconds portion is zero": data written, no SQLSTATE, footnote [d]:
        // "The date portion of the timestamp-value is ignored", so a discarded
        // date is not a truncation and reports nothing. "a valid
        // timestamp-value; fractional seconds portion is nonzero": truncated
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
                // that test. The newer
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
        // zero": both are what `parse_sql_timestamp` already produces. "a
        // valid time-value", footnote [g]: "The date fields of the timestamp
        // structure are set to the current date", which is the branch below.
        // Anything else is 22018 with nothing written.
        //
        // Footnote [g] speaks only of the date fields, and the row above it
        // makes a time-value's fractional seconds something that can be
        // "truncated", so the literal's fraction is carried into the target's
        // own fraction field rather than zeroed. That is the opposite of the
        // typed `ColumnValue::Time` arm below, whose row (SQL to C: Time) says
        // in as many words that the fraction is set to zero. Two source types,
        // two tables, two answers.
        //
        // Known limitation, recorded rather than fixed: the "fractional seconds
        // portion truncated" row's 01S07 is not reported. `parse_time_fields`
        // truncates a literal carrying more than nine fractional digits to
        // nanoseconds silently, on this path and on the timestamp-value path
        // alike, so the two cannot disagree and the data still arrives; only
        // the warning is missing. This is a ruling rather than an open
        // intention: reporting it means deciding what a driver should do when
        // the two paths disagree about precision, which is a larger question
        // than the warning itself.
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
        // or, for text bound for an integer target, exact decimal digits, none of which
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
            Ok(pivot) => unsafe {
                write_numeric_pivot(pivot, target_type, target_ptr, len_ind_ptr)
            },
            // Text that should have been numeric but was not parseable.
            Err(NumericPivotError::NotNumericLiteral) => Err(OdbcError::general(
                format!("Invalid character value for cast: {value:?}"),
                SqlState::invalid_character_value_for_cast(),
            )),
            // Numeric text of a magnitude no C target holds. Nothing is written
            // and the length indicator is left alone, which is what the two
            // "Undefined" cells of that row require.
            Err(NumericPivotError::OutOfRange) => Err(OdbcError::general(
                format!(
                    "Numeric value out of range: {value:?} exceeds the range of {target_type:?}"
                ),
                SqlState::numeric_value_out_of_range(),
            )),
            // The column value's type has no defined conversion to the
            // requested C type (e.g. a Bytes/Guid/structured value asked
            // to become a numeric target). Spec 07006: "The data value of
            // a column in the result set could not be converted to the
            // data type specified by the TargetType argument."
            Err(NumericPivotError::NotNumericType) => Err(OdbcError::general(
                format!("Unsupported conversion from {value:?} to {target_type:?}"),
                SqlState::restricted_data_type_attribute_violation(),
            )),
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
        // zero." No SQLSTATE, because nothing is lost.
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
        // reported here, unlike the SQL_C_TYPE_TIME row above, where the target
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
        // is on the *fractional seconds* alone: a discarded date is not a
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

        // The `Binary`, `WChar` and `Char` catch-alls sit ahead of this match,
        // where the chunking offset is applied, so every target reaching here
        // is fixed-width.

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
/// would be to substitute one, and a wrong date presented as correct is worse
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
/// Covers only the types that inference can produce. A wider match would invite
/// the impression that this is a general size table for `CDataType`, which it is
/// not: it exists solely to bound the one path where the driver, not the
/// application, picks the C type.
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
// in this module. stackable-odbc-core has no numeric datetime encodings to
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
/// Divisible by 4, except centuries, except every fourth century, so 2000 is a
/// leap year and 1900 is not. The same calendar [`civil_from_days`] implements,
/// which is what keeps this module's two date computations from disagreeing.
/// `days_in_month_agrees_with_civil_from_days` checks that rather than leaving
/// it to this sentence, by walking every day of nine chosen years through
/// both.
///
/// `%` is correct for a negative year here because every arm compares against
/// zero, and `-100 % 100` is 0 in Rust as it is in mathematics. No negative year
/// reaches this function, because [`parse_date_fields`] splits on `-` and a
/// leading minus sign therefore produces a fourth part that is refused as
/// malformed. The rule is written to be right either way rather than to depend
/// on that.
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
    // "Data value is not a valid date-value or timestamp-value", from SQL to
    // C: Character. ODBC's grammar says only `days-value ::= digit digit`, so what
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
/// 01S07 if it is non-zero. This function only parses; it does not decide
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
// Helper: the SQL to C: Numeric whole-digit rule for character targets
// ---------------------------------------------------------------------------

/// Whether the *SQL to C: Numeric* table governs this value's conversion to a
/// character C type.
///
/// The table's own list of numeric SQL types is SQL_DECIMAL, SQL_NUMERIC,
/// SQL_TINYINT, SQL_SMALLINT, SQL_INTEGER, SQL_BIGINT, SQL_REAL, SQL_FLOAT and
/// SQL_DOUBLE, which is exactly the seven variants below and no others.
///
/// The other fifteen variants are excluded. Enumerated in full, because a
/// variant quietly missing from this list is a silently wrong number:
///
/// - `Bool` is SQL_BIT (*SQL to C: Bit*), whose character row is
///   "*BufferLength* > 1" / "*BufferLength* <= 1" → 22003. A different test,
///   not implemented here; `a_bool_takes_the_bit_table_not_the_numeric_one`
///   pins the current 01004 so the omission is visible rather than assumed.
///
///   TODO(spec): implement *SQL to C: Bit*'s "*BufferLength* <= 1" → 22003 row
///   for SQL_C_CHAR and SQL_C_WCHAR. Core returns 01004 today.
/// - `Date`, `Time`, `Timestamp` and `TimestampTz` are *SQL to C: Date* /
///   *Time* / *Timestamp*, whose character rows carry a 22003 of their own
///   keyed to a fixed minimum width, not to a digit count.
///
///   TODO(spec): implement those minimum-width 22003 rows, "*BufferLength* <
///   20" for a timestamp and the analogous widths on the date and time pages.
///   Core returns 01004 for all of them today.
/// - `String`, `Json`, `Bytes`, `Guid`, `Array`, `Map`, `Row` and the two
///   interval variants (`IntervalYearMonth`, `IntervalDayTime`) are not numeric
///   at all. *SQL to C: Character* has no 22003 row for SQL_C_CHAR whatsoever,
///   so a character column that does not fit stays an ordinary 01004.
/// - `Null` never reaches here: `write_fixed_or_chunked` answers it with
///   `SQL_NULL_DATA` before the target type is examined. Listed for
///   completeness so the fifteen add up rather than leaving a reader to check.
const fn is_numeric_source(value: &ColumnValue) -> bool {
    matches!(
        value,
        ColumnValue::I8(_)
            | ColumnValue::I16(_)
            | ColumnValue::I32(_)
            | ColumnValue::I64(_)
            | ColumnValue::F32(_)
            | ColumnValue::F64(_)
            | ColumnValue::Decimal(_)
    )
}

/// The part of a rendered number that cannot be given up: everything before the
/// decimal point.
///
/// A leading `-` is included. The table says "digits" and a sign is not one,
/// but the boundary the table draws is `>=`, which is precisely "the whole part
/// plus the null terminator must fit", and a sign occupies a byte of the
/// application's buffer exactly as a digit does. Excluding it would deliver
/// `-12` for `-123.45` in a four-byte buffer under the 01004 row: a different
/// number, which is the outcome this row exists to prevent. Pinned in both
/// directions by `a_minus_sign_occupies_a_whole_digit_position`.
///
/// This is also the reading the crate already takes in the opposite direction:
/// `numeric_convert`'s C-to-SQL size check counts the sign against the declared
/// `ColumnSize`, pinned by `the_minus_sign_counts_toward_the_declared_size`. The
/// two tables word their limits differently, so this is corroboration rather
/// than a shared rule, but the fetch and bind paths agreeing on what a sign
/// costs is worth more than either wording.
///
/// A rendering with no decimal point is all whole part, which is what makes
/// `Infinity` and `NaN` fall out correctly with no special case: they have no
/// fraction to sacrifice, so any truncation at all is whole-part loss.
///
/// Both of the odd renderings this can meet come from a backend-supplied
/// `ColumnValue::Decimal`, since core produces neither itself:
///
/// - **Exponent notation is not decomposed.** `1.5E10` counts one whole digit
///   rather than eleven. This *under*-counts, which is the safe direction: such
///   a value gets the ordinary 01004 rather than a false 22003. Rust's
///   `Display` for `f32`/`f64` never uses exponent form, so it cannot arrive
///   from the float variants.
/// - **A leading space or `+` counts as a whole-part position**, and that is
///   correct rather than an over-count, because core writes a `Decimal`'s text
///   through verbatim: the character occupies a byte of the application's
///   buffer exactly as the sign does. `" 123.45"` needs five bytes for `" 123"`
///   and its terminator, and gets them. The two halves agree, so what is
///   reserved is what is written, and that is the property that would break if
///   this trimmed.
fn whole_part(rendered: &str) -> &str {
    match rendered.find('.') {
        Some(point) => &rendered[..point],
        None => rendered,
    }
}

/// Enforces the *SQL to C: Numeric* table's third character row.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/appendixes/sql-to-c-numeric>
///
/// The SQL_C_CHAR row's three outcomes, in the order the table lists them:
///
/// | Test | \**TargetValuePtr* | \**StrLen_or_IndPtr* | SQLSTATE |
/// |---|---|---|---|
/// | Character byte length < *BufferLength* | Data | Length of data in bytes | n/a |
/// | Number of whole (as opposed to fractional) digits < *BufferLength* | Truncated data | Length of data in bytes | 01004 |
/// | Number of whole (as opposed to fractional) digits >= *BufferLength* | Undefined | Undefined | 22003 |
///
/// SQL_C_WCHAR states the same three tests, with "Character length" and
/// "Length of data in characters" in place of the byte forms.
///
/// This function answers only the third row; the first two are the ordinary
/// truncation the character writers already implement. Both output locations
/// are "Undefined" on that row, so it returns before either writer runs and
/// **nothing at all is written: not the data, not the indicator**.
/// `assert_22003_writes_nothing` checks both with sentinels.
///
/// # Who reaches this
///
/// `write_column_value` has three call sites, and all three inherit this row:
///
/// - `sql_fetch`'s bound-column loop (`ffi/fetch.rs`), so `SQLFetch` and
///   `SQLFetchScroll`.
/// - `sql_get_data` (`ffi/fetch.rs`), via `write_column_value_at`.
/// - `write_output_params` (`ffi/params.rs`), so a numeric **output parameter**
///   too, reached from `sql_exec_direct_w` and `sql_execute`, and not from
///   `sql_param_data`, which executes without writing output parameters back.
///   That site is the sharpest: it discards the `SqlReturn` (it has no
///   diagnostic queue to raise `01004` on) but propagates the `Err` with `?`,
///   so a truncation there is a failed execution rather than a warning. It
///   cannot hit the no-buffer exemption spuriously, because the loop skips
///   records where `DescriptorRecord::is_bound` is false and the data pointer
///   is therefore non-null by the time this runs.
///
/// Two points about the arithmetic:
///
/// - For SQL_C_CHAR the condition below is `buf_len < whole + 1`, which is
///   `whole >= buf_len`, the table's `>=` exactly, and readable as "the whole
///   part and its null terminator must both fit".
/// - For SQL_C_WCHAR the row's "Number of whole ... digits" is a character
///   count while `BufferLength` is a byte count on the wire, so the same
///   condition is scaled by two. That agrees with `write_wchar`'s own
///   `capacity_units = buf_len / 2 - 1`, so the two cannot disagree about
///   where the buffer ends.
///
/// # A call that supplies no buffer is exempt
///
/// The row is not applied when the writer would write nothing anyway: a null
/// `target_ptr`, or a `buf_len` with no room for even the null terminator
/// (`<= 0` for SQL_C_CHAR, `< 2` for SQL_C_WCHAR). Those are exactly the
/// early-return conditions `write_char` and `write_wchar` already have, so this
/// is one rule stated in two places rather than a second rule.
///
/// **The reason is what the row is for.** A wrong *number* reaching the
/// application's buffer is the harm 22003 prevents; where there is no buffer,
/// that harm cannot occur, so literalism buys nothing and costs two idioms the
/// spec sanctions:
///
/// - **The zero-length length probe.** `BufferLength` 0 with a real pointer,
///   the documented "how much room do I need" call, which `SQLGetData`'s own
///   prose protects by returning `HY090` when `BufferLength` is less than 0
///   *but not when it is 0*. Pinned by
///   `a_zero_length_buffer_on_a_numeric_column_stays_a_length_probe`.
/// - **The indicator-only binding.** `SQLBindCol` with a null data pointer and
///   a live length/indicator pointer, which the spec permits in as many words
///   ("An application can unbind the data buffer for a column but still have a
///   length/indicator buffer bound for the column") and which
///   `collect_bindings` deliberately keeps. Pinned by
///   `fetch_writes_the_indicator_of_an_indicator_only_numeric_binding`.
///
/// Both reference drivers agree: psqlODBC's `setup_getdataclass`
/// (`convert.c`) branches on `cbValueMax == 0` with the comment "just returns
/// length info", and MySQL Connector/ODBC does the same in `utility.cc`.
///
/// **`buf_len == 1` on SQL_C_CHAR is *not* exempt.** There is a buffer there,
/// and the writer would put a bare null terminator in it, delivering `""` for
/// a number, which is the wrong number and exactly the harm above.
/// `a_one_byte_char_buffer_is_still_22003` pins that edge.
///
/// The check is independent of the `SQLGetData` chunk offset by construction,
/// since it reads the whole rendered value rather than the not-yet-delivered
/// remainder, so a value that passes on the first chunk passes on every later
/// one. The consequence is that a rendering longer than the buffer cannot be
/// retrieved in parts at all: a `DECIMAL(38,0)` is 39 characters, and a 32-byte
/// buffer answers 22003 where both reference drivers would deliver it in
/// chunks. That is spec-defensible, since the numeric types are absent from
/// `SQLGetData`'s "Retrieving Variable-Length Data in Parts" list, and
/// `sql_get_data` does
/// not advance the cursor on the `Err` path, so the column stays readable with
/// a buffer that fits.
fn check_whole_digits_fit(
    value: &ColumnValue,
    rendered: &str,
    target_type: CDataType,
    target_ptr: *mut c_void,
    buf_len: isize,
) -> Result<(), OdbcError> {
    if !is_numeric_source(value) {
        return Ok(());
    }

    // The "no buffer at all" carve-out above. These two conditions mirror
    // `write_char`'s `buf_len <= 0` and `write_wchar`'s `buf_len < 2`.
    let minimum_useful_buf_len = if target_type == CDataType::WChar {
        2
    } else {
        1
    };
    if target_ptr.is_null() || buf_len < minimum_useful_buf_len {
        return Ok(());
    }

    let whole = whole_part(rendered);
    let (units, bytes_per_unit) = if target_type == CDataType::WChar {
        (whole.encode_utf16().count(), 2_isize)
    } else {
        (whole.len(), 1_isize)
    };

    let needed = isize::try_from(units)
        .unwrap_or(isize::MAX)
        .saturating_add(1)
        .saturating_mul(bytes_per_unit);

    if buf_len < needed {
        return Err(OdbcError::general(
            format!(
                "Numeric value out of range: {rendered} needs {needed} bytes for its whole part \
                 and null terminator as {target_type:?}, but the application supplied a \
                 {buf_len}-byte buffer"
            ),
            SqlState::numeric_value_out_of_range(),
        ));
    }
    Ok(())
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
    // Encoded straight into the caller's buffer: no intermediate `Vec`.
    //
    // A `Vec` here would allocate on every bound-column fetch of every
    // character column, since `sql_fetch`'s loop calls this once per column per
    // row, and the units would be built only to be copied out and dropped. The
    // chunked path keeps a materialised copy on purpose (see
    // [`CachedChunkSource`]) and enters at [`write_wchar_units`] instead.
    //
    // Two passes over the string, still allocation-free. The first counts, and
    // cannot be avoided: the indicator must report the total length *remaining*,
    // which is a property of the whole string and not of the part that fits.
    let total_units = s.encode_utf16().count();
    let remaining_units = total_units.saturating_sub(offset);
    let total_bytes = (remaining_units * 2) as isize;

    if !len_ind_ptr.is_null() {
        unsafe { std::ptr::write_unaligned(len_ind_ptr, total_bytes) };
    }

    // Both carve-outs are `write_wchar_units`', for the reasons documented
    // there: a null target is the bound-column indicator-only binding, and a
    // buffer with no room for even the terminator is total truncation rather
    // than a length query.
    if target_ptr.is_null() {
        return Ok((SqlReturn::SUCCESS, 0));
    }
    if buf_len < 2 {
        return Ok((SqlReturn::SUCCESS_WITH_INFO, 0));
    }

    let capacity_units = ((buf_len as usize) / 2).saturating_sub(1);
    let out = target_ptr.cast::<u16>();
    let mut written = 0usize;
    // Per-unit `write_unaligned`: an application's buffer may sit at any offset
    // in a packed row-wise binding, so a `u16`-aligned store would be UB.
    for unit in s.encode_utf16().skip(offset).take(capacity_units) {
        unsafe { std::ptr::write_unaligned(out.add(written), unit) };
        written += 1;
    }
    unsafe { std::ptr::write_unaligned(out.add(written), 0u16) };

    if written < remaining_units {
        Ok((SqlReturn::SUCCESS_WITH_INFO, written))
    } else {
        Ok((SqlReturn::SUCCESS, written))
    }
}

/// [`write_wchar`] from the point the UTF-16 units exist.
///
/// Split out so a chunked read can encode once and write many times: encoding
/// inside the per-chunk path would make draining an N-unit column in K-unit
/// parts cost O(N²/K). See [`CachedChunkSource`].
unsafe fn write_wchar_units(
    wide: &[u16],
    target_ptr: *mut c_void,
    buf_len: isize,
    len_ind_ptr: *mut isize,
    offset: usize,
) -> Result<(SqlReturn, usize), OdbcError> {
    // An offset past the end yields an empty remainder rather than panicking:
    // the caller stops at `done`, but a truncating write that lands exactly on
    // the end would otherwise index one past it.
    let remaining = wide.get(offset.min(wide.len())..).unwrap_or(&[]);
    let total_bytes = (remaining.len() * 2) as isize;

    // Always report the byte length still to come.
    if !len_ind_ptr.is_null() {
        unsafe { std::ptr::write_unaligned(len_ind_ptr, total_bytes) };
    }

    // A null target is not something SQLGetData's own spec sanctions: its
    // Arguments section is explicit that "TargetValuePtr cannot be NULL."
    // The case that actually reaches this branch comes from this function's
    // *other* caller: `sql_fetch`'s bound-column loop (`ffi/fetch.rs`)
    // legitimately passes a null data pointer when `SQL_DESC_DATA_PTR` is
    // null but `SQL_DESC_INDICATOR_PTR` is not. That is the indicator-only binding
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

    // A non-null target with fewer than two bytes of room, including exactly
    // zero (the standard "how big a buffer do I need" probe), cannot hold even
    // the one-UTF-16-code-unit null terminator. That is total truncation, not a
    // length query: the application supplied somewhere to write and nothing was
    // written there. Spec: "If the data buffer supplied is too small to hold
    // the null-termination character, SQLGetData returns SQL_SUCCESS_WITH_INFO
    // and SQLSTATE 01004." Reporting plain SUCCESS here, as a shared branch
    // with the null-target case above, would make SQLGetData indistinguishable
    // from "this column is fully delivered" and permanently strand the data
    // behind a `buf_len == 0` probe: `cursor.done` is derived from this return
    // value.
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
/// "fixed" by backing up to a character boundary, which would deliver fewer
/// bytes than the buffer holds and, on a buffer smaller than one character,
/// would deliver nothing and never terminate.
unsafe fn write_char(
    s: &str,
    target_ptr: *mut c_void,
    buf_len: isize,
    len_ind_ptr: *mut isize,
    offset: usize,
) -> Result<(SqlReturn, usize), OdbcError> {
    unsafe { write_char_bytes(s.as_bytes(), target_ptr, buf_len, len_ind_ptr, offset) }
}

/// [`write_char`] over bytes that are already UTF-8.
///
/// The character path needs no re-encoding, so this exists for the same reason
/// [`write_wchar_units`] does: a cached chunk source hands over bytes, not a
/// `&str`, and `s.as_bytes()` is the only thing `write_char` ever wanted.
unsafe fn write_char_bytes(
    all: &[u8],
    target_ptr: *mut c_void,
    buf_len: isize,
    len_ind_ptr: *mut isize,
    offset: usize,
) -> Result<(SqlReturn, usize), OdbcError> {
    let bytes = all.get(offset.min(all.len())..).unwrap_or(&[]);
    let total_bytes = bytes.len() as isize;

    if !len_ind_ptr.is_null() {
        unsafe { std::ptr::write_unaligned(len_ind_ptr, total_bytes) };
    }

    // A null target here is the bound-column caller's indicator-only
    // binding, not something SQLGetData's own spec permits; see
    // `write_wchar`'s full reasoning.
    if target_ptr.is_null() {
        return Ok((SqlReturn::SUCCESS, 0));
    }

    // A non-null target with no room in it, including exactly zero (the
    // standard length-probe), cannot hold even the one-byte null terminator,
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
    // binding, not something SQLGetData's own spec permits; see
    // `write_wchar`'s full reasoning.
    if target_ptr.is_null() {
        return Ok((SqlReturn::SUCCESS, 0));
    }

    // A non-null target with no room in it, including exactly zero (the
    // standard length-probe), cannot hold any of the data, which is total
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
/// that is not a *numeric-literal* at all, meaning `inf` and `NaN`, which
/// [`parse_numeric_literal`] rejects and Rust's float parser accepts.
///
/// **A literal whose magnitude no `f64` holds is [`NumericPivotError::OutOfRange`]
/// here, not an infinity downstream.** Rust's parser saturates `"1e400"` to
/// `f64::INFINITY`, and every caller below this point sees a legitimate
/// infinity, the value a `'Infinity'::float8` column really holds, so the
/// overflow has to be caught at the parse or not at all. *SQL to C:
/// Character*'s row for
/// `SQL_C_FLOAT`/`SQL_C_DOUBLE` gives it the second of its three cells: "outside
/// the range of the data type to which the number is being converted" →
/// *Undefined* / `22003`.
///
/// The discriminator is **whether the text contains a digit**, which needs a word
/// because the obvious spelling is wrong twice over. Testing the parsed value
/// alone cannot work: an overflowing literal and the text `"Infinity"` produce
/// the same `f64`, and `the_infinity_spelling_parses_back_into_a_float` pins that
/// the second must survive. Testing `parse_numeric_literal(t).is_some()` is
/// closer but leaks: it parses the exponent as an `i32`, so `"1e99999999999999"`
/// is `None` there and would slip through as an infinity. Among the strings
/// Rust's float parser accepts at all, the only digitless ones are the
/// `inf`/`infinity`/`nan` spellings, so a digit is exactly the line between
/// "a numeric-literal that overflowed" and "the source said infinity".
///
/// Underflow is not out of range: `"1e-400"` parses to `0.0`, and zero is a
/// value the target holds, so it is the row's *first* cell. The same reading
/// the `F64` → `f32` narrowing takes.
fn parse_numeric_text(
    s: &str,
    target_type: CDataType,
) -> Result<NumericPivot<'_>, NumericPivotError> {
    let t = s.trim();
    if is_exact_integer_target(target_type)
        && let Some(literal) = parse_numeric_literal(t)
    {
        return Ok(NumericPivot::Exact { literal, text: t });
    }
    if let Ok(i) = t.parse::<i64>() {
        return Ok(NumericPivot::Int(i));
    }
    match t.parse::<f64>() {
        Ok(f) if f.is_infinite() && t.bytes().any(|b| b.is_ascii_digit()) => {
            Err(NumericPivotError::OutOfRange)
        }
        Ok(f) => Ok(NumericPivot::Float(f)),
        Err(_) => Err(NumericPivotError::NotNumericLiteral),
    }
}

/// Why a [`ColumnValue`] has no [`NumericPivot`] reading, and therefore which
/// SQLSTATE the numeric arm of [`write_column_value`] answers with.
///
/// Three variants rather than a plain `None`, because the two text failures are
/// different cells of the *SQL to C: Character* row. Collapsing them lets an
/// overflowing literal reach the pivot as an infinity and be delivered as a
/// success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NumericPivotError {
    /// The variant has no numeric reading at all: a `Bytes`, a `Guid`, a
    /// structured value. `07006`, and off both tables: neither has a row for a
    /// source type that is not numeric or character in the first place.
    NotNumericType,
    /// Text that is not a *numeric-literal*. The character table's `22018` cell.
    NotNumericLiteral,
    /// Text that *is* a *numeric-literal*, of a magnitude no `f64` holds. The
    /// character table's `22003` cell.
    OutOfRange,
}

/// Map a [`ColumnValue`] to a [`NumericPivot`], or to the [`NumericPivotError`]
/// that says which SQLSTATE the caller answers with.
///
/// This match is intentionally exhaustive (no wildcard) so that adding a new
/// `ColumnValue` variant causes a compile error here, forcing an explicit decision
/// about whether the new type is numeric.
fn column_value_as_numeric(
    value: &ColumnValue,
    target_type: CDataType,
) -> Result<NumericPivot<'_>, NumericPivotError> {
    match value {
        ColumnValue::I8(v) => Ok(NumericPivot::Int(i64::from(*v))),
        ColumnValue::I16(v) => Ok(NumericPivot::Int(i64::from(*v))),
        ColumnValue::I32(v) => Ok(NumericPivot::Int(i64::from(*v))),
        ColumnValue::I64(v) => Ok(NumericPivot::Int(*v)),
        ColumnValue::F32(v) => Ok(NumericPivot::Float(f64::from(*v))),
        ColumnValue::F64(v) => Ok(NumericPivot::Float(*v)),
        ColumnValue::Bool(v) => Ok(NumericPivot::Int(*v as i64)),
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
        | ColumnValue::IntervalDayTime { .. } => Err(NumericPivotError::NotNumericType),
    }
}

/// Write a numeric pivot value into a C buffer for the given target type.
///
/// Handles all signed and unsigned integer C types, `SQL_C_FLOAT`, `SQL_C_DOUBLE`,
/// and `SQL_C_BIT`. Returns `SQL_ERROR` with SQLSTATE `22003` (numeric value out of
/// range) when the value does not fit the target type, including a finite `f64`
/// whose magnitude exceeds `f32::MAX` on its way to `SQL_C_FLOAT`, which the
/// *SQL to C: Numeric* row for that target calls "outside the range of the data
/// type to which the number is being converted". It returns `SQL_SUCCESS_WITH_INFO`
/// with SQLSTATE `01S07` (fractional truncation) in the three cases that drop a
/// *fraction*: a value between 0 and 2 losing its fractional part to reach
/// `SQL_C_BIT`, an exact decimal losing a non-zero fraction to reach an integer
/// target (see [`write_exact_integer`]), and an `f64` losing a non-zero fraction
/// to reach an integer target (see [`write_truncated_float`]).
///
/// **A float target reports no `01S07` at all**, however inexact the narrowing:
/// the *SQL to C: Numeric* row for `SQL_C_FLOAT`/`SQL_C_DOUBLE` has exactly two
/// cells (in range → *Data* / `n/a`, out of range → *Undefined* / `22003`), and
/// the integer row above it and the `SQL_C_BIT` row below it both do carry
/// `01S07`, so the omission is a distinction the table draws. See the
/// `SQL_C_FLOAT` arm for the rest of that argument.
///
/// Any `CDataType` not covered by the numeric arms returns `SQL_ERROR` with
/// SQLSTATE `HY003` (invalid application buffer type).
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
        // An `i64` beyond 2^24 loses precision reaching `f32`, and that is not
        // reported: the row's in-range cell says `n/a`, and every `i64` is inside
        // the range of `f32` (`i64::MAX` is about 9.2e18, `f32::MAX` about
        // 3.4e38), so no `i64` can reach the row's second cell either. This arm
        // therefore has one outcome. See the `Float` → `SQL_C_FLOAT` arm below
        // for why the omitted `01S07` is the table's decision and not an
        // oversight; the two arms answer one question and must answer it alike.
        (NumericPivot::Int(v), CDataType::Float) => unsafe {
            write_fixed(target_ptr, len_ind_ptr, v as f32)
        },
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
        // The SQL_C_FLOAT / SQL_C_DOUBLE row has exactly two outcomes: "Data is
        // within the range of the data type to which the number is being
        // converted" → *Data* / n/a, and "Data is outside the range of the data
        // type to which the number is being converted" → *Undefined* / 22003.
        //
        // A finite f64 beyond ±f32::MAX is the second of those: `as f32`
        // saturates it to ±inf, so writing the result would hand the
        // application an infinity the data source never held and call it a
        // warning. Nothing is written, and the length indicator is left alone
        // too, which is what the row's two "Undefined" cells require.
        //
        // **The `v.is_finite()` half is load-bearing, not defensive.** A source
        // value that really is ±infinity (PostgreSQL's 'Infinity'::float8, say)
        // narrows to ±infinity exactly and is delivered unchanged; testing only
        // `f.is_infinite()` would make that column unreadable through
        // SQL_C_FLOAT. `f64_infinity_to_float_is_the_value_the_source_held`
        // pins it. One thing that half cannot distinguish: character text that
        // *parsed* to an infinity, since `"1e400".parse::<f64>()` is `Ok(inf)`.
        // By the time such a value reaches this arm it is indistinguishable from
        // a column that really holds an infinity. That is why the overflow of a
        // character literal is caught in `parse_numeric_text` instead, which is
        // also where it has to be for the other two reasons: its governing table
        // is *SQL to C: Character* rather than this one, and the same text
        // reaches SQL_C_DOUBLE by a path this arm never sees.
        //
        // Underflow is *not* the second outcome: a subnormal f32, and zero, are
        // values f32 can hold, so they are inside the row's "within the range"
        // cell. `1e-300` therefore writes `0.0` rather than
        // failing, the reading psqlODBC and MySQL Connector/ODBC also take,
        // both of which narrow with a plain C cast and range-check neither end
        // (psqlODBC `convert.c`, `case SQL_C_FLOAT`; MySQL `driver/results.cc`,
        // `sql_get_data`).
        //
        // What is left after the range test is an inexact narrowing, and it is
        // reported as **nothing**: the row's in-range cell is *Data* / `n/a`, and
        // it has no third cell to hold a warning. The rows either side of it do:
        // the integer row's "truncation of fractional digits" and the
        // `SQL_C_BIT` row's "greater than 0, less than 2, and not equal to 1"
        // both carry `01S07`. So the float row's omission is a distinction the
        // table draws rather than a gap to fill, and neither psqlODBC
        // (`convert.c`, `case SQL_C_FLOAT`) nor MySQL Connector/ODBC
        // (`driver/results.cc`, `sql_get_data`) reports anything here.
        //
        // A NaN shows why an equality test between source and narrowed value
        // would be wrong rather than merely unauthorised: no comparison calls a
        // NaN equal to its source, so a faithfully delivered NaN would report a
        // fractional truncation that never happened. It is delivered and
        // nothing is reported. Note the contrast with the `SQL_C_BIT` arm
        // below, which has a range test a NaN fails, where `SQL_C_FLOAT` has no
        // range a NaN is outside of.
        (NumericPivot::Float(v), CDataType::Float) => {
            let f = v as f32;
            if f.is_infinite() && v.is_finite() {
                return Err(OdbcError::general(
                    format!("Numeric value out of range: {v} exceeds the range of SQL_C_FLOAT"),
                    SqlState::numeric_value_out_of_range(),
                ));
            }
            unsafe { write_fixed(target_ptr, len_ind_ptr, f) }
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
/// The governing row is the same one [`write_exact_integer`] cites, [SQL to C:
/// Numeric]'s row for `SQL_C_STINYINT`, `SQL_C_UTINYINT`, `SQL_C_TINYINT`,
/// `SQL_C_SBIGINT`, `SQL_C_UBIGINT`, `SQL_C_SSHORT`, `SQL_C_USHORT`,
/// `SQL_C_SHORT`, `SQL_C_SLONG`, `SQL_C_ULONG`, `SQL_C_LONG` and
/// `SQL_C_NUMERIC`. That table covers the approximate numeric SQL types
/// (`SQL_REAL`, `SQL_FLOAT`, `SQL_DOUBLE`) alongside the exact ones, since its
/// identifier list names all nine, so a float source gets the same three
/// outcomes. Eight of the row's C types reach here, one per caller in
/// [`write_numeric_pivot`]: `odbc-sys` models the deprecated `SQL_C_TINYINT`,
/// `SQL_C_SHORT` and `SQL_C_LONG` only as commented-out entries, and
/// `SQL_C_NUMERIC` is answered before the pivot is built at all, because it
/// needs the value's *digits* rather than a pivot narrowed to `i64`/`f64`, so it
/// has its own writer, [`write_numeric`], reached from `write_fixed_or_chunked`
/// beside the character targets. It shares this row's three outcomes, which is
/// why they are stated once here. The three outcomes:
///
/// - "Data converted without truncation": `SQL_SUCCESS`.
/// - "Data converted with truncation of fractional digits": the truncated
///   value is written and `01S07` returned.
/// - "Conversion of data would result in loss of whole (as opposed to
///   fractional) digits": `22003`, with `*TargetValuePtr` left alone, which the
///   table's "Undefined" requires.
///
/// **The order matters: truncate first, then range-check the truncated value.**
/// It is whole digits the third outcome protects, so `127.5` into
/// `SQL_C_STINYINT` is the second outcome and writes `127`, and `-0.5` into any
/// unsigned target writes `0`, the same reading [`write_exact_integer`] gives
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
///   target widths are all far below that, so `T::try_from` decides the
///   bound at the target's own width rather than at an `f64`-rounded
///   approximation of it. That is what makes `2^63` reject for `SQL_C_SBIGINT`
///   where `v > i64::MAX as f64` would admit it: `i64::MAX as f64` rounds *up* to
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
/// difference is a fourth row that only the character table has, "Data is not a
/// *numeric-literal*" → `22018`, which a numeric SQL source cannot reach, and
/// which is handled by [`column_value_as_numeric`] returning
/// [`NumericPivotError::NotNumericLiteral`] rather than here.
///
/// The three shared outcomes, in the tables' own order:
///
/// - "Data converted without truncation": `SQL_SUCCESS`, nothing to report.
/// - "Data converted with truncation of fractional digits": the truncated
///   value is written and `01S07` returned. Truncation is toward zero, which is
///   what [`DecimalLiteral::to_integer`] does, so `-3.9` delivers `-3`.
/// - "Conversion of data would result in loss of whole (as opposed to
///   fractional) digits": `22003`, and the range test runs before the write so
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
/// driver renders `Infinity`, and PostgreSQL, the other major data source with
/// infinite floats, emits `Infinity` in its own text output. Rust's `Display`
/// gives `inf`/`-inf`, which is a default rather than a decision.
///
/// The deciding argument is that this is core's *shared* coercion path and a
/// driver cannot override it, so core should not impose a Rust-ism on every
/// backend. `NaN` already agrees between Rust and Java and is left alone, so
/// only the two infinities differ from `Display`.
const fn infinity_text(negative: bool) -> &'static str {
    if negative { "-Infinity" } else { "Infinity" }
}

/// Write a value as a `SQL_NUMERIC_STRUCT` (`SQL_C_NUMERIC`).
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/appendixes/sql-to-c-numeric>
///
/// `SQL_C_NUMERIC` shares the *SQL to C: Numeric* table's exact-integer row with
/// `SQL_C_SLONG` and `SQL_C_SBIGINT`, so it has that row's three outcomes:
///
/// | Test | \**TargetValuePtr* | \**StrLen_or_IndPtr* | SQLSTATE |
/// |---|---|---|---|
/// | Data converted without truncation | Data | Size of the C data type | n/a |
/// | Data converted with truncation of fractional digits | Truncated data | Size of the C data type | `01S07` |
/// | Conversion would lose whole digits | Undefined | Undefined | `22003` |
///
/// Footnote [a] applies to all three: "The value of *BufferLength* is ignored
/// for this conversion. The driver assumes that the size of \**TargetValuePtr*
/// is the size of the C data type." So `buf_len` is not a parameter here.
///
/// This target is not optional. The overview page: drivers "are required to
/// support conversions to all ODBC C data types from the ODBC SQL data types
/// that they support".
///
/// # Precision and scale
///
/// The struct's own `precision` and `scale` fields describe what is in `val`,
/// and the application may dictate them: "**SQLSetDescField** is required to
/// perform manual binding with SQL_C_NUMERIC values". So a non-zero
/// [`NumericTarget::precision`] is honoured and the value is rescaled to it;
/// [`NumericTarget::UNSPECIFIED`] means the application said nothing and both
/// are taken from the value, which is the self-describing reading the struct's
/// own fields invite. Zero is not a legal precision, which is what makes it
/// usable as "unspecified".
///
/// # Sign
///
/// `odbc-sys` documents `sign` as "1 if positive, 0 if negative", the opposite
/// of a sign *bit*, and the field most likely to be inverted by habit.
/// `a_negative_value_sets_the_numeric_sign_byte_to_zero` pins it.
unsafe fn write_numeric(
    value: &ColumnValue,
    target_ptr: *mut c_void,
    len_ind_ptr: *mut isize,
    target: NumericTarget,
) -> Result<SqlReturn, OdbcError> {
    // The rendered decimal is the pivot, for the same reason the character
    // targets above use it: it is exact for `Decimal`, which is the variant an
    // application reading `SQL_C_NUMERIC` is overwhelmingly reading, and an
    // `f64` round-trip would corrupt the digits the struct exists to preserve.
    let rendered = match value {
        ColumnValue::String(s) | ColumnValue::Decimal(s) => s.clone(),
        ColumnValue::I8(_)
        | ColumnValue::I16(_)
        | ColumnValue::I32(_)
        | ColumnValue::I64(_)
        | ColumnValue::F32(_)
        | ColumnValue::F64(_)
        | ColumnValue::Bool(_) => column_value_to_string(value),
        // Off both tables: the source is not numeric or character at all.
        _ => {
            return Err(OdbcError::general(
                format!("Cannot convert {value:?} to SQL_C_NUMERIC"),
                SqlState::restricted_data_type_attribute_violation(),
            ));
        }
    };

    let literal =
        crate::param_convert::parse_numeric_literal(rendered.trim()).ok_or_else(|| {
            OdbcError::general(
                format!("Value is not a numeric literal: {rendered}"),
                SqlState::invalid_character_value_for_cast(),
            )
        })?;

    let (out, fraction_lost) = literal.to_numeric_struct(target)?;

    let _ = unsafe { write_fixed(target_ptr, len_ind_ptr, out) }?;
    if fraction_lost {
        // The row's second outcome: truncated data *is* written, and 01S07 is a
        // warning rather than a failure. `write_fixed` above has already run.
        return Err(OdbcError::FractionalTruncation);
    }
    Ok(SqlReturn::SUCCESS)
}

/// The display size ODBC defines for `SQL_REAL`, in characters.
///
/// Spec ("Display Size" appendix): "SQL_REAL | 14 (a sign, 7 digits, a decimal
/// point, the letter *E*, a sign, and 2 digits)."
const DISPLAY_SIZE_REAL: usize = 14;

/// The display size ODBC defines for `SQL_FLOAT` and `SQL_DOUBLE`, in
/// characters.
///
/// Spec ("Display Size" appendix): "SQL_FLOAT SQL_DOUBLE | 24 (a sign, 15
/// digits, a decimal point, the letter *E*, a sign, and 3 digits)."
const DISPLAY_SIZE_FLOAT_DOUBLE: usize = 24;

/// Render a float for a character target, within the display size ODBC defines
/// for its SQL type.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/appendixes/display-size>
///
/// The *Display Size* appendix does not just give a number, it says what the
/// number is made of ("a sign, 15 digits, a decimal point, the letter *E*, a
/// sign, and 3 digits"), so the size it fixes is the size of an **exponent**
/// rendering. `col_attr::display_size_for` reports 24 and 14 on the strength of
/// exactly those sentences.
///
/// Rust's `Display` for `f32`/`f64` never emits an exponent, so rendering
/// positionally would promise a 24-character exponent form and deliver a
/// positional one up to 326 characters long. Two things follow, and the second
/// is the worse:
///
/// - **Large magnitudes become a hard error.** 309 positional digits against a
///   display-size buffer trips the *SQL to C: Numeric* whole-digit rule, so
///   `f64::MAX` is `22003`, loud at least.
/// - **Small magnitudes become silently wrong.** `4.9e-324` is 326 characters
///   whose first 24 are `0.00000000000000000000000`, so the application reads
///   **zero**, flagged `01004` ("truncated"), which is not the same claim as
///   "wrong".
///
/// # Why the switch is conditional
///
/// Rendering *every* float in exponent form would satisfy the display size too,
/// and would turn `1.5` into `1.5E0` for every application reading a float as
/// text. The spec fixes the size, not the notation, so the notation is chosen
/// to keep the familiar rendering wherever it already fits, which is every
/// value an application is likely to see. `an_ordinary_float_keeps_its_positional_rendering`
/// pins that half.
///
/// This is also what the neighbouring drivers do, though neither could be
/// confirmed to the "read the source" standard this crate prefers: MySQL
/// Connector/ODBC formats through `my_gcvt`, whose `gcvt` lineage is the C
/// general-format conversion that switches to an exponent when the value does
/// not fit; and psqlODBC does not format at all, passing PostgreSQL's own
/// `float8` text through, which is a shortest round-trip rendering that uses an
/// exponent for extreme magnitudes. Both are consistent with the conditional
/// switch and neither was read end-to-end, so the deciding argument here is the
/// spec's own definition of the display size, not the survey.
///
/// # Precision
///
/// The exponent form keeps Rust's shortest-round-trip digits rather than
/// truncating to the 15 the appendix names, because the appendix is describing
/// a *maximum* width and 17 significant digits are what an `f64` needs to
/// survive the round trip: `f64::MAX` renders as `1.7976931348623157E308`, 22
/// characters, inside the 24. Truncating to 15 would fit a budget that is not
/// binding and lose the value's identity, which is what this function exists to
/// prevent.
fn render_float<T>(v: T, display_size: usize) -> String
where
    T: std::fmt::Display + std::fmt::UpperExp,
{
    let positional = v.to_string();
    if positional.len() <= display_size {
        positional
    } else {
        format!("{v:E}")
    }
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
        ColumnValue::F32(v) => render_float(*v, DISPLAY_SIZE_REAL),
        ColumnValue::F64(v) if v.is_infinite() => infinity_text(v.is_sign_negative()).to_string(),
        ColumnValue::F64(v) => render_float(*v, DISPLAY_SIZE_FLOAT_DOUBLE),
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
                NumericTarget::UNSPECIFIED,
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
            write_column_value(
                &value,
                CDataType::Default,
                target,
                4,
                &mut ind,
                NumericTarget::UNSPECIFIED,
            )
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
        // (SUCCESS); see `write_utf16`'s identical split.
        let mut buf = [0xAAu8; 4];
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::String("hello".into()),
                CDataType::WChar,
                buf.as_mut_ptr() as *mut c_void,
                0,
                &mut ind,
                NumericTarget::UNSPECIFIED,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS_WITH_INFO);
        assert_eq!(ind, 10); // 5 chars * 2 bytes, still reported
        assert_eq!(buf, [0xAA; 4], "wrote into a zero-length buffer");
    }

    #[test]
    fn wchar_null_target_with_zero_length_is_a_pure_length_query() {
        // A null target pointer stays SUCCESS regardless of buf_len. Not
        // something SQLGetData's own spec sanctions directly (its Arguments
        // section says "TargetValuePtr cannot be NULL"), but this writer is
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                    timezone_offset_minutes: 330, // +05:30, dropped by write_column_value
                },
                CDataType::TypeTimestamp,
                buf.as_mut_ptr() as *mut c_void,
                std::mem::size_of::<Timestamp>() as isize,
                &mut ind,
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
            )
        };
        assert!(ret.is_err());
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
        // over `inf`/`infinity`, so both spellings survive, but nothing pinned
        // that before, and emitting a spelling core cannot read back would be a
        // one-way door.
        //
        // An integer target is the interesting one: none of these is a
        // *numeric-literal*, so the exact path declines them and the `f64`
        // fallback is what answers.
        //
        // This is also the other half of the overflow check
        // `a_character_literal_beyond_f64_range_is_22003_with_nothing_written`
        // pins: these four parse to the same `f64` an overflowing literal does,
        // so a check that looked only at the parsed value would fail them.
        for text in ["Infinity", "-Infinity", "inf", "-inf"] {
            assert!(
                matches!(
                    parse_numeric_text(text, CDataType::SBigInt),
                    Ok(NumericPivot::Float(f)) if f.is_infinite()
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                        &mut ind, NumericTarget::UNSPECIFIED)
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
                        &mut ind, NumericTarget::UNSPECIFIED)
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
                        &mut ind, NumericTarget::UNSPECIFIED)
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
                        &mut ind, NumericTarget::UNSPECIFIED)
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
        // not; together they pin the bound as exact for the target's width.
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS);
        assert_eq!(buf, 0);
        assert_eq!(ind, 1);
    }

    /// `2.0` is the lower edge of the table's "greater than or equal to 2" row,
    /// so it is 22003 rather than the `1` a "non-zero means true" reading would
    /// write.
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
                NumericTarget::UNSPECIFIED,
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
    fn int_to_float_precision_loss_is_success_with_the_narrowed_value() {
        // 2^24 + 1 = 16_777_217 cannot be represented exactly as f32, so the
        // narrowing is inexact. The *SQL to C: Numeric* row for
        // SQL_C_FLOAT/SQL_C_DOUBLE has only two cells (in range -> *Data* /
        // n/a, out of range -> *Undefined* / 22003), and 16_777_216.0 is
        // in range, so the outcome is the first cell: the value is written and
        // nothing is reported. The integer row above and the SQL_C_BIT row
        // below both carry 01S07; the float row's omission is a distinction
        // the table draws.
        let mut buf: f32 = 0.0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::I64(16_777_217),
                CDataType::Float,
                &mut buf as *mut f32 as *mut c_void,
                4,
                std::ptr::null_mut(),
                NumericTarget::UNSPECIFIED,
            )
        }
        .expect("an in-range inexact narrowing is the row's first cell");
        assert_eq!(ret, SqlReturn::SUCCESS);
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
            )
        }
        .expect_err("2^64 does not fit in u64");
        assert_eq!(
            err.sqlstate().as_str(),
            crate::types::sql_state::NUMERIC_VALUE_OUT_OF_RANGE
        );
    }

    #[test]
    fn f64_narrowed_to_f32_with_precision_loss_is_success() {
        // 0.1 is a genuinely inexact narrowing: the nearest f32 is not the
        // f64 the source held. It is still inside the range of SQL_C_FLOAT,
        // so it is the float row's "within the range" cell: *Data* / n/a.
        let mut out = 0f32;
        let mut ind = 0isize;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::F64(0.1),
                CDataType::Float,
                std::ptr::from_mut(&mut out).cast(),
                size_of::<f32>() as isize,
                &mut ind,
                NumericTarget::UNSPECIFIED,
            )
        }
        .expect("an inexact but in-range narrowing reports nothing");
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!(out, 0.1f32);
        assert_eq!(ind, size_of::<f32>() as isize);
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
                NumericTarget::UNSPECIFIED,
            )
        }
        .expect("0.5 is exact in f32");
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!(out, 0.5f32);
    }

    #[test]
    fn f64_overflowing_f32_to_float_is_22003_with_nothing_written() {
        // Both directions of the narrowing: 1e300 is finite as an f64 and
        // beyond f32::MAX either way round, so `as f32` saturates to ±inf.
        // The table's SQL_C_FLOAT/SQL_C_DOUBLE row calls that "outside the
        // range of the data type to which the number is being converted" and
        // gives it 22003 with both output columns "Undefined", so the
        // sentinels below must survive.
        for v in [1e300_f64, -1e300_f64] {
            let mut out = 9.0f32;
            let mut ind = 99isize;
            let err = unsafe {
                write_column_value(
                    &ColumnValue::F64(v),
                    CDataType::Float,
                    std::ptr::from_mut(&mut out).cast(),
                    size_of::<f32>() as isize,
                    &mut ind,
                    NumericTarget::UNSPECIFIED,
                )
            }
            .expect_err("a magnitude beyond f32::MAX is outside the range of SQL_C_FLOAT");
            assert_eq!(
                sqlstate_of_err(&err),
                SqlState::numeric_value_out_of_range().as_str()
            );
            assert_eq!(err.sql_return(), SqlReturn::ERROR);
            assert_eq!(out, 9.0f32, "*TargetValuePtr must be left alone");
            assert_eq!(ind, 99, "*StrLen_or_IndPtr must be left alone");
        }
    }

    #[test]
    fn f64_infinity_to_float_is_the_value_the_source_held() {
        // The finiteness half of the overflow test above is load-bearing: a
        // source that really is ±infinity narrows to ±infinity exactly, so it
        // is inside the range of SQL_C_FLOAT in the only sense f32 has and is
        // delivered unchanged. Without that half, a data source with an
        // IEEE infinity in a column (PostgreSQL's 'Infinity'::float8) could
        // never read it back through SQL_C_FLOAT.
        for v in [f64::INFINITY, f64::NEG_INFINITY] {
            let mut out = 9.0f32;
            let mut ind = 0isize;
            let ret = unsafe {
                write_column_value(
                    &ColumnValue::F64(v),
                    CDataType::Float,
                    std::ptr::from_mut(&mut out).cast(),
                    size_of::<f32>() as isize,
                    &mut ind,
                    NumericTarget::UNSPECIFIED,
                )
            }
            .expect("an infinity the source held narrows to f32 exactly");
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(out, v as f32);
            assert_eq!(ind, size_of::<f32>() as isize);
        }
    }

    #[test]
    fn f64_narrowing_to_the_smallest_f32_subnormal_is_success() {
        // Underflow is not overflow. A subnormal f32 is a value f32 can hold,
        // so it is inside the row's "within the range" cell, not outside it:
        // this must stay SQL_SUCCESS and must not become 22003. The smallest
        // positive f32 subnormal is exactly representable as an f64, so the
        // round trip is exact and there is nothing to warn about either.
        let v = f64::from(f32::from_bits(1));
        let mut out = 0f32;
        let mut ind = 0isize;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::F64(v),
                CDataType::Float,
                std::ptr::from_mut(&mut out).cast(),
                size_of::<f32>() as isize,
                &mut ind,
                NumericTarget::UNSPECIFIED,
            )
        }
        .expect("a subnormal is within the range of SQL_C_FLOAT");
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!(out, f32::from_bits(1));
        assert_eq!(ind, size_of::<f32>() as isize);
    }

    #[test]
    fn f64_underflowing_to_zero_in_f32_is_success_with_zero_written() {
        // 1e-300 is far below the smallest f32 subnormal, so it narrows to
        // 0.0. Zero is inside the range of SQL_C_FLOAT, so this is not the
        // 22003 cell; what is left is an inexact narrowing, which the row's
        // "within the range" cell reports as nothing at all.
        let mut out = 9.0f32;
        let mut ind = 0isize;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::F64(1e-300),
                CDataType::Float,
                std::ptr::from_mut(&mut out).cast(),
                size_of::<f32>() as isize,
                &mut ind,
                NumericTarget::UNSPECIFIED,
            )
        }
        .expect("underflow to zero is inside the range of SQL_C_FLOAT");
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!(out, 0.0f32);
        assert_eq!(ind, size_of::<f32>() as isize);
    }

    #[test]
    fn f64_nan_to_float_is_written_without_a_diagnostic() {
        // A NaN narrows to a NaN, which no comparison can call equal to its
        // source, so an equality test between the two would report a fractional
        // truncation that never happened. The NaN is delivered and nothing is
        // reported. Note the contrast with
        // `float_nan_to_bit_returns_22003`: SQL_C_BIT has a range test a NaN
        // fails, and SQL_C_FLOAT has no range a NaN is outside of.
        let mut out = 9.0f32;
        let mut ind = 0isize;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::F64(f64::NAN),
                CDataType::Float,
                std::ptr::from_mut(&mut out).cast(),
                size_of::<f32>() as isize,
                &mut ind,
                NumericTarget::UNSPECIFIED,
            )
        }
        .expect("a NaN is not outside the range of SQL_C_FLOAT");
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert!(out.is_nan(), "the NaN the source held must be delivered");
        assert_eq!(ind, size_of::<f32>() as isize);
    }

    // -----------------------------------------------------------------------
    // Misaligned application buffers
    //
    // ODBC row-wise binding hands out pointers at arbitrary offsets into a
    // packed buffer, so no target pointer is ever guaranteed aligned. Each test
    // below offsets one byte into an allocation of the *target* type, which is
    // misaligned on every platform. Offsetting into a `Vec<u8>` would not be,
    // since a byte allocation may already start on an odd address.
    //
    // What these prove where, measured on x86-64 with `debug-assertions`:
    // a regression to `*ptr = v`, `slice::from_raw_parts`, `&*ptr` or
    // `copy_nonoverlapping` aborts the test process outright; a regression from
    // `write_unaligned` to `ptr::write` is **not** detected natively and needs
    // `MIRIFLAGS=-Zmiri-symbolic-alignment-check`.
    // -----------------------------------------------------------------------

    /// An 8-byte integer target at an odd address, with the length indicator
    /// misaligned too.
    #[test]
    fn sbigint_target_may_be_misaligned() {
        let mut arena = vec![0i64; 4];
        let mut ind_arena = vec![0isize; 4];
        // SAFETY: both offsets stay inside their own allocation.
        let out = unsafe { arena.as_mut_ptr().cast::<u8>().add(1) }.cast::<i64>();
        let ind = unsafe { ind_arena.as_mut_ptr().cast::<u8>().add(1) }.cast::<isize>();
        assert!(!out.is_aligned() && !ind.is_aligned(), "the test's premise");

        let ret = unsafe {
            write_column_value(
                &ColumnValue::I64(-3),
                CDataType::SBigInt,
                out.cast::<c_void>(),
                size_of::<i64>() as isize,
                ind,
                NumericTarget::UNSPECIFIED,
            )
        }
        .expect("a misaligned target is legal");
        assert_eq!(ret, SqlReturn::SUCCESS);
        // SAFETY: read back through the same unaligned pointers.
        unsafe {
            assert_eq!(std::ptr::read_unaligned(out), -3);
            assert_eq!(std::ptr::read_unaligned(ind), size_of::<i64>() as isize);
        }
    }

    /// The same for a float target: `f64` has the same alignment as `i64` but a
    /// different write path through `write_fixed`'s monomorphisation.
    #[test]
    fn double_target_may_be_misaligned() {
        let mut arena = vec![0f64; 4];
        // SAFETY: stays inside the allocation.
        let out = unsafe { arena.as_mut_ptr().cast::<u8>().add(1) }.cast::<f64>();
        assert!(!out.is_aligned(), "the test's premise");

        let ret = unsafe {
            write_column_value(
                &ColumnValue::F64(0.5),
                CDataType::Double,
                out.cast::<c_void>(),
                size_of::<f64>() as isize,
                std::ptr::null_mut(),
                NumericTarget::UNSPECIFIED,
            )
        }
        .expect("a misaligned target is legal");
        assert_eq!(ret, SqlReturn::SUCCESS);
        // SAFETY: read back through the same unaligned pointer.
        unsafe { assert_eq!(std::ptr::read_unaligned(out), 0.5) };
    }

    /// A struct target rather than a scalar. `SQL_TIMESTAMP_STRUCT` is written
    /// as one value, so a single misaligned write covers all seven fields. Its
    /// `year` is an `i16` inside a struct whose alignment is 4, so a naive
    /// field-by-field write would have a different bug.
    #[test]
    fn timestamp_struct_target_may_be_misaligned() {
        let mut arena = vec![odbc_sys::Timestamp::default(); 4];
        // SAFETY: stays inside the allocation.
        let out = unsafe { arena.as_mut_ptr().cast::<u8>().add(1) }.cast::<odbc_sys::Timestamp>();
        assert!(!out.is_aligned(), "the test's premise");

        let ret = unsafe {
            write_column_value(
                &ColumnValue::Timestamp {
                    year: 2026,
                    month: 8,
                    day: 3,
                    hour: 10,
                    minute: 30,
                    second: 15,
                    fraction: 0,
                },
                CDataType::TypeTimestamp,
                out.cast::<c_void>(),
                size_of::<odbc_sys::Timestamp>() as isize,
                std::ptr::null_mut(),
                NumericTarget::UNSPECIFIED,
            )
        }
        .expect("a misaligned target is legal");
        assert_eq!(ret, SqlReturn::SUCCESS);
        // SAFETY: read back through the same unaligned pointer.
        let ts = unsafe { std::ptr::read_unaligned(out) };
        assert_eq!((ts.year, ts.month, ts.day), (2026, 8, 3));
        assert_eq!((ts.hour, ts.minute, ts.second), (10, 30, 15));
    }

    /// `SQL_GUID` → `SQL_C_GUID` is the *SQL to C: GUID* table's own row, and
    /// the only one that table gives for this C type: test "None", data
    /// written, indicator 16, no SQLSTATE. There is no failure case.
    ///
    /// `ColumnValue::Guid` already converted to `SQL_C_BINARY` and
    /// `SQL_C_CHAR`; its *own* C type was a blanket `07006`.
    #[test]
    fn a_guid_column_converts_to_sql_c_guid() {
        // 00112233-4455-6677-8899-aabbccddeeff
        let bytes: [u8; 16] = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        let mut out = odbc_sys::Guid::default();
        let mut ind: isize = -999;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::Guid(bytes),
                CDataType::Guid,
                (&raw mut out).cast::<c_void>(),
                // Footnote [a]: BufferLength is ignored for this conversion.
                0,
                &mut ind,
                NumericTarget::UNSPECIFIED,
            )
        }
        .expect("SQL_GUID -> SQL_C_GUID is the table's own row");
        assert_eq!(ret, SqlReturn::SUCCESS);
        // The first three groups are integers whose textual form is the
        // big-endian reading of the bytes, which is the order
        // `column_value_to_string` already renders (data[0] is the first digit
        // pair). Getting this wrong byte-swaps the GUID silently.
        assert_eq!(out.d1, 0x0011_2233);
        assert_eq!(out.d2, 0x4455);
        assert_eq!(out.d3, 0x6677);
        assert_eq!(out.d4, [0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]);
        assert_eq!(ind, 16, "the table's indicator cell is 16");
    }

    /// The guard against over-reach. `SQL_C_GUID` appears in **one** conversion
    /// table, *SQL to C: GUID*, whose only source type is `SQL_GUID`. The
    /// *SQL to C: Character* table has no `SQL_C_GUID` row at all, so a
    /// character column read as `SQL_C_GUID` is not a defined conversion, and
    /// the overview page says exactly what that is: "If the *TargetType*
    /// argument ... contains an identifier for an ODBC C data type not shown in
    /// the table for a given ODBC SQL data type, **SQLFetch**,
    /// **SQLFetchScroll**, or **SQLGetData** returns SQLSTATE 07006".
    ///
    /// So `07006` here is correct rather than a gap, and a `22018` "bad GUID
    /// parse" would be inventing a cell the spec does not have.
    #[test]
    fn a_character_column_read_as_sql_c_guid_is_07006() {
        for value in [
            ColumnValue::String("00112233-4455-6677-8899-aabbccddeeff".to_string()),
            ColumnValue::String("not a guid".to_string()),
            ColumnValue::I64(1),
        ] {
            let mut out = odbc_sys::Guid::default();
            let mut ind: isize = 0;
            let err = unsafe {
                write_column_value(
                    &value,
                    CDataType::Guid,
                    (&raw mut out).cast::<c_void>(),
                    0,
                    &mut ind,
                    NumericTarget::UNSPECIFIED,
                )
            }
            .expect_err("only SQL_GUID converts to SQL_C_GUID");
            assert_eq!(
                sqlstate_of_err(&err),
                crate::types::sql_state::RESTRICTED_DATA_TYPE_ATTRIBUTE_VIOLATION,
                "{value:?}",
            );
        }
    }

    /// `SQLGUID` leads with a `u32`, so it has alignment 4 and *can* be
    /// misaligned, unlike `SQL_NUMERIC_STRUCT` below. Offset one byte into an
    /// arena of the target type so this is misaligned on every platform.
    #[test]
    fn guid_target_may_be_misaligned() {
        let mut arena = vec![odbc_sys::Guid::default(); 4];
        // SAFETY: stays inside the allocation.
        let out = unsafe { arena.as_mut_ptr().cast::<u8>().add(1) }.cast::<odbc_sys::Guid>();
        assert!(!out.is_aligned(), "the test's premise");

        let bytes: [u8; 16] = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::Guid(bytes),
                CDataType::Guid,
                out.cast::<c_void>(),
                0,
                &mut ind,
                NumericTarget::UNSPECIFIED,
            )
        }
        .expect("a misaligned target is legal");
        assert_eq!(ret, SqlReturn::SUCCESS);
        // SAFETY: read back through the same unaligned pointer.
        let g = unsafe { std::ptr::read_unaligned(out) };
        assert_eq!((g.d1, g.d2, g.d3), (0x0011_2233, 0x4455, 0x6677));
    }

    /// `SQL_NUMERIC_STRUCT` has **alignment 1**, so, alone among the fixed
    /// targets, it needs no misalignment test and one cannot be written: its
    /// fields are `u8`, `i8`, `u8` and `[u8; 16]`, so every address is aligned
    /// for it and `u8`'s exemption in AGENTS.md's alignment table covers it.
    ///
    /// This is the test that says so. An attempt at the usual
    /// offset-one-byte-into-an-arena test (as
    /// `timestamp_struct_target_may_be_misaligned` does) fails on its own
    /// `assert!(!out.is_aligned())` premise, which reads like a broken test
    /// rather than a type that cannot be misaligned. Asserting the alignment
    /// directly records the reason, and fails if `odbc-sys` ever gives
    /// `Numeric` a wider field, at which point a real misalignment test becomes
    /// both necessary and possible.
    #[test]
    fn the_numeric_struct_cannot_be_misaligned() {
        assert_eq!(
            std::mem::align_of::<odbc_sys::Numeric>(),
            1,
            "SQL_NUMERIC_STRUCT is all bytes; a wider field would need a \
             misalignment test for the SQL_C_NUMERIC write",
        );
        assert_eq!(
            std::mem::size_of::<odbc_sys::Numeric>(),
            3 + odbc_sys::MAX_NUMERIC_LEN,
            "precision, scale, sign, then val",
        );
    }

    // -----------------------------------------------------------------------
    // Tests for Decimal/String → numeric C type conversion
    // -----------------------------------------------------------------------

    #[test]
    fn a_character_literal_beyond_f64_range_is_22003_with_nothing_written() {
        // "1e400" is a *numeric-literal*, so not the SQL to C: Character row's
        // 22018 cell, but its magnitude is one no f64 holds. Rust's parser
        // saturates it to an infinity, so delivering the parse result would hand
        // the application +inf and SQL_SUCCESS: a number the data source never
        // held, reported as exact. The row's second cell governs it, "outside the range of the
        // data type to which the number is being converted" → *Undefined* /
        // 22003, so both sentinels must survive.
        //
        // A Decimal source is checked alongside a String one because the two
        // share this parse path, and SQL to C: Numeric's float row draws the
        // same distinction for it.
        //
        // The last two cases are what rule out the near-miss implementation of
        // this check. `"9" * 400` has no exponent to inspect at all, and
        // `1e2147483648`'s exponent does not fit an `i32`, so
        // `parse_numeric_literal` answers `None` for it, and a check written in
        // terms of that function would let it through as an infinity.
        for text in ["1e400", "-1e400", "9".repeat(400).as_str(), "1e2147483648"] {
            for value in [
                ColumnValue::String(text.into()),
                ColumnValue::Decimal(text.into()),
            ] {
                let mut f32_out = 9.0f32;
                let mut ind = 99isize;
                let err = unsafe {
                    write_column_value(
                        &value,
                        CDataType::Float,
                        std::ptr::from_mut(&mut f32_out).cast(),
                        size_of::<f32>() as isize,
                        &mut ind,
                        NumericTarget::UNSPECIFIED,
                    )
                }
                .expect_err("a literal beyond f64 range is outside the range of SQL_C_FLOAT");
                assert_eq!(
                    sqlstate_of_err(&err),
                    SqlState::numeric_value_out_of_range().as_str(),
                    "{value:?} to SQL_C_FLOAT"
                );
                assert_eq!(err.sql_return(), SqlReturn::ERROR);
                assert_eq!(f32_out, 9.0f32, "*TargetValuePtr must be left alone");
                assert_eq!(ind, 99, "*StrLen_or_IndPtr must be left alone");

                // SQL_C_DOUBLE is reached by the same parse, so it must agree.
                let mut f64_out = 9.0f64;
                let mut ind = 99isize;
                let err = unsafe {
                    write_column_value(
                        &value,
                        CDataType::Double,
                        std::ptr::from_mut(&mut f64_out).cast(),
                        size_of::<f64>() as isize,
                        &mut ind,
                        NumericTarget::UNSPECIFIED,
                    )
                }
                .expect_err("a literal beyond f64 range is outside the range of SQL_C_DOUBLE");
                assert_eq!(
                    sqlstate_of_err(&err),
                    SqlState::numeric_value_out_of_range().as_str(),
                    "{value:?} to SQL_C_DOUBLE"
                );
                assert_eq!(err.sql_return(), SqlReturn::ERROR);
                assert_eq!(f64_out, 9.0f64, "*TargetValuePtr must be left alone");
                assert_eq!(ind, 99, "*StrLen_or_IndPtr must be left alone");
            }
        }
    }

    #[test]
    fn a_character_literal_underflowing_f64_is_zero_and_success() {
        // The other end of the range, and deliberately not symmetrical with the
        // test above: zero is a value f64 holds, so "1e-400" is the row's
        // *first* cell rather than its second. The same reading the F64 → f32
        // underflow takes (`f64_underflowing_to_zero_in_f32_is_success_with_zero_written`),
        // so the parse site and the narrowing site agree.
        let mut out = 9.0f64;
        let mut ind = 0isize;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::String("1e-400".into()),
                CDataType::Double,
                std::ptr::from_mut(&mut out).cast(),
                size_of::<f64>() as isize,
                &mut ind,
                NumericTarget::UNSPECIFIED,
            )
        }
        .expect("an underflow to zero is inside the range of SQL_C_DOUBLE");
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!(out, 0.0f64);
        assert_eq!(ind, size_of::<f64>() as isize);
    }

    #[test]
    fn text_that_is_not_a_numeric_literal_stays_22018() {
        // The cell either side of the overflow one: "Data is not a
        // *numeric-literal*" → 22018. The overflow fix must not swallow it,
        // which is the failure mode of deciding "out of range" from the parsed
        // value alone, because an infinity spelling parses to the same f64 as an
        // overflow does. `parse_numeric_literal` is what separates them, and
        // `the_infinity_spelling_parses_back_into_a_float` pins the other half.
        let mut out = 9.0f32;
        let mut ind = 99isize;
        let err = unsafe {
            write_column_value(
                &ColumnValue::String("not a number".into()),
                CDataType::Float,
                std::ptr::from_mut(&mut out).cast(),
                size_of::<f32>() as isize,
                &mut ind,
                NumericTarget::UNSPECIFIED,
            )
        }
        .expect_err("text that is not a numeric literal is 22018");
        assert_eq!(
            sqlstate_of_err(&err),
            SqlState::invalid_character_value_for_cast().as_str()
        );
        assert_eq!(out, 9.0f32, "*TargetValuePtr must be left alone");
        assert_eq!(ind, 99, "*StrLen_or_IndPtr must be left alone");
    }

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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                    NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
    /// attacker-controlled on a compromised or hostile source. Without
    /// `param_convert::MAX_DECIMAL_EXPANSION_DIGITS`, this reaches
    /// `"0".repeat(2_147_483_646)` inside `DecimalLiteral::to_integer`, a
    /// ~2 GB allocation with a second copy in the `format!` that follows.
    /// An allocation failure aborts the process rather than unwinding, so
    /// `panic_safe` cannot contain it.
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
                NumericTarget::UNSPECIFIED,
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
    /// no cost, since `to_integer`'s positive-scale branch slices digits the
    /// source supplied rather than expanding anything, so the expansion bound
    /// must not reach it. `01S07` because a non-zero fraction was dropped.
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
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
                NumericTarget::UNSPECIFIED,
            )
        }
        .expect_err("hour 700000 must not convert");
        assert_eq!(
            err.sqlstate().as_str(),
            crate::types::sql_state::INVALID_DATETIME_FORMAT
        );
    }

    // -----------------------------------------------------------------------
    // The cross-form rows of SQL to C: Character: a character column whose
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
                NumericTarget::UNSPECIFIED,
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
        // "Data value is a valid timestamp-value; time portion is zero": the
        // date is written and the SQLSTATE column is "n/a".
        let (out, ret) =
            unsafe { convert_text("2026-07-21 00:00:00", CDataType::TypeDate, date_sentinel()) };
        let ret = ret.expect("a timestamp whose time is zero converts to a date cleanly");
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!((out.year, out.month, out.day), (2026, 7, 21));
    }

    #[test]
    fn timestamp_text_with_time_to_date_is_01s07() {
        // "Data value is a valid timestamp-value; time portion is nonzero":
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
        // timestamp-value": 22018, *TargetValuePtr* undefined.
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
        // nonzero": 01S07 with the truncated data written. Only the *fraction*
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
    // "Data value is not a valid date-value or timestamp-value": the last row
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
        // too: a well-formed time does not rescue an impossible date.
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
        // timestamp-value, but only of a valid one, and the row's last line
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
                NumericTarget::UNSPECIFIED,
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
            // SQL to C: Date, so SQL_C_TYPE_DATE and SQL_C_TYPE_TIMESTAMP only.
            ("DATE -> DATE", a_date(), CDataType::TypeDate, true),
            (
                "DATE -> TIMESTAMP",
                a_date(),
                CDataType::TypeTimestamp,
                true,
            ),
            ("DATE -> TIME", a_date(), CDataType::TypeTime, false),
            // SQL to C: Time, so SQL_C_TYPE_TIME and SQL_C_TYPE_TIMESTAMP only.
            ("TIME -> TIME", a_time(0), CDataType::TypeTime, true),
            (
                "TIME -> TIMESTAMP",
                a_time(0),
                CDataType::TypeTimestamp,
                true,
            ),
            ("TIME -> DATE", a_time(0), CDataType::TypeDate, false),
            // SQL to C: Timestamp, so all three.
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
        // zero." No SQLSTATE, because nothing is lost.
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
        // splits on the fractional seconds alone, so a discarded date is not a
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
        // year and its neighbour, year 0, which is divisible by 400 and
        // therefore leap in the proleptic Gregorian calendar both sides use,
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

    // -----------------------------------------------------------------------
    // SQL to C: Numeric: the SQL_C_CHAR / SQL_C_WCHAR rows
    //
    // "Number of whole (as opposed to fractional) digits < BufferLength":
    // truncated data written, indicator set, 01004.
    // "Number of whole (as opposed to fractional) digits >= BufferLength":
    // *TargetValuePtr* "Undefined", *StrLen_or_IndPtr* "Undefined", 22003.
    // -----------------------------------------------------------------------

    /// Fills a buffer and an indicator with sentinels, runs a character-target
    /// conversion, and asserts the table's 22003 row: SQLSTATE 22003 and both
    /// output locations left exactly as they were, since the row calls each of
    /// them "Undefined".
    fn assert_22003_writes_nothing(value: &ColumnValue, target_type: CDataType, buf_len: isize) {
        const BUF_SENTINEL: u8 = 0xAA;
        const IND_SENTINEL: isize = -12_345;

        let mut buf = [BUF_SENTINEL; 64];
        let mut ind: isize = IND_SENTINEL;
        let ret = unsafe {
            write_column_value(
                value,
                target_type,
                buf.as_mut_ptr().cast::<c_void>(),
                buf_len,
                &mut ind,
                NumericTarget::UNSPECIFIED,
            )
        };
        assert_eq!(
            sqlstate_of_err(&ret.unwrap_err()),
            SqlState::numeric_value_out_of_range().as_str(),
            "{value:?} into a {buf_len}-byte {target_type:?} buffer"
        );
        assert!(
            buf.iter().all(|&b| b == BUF_SENTINEL),
            "*TargetValuePtr* is \"Undefined\" on the 22003 row, so nothing may be written: {:?}",
            &buf[..8]
        );
        assert_eq!(
            ind, IND_SENTINEL,
            "*StrLen_or_IndPtr* is \"Undefined\" on the 22003 row, so it must not be written"
        );
    }

    /// Runs a character-target conversion and returns the bytes delivered plus
    /// the indicator, for the 01004 row where truncated data *is* written.
    fn char_write(value: &ColumnValue, buf_len: isize) -> (SqlReturn, Vec<u8>, isize) {
        let mut buf = [0xAAu8; 64];
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                value,
                CDataType::Char,
                buf.as_mut_ptr().cast::<c_void>(),
                buf_len,
                &mut ind,
                NumericTarget::UNSPECIFIED,
            )
        };
        let len = usize::try_from(buf_len.max(0)).expect("test buffer length");
        (ret.expect("no error expected"), buf[..len].to_vec(), ind)
    }

    #[test]
    fn i64_123456_into_4_byte_char_buffer_is_22003() {
        // Six whole digits, BufferLength 4: "Number of whole (as opposed to
        // fractional) digits >= BufferLength". Delivering "123" would hand the
        // application a different *number*, which is what separates this row
        // from the character table's ordinary 01004.
        assert_22003_writes_nothing(&ColumnValue::I64(123_456), CDataType::Char, 4);
    }

    #[test]
    fn f64_1_25_into_buffer_holding_1_2_is_01004() {
        // Whole digits 1 < BufferLength 4, so this is the middle row:
        // "Truncated data" written, "Length of data in bytes" in the
        // indicator, 01004. Only the fraction is lost.
        let (ret, written, ind) = char_write(&ColumnValue::F64(1.25), 4);
        assert_eq!(ret, SqlReturn::SUCCESS_WITH_INFO);
        assert_eq!(&written, b"1.2\0");
        assert_eq!(ind, 4, "\"1.25\" is four bytes of data");
    }

    #[test]
    fn the_char_boundary_is_every_whole_digit_plus_the_terminator() {
        // "1234.5": four whole digits. BufferLength 5 holds "1234" and the
        // null terminator and nothing else: whole digits 4 < 5, the 01004
        // row. One byte less and 4 >= 4, the 22003 row. This pins the exact
        // `>=` of the table rather than an off-by-one either side of it.
        let (ret, written, ind) = char_write(&ColumnValue::Decimal("1234.5".to_string()), 5);
        assert_eq!(ret, SqlReturn::SUCCESS_WITH_INFO);
        assert_eq!(&written, b"1234\0");
        assert_eq!(ind, 6);

        assert_22003_writes_nothing(
            &ColumnValue::Decimal("1234.5".to_string()),
            CDataType::Char,
            4,
        );
    }

    #[test]
    fn a_numeric_that_fits_entirely_is_not_truncated_at_the_boundary() {
        // The first row: "Character byte length < BufferLength". "1234" in a
        // five-byte buffer is four bytes plus the terminator, so it is whole
        // data with no SQLSTATE, the boundary case that must not be dragged
        // into either truncation row.
        let (ret, written, ind) = char_write(&ColumnValue::I32(1234), 5);
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!(&written, b"1234\0");
        assert_eq!(ind, 4);
    }

    #[test]
    fn the_wchar_boundary_counts_utf16_units_not_bytes() {
        // The SQL_C_WCHAR row states the same test, and `BufferLength` is a
        // byte count on the wire while the row's "Number of whole ... digits"
        // is a character count, so four whole digits need ten bytes here,
        // five UTF-16 units, not five bytes. Ten passes; eight is 22003.
        let mut buf = [0u16; 16];
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::Decimal("1234.5".to_string()),
                CDataType::WChar,
                buf.as_mut_ptr().cast::<c_void>(),
                10,
                &mut ind,
                NumericTarget::UNSPECIFIED,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS_WITH_INFO);
        assert_eq!(String::from_utf16_lossy(&buf[..4]), "1234");
        assert_eq!(buf[4], 0, "null terminator");
        assert_eq!(ind, 12, "\"1234.5\" is six UTF-16 units, twelve bytes");

        assert_22003_writes_nothing(
            &ColumnValue::Decimal("1234.5".to_string()),
            CDataType::WChar,
            8,
        );
    }

    #[test]
    fn a_minus_sign_occupies_a_whole_digit_position() {
        // The table says "digits" and a minus sign is not one, but the `>=`
        // boundary it draws is exactly "the whole part plus the null
        // terminator must fit", and the sign occupies a byte of the buffer
        // just as a digit does. Reading it out of the count would deliver
        // "-12" for -123.45 in a four-byte buffer: a different number, which
        // is the outcome this row exists to prevent. So the sign counts, in
        // both directions of the boundary.
        let (ret, written, ind) = char_write(&ColumnValue::F64(-123.45), 5);
        assert_eq!(ret, SqlReturn::SUCCESS_WITH_INFO);
        assert_eq!(&written, b"-123\0");
        assert_eq!(ind, 7);

        assert_22003_writes_nothing(&ColumnValue::F64(-123.45), CDataType::Char, 4);
    }

    /// A double whose positional rendering exceeds the display size ODBC
    /// defines for it switches to exponent notation.
    ///
    /// The *Display Size* appendix does not merely give a number, it says what
    /// the number is made of: "SQL_FLOAT SQL_DOUBLE | 24 (a sign, 15 digits, a
    /// decimal point, the letter *E*, a sign, and 3 digits)". That is an
    /// exponent rendering, and `col_attr::display_size_for` already reports 24
    /// on the strength of it. Rust's `Display` for floats never emits an
    /// exponent, so core promised a 24-character exponent form and produced a
    /// 309-character positional one.
    #[test]
    fn a_double_too_wide_for_its_display_size_renders_as_an_exponent() {
        for v in [f64::MAX, f64::MIN, 1.0e30, -1.0e30] {
            let s = column_value_to_string(&ColumnValue::F64(v));
            assert!(
                s.len() <= 24,
                "{v:e} rendered {} chars, over SQL_DOUBLE's display size: {s}",
                s.len(),
            );
            assert_eq!(
                s.parse::<f64>().expect("the rendering must parse back"),
                v,
                "rendering {s} must round-trip",
            );
        }
    }

    /// The subnormal case, which is the one that delivered *wrong data* rather
    /// than an error. Positional `4.9e-324` is 326 characters whose first 24
    /// are `0.00000000000000000000000`, so an application sizing its buffer
    /// from the display size read **zero**, under `01004`, which says
    /// "truncated", not "wrong".
    ///
    /// Its large-magnitude sibling was at least loud: the *SQL to C: Numeric*
    /// whole-digit rule made `f64::MAX` a hard `22003`.
    #[test]
    fn a_subnormal_double_does_not_render_as_zero() {
        let s = column_value_to_string(&ColumnValue::F64(4.9e-324));
        assert!(s.len() <= 24, "rendered {} chars: {s}", s.len());
        assert_ne!(
            s.parse::<f64>().expect("must parse back"),
            0.0,
            "rendering {s} lost the whole value",
        );
    }

    /// The `f32` half, against its own display size. `SQL_REAL` is 14: "a
    /// sign, 7 digits, a decimal point, the letter *E*, a sign, and 2 digits",
    /// so an `f32` is measured against 14 and not against the double's 24.
    #[test]
    fn a_real_is_measured_against_its_own_display_size() {
        for v in [f32::MAX, f32::MIN, 1.0e20, -1.0e20] {
            let s = column_value_to_string(&ColumnValue::F32(v));
            assert!(
                s.len() <= 14,
                "{v:e} rendered {} chars, over SQL_REAL's display size: {s}",
                s.len(),
            );
            assert_eq!(s.parse::<f32>().expect("must parse back"), v);
        }
    }

    /// The guard against over-reach: an ordinary value keeps the rendering it
    /// has always had. Switching every float to exponent form would satisfy the
    /// display size too, and would turn `1.5` into `1.5E0` for every
    /// application in the world.
    #[test]
    fn an_ordinary_float_keeps_its_positional_rendering() {
        assert_eq!(column_value_to_string(&ColumnValue::F64(1.5)), "1.5");
        assert_eq!(
            column_value_to_string(&ColumnValue::F64(-123.45)),
            "-123.45"
        );
        assert_eq!(column_value_to_string(&ColumnValue::F64(0.0)), "0");
        assert_eq!(column_value_to_string(&ColumnValue::F32(0.1)), "0.1");
        // Exactly at the boundary: 17 characters, well inside 24.
        assert_eq!(
            column_value_to_string(&ColumnValue::F64(1.0e16)),
            "10000000000000000"
        );
    }

    /// Convert to `SQL_C_NUMERIC` through the real marshalling entry point,
    /// with a sentinel-filled struct so a partial write is visible.
    fn numeric_write(
        value: &ColumnValue,
        target: NumericTarget,
    ) -> Result<(SqlReturn, odbc_sys::Numeric, isize), OdbcError> {
        let mut out = odbc_sys::Numeric::default();
        let mut ind: isize = -999;
        let ret = unsafe {
            write_column_value(
                value,
                CDataType::Numeric,
                (&raw mut out).cast::<c_void>(),
                // Footnote [a]: "The value of BufferLength is ignored for this
                // conversion." Zero proves it is genuinely ignored.
                0,
                &mut ind,
                target,
            )
        }?;
        Ok((ret, out, ind))
    }

    /// `SQL_C_NUMERIC` had no arm at all and answered `07006` for every value,
    /// while the same C type worked as an *input* parameter. The overview page
    /// is explicit that this is not optional: drivers "are required to support
    /// conversions to all ODBC C data types from the ODBC SQL data types that
    /// they support", and `SQL_C_NUMERIC` shares the *SQL to C: Numeric*
    /// table's exact-integer row with `SQL_C_SLONG` and `SQL_C_SBIGINT`.
    #[test]
    fn a_decimal_column_converts_to_sql_c_numeric() {
        let (ret, out, ind) = numeric_write(
            &ColumnValue::Decimal("-123.45".to_string()),
            NumericTarget {
                precision: 5,
                scale: 2,
            },
        )
        .expect("the conversion is defined by the SQL to C: Numeric table");
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!(out.precision, 5);
        assert_eq!(out.scale, 2);
        assert_eq!(out.sign, 0, "odbc-sys: 1 if positive, 0 if negative");
        assert_eq!(u128::from_le_bytes(out.val), 12345);
        assert_eq!(
            ind,
            std::mem::size_of::<odbc_sys::Numeric>() as isize,
            "the row's indicator cell is \"Size of the C data type\"",
        );
    }

    /// An integer column reaches `SQL_C_NUMERIC` too, since the table's row lists
    /// every exact numeric SQL type, not only `DECIMAL`.
    #[test]
    fn an_integer_column_converts_to_sql_c_numeric() {
        let (ret, out, _) = numeric_write(&ColumnValue::I64(42), NumericTarget::UNSPECIFIED)
            .expect("an integer is a numeric source");
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!(u128::from_le_bytes(out.val), 42);
        assert_eq!(out.scale, 0);
        assert_eq!(out.sign, 1);
    }

    /// The row's second outcome: "Data converted with truncation of fractional
    /// digits" → truncated data and `01S07`. The data *is* written, so this is
    /// a warning; `OdbcError::FractionalTruncation` already carries `01S07` and
    /// already classifies as `SUCCESS_WITH_INFO`.
    #[test]
    fn dropping_a_fraction_into_sql_c_numeric_is_01s07() {
        let err = numeric_write(
            &ColumnValue::Decimal("1.239".to_string()),
            NumericTarget {
                precision: 5,
                scale: 2,
            },
        )
        .expect_err("a dropped fractional digit is 01S07");
        assert_eq!(
            sqlstate_of_err(&err),
            crate::types::sql_state::FRACTIONAL_TRUNCATION
        );
    }

    /// The row's third outcome: "Conversion of data would result in loss of
    /// whole (as opposed to fractional) digits" → `22003`.
    #[test]
    fn a_value_too_wide_for_sql_c_numeric_is_22003() {
        let err = numeric_write(
            &ColumnValue::Decimal("123456".to_string()),
            NumericTarget {
                precision: 3,
                scale: 0,
            },
        )
        .expect_err("six digits do not fit a declared precision of three");
        assert_eq!(
            sqlstate_of_err(&err),
            crate::types::sql_state::NUMERIC_VALUE_OUT_OF_RANGE
        );
    }

    /// The guard against over-reach: a source with no numeric reading at all is
    /// still `07006`. Only the sources the *SQL to C: Numeric* and
    /// *SQL to C: Character* tables govern gained this target.
    #[test]
    fn a_non_numeric_source_is_still_07006_for_sql_c_numeric() {
        let err = numeric_write(
            &ColumnValue::Bytes(vec![1, 2, 3]),
            NumericTarget::UNSPECIFIED,
        )
        .expect_err("bytes have no numeric reading");
        assert_eq!(
            sqlstate_of_err(&err),
            crate::types::sql_state::RESTRICTED_DATA_TYPE_ATTRIBUTE_VIOLATION
        );
    }

    /// A character column that is not a numeric literal is `22018`, the
    /// *SQL to C: Character* table's cell for it, not `07006`, which is about
    /// the type pairing rather than the value.
    #[test]
    fn a_non_numeric_string_into_sql_c_numeric_is_22018() {
        let err = numeric_write(
            &ColumnValue::String("not a number".to_string()),
            NumericTarget::UNSPECIFIED,
        )
        .expect_err("a non-literal is a value error");
        assert_eq!(
            sqlstate_of_err(&err),
            crate::types::sql_state::INVALID_CHARACTER_VALUE_FOR_CAST
        );
    }

    #[test]
    fn a_character_column_truncating_is_still_ordinary_01004() {
        // The guard against over-reach: SQL to C: Character has no 22003 row
        // at all for SQL_C_CHAR, so a genuine character column that does not
        // fit keeps returning truncated data with 01004. Only the sources the
        // SQL to C: Numeric table governs move.
        let (ret, written, ind) = char_write(&ColumnValue::String("123456".to_string()), 4);
        assert_eq!(ret, SqlReturn::SUCCESS_WITH_INFO);
        assert_eq!(&written, b"123\0");
        assert_eq!(ind, 6);
    }

    #[test]
    fn a_non_numeric_source_that_looks_numeric_is_still_01004() {
        // `ColumnValue::Bytes` renders as hex digits and `Guid` as a hex
        // string, but neither is a numeric SQL type, so neither takes the
        // numeric row. Pins that the rule keys off the source variant rather
        // than off what the rendering happens to look like.
        let (ret, written, ind) = char_write(&ColumnValue::Bytes(vec![0x12, 0x34, 0x56]), 4);
        assert_eq!(ret, SqlReturn::SUCCESS_WITH_INFO);
        assert_eq!(&written, b"123\0");
        assert_eq!(ind, 6);
    }

    #[test]
    fn every_numeric_variant_takes_the_22003_row() {
        // Enumerates the full set the rule applies to, so a new numeric
        // `ColumnValue` variant that is left out of `is_numeric_source` shows
        // up here rather than as a silently wrong number. SQL_DECIMAL,
        // SQL_NUMERIC, SQL_TINYINT, SQL_SMALLINT, SQL_INTEGER, SQL_BIGINT,
        // SQL_REAL, SQL_FLOAT and SQL_DOUBLE are the table's own list.
        //
        // Every value here is positive and every buffer is sized off its digit
        // count alone, so this test covers variant coverage only. The sign rule
        // is `a_minus_sign_occupies_a_whole_digit_position`'s job, and keeping
        // the two apart means a regression in either one names itself.
        for (value, char_buf_len) in [
            (ColumnValue::I8(127), 3),
            (ColumnValue::I16(32_767), 4),
            (ColumnValue::I32(2_000_000_000), 4),
            (ColumnValue::I64(123_456), 4),
            (ColumnValue::F32(123_456.0), 4),
            (ColumnValue::F64(123_456.0), 4),
            (ColumnValue::Decimal("123456".to_string()), 4),
        ] {
            assert_22003_writes_nothing(&value, CDataType::Char, char_buf_len);
            assert_22003_writes_nothing(&value, CDataType::WChar, char_buf_len * 2);
        }
    }

    #[test]
    fn a_non_finite_float_has_no_fraction_to_sacrifice() {
        // "Infinity" and "NaN" carry no decimal point, so the whole of the
        // rendering is the whole part and any truncation at all is whole-part
        // loss. That falls out of the same rule with no special case: "Inf"
        // is not a value the application can read back as a number.
        assert_22003_writes_nothing(&ColumnValue::F64(f64::INFINITY), CDataType::Char, 5);
        assert_22003_writes_nothing(&ColumnValue::F64(f64::NAN), CDataType::Char, 3);

        // Exactly enough room, so no truncation: "NaN" is three bytes plus a
        // terminator.
        let (ret, written, _) = char_write(&ColumnValue::F64(f64::NAN), 4);
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!(&written, b"NaN\0");
    }

    #[test]
    fn a_zero_length_buffer_on_a_numeric_column_stays_a_length_probe() {
        // The carve-out. Read literally, the table's third row would fire here,
        // every number has at least one whole digit, so "whole digits >=
        // BufferLength" holds for BufferLength 0, but the row exists to stop a
        // wrong *number* reaching the application's buffer, and there is no
        // buffer to reach. `SQLGetData`'s own prose protects this call
        // (`HY090` when BufferLength is less than 0 but not when it is 0), and
        // both reference drivers special-case it: psqlODBC's
        // `setup_getdataclass` branches on `cbValueMax == 0` with the comment
        // "just returns length info", and MySQL Connector/ODBC does the same in
        // `utility.cc`.
        //
        // So a numeric column probes exactly as a character one does: 01004,
        // nothing written, and the full length in the indicator so the
        // application can size its buffer and call again.
        let (ret, _, ind) = char_write(&ColumnValue::I64(123_456), 0);
        assert_eq!(ret, SqlReturn::SUCCESS_WITH_INFO);
        assert_eq!(ind, 6, "the probe must still report the length it needs");

        let mut buf = [0xAAu16; 8];
        let mut ind: isize = 0;
        let ret = unsafe {
            write_column_value(
                &ColumnValue::I64(123_456),
                CDataType::WChar,
                buf.as_mut_ptr().cast::<c_void>(),
                0,
                &mut ind,
                NumericTarget::UNSPECIFIED,
            )
        };
        assert_eq!(ret.unwrap(), SqlReturn::SUCCESS_WITH_INFO);
        assert_eq!(ind, 12);
        assert_eq!(buf, [0xAAu16; 8], "wrote into a zero-length buffer");
    }

    #[test]
    fn leading_padding_in_a_decimal_is_reserved_because_it_is_written() {
        // A backend may hand core a `Decimal` carrying a leading space or `+`.
        // `whole_part` counts it, and that is right rather than an over-count:
        // core writes a `Decimal`'s text through verbatim, so the character
        // occupies a byte of the buffer exactly as a digit does. This pins the
        // property that makes it right: what is reserved is what is written.
        let (ret, written, ind) = char_write(&ColumnValue::Decimal(" 123.45".to_string()), 5);
        assert_eq!(ret, SqlReturn::SUCCESS_WITH_INFO);
        assert_eq!(
            &written, b" 123\0",
            "the leading space is written, so reserving room for it is correct"
        );
        assert_eq!(ind, 7);

        // One byte less and the whole part does not fit.
        assert_22003_writes_nothing(
            &ColumnValue::Decimal(" 123.45".to_string()),
            CDataType::Char,
            4,
        );
    }

    #[test]
    fn a_one_byte_char_buffer_is_still_22003() {
        // The other side of the carve-out, and the reason it is drawn at "the
        // writer would write nothing" rather than at "the buffer is small".
        // One byte is a real buffer: `write_char` would put a bare null
        // terminator in it and report 01004, delivering "" for a number. That
        // is the wrong number, which is what the row prevents.
        assert_22003_writes_nothing(&ColumnValue::I64(123_456), CDataType::Char, 1);
        // The SQL_C_WCHAR counterpart needs two bytes before a terminator fits,
        // so one byte is the exempt case there and two is the first checked one.
        assert_22003_writes_nothing(&ColumnValue::I64(123_456), CDataType::WChar, 2);
    }

    #[test]
    fn a_null_target_with_a_live_indicator_is_never_22003() {
        // The indicator-only binding, at this layer: `SQLBindCol` with a null
        // data pointer and a live length/indicator pointer, which the spec
        // permits in as many words ("An application can unbind the data buffer
        // for a column but still have a length/indicator buffer bound for the
        // column"). There is no buffer for a wrong number to reach, so the row
        // does not apply and the length is still reported.
        //
        // `buf_len` is swept because a binding of this shape can carry any
        // octet length; none of them may turn into an error.
        for buf_len in [0, 1, 2, 4, 64] {
            let mut ind: isize = 0;
            let ret = unsafe {
                write_column_value(
                    &ColumnValue::I64(123_456),
                    CDataType::Char,
                    std::ptr::null_mut(),
                    buf_len,
                    &mut ind,
                    NumericTarget::UNSPECIFIED,
                )
            };
            assert_eq!(
                ret.unwrap(),
                SqlReturn::SUCCESS,
                "null target with buf_len {buf_len}"
            );
            assert_eq!(ind, 6, "null target with buf_len {buf_len}");
        }
    }

    #[test]
    fn a_bool_takes_the_bit_table_not_the_numeric_one() {
        // SQL_BIT is its own table (SQL to C: Bit), whose SQL_C_CHAR row reads
        // "BufferLength > 1" / "BufferLength <= 1" -> 22003. Core does not
        // implement that row yet, so a `Bool` in a one-byte buffer still
        // reports 01004; pinning it keeps this change honest about its scope
        // rather than letting a reader assume SQL_BIT moved with the numerics.
        let (ret, _, ind) = char_write(&ColumnValue::Bool(true), 1);
        assert_eq!(ret, SqlReturn::SUCCESS_WITH_INFO);
        assert_eq!(ind, 1);
    }
}
