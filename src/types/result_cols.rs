//! Result-set column layouts for the ODBC catalog functions — the fixed
//! column sets `SQLTables`, `SQLColumns`, `SQLPrimaryKeys` and
//! `SQLForeignKeys` must return, as enums that produce the corresponding
//! [`ColumnDescriptor`]s.

use crate::types::{ColumnDescriptor, SqlDataType};

/// The data-source-dependent widths of an ODBC catalog result set's columns.
///
/// The ODBC spec fixes the *structure* of the catalog result sets --
/// `SQLTables`, `SQLColumns`, `SQLPrimaryKeys`, `SQLForeignKeys`,
/// `SQLStatistics`, `SQLSpecialColumns`, `SQLProcedures`,
/// `SQLProcedureColumns`, `SQLColumnPrivileges`, `SQLTablePrivileges` and
/// `SQLGetTypeInfo` -- which columns exist, in what order, and whether each is
/// character, `SMALLINT` or `INTEGER`. That part is identical for every driver
/// and is not configurable here.
///
/// What varies is how wide the character columns are, because that follows the
/// data source's own identifier limit. PostgreSQL caps identifiers at 63
/// characters (`NAMEDATALEN - 1`); a backend that imposes no identifier limit
/// at all uses the conventional 128 default rather than a real bound.
///
/// The same values answer `SQL_MAX_CATALOG_NAME_LEN`,
/// `SQL_MAX_SCHEMA_NAME_LEN`, `SQL_MAX_TABLE_NAME_LEN`,
/// `SQL_MAX_COLUMN_NAME_LEN` and `SQL_MAX_IDENTIFIER_LEN` via
/// [`crate::backend::default_get_info`]. Deriving both from one value is
/// deliberate: a driver whose catalog result sets disagree with its own
/// `SQLGetInfo` answers is telling an application two different things about
/// the same limit, which is the defect that motivated this type.
///
/// Supply a non-default value by overriding
/// [`crate::backend::Backend::catalog_result_column_widths`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogResultColumnWidths {
    /// Width of an identifier column (`TABLE_CAT`, `TABLE_SCHEM`,
    /// `TABLE_NAME`, `COLUMN_NAME`, `TYPE_NAME`, `PK_NAME`, `FK_NAME`).
    ///
    /// `u16` because `SQL_MAX_*_NAME_LEN` is an `SQLUSMALLINT`; widening to
    /// the descriptor's `u32` precision is lossless, so no fallible cast is
    /// needed in either direction.
    pub identifier_len: u16,
    /// Width of a free-text column (`REMARKS`, `COLUMN_DEF`).
    pub remarks_len: u32,
    /// SQL type reported for the character columns.
    ///
    /// `SQL_WVARCHAR` for a Unicode driver -- which the driver crates are,
    /// since only the W-suffix ODBC functions are exported and these columns
    /// are delivered as UTF-16. An ANSI driver would report
    /// `SqlDataType::VARCHAR`.
    pub char_sql_type: SqlDataType,
    /// SQL type reported for the *fixed-width* character columns.
    ///
    /// The spec declares exactly one catalog column as `char(n)` rather than
    /// `varchar`: `SQLStatistics.ASC_OR_DESC`, which is `char(1)`. It gets its
    /// own type for the same reason [`Self::char_sql_type`] exists -- a
    /// Unicode driver must report `SQL_WCHAR`, not the ANSI `SQL_CHAR`.
    pub fixed_char_sql_type: SqlDataType,
}

/// [`CatalogResultColumnWidths::default`]'s identifier width. A `const`
/// rather than inlined directly in `default()` so that the `EXPECTED`
/// snapshot tables in `stackable-odbc-core::backend` and each driver crate can assert
/// their five identifier-length `SQLGetInfo` answers against this single value
/// instead of restating `128` as an unrelated literal that could silently
/// drift from the struct's actual default.
pub const DEFAULT_IDENTIFIER_LEN: u16 = 128;

impl Default for CatalogResultColumnWidths {
    fn default() -> Self {
        Self {
            identifier_len: DEFAULT_IDENTIFIER_LEN,
            remarks_len: 254,
            char_sql_type: SqlDataType::EXT_W_VARCHAR,
            fixed_char_sql_type: SqlDataType::EXT_W_CHAR,
        }
    }
}

