//! The `SQLGetInfo` spec's per-`InfoType` return-value shape, transcribed as
//! code.
//!
//! Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlgetinfo-function?view=sql-server-ver17>,
//! "Information Type Descriptions" section, the alphabetical table listing
//! every `InfoType`, the ODBC version it was introduced in, and its
//! description. Every description opens by naming the value's shape: "A
//! character string...", "An SQLUSMALLINT value...", or "An SQLUINTEGER
//! value/bitmask/flag...". This module transcribes exactly that first
//! sentence for every `InfoType` variant `odbc-sys` compiles, nothing more,
//! in the same spirit as [`crate::types::column_size`]'s appendix
//! transcription.
//!
//! # Why this exists
//!
//! `SQLGetInfoW`'s backend-does-not-implement-this-type fallback
//! (`info_type_default_response`, not exported from this module but the caller
//! of [`expected_kind`]) has to answer for *every* unhandled info type. Without
//! the spec's own shape to consult, an answer is accidental rather than
//! declared, and three shapes of mistake follow:
//!
//! - **A numeric range that swallows a type outside it.** `SQL_CONVERT_GUID`
//!   (173) falls outside the "all conversions supported" range, and `U32(0)` is
//!   the value that makes the Windows Driver Manager block `SQLGetData` with
//!   `HYC00`.
//! - **A string-shaped type inside a numeric range.**
//!   `SQL_ODBC_SQL_OPT_IEF`/`SQL_INTEGRITY` (73) is a `Y`/`N` *character
//!   string*, so `U32(0xFFFFFFFF)` puts an integer where an application expects
//!   a string.
//! - **A bitmap read as the wrong bitmap.**
//!   `SQL_NUMERIC_FUNCTIONS`/`SQL_STRING_FUNCTIONS`/`SQL_SYSTEM_FUNCTIONS`/
//!   `SQL_TIMEDATE_FUNCTIONS` (49-52) are scalar-function bitmaps, not
//!   conversion maps, so an "all supported" answer claims scalar functions the
//!   backend may not have.
//!
//! This table is what lets [`expected_kind`] be an **exhaustive** match over
//! `InfoType`, so adding a new `odbc-sys` variant without adding its shape here
//! is a compile error rather than a silently-missing test case.
//!
//! # Deriving the type list, not hand-copying it
//!
//! This module only supplies the *shape* half of the mapping. The *list* of
//! `InfoType` values to check a shape for is not duplicated here as a second
//! hand-maintained list. Every caller derives it by scanning `0..=u16::MAX`
//! through [`crate::types::info_type_from_raw`] and collecting the `Some`
//! results (see `stackable-odbc-core`'s own `conformance` module and each
//! driver's FFI integration tests). A hand-maintained list of "info types
//! someone thought to check" is the failure mode this table exists to close.

use odbc_sys::InfoType;

use crate::types::InfoValue;

/// The shape of the value `SQLGetInfoW` must produce for a given `InfoType`.
/// Mirrors the three variants [`InfoValue`] can take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InfoValueKind {
    /// A null-terminated character string.
    String,
    /// An `SQLUSMALLINT` (`u16`) value.
    U16,
    /// An `SQLUINTEGER` (`u32`) value, bitmask, or flag.
    U32,
}

impl InfoValueKind {
    /// The shape of an actual [`InfoValue`] a `Backend`/the default fallback
    /// produced, what property 1 of the conformance test compares
    /// [`expected_kind`] against.
    pub fn of(value: &InfoValue) -> Self {
        match value {
            InfoValue::String(_) => InfoValueKind::String,
            InfoValue::U16(_) => InfoValueKind::U16,
            InfoValue::U32(_) => InfoValueKind::U32,
        }
    }
}

