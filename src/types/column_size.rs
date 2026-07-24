//! Shared ODBC column-size formulas.
//!
//! Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/appendixes/column-size-decimal-digits-transfer-octet-length-and-display-size>
//! and its "Column Size" sub-page:
//! <https://learn.microsoft.com/en-us/sql/odbc/reference/appendixes/column-size>
//!
//! ODBC defines two genuinely different "column size" quantities, and this
//! module deliberately gives them different names so a caller cannot use one
//! where the other belongs — that exact confusion (using a data source's
//! *maximum* supported precision where one column's own *declared* precision
//! was meant, or vice versa) must not recur:
//!
//! - [`column_size`] — the quantity `SQLColumns`, `SQLDescribeCol`, and
//!   `SQLColAttributeW` report for one already-declared, concrete column,
//!   computed from *that column's own* precision/scale.
//! - [`catalog_column_size`] — the quantity `SQLGetTypeInfo` reports: per its
//!   own spec text, "the maximum column size that the server supports for
//!   this data type" — computed from the *data source's maximum supported*
//!   precision/scale, which is a backend property the caller must supply.
//!   [`MaxPrecision`]/[`MaxScale`] are newtypes specifically so this maximum
//!   can never be passed as a plain, easily-confused `i32`/`i16`.
//!
//! Both public functions forward to the same private formula
//! ([`column_size_formula`]) — the only difference between them is which
//! quantity the caller is obligated to supply. `stackable-odbc-core` has no notion of
//! any particular backend's actual maximum precision (e.g. a backend's
//! fractional-seconds digits, or a DECIMAL type's maximum precision) — that
//! is always supplied by the caller, never hardcoded here.

use crate::types::SqlDataType;
use odbc_sys::NO_TOTAL;

/// Sentinel a backend writes into `ColumnDescriptor::precision` (`u32`) for
/// a variable-length column whose actual length it cannot determine, e.g.
/// a computed expression with no catalog entry to read a declared length
/// from (`SELECT CAST(x AS VARCHAR)` with no `information_schema` row).
///
/// Spec (ODBC "Column Size"/"Display Size" appendices, footnotes a/b):
/// "If the driver cannot determine the column or parameter length for a
/// variable type, it returns SQL_NO_TOTAL." `SQL_NO_TOTAL` (`odbc_sys::NO_TOTAL`,
/// `-4`) is a signed `Len`/`ULen`-width value and does not fit in the `u32`
/// `precision` is stored as, so this is a distinct fixed `u32` value instead.
/// [`resolve_precision_isize`]/[`resolve_precision_ulen`] are the two
/// places that translate it back to the real `NO_TOTAL` constant for the
/// pointer-width descriptor fields that actually carry it
/// (`SQL_DESC_LENGTH`/`SQL_DESC_DISPLAY_SIZE` via `SQLColAttributeW`,
/// `SQLDescribeCol`'s `ColumnSizePtr`).
///
/// Deliberately `u32::MAX`, not `i32::MAX`: `i32::MAX` is already an
/// established, different convention in the driver crates for "unbounded but
/// reportable as a literal number" (an undeclared BLOB/VARCHAR's default
/// column size). That value must keep being reported as the literal
/// `2147483647`, not reinterpreted as "undeterminable". Using a value
/// outside `i32`'s range entirely keeps the two conventions from colliding.
pub const PRECISION_UNDETERMINABLE: u32 = u32::MAX;

/// Reinterpret a `ColumnDescriptor::precision` value as the `isize`
/// `SQL_DESC_LENGTH`/`SQL_DESC_DISPLAY_SIZE` (`SQLColAttributeW`) actually
/// want, substituting `odbc_sys::NO_TOTAL` for the
/// [`PRECISION_UNDETERMINABLE`] sentinel.
pub fn resolve_precision_isize(precision: u32) -> isize {
    if precision == PRECISION_UNDETERMINABLE {
        NO_TOTAL
    } else {
        precision as isize
    }
}

