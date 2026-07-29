//! Column attribute derivation for SQLColAttributeW.
//!
//! Maps ODBC descriptor field identifiers ([`odbc_sys::Desc`]) to values
//! derived from [`ColumnDescriptor`].

use odbc_sys::Desc;

use crate::errors::OdbcError;
use crate::types::{
    ColumnDescriptor, SQL_ATTR_READWRITE_UNKNOWN, SQL_FALSE, SQL_NAMED, SQL_UNNAMED, SqlDataType,
    resolve_precision_isize,
};

/// The result of a column attribute query, either a string or a numeric value.
#[derive(Debug, PartialEq)]
#[non_exhaustive]
pub enum ColAttrValue {
    /// A string value (returned via CharacterAttributePtr).
    String(String),
    /// A numeric value (returned via NumericAttributePtr).
    Numeric(isize),
}

/// Derive a column attribute from a [`ColumnDescriptor`].
///
/// `column_count` is the total number of columns in the result set (for `Desc::Count`).
/// `field` is the ODBC descriptor field identifier.
pub fn get_column_attribute(
    desc: &ColumnDescriptor,
    column_count: i16,
    field: Desc,
) -> Result<ColAttrValue, OdbcError> {
    match field {
        // --- String attributes ---
        Desc::Name | Desc::Label | Desc::BaseColumnName => {
            Ok(ColAttrValue::String(desc.name.clone()))
        }
        Desc::TypeName => Ok(ColAttrValue::String(if desc.type_name.is_empty() {
            type_name_for(desc.sql_type).to_string()
        } else {
            desc.type_name.clone()
        })),
        // Taken from the descriptor, so they are empty unless the backend
        // populated them. A backend that tracks a column's origin reports it
        // here; one that does not gets the empty string, rather than core
        // inventing a name for a table it knows nothing about.
        Desc::TableName => Ok(ColAttrValue::String(desc.table_name.clone())),
        Desc::SchemaName => Ok(ColAttrValue::String(desc.schema_name.clone())),
        Desc::CatalogName => Ok(ColAttrValue::String(desc.catalog_name.clone())),
        Desc::LocalTypeName => Ok(ColAttrValue::String(String::new())),
        Desc::BaseTableName => Ok(ColAttrValue::String(desc.table_name.clone())),
        Desc::LiteralPrefix => Ok(ColAttrValue::String(desc.literal_prefix.clone())),
        Desc::LiteralSuffix => Ok(ColAttrValue::String(desc.literal_suffix.clone())),

        // --- Numeric attributes ---
        Desc::Count => Ok(ColAttrValue::Numeric(isize::from(column_count))),
        // `SQL_DESC_CONCISE_TYPE` is the concise type; `SQL_DESC_TYPE` is the
        // *verbose* one, which differs for exactly the datetime and interval
        // families: the spec has `SQL_DESC_TYPE` report `SQL_DATETIME` or
        // `SQL_INTERVAL` there, with the concise type moving to
        // `SQL_DESC_DATETIME_INTERVAL_CODE`. For every other type the two are
        // the same value.
        Desc::ConciseType => Ok(ColAttrValue::Numeric(desc.sql_type.0 as isize)),
        Desc::Type => Ok(ColAttrValue::Numeric(isize::from(verbose_type(
            desc.sql_type,
        )))),
        Desc::DatetimeIntervalCode => Ok(ColAttrValue::Numeric(
            if verbose_type(desc.sql_type) == desc.sql_type.0 {
                // Not a datetime or interval type: the spec leaves this field
                // undefined, and 0 is the value an application tests against.
                0
            } else {
                isize::from(desc.sql_type.0)
            },
        )),
        Desc::Precision => Ok(ColAttrValue::Numeric(precision_for(desc))),
        Desc::Length => Ok(ColAttrValue::Numeric(resolve_precision_isize(
            desc.precision,
        ))),
        Desc::Scale => Ok(ColAttrValue::Numeric(desc.scale as isize)),
        // All three spec values, not the two a `bool` could carry.
        Desc::Nullable => Ok(ColAttrValue::Numeric(desc.nullable as isize)),
        Desc::DisplaySize => Ok(ColAttrValue::Numeric(display_size_for(desc))),
        Desc::OctetLength => Ok(ColAttrValue::Numeric(octet_length_for(desc))),
        Desc::Unsigned => Ok(ColAttrValue::Numeric(if is_unsigned(desc.sql_type) {
            1
        } else {
            0
        })),
        Desc::Searchable => Ok(ColAttrValue::Numeric(isize::from(desc.searchable))),
        Desc::AutoUniqueValue => Ok(ColAttrValue::Numeric(SQL_FALSE as isize)),
        Desc::Updatable => Ok(ColAttrValue::Numeric(SQL_ATTR_READWRITE_UNKNOWN as isize)),
        Desc::FixedPrecScale => Ok(ColAttrValue::Numeric(SQL_FALSE as isize)),
        Desc::CaseSensitive => Ok(ColAttrValue::Numeric(if is_character_type(desc.sql_type) {
            1
        } else {
            0
        })),
        Desc::NumPrecRadix => {
            let radix = if is_numeric_type(desc.sql_type) {
                10
            } else {
                0
            };
            Ok(ColAttrValue::Numeric(radix))
        }
        Desc::Unnamed => Ok(ColAttrValue::Numeric(if desc.name.is_empty() {
            SQL_UNNAMED
        } else {
            SQL_NAMED
        })),

        _ => Err(OdbcError::NotImplemented {
            feature: format!("SQLColAttribute field {:?}", field),
        }),
    }
}

/// The *verbose* type for `sql_type`, as `SQL_DESC_TYPE` reports it.
///
/// Equal to the concise type for everything except the datetime and interval
/// families, which the spec collapses to [`SQL_DATETIME`] and [`SQL_INTERVAL`].
fn verbose_type(sql_type: SqlDataType) -> i16 {
    use crate::types::{SQL_DATETIME, SQL_INTERVAL};
    match sql_type.0 {
        // SQL_TYPE_DATE (91) through SQL_TYPE_TIMESTAMP_WITH_TIMEZONE (95).
        //
        // Only the ODBC 3.x concise codes. The 2.x spellings `SQL_DATE` (9),
        // `SQL_TIME` (10) and `SQL_TIMESTAMP` (11) are deliberately not mapped:
        // 9 and 10 are the *verbose* SQL_DATETIME and SQL_INTERVAL values
        // themselves, so treating them as concise datetime types would make
        // this function ambiguous. Core reports ODBC 3.80, and a descriptor
        // built with the 3.x codes is what that entails.
        91..=95 => SQL_DATETIME,
        // The SQL_INTERVAL_* concise codes, SQL_INTERVAL_YEAR (101) through
        // SQL_INTERVAL_MINUTE_TO_SECOND (113).
        101..=113 => SQL_INTERVAL,
        other => other,
    }
}

/// Generic SQL type name for a `SqlDataType`.
///
/// Used only when the backend supplied no native type name. Spec,
/// `SQL_DESC_TYPE_NAME`: "If the type is unknown, an empty string is returned."
fn type_name_for(sql_type: SqlDataType) -> &'static str {
    if sql_type == SqlDataType::CHAR {
        "CHAR"
    } else if sql_type == SqlDataType::VARCHAR {
        "VARCHAR"
    } else if sql_type == SqlDataType::EXT_LONG_VARCHAR {
        "LONGVARCHAR"
    } else if sql_type == SqlDataType::EXT_W_CHAR {
        "WCHAR"
    } else if sql_type == SqlDataType::EXT_W_VARCHAR {
        "WVARCHAR"
    } else if sql_type == SqlDataType::EXT_W_LONG_VARCHAR {
        "WLONGVARCHAR"
    } else if sql_type == SqlDataType::DECIMAL {
        "DECIMAL"
    } else if sql_type == SqlDataType::NUMERIC {
        "NUMERIC"
    } else if sql_type == SqlDataType::SMALLINT {
        "SMALLINT"
    } else if sql_type == SqlDataType::INTEGER {
        "INTEGER"
    } else if sql_type == SqlDataType::REAL {
        "REAL"
    } else if sql_type == SqlDataType::FLOAT {
        "FLOAT"
    } else if sql_type == SqlDataType::DOUBLE {
        "DOUBLE"
    } else if sql_type == SqlDataType::EXT_BIG_INT {
        "BIGINT"
    } else if sql_type == SqlDataType::EXT_TINY_INT {
        "TINYINT"
    } else if sql_type == SqlDataType::EXT_BIT {
        "BIT"
    } else if sql_type == SqlDataType::EXT_BINARY {
        "BINARY"
    } else if sql_type == SqlDataType::EXT_VAR_BINARY {
        "VARBINARY"
    } else if sql_type == SqlDataType::EXT_LONG_VAR_BINARY {
        "LONGVARBINARY"
    // `odbc_sys` names 9 `DATETIME` after the *verbose* `SQL_DATETIME`, but as
    // a concise type — which is what a backend reports for a column — 9 is the
    // ODBC 2.0 `SQL_DATE`. See `verbose_type` below for the other direction,
    // where 9 is deliberately not treated as a concise datetime code at all.
    } else if sql_type == SqlDataType::DATE || sql_type == SqlDataType::DATETIME {
        "DATE"
    } else if sql_type == SqlDataType::TIME || sql_type == SqlDataType::EXT_TIME_OR_INTERVAL {
        "TIME"
    } else if sql_type == SqlDataType::TIMESTAMP || sql_type == SqlDataType::EXT_TIMESTAMP {
        "TIMESTAMP"
    } else if sql_type == SqlDataType::EXT_GUID {
        "GUID"
    } else {
        ""
    }
}