/// Width of `SQLTables.TABLE_TYPE`. Spec-fixed, not data-source-dependent:
/// the spec's enumerated values ("TABLE", "VIEW", "SYSTEM TABLE", "GLOBAL
/// TEMPORARY", "LOCAL TEMPORARY", "ALIAS", "SYNONYM") all fit comfortably.
const TABLE_TYPE_LEN: u32 = 32;

/// Width of the "YES" / "NO" / "" columns: `SQLColumns.IS_NULLABLE`,
/// `SQLProcedureColumns.IS_NULLABLE`, `SQLColumnPrivileges.IS_GRANTABLE` and
/// `SQLTablePrivileges.IS_GRANTABLE`. Spec-fixed by the enumerated values.
pub(crate) const YES_NO_LEN: u32 = 3;

/// Width of `SQLStatistics.ASC_OR_DESC`, spec'd `char(1)` ("A" or "D").
pub(crate) const ASC_OR_DESC_LEN: u32 = 1;

/// Width of the `PRIVILEGE` column of `SQLColumnPrivileges` and
/// `SQLTablePrivileges`. Spec-fixed by the enumerated values ("SELECT",
/// "INSERT", "UPDATE", "DELETE", "REFERENCES"), with room for the
/// data-source-specific privileges the spec also permits.
pub(crate) const PRIVILEGE_LEN: u32 = 32;

/// Width of `SQLGetTypeInfo.LITERAL_PREFIX` / `LITERAL_SUFFIX`. These hold a
/// literal delimiter such as `'` or `0x`, not an identifier, so the width must
/// *not* follow [`CatalogResultColumnWidths::identifier_len`].
pub(crate) const LITERAL_AFFIX_LEN: u32 = 128;

/// Width of `SQLGetTypeInfo.CREATE_PARAMS`, a comma-separated keyword list
/// ("length", "precision,scale"). Not an identifier; see [`LITERAL_AFFIX_LEN`].
pub(crate) const CREATE_PARAMS_LEN: u32 = 128;

/// Digits in a `SQL_SMALLINT` catalog column. Spec-fixed.
pub(crate) const SMALLINT_LEN: u32 = 5;

/// Digits in a `SQL_INTEGER` catalog column. Spec-fixed.
pub(crate) const INTEGER_LEN: u32 = 10;

/// A character column in a catalog result set, at the driver's identifier width.
pub(crate) fn identifier(
    name: &'static str,
    widths: &CatalogResultColumnWidths,
    nullable: bool,
) -> ColumnDescriptor {
    character(name, u32::from(widths.identifier_len), widths, nullable)
}

/// A character column in a catalog result set, at an explicit width.
pub(crate) fn character(
    name: &'static str,
    precision: u32,
    widths: &CatalogResultColumnWidths,
    nullable: bool,
) -> ColumnDescriptor {
    ColumnDescriptor {
        name: name.into(),
        type_name: String::new(),
        sql_type: widths.char_sql_type,
        precision,
        scale: 0,
        nullable,
    }
}

/// A fixed-width (`CHAR`) character column in a catalog result set.
pub(crate) fn fixed_char(
    name: &'static str,
    precision: u32,
    widths: &CatalogResultColumnWidths,
    nullable: bool,
) -> ColumnDescriptor {
    ColumnDescriptor {
        name: name.into(),
        type_name: String::new(),
        sql_type: widths.fixed_char_sql_type,
        precision,
        scale: 0,
        nullable,
    }
}

/// A `SQL_SMALLINT` column in a catalog result set.
pub(crate) fn smallint(name: &'static str, nullable: bool) -> ColumnDescriptor {
    ColumnDescriptor {
        name: name.into(),
        type_name: String::new(),
        sql_type: SqlDataType::SMALLINT,
        precision: SMALLINT_LEN,
        scale: 0,
        nullable,
    }
}

/// A `SQL_INTEGER` column in a catalog result set.
pub(crate) fn integer(name: &'static str, nullable: bool) -> ColumnDescriptor {
    ColumnDescriptor {
        name: name.into(),
        type_name: String::new(),
        sql_type: SqlDataType::INTEGER,
        precision: INTEGER_LEN,
        scale: 0,
        nullable,
    }
}

