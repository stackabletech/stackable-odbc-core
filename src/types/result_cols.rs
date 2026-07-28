//! Result-set column layouts for the ODBC catalog functions — the fixed
//! column sets `SQLTables`, `SQLColumns`, `SQLPrimaryKeys` and
//! `SQLForeignKeys` must return, as enums that produce the corresponding
//! [`ColumnDescriptor`]s.

use crate::types::{ColumnDescriptor, Nullable, SqlDataType};

/// The data-source-dependent widths of an ODBC catalog result set's columns.
///
/// The ODBC spec fixes the *structure* of the catalog result sets --
/// `SQLTables`, `SQLColumns`, `SQLPrimaryKeys`, `SQLForeignKeys`,
/// `SQLStatistics`, `SQLSpecialColumns`, `SQLProcedures`,
/// `SQLProcedureColumns`, `SQLColumnPrivileges`, `SQLTablePrivileges` and
/// `SQLGetTypeInfo` — which columns exist, in what order, and whether each is
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
#[non_exhaustive]
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
    /// `SQL_WVARCHAR` for a Unicode driver — which the driver crates are,
    /// since only the W-suffix ODBC functions are exported and these columns
    /// are delivered as UTF-16. An ANSI driver would report
    /// `SqlDataType::VARCHAR`.
    pub char_sql_type: SqlDataType,
    /// SQL type reported for the *fixed-width* character columns.
    ///
    /// The spec declares exactly one catalog column as `char(n)` rather than
    /// `varchar`: `SQLStatistics.ASC_OR_DESC`, which is `char(1)`. It gets its
    /// own type for the same reason [`Self::char_sql_type`] exists — a
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

impl CatalogResultColumnWidths {
    /// Sets the width of an identifier column.
    #[must_use]
    pub fn with_identifier_len(mut self, len: u16) -> Self {
        self.identifier_len = len;
        self
    }

    /// Sets the width of a `REMARKS` column.
    #[must_use]
    pub fn with_remarks_len(mut self, len: u32) -> Self {
        self.remarks_len = len;
        self
    }

    /// Sets the SQL types used for variable- and fixed-width character columns.
    #[must_use]
    pub fn with_char_sql_types(mut self, variable: SqlDataType, fixed: SqlDataType) -> Self {
        self.char_sql_type = variable;
        self.fixed_char_sql_type = fixed;
        self
    }
}

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
    nullable: Nullable,
) -> ColumnDescriptor {
    character(name, u32::from(widths.identifier_len), widths, nullable)
}

/// A character column in a catalog result set, at an explicit width.
pub(crate) fn character(
    name: &'static str,
    precision: u32,
    widths: &CatalogResultColumnWidths,
    nullable: Nullable,
) -> ColumnDescriptor {
    ColumnDescriptor {
        name: name.into(),
        type_name: String::new(),
        sql_type: widths.char_sql_type,
        precision,
        scale: 0,
        nullable,
        ..Default::default()
    }
}

/// A fixed-width (`CHAR`) character column in a catalog result set.
pub(crate) fn fixed_char(
    name: &'static str,
    precision: u32,
    widths: &CatalogResultColumnWidths,
    nullable: Nullable,
) -> ColumnDescriptor {
    ColumnDescriptor {
        name: name.into(),
        type_name: String::new(),
        sql_type: widths.fixed_char_sql_type,
        precision,
        scale: 0,
        nullable,
        ..Default::default()
    }
}

/// A `SQL_SMALLINT` column in a catalog result set.
pub(crate) fn smallint(name: &'static str, nullable: Nullable) -> ColumnDescriptor {
    ColumnDescriptor {
        name: name.into(),
        type_name: String::new(),
        sql_type: SqlDataType::SMALLINT,
        precision: SMALLINT_LEN,
        scale: 0,
        nullable,
        ..Default::default()
    }
}

