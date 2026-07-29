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
//!
//! # Every row type here is `#[non_exhaustive]`
//!
//! Core owns these column layouts: a spec result set that gains a column is a
//! change to this file, and a backend fills named fields rather than positions.
//! `#[non_exhaustive]` is what makes that a *core-only* change — without it, a
//! new field breaks every struct literal in every driver, which is a major
//! version bump for a column the driver does not even have to populate.
//!
//! A `#[non_exhaustive]` struct has no struct expression outside its own crate
//! — not even `..Default::default()`, which Rust rejects with `E0639` — so
//! every row type carries a consuming setter per column, generated from the
//! same field list by the `catalog_rows!` macro below. A backend names the
//! columns it has and says nothing about the rest:
//!
//! ```
//! use stackable_odbc_core::types::TableRow;
//!
//! let row = TableRow::default()
//!     .catalog("hive".to_string())
//!     .name("orders".to_string())
//!     .table_type("TABLE".to_string());
//! ```
//!
//! Each setter takes `impl Into<T>`, so an `Option<String>` column accepts a
//! bare `String` and a `String` column accepts a `&str`. Adding a column later
//! adds a setter, which no driver has to react to.
//!
//! Deliberately not a positional constructor. `ColumnRow` has eighteen columns
//! and `ProcedureColumnRow` nineteen; a `new(...)` taking them in order would
//! reintroduce exactly the argument-order mistake that named fields exist to
//! make unrepresentable. This doc example is a doctest, so it is compiled as a
//! separate crate — which is what makes it proof that the idiom works from
//! *outside* core rather than only inside it.

use crate::types::ColumnValue;

