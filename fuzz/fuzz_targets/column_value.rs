#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use stackable_odbc_core::column_value::write_column_value;
use stackable_odbc_core::types::{CDataType, ColumnValue};
use std::ffi::c_void;

// Fuzzes the full write_column_value coercion matrix — every marshallable
// ColumnValue variant against every interesting C target type — the pointer
// marshalling and narrowing/formatting code where a buffer overrun would live.
// The output buffer is allocated at exactly the declared length, so
// AddressSanitizer reports any write past it. Nested container variants
// (Array/Map/Row) are omitted: they are not marshalled by SQLGetData.
#[derive(Arbitrary, Debug)]
enum FuzzValue {
    Null,
    String(String),
    I8(i8),
    I16(i16),
    I32(i32),
    I64(i64),
    F32(f32),
    F64(f64),
    Bool(bool),
    Date { year: i16, month: u16, day: u16 },
    Time { hour: u16, minute: u16, second: u16, fraction: u32 },
    Timestamp { year: i16, month: u16, day: u16, hour: u16, minute: u16, second: u16, fraction: u32 },
    Bytes(Vec<u8>),
    Guid([u8; 16]),
    Decimal(String),
    TimestampTz { year: i16, month: u16, day: u16, hour: u16, minute: u16, second: u16, fraction: u32, timezone_offset_minutes: i16 },
    Json(String),
    IntervalYearMonth { years: i32, months: i32 },
    IntervalDayTime { total_milliseconds: i64 },
}

impl From<FuzzValue> for ColumnValue {
    fn from(v: FuzzValue) -> ColumnValue {
        match v {
            FuzzValue::Null => ColumnValue::Null,
            FuzzValue::String(s) => ColumnValue::String(s),
            FuzzValue::I8(n) => ColumnValue::I8(n),
            FuzzValue::I16(n) => ColumnValue::I16(n),
            FuzzValue::I32(n) => ColumnValue::I32(n),
            FuzzValue::I64(n) => ColumnValue::I64(n),
            FuzzValue::F32(n) => ColumnValue::F32(n),
            FuzzValue::F64(n) => ColumnValue::F64(n),
            FuzzValue::Bool(b) => ColumnValue::Bool(b),
            FuzzValue::Date { year, month, day } => ColumnValue::Date { year, month, day },
            FuzzValue::Time { hour, minute, second, fraction } => {
                ColumnValue::Time { hour, minute, second, fraction }
            }
            FuzzValue::Timestamp { year, month, day, hour, minute, second, fraction } => {
                ColumnValue::Timestamp { year, month, day, hour, minute, second, fraction }
            }
            FuzzValue::Bytes(b) => ColumnValue::Bytes(b),
            FuzzValue::Guid(g) => ColumnValue::Guid(g),
            FuzzValue::Decimal(s) => ColumnValue::Decimal(s),
            FuzzValue::TimestampTz {
                year, month, day, hour, minute, second, fraction, timezone_offset_minutes,
            } => ColumnValue::TimestampTz {
                year, month, day, hour, minute, second, fraction, timezone_offset_minutes,
            },
            FuzzValue::Json(s) => ColumnValue::Json(s),
            FuzzValue::IntervalYearMonth { years, months } => {
                ColumnValue::IntervalYearMonth { years, months }
            }
            FuzzValue::IntervalDayTime { total_milliseconds } => {
                ColumnValue::IntervalDayTime { total_milliseconds }
            }
        }
    }
}

// The interesting C target types SQLGetData can be asked to produce.
const TARGETS: &[CDataType] = &[
    CDataType::WChar,
    CDataType::Char,
    CDataType::SLong,
    CDataType::ULong,
    CDataType::SShort,
    CDataType::UShort,
    CDataType::STinyInt,
    CDataType::UTinyInt,
    CDataType::SBigInt,
    CDataType::UBigInt,
    CDataType::Float,
    CDataType::Double,
    CDataType::Bit,
    CDataType::Binary,
    CDataType::Guid,
    CDataType::TypeDate,
    CDataType::TypeTime,
    CDataType::TypeTimestamp,
    CDataType::Numeric,
    CDataType::Default,
];

#[derive(Arbitrary, Debug)]
struct Input {
    value: FuzzValue,
    target: u8,
    buf_len: u8,
}

fuzz_target!(|input: Input| {
    let value: ColumnValue = input.value.into();
    let target = TARGETS[input.target as usize % TARGETS.len()];
    let buf_len = input.buf_len as isize;

    // Allocation size and the `BufferLength` argument are decoupled, exactly as
    // in a real ODBC application:
    //
    // - Variable-length C targets (CHAR/WCHAR/BINARY) honour `BufferLength`, so
    //   an exact-size buffer lets ASAN catch a write past it (a buffer overrun).
    // - Fixed-length C targets ignore `BufferLength` per the ODBC spec — an app
    //   supplies a variable of the C type's size and may pass any `BufferLength`
    //   (0 is common and legal), so buffer size is independent of the argument.
    //   Give those a buffer sized to the C type; writing `sizeof(T)` into a
    //   smaller one would be the app's contract violation, not a driver bug.
    //   256 bytes covers every fixed ODBC C type (SQL_C_NUMERIC and the interval
    //   structs, the largest, are well under it), so a write past it would be a
    //   genuine defect.
    // - SQL_C_DEFAULT is neither, and giving it the blanket 256 bytes is what
    //   hid a real heap overflow from this fuzzer: the *driver* picks the C
    //   type there, and it picks from the runtime `ColumnValue` variant rather
    //   than from the `sql_type` the application sized its buffer against, so
    //   `BufferLength` is the only bound that exists. A positive `BufferLength`
    //   must therefore be honoured and gets an exact-size allocation. Zero is
    //   the documented exemption — it is how an application says "not
    //   applicable" for a fixed C type, so it carries no size information — and
    //   gets the fixed-type allocation instead.
    let variable_len = matches!(
        target,
        CDataType::WChar | CDataType::Char | CDataType::Binary
    );
    let bounded_by_buf_len = variable_len || (target == CDataType::Default && buf_len > 0);
    let alloc = if bounded_by_buf_len {
        input.buf_len as usize
    } else {
        256
    };
    // Deliberately misaligned, not incidentally aligned. `vec![0u8; alloc]`
    // handed over `buf.as_mut_ptr()` directly, which the allocator returns
    // suitably aligned for anything this writes, so no run ever exercised the
    // unaligned path and the `write_unaligned` calls were taken on trust. An
    // arena of the widest-*alignment* target type (8 bytes: `i64`, `f64`,
    // `SQLLEN`) offset by one byte is misaligned for every target on every
    // platform, and one extra element keeps `alloc` bytes writable past the
    // offset.
    let mut arena = vec![0u64; alloc / 8 + 2];
    let mut ind_arena = [0isize; 2];
    // SAFETY: both offsets stay inside their own allocation, and every write
    // through them is an unaligned write.
    unsafe {
        let buf = arena.as_mut_ptr().cast::<u8>().add(1);
        let ind = ind_arena.as_mut_ptr().cast::<u8>().add(1).cast::<isize>();
        debug_assert!(!ind.is_aligned(), "the point of the offset");
        let _ = write_column_value(&value, target, buf as *mut c_void, buf_len, ind);
    }
});