// Display size = number of characters needed to print the value including sign.
const DISPLAY_SIZE_INTEGER: isize = 11; // "-2147483648"
const DISPLAY_SIZE_SMALLINT: isize = 6; // "-32768"
const DISPLAY_SIZE_BIGINT: isize = 20; // "-9223372036854775808"
const DISPLAY_SIZE_TINYINT: isize = 4; // "-128"
const DISPLAY_SIZE_REAL: isize = 14; // "-1.234567E+38" (sign, 7 digits, '.', 'E', sign, 2 digits)
const DISPLAY_SIZE_FLOAT_DOUBLE: isize = 24; // scientific notation e.g. "-1.23456789012345E+308"
const DISPLAY_SIZE_BIT: isize = 1; // "0" or "1"

// Octet length = storage size in bytes.
const OCTET_LENGTH_INTEGER: isize = 4;
const OCTET_LENGTH_SMALLINT: isize = 2;
const OCTET_LENGTH_BIGINT: isize = 8;
const OCTET_LENGTH_TINYINT: isize = 1;
const OCTET_LENGTH_DOUBLE: isize = 8;
const OCTET_LENGTH_BIT: isize = 1;
// Transfer octet length for DATE/TIME/TIMESTAMP is the size of the C struct
// the value is delivered into (SQL_DATE_STRUCT / SQL_TIME_STRUCT /
// SQL_TIMESTAMP_STRUCT), not a string-length quantity, unlike COLUMN_SIZE
// and DISPLAY_SIZE, which *are* string-length quantities for these types.
// Spec ("Transfer Octet Length" appendix): "SQL_TYPE_DATE, SQL_TYPE_TIME: 6
// (the size of the SQL_DATE_STRUCT or SQL_TIME_STRUCT structure) ...
// SQL_TYPE_TIMESTAMP: 16 (the size of the SQL_TIMESTAMP_STRUCT structure)."
// <https://learn.microsoft.com/en-us/sql/odbc/reference/appendixes/transfer-octet-length>
const OCTET_LENGTH_DATE_OR_TIME: isize = 6;
const OCTET_LENGTH_TIMESTAMP: isize = 16;

/// Conservative upper bound on bytes needed per character, for octet-length
/// calculations. Used for every ODBC character type, narrow or wide: 4 bytes
/// covers both a UTF-8-encoded code point and a UTF-16 surrogate pair (2
/// code units × 2 bytes). Matches the `BYTES_PER_CHAR` convention used for
/// the analogous `CHAR_OCTET_LENGTH` catalog column.
const UTF8_MAX_BYTES_PER_CHAR: isize = 4;

/// Whether a `SqlDataType` is one of the ODBC character types: narrow or
/// wide, fixed or variable length. Centralised so every column-attribute
/// predicate that needs to distinguish character columns from all others
/// (`SQL_DESC_CASE_SENSITIVE`, `SQL_DESC_DISPLAY_SIZE`,
/// `SQL_DESC_OCTET_LENGTH`) uses the same complete set, rather than each
/// predicate separately (and incompletely) enumerating it.
fn is_character_type(sql_type: SqlDataType) -> bool {
    sql_type == SqlDataType::CHAR
        || sql_type == SqlDataType::VARCHAR
        || sql_type == SqlDataType::EXT_LONG_VARCHAR
        || sql_type == SqlDataType::EXT_W_CHAR
        || sql_type == SqlDataType::EXT_W_VARCHAR
        || sql_type == SqlDataType::EXT_W_LONG_VARCHAR
}

/// Whether a `SqlDataType` is one of the ODBC binary types: fixed, variable,
/// or long. Centralised for the same reason as `is_character_type` above.
fn is_binary_type(sql_type: SqlDataType) -> bool {
    sql_type == SqlDataType::EXT_BINARY
        || sql_type == SqlDataType::EXT_VAR_BINARY
        || sql_type == SqlDataType::EXT_LONG_VAR_BINARY
}

/// Whether a `SqlDataType` is `SQL_DECIMAL`/`SQL_NUMERIC`.
fn is_decimal_type(sql_type: SqlDataType) -> bool {
    sql_type == SqlDataType::DECIMAL || sql_type == SqlDataType::NUMERIC
}

/// Return the `SQL_DESC_PRECISION` value for a column.
///
/// This is *not* simply `desc.precision` (the column-size quantity `SQL_DESC_LENGTH`
/// reports). Per the ODBC "Column Size"/"Decimal Digits" appendices' own
/// descriptor-field mapping tables
/// (<https://learn.microsoft.com/en-us/sql/odbc/reference/appendixes/column-size>,
/// <https://learn.microsoft.com/en-us/sql/odbc/reference/appendixes/decimal-digits>),
/// which descriptor field holds "precision" versus "decimal digits" depends
/// on the SQL type:
///
/// - Character/binary types and `SQL_TYPE_TIME`/`SQL_TYPE_TIMESTAMP`/interval-with-seconds
///   types: "column size" (the character length, e.g. `20 + s` for
///   `SQL_TYPE_TIMESTAMP`) comes from `SQL_DESC_LENGTH`, while "decimal digits"
///   (the fractional-seconds scale `s` for datetime/interval-with-seconds
///   types) comes from `SQL_DESC_PRECISION`: the two field names are
///   reversed from what their ODBC 3.x names suggest.
/// - Numeric types (`DECIMAL`/`NUMERIC`, and every other numeric type):
///   `SQL_DESC_PRECISION` holds the defined/applicable precision directly,
///   matching `desc.precision`.
///
/// The `SQLColAttribute` spec text for `SQL_DESC_PRECISION` itself confirms
/// this for datetime types explicitly: "For data types SQL_TYPE_TIME,
/// SQL_TYPE_TIMESTAMP, and all the interval data types that represent a time
/// interval, its value is the applicable precision of the fractional seconds
/// component", i.e. the scale, not the column size.
fn precision_for(desc: &ColumnDescriptor) -> isize {
    if desc.sql_type == SqlDataType::TIME || desc.sql_type == SqlDataType::TIMESTAMP {
        desc.scale as isize
    } else {
        resolve_precision_isize(desc.precision)
    }
}

