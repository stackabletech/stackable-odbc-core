//! Typed rows for the catalog functions.
//!
//! A backend returns these; core converts them to `ColumnValue`s in spec
//! column order, sorts them, and serves them as a `SyntheticStatement`.
//!
//! Named fields rather than `Vec<ColumnValue>` so a backend cannot get the
//! column order or count wrong. `SyntheticStatement::new` only
//! `debug_assert!`s that the row width matches the descriptor count, which a
//! release build does not check — with typed rows the mismatch is
//! unrepresentable instead.

use crate::types::ColumnValue;

/// One row of `SQLTables`.
///
/// Every column is nullable: the spec marks none "not NULL", because the
/// `SQL_ALL_*` enumerations NULL out all but the enumerated one.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TableRow {
    /// `TABLE_CAT` (column 1).
    pub catalog: Option<String>,
    /// `TABLE_SCHEM` (column 2).
    pub schema: Option<String>,
    /// `TABLE_NAME` (column 3).
    pub name: Option<String>,
    /// `TABLE_TYPE` (column 4).
    pub table_type: Option<String>,
    /// `REMARKS` (column 5).
    pub remarks: Option<String>,
}

/// One row of `SQLColumns`. Field order is spec column order; 18 columns.
///
/// The non-`Option` fields are the seven the spec marks "not NULL":
/// `TABLE_NAME`, `COLUMN_NAME`, `DATA_TYPE`, `TYPE_NAME`, `NULLABLE`,
/// `SQL_DATA_TYPE` and `ORDINAL_POSITION`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ColumnRow {
    /// `TABLE_CAT` (1).
    pub catalog: Option<String>,
    /// `TABLE_SCHEM` (2).
    pub schema: Option<String>,
    /// `TABLE_NAME` (3), not NULL.
    pub table_name: String,
    /// `COLUMN_NAME` (4), not NULL.
    pub column_name: String,
    /// `DATA_TYPE` (5), not NULL.
    pub data_type: i16,
    /// `TYPE_NAME` (6), not NULL.
    pub type_name: String,
    /// `COLUMN_SIZE` (7).
    pub column_size: Option<i32>,
    /// `BUFFER_LENGTH` (8).
    pub buffer_length: Option<i32>,
    /// `DECIMAL_DIGITS` (9).
    pub decimal_digits: Option<i16>,
    /// `NUM_PREC_RADIX` (10).
    pub num_prec_radix: Option<i16>,
    /// `NULLABLE` (11), not NULL.
    pub nullable: i16,
    /// `REMARKS` (12).
    pub remarks: Option<String>,
    /// `COLUMN_DEF` (13).
    pub column_def: Option<String>,
    /// `SQL_DATA_TYPE` (14), not NULL.
    pub sql_data_type: i16,
    /// `SQL_DATETIME_SUB` (15).
    pub sql_datetime_sub: Option<i16>,
    /// `CHAR_OCTET_LENGTH` (16).
    pub char_octet_length: Option<i32>,
    /// `ORDINAL_POSITION` (17), not NULL.
    pub ordinal_position: i32,
    /// `IS_NULLABLE` (18).
    pub is_nullable: Option<String>,
}

/// One row of `SQLPrimaryKeys`. Spec column order; 6 columns.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PrimaryKeyRow {
    /// `TABLE_CAT` (1).
    pub catalog: Option<String>,
    /// `TABLE_SCHEM` (2).
    pub schema: Option<String>,
    /// `TABLE_NAME` (3), not NULL.
    pub table_name: String,
    /// `COLUMN_NAME` (4), not NULL.
    pub column_name: String,
    /// `KEY_SEQ` (5), not NULL.
    pub key_seq: i16,
    /// `PK_NAME` (6).
    pub pk_name: Option<String>,
}

/// One row of `SQLForeignKeys`. Spec column order; 14 columns.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ForeignKeyRow {
    /// `PKTABLE_CAT` (1).
    pub pk_catalog: Option<String>,
    /// `PKTABLE_SCHEM` (2).
    pub pk_schema: Option<String>,
    /// `PKTABLE_NAME` (3), not NULL.
    pub pk_table_name: String,
    /// `PKCOLUMN_NAME` (4), not NULL.
    pub pk_column_name: String,
    /// `FKTABLE_CAT` (5).
    pub fk_catalog: Option<String>,
    /// `FKTABLE_SCHEM` (6).
    pub fk_schema: Option<String>,
    /// `FKTABLE_NAME` (7), not NULL.
    pub fk_table_name: String,
    /// `FKCOLUMN_NAME` (8), not NULL.
    pub fk_column_name: String,
    /// `KEY_SEQ` (9), not NULL.
    pub key_seq: i16,
    /// `UPDATE_RULE` (10).
    pub update_rule: Option<i16>,
    /// `DELETE_RULE` (11).
    pub delete_rule: Option<i16>,
    /// `FK_NAME` (12).
    pub fk_name: Option<String>,
    /// `PK_NAME` (13).
    pub pk_name: Option<String>,
    /// `DEFERRABILITY` (14).
    pub deferrability: Option<i16>,
}