/// Defines the catalog row types.
///
/// One field list per type produces three things that must not drift apart:
/// the struct, its `#[non_exhaustive]` marker, and a consuming setter per
/// field. A column added to a spec result set is therefore one edit here, and
/// the new setter it generates is an additive change no driver has to react to.
///
/// The setters are named after their fields rather than `with_*` because
/// `macro_rules!` cannot build an identifier from parts, and a `with_` list
/// written out beside the fields would be a second place for the same names to
/// live. Each takes `impl Into<T>`, so an `Option<String>` column accepts a
/// bare `String` and a `String` column accepts a `&str`.
macro_rules! catalog_rows {
    ($(
        $(#[$struct_doc:meta])*
        pub struct $name:ident {
            $(
                $(#[$field_doc:meta])*
                pub $field:ident: $ty:ty,
            )*
        }
    )*) => {$(
        $(#[$struct_doc])*
        ///
        /// Construct from [`Default`] and the setters below; this type is
        /// `#[non_exhaustive]`, so a struct expression is not available outside
        /// this crate.
        #[derive(Debug, Clone, Default, PartialEq)]
        #[non_exhaustive]
        pub struct $name {
            $(
                $(#[$field_doc])*
                pub $field: $ty,
            )*
        }

        impl $name {
            $(
                #[doc = concat!("Set [`", stringify!($name), "::", stringify!($field), "`].")]
                #[must_use]
                pub fn $field(mut self, value: impl Into<$ty>) -> Self {
                    self.$field = value.into();
                    self
                }
            )*
        }
    )*};
}

catalog_rows! {
    /// One row of `SQLTables`.
    ///
    /// Every column is nullable: the spec marks none "not NULL", because the
    /// `SQL_ALL_*` enumerations NULL out all but the enumerated one.
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

    /// One row of `SQLProcedures`. Spec column order; 8 columns.
    ///
    /// `PROCEDURE_NAME` (3) is the only column the spec marks "not NULL".
    ///
    /// Columns 4-6 are listed with data type "N/A" — "reserved for future use" —
    /// but core reports them as `SMALLINT`, the ODBC 2.0 layout that applications
    /// binding by column number expect, so they are modelled as `Option<i16>`
    /// rather than dropped.
    pub struct ProcedureRow {
        /// `PROCEDURE_CAT` (1).
        pub catalog: Option<String>,
        /// `PROCEDURE_SCHEM` (2).
        pub schema: Option<String>,
        /// `PROCEDURE_NAME` (3), not NULL.
        pub name: String,
        /// `NUM_INPUT_PARAMS` (4), reserved for future use.
        pub num_input_params: Option<i16>,
        /// `NUM_OUTPUT_PARAMS` (5), reserved for future use.
        pub num_output_params: Option<i16>,
        /// `NUM_RESULT_SETS` (6), reserved for future use.
        pub num_result_sets: Option<i16>,
        /// `REMARKS` (7).
        pub remarks: Option<String>,
        /// `PROCEDURE_TYPE` (8) — one of the
        /// [`SQL_PT_*`](crate::types::SQL_PT_PROCEDURE) values.
        pub procedure_type: Option<i16>,
    }

    /// One row of `SQLProcedureColumns`. Spec column order; 19 columns.
    ///
    /// The non-`Option` fields are the eight the spec marks "not NULL":
    /// `PROCEDURE_NAME`, `COLUMN_NAME`, `COLUMN_TYPE`, `DATA_TYPE`, `TYPE_NAME`,
    /// `NULLABLE`, `SQL_DATA_TYPE` and `ORDINAL_POSITION`.
    pub struct ProcedureColumnRow {
        /// `PROCEDURE_CAT` (1).
        pub catalog: Option<String>,
        /// `PROCEDURE_SCHEM` (2).
        pub schema: Option<String>,
        /// `PROCEDURE_NAME` (3), not NULL.
        pub procedure_name: String,
        /// `COLUMN_NAME` (4), not NULL.
        pub column_name: String,
        /// `COLUMN_TYPE` (5), not NULL — an [`odbc_sys::ParamType`] discriminant,
        /// whose values are exactly the spec's `SQL_PARAM_*` / `SQL_RESULT_COL` /
        /// `SQL_RETURN_VALUE` set, so spell it `ParamType::Input as i16`.
        pub column_type: i16,
        /// `DATA_TYPE` (6), not NULL.
        pub data_type: i16,
        /// `TYPE_NAME` (7), not NULL.
        pub type_name: String,
        /// `COLUMN_SIZE` (8).
        pub column_size: Option<i32>,
        /// `BUFFER_LENGTH` (9).
        pub buffer_length: Option<i32>,
        /// `DECIMAL_DIGITS` (10).
        pub decimal_digits: Option<i16>,
        /// `NUM_PREC_RADIX` (11).
        pub num_prec_radix: Option<i16>,
        /// `NULLABLE` (12), not NULL.
        pub nullable: i16,
        /// `REMARKS` (13).
        pub remarks: Option<String>,
        /// `COLUMN_DEF` (14).
        pub column_def: Option<String>,
        /// `SQL_DATA_TYPE` (15), not NULL.
        pub sql_data_type: i16,
        /// `SQL_DATETIME_SUB` (16).
        pub sql_datetime_sub: Option<i16>,
        /// `CHAR_OCTET_LENGTH` (17).
        pub char_octet_length: Option<i32>,
        /// `ORDINAL_POSITION` (18), not NULL.
        pub ordinal_position: i32,
        /// `IS_NULLABLE` (19).
        pub is_nullable: Option<String>,
    }

    /// One row of `SQLColumnPrivileges`. Spec column order; 8 columns.
    ///
    /// The non-`Option` fields are the four the spec marks "not NULL":
    /// `TABLE_NAME`, `COLUMN_NAME`, `GRANTEE` and `PRIVILEGE`.
    pub struct ColumnPrivilegeRow {
        /// `TABLE_CAT` (1).
        pub catalog: Option<String>,
        /// `TABLE_SCHEM` (2).
        pub schema: Option<String>,
        /// `TABLE_NAME` (3), not NULL.
        pub table_name: String,
        /// `COLUMN_NAME` (4), not NULL.
        pub column_name: String,
        /// `GRANTOR` (5).
        pub grantor: Option<String>,
        /// `GRANTEE` (6), not NULL.
        pub grantee: String,
        /// `PRIVILEGE` (7), not NULL.
        pub privilege: String,
        /// `IS_GRANTABLE` (8).
        pub is_grantable: Option<String>,
    }

    /// One row of `SQLTablePrivileges`. Spec column order; 7 columns.
    ///
    /// The non-`Option` fields are the three the spec marks "not NULL":
    /// `TABLE_NAME`, `GRANTEE` and `PRIVILEGE`.
    pub struct TablePrivilegeRow {
        /// `TABLE_CAT` (1).
        pub catalog: Option<String>,
        /// `TABLE_SCHEM` (2).
        pub schema: Option<String>,
        /// `TABLE_NAME` (3), not NULL.
        pub table_name: String,
        /// `GRANTOR` (4).
        pub grantor: Option<String>,
        /// `GRANTEE` (5), not NULL.
        pub grantee: String,
        /// `PRIVILEGE` (6), not NULL.
        pub privilege: String,
        /// `IS_GRANTABLE` (7).
        pub is_grantable: Option<String>,
    }
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

impl ProcedureRow {
    /// Values in spec column order; 8 columns, matching the field order.
    pub fn to_values(&self) -> Vec<ColumnValue> {
        vec![
            opt_str(&self.catalog),
            opt_str(&self.schema),
            ColumnValue::String(self.name.clone()),
            opt_i16(self.num_input_params),
            opt_i16(self.num_output_params),
            opt_i16(self.num_result_sets),
            opt_str(&self.remarks),
            opt_i16(self.procedure_type),
        ]
    }
}

impl ProcedureColumnRow {
    /// Values in spec column order; 19 columns, matching the field order.
    pub fn to_values(&self) -> Vec<ColumnValue> {
        vec![
            opt_str(&self.catalog),
            opt_str(&self.schema),
            ColumnValue::String(self.procedure_name.clone()),
            ColumnValue::String(self.column_name.clone()),
            ColumnValue::I16(self.column_type),
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

impl ColumnPrivilegeRow {
    /// Values in spec column order; 8 columns, matching the field order.
    pub fn to_values(&self) -> Vec<ColumnValue> {
        vec![
            opt_str(&self.catalog),
            opt_str(&self.schema),
            ColumnValue::String(self.table_name.clone()),
            ColumnValue::String(self.column_name.clone()),
            opt_str(&self.grantor),
            ColumnValue::String(self.grantee.clone()),
            ColumnValue::String(self.privilege.clone()),
            opt_str(&self.is_grantable),
        ]
    }
}

impl TablePrivilegeRow {
    /// Values in spec column order; 7 columns, matching the field order.
    pub fn to_values(&self) -> Vec<ColumnValue> {
        vec![
            opt_str(&self.catalog),
            opt_str(&self.schema),
            ColumnValue::String(self.table_name.clone()),
            opt_str(&self.grantor),
            ColumnValue::String(self.grantee.clone()),
            ColumnValue::String(self.privilege.clone()),
            opt_str(&self.is_grantable),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffi::metadata::{
        column_privileges_columns, procedure_columns_columns, procedures_columns,
        table_privileges_columns,
    };
    use crate::types::{
        CatalogResultColumnWidths, ColumnsResultCol, ForeignKeysResultCol, PrimaryKeysResultCol,
        SQL_PT_PROCEDURE, TablesResultCol, special_columns_columns, statistics_columns,
    };

    /// The generated setters assign the field they are named after, and their
    /// `impl Into<T>` bound covers the three field shapes these rows use: a
    /// nullable string column takes a bare `String`, a not-NULL string column
    /// takes a `&str`, and a nullable numeric column takes the bare number.
    ///
    /// A driver has no other way to build these types — `#[non_exhaustive]`
    /// rules out a struct expression outside this crate — so a setter that
    /// wrote the wrong field would be unreachable by any in-crate literal test.
    #[test]
    fn the_generated_setters_assign_their_own_field() {
        assert_eq!(
            TableRow::default()
                .catalog("hive".to_string())
                .name("orders".to_string()),
            TableRow {
                catalog: Some("hive".into()),
                name: Some("orders".into()),
                ..Default::default()
            }
        );

        assert_eq!(
            ColumnRow::default()
                .table_name("orders")
                .ordinal_position(1)
                .column_size(38),
            ColumnRow {
                table_name: "orders".into(),
                ordinal_position: 1,
                column_size: Some(38),
                ..Default::default()
            }
        );
    }

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
        // The last four descriptor functions live beside their FFI entry
        // points rather than in `types`, so they are imported here rather than
        // restated — a second statement of a column layout is a second way to
        // state it differently.
        assert_eq!(
            ProcedureRow::default().to_values().len(),
            procedures_columns(&widths).len()
        );
        assert_eq!(
            ProcedureColumnRow::default().to_values().len(),
            procedure_columns_columns(&widths).len()
        );
        assert_eq!(
            ColumnPrivilegeRow::default().to_values().len(),
            column_privileges_columns(&widths).len()
        );
        assert_eq!(
            TablePrivilegeRow::default().to_values().len(),
            table_privileges_columns(&widths).len()
        );
    }

    #[test]
    fn table_privilege_row_converts_in_spec_column_order() {
        // Spec, SQLTablePrivileges result columns: 1 TABLE_CAT,
        // 2 TABLE_SCHEM, 3 TABLE_NAME, 4 GRANTOR, 5 GRANTEE, 6 PRIVILEGE,
        // 7 IS_GRANTABLE.
        let row = TablePrivilegeRow {
            catalog: Some("cat".into()),
            schema: None,
            table_name: "t".into(),
            grantor: None,
            grantee: "u".into(),
            privilege: "SELECT".into(),
            is_grantable: Some("YES".into()),
        };
        assert_eq!(
            row.to_values(),
            vec![
                ColumnValue::String("cat".into()),
                ColumnValue::Null,
                ColumnValue::String("t".into()),
                ColumnValue::Null,
                ColumnValue::String("u".into()),
                ColumnValue::String("SELECT".into()),
                ColumnValue::String("YES".into()),
            ]
        );
    }

    #[test]
    fn procedure_row_converts_in_spec_column_order() {
        // Spec, SQLProcedures result columns: 1 PROCEDURE_CAT,
        // 2 PROCEDURE_SCHEM, 3 PROCEDURE_NAME, 4 NUM_INPUT_PARAMS,
        // 5 NUM_OUTPUT_PARAMS, 6 NUM_RESULT_SETS, 7 REMARKS,
        // 8 PROCEDURE_TYPE. Columns 4-6 are "reserved for future use", so a
        // conversion that dropped them would still produce plausible-looking
        // values in 7 and 8 — this pins their positions.
        let row = ProcedureRow {
            catalog: Some("cat".into()),
            schema: None,
            name: "p".into(),
            num_input_params: None,
            num_output_params: None,
            num_result_sets: None,
            remarks: Some("note".into()),
            procedure_type: Some(SQL_PT_PROCEDURE),
        };
        assert_eq!(
            row.to_values(),
            vec![
                ColumnValue::String("cat".into()),
                ColumnValue::Null,
                ColumnValue::String("p".into()),
                ColumnValue::Null,
                ColumnValue::Null,
                ColumnValue::Null,
                ColumnValue::String("note".into()),
                ColumnValue::I16(SQL_PT_PROCEDURE),
            ]
        );
    }
}