/// The spec-declared shape for `info_type`.
///
/// Transcribed directly from the SQLGetInfo spec's "Information Type
/// Descriptions" table (see module docs for the URL and the exact sentence
/// pattern each entry is read from). This is the deliberate, commented
/// exception to "no raw literals for spec-defined values": the `// SQL_*:
/// ...` comment on each arm *is* the spec citation, not decoration.
///
/// Every one of the 109 `InfoType` variants `odbc-sys` 0.31 (default
/// features) compiles resolves to a shape here: the spec's own description
/// text names a shape for every one of them, so there is no "spec does not
/// define a return type" exception to carve out for this crate's feature
/// set. This match has **no wildcard arm**: adding a new `odbc-sys` variant
/// without adding its shape here is a compile error.
pub const fn expected_kind(info_type: InfoType) -> InfoValueKind {
    use InfoValueKind::{String, U16, U32};
    match info_type {
        InfoType::MaxDriverConnections => U16, // SQL_MAX_DRIVER_CONNECTIONS: SQLUSMALLINT value
        InfoType::MaxConcurrentActivities => U16, // SQL_MAX_CONCURRENT_ACTIVITIES: SQLUSMALLINT value
        InfoType::DataSourceName => String,       // SQL_DATA_SOURCE_NAME: character string
        InfoType::DriverName => String,           // SQL_DRIVER_NAME: character string
        InfoType::DriverVer => String,            // SQL_DRIVER_VER: character string
        InfoType::ServerName => String,           // SQL_SERVER_NAME: character string
        InfoType::SearchPatternEscape => String,  // SQL_SEARCH_PATTERN_ESCAPE: character string
        InfoType::DbmsName => String,             // SQL_DBMS_NAME: character string
        InfoType::DbmsVer => String,              // SQL_DBMS_VER: character string
        InfoType::AccessibleTables => String,     // SQL_ACCESSIBLE_TABLES: character string
        InfoType::AccessibleProcedures => String, // SQL_ACCESSIBLE_PROCEDURES: character string
        InfoType::ConcatNullBehavior => U16,      // SQL_CONCAT_NULL_BEHAVIOR: SQLUSMALLINT value
        InfoType::CursorCommitBehaviour => U16,   // SQL_CURSOR_COMMIT_BEHAVIOR: SQLUSMALLINT value
        InfoType::DataSourceReadOnly => String,   // SQL_DATA_SOURCE_READ_ONLY: character string
        InfoType::DefaultTxnIsolation => U32,     // SQL_DEFAULT_TXN_ISOLATION: SQLUINTEGER value
        InfoType::ExpressionsInOrderBy => String, // SQL_EXPRESSIONS_IN_ORDERBY: character string
        InfoType::IdentifierCase => U16,          // SQL_IDENTIFIER_CASE: SQLUSMALLINT value
        InfoType::IdentifierQuoteChar => String,  // SQL_IDENTIFIER_QUOTE_CHAR: character string
        InfoType::MaxColumnNameLen => U16,        // SQL_MAX_COLUMN_NAME_LEN: SQLUSMALLINT value
        InfoType::MaxCursorNameLen => U16,        // SQL_MAX_CURSOR_NAME_LEN: SQLUSMALLINT value
        InfoType::MaxSchemaNameLen => U16,        // SQL_MAX_SCHEMA_NAME_LEN: SQLUSMALLINT value
        InfoType::MaxCatalogNameLen => U16,       // SQL_MAX_CATALOG_NAME_LEN: SQLUSMALLINT value
        InfoType::MaxTableNameLen => U16,         // SQL_MAX_TABLE_NAME_LEN: SQLUSMALLINT value
        InfoType::MultResultSets => String,       // SQL_MULT_RESULT_SETS: character string
        InfoType::OuterJoins => String,           // SQL_OUTER_JOINS: character string ("Y"/"P"/"N")
        InfoType::SchemaTerm => String,           // SQL_SCHEMA_TERM: character string
        InfoType::CatalogNameSeparator => String, // SQL_CATALOG_NAME_SEPARATOR: character string
        InfoType::CatalogTerm => String,          // SQL_CATALOG_TERM: character string
        InfoType::ScrollOptions => U32,           // SQL_SCROLL_OPTIONS: SQLUINTEGER bitmask
        InfoType::TransactionCapable => U16,      // SQL_TXN_CAPABLE: SQLUSMALLINT value
        InfoType::UserName => String,             // SQL_USER_NAME: character string
        InfoType::ConvertFunctions => U32,        // SQL_CONVERT_FUNCTIONS: SQLUINTEGER bitmask
        InfoType::NumericFunctions => U32,        // SQL_NUMERIC_FUNCTIONS: SQLUINTEGER bitmask
        InfoType::StringFunctions => U32,         // SQL_STRING_FUNCTIONS: SQLUINTEGER bitmask
        InfoType::SystemFunctions => U32,         // SQL_SYSTEM_FUNCTIONS: SQLUINTEGER bitmask
        InfoType::TimedateFunctions => U32,       // SQL_TIMEDATE_FUNCTIONS: SQLUINTEGER bitmask
        InfoType::TransactionIsolationProtocol => U32, // SQL_TXN_ISOLATION_OPTION: SQLUINTEGER bitmask
        InfoType::Integrity => String,                 // SQL_INTEGRITY: character string ("Y"/"N")
        InfoType::CorrelationName => U16,              // SQL_CORRELATION_NAME: SQLUSMALLINT value
        InfoType::NonNullableColumns => U16, // SQL_NON_NULLABLE_COLUMNS: SQLUSMALLINT value
        InfoType::DriverOdbcVer => String,   // SQL_DRIVER_ODBC_VER: character string
        InfoType::GetDataExtensions => U32,  // SQL_GETDATA_EXTENSIONS: SQLUINTEGER bitmask
        InfoType::SqlFileUsage => U16,       // SQL_FILE_USAGE: SQLUSMALLINT value
        InfoType::NullCollation => U16,      // SQL_NULL_COLLATION: SQLUSMALLINT value
        InfoType::AlterTable => U32,         // SQL_ALTER_TABLE: SQLUINTEGER bitmask
        InfoType::ColumnAlias => String,     // SQL_COLUMN_ALIAS: character string
        InfoType::GroupBy => U16,            // SQL_GROUP_BY: SQLUSMALLINT value
        InfoType::OrderByColumnsInSelect => String, // SQL_ORDER_BY_COLUMNS_IN_SELECT: character string
        InfoType::SchemaUsage => U32,               // SQL_SCHEMA_USAGE: SQLUINTEGER bitmask
        InfoType::CatalogUsage => U32,              // SQL_CATALOG_USAGE: SQLUINTEGER bitmask
        InfoType::SqlQuotedIdentifierCase => U16, // SQL_QUOTED_IDENTIFIER_CASE: SQLUSMALLINT value
        InfoType::SpecialCharacters => String,    // SQL_SPECIAL_CHARACTERS: character string
        InfoType::Subqueries => U32,              // SQL_SUBQUERIES: SQLUINTEGER bitmask
        InfoType::UnionStatement => U32,          // SQL_UNION: SQLUINTEGER bitmask
        InfoType::MaxColumnsInGroupBy => U16,     // SQL_MAX_COLUMNS_IN_GROUP_BY: SQLUSMALLINT value
        InfoType::MaxColumnsInIndex => U16,       // SQL_MAX_COLUMNS_IN_INDEX: SQLUSMALLINT value
        InfoType::MaxColumnsInOrderBy => U16,     // SQL_MAX_COLUMNS_IN_ORDER_BY: SQLUSMALLINT value
        InfoType::MaxColumnsInSelect => U16,      // SQL_MAX_COLUMNS_IN_SELECT: SQLUSMALLINT value
        InfoType::MaxColumnsInTable => U16,       // SQL_MAX_COLUMNS_IN_TABLE: SQLUSMALLINT value
        InfoType::MaxIndexSize => U32,            // SQL_MAX_INDEX_SIZE: SQLUINTEGER value
        InfoType::MaxRowSizeIncludesLong => String, // SQL_MAX_ROW_SIZE_INCLUDES_LONG: character string
        InfoType::MaxRowSize => U32,                // SQL_MAX_ROW_SIZE: SQLUINTEGER value
        InfoType::MaxStatementLen => U32,           // SQL_MAX_STATEMENT_LEN: SQLUINTEGER value
        InfoType::MaxTablesInSelect => U16,         // SQL_MAX_TABLES_IN_SELECT: SQLUSMALLINT value
        InfoType::MaxUserNameLen => U16,            // SQL_MAX_USER_NAME_LEN: SQLUSMALLINT value
        InfoType::TimedateAddIntervals => U32, // SQL_TIMEDATE_ADD_INTERVALS: SQLUINTEGER bitmask
        InfoType::TimedateDiffIntervals => U32, // SQL_TIMEDATE_DIFF_INTERVALS: SQLUINTEGER bitmask
        InfoType::NeedLongDataLen => String,   // SQL_NEED_LONG_DATA_LEN: character string
        InfoType::LikeEscapeClause => String,  // SQL_LIKE_ESCAPE_CLAUSE: character string
        InfoType::CatalogLocation => U16,      // SQL_CATALOG_LOCATION: SQLUSMALLINT value
        InfoType::OuterJoinCapabilities => U32, // SQL_OJ_CAPABILITIES: SQLUINTEGER bitmask
        InfoType::ActiveEnvironments => U16,   // SQL_ACTIVE_ENVIRONMENTS: SQLUSMALLINT value
        InfoType::SqlConformance => U32,       // SQL_SQL_CONFORMANCE: SQLUINTEGER value
        InfoType::BatchRowCount => U32,        // SQL_BATCH_ROW_COUNT: SQLUINTEGER bitmask
        InfoType::BatchSupport => U32,         // SQL_BATCH_SUPPORT: SQLUINTEGER bitmask
        InfoType::DynamicCursorAttributes1 => U32, // SQL_DYNAMIC_CURSOR_ATTRIBUTES1: SQLUINTEGER bitmask
        InfoType::DynamicCursorAttributes2 => U32, // SQL_DYNAMIC_CURSOR_ATTRIBUTES2: SQLUINTEGER bitmask
        InfoType::ForwardOnlyCursorAttributes1 => U32, // SQL_FORWARD_ONLY_CURSOR_ATTRIBUTES1: SQLUINTEGER bitmask
        InfoType::ForwardOnlyCursorAttributes2 => U32, // SQL_FORWARD_ONLY_CURSOR_ATTRIBUTES2: SQLUINTEGER bitmask
        InfoType::KeysetCursorAttributes1 => U32, // SQL_KEYSET_CURSOR_ATTRIBUTES1: SQLUINTEGER bitmask
        InfoType::KeysetCursorAttributes2 => U32, // SQL_KEYSET_CURSOR_ATTRIBUTES2: SQLUINTEGER bitmask
        InfoType::OdbcInterfaceConformance => U32, // SQL_ODBC_INTERFACE_CONFORMANCE: SQLUINTEGER value
        InfoType::ParamArrayRowCounts => U32, // SQL_PARAM_ARRAY_ROW_COUNTS: SQLUINTEGER bitmask
        InfoType::ParamArraySelects => U32,   // SQL_PARAM_ARRAY_SELECTS: SQLUINTEGER bitmask
        InfoType::Sql92DatetimeFunctions => U32, // SQL_SQL92_DATETIME_FUNCTIONS: SQLUINTEGER bitmask
        InfoType::Sql92ForeignKeyDeleteRule => U32, // SQL_SQL92_FOREIGN_KEY_DELETE_RULE: SQLUINTEGER bitmask
        InfoType::Sql92ForeignKeyUpdateRule => U32, // SQL_SQL92_FOREIGN_KEY_UPDATE_RULE: SQLUINTEGER bitmask
        InfoType::Sql92Grant => U32,                // SQL_SQL92_GRANT: SQLUINTEGER bitmask
        InfoType::Sql92NumericValueFunctions => U32, // SQL_SQL92_NUMERIC_VALUE_FUNCTIONS: SQLUINTEGER bitmask
        InfoType::Sql92Predicates => U32,            // SQL_SQL92_PREDICATES: SQLUINTEGER bitmask
        InfoType::Sql92RelationalJoinOperators => U32, // SQL_SQL92_RELATIONAL_JOIN_OPERATORS: SQLUINTEGER bitmask
        InfoType::Sql92Revoke => U32,                  // SQL_SQL92_REVOKE: SQLUINTEGER bitmask
        InfoType::Sql92RowValueConstructor => U32, // SQL_SQL92_ROW_VALUE_CONSTRUCTOR: SQLUINTEGER bitmask
        InfoType::Sql92StringFunctions => U32, // SQL_SQL92_STRING_FUNCTIONS: SQLUINTEGER bitmask
        InfoType::Sql92ValueExpressions => U32, // SQL_SQL92_VALUE_EXPRESSIONS: SQLUINTEGER bitmask
        InfoType::StaticCursorAttributes1 => U32, // SQL_STATIC_CURSOR_ATTRIBUTES1: SQLUINTEGER bitmask
        InfoType::StaticCursorAttributes2 => U32, // SQL_STATIC_CURSOR_ATTRIBUTES2: SQLUINTEGER bitmask
        InfoType::AggregateFunctions => U32,      // SQL_AGGREGATE_FUNCTIONS: SQLUINTEGER bitmask
        InfoType::XopenCliYear => String,         // SQL_XOPEN_CLI_YEAR: character string
        InfoType::CursorSensitivity => U32,       // SQL_CURSOR_SENSITIVITY: SQLUINTEGER value
        InfoType::DescribeParameter => String,    // SQL_DESCRIBE_PARAMETER: character string
        InfoType::CatalogName => String,          // SQL_CATALOG_NAME: character string
        InfoType::CollationSeq => String,         // SQL_COLLATION_SEQ: character string
        InfoType::MaxIdentifierLen => U16,        // SQL_MAX_IDENTIFIER_LEN: SQLUSMALLINT value
        InfoType::AsyncMode => U32,               // SQL_ASYNC_MODE: SQLUINTEGER value
        InfoType::MaxAsyncConcurrentStatements => U32, // SQL_MAX_ASYNC_CONCURRENT_STATEMENTS: SQLUINTEGER value
        InfoType::AsyncDbcFunctions => U32,            // SQL_ASYNC_DBC_FUNCTIONS: SQLUINTEGER value
        InfoType::DriverAwarePoolingSupported => U32, // SQL_DRIVER_AWARE_POOLING_SUPPORTED: SQLUINTEGER value
        InfoType::AsyncNotification => U32,           // SQL_ASYNC_NOTIFICATION: SQLUINTEGER value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::info_type_from_raw;

    /// Cross-check: every `InfoType` [`crate::types::info_type_from_raw`]
    /// can produce (i.e. every raw value the FFI boundary actually converts)
    /// must have a shape in [`expected_kind`]. `expected_kind`'s match has no
    /// wildcard arm, so this is really just confirming the two functions
    /// were built from the same variant set: if `odbc-sys` ever adds a
    /// variant, the match itself already refuses to compile until this
    /// module is updated, this test documents *why* that invariant matters.
    #[cfg_attr(
        miri,
        ignore = "65,536-value sweep; no unsafe in this module for Miri to check"
    )]
    #[test]
    fn every_raw_info_type_has_a_declared_shape() {
        let mut count = 0;
        for raw in 0..=u16::MAX {
            if let Some(info_type) = info_type_from_raw(raw) {
                let _ = expected_kind(info_type); // must not panic / fail to compile
                count += 1;
            }
        }
        // Sanity floor: odbc-sys 0.31 (default features) exposes 109 named
        // InfoType variants. This isn't pinned to the exact number (a future
        // odbc-sys bump could add more, which `expected_kind`'s exhaustive
        // match would already force us to handle). It just guards against the
        // derivation loop above silently finding zero variants.
        assert!(
            count >= 109,
            "expected at least 109 InfoType variants, found {count}"
        );
    }
}