/// Return the display size for a column.
fn display_size_for(desc: &ColumnDescriptor) -> isize {
    if (is_character_type(desc.sql_type) || is_binary_type(desc.sql_type))
        && desc.precision == crate::types::PRECISION_UNDETERMINABLE
    {
        // Spec ("Display Size" appendix, footnote [a]; "Column Size"
        // appendix, footnote [b]): "If the driver cannot determine the
        // column or parameter length for a variable type, it returns
        // SQL_NO_TOTAL." Must be checked before the character/binary
        // formulas below, which would otherwise multiply the sentinel
        // (binary types) or return it unscaled but still as the wrong,
        // reinterpreted-as-`u32` value (character types).
        return resolve_precision_isize(desc.precision);
    }
    if is_character_type(desc.sql_type) {
        desc.precision as isize
    } else if is_binary_type(desc.sql_type) {
        // Spec ("Display Size" appendix): "All binary types: The defined or
        // maximum (for variable types) length of the column times 2. (Each
        // binary byte is represented by a 2-digit hexadecimal number.)",
        // matching `column_value_to_binary`'s hex-string rendering for
        // SQL_C_CHAR/WCHAR targets.
        desc.precision as isize * 2
    } else if is_decimal_type(desc.sql_type) {
        // Spec: "SQL_DECIMAL, SQL_NUMERIC: The precision of the column plus
        // 2 (a sign, precision digits, and a decimal point)."
        desc.precision as isize + 2
    } else if desc.sql_type == SqlDataType::INTEGER {
        DISPLAY_SIZE_INTEGER
    } else if desc.sql_type == SqlDataType::SMALLINT {
        DISPLAY_SIZE_SMALLINT
    } else if desc.sql_type == SqlDataType::EXT_BIG_INT {
        DISPLAY_SIZE_BIGINT
    } else if desc.sql_type == SqlDataType::EXT_TINY_INT {
        DISPLAY_SIZE_TINYINT
    } else if desc.sql_type == SqlDataType::REAL {
        // Spec ("Display Size" appendix): "SQL_REAL: 14 (a sign, 7 digits, a
        // decimal point, the letter E, a sign, and 2 digits)."
        DISPLAY_SIZE_REAL
    } else if desc.sql_type == SqlDataType::FLOAT || desc.sql_type == SqlDataType::DOUBLE {
        // Spec ("Display Size" appendix): "SQL_FLOAT, SQL_DOUBLE: 24 (a
        // sign, 15 digits, a decimal point, the letter E, a sign, and 3
        // digits)."
        DISPLAY_SIZE_FLOAT_DOUBLE
    } else if desc.sql_type == SqlDataType::EXT_BIT {
        DISPLAY_SIZE_BIT
    } else {
        desc.precision as isize
    }
}

/// Return the octet length (transfer size in bytes) for a column.
fn octet_length_for(desc: &ColumnDescriptor) -> isize {
    if is_character_type(desc.sql_type) {
        desc.precision as isize * UTF8_MAX_BYTES_PER_CHAR
    } else if is_decimal_type(desc.sql_type) {
        // Spec ("Transfer Octet Length" appendix): "SQL_DECIMAL, SQL_NUMERIC:
        // The number of bytes required to hold the character representation
        // of this data if the character set is ANSI, and twice this number
        // if the character set is UNICODE. This is the maximum number of
        // digits plus two, because the data is returned as a character
        // string and characters are needed for the digits, a sign, and a
        // decimal point." This driver exports only the Unicode (W-suffix)
        // entry points (see AGENTS.md "W-only exports"), so the UNICODE case
        // applies: 2 * (precision + 2).
        (desc.precision as isize + 2) * 2
    } else if desc.sql_type == SqlDataType::INTEGER {
        OCTET_LENGTH_INTEGER
    } else if desc.sql_type == SqlDataType::SMALLINT {
        OCTET_LENGTH_SMALLINT
    } else if desc.sql_type == SqlDataType::EXT_BIG_INT {
        OCTET_LENGTH_BIGINT
    } else if desc.sql_type == SqlDataType::EXT_TINY_INT {
        OCTET_LENGTH_TINYINT
    } else if desc.sql_type == SqlDataType::DOUBLE {
        OCTET_LENGTH_DOUBLE
    } else if desc.sql_type == SqlDataType::EXT_BIT {
        OCTET_LENGTH_BIT
    } else if desc.sql_type == SqlDataType::DATE || desc.sql_type == SqlDataType::TIME {
        OCTET_LENGTH_DATE_OR_TIME
    } else if desc.sql_type == SqlDataType::TIMESTAMP {
        OCTET_LENGTH_TIMESTAMP
    } else {
        // Binary types fall here deliberately: OCTET_LENGTH for a binary
        // column is its raw byte length (`desc.precision`), not doubled,
        // unlike DISPLAY_SIZE above, which *is* doubled for the hex-text
        // rendering. Spec: "All binary types: The number of bytes required
        // to hold the defined (for fixed types) or maximum (for variable
        // types) number of characters" (i.e. bytes, no hex expansion).
        desc.precision as isize
    }
}

/// Returns true if the SQL type is unsigned (or non-numeric).
fn is_unsigned(sql_type: SqlDataType) -> bool {
    !is_numeric_type(sql_type)
}