/// Reinterpret a `ColumnDescriptor::precision` value as the `ULen`
/// `SQLDescribeCol`'s `ColumnSizePtr` actually wants, substituting
/// `odbc_sys::NO_TOTAL` for the [`PRECISION_UNDETERMINABLE`] sentinel. `Len`
/// and `ULen` are the same pointer width, so `NO_TOTAL`'s bit pattern is
/// preserved by the `as` cast the same way `SQL_NULL_DATA`/other signed
/// indicator sentinels already are elsewhere at this boundary.
pub fn resolve_precision_ulen(precision: u32) -> odbc_sys::ULen {
    if precision == PRECISION_UNDETERMINABLE {
        NO_TOTAL as odbc_sys::ULen
    } else {
        precision as odbc_sys::ULen
    }
}

/// A data source's maximum supported precision for a parametric SQL type
/// (e.g. the maximum fractional-seconds digits a backend's TIME/TIMESTAMP
/// columns can carry, or the maximum DECIMAL precision). A newtype rather
/// than a plain `i32` so [`catalog_column_size`] cannot be accidentally
/// called with one column's own declared precision instead of the data
/// source's maximum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaxPrecision(pub i32);

/// A data source's maximum supported scale. See [`MaxPrecision`]: the same
/// mix-up risk applies to scale (e.g. maximum fractional-seconds digits).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaxScale(pub i16);

/// Per-column `COLUMN_SIZE` / `SQL_DESC_LENGTH`: the quantity reported by
/// `SQLColumns`, `SQLDescribeCol`, and `SQLColAttributeW` for one concrete,
/// already-declared column, using *that column's own* precision/scale.
///
/// `const fn` so each driver's `SQLGetTypeInfo` tables (`static` arrays) can
/// call it directly rather than duplicating its output as literals.
///
/// Spec ("Column Size" appendix): see module docs.
pub const fn column_size(sql_type: SqlDataType, precision: i32, scale: i16) -> i32 {
    column_size_formula(sql_type, precision, scale)
}

/// Catalog `COLUMN_SIZE` — the quantity reported by `SQLGetTypeInfo`:
/// "the maximum column size that the server supports for this data type",
/// evaluated at the data source's maximum supported precision/scale. That
/// maximum is a backend property and must be supplied by the caller; it is
/// never hardcoded in `stackable-odbc-core`.
///
/// Spec ("Column Size" appendix, and `SQLGetTypeInfo`'s own COLUMN_SIZE
/// description): see module docs.
pub const fn catalog_column_size(
    sql_type: SqlDataType,
    max_precision: MaxPrecision,
    max_scale: MaxScale,
) -> i32 {
    column_size_formula(sql_type, max_precision.0, max_scale.0)
}