/// 1-indexed column positions in the `SQLTablesW` result set, as defined by the ODBC spec.
///
/// Use `.pos()` to get the column number to pass to `describe_col` / `get_data`.
#[derive(Debug, Clone, Copy)]
pub enum TablesResultCol {
    TableCat = 1,
    TableSchem = 2,
    TableName = 3,
    TableType = 4,
    Remarks = 5,
}

impl TablesResultCol {
    /// 1-indexed column position for use with `describe_col` / `get_data`.
    pub fn pos(self) -> u16 {
        self as u16
    }

    /// ODBC spec column name for use in `ColumnDescriptor`.
    pub fn name(self) -> &'static str {
        match self {
            Self::TableCat => "TABLE_CAT",
            Self::TableSchem => "TABLE_SCHEM",
            Self::TableName => "TABLE_NAME",
            Self::TableType => "TABLE_TYPE",
            Self::Remarks => "REMARKS",
        }
    }

    /// Spec-defined descriptor for this result-set column, at the widths
    /// `widths` gives.
    ///
    /// Every column is nullable. The spec's own column table does not mark
    /// any of the five as "not NULL", and its Comments section is explicit
    /// that under the enumeration special cases (e.g. `SQL_ALL_CATALOGS`
    /// with empty schema/table patterns) "all columns except TABLE_CAT
    /// contain NULLs" -- so TABLE_NAME and TABLE_TYPE, in particular, must
    /// be reported nullable even though a normal (non-enumeration) row
    /// always populates them.
    pub fn descriptor(self, widths: &CatalogResultColumnWidths) -> ColumnDescriptor {
        match self {
            Self::TableCat => identifier(self.name(), widths, true),
            Self::TableSchem => identifier(self.name(), widths, true),
            Self::TableName => identifier(self.name(), widths, true),
            Self::TableType => character(self.name(), TABLE_TYPE_LEN, widths, true),
            Self::Remarks => character(self.name(), widths.remarks_len, widths, true),
        }
    }

    /// Descriptors for every column of this result set, in spec order.
    pub fn all_descriptors(widths: &CatalogResultColumnWidths) -> Vec<ColumnDescriptor> {
        [
            Self::TableCat,
            Self::TableSchem,
            Self::TableName,
            Self::TableType,
            Self::Remarks,
        ]
        .into_iter()
        .map(|c| c.descriptor(widths))
        .collect()
    }
}

/// 1-indexed column positions in the `SQLColumnsW` result set, as defined by the ODBC spec.
///
/// Use `.pos()` to get the column number to pass to `describe_col` / `get_data`.
#[derive(Debug, Clone, Copy)]
pub enum ColumnsResultCol {
    TableCat = 1,
    TableSchem = 2,
    TableName = 3,
    ColumnName = 4,
    DataType = 5,
    TypeName = 6,
    ColumnSize = 7,
    BufferLength = 8,
    DecimalDigits = 9,
    NumPrecRadix = 10,
    Nullable = 11,
    Remarks = 12,
    ColumnDef = 13,
    SqlDataType = 14,
    SqlDatetimeSub = 15,
    CharOctetLength = 16,
    OrdinalPosition = 17,
    IsNullable = 18,
}

impl ColumnsResultCol {
    /// 1-indexed column position for use with `describe_col` / `get_data`.
    pub fn pos(self) -> u16 {
        self as u16
    }