/// Whether a type is numeric.
///
/// Drives `SQL_DESC_UNSIGNED`, which per spec is SQL_TRUE "if the column is
/// unsigned (or not numeric)", so every non-numeric type correctly reports
/// unsigned. All ODBC numeric types are signed, hence `is_unsigned` is the
/// negation of this.
fn is_numeric_type(sql_type: SqlDataType) -> bool {
    sql_type == SqlDataType::INTEGER
        || sql_type == SqlDataType::SMALLINT
        || sql_type == SqlDataType::DOUBLE
        || sql_type == SqlDataType::EXT_BIG_INT
        || sql_type == SqlDataType::EXT_TINY_INT
        || sql_type == SqlDataType::DECIMAL
        || sql_type == SqlDataType::NUMERIC
        || sql_type == SqlDataType::REAL
        || sql_type == SqlDataType::FLOAT
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ColumnDescriptor, Nullable, SQL_NAMED, SQL_SEARCHABLE, SQL_UNNAMED};

    fn varchar_desc() -> ColumnDescriptor {
        ColumnDescriptor {
            name: "test_col".into(),
            type_name: String::new(),
            sql_type: SqlDataType::VARCHAR,
            precision: 255,
            scale: 0,
            nullable: Nullable::SqlNullable,
            ..Default::default()
        }
    }

    fn int_desc() -> ColumnDescriptor {
        ColumnDescriptor {
            name: "id".into(),
            type_name: String::new(),
            sql_type: SqlDataType::INTEGER,
            precision: 10,
            scale: 0,
            nullable: Nullable::SqlNoNulls,
            ..Default::default()
        }
    }

    #[test]
    fn col_attr_name_returns_column_name() {
        let desc = varchar_desc();
        match get_column_attribute(&desc, 5, Desc::Name).unwrap() {
            ColAttrValue::String(s) => assert_eq!(s, "test_col"),
            ColAttrValue::Numeric(_) => panic!("expected string"),
        }
    }

    #[test]
    fn label_returns_column_name() {
        let desc = varchar_desc();
        match get_column_attribute(&desc, 5, Desc::Label).unwrap() {
            ColAttrValue::String(s) => assert_eq!(s, "test_col"),
            ColAttrValue::Numeric(_) => panic!("expected string"),
        }
    }

    #[test]
    fn col_attr_type_returns_sql_type_code() {
        let desc = int_desc();
        match get_column_attribute(&desc, 5, Desc::Type).unwrap() {
            ColAttrValue::Numeric(n) => assert_eq!(n, SqlDataType::INTEGER.0 as isize),
            ColAttrValue::String(_) => panic!("expected numeric"),
        }
    }

    #[test]
    fn col_attr_count_returns_column_count() {
        let desc = int_desc();
        match get_column_attribute(&desc, 7, Desc::Count).unwrap() {
            ColAttrValue::Numeric(n) => assert_eq!(n, 7),
            ColAttrValue::String(_) => panic!("expected numeric"),
        }
    }

    #[test]
    fn desc_type_reports_the_verbose_type_for_datetime_columns() {
        // The spec splits these: SQL_DESC_CONCISE_TYPE keeps SQL_TYPE_TIMESTAMP,
        // while SQL_DESC_TYPE reports SQL_DATETIME and the concise code moves to
        // SQL_DESC_DATETIME_INTERVAL_CODE.
        let desc = ColumnDescriptor::new("ts", SqlDataType::TIMESTAMP);
        assert_eq!(desc.sql_type.0, 93, "SQL_TYPE_TIMESTAMP");

        assert_eq!(
            expect_numeric(get_column_attribute(&desc, 5, Desc::Type).unwrap()),
            isize::from(crate::types::SQL_DATETIME)
        );
        assert_eq!(
            expect_numeric(get_column_attribute(&desc, 5, Desc::ConciseType).unwrap()),
            93
        );
        assert_eq!(
            expect_numeric(get_column_attribute(&desc, 5, Desc::DatetimeIntervalCode).unwrap()),
            93
        );
    }

    #[test]
    fn desc_type_and_concise_type_agree_for_an_ordinary_type() {
        let desc = ColumnDescriptor::new("n", SqlDataType::INTEGER);
        let verbose = expect_numeric(get_column_attribute(&desc, 5, Desc::Type).unwrap());
        let concise = expect_numeric(get_column_attribute(&desc, 5, Desc::ConciseType).unwrap());
        assert_eq!(verbose, concise, "only datetime and interval types differ");
        assert_eq!(
            expect_numeric(get_column_attribute(&desc, 5, Desc::DatetimeIntervalCode).unwrap()),
            0,
            "a non-datetime column has no interval code"
        );
    }

    #[test]
    fn base_table_name_is_answered_rather_than_reported_unimplemented() {
        // ODBC 3.x requires a value for every descriptor field; HYC00 here made
        // an application asking for column provenance treat the whole call as
        // failed.
        let desc =
            ColumnDescriptor::new("c", SqlDataType::INTEGER).with_origin("cat", "sch", "tbl");
        assert_eq!(
            expect_string(get_column_attribute(&desc, 5, Desc::BaseTableName).unwrap()),
            "tbl"
        );
        let bare = ColumnDescriptor::new("c", SqlDataType::INTEGER);
        assert_eq!(
            expect_string(get_column_attribute(&bare, 5, Desc::BaseTableName).unwrap()),
            "",
            "empty when the backend does not track it, not an error"
        );
    }

    #[test]
    fn col_attr_nullable_reports_the_unknown_case_the_bool_could_not() {
        // The reason `nullable` is an enum: SQL_NULLABLE_UNKNOWN is what the
        // spec requires for a computed or outer-joined column, and a `bool`
        // forced it to be reported as SQL_NO_NULLS — telling an application it
        // could skip a NULL check it needs.
        let desc = ColumnDescriptor::new("computed", SqlDataType::INTEGER)
            .with_nullable(Nullable::SqlNullableUnknown);
        match get_column_attribute(&desc, 5, Desc::Nullable).unwrap() {
            ColAttrValue::Numeric(n) => assert_eq!(n, 2),
            other => panic!("expected numeric, got {other:?}"),
        }
    }

    #[test]
    fn col_attr_reads_searchable_from_the_descriptor() {
        let desc = ColumnDescriptor::new("blob", SqlDataType::EXT_LONG_VAR_BINARY)
            .with_searchable(crate::types::SQL_PRED_NONE);
        match get_column_attribute(&desc, 5, Desc::Searchable).unwrap() {
            ColAttrValue::Numeric(n) => assert_eq!(
                n,
                isize::from(crate::types::SQL_PRED_NONE),
                "a column the backend declared unsearchable was reported as searchable"
            ),
            other => panic!("expected numeric, got {other:?}"),
        }
    }

    #[test]
    fn col_attr_reads_literal_affixes_and_origin_from_the_descriptor() {
        let desc = ColumnDescriptor::new("c", SqlDataType::VARCHAR)
            .with_literal_affixes("'", "'")
            .with_origin("cat", "sch", "tbl");
        for (field, expected) in [
            (Desc::LiteralPrefix, "'"),
            (Desc::LiteralSuffix, "'"),
            (Desc::CatalogName, "cat"),
            (Desc::SchemaName, "sch"),
            (Desc::TableName, "tbl"),
        ] {
            let got = expect_string(get_column_attribute(&desc, 5, field).unwrap());
            assert_eq!(got, expected, "{field:?} was not read from the descriptor");
        }
    }

    #[test]
    fn col_attr_origin_and_affixes_stay_empty_when_the_backend_declares_none() {
        // The previous hard-coded behaviour, preserved as the default.
        let desc = ColumnDescriptor::new("c", SqlDataType::VARCHAR);
        for field in [
            Desc::LiteralPrefix,
            Desc::LiteralSuffix,
            Desc::CatalogName,
            Desc::SchemaName,
            Desc::TableName,
        ] {
            assert_eq!(
                expect_string(get_column_attribute(&desc, 5, field).unwrap()),
                ""
            );
        }
    }

    #[test]
    fn col_attr_nullable_true() {
        let desc = varchar_desc();
        match get_column_attribute(&desc, 5, Desc::Nullable).unwrap() {
            ColAttrValue::Numeric(n) => assert_eq!(n, 1),
            ColAttrValue::String(_) => panic!("expected numeric"),
        }
    }

    #[test]
    fn col_attr_nullable_false() {
        let desc = int_desc();
        match get_column_attribute(&desc, 5, Desc::Nullable).unwrap() {
            ColAttrValue::Numeric(n) => assert_eq!(n, 0),
            ColAttrValue::String(_) => panic!("expected numeric"),
        }
    }

    /// 9 is `SQL_DATE` as a concise type, which is what a backend reports for a
    /// column, even though `odbc_sys` names it after the verbose
    /// `SQL_DATETIME`. `verbose_type` deliberately treats the same number
    /// differently, and its own comment says why.
    #[test]
    fn the_2x_date_spelling_names_a_date_not_a_timestamp() {
        assert_eq!(type_name_for(SqlDataType::DATETIME), "DATE");
        assert_eq!(type_name_for(SqlDataType::DATE), "DATE");
        assert_eq!(type_name_for(SqlDataType::EXT_TIMESTAMP), "TIMESTAMP");
        assert_eq!(type_name_for(SqlDataType::EXT_TIME_OR_INTERVAL), "TIME");
    }

    #[test]
    fn type_name_returns_string() {
        let desc = int_desc();
        match get_column_attribute(&desc, 5, Desc::TypeName).unwrap() {
            ColAttrValue::String(s) => assert_eq!(s, "INTEGER"),
            ColAttrValue::Numeric(_) => panic!("expected string"),
        }
    }

    #[test]
    fn col_attr_unsigned_for_integer() {
        let desc = int_desc();
        match get_column_attribute(&desc, 5, Desc::Unsigned).unwrap() {
            ColAttrValue::Numeric(n) => assert_eq!(n, 0),
            ColAttrValue::String(_) => panic!("expected numeric"),
        }
    }

    #[test]
    fn col_attr_unsigned_for_varchar() {
        let desc = varchar_desc();
        match get_column_attribute(&desc, 5, Desc::Unsigned).unwrap() {
            ColAttrValue::Numeric(n) => assert_eq!(n, 1),
            ColAttrValue::String(_) => panic!("expected numeric"),
        }
    }

    #[test]
    fn col_attr_unimplemented_returns_error() {
        let desc = varchar_desc();
        // RowsProcessedPtr is not a column attribute
        assert!(get_column_attribute(&desc, 5, Desc::RowsProcessedPtr).is_err());
    }

    // -----------------------------------------------------------------------
    // Additional descriptor helpers
    // -----------------------------------------------------------------------

    fn smallint_desc() -> ColumnDescriptor {
        ColumnDescriptor {
            name: "small".into(),
            type_name: String::new(),
            sql_type: SqlDataType::SMALLINT,
            precision: 5,
            scale: 0,
            nullable: Nullable::SqlNullable,
            ..Default::default()
        }
    }

    fn bigint_desc() -> ColumnDescriptor {
        ColumnDescriptor {
            name: "big".into(),
            type_name: String::new(),
            sql_type: SqlDataType::EXT_BIG_INT,
            precision: 19,
            scale: 0,
            nullable: Nullable::SqlNoNulls,
            ..Default::default()
        }
    }

    fn tinyint_desc() -> ColumnDescriptor {
        ColumnDescriptor {
            name: "tiny".into(),
            type_name: String::new(),
            sql_type: SqlDataType::EXT_TINY_INT,
            precision: 3,
            scale: 0,
            nullable: Nullable::SqlNullable,
            ..Default::default()
        }
    }

    fn double_desc() -> ColumnDescriptor {
        ColumnDescriptor {
            name: "dbl".into(),
            type_name: String::new(),
            sql_type: SqlDataType::DOUBLE,
            precision: 15,
            scale: 0,
            nullable: Nullable::SqlNullable,
            ..Default::default()
        }
    }

    fn real_desc() -> ColumnDescriptor {
        ColumnDescriptor {
            name: "flt".into(),
            type_name: String::new(),
            sql_type: SqlDataType::REAL,
            precision: 7,
            scale: 0,
            nullable: Nullable::SqlNullable,
            ..Default::default()
        }
    }

    fn float_desc() -> ColumnDescriptor {
        ColumnDescriptor {
            name: "flt".into(),
            type_name: String::new(),
            sql_type: SqlDataType::FLOAT,
            precision: 15,
            scale: 0,
            nullable: Nullable::SqlNullable,
            ..Default::default()
        }
    }

    fn bit_desc() -> ColumnDescriptor {
        ColumnDescriptor {
            name: "flag".into(),
            type_name: String::new(),
            sql_type: SqlDataType::EXT_BIT,
            precision: 1,
            scale: 0,
            nullable: Nullable::SqlNoNulls,
            ..Default::default()
        }
    }

    fn varbinary_desc() -> ColumnDescriptor {
        ColumnDescriptor {
            name: "bin".into(),
            type_name: String::new(),
            sql_type: SqlDataType::EXT_VAR_BINARY,
            precision: 256,
            scale: 0,
            nullable: Nullable::SqlNullable,
            ..Default::default()
        }
    }

    fn unnamed_desc() -> ColumnDescriptor {
        ColumnDescriptor {
            name: String::new(),
            type_name: String::new(),
            sql_type: SqlDataType::INTEGER,
            precision: 10,
            scale: 0,
            nullable: Nullable::SqlNoNulls,
            ..Default::default()
        }
    }

    /// Extract the numeric value from a ColAttrValue, panicking if it's a string.
    fn expect_numeric(val: ColAttrValue) -> isize {
        match val {
            ColAttrValue::Numeric(n) => n,
            ColAttrValue::String(_) => panic!("expected numeric"),
        }
    }

    /// Extract the string value from a ColAttrValue, panicking if it's numeric.
    fn expect_string(val: ColAttrValue) -> String {
        match val {
            ColAttrValue::String(s) => s,
            ColAttrValue::Numeric(_) => panic!("expected string"),
        }
    }

    // -----------------------------------------------------------------------
    // Shared match arms not directly exercised
    // -----------------------------------------------------------------------

    #[test]
    fn base_column_name_returns_column_name() {
        let desc = varchar_desc();
        let s = expect_string(get_column_attribute(&desc, 5, Desc::BaseColumnName).unwrap());
        assert_eq!(s, "test_col");
    }

    #[test]
    fn concise_type_returns_sql_type() {
        let desc = int_desc();
        let n = expect_numeric(get_column_attribute(&desc, 5, Desc::ConciseType).unwrap());
        assert_eq!(n, SqlDataType::INTEGER.0 as isize);
    }

    // -----------------------------------------------------------------------
    // Precision, Length, Scale
    // -----------------------------------------------------------------------

    #[test]
    fn precision_returns_descriptor_precision() {
        let desc = varchar_desc();
        let n = expect_numeric(get_column_attribute(&desc, 5, Desc::Precision).unwrap());
        assert_eq!(n, 255);
    }

    #[test]
    fn length_returns_descriptor_precision() {
        let desc = int_desc();
        let n = expect_numeric(get_column_attribute(&desc, 5, Desc::Length).unwrap());
        assert_eq!(n, 10);
    }

    #[test]
    fn scale_returns_descriptor_scale() {
        let mut desc = double_desc();
        desc.scale = 3;
        let n = expect_numeric(get_column_attribute(&desc, 5, Desc::Scale).unwrap());
        assert_eq!(n, 3);
    }

    /// A `TIMESTAMP` column declared with 6 fractional-seconds digits:
    /// `desc.precision` holds the column-size quantity (`20 + s` = 26, per
    /// the "Column Size" appendix), `desc.scale` holds the fractional-seconds
    /// count itself (6).
    fn timestamp_desc(scale: i16, column_size: u32) -> ColumnDescriptor {
        ColumnDescriptor {
            name: "ts".into(),
            type_name: String::new(),
            sql_type: SqlDataType::TIMESTAMP,
            precision: column_size,
            scale,
            nullable: Nullable::SqlNullable,
            ..Default::default()
        }
    }

    #[test]
    fn precision_for_timestamp_is_the_fractional_seconds_scale_not_the_column_size() {
        // SQL_DESC_PRECISION for SQL_TYPE_TIMESTAMP must be the
        // scale `s` (per the SQLColAttribute spec: "For data types
        // SQL_TYPE_TIME, SQL_TYPE_TIMESTAMP, ... its value is the applicable
        // precision of the fractional seconds component"), not
        // `desc.precision` (the column-size quantity, 20 + s). A `timestamp(6)`
        // column has column size 26 but SQL_DESC_PRECISION 6; conflating the
        // two would make SQL_DESC_PRECISION report 26 instead of 6.
        let desc = timestamp_desc(6, 26);
        let n = expect_numeric(get_column_attribute(&desc, 5, Desc::Precision).unwrap());
        assert_eq!(n, 6);
    }

    #[test]
    fn length_for_timestamp_is_the_column_size_not_the_scale() {
        // The companion property: SQL_DESC_LENGTH
        // (and COLUMN_SIZE) for the same column must stay the full 20 + s
        // character length (26), not collapse to the scale (6); see a
        // driver's `type_conversion.rs` `type_name_precision` doc comments for
        // how a backend derives these.
        let desc = timestamp_desc(6, 26);
        let n = expect_numeric(get_column_attribute(&desc, 5, Desc::Length).unwrap());
        assert_eq!(n, 26);
    }

    #[test]
    fn scale_for_timestamp_still_reports_the_scale_field_directly() {
        // SQL_DESC_SCALE is a separate field from SQL_DESC_PRECISION; per the
        // "Decimal Digits" appendix it is "n/a" for datetime types (the
        // fractional-seconds count is reported via SQL_DESC_PRECISION
        // instead, per the two tests above), but this driver still exposes
        // `desc.scale` verbatim through SQL_DESC_SCALE rather than forcing it
        // to 0 — pinning that `precision_for`'s special case does not affect
        // the `Desc::Scale` arm.
        let desc = timestamp_desc(6, 26);
        let n = expect_numeric(get_column_attribute(&desc, 5, Desc::Scale).unwrap());
        assert_eq!(n, 6);
    }

    // -----------------------------------------------------------------------
    // DisplaySize — type-specific logic
    // -----------------------------------------------------------------------

    #[test]
    fn display_size_varchar_uses_precision() {
        let desc = varchar_desc();
        let n = expect_numeric(get_column_attribute(&desc, 5, Desc::DisplaySize).unwrap());
        assert_eq!(n, 255); // uses precision
    }

    #[test]
    fn display_size_integer() {
        let desc = int_desc();
        let n = expect_numeric(get_column_attribute(&desc, 5, Desc::DisplaySize).unwrap());
        assert_eq!(n, 11);
    }

    #[test]
    fn display_size_smallint() {
        let desc = smallint_desc();
        let n = expect_numeric(get_column_attribute(&desc, 5, Desc::DisplaySize).unwrap());
        assert_eq!(n, 6);
    }

    #[test]
    fn display_size_bigint() {
        let desc = bigint_desc();
        let n = expect_numeric(get_column_attribute(&desc, 5, Desc::DisplaySize).unwrap());
        assert_eq!(n, 20);
    }

    #[test]
    fn display_size_tinyint() {
        let desc = tinyint_desc();
        let n = expect_numeric(get_column_attribute(&desc, 5, Desc::DisplaySize).unwrap());
        assert_eq!(n, 4);
    }

    #[test]
    fn display_size_double() {
        let desc = double_desc();
        let n = expect_numeric(get_column_attribute(&desc, 5, Desc::DisplaySize).unwrap());
        assert_eq!(n, 24);
    }

    #[test]
    fn display_size_real() {
        // REAL reports a fixed DISPLAY_SIZE (14: sign, 7 digits, '.', 'E',
        // sign, 2 digits) from the appendix, not the generic "uses precision"
        // fallback (7), which would under-report it.
        let desc = real_desc();
        let n = expect_numeric(get_column_attribute(&desc, 5, Desc::DisplaySize).unwrap());
        assert_eq!(n, 14);
    }

    #[test]
    fn display_size_float() {
        // FLOAT reports the appendix's dedicated DISPLAY_SIZE value (24,
        // shared with SQL_DOUBLE), not the generic "uses precision" fallback
        // (15).
        let desc = float_desc();
        let n = expect_numeric(get_column_attribute(&desc, 5, Desc::DisplaySize).unwrap());
        assert_eq!(n, 24);
    }

    #[test]
    fn display_size_bit() {
        let desc = bit_desc();
        let n = expect_numeric(get_column_attribute(&desc, 5, Desc::DisplaySize).unwrap());
        assert_eq!(n, 1);
    }

    #[test]
    fn display_size_binary_is_precision_times_two() {
        // Spec ("Display Size" appendix): binary types render as a 2-digit
        // hex number per byte, so DISPLAY_SIZE is precision * 2, not
        // precision. The generic "uses precision" fallback would
        // under-report by half.
        let desc = varbinary_desc();
        let n = expect_numeric(get_column_attribute(&desc, 5, Desc::DisplaySize).unwrap());
        assert_eq!(n, 512);
    }

    #[test]
    fn display_size_decimal_is_precision_plus_two() {
        // Spec ("Display Size" appendix): "SQL_DECIMAL, SQL_NUMERIC: The
        // precision of the column plus 2 (a sign, precision digits, and a
        // decimal point)." The generic "uses precision" fallback would
        // miss the +2.
        for sql_type in [SqlDataType::DECIMAL, SqlDataType::NUMERIC] {
            let desc = ColumnDescriptor {
                name: "amount".into(),
                type_name: String::new(),
                sql_type,
                precision: 10,
                scale: 2,
                nullable: Nullable::SqlNullable,
                ..Default::default()
            };
            let n = expect_numeric(get_column_attribute(&desc, 1, Desc::DisplaySize).unwrap());
            assert_eq!(n, 12, "{sql_type:?} display size must be precision + 2");
        }
    }

    #[test]
    fn display_size_uses_precision_for_every_character_type() {
        // Wide-char types (EXT_W_CHAR / EXT_W_VARCHAR / EXT_W_LONG_VARCHAR)
        // must take the character-type branch explicitly, by the same rule as
        // VARCHAR, not the generic "uses precision" branch.
        for ty in [
            SqlDataType::CHAR,
            SqlDataType::VARCHAR,
            SqlDataType::EXT_LONG_VARCHAR,
            SqlDataType::EXT_W_CHAR,
            SqlDataType::EXT_W_VARCHAR,
            SqlDataType::EXT_W_LONG_VARCHAR,
        ] {
            let desc = ColumnDescriptor {
                name: "col".into(),
                type_name: String::new(),
                sql_type: ty,
                precision: 50,
                scale: 0,
                nullable: Nullable::SqlNullable,
                ..Default::default()
            };
            let n = expect_numeric(get_column_attribute(&desc, 1, Desc::DisplaySize).unwrap());
            assert_eq!(n, 50, "{ty:?} display size must equal precision");
        }
    }

    // -----------------------------------------------------------------------
    // OctetLength — type-specific logic
    // -----------------------------------------------------------------------

    #[test]
    fn octet_length_varchar_is_precision_times_4() {
        let desc = varchar_desc();
        let n = expect_numeric(get_column_attribute(&desc, 5, Desc::OctetLength).unwrap());
        assert_eq!(n, 255 * 4); // UTF-8 max bytes per char
    }

    #[test]
    fn octet_length_integer() {
        let desc = int_desc();
        let n = expect_numeric(get_column_attribute(&desc, 5, Desc::OctetLength).unwrap());
        assert_eq!(n, 4);
    }

    #[test]
    fn octet_length_smallint() {
        let desc = smallint_desc();
        let n = expect_numeric(get_column_attribute(&desc, 5, Desc::OctetLength).unwrap());
        assert_eq!(n, 2);
    }

    #[test]
    fn octet_length_bigint() {
        let desc = bigint_desc();
        let n = expect_numeric(get_column_attribute(&desc, 5, Desc::OctetLength).unwrap());
        assert_eq!(n, 8);
    }

    #[test]
    fn octet_length_tinyint() {
        let desc = tinyint_desc();
        let n = expect_numeric(get_column_attribute(&desc, 5, Desc::OctetLength).unwrap());
        assert_eq!(n, 1);
    }

    #[test]
    fn octet_length_double() {
        let desc = double_desc();
        let n = expect_numeric(get_column_attribute(&desc, 5, Desc::OctetLength).unwrap());
        assert_eq!(n, 8);
    }

    #[test]
    fn octet_length_bit() {
        let desc = bit_desc();
        let n = expect_numeric(get_column_attribute(&desc, 5, Desc::OctetLength).unwrap());
        assert_eq!(n, 1);
    }

    #[test]
    fn octet_length_binary_is_raw_precision_not_doubled() {
        // Unlike DISPLAY_SIZE (hex-text length, doubled), OCTET_LENGTH for a
        // binary column is its raw byte length. Spec: "the number of bytes
        // required to hold the defined ... number of characters."
        let desc = varbinary_desc();
        let n = expect_numeric(get_column_attribute(&desc, 5, Desc::OctetLength).unwrap());
        assert_eq!(n, 256);
    }

    #[test]
    fn octet_length_decimal_is_unicode_precision_plus_two_doubled() {
        // Spec ("Transfer Octet Length" appendix): "(precision + 2), and
        // twice this number if the character set is UNICODE"; this driver
        // is Unicode-only (W-suffix exports), so 2 * (precision + 2).
        for sql_type in [SqlDataType::DECIMAL, SqlDataType::NUMERIC] {
            let desc = ColumnDescriptor {
                name: "amount".into(),
                type_name: String::new(),
                sql_type,
                precision: 10,
                scale: 2,
                nullable: Nullable::SqlNullable,
                ..Default::default()
            };
            let n = expect_numeric(get_column_attribute(&desc, 1, Desc::OctetLength).unwrap());
            assert_eq!(
                n, 24,
                "{sql_type:?} octet length must be 2 * (precision + 2)"
            );
        }
    }

    #[test]
    fn octet_length_date_and_time_is_six_not_precision() {
        // Spec ("Transfer Octet Length" appendix): SQL_TYPE_DATE/SQL_TYPE_TIME
        // is 6 (the SQL_DATE_STRUCT / SQL_TIME_STRUCT struct size), not the
        // string-length precision (10 for DATE, 8+ for TIME). Falling through
        // to `desc.precision` would give the right value for
        // COLUMN_SIZE/DISPLAY_SIZE but the wrong one for OCTET_LENGTH.
        for (sql_type, precision) in [(SqlDataType::DATE, 10), (SqlDataType::TIME, 21)] {
            let desc = ColumnDescriptor {
                name: "col".into(),
                type_name: String::new(),
                sql_type,
                precision,
                scale: 0,
                nullable: Nullable::SqlNullable,
                ..Default::default()
            };
            let n = expect_numeric(get_column_attribute(&desc, 1, Desc::OctetLength).unwrap());
            assert_eq!(n, 6, "{sql_type:?} octet length must be 6, not precision");
        }
    }

    #[test]
    fn octet_length_timestamp_is_sixteen_not_precision() {
        // Spec: SQL_TYPE_TIMESTAMP transfer octet length is 16 (the
        // SQL_TIMESTAMP_STRUCT struct size), not the string-length precision.
        let desc = ColumnDescriptor {
            name: "ts".into(),
            type_name: String::new(),
            sql_type: SqlDataType::TIMESTAMP,
            precision: 32,
            scale: 0,
            nullable: Nullable::SqlNullable,
            ..Default::default()
        };
        let n = expect_numeric(get_column_attribute(&desc, 1, Desc::OctetLength).unwrap());
        assert_eq!(n, 16, "TIMESTAMP octet length must be 16, not precision");
    }

    #[test]
    fn octet_length_is_precision_times_bytes_per_char_for_every_character_type() {
        // Every character type, including the wide-char types, must report
        // SQL_DESC_OCTET_LENGTH as precision * 4, not its precision:
        // recognising only the narrow types as character types here would
        // under-report the wide ones. SQL_DESC_OCTET_LENGTH is specified in
        // bytes, so an application sizing a SQL_C_WCHAR buffer from it would
        // under-allocate by half and see silent truncation.
        for ty in [
            SqlDataType::CHAR,
            SqlDataType::VARCHAR,
            SqlDataType::EXT_LONG_VARCHAR,
            SqlDataType::EXT_W_CHAR,
            SqlDataType::EXT_W_VARCHAR,
            SqlDataType::EXT_W_LONG_VARCHAR,
        ] {
            let desc = ColumnDescriptor {
                name: "col".into(),
                type_name: String::new(),
                sql_type: ty,
                precision: 50,
                scale: 0,
                nullable: Nullable::SqlNullable,
                ..Default::default()
            };
            let n = expect_numeric(get_column_attribute(&desc, 1, Desc::OctetLength).unwrap());
            assert_eq!(n, 200, "{ty:?} octet length must be precision * 4");
        }
    }

    // -----------------------------------------------------------------------
    // Empty string returns
    // -----------------------------------------------------------------------

    #[test]
    fn table_name_returns_empty() {
        let desc = int_desc();
        let s = expect_string(get_column_attribute(&desc, 5, Desc::TableName).unwrap());
        assert_eq!(s, "");
    }

    #[test]
    fn schema_name_returns_empty() {
        let desc = int_desc();
        let s = expect_string(get_column_attribute(&desc, 5, Desc::SchemaName).unwrap());
        assert_eq!(s, "");
    }

    #[test]
    fn catalog_name_returns_empty() {
        let desc = int_desc();
        let s = expect_string(get_column_attribute(&desc, 5, Desc::CatalogName).unwrap());
        assert_eq!(s, "");
    }

    #[test]
    fn local_type_name_returns_empty() {
        let desc = int_desc();
        let s = expect_string(get_column_attribute(&desc, 5, Desc::LocalTypeName).unwrap());
        assert_eq!(s, "");
    }

    #[test]
    fn literal_prefix_returns_empty() {
        let desc = int_desc();
        let s = expect_string(get_column_attribute(&desc, 5, Desc::LiteralPrefix).unwrap());
        assert_eq!(s, "");
    }

    #[test]
    fn literal_suffix_returns_empty() {
        let desc = int_desc();
        let s = expect_string(get_column_attribute(&desc, 5, Desc::LiteralSuffix).unwrap());
        assert_eq!(s, "");
    }

    // -----------------------------------------------------------------------
    // Fixed constant returns
    // -----------------------------------------------------------------------

    #[test]
    fn searchable_returns_pred_searchable() {
        let desc = int_desc();
        let n = expect_numeric(get_column_attribute(&desc, 5, Desc::Searchable).unwrap());
        assert_eq!(n, SQL_SEARCHABLE as isize);
    }

    #[test]
    fn auto_unique_value_returns_false() {
        let desc = int_desc();
        let n = expect_numeric(get_column_attribute(&desc, 5, Desc::AutoUniqueValue).unwrap());
        assert_eq!(n, 0);
    }

    #[test]
    fn updatable_returns_readwrite_unknown() {
        let desc = int_desc();
        let n = expect_numeric(get_column_attribute(&desc, 5, Desc::Updatable).unwrap());
        assert_eq!(n, SQL_ATTR_READWRITE_UNKNOWN as isize);
    }

    #[test]
    fn fixed_prec_scale_returns_false() {
        let desc = int_desc();
        let n = expect_numeric(get_column_attribute(&desc, 5, Desc::FixedPrecScale).unwrap());
        assert_eq!(n, 0);
    }

    // -----------------------------------------------------------------------
    // CaseSensitive — VARCHAR vs non-VARCHAR
    // -----------------------------------------------------------------------

    #[test]
    fn case_sensitive_varchar_returns_true() {
        let desc = varchar_desc();
        let n = expect_numeric(get_column_attribute(&desc, 5, Desc::CaseSensitive).unwrap());
        assert_eq!(n, 1);
    }

    #[test]
    fn case_sensitive_integer_returns_false() {
        let desc = int_desc();
        let n = expect_numeric(get_column_attribute(&desc, 5, Desc::CaseSensitive).unwrap());
        assert_eq!(n, 0);
    }

    #[test]
    fn case_sensitive_is_true_for_every_character_type() {
        // This predicate, like display_size_for and octet_length_for, must
        // recognise every character type: recognising only the narrow ones
        // would report a column described with one of the wide-char types as
        // not case-sensitive.
        for ty in [
            SqlDataType::CHAR,
            SqlDataType::VARCHAR,
            SqlDataType::EXT_LONG_VARCHAR,
            SqlDataType::EXT_W_CHAR,
            SqlDataType::EXT_W_VARCHAR,
            SqlDataType::EXT_W_LONG_VARCHAR,
        ] {
            let desc = typed_col(ty);
            let n = expect_numeric(get_column_attribute(&desc, 1, Desc::CaseSensitive).unwrap());
            assert_eq!(n, 1, "{ty:?} must report case-sensitive");
        }
    }

    // -----------------------------------------------------------------------
    // NumPrecRadix — numeric vs non-numeric
    // -----------------------------------------------------------------------

    #[test]
    fn num_prec_radix_integer_returns_10() {
        let desc = int_desc();
        let n = expect_numeric(get_column_attribute(&desc, 5, Desc::NumPrecRadix).unwrap());
        assert_eq!(n, 10);
    }

    #[test]
    fn num_prec_radix_varchar_returns_0() {
        let desc = varchar_desc();
        let n = expect_numeric(get_column_attribute(&desc, 5, Desc::NumPrecRadix).unwrap());
        assert_eq!(n, 0);
    }

    // -----------------------------------------------------------------------
    // Unnamed — empty vs non-empty name
    // -----------------------------------------------------------------------

    #[test]
    fn unnamed_with_name_returns_named() {
        let desc = int_desc();
        let n = expect_numeric(get_column_attribute(&desc, 5, Desc::Unnamed).unwrap());
        assert_eq!(n, SQL_NAMED);
    }

    #[test]
    fn unnamed_without_name_returns_unnamed() {
        let desc = unnamed_desc();
        let n = expect_numeric(get_column_attribute(&desc, 5, Desc::Unnamed).unwrap());
        assert_eq!(n, SQL_UNNAMED);
    }

    // -----------------------------------------------------------------------
    // type_name_for — all types
    // -----------------------------------------------------------------------

    #[test]
    fn type_name_for_all_types() {
        let cases: &[(SqlDataType, &str)] = &[
            (SqlDataType::VARCHAR, "VARCHAR"),
            (SqlDataType::INTEGER, "INTEGER"),
            (SqlDataType::SMALLINT, "SMALLINT"),
            (SqlDataType::DOUBLE, "DOUBLE"),
            (SqlDataType::EXT_BIG_INT, "BIGINT"),
            (SqlDataType::EXT_TINY_INT, "TINYINT"),
            (SqlDataType::EXT_BIT, "BIT"),
            (SqlDataType::EXT_VAR_BINARY, "VARBINARY"),
            // Not a spec-defined type code, a deliberately unrecognized sentinel.
            // Spec, SQL_DESC_TYPE_NAME: "If the type is unknown, an empty string
            // is returned."
            (SqlDataType(9999), ""),
        ];
        for &(sql_type, expected) in cases {
            let desc = ColumnDescriptor {
                name: "x".into(),
                type_name: String::new(),
                sql_type,
                precision: 10,
                scale: 0,
                nullable: Nullable::SqlNoNulls,
                ..Default::default()
            };
            let s = expect_string(get_column_attribute(&desc, 1, Desc::TypeName).unwrap());
            assert_eq!(s, expected, "type_name_for({:?})", sql_type);
        }
    }

    #[test]
    fn type_name_varchar() {
        assert_eq!(type_name_for(SqlDataType::VARCHAR), "VARCHAR");
    }

    #[test]
    fn type_name_bigint() {
        assert_eq!(type_name_for(SqlDataType::EXT_BIG_INT), "BIGINT");
    }

    #[test]
    fn type_name_unknown_falls_back() {
        // Not a spec-defined type code, a deliberately unrecognized sentinel.
        // Spec, SQL_DESC_TYPE_NAME: "If the type is unknown, an empty string
        // is returned."
        assert_eq!(type_name_for(SqlDataType(9999)), "");
    }

    // -----------------------------------------------------------------------
    // Named-constant sanity checks
    // -----------------------------------------------------------------------

    fn typed_col(sql_type: SqlDataType) -> ColumnDescriptor {
        ColumnDescriptor {
            name: "col".into(),
            type_name: String::new(),
            sql_type,
            precision: 10,
            scale: 0,
            nullable: Nullable::SqlNoNulls,
            ..Default::default()
        }
    }

    fn varchar_col(precision: u32) -> ColumnDescriptor {
        ColumnDescriptor {
            name: "col".into(),
            type_name: String::new(),
            sql_type: SqlDataType::VARCHAR,
            precision,
            scale: 0,
            nullable: Nullable::SqlNullable,
            ..Default::default()
        }
    }

    #[test]
    fn constants_display_size_integer_equals_11() {
        assert_eq!(DISPLAY_SIZE_INTEGER, 11);
        let n = expect_numeric(
            get_column_attribute(&typed_col(SqlDataType::INTEGER), 1, Desc::DisplaySize).unwrap(),
        );
        assert_eq!(n, DISPLAY_SIZE_INTEGER);
    }

    #[test]
    fn constants_display_size_smallint_equals_6() {
        assert_eq!(DISPLAY_SIZE_SMALLINT, 6);
        let n = expect_numeric(
            get_column_attribute(&typed_col(SqlDataType::SMALLINT), 1, Desc::DisplaySize).unwrap(),
        );
        assert_eq!(n, DISPLAY_SIZE_SMALLINT);
    }

    #[test]
    fn constants_display_size_bigint_equals_20() {
        assert_eq!(DISPLAY_SIZE_BIGINT, 20);
        let n = expect_numeric(
            get_column_attribute(&typed_col(SqlDataType::EXT_BIG_INT), 1, Desc::DisplaySize)
                .unwrap(),
        );
        assert_eq!(n, DISPLAY_SIZE_BIGINT);
    }

    #[test]
    fn constants_display_size_tinyint_equals_4() {
        assert_eq!(DISPLAY_SIZE_TINYINT, 4);
        let n = expect_numeric(
            get_column_attribute(&typed_col(SqlDataType::EXT_TINY_INT), 1, Desc::DisplaySize)
                .unwrap(),
        );
        assert_eq!(n, DISPLAY_SIZE_TINYINT);
    }

    #[test]
    fn constants_display_size_double_equals_24() {
        assert_eq!(DISPLAY_SIZE_FLOAT_DOUBLE, 24);
        let n = expect_numeric(
            get_column_attribute(&typed_col(SqlDataType::DOUBLE), 1, Desc::DisplaySize).unwrap(),
        );
        assert_eq!(n, DISPLAY_SIZE_FLOAT_DOUBLE);
    }

    #[test]
    fn constants_display_size_real_equals_14() {
        assert_eq!(DISPLAY_SIZE_REAL, 14);
        let n = expect_numeric(
            get_column_attribute(&typed_col(SqlDataType::REAL), 1, Desc::DisplaySize).unwrap(),
        );
        assert_eq!(n, DISPLAY_SIZE_REAL);
    }

    #[test]
    fn constants_display_size_float_equals_24() {
        let n = expect_numeric(
            get_column_attribute(&typed_col(SqlDataType::FLOAT), 1, Desc::DisplaySize).unwrap(),
        );
        assert_eq!(n, DISPLAY_SIZE_FLOAT_DOUBLE);
    }

    #[test]
    fn constants_octet_length_varchar_uses_utf8_max_bytes() {
        let n =
            expect_numeric(get_column_attribute(&varchar_col(100), 1, Desc::OctetLength).unwrap());
        assert_eq!(n, 100 * UTF8_MAX_BYTES_PER_CHAR);
        assert_eq!(n, 400);
    }

    #[test]
    fn constants_octet_length_integer_equals_4() {
        assert_eq!(OCTET_LENGTH_INTEGER, 4);
        let n = expect_numeric(
            get_column_attribute(&typed_col(SqlDataType::INTEGER), 1, Desc::OctetLength).unwrap(),
        );
        assert_eq!(n, OCTET_LENGTH_INTEGER);
    }

    #[test]
    fn constants_octet_length_bigint_equals_8() {
        assert_eq!(OCTET_LENGTH_BIGINT, 8);
        let n = expect_numeric(
            get_column_attribute(&typed_col(SqlDataType::EXT_BIG_INT), 1, Desc::OctetLength)
                .unwrap(),
        );
        assert_eq!(n, OCTET_LENGTH_BIGINT);
    }

    #[test]
    fn constants_octet_length_double_equals_8() {
        assert_eq!(OCTET_LENGTH_DOUBLE, 8);
        let n = expect_numeric(
            get_column_attribute(&typed_col(SqlDataType::DOUBLE), 1, Desc::OctetLength).unwrap(),
        );
        assert_eq!(n, OCTET_LENGTH_DOUBLE);
    }

    #[test]
    fn constants_octet_length_bit_equals_1() {
        assert_eq!(OCTET_LENGTH_BIT, 1);
        let n = expect_numeric(
            get_column_attribute(&typed_col(SqlDataType::EXT_BIT), 1, Desc::OctetLength).unwrap(),
        );
        assert_eq!(n, OCTET_LENGTH_BIT);
    }

    // -----------------------------------------------------------------------
    // type_name: data-source name vs static fallback
    // -----------------------------------------------------------------------

    #[test]
    fn type_name_prefers_the_data_source_name() {
        let desc = ColumnDescriptor {
            name: "amount".into(),
            type_name: "decimal(10,2)".into(),
            sql_type: SqlDataType::DECIMAL,
            precision: 10,
            scale: 2,
            nullable: Nullable::SqlNullable,
            ..Default::default()
        };
        let attr = get_column_attribute(&desc, 1, Desc::TypeName).expect("TypeName is supported");
        assert_eq!(attr, ColAttrValue::String("decimal(10,2)".into()));
    }

    #[test]
    fn type_name_falls_back_to_the_static_table_when_empty() {
        let desc = ColumnDescriptor {
            name: "n".into(),
            type_name: String::new(),
            sql_type: SqlDataType::INTEGER,
            precision: 10,
            scale: 0,
            nullable: Nullable::SqlNullable,
            ..Default::default()
        };
        let attr = get_column_attribute(&desc, 1, Desc::TypeName).expect("TypeName is supported");
        assert_eq!(attr, ColAttrValue::String("INTEGER".into()));
    }

    #[test]
    fn unknown_type_name_is_the_empty_string_not_the_word_unknown() {
        // Spec, SQL_DESC_TYPE_NAME: "If the type is unknown, an empty string
        // is returned."
        let desc = ColumnDescriptor {
            name: "n".into(),
            type_name: String::new(),
            sql_type: SqlDataType::UNKNOWN_TYPE,
            precision: 0,
            scale: 0,
            nullable: Nullable::SqlNullable,
            ..Default::default()
        };
        let attr = get_column_attribute(&desc, 1, Desc::TypeName).expect("TypeName is supported");
        assert_eq!(attr, ColAttrValue::String(String::new()));
    }

    #[test]
    fn signed_numeric_types_are_not_reported_unsigned() {
        // Spec, SQL_DESC_UNSIGNED: "SQL_TRUE if the column is unsigned (or not
        // numeric)." DECIMAL, NUMERIC, REAL and FLOAT are signed numerics.
        for ty in [
            SqlDataType::DECIMAL,
            SqlDataType::NUMERIC,
            SqlDataType::REAL,
            SqlDataType::FLOAT,
        ] {
            let desc = ColumnDescriptor {
                name: "n".into(),
                type_name: String::new(),
                sql_type: ty,
                precision: 10,
                scale: 2,
                nullable: Nullable::SqlNullable,
                ..Default::default()
            };
            let attr = get_column_attribute(&desc, 1, Desc::Unsigned).expect("Unsigned supported");
            assert_eq!(attr, ColAttrValue::Numeric(0), "{ty:?} must report signed");
        }
    }

    #[test]
    fn non_numeric_types_are_reported_unsigned() {
        // Per spec this is correct, not a bug: non-numeric columns report TRUE.
        for ty in [SqlDataType::VARCHAR, SqlDataType::DATE] {
            let desc = ColumnDescriptor {
                name: "n".into(),
                type_name: String::new(),
                sql_type: ty,
                precision: 10,
                scale: 0,
                nullable: Nullable::SqlNullable,
                ..Default::default()
            };
            let attr = get_column_attribute(&desc, 1, Desc::Unsigned).expect("Unsigned supported");
            assert_eq!(
                attr,
                ColAttrValue::Numeric(1),
                "{ty:?} must report unsigned"
            );
        }
    }
}