/// The "Column Size" appendix table, transcribed as code. `precision`/`scale`
/// are used only by the SQL types whose column size the appendix actually
/// defines in terms of them (character/binary types pass `precision`
/// straight through as their length; `DECIMAL`/`NUMERIC` likewise use
/// `precision` as "the defined number of digits"; `TIME`/`TIMESTAMP` use
/// `scale` for the fractional-seconds formula). Every other type in this
/// table is a fixed constant per the appendix and ignores both arguments.
const fn column_size_formula(sql_type: SqlDataType, precision: i32, scale: i16) -> i32 {
    match sql_type {
        // "All character types ... The defined or maximum column size in
        // characters of the column or parameter."
        // "All binary types ... The defined or maximum length in bytes."
        SqlDataType::CHAR
        | SqlDataType::VARCHAR
        | SqlDataType::EXT_LONG_VARCHAR
        | SqlDataType::EXT_W_CHAR
        | SqlDataType::EXT_W_VARCHAR
        | SqlDataType::EXT_W_LONG_VARCHAR
        | SqlDataType::EXT_BINARY
        | SqlDataType::EXT_VAR_BINARY
        | SqlDataType::EXT_LONG_VAR_BINARY => precision,

        // "SQL_DECIMAL, SQL_NUMERIC: The defined number of digits. For
        // example, the precision of a column defined as NUMERIC(10,3) is 10."
        SqlDataType::DECIMAL | SqlDataType::NUMERIC => precision,

        // Fixed constants, one row per appendix entry.
        SqlDataType::EXT_BIT => 1,
        SqlDataType::EXT_TINY_INT => 3,
        SqlDataType::SMALLINT => 5,
        SqlDataType::INTEGER => 10,
        // "19 (if signed) or 20 (if unsigned)". This codebase never reports
        // an unsigned BIGINT column (see `is_unsigned` in `col_attr.rs`,
        // which treats every numeric type as signed), so only the signed
        // case applies.
        SqlDataType::EXT_BIG_INT => 19,
        SqlDataType::REAL => 7,
        SqlDataType::FLOAT | SqlDataType::DOUBLE => 15,
        SqlDataType::DATE => 10,

        // "SQL_TYPE_TIME: 8 (the number of characters in the hh-mm-ss
        // format), or 9 + s (the number of characters in the
        // hh:mm:ss[.fff...] format, where s is the seconds precision)."
        SqlDataType::TIME => {
            if scale == 0 {
                8
            } else {
                // Widening i16 -> i32 cannot truncate; `i32::from` is not yet
                // usable in a const fn (its `From` impl is not const-stable),
                // so this uses `as` instead of the workspace's usual
                // try_from-over-as preference, which targets truncating casts.
                9 + scale as i32
            }
        }

        // "SQL_TYPE_TIMESTAMP: 16 (yyyy-mm-dd hh:mm) ... 19 (yyyy-mm-dd
        // hh:mm:ss) ... or 20 + s (yyyy-mm-dd hh:mm:ss[.fff...], where s is
        // the seconds precision)." This driver family never omits the
        // seconds field, so only the 19 / 20+s cases apply.
        SqlDataType::TIMESTAMP => {
            if scale == 0 {
                19
            } else {
                20 + scale as i32 // see the TIME arm above re: `as` vs `i32::from`
            }
        }

        // "SQL_GUID: 36 (the number of characters in the
        // aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee format)." The driver crates do
        // not report SQL_GUID today (they represent UUID/GUID values as
        // VARCHAR text; see a driver's `type_conversion.rs`), but the constant
        // is recognised elsewhere (`col_attr.rs`'s `type_name_for`), so this
        // row is included for completeness rather than silently falling
        // through to the passthrough default below.
        SqlDataType::EXT_GUID => 36,

        // No formula-governed type in the driver crates' catalogs falls here
        // today (interval types are represented as VARCHAR text; see a
        // driver's `type_conversion.rs`), but a precision passthrough is the
        // least surprising default for any future SQL type this table does
        // not yet enumerate.
        _ => precision,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every row transcribed directly from the ODBC "Column Size" appendix:
    /// <https://learn.microsoft.com/en-us/sql/odbc/reference/appendixes/column-size>
    /// `(SqlDataType, precision, scale, expected column size)`. This is the
    /// deliberate exception to the "no raw literals for spec-defined values"
    /// rule: these numbers *are* the spec, transcribed for comparison, not
    /// values this driver invented.
    #[rustfmt::skip]
    const APPENDIX_COLUMN_SIZE_CASES: &[(SqlDataType, i32, i16, i32)] = &[
        // "All character types ... column size of a single-byte character
        // column defined as CHAR(10) is 10."
        (SqlDataType::CHAR, 10, 0, 10),
        (SqlDataType::VARCHAR, 50, 0, 50),
        (SqlDataType::EXT_LONG_VARCHAR, 1_000, 0, 1_000),
        (SqlDataType::EXT_W_CHAR, 10, 0, 10),
        (SqlDataType::EXT_W_VARCHAR, 50, 0, 50),
        (SqlDataType::EXT_W_LONG_VARCHAR, 1_000, 0, 1_000),
        // "SQL_DECIMAL, SQL_NUMERIC ... NUMERIC(10,3) is 10."
        (SqlDataType::DECIMAL, 10, 3, 10),
        (SqlDataType::NUMERIC, 10, 3, 10),
        // Fixed constants.
        (SqlDataType::EXT_BIT, 0, 0, 1),
        (SqlDataType::EXT_TINY_INT, 0, 0, 3),
        (SqlDataType::SMALLINT, 0, 0, 5),
        (SqlDataType::INTEGER, 0, 0, 10),
        (SqlDataType::EXT_BIG_INT, 0, 0, 19),
        (SqlDataType::REAL, 0, 0, 7),
        (SqlDataType::FLOAT, 0, 0, 15),
        (SqlDataType::DOUBLE, 0, 0, 15),
        // "All binary types ... length of a column defined as BINARY(10) is 10."
        (SqlDataType::EXT_BINARY, 10, 0, 10),
        (SqlDataType::EXT_VAR_BINARY, 255, 0, 255),
        (SqlDataType::EXT_LONG_VAR_BINARY, 1_000_000, 0, 1_000_000),
        // "SQL_TYPE_DATE ... 10 (yyyy-mm-dd)."
        (SqlDataType::DATE, 0, 0, 10),
        // "SQL_TYPE_TIME ... 8 (hh-mm-ss), or 9 + s (hh:mm:ss[.fff...])."
        (SqlDataType::TIME, 0, 0, 8),
        (SqlDataType::TIME, 0, 3, 12),   // 9 + 3
        (SqlDataType::TIME, 0, 12, 21),  // 9 + 12 (a 12-digit fractional-seconds precision)
        // "SQL_TYPE_TIMESTAMP ... 19 (yyyy-mm-dd hh:mm:ss) or 20 + s."
        (SqlDataType::TIMESTAMP, 0, 0, 19),
        (SqlDataType::TIMESTAMP, 0, 3, 23),  // 20 + 3
        (SqlDataType::TIMESTAMP, 0, 12, 32), // 20 + 12
    ];

    #[test]
    fn column_size_matches_the_odbc_appendix_table() {
        for &(ty, precision, scale, expected) in APPENDIX_COLUMN_SIZE_CASES {
            assert_eq!(
                column_size(ty, precision, scale),
                expected,
                "column_size({ty:?}, precision={precision}, scale={scale})"
            );
        }
    }

    /// `catalog_column_size` must reproduce the exact same appendix table as
    /// `column_size` when given the same numbers via `MaxPrecision`/
    /// `MaxScale`: the two functions differ only in which quantity the
    /// caller is obligated to supply, never in the formula itself.
    #[test]
    fn catalog_column_size_matches_the_same_appendix_table() {
        for &(ty, precision, scale, expected) in APPENDIX_COLUMN_SIZE_CASES {
            assert_eq!(
                catalog_column_size(ty, MaxPrecision(precision), MaxScale(scale)),
                expected,
                "catalog_column_size({ty:?}, max_precision={precision}, max_scale={scale})"
            );
        }
    }

    #[test]
    fn catalog_column_size_and_column_size_never_diverge_for_the_same_inputs() {
        // Pinning this down directly (rather than relying on both tests above
        // separately matching the same table) guards against the two public
        // functions ever being given independent formula implementations
        // that could drift apart: the whole point of routing both through
        // `column_size_formula`.
        for &(ty, precision, scale, _) in APPENDIX_COLUMN_SIZE_CASES {
            assert_eq!(
                column_size(ty, precision, scale),
                catalog_column_size(ty, MaxPrecision(precision), MaxScale(scale)),
                "column_size and catalog_column_size diverged for {ty:?} \
                 precision={precision} scale={scale}"
            );
        }
    }

    #[test]
    fn resolve_precision_isize_substitutes_no_total_for_the_sentinel() {
        // A variable-length column whose actual length a backend cannot
        // determine reports `PRECISION_UNDETERMINABLE`, which must come back
        // out as `SQL_NO_TOTAL` (-4), not the sentinel's own huge `u32` bit
        // pattern reinterpreted as a positive `isize`.
        assert_eq!(resolve_precision_isize(PRECISION_UNDETERMINABLE), NO_TOTAL);
        assert_eq!(NO_TOTAL, -4, "SQL_NO_TOTAL is defined as -4");
    }

    #[test]
    fn resolve_precision_isize_passes_through_every_real_value() {
        for precision in [0u32, 1, 255, i32::MAX as u32, u32::MAX - 1] {
            assert_eq!(
                resolve_precision_isize(precision),
                precision as isize,
                "precision={precision} should pass through unchanged"
            );
        }
    }

    #[test]
    fn resolve_precision_ulen_substitutes_no_total_for_the_sentinel() {
        assert_eq!(
            resolve_precision_ulen(PRECISION_UNDETERMINABLE),
            NO_TOTAL as odbc_sys::ULen
        );
    }

    #[test]
    fn resolve_precision_ulen_passes_through_every_real_value() {
        for precision in [0u32, 1, 255, i32::MAX as u32, u32::MAX - 1] {
            assert_eq!(
                resolve_precision_ulen(precision),
                precision as odbc_sys::ULen,
                "precision={precision} should pass through unchanged"
            );
        }
    }
}
