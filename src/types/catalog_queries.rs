//! The ten typed argument objects the catalog [`Backend`] hooks take.
//!
//! # Why these are types rather than argument lists
//!
//! Each hook took five to eight positional arguments, and most of them were a
//! run of `Option<&str>`: `SQLForeignKeys` alone took six in a row, where
//! swapping a primary-key argument for its foreign-key counterpart compiled
//! silently and failed at runtime as a wrong or empty result set. This is the
//! argument [`crate::types::catalog_rows`] already makes for the *return* side,
//! applied to the inputs.
//!
//! # Every query type here is sealed
//!
//! Fields are crate-private, the type is `#[non_exhaustive]`, and every field
//! has an accessor and a setter. Together those make an argument added to a
//! catalog hook a source-compatible change for every driver: there is no
//! argument list to go stale, no struct expression to update, and no field to
//! assign around the setters.
//!
//! [`Backend`]: crate::backend::Backend

use crate::types::{IdentifierType, Nullable, Scope};

/// Generates a sealed catalog query type from a field table.
///
/// Each field is written `accessor / setter: Type`. The two names are spelled
/// out rather than derived because `macro_rules!` cannot build an identifier
/// from parts and this crate takes no proc-macro dependency; deriving `with_`
/// from the field name would need `paste`.
///
/// The first arm handles a type with required fields, which take no default and
/// so become `new()` arguments rather than setters. It must come first: the
/// second arm would match `required` as a field name and then fail on the `{`.
macro_rules! catalog_queries {
    // --- types with required fields: no `Default`, a `new()` instead ---
    ($(
        $(#[$struct_doc:meta])*
        pub struct $name:ident<'a> {
            required {
                $(
                    $(#[$req_doc:meta])*
                    $req:ident: $req_ty:ty,
                )+
            }
            $(
                $(#[$field_doc:meta])*
                $field:ident / $setter:ident: $ty:ty,
            )*
        }
    )*) => {$(
        $(#[$struct_doc])*
        ///
        /// Construct with `new` and the `with_*` setters. This type is
        /// `#[non_exhaustive]` with crate-private fields, so neither a struct
        /// expression nor a field assignment is available outside this crate.
        #[derive(Debug, Clone, Copy, PartialEq)]
        #[non_exhaustive]
        pub struct $name<'a> {
            $( pub(crate) $req: $req_ty, )+
            $( pub(crate) $field: $ty, )*
        }

        impl<'a> $name<'a> {
            /// A query carrying the arguments that have no default, filtering
            /// on nothing else until a setter says otherwise.
            #[must_use]
            pub fn new($( $req: $req_ty, )+) -> Self {
                Self {
                    $( $req, )+
                    $( $field: Default::default(), )*
                }
            }

            $(
                $(#[$req_doc])*
                #[must_use]
                pub fn $req(&self) -> $req_ty {
                    self.$req
                }
            )+

            $(
                $(#[$field_doc])*
                #[must_use]
                pub fn $field(&self) -> $ty {
                    self.$field
                }

                #[doc = concat!("Sets [`", stringify!($name), "::", stringify!($field), "`].")]
                #[must_use]
                pub fn $setter(mut self, value: impl Into<$ty>) -> Self {
                    self.$field = value.into();
                    self
                }
            )*
        }
    )*};

    // --- types whose every field is optional: `Default` plus setters ---
    ($(
        $(#[$struct_doc:meta])*
        pub struct $name:ident<'a> {
            $(
                $(#[$field_doc:meta])*
                $field:ident / $setter:ident: $ty:ty,
            )*
        }
    )*) => {$(
        $(#[$struct_doc])*
        ///
        /// Construct from [`Default`] and the `with_*` setters. This type is
        /// `#[non_exhaustive]` with crate-private fields, so neither a struct
        /// expression nor a field assignment is available outside this crate.
        #[derive(Debug, Clone, Copy, Default, PartialEq)]
        #[non_exhaustive]
        pub struct $name<'a> {
            $( pub(crate) $field: $ty, )*
        }

        impl<'a> $name<'a> {
            $(
                $(#[$field_doc])*
                #[must_use]
                pub fn $field(&self) -> $ty {
                    self.$field
                }

                #[doc = concat!("Sets [`", stringify!($name), "::", stringify!($field), "`].")]
                #[must_use]
                pub fn $setter(mut self, value: impl Into<$ty>) -> Self {
                    self.$field = value.into();
                    self
                }
            )*
        }
    )*};
}

catalog_queries! {
    /// The arguments of [`Backend::tables`](crate::backend::Backend::tables).
    pub struct TablesQuery<'a> {
        /// `SQLTables`' *CatalogName*.
        catalog / with_catalog: Option<&'a str>,
        /// `SQLTables`' *SchemaName*.
        schema / with_schema: Option<&'a str>,
        /// `SQLTables`' *TableName*.
        table / with_table: Option<&'a str>,
        /// `SQLTables`' *TableType*, parsed by core into a value list. Empty
        /// means no filter. This is the one catalog argument
        /// `SQL_ATTR_METADATA_ID` never applies to, so core parses it rather
        /// than every driver.
        table_types / with_table_types: &'a [String],
    }

    /// The arguments of [`Backend::columns`](crate::backend::Backend::columns).
    pub struct ColumnsQuery<'a> {
        /// `SQLColumns`' *CatalogName*.
        catalog / with_catalog: Option<&'a str>,
        /// `SQLColumns`' *SchemaName*.
        schema / with_schema: Option<&'a str>,
        /// `SQLColumns`' *TableName*.
        table / with_table: Option<&'a str>,
        /// `SQLColumns`' *ColumnName*.
        column / with_column: Option<&'a str>,
    }

    /// The arguments of [`Backend::primary_keys`](crate::backend::Backend::primary_keys).
    pub struct PrimaryKeysQuery<'a> {
        /// `SQLPrimaryKeys`' *CatalogName*.
        catalog / with_catalog: Option<&'a str>,
        /// `SQLPrimaryKeys`' *SchemaName*.
        schema / with_schema: Option<&'a str>,
        /// `SQLPrimaryKeys`' *TableName*.
        table / with_table: Option<&'a str>,
    }

    /// The arguments of [`Backend::foreign_keys`](crate::backend::Backend::foreign_keys).
    ///
    /// Both sides are named, which is the whole point: as six positional
    /// `Option<&str>` arguments, a crossed primary-key and foreign-key pair
    /// compiled without complaint.
    pub struct ForeignKeysQuery<'a> {
        /// `SQLForeignKeys`' *PKCatalogName*.
        pk_catalog / with_pk_catalog: Option<&'a str>,
        /// `SQLForeignKeys`' *PKSchemaName*.
        pk_schema / with_pk_schema: Option<&'a str>,
        /// `SQLForeignKeys`' *PKTableName*.
        pk_table / with_pk_table: Option<&'a str>,
        /// `SQLForeignKeys`' *FKCatalogName*.
        fk_catalog / with_fk_catalog: Option<&'a str>,
        /// `SQLForeignKeys`' *FKSchemaName*.
        fk_schema / with_fk_schema: Option<&'a str>,
        /// `SQLForeignKeys`' *FKTableName*.
        fk_table / with_fk_table: Option<&'a str>,
    }

    /// The arguments of [`Backend::procedures`](crate::backend::Backend::procedures).
    pub struct ProceduresQuery<'a> {
        /// `SQLProcedures`' *CatalogName*.
        catalog / with_catalog: Option<&'a str>,
        /// `SQLProcedures`' *SchemaName*.
        schema / with_schema: Option<&'a str>,
        /// `SQLProcedures`' *ProcName*. Named for the procedure rather than
        /// reusing `table`, so a driver cannot read the wrong filter.
        proc_name / with_proc_name: Option<&'a str>,
    }

    /// The arguments of [`Backend::procedure_columns`](crate::backend::Backend::procedure_columns).
    pub struct ProcedureColumnsQuery<'a> {
        /// `SQLProcedureColumns`' *CatalogName*.
        catalog / with_catalog: Option<&'a str>,
        /// `SQLProcedureColumns`' *SchemaName*.
        schema / with_schema: Option<&'a str>,
        /// `SQLProcedureColumns`' *ProcName*.
        proc_name / with_proc_name: Option<&'a str>,
        /// `SQLProcedureColumns`' *ColumnName*.
        column / with_column: Option<&'a str>,
    }

    /// The arguments of [`Backend::column_privileges`](crate::backend::Backend::column_privileges).
    pub struct ColumnPrivilegesQuery<'a> {
        /// `SQLColumnPrivileges`' *CatalogName*.
        catalog / with_catalog: Option<&'a str>,
        /// `SQLColumnPrivileges`' *SchemaName*.
        schema / with_schema: Option<&'a str>,
        /// `SQLColumnPrivileges`' *TableName*.
        table / with_table: Option<&'a str>,
        /// `SQLColumnPrivileges`' *ColumnName*.
        column / with_column: Option<&'a str>,
    }

    /// The arguments of [`Backend::table_privileges`](crate::backend::Backend::table_privileges).
    pub struct TablePrivilegesQuery<'a> {
        /// `SQLTablePrivileges`' *CatalogName*.
        catalog / with_catalog: Option<&'a str>,
        /// `SQLTablePrivileges`' *SchemaName*.
        schema / with_schema: Option<&'a str>,
        /// `SQLTablePrivileges`' *TableName*.
        table / with_table: Option<&'a str>,
    }
}