    /// ODBC spec column name for use in `ColumnDescriptor`.
    pub fn name(self) -> &'static str {
        match self {
            Self::TableCat => "TABLE_CAT",
            Self::TableSchem => "TABLE_SCHEM",
            Self::TableName => "TABLE_NAME",
            Self::ColumnName => "COLUMN_NAME",
            Self::DataType => "DATA_TYPE",
            Self::TypeName => "TYPE_NAME",
            Self::ColumnSize => "COLUMN_SIZE",
            Self::BufferLength => "BUFFER_LENGTH",
            Self::DecimalDigits => "DECIMAL_DIGITS",
            Self::NumPrecRadix => "NUM_PREC_RADIX",
            Self::Nullable => "NULLABLE",
            Self::Remarks => "REMARKS",
            Self::ColumnDef => "COLUMN_DEF",
            Self::SqlDataType => "SQL_DATA_TYPE",
            Self::SqlDatetimeSub => "SQL_DATETIME_SUB",
            Self::CharOctetLength => "CHAR_OCTET_LENGTH",
            Self::OrdinalPosition => "ORDINAL_POSITION",
            Self::IsNullable => "IS_NULLABLE",
        }
    }

    /// Spec-defined descriptor for this result-set column, at the widths
    /// `widths` gives.
    pub fn descriptor(self, widths: &CatalogResultColumnWidths) -> ColumnDescriptor {
        match self {
            Self::TableCat => identifier(self.name(), widths, true),
            Self::TableSchem => identifier(self.name(), widths, true),
            Self::TableName => identifier(self.name(), widths, false),
            Self::ColumnName => identifier(self.name(), widths, false),
            Self::DataType => smallint(self.name(), false),
            Self::TypeName => identifier(self.name(), widths, false),
            Self::ColumnSize => integer(self.name(), true),
            Self::BufferLength => integer(self.name(), true),
            Self::DecimalDigits => smallint(self.name(), true),
            Self::NumPrecRadix => smallint(self.name(), true),
            Self::Nullable => smallint(self.name(), false),
            Self::Remarks => character(self.name(), widths.remarks_len, widths, true),
            Self::ColumnDef => character(self.name(), widths.remarks_len, widths, true),
            Self::SqlDataType => smallint(self.name(), false),
            Self::SqlDatetimeSub => smallint(self.name(), true),
            Self::CharOctetLength => integer(self.name(), true),
            Self::OrdinalPosition => integer(self.name(), false),
            Self::IsNullable => character(self.name(), YES_NO_LEN, widths, true),
        }
    }

    /// Descriptors for every column of this result set, in spec order.
    pub fn all_descriptors(widths: &CatalogResultColumnWidths) -> Vec<ColumnDescriptor> {
        [
            Self::TableCat,
            Self::TableSchem,
            Self::TableName,
            Self::ColumnName,
            Self::DataType,
            Self::TypeName,
            Self::ColumnSize,
            Self::BufferLength,
            Self::DecimalDigits,
            Self::NumPrecRadix,
            Self::Nullable,
            Self::Remarks,
            Self::ColumnDef,
            Self::SqlDataType,
            Self::SqlDatetimeSub,
            Self::CharOctetLength,
            Self::OrdinalPosition,
            Self::IsNullable,
        ]
        .into_iter()
        .map(|c| c.descriptor(widths))
        .collect()
    }
}

/// 1-indexed column positions in the `SQLPrimaryKeysW` result set, as defined by the ODBC spec.
///
/// Use `.pos()` to get the column number to pass to `describe_col` / `get_data`.
#[derive(Debug, Clone, Copy)]
pub enum PrimaryKeysResultCol {
    TableCat = 1,
    TableSchem = 2,
    TableName = 3,
    ColumnName = 4,
    KeySeq = 5,
    PkName = 6,
}

impl PrimaryKeysResultCol {
    /// 1-indexed column position for use with `describe_col` / `get_data`.
    pub fn pos(self) -> u16 {
        self as u16
    }

    /// ODBC spec column name for use in `ColumnDescriptor`.
    pub fn name(self) -> &'static str {
        match self {
            Self::TableCat => "TABLE_CAT",
            Self::TableSchem => "TABLE_SCHEM",
            Self::TableName => "TABLE_NAME",
            Self::ColumnName => "COLUMN_NAME",
            Self::KeySeq => "KEY_SEQ",
            Self::PkName => "PK_NAME",
        }
    }

    /// Spec-defined descriptor for this result-set column, at the widths
    /// `widths` gives.
    pub fn descriptor(self, widths: &CatalogResultColumnWidths) -> ColumnDescriptor {
        match self {
            Self::TableCat => identifier(self.name(), widths, true),
            Self::TableSchem => identifier(self.name(), widths, true),
            Self::TableName => identifier(self.name(), widths, false),
            Self::ColumnName => identifier(self.name(), widths, false),
            Self::KeySeq => smallint(self.name(), false),
            Self::PkName => identifier(self.name(), widths, true),
        }
    }

    /// Descriptors for every column of this result set, in spec order.
    pub fn all_descriptors(widths: &CatalogResultColumnWidths) -> Vec<ColumnDescriptor> {
        [
            Self::TableCat,
            Self::TableSchem,
            Self::TableName,
            Self::ColumnName,
            Self::KeySeq,
            Self::PkName,
        ]
        .into_iter()
        .map(|c| c.descriptor(widths))
        .collect()
    }
}