/// One row of `SQLStatistics`. Spec column order; 13 columns.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StatisticsRow {
    /// `TABLE_CAT` (1).
    pub catalog: Option<String>,
    /// `TABLE_SCHEM` (2).
    pub schema: Option<String>,
    /// `TABLE_NAME` (3), not NULL.
    pub table_name: String,
    /// `NON_UNIQUE` (4).
    pub non_unique: Option<i16>,
    /// `INDEX_QUALIFIER` (5).
    pub index_qualifier: Option<String>,
    /// `INDEX_NAME` (6).
    pub index_name: Option<String>,
    /// `TYPE` (7), not NULL. Named `index_type` because `type` is a keyword.
    pub index_type: i16,
    /// `ORDINAL_POSITION` (8).
    pub ordinal_position: Option<i16>,
    /// `COLUMN_NAME` (9).
    pub column_name: Option<String>,
    /// `ASC_OR_DESC` (10) — the one catalog column the spec declares
    /// `char(1)` rather than `varchar`.
    pub asc_or_desc: Option<String>,
    /// `CARDINALITY` (11).
    pub cardinality: Option<i32>,
    /// `PAGES` (12).
    pub pages: Option<i32>,
    /// `FILTER_CONDITION` (13).
    pub filter_condition: Option<String>,
}

/// One row of `SQLSpecialColumns`. Spec column order; 8 columns.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SpecialColumnRow {
    /// `SCOPE` (1). Nullable: NULL when `IdentifierType` is `SQL_ROWVER`.
    pub scope: Option<i16>,
    /// `COLUMN_NAME` (2), not NULL.
    pub column_name: String,
    /// `DATA_TYPE` (3), not NULL.
    pub data_type: i16,
    /// `TYPE_NAME` (4), not NULL.
    pub type_name: String,
    /// `COLUMN_SIZE` (5).
    pub column_size: Option<i32>,
    /// `BUFFER_LENGTH` (6).
    pub buffer_length: Option<i32>,
    /// `DECIMAL_DIGITS` (7).
    pub decimal_digits: Option<i16>,
    /// `PSEUDO_COLUMN` (8).
    pub pseudo_column: Option<i16>,
}

fn opt_str(v: &Option<String>) -> ColumnValue {
    match v {
        Some(s) => ColumnValue::String(s.clone()),
        None => ColumnValue::Null,
    }
}

fn opt_i16(v: Option<i16>) -> ColumnValue {
    match v {
        Some(n) => ColumnValue::I16(n),
        None => ColumnValue::Null,
    }
}

fn opt_i32(v: Option<i32>) -> ColumnValue {
    match v {
        Some(n) => ColumnValue::I32(n),
        None => ColumnValue::Null,
    }
}

impl TableRow {
    /// Values in spec column order: `TABLE_CAT`, `TABLE_SCHEM`, `TABLE_NAME`,
    /// `TABLE_TYPE`, `REMARKS`.
    pub fn to_values(&self) -> Vec<ColumnValue> {
        vec![
            opt_str(&self.catalog),
            opt_str(&self.schema),
            opt_str(&self.name),
            opt_str(&self.table_type),
            opt_str(&self.remarks),
        ]
    }
}

impl ColumnRow {
    /// Values in spec column order; 18 columns, matching the field order.
    pub fn to_values(&self) -> Vec<ColumnValue> {
        vec![
            opt_str(&self.catalog),
            opt_str(&self.schema),
            ColumnValue::String(self.table_name.clone()),
            ColumnValue::String(self.column_name.clone()),
            ColumnValue::I16(self.data_type),
            ColumnValue::String(self.type_name.clone()),
            opt_i32(self.column_size),
            opt_i32(self.buffer_length),
            opt_i16(self.decimal_digits),
            opt_i16(self.num_prec_radix),
            ColumnValue::I16(self.nullable),
            opt_str(&self.remarks),
            opt_str(&self.column_def),
            ColumnValue::I16(self.sql_data_type),
            opt_i16(self.sql_datetime_sub),
            opt_i32(self.char_octet_length),
            ColumnValue::I32(self.ordinal_position),
            opt_str(&self.is_nullable),
        ]
    }
}