catalog_queries! {
    /// The arguments of [`Backend::statistics`](crate::backend::Backend::statistics).
    pub struct StatisticsQuery<'a> {
        required {
            /// `SQLStatistics`' *Unique*: `true` for `SQL_INDEX_UNIQUE`,
            /// `false` for `SQL_INDEX_ALL`.
            ///
            /// A constructor argument rather than a setter because `false` is
            /// a claim about what the application asked for, not an absence.
            unique_only: bool,
        }
        /// `SQLStatistics`' *CatalogName*.
        catalog / with_catalog: Option<&'a str>,
        /// `SQLStatistics`' *SchemaName*.
        schema / with_schema: Option<&'a str>,
        /// `SQLStatistics`' *TableName*.
        table / with_table: Option<&'a str>,
    }

    /// The arguments of [`Backend::special_columns`](crate::backend::Backend::special_columns).
    pub struct SpecialColumnsQuery<'a> {
        required {
            /// `SQLSpecialColumns`' *IdentifierType*.
            ///
            /// A constructor argument rather than a setter: no value of this
            /// enum is a defensible default, and core inventing one would be a
            /// claim the caller never made.
            identifier_type: IdentifierType,
            /// `SQLSpecialColumns`' *Scope*. A constructor argument for the
            /// same reason as `identifier_type`.
            scope: Scope,
            /// `SQLSpecialColumns`' *Nullable*. A constructor argument for the
            /// same reason as `identifier_type`.
            nullable: Nullable,
        }
        /// `SQLSpecialColumns`' *CatalogName*.
        catalog / with_catalog: Option<&'a str>,
        /// `SQLSpecialColumns`' *SchemaName*.
        schema / with_schema: Option<&'a str>,
        /// `SQLSpecialColumns`' *TableName*.
        table / with_table: Option<&'a str>,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every setter writes the field its accessor reads. The macro generates
    /// the two halves from one field table, so a mismatch here means the macro
    /// is wrong for all ten types, not just this one.
    #[test]
    fn a_value_set_on_tables_query_reads_back_through_its_accessor() {
        let types = vec![String::from("TABLE"), String::from("VIEW")];
        let query = TablesQuery::default()
            .with_catalog("cat")
            .with_schema("sch")
            .with_table("tbl")
            .with_table_types(types.as_slice());

        assert_eq!(query.catalog(), Some("cat"));
        assert_eq!(query.schema(), Some("sch"));
        assert_eq!(query.table(), Some("tbl"));
        assert_eq!(query.table_types(), types.as_slice());
    }

    /// A default-constructed query filters on nothing, which is what core
    /// passes when the application supplied a null pointer for every argument.
    #[test]
    fn a_default_tables_query_filters_on_nothing() {
        let query = TablesQuery::default();

        assert_eq!(query.catalog(), None);
        assert_eq!(query.schema(), None);
        assert_eq!(query.table(), None);
        assert!(query.table_types().is_empty());
    }

    /// The six `Option<&str>` arguments of `SQLForeignKeys` are the
    /// transposition hazard this whole type exists to remove: as a positional
    /// list, swapping a pk argument for its fk counterpart compiled silently.
    /// Each of the six gets a distinguishable value so a crossed wire in the
    /// macro's field table cannot pass.
    #[test]
    fn the_two_sides_of_a_foreign_keys_query_do_not_cross() {
        let query = ForeignKeysQuery::default()
            .with_pk_catalog("pkc")
            .with_pk_schema("pks")
            .with_pk_table("pkt")
            .with_fk_catalog("fkc")
            .with_fk_schema("fks")
            .with_fk_table("fkt");

        assert_eq!(query.pk_catalog(), Some("pkc"));
        assert_eq!(query.pk_schema(), Some("pks"));
        assert_eq!(query.pk_table(), Some("pkt"));
        assert_eq!(query.fk_catalog(), Some("fkc"));
        assert_eq!(query.fk_schema(), Some("fks"));
        assert_eq!(query.fk_table(), Some("fkt"));
    }

    /// `unique_only` has no honest default: `false` means `SQL_INDEX_ALL`,
    /// which is a claim about what the application asked for rather than an
    /// absence, so it is a constructor argument and not a setter.
    #[test]
    fn a_statistics_query_carries_its_required_unique_only_flag() {
        let query = StatisticsQuery::new(true).with_table("tbl");

        assert!(query.unique_only());
        assert_eq!(query.table(), Some("tbl"));
        assert_eq!(query.catalog(), None);
    }

    /// `Scope` and `IdentifierType` have no defensible default, so inventing
    /// one for them would be the "core guessing" antipattern. All three are
    /// constructor arguments.
    #[test]
    fn a_special_columns_query_carries_its_three_required_arguments() {
        let query = SpecialColumnsQuery::new(
            IdentifierType::BestRowId,
            Scope::CurRow,
            Nullable::SqlNullable,
        )
        .with_schema("sch");

        assert_eq!(query.identifier_type(), IdentifierType::BestRowId);
        assert_eq!(query.scope(), Scope::CurRow);
        assert_eq!(query.nullable(), Nullable::SqlNullable);
        assert_eq!(query.schema(), Some("sch"));
    }

    /// `SQLProcedures` filters on a procedure name, not a table name. Keeping
    /// the field named `proc_name` stops a driver reading the wrong one, the
    /// same reason `RecordedCatalogArgs` keeps `proc` apart from `table`.
    #[test]
    fn a_procedures_query_names_its_filter_proc_name() {
        let query = ProceduresQuery::default().with_proc_name("p");

        assert_eq!(query.proc_name(), Some("p"));
        assert_eq!(query.catalog(), None);
    }

    /// The five types the tests above do not reach. Each is generated from the
    /// same field table by the same macro arm, so this covers the field lists
    /// rather than the machinery: a name typed wrong in one table would not
    /// show up anywhere else.
    #[test]
    fn the_remaining_query_types_round_trip_every_field() {
        let columns = ColumnsQuery::default()
            .with_catalog("c")
            .with_schema("s")
            .with_table("t")
            .with_column("col");
        assert_eq!(columns.catalog(), Some("c"));
        assert_eq!(columns.schema(), Some("s"));
        assert_eq!(columns.table(), Some("t"));
        assert_eq!(columns.column(), Some("col"));

        let primary_keys = PrimaryKeysQuery::default()
            .with_catalog("c")
            .with_schema("s")
            .with_table("t");
        assert_eq!(primary_keys.catalog(), Some("c"));
        assert_eq!(primary_keys.schema(), Some("s"));
        assert_eq!(primary_keys.table(), Some("t"));

        let procedure_columns = ProcedureColumnsQuery::default()
            .with_catalog("c")
            .with_schema("s")
            .with_proc_name("p")
            .with_column("col");
        assert_eq!(procedure_columns.catalog(), Some("c"));
        assert_eq!(procedure_columns.schema(), Some("s"));
        assert_eq!(procedure_columns.proc_name(), Some("p"));
        assert_eq!(procedure_columns.column(), Some("col"));

        let column_privileges = ColumnPrivilegesQuery::default()
            .with_catalog("c")
            .with_schema("s")
            .with_table("t")
            .with_column("col");
        assert_eq!(column_privileges.catalog(), Some("c"));
        assert_eq!(column_privileges.schema(), Some("s"));
        assert_eq!(column_privileges.table(), Some("t"));
        assert_eq!(column_privileges.column(), Some("col"));

        let table_privileges = TablePrivilegesQuery::default()
            .with_catalog("c")
            .with_schema("s")
            .with_table("t");
        assert_eq!(table_privileges.catalog(), Some("c"));
        assert_eq!(table_privileges.schema(), Some("s"));
        assert_eq!(table_privileges.table(), Some("t"));
    }
}