/// 1-indexed column positions in the `SQLForeignKeysW` result set, as defined by the ODBC spec.
///
/// Use `.pos()` to get the column number to pass to `describe_col` / `get_data`.
#[derive(Debug, Clone, Copy)]
pub enum ForeignKeysResultCol {
    PkTableCat = 1,
    PkTableSchem = 2,
    PkTableName = 3,
    PkColumnName = 4,
    FkTableCat = 5,
    FkTableSchem = 6,
    FkTableName = 7,
    FkColumnName = 8,
    KeySeq = 9,
    UpdateRule = 10,
    DeleteRule = 11,
    FkName = 12,
    PkName = 13,
    Deferrability = 14,
}

impl ForeignKeysResultCol {
    /// 1-indexed column position for use with `describe_col` / `get_data`.
    pub fn pos(self) -> u16 {
        self as u16
    }

    /// ODBC spec column name for use in `ColumnDescriptor`.
    pub fn name(self) -> &'static str {
        match self {
            Self::PkTableCat => "PKTABLE_CAT",
            Self::PkTableSchem => "PKTABLE_SCHEM",
            Self::PkTableName => "PKTABLE_NAME",
            Self::PkColumnName => "PKCOLUMN_NAME",
            Self::FkTableCat => "FKTABLE_CAT",
            Self::FkTableSchem => "FKTABLE_SCHEM",
            Self::FkTableName => "FKTABLE_NAME",
            Self::FkColumnName => "FKCOLUMN_NAME",
            Self::KeySeq => "KEY_SEQ",
            Self::UpdateRule => "UPDATE_RULE",
            Self::DeleteRule => "DELETE_RULE",
            Self::FkName => "FK_NAME",
            Self::PkName => "PK_NAME",
            Self::Deferrability => "DEFERRABILITY",
        }
    }

    /// Spec-defined descriptor for this result-set column, at the widths
    /// `widths` gives.
    pub fn descriptor(self, widths: &CatalogResultColumnWidths) -> ColumnDescriptor {
        match self {
            Self::PkTableCat => identifier(self.name(), widths, true),
            Self::PkTableSchem => identifier(self.name(), widths, true),
            Self::PkTableName => identifier(self.name(), widths, false),
            Self::PkColumnName => identifier(self.name(), widths, false),
            Self::FkTableCat => identifier(self.name(), widths, true),
            Self::FkTableSchem => identifier(self.name(), widths, true),
            Self::FkTableName => identifier(self.name(), widths, false),
            Self::FkColumnName => identifier(self.name(), widths, false),
            Self::KeySeq => smallint(self.name(), false),
            Self::UpdateRule => smallint(self.name(), true),
            Self::DeleteRule => smallint(self.name(), true),
            Self::FkName => identifier(self.name(), widths, true),
            Self::PkName => identifier(self.name(), widths, true),
            Self::Deferrability => smallint(self.name(), true),
        }
    }

    /// Descriptors for every column of this result set, in spec order.
    pub fn all_descriptors(widths: &CatalogResultColumnWidths) -> Vec<ColumnDescriptor> {
        [
            Self::PkTableCat,
            Self::PkTableSchem,
            Self::PkTableName,
            Self::PkColumnName,
            Self::FkTableCat,
            Self::FkTableSchem,
            Self::FkTableName,
            Self::FkColumnName,
            Self::KeySeq,
            Self::UpdateRule,
            Self::DeleteRule,
            Self::FkName,
            Self::PkName,
            Self::Deferrability,
        ]
        .into_iter()
        .map(|c| c.descriptor(widths))
        .collect()
    }
}