impl PrimaryKeyRow {
    /// Values in spec column order; 6 columns, matching the field order.
    pub fn to_values(&self) -> Vec<ColumnValue> {
        vec![
            opt_str(&self.catalog),
            opt_str(&self.schema),
            ColumnValue::String(self.table_name.clone()),
            ColumnValue::String(self.column_name.clone()),
            ColumnValue::I16(self.key_seq),
            opt_str(&self.pk_name),
        ]
    }
}

impl ForeignKeyRow {
    /// Values in spec column order; 14 columns, matching the field order.
    pub fn to_values(&self) -> Vec<ColumnValue> {
        vec![
            opt_str(&self.pk_catalog),
            opt_str(&self.pk_schema),
            ColumnValue::String(self.pk_table_name.clone()),
            ColumnValue::String(self.pk_column_name.clone()),
            opt_str(&self.fk_catalog),
            opt_str(&self.fk_schema),
            ColumnValue::String(self.fk_table_name.clone()),
            ColumnValue::String(self.fk_column_name.clone()),
            ColumnValue::I16(self.key_seq),
            opt_i16(self.update_rule),
            opt_i16(self.delete_rule),
            opt_str(&self.fk_name),
            opt_str(&self.pk_name),
            opt_i16(self.deferrability),
        ]
    }
}

impl StatisticsRow {
    /// Values in spec column order; 13 columns, matching the field order.
    pub fn to_values(&self) -> Vec<ColumnValue> {
        vec![
            opt_str(&self.catalog),
            opt_str(&self.schema),
            ColumnValue::String(self.table_name.clone()),
            opt_i16(self.non_unique),
            opt_str(&self.index_qualifier),
            opt_str(&self.index_name),
            ColumnValue::I16(self.index_type),
            opt_i16(self.ordinal_position),
            opt_str(&self.column_name),
            opt_str(&self.asc_or_desc),
            opt_i32(self.cardinality),
            opt_i32(self.pages),
            opt_str(&self.filter_condition),
        ]
    }
}

impl SpecialColumnRow {
    /// Values in spec column order; 8 columns, matching the field order.
    pub fn to_values(&self) -> Vec<ColumnValue> {
        vec![
            opt_i16(self.scope),
            ColumnValue::String(self.column_name.clone()),
            ColumnValue::I16(self.data_type),
            ColumnValue::String(self.type_name.clone()),
            opt_i32(self.column_size),
            opt_i32(self.buffer_length),
            opt_i16(self.decimal_digits),
            opt_i16(self.pseudo_column),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        CatalogResultColumnWidths, ColumnsResultCol, ForeignKeysResultCol, PrimaryKeysResultCol,
        TablesResultCol, special_columns_columns, statistics_columns,
    };

    #[test]
    fn table_row_converts_in_spec_column_order() {
        // Spec, SQLTables result columns: 1 TABLE_CAT, 2 TABLE_SCHEM,
        // 3 TABLE_NAME, 4 TABLE_TYPE, 5 REMARKS.
        let row = TableRow {
            catalog: Some("cat".into()),
            schema: Some("sch".into()),
            name: Some("tbl".into()),
            table_type: Some("TABLE".into()),
            remarks: None,
        };
        assert_eq!(
            row.to_values(),
            vec![
                ColumnValue::String("cat".into()),
                ColumnValue::String("sch".into()),
                ColumnValue::String("tbl".into()),
                ColumnValue::String("TABLE".into()),
                ColumnValue::Null,
            ]
        );
    }

    #[test]
    fn every_row_type_produces_its_spec_column_count() {
        // The descriptors and the row values are built by separate code and
        // nothing else pairs them up; `SyntheticStatement::new` only
        // debug_asserts the match, so a release build would ship a mismatch.
        let widths = CatalogResultColumnWidths::default();
        assert_eq!(
            TableRow::default().to_values().len(),
            TablesResultCol::all_descriptors(&widths).len()
        );
        assert_eq!(
            ColumnRow::default().to_values().len(),
            ColumnsResultCol::all_descriptors(&widths).len()
        );
        assert_eq!(
            PrimaryKeyRow::default().to_values().len(),
            PrimaryKeysResultCol::all_descriptors(&widths).len()
        );
        assert_eq!(
            ForeignKeyRow::default().to_values().len(),
            ForeignKeysResultCol::all_descriptors(&widths).len()
        );
        // These two are functions, not enums.
        assert_eq!(
            StatisticsRow::default().to_values().len(),
            statistics_columns(&widths).len()
        );
        assert_eq!(
            SpecialColumnRow::default().to_values().len(),
            special_columns_columns(&widths).len()
        );
    }
}