/// A `SQL_INTEGER` column in a catalog result set.
pub(crate) fn integer(name: &'static str, nullable: Nullable) -> ColumnDescriptor {
    ColumnDescriptor {
        name: name.into(),
        type_name: String::new(),
        sql_type: SqlDataType::INTEGER,
        precision: INTEGER_LEN,
        scale: 0,
        nullable,
        ..Default::default()
    }
}

/// 1-indexed column positions in the `SQLTablesW` result set, as defined by the ODBC spec.
///
/// Use `.pos()` to get the column number to pass to `describe_col` / `get_data`.
#[derive(Debug, Clone, Copy)]
pub enum TablesResultCol {
    /// `TABLE_CAT` — column 1 of the `SQLTables` result set.
    TableCat = 1,
    /// `TABLE_SCHEM` — column 2 of the `SQLTables` result set.
    TableSchem = 2,
    /// `TABLE_NAME` — column 3 of the `SQLTables` result set.
    TableName = 3,
    /// `TABLE_TYPE` — column 4 of the `SQLTables` result set.
    TableType = 4,
    /// `REMARKS` — column 5 of the `SQLTables` result set.
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
    /// contain NULLs" — so TABLE_NAME and TABLE_TYPE, in particular, must
    /// be reported nullable even though a normal (non-enumeration) row
    /// always populates them.
    pub fn descriptor(self, widths: &CatalogResultColumnWidths) -> ColumnDescriptor {
        match self {
            Self::TableCat => identifier(self.name(), widths, Nullable::SqlNullable),
            Self::TableSchem => identifier(self.name(), widths, Nullable::SqlNullable),
            Self::TableName => identifier(self.name(), widths, Nullable::SqlNullable),
            Self::TableType => {
                character(self.name(), TABLE_TYPE_LEN, widths, Nullable::SqlNullable)
            }
            Self::Remarks => character(
                self.name(),
                widths.remarks_len,
                widths,
                Nullable::SqlNullable,
            ),
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
    /// `TABLE_CAT` — column 1 of the `SQLColumns` result set.
    TableCat = 1,
    /// `TABLE_SCHEM` — column 2 of the `SQLColumns` result set.
    TableSchem = 2,
    /// `TABLE_NAME` — column 3 of the `SQLColumns` result set.
    TableName = 3,
    /// `COLUMN_NAME` — column 4 of the `SQLColumns` result set.
    ColumnName = 4,
    /// `DATA_TYPE` — column 5 of the `SQLColumns` result set.
    DataType = 5,
    /// `TYPE_NAME` — column 6 of the `SQLColumns` result set.
    TypeName = 6,
    /// `COLUMN_SIZE` — column 7 of the `SQLColumns` result set.
    ColumnSize = 7,
    /// `BUFFER_LENGTH` — column 8 of the `SQLColumns` result set.
    BufferLength = 8,
    /// `DECIMAL_DIGITS` — column 9 of the `SQLColumns` result set.
    DecimalDigits = 9,
    /// `NUM_PREC_RADIX` — column 10 of the `SQLColumns` result set.
    NumPrecRadix = 10,
    /// `NULLABLE` — column 11 of the `SQLColumns` result set.
    Nullable = 11,
    /// `REMARKS` — column 12 of the `SQLColumns` result set.
    Remarks = 12,
    /// `COLUMN_DEF` — column 13 of the `SQLColumns` result set.
    ColumnDef = 13,
    /// `SQL_DATA_TYPE` — column 14 of the `SQLColumns` result set.
    SqlDataType = 14,
    /// `SQL_DATETIME_SUB` — column 15 of the `SQLColumns` result set.
    SqlDatetimeSub = 15,
    /// `CHAR_OCTET_LENGTH` — column 16 of the `SQLColumns` result set.
    CharOctetLength = 16,
    /// `ORDINAL_POSITION` — column 17 of the `SQLColumns` result set.
    OrdinalPosition = 17,
    /// `IS_NULLABLE` — column 18 of the `SQLColumns` result set.
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
            Self::TableCat => identifier(self.name(), widths, Nullable::SqlNullable),
            Self::TableSchem => identifier(self.name(), widths, Nullable::SqlNullable),
            Self::TableName => identifier(self.name(), widths, Nullable::SqlNoNulls),
            Self::ColumnName => identifier(self.name(), widths, Nullable::SqlNoNulls),
            Self::DataType => smallint(self.name(), Nullable::SqlNoNulls),
            Self::TypeName => identifier(self.name(), widths, Nullable::SqlNoNulls),
            Self::ColumnSize => integer(self.name(), Nullable::SqlNullable),
            Self::BufferLength => integer(self.name(), Nullable::SqlNullable),
            Self::DecimalDigits => smallint(self.name(), Nullable::SqlNullable),
            Self::NumPrecRadix => smallint(self.name(), Nullable::SqlNullable),
            Self::Nullable => smallint(self.name(), Nullable::SqlNoNulls),
            Self::Remarks => character(
                self.name(),
                widths.remarks_len,
                widths,
                Nullable::SqlNullable,
            ),
            Self::ColumnDef => character(
                self.name(),
                widths.remarks_len,
                widths,
                Nullable::SqlNullable,
            ),
            Self::SqlDataType => smallint(self.name(), Nullable::SqlNoNulls),
            Self::SqlDatetimeSub => smallint(self.name(), Nullable::SqlNullable),
            Self::CharOctetLength => integer(self.name(), Nullable::SqlNullable),
            Self::OrdinalPosition => integer(self.name(), Nullable::SqlNoNulls),
            Self::IsNullable => character(self.name(), YES_NO_LEN, widths, Nullable::SqlNullable),
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
    /// `TABLE_CAT` — column 1 of the `SQLPrimaryKeys` result set.
    TableCat = 1,
    /// `TABLE_SCHEM` — column 2 of the `SQLPrimaryKeys` result set.
    TableSchem = 2,
    /// `TABLE_NAME` — column 3 of the `SQLPrimaryKeys` result set.
    TableName = 3,
    /// `COLUMN_NAME` — column 4 of the `SQLPrimaryKeys` result set.
    ColumnName = 4,
    /// `KEY_SEQ` — column 5 of the `SQLPrimaryKeys` result set.
    KeySeq = 5,
    /// `PK_NAME` — column 6 of the `SQLPrimaryKeys` result set.
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
            Self::TableCat => identifier(self.name(), widths, Nullable::SqlNullable),
            Self::TableSchem => identifier(self.name(), widths, Nullable::SqlNullable),
            Self::TableName => identifier(self.name(), widths, Nullable::SqlNoNulls),
            Self::ColumnName => identifier(self.name(), widths, Nullable::SqlNoNulls),
            Self::KeySeq => smallint(self.name(), Nullable::SqlNoNulls),
            Self::PkName => identifier(self.name(), widths, Nullable::SqlNullable),
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
    /// `PKTABLE_CAT` — column 1 of the `SQLForeignKeys` result set.
    PkTableCat = 1,
    /// `PKTABLE_SCHEM` — column 2 of the `SQLForeignKeys` result set.
    PkTableSchem = 2,
    /// `PKTABLE_NAME` — column 3 of the `SQLForeignKeys` result set.
    PkTableName = 3,
    /// `PKCOLUMN_NAME` — column 4 of the `SQLForeignKeys` result set.
    PkColumnName = 4,
    /// `FKTABLE_CAT` — column 5 of the `SQLForeignKeys` result set.
    FkTableCat = 5,
    /// `FKTABLE_SCHEM` — column 6 of the `SQLForeignKeys` result set.
    FkTableSchem = 6,
    /// `FKTABLE_NAME` — column 7 of the `SQLForeignKeys` result set.
    FkTableName = 7,
    /// `FKCOLUMN_NAME` — column 8 of the `SQLForeignKeys` result set.
    FkColumnName = 8,
    /// `KEY_SEQ` — column 9 of the `SQLForeignKeys` result set.
    KeySeq = 9,
    /// `UPDATE_RULE` — column 10 of the `SQLForeignKeys` result set.
    UpdateRule = 10,
    /// `DELETE_RULE` — column 11 of the `SQLForeignKeys` result set.
    DeleteRule = 11,
    /// `FK_NAME` — column 12 of the `SQLForeignKeys` result set.
    FkName = 12,
    /// `PK_NAME` — column 13 of the `SQLForeignKeys` result set.
    PkName = 13,
    /// `DEFERRABILITY` — column 14 of the `SQLForeignKeys` result set.
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
            Self::PkTableCat => identifier(self.name(), widths, Nullable::SqlNullable),
            Self::PkTableSchem => identifier(self.name(), widths, Nullable::SqlNullable),
            Self::PkTableName => identifier(self.name(), widths, Nullable::SqlNoNulls),
            Self::PkColumnName => identifier(self.name(), widths, Nullable::SqlNoNulls),
            Self::FkTableCat => identifier(self.name(), widths, Nullable::SqlNullable),
            Self::FkTableSchem => identifier(self.name(), widths, Nullable::SqlNullable),
            Self::FkTableName => identifier(self.name(), widths, Nullable::SqlNoNulls),
            Self::FkColumnName => identifier(self.name(), widths, Nullable::SqlNoNulls),
            Self::KeySeq => smallint(self.name(), Nullable::SqlNoNulls),
            Self::UpdateRule => smallint(self.name(), Nullable::SqlNullable),
            Self::DeleteRule => smallint(self.name(), Nullable::SqlNullable),
            Self::FkName => identifier(self.name(), widths, Nullable::SqlNullable),
            Self::PkName => identifier(self.name(), widths, Nullable::SqlNullable),
            Self::Deferrability => smallint(self.name(), Nullable::SqlNullable),
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
        identifier("TABLE_CAT", widths, Nullable::SqlNullable),
        identifier("TABLE_SCHEM", widths, Nullable::SqlNullable),
        identifier("TABLE_NAME", widths, Nullable::SqlNoNulls),
        smallint("NON_UNIQUE", Nullable::SqlNullable),
        identifier("INDEX_QUALIFIER", widths, Nullable::SqlNullable),
        identifier("INDEX_NAME", widths, Nullable::SqlNullable),
        smallint("TYPE", Nullable::SqlNoNulls),
        smallint("ORDINAL_POSITION", Nullable::SqlNullable),
        identifier("COLUMN_NAME", widths, Nullable::SqlNullable),
        fixed_char(
            "ASC_OR_DESC",
            ASC_OR_DESC_LEN,
            widths,
            Nullable::SqlNullable,
        ),
        integer("CARDINALITY", Nullable::SqlNullable),
        integer("PAGES", Nullable::SqlNullable),
        character(
            "FILTER_CONDITION",
            widths.remarks_len,
            widths,
            Nullable::SqlNullable,
        ),
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
        smallint("SCOPE", Nullable::SqlNullable),
        identifier("COLUMN_NAME", widths, Nullable::SqlNoNulls),
        smallint("DATA_TYPE", Nullable::SqlNoNulls),
        identifier("TYPE_NAME", widths, Nullable::SqlNoNulls),
        integer("COLUMN_SIZE", Nullable::SqlNullable),
        integer("BUFFER_LENGTH", Nullable::SqlNullable),
        smallint("DECIMAL_DIGITS", Nullable::SqlNullable),
        smallint("PSEUDO_COLUMN", Nullable::SqlNullable),
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
    /// SyntheticStatement positionally — a reordering would silently
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
    /// contain NULLs — so, unlike SQLColumns/SQLPrimaryKeys/SQLForeignKeys,
    /// SQLTables has no column the driver may report as non-nullable. This
    /// pins that so a future edit cannot silently reintroduce `nullable:
    /// false` on TABLE_NAME or TABLE_TYPE.
    #[test]
    fn every_sqltables_column_is_nullable() {
        let s = CatalogResultColumnWidths::default();
        for desc in TablesResultCol::all_descriptors(&s) {
            assert_eq!(
                desc.nullable,
                Nullable::SqlNullable,
                "{} must be nullable",
                desc.name
            );
        }
    }
}