/// Column descriptors for the SQLStatistics result set (13 columns per spec).
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlstatistics-function>
///
/// `TABLE_NAME` (3) and `TYPE` (7) are the only two the spec marks "not NULL";
/// every other column is NULL for a `SQL_TABLE_STAT` row. `ASC_OR_DESC` is the
/// one catalog column the spec declares `char(1)` rather than `varchar`.
/// `FILTER_CONDITION` holds a free-text predicate, so it takes the free-text
/// width rather than the identifier width.
pub fn statistics_columns(widths: &CatalogResultColumnWidths) -> Vec<ColumnDescriptor> {
    vec![
        identifier("TABLE_CAT", widths, true),
        identifier("TABLE_SCHEM", widths, true),
        identifier("TABLE_NAME", widths, false),
        smallint("NON_UNIQUE", true),
        identifier("INDEX_QUALIFIER", widths, true),
        identifier("INDEX_NAME", widths, true),
        smallint("TYPE", false),
        smallint("ORDINAL_POSITION", true),
        identifier("COLUMN_NAME", widths, true),
        fixed_char("ASC_OR_DESC", ASC_OR_DESC_LEN, widths, true),
        integer("CARDINALITY", true),
        integer("PAGES", true),
        character("FILTER_CONDITION", widths.remarks_len, widths, true),
    ]
}

/// Column descriptors for the SQLSpecialColumns result set (8 columns per spec).
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlspecialcolumns-function>
///
/// `COLUMN_NAME` (2), `DATA_TYPE` (3) and `TYPE_NAME` (4) are marked "not
/// NULL"; `SCOPE` is nullable because it is NULL when *IdentifierType* is
/// `SQL_ROWVER`.
pub fn special_columns_columns(widths: &CatalogResultColumnWidths) -> Vec<ColumnDescriptor> {
    vec![
        smallint("SCOPE", true),
        identifier("COLUMN_NAME", widths, false),
        smallint("DATA_TYPE", false),
        identifier("TYPE_NAME", widths, false),
        integer("COLUMN_SIZE", true),
        integer("BUFFER_LENGTH", true),
        smallint("DECIMAL_DIGITS", true),
        smallint("PSEUDO_COLUMN", true),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A PostgreSQL-shaped override: identifiers cap at NAMEDATALEN - 1.
    /// Used to prove the widths genuinely flow from CatalogResultColumnWidths rather
    /// than from a constant that merely happens to equal the default.
    fn postgres_like() -> CatalogResultColumnWidths {
        CatalogResultColumnWidths {
            identifier_len: 63,
            ..CatalogResultColumnWidths::default()
        }
    }

    /// The invariant that keeps a future column addition honest: a new enum
    /// variant with no descriptor arm fails here rather than reaching a
    /// caller. Same guard shape used for the type-info tables.
    #[test]
    fn every_result_set_has_one_descriptor_per_column() {
        let s = CatalogResultColumnWidths::default();
        assert_eq!(TablesResultCol::all_descriptors(&s).len(), 5);
        assert_eq!(ColumnsResultCol::all_descriptors(&s).len(), 18);
        assert_eq!(PrimaryKeysResultCol::all_descriptors(&s).len(), 6);
        assert_eq!(ForeignKeysResultCol::all_descriptors(&s).len(), 14);
    }

    /// Descriptors must be in spec order, because the caller feeds them to
    /// SyntheticStatement positionally -- a reordering would silently
    /// mislabel every row.
    #[test]
    fn descriptors_are_in_spec_column_order() {
        let s = CatalogResultColumnWidths::default();
        let descs = ColumnsResultCol::all_descriptors(&s);
        assert_eq!(descs[0].name, "TABLE_CAT");
        assert_eq!(descs[4].name, "DATA_TYPE");
        assert_eq!(descs[17].name, "IS_NULLABLE");

        let fk = ForeignKeysResultCol::all_descriptors(&s);
        assert_eq!(fk[0].name, "PKTABLE_CAT");
        assert_eq!(fk[13].name, "DEFERRABILITY");
    }

    /// The widths must come from the supplied widths, not from a baked-in
    /// constant. A driver for a data source with a real identifier limit
    /// (PostgreSQL's 63) must see that limit in its catalog result sets.
    #[test]
    fn identifier_widths_follow_the_supplied_widths() {
        let s = postgres_like();
        for desc in TablesResultCol::all_descriptors(&s) {
            if desc.name.ends_with("_CAT")
                || desc.name.ends_with("_SCHEM")
                || desc.name.ends_with("_NAME")
            {
                assert_eq!(desc.precision, 63, "{}", desc.name);
            }
        }
        for desc in ForeignKeysResultCol::all_descriptors(&s) {
            if desc.sql_type == s.char_sql_type && desc.name != "REMARKS" {
                assert_eq!(desc.precision, 63, "{}", desc.name);
            }
        }
    }

    /// The character type is a property of the driver, not of the spec: a
    /// hypothetical ANSI driver would report SQL_VARCHAR here.
    #[test]
    fn the_character_type_follows_the_supplied_widths() {
        let ansi = CatalogResultColumnWidths {
            char_sql_type: SqlDataType::VARCHAR,
            ..CatalogResultColumnWidths::default()
        };
        let descs = TablesResultCol::all_descriptors(&ansi);
        assert!(descs.iter().any(|d| d.sql_type == SqlDataType::VARCHAR));
        assert!(
            !descs
                .iter()
                .any(|d| d.sql_type == SqlDataType::EXT_W_VARCHAR)
        );
    }

    /// A SMALLINT column cannot have 50 digits of precision. Integer column
    /// widths are spec-fixed and must NOT follow identifier_len.
    #[test]
    fn integer_column_widths_are_fixed_regardless_of_widths() {
        for s in [CatalogResultColumnWidths::default(), postgres_like()] {
            for desc in ColumnsResultCol::all_descriptors(&s) {
                match desc.sql_type {
                    SqlDataType::SMALLINT => {
                        assert_eq!(desc.precision, SMALLINT_LEN, "{}", desc.name)
                    }
                    SqlDataType::INTEGER => {
                        assert_eq!(desc.precision, INTEGER_LEN, "{}", desc.name)
                    }
                    _ => {}
                }
            }
        }
    }

    /// Every catalog result-set column is one of the three shapes the spec
    /// defines. Anything else means a descriptor arm was written by hand
    /// instead of going through the constructors.
    #[test]
    fn every_column_is_char_smallint_or_integer() {
        let s = CatalogResultColumnWidths::default();
        let all = TablesResultCol::all_descriptors(&s)
            .into_iter()
            .chain(ColumnsResultCol::all_descriptors(&s))
            .chain(PrimaryKeysResultCol::all_descriptors(&s))
            .chain(ForeignKeysResultCol::all_descriptors(&s));
        for desc in all {
            assert!(
                matches!(
                    desc.sql_type,
                    SqlDataType::EXT_W_VARCHAR | SqlDataType::SMALLINT | SqlDataType::INTEGER
                ),
                "{} has unexpected type {:?}",
                desc.name,
                desc.sql_type
            );
        }
    }

    /// The default must match what the driver crates report, so none has to
    /// override it.
    #[test]
    fn the_default_is_the_convention_for_a_limitless_data_source() {
        let s = CatalogResultColumnWidths::default();
        assert_eq!(s.identifier_len, 128);
        assert_eq!(s.remarks_len, 254);
        assert_eq!(s.char_sql_type, SqlDataType::EXT_W_VARCHAR);
    }

    /// Every SQLTables column must be nullable. The spec's column table
    /// marks none of the five "not NULL", and its Comments section states
    /// that under `SQL_ALL_CATALOGS` (and the analogous schema/table-type
    /// enumeration cases) all columns except the one being enumerated
    /// contain NULLs -- so, unlike SQLColumns/SQLPrimaryKeys/SQLForeignKeys,
    /// SQLTables has no column the driver may report as non-nullable. This
    /// pins that so a future edit cannot silently reintroduce `nullable:
    /// false` on TABLE_NAME or TABLE_TYPE.
    #[test]
    fn every_sqltables_column_is_nullable() {
        let s = CatalogResultColumnWidths::default();
        for desc in TablesResultCol::all_descriptors(&s) {
            assert!(desc.nullable, "{} must be nullable", desc.name);
        }
    }
}
