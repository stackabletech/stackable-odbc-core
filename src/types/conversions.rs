//! `*_from_raw` conversion functions that turn the raw integers arriving over
//! the ODBC C ABI into strongly-typed `odbc-sys` enums, returning `None` for
//! unrecognized values. These are the safe alternative to `transmute` and are
//! called at the FFI boundary before any logic runs.

use odbc_sys::{
    AttrOdbcVersion, CDataType, Desc, DriverConnectOption, EnvironmentAttribute, FetchOrientation,
    FreeStmtOption, HandleType, InfoType, Interval, StatementAttribute,
};
use odbc_sys::{BulkOperation, CompletionType, ParamType};

use crate::types::constants::*;
use crate::types::{DeclaredOdbcVersion, IdentifierType, Nullable, Scope};

/// Convert a raw `i16` from the ODBC ABI into an `odbc_sys::HandleType`.
///
/// Returns `None` for values that are not a recognized handle type.
#[must_use]
pub fn handle_type_from_raw(value: i16) -> Option<HandleType> {
    match value {
        1 => Some(HandleType::Env),
        2 => Some(HandleType::Dbc),
        3 => Some(HandleType::Stmt),
        4 => Some(HandleType::Desc),
        6 => Some(HandleType::DbcInfoToken),
        _ => None,
    }
}

/// Convert a raw `u16` from the ODBC ABI into an `odbc_sys::Desc`.
///
/// Returns `None` for values that are not a recognized descriptor field identifier.
/// This is the safe alternative to `transmute` for the `#[repr(u16)]` `Desc` enum.
#[must_use]
pub fn desc_from_raw(value: u16) -> Option<Desc> {
    match value {
        2 => Some(Desc::ConciseType),
        6 => Some(Desc::DisplaySize),
        8 => Some(Desc::Unsigned),
        9 => Some(Desc::FixedPrecScale),
        10 => Some(Desc::Updatable),
        11 => Some(Desc::AutoUniqueValue),
        12 => Some(Desc::CaseSensitive),
        13 => Some(Desc::Searchable),
        14 => Some(Desc::TypeName),
        15 => Some(Desc::TableName),
        16 => Some(Desc::SchemaName),
        17 => Some(Desc::CatalogName),
        18 => Some(Desc::Label),
        20 => Some(Desc::ArraySize),
        21 => Some(Desc::ArrayStatusPtr),
        22 => Some(Desc::BaseColumnName),
        23 => Some(Desc::BaseTableName),
        24 => Some(Desc::BindOffsetPtr),
        25 => Some(Desc::BindType),
        26 => Some(Desc::DatetimeIntervalPrecision),
        27 => Some(Desc::LiteralPrefix),
        28 => Some(Desc::LiteralSuffix),
        29 => Some(Desc::LocalTypeName),
        30 => Some(Desc::MaximumScale),
        31 => Some(Desc::MinimumScale),
        32 => Some(Desc::NumPrecRadix),
        33 => Some(Desc::ParameterType),
        34 => Some(Desc::RowsProcessedPtr),
        35 => Some(Desc::RowVer),
        1001 => Some(Desc::Count),
        1002 => Some(Desc::Type),
        1003 => Some(Desc::Length),
        1004 => Some(Desc::OctetLengthPtr),
        1005 => Some(Desc::Precision),
        1006 => Some(Desc::Scale),
        1007 => Some(Desc::DatetimeIntervalCode),
        1008 => Some(Desc::Nullable),
        1009 => Some(Desc::IndicatorPtr),
        1010 => Some(Desc::DataPtr),
        1011 => Some(Desc::Name),
        1012 => Some(Desc::Unnamed),
        1013 => Some(Desc::OctetLength),
        1099 => Some(Desc::AllocType),
        _ => None,
    }
}

/// Convert a raw `u16` from the ODBC ABI into an `odbc_sys::InfoType`.
///
/// Returns `None` for values that are not a recognized info type.
#[must_use]
pub fn info_type_from_raw(value: u16) -> Option<odbc_sys::InfoType> {
    match value {
        0 => Some(InfoType::MaxDriverConnections),
        1 => Some(InfoType::MaxConcurrentActivities),
        2 => Some(InfoType::DataSourceName),
        6 => Some(InfoType::DriverName),
        7 => Some(InfoType::DriverVer),
        13 => Some(InfoType::ServerName),
        14 => Some(InfoType::SearchPatternEscape),
        17 => Some(InfoType::DbmsName),
        18 => Some(InfoType::DbmsVer),
        19 => Some(InfoType::AccessibleTables),
        20 => Some(InfoType::AccessibleProcedures),
        22 => Some(InfoType::ConcatNullBehavior),
        23 => Some(InfoType::CursorCommitBehaviour),
        // 24 = SQL_CURSOR_ROLLBACK_BEHAVIOR — not in odbc-sys, handled via get_info_raw
        25 => Some(InfoType::DataSourceReadOnly),
        26 => Some(InfoType::DefaultTxnIsolation),
        27 => Some(InfoType::ExpressionsInOrderBy),
        28 => Some(InfoType::IdentifierCase),
        29 => Some(InfoType::IdentifierQuoteChar),
        30 => Some(InfoType::MaxColumnNameLen),
        31 => Some(InfoType::MaxCursorNameLen),
        32 => Some(InfoType::MaxSchemaNameLen),
        34 => Some(InfoType::MaxCatalogNameLen),
        35 => Some(InfoType::MaxTableNameLen),
        36 => Some(InfoType::MultResultSets),
        38 => Some(InfoType::OuterJoins),
        39 => Some(InfoType::SchemaTerm),
        41 => Some(InfoType::CatalogNameSeparator),
        42 => Some(InfoType::CatalogTerm),
        44 => Some(InfoType::ScrollOptions),
        46 => Some(InfoType::TransactionCapable),
        47 => Some(InfoType::UserName),
        48 => Some(InfoType::ConvertFunctions),
        49 => Some(InfoType::NumericFunctions),
        50 => Some(InfoType::StringFunctions),
        51 => Some(InfoType::SystemFunctions),
        52 => Some(InfoType::TimedateFunctions),
        72 => Some(InfoType::TransactionIsolationProtocol),
        73 => Some(InfoType::Integrity),
        74 => Some(InfoType::CorrelationName),
        75 => Some(InfoType::NonNullableColumns),
        77 => Some(InfoType::DriverOdbcVer),
        81 => Some(InfoType::GetDataExtensions),
        84 => Some(InfoType::SqlFileUsage),
        85 => Some(InfoType::NullCollation),
        86 => Some(InfoType::AlterTable),
        87 => Some(InfoType::ColumnAlias),
        88 => Some(InfoType::GroupBy),
        90 => Some(InfoType::OrderByColumnsInSelect),
        91 => Some(InfoType::SchemaUsage),
        92 => Some(InfoType::CatalogUsage),
        93 => Some(InfoType::SqlQuotedIdentifierCase),
        94 => Some(InfoType::SpecialCharacters),
        95 => Some(InfoType::Subqueries),
        96 => Some(InfoType::UnionStatement),
        97 => Some(InfoType::MaxColumnsInGroupBy),
        98 => Some(InfoType::MaxColumnsInIndex),
        99 => Some(InfoType::MaxColumnsInOrderBy),
        100 => Some(InfoType::MaxColumnsInSelect),
        101 => Some(InfoType::MaxColumnsInTable),
        102 => Some(InfoType::MaxIndexSize),
        103 => Some(InfoType::MaxRowSizeIncludesLong),
        104 => Some(InfoType::MaxRowSize),
        105 => Some(InfoType::MaxStatementLen),
        106 => Some(InfoType::MaxTablesInSelect),
        107 => Some(InfoType::MaxUserNameLen),
        109 => Some(InfoType::TimedateAddIntervals),
        110 => Some(InfoType::TimedateDiffIntervals),
        111 => Some(InfoType::NeedLongDataLen),
        113 => Some(InfoType::LikeEscapeClause),
        114 => Some(InfoType::CatalogLocation),
        115 => Some(InfoType::OuterJoinCapabilities),
        116 => Some(InfoType::ActiveEnvironments),
        118 => Some(InfoType::SqlConformance),
        120 => Some(InfoType::BatchRowCount),
        121 => Some(InfoType::BatchSupport),
        144 => Some(InfoType::DynamicCursorAttributes1),
        145 => Some(InfoType::DynamicCursorAttributes2),
        146 => Some(InfoType::ForwardOnlyCursorAttributes1),
        147 => Some(InfoType::ForwardOnlyCursorAttributes2),
        150 => Some(InfoType::KeysetCursorAttributes1),
        151 => Some(InfoType::KeysetCursorAttributes2),
        152 => Some(InfoType::OdbcInterfaceConformance),
        153 => Some(InfoType::ParamArrayRowCounts),
        154 => Some(InfoType::ParamArraySelects),
        155 => Some(InfoType::Sql92DatetimeFunctions),
        156 => Some(InfoType::Sql92ForeignKeyDeleteRule),
        157 => Some(InfoType::Sql92ForeignKeyUpdateRule),
        158 => Some(InfoType::Sql92Grant),
        159 => Some(InfoType::Sql92NumericValueFunctions),
        160 => Some(InfoType::Sql92Predicates),
        161 => Some(InfoType::Sql92RelationalJoinOperators),
        162 => Some(InfoType::Sql92Revoke),
        163 => Some(InfoType::Sql92RowValueConstructor),
        164 => Some(InfoType::Sql92StringFunctions),
        165 => Some(InfoType::Sql92ValueExpressions),
        167 => Some(InfoType::StaticCursorAttributes1),
        168 => Some(InfoType::StaticCursorAttributes2),
        169 => Some(InfoType::AggregateFunctions),
        10000 => Some(InfoType::XopenCliYear),
        10001 => Some(InfoType::CursorSensitivity),
        10002 => Some(InfoType::DescribeParameter),
        10003 => Some(InfoType::CatalogName),
        10004 => Some(InfoType::CollationSeq),
        10005 => Some(InfoType::MaxIdentifierLen),
        10021 => Some(InfoType::AsyncMode),
        10022 => Some(InfoType::MaxAsyncConcurrentStatements),
        10023 => Some(InfoType::AsyncDbcFunctions),
        10024 => Some(InfoType::DriverAwarePoolingSupported),
        10025 => Some(InfoType::AsyncNotification),
        _ => None,
    }
}

/// Convert a raw `SQL_IS_*` subcode into an `odbc_sys::Interval`.
///
/// Returns `None` for values that are not a recognised interval subcode.
/// This is the safe alternative to `transmute` for the `#[repr(C)]` `Interval`
/// enum, whose explicit discriminants 1..=13 let a converted value be cast back
/// to the integer it came from — the round-trip that makes a conversion
/// function the right answer here rather than a named constant.
///
/// **These are `SQL_DESC_DATETIME_INTERVAL_CODE`'s values, 1..=13, not the
/// concise `SQL_INTERVAL_*` SQL types**, which are the same subcodes plus 100.
/// The two numbering schemes are easy to conflate and mean different things:
/// the subcode says *which* interval a verbose `SQL_INTERVAL` type is, and the
/// concise type says the whole thing in one number.
#[must_use]
pub fn interval_from_raw(value: i16) -> Option<Interval> {
    match value {
        1 => Some(Interval::Year),
        2 => Some(Interval::Month),
        3 => Some(Interval::Day),
        4 => Some(Interval::Hour),
        5 => Some(Interval::Minute),
        6 => Some(Interval::Second),
        7 => Some(Interval::YearToMonth),
        8 => Some(Interval::DayToHour),
        9 => Some(Interval::DayToMinute),
        10 => Some(Interval::DayToSecond),
        11 => Some(Interval::HourToMinute),
        12 => Some(Interval::HourToSecond),
        13 => Some(Interval::MinuteToSecond),
        _ => None,
    }
}

/// Convert a raw `i16` from the ODBC ABI into an `odbc_sys::CDataType`.
///
/// Returns `None` for values that are not a recognised C data type.
/// This is the safe alternative to `transmute` for the `#[repr(i16)]` `CDataType` enum.
#[must_use]
pub fn c_data_type_from_raw(value: i16) -> Option<CDataType> {
    match value {
        -100 => Some(CDataType::Apd),
        -99 => Some(CDataType::Ard),
        -28 => Some(CDataType::UTinyInt),
        -27 => Some(CDataType::UBigInt),
        -26 => Some(CDataType::STinyInt),
        -25 => Some(CDataType::SBigInt),
        -18 => Some(CDataType::ULong),
        -17 => Some(CDataType::UShort),
        -16 => Some(CDataType::SLong),
        -15 => Some(CDataType::SShort),
        -11 => Some(CDataType::Guid),
        -8 => Some(CDataType::WChar),
        -7 => Some(CDataType::Bit),
        // SQL_C_TINYINT (-6), SQL_C_LONG (4) and SQL_C_SHORT (5) are the
        // deprecated ODBC 2.x spellings of SQL_C_STINYINT (-26), SQL_C_SLONG
        // (-16) and SQL_C_SSHORT (-15). All three default to signed.
        // Applications still emit them — pyodbc among them — so they are
        // accepted and normalised here, which is why no code past this boundary
        // has to know they exist. `odbc-sys` models none of the three, carrying
        // them only as commented-out lines; that is a gap in the binding, not
        // in the ABI this crate accepts.
        -6 => Some(CDataType::STinyInt),
        -2 => Some(CDataType::Binary),
        1 => Some(CDataType::Char),
        2 => Some(CDataType::Numeric),
        4 => Some(CDataType::SLong),
        5 => Some(CDataType::SShort),
        7 => Some(CDataType::Float),
        8 => Some(CDataType::Double),
        9 => Some(CDataType::Date),
        10 => Some(CDataType::Time),
        11 => Some(CDataType::TimeStamp),
        91 => Some(CDataType::TypeDate),
        92 => Some(CDataType::TypeTime),
        93 => Some(CDataType::TypeTimestamp),
        99 => Some(CDataType::Default),
        101 => Some(CDataType::IntervalYear),
        102 => Some(CDataType::IntervalMonth),
        103 => Some(CDataType::IntervalDay),
        104 => Some(CDataType::IntervalHour),
        105 => Some(CDataType::IntervalMinute),
        106 => Some(CDataType::IntervalSecond),
        107 => Some(CDataType::IntervalYearToMonth),
        108 => Some(CDataType::IntervalDayToHour),
        109 => Some(CDataType::IntervalDayToMinute),
        110 => Some(CDataType::IntervalDayToSecond),
        111 => Some(CDataType::IntervalHourToMinute),
        112 => Some(CDataType::IntervalHourToSecond),
        113 => Some(CDataType::IntervalMinuteToSecond),
        // SQL_C_TYPES_EXTENDED (0x4000) and 0x4001: SQL Server-specific extended
        // C types, defined relative to `odbc_sys::C_TYPES_EXTENDED`.
        16384 => Some(CDataType::SsTime2),
        16385 => Some(CDataType::SsTimestampOffset),
        _ => None,
    }
}

/// Convert a raw `i16` from the ODBC ABI into an `odbc_sys::ParamType`.
///
/// Returns `None` for values that are not a recognised parameter type.
/// This is the safe alternative to `transmute` for the `#[repr(i16)]` `ParamType` enum.
#[must_use]
pub fn param_type_from_raw(value: i16) -> Option<ParamType> {
    match value {
        0 => Some(ParamType::Unknown),
        1 => Some(ParamType::Input),
        2 => Some(ParamType::InputOutput),
        3 => Some(ParamType::ResultCol),
        4 => Some(ParamType::Output),
        5 => Some(ParamType::ReturnValue),
        8 => Some(ParamType::InputOutputStream),
        16 => Some(ParamType::OutputStream),
        _ => None,
    }
}

/// Convert a raw `i32` from the ODBC ABI into an `odbc_sys::EnvironmentAttribute`.
///
/// Returns `None` for values that are not a recognised environment attribute.
/// This is the safe alternative to `transmute` for the `#[repr(i32)]` `EnvironmentAttribute` enum.
#[must_use]
pub fn environment_attribute_from_raw(value: i32) -> Option<EnvironmentAttribute> {
    match value {
        200 => Some(EnvironmentAttribute::OdbcVersion),
        201 => Some(EnvironmentAttribute::ConnectionPooling),
        202 => Some(EnvironmentAttribute::CpMatch),
        10001 => Some(EnvironmentAttribute::OutputNts),
        _ => None,
    }
}

/// Convert a raw `i32` from the ODBC ABI into an `odbc_sys::AttrOdbcVersion`.
///
/// Returns `None` for values that are not a recognised ODBC version.
/// This is the safe alternative to `transmute` for the `#[repr(i32)]` `AttrOdbcVersion` enum.
///
/// `SQL_OV_ODBC2` is `None` here because `odbc-sys` has no variant for it, not
/// because core rejects it. `SQLSetEnvAttr` uses
/// [`declared_odbc_version_from_raw`], which accepts all three spec values.
#[must_use]
pub fn attr_odbc_version_from_raw(value: i32) -> Option<AttrOdbcVersion> {
    match value {
        3 => Some(AttrOdbcVersion::Odbc3),
        380 => Some(AttrOdbcVersion::Odbc3_80),
        _ => None,
    }
}

/// Convert a raw `i32` from the ODBC ABI into a [`DeclaredOdbcVersion`].
///
/// Returns `None` for values that are not a recognised `SQL_OV_*` version.
///
/// This is the conversion `SQLSetEnvAttr` uses, and it differs from
/// [`attr_odbc_version_from_raw`] in exactly one way: it accepts
/// `SQL_OV_ODBC2`, which [`odbc_sys::AttrOdbcVersion`] has no variant for.
#[must_use]
pub fn declared_odbc_version_from_raw(value: i32) -> Option<DeclaredOdbcVersion> {
    match value {
        SQL_OV_ODBC2 => Some(DeclaredOdbcVersion::Odbc2),
        SQL_OV_ODBC3 => Some(DeclaredOdbcVersion::Odbc3),
        SQL_OV_ODBC3_80 => Some(DeclaredOdbcVersion::Odbc3_80),
        _ => None,
    }
}

/// Convert a raw `u16` from the ODBC ABI into an `odbc_sys::FreeStmtOption`.
///
/// Returns `None` for values that are not a recognised free-statement option.
/// This is the safe alternative to `transmute` for the `#[repr(u16)]` `FreeStmtOption` enum.
#[must_use]
pub fn free_stmt_option_from_raw(value: u16) -> Option<FreeStmtOption> {
    match value {
        0 => Some(FreeStmtOption::Close),
        2 => Some(FreeStmtOption::Unbind),
        3 => Some(FreeStmtOption::ResetParams),
        _ => None,
    }
}

/// Convert a raw `u16` from the ODBC ABI into an `odbc_sys::DriverConnectOption`.
///
/// This is `SQLDriverConnect`'s *DriverCompletion* argument, which tells the
/// driver whether it may prompt the user.
///
/// Returns `None` for values that are not a recognised completion flag. The
/// spec's state for that is `HY110` ("Invalid driver completion"), but **both**
/// of its clauses carry `(DM)`, so the check belongs to the Driver Manager and
/// core adds none — the caller treats `None` as permitting a prompt, which is
/// the reading that does not silently disable interactive authentication for an
/// application whose value never reached a Driver Manager to be validated.
#[must_use]
pub fn driver_connect_option_from_raw(value: u16) -> Option<DriverConnectOption> {
    match value {
        0 => Some(DriverConnectOption::NoPrompt),
        1 => Some(DriverConnectOption::Complete),
        2 => Some(DriverConnectOption::Prompt),
        3 => Some(DriverConnectOption::CompleteRequired),
        _ => None,
    }
}

/// Convert a raw `i32` from the ODBC ABI into an `odbc_sys::StatementAttribute`.
///
/// Returns `None` for values that are not a recognised statement attribute.
/// This is the safe alternative to `transmute` for the `#[repr(i32)]` `StatementAttribute` enum.
#[must_use]
pub fn statement_attribute_from_raw(value: i32) -> Option<StatementAttribute> {
    match value {
        -2 => Some(StatementAttribute::CursorSensitivity),
        -1 => Some(StatementAttribute::CursorScrollable),
        0 => Some(StatementAttribute::QueryTimeout),
        1 => Some(StatementAttribute::MaxRows),
        2 => Some(StatementAttribute::NoScan),
        3 => Some(StatementAttribute::MaxLength),
        4 => Some(StatementAttribute::AsyncEnable),
        5 => Some(StatementAttribute::RowBindType),
        6 => Some(StatementAttribute::CursorType),
        7 => Some(StatementAttribute::Concurrency),
        8 => Some(StatementAttribute::KeysetSize),
        10 => Some(StatementAttribute::SimulateCursor),
        11 => Some(StatementAttribute::RetrieveData),
        12 => Some(StatementAttribute::UseBookmarks),
        14 => Some(StatementAttribute::RowNumber),
        15 => Some(StatementAttribute::EnableAutoIpd),
        16 => Some(StatementAttribute::FetchBookmarkPtr),
        17 => Some(StatementAttribute::ParamBindOffsetPtr),
        18 => Some(StatementAttribute::ParamBindType),
        19 => Some(StatementAttribute::ParamOpterationPtr),
        20 => Some(StatementAttribute::ParamStatusPtr),
        21 => Some(StatementAttribute::ParamsProcessedPtr),
        22 => Some(StatementAttribute::ParamsetSize),
        23 => Some(StatementAttribute::RowBindOffsetPtr),
        24 => Some(StatementAttribute::RowOperationPtr),
        25 => Some(StatementAttribute::RowStatusPtr),
        26 => Some(StatementAttribute::RowsFetchedPtr),
        27 => Some(StatementAttribute::RowArraySize),
        29 => Some(StatementAttribute::AsyncStmtEvent),
        10010 => Some(StatementAttribute::AppRowDesc),
        10011 => Some(StatementAttribute::AppParamDesc),
        10012 => Some(StatementAttribute::ImpRowDesc),
        10013 => Some(StatementAttribute::ImpParamDesc),
        10014 => Some(StatementAttribute::MetadataId),
        _ => None,
    }
}

/// Convert a raw `i16` from the ODBC ABI into an `odbc_sys::CompletionType`.
///
/// Returns `None` for values that are not a recognised completion type.
/// This is the safe alternative to `transmute` for the `#[repr(i16)]` `CompletionType` enum.
#[must_use]
pub fn completion_type_from_raw(value: i16) -> Option<CompletionType> {
    match value {
        0 => Some(CompletionType::Commit),
        1 => Some(CompletionType::Rollback),
        _ => None,
    }
}

// `SQLSetPos`'s Operation and LockType have no conversion here, and that is a
// limitation of `odbc-sys` rather than an oversight. `odbc_sys::Operation` and
// `odbc_sys::Lock` are newtype structs over a *private* `i16` with no accessor,
// no `From`, and no `#[repr]` enum to cast through — so a converted value could
// be compared against the three associated constants and nothing else. Neither
// core nor a driver could recover the raw code to forward it, and a test could
// not name a valid input at all. `SQLSetPos` therefore validates against
// `SQL_POSITION` / `SQL_LOCK_*` in `constants.rs`, which are the only spelling
// of those codes anything can actually use.

/// Convert a raw `i16` from the ODBC ABI into an `odbc_sys::BulkOperation`.
///
/// Returns `None` for values that are not a recognised bulk operation, which
/// is what `SQLBulkOperations` reports as `HY092`. This is the safe
/// alternative to `transmute` for the `#[repr(u16)]` `BulkOperation` enum.
#[must_use]
pub fn bulk_operation_from_raw(value: i16) -> Option<BulkOperation> {
    match value {
        4 => Some(BulkOperation::Add),
        5 => Some(BulkOperation::UpdateByBookmark),
        6 => Some(BulkOperation::DeleteByBookmark),
        7 => Some(BulkOperation::FetchByBookmark),
        _ => None,
    }
}

/// Convert a raw `i16` from the ODBC ABI into an `odbc_sys::FetchOrientation`.
///
/// Returns `None` for values that are not a recognised fetch orientation.
/// This is the safe alternative to `transmute` for the `#[repr(u16)]` `FetchOrientation` enum.
#[must_use]
pub fn fetch_orientation_from_raw(value: i16) -> Option<FetchOrientation> {
    match value {
        1 => Some(FetchOrientation::Next),
        2 => Some(FetchOrientation::First),
        3 => Some(FetchOrientation::Last),
        4 => Some(FetchOrientation::Prior),
        5 => Some(FetchOrientation::Absolute),
        6 => Some(FetchOrientation::Relative),
        31 => Some(FetchOrientation::FirstUser),
        32 => Some(FetchOrientation::FirstSystem),
        _ => None,
    }
}

/// Convert a raw `IdentifierType` argument (`SQLSpecialColumns`) to the typed enum.
///
/// Returns `None` for values that are not a recognised identifier type.
#[must_use]
pub fn identifier_type_from_raw(value: u16) -> Option<IdentifierType> {
    match value {
        SQL_BEST_ROWID => Some(IdentifierType::BestRowId),
        SQL_ROWVER => Some(IdentifierType::RowVer),
        _ => None,
    }
}

/// Convert a raw `Scope` argument (`SQLSpecialColumns`) to the typed enum.
///
/// Returns `None` for values that are not a recognised scope.
#[must_use]
pub fn scope_from_raw(value: u16) -> Option<Scope> {
    match value {
        SQL_SCOPE_CURROW => Some(Scope::CurRow),
        SQL_SCOPE_TRANSACTION => Some(Scope::Transaction),
        SQL_SCOPE_SESSION => Some(Scope::Session),
        _ => None,
    }
}

/// Convert a raw `Nullable` argument to `stackable_odbc_core::types::Nullable`.
///
/// Returns `None` for values that are not a recognised nullability.
#[must_use]
pub fn nullable_from_raw(value: u16) -> Option<Nullable> {
    match value {
        0 => Some(Nullable::SqlNoNulls),
        1 => Some(Nullable::SqlNullable),
        2 => Some(Nullable::SqlNullableUnknown),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;

    // --- handle_type_from_raw ---

    #[test]
    fn handle_type_valid() {
        assert_eq!(handle_type_from_raw(1), Some(HandleType::Env));
        assert_eq!(handle_type_from_raw(2), Some(HandleType::Dbc));
        assert_eq!(handle_type_from_raw(3), Some(HandleType::Stmt));
        assert_eq!(handle_type_from_raw(4), Some(HandleType::Desc));
        assert_eq!(handle_type_from_raw(6), Some(HandleType::DbcInfoToken));
    }

    #[test]
    fn handle_type_invalid() {
        assert_eq!(handle_type_from_raw(0), None);
        assert_eq!(handle_type_from_raw(5), None);
        assert_eq!(handle_type_from_raw(99), None);
    }

    // --- desc_from_raw ---

    /// Every `Desc` variant compiled under `odbc_version_3_80` (the workspace's
    /// `odbc-sys` feature set). Hand-maintained so a future addition to
    /// `odbc_sys::Desc` that isn't also added to `desc_from_raw` fails a test
    /// instead of silently staying unnamed. Excludes `odbc_version_4`-gated
    /// variants (`CharacterSetCatalog`, `CharacterSetSchema`, `CharacterSetName`,
    /// `CollationCatalog`, `CollationSchema`, `CollationName`,
    /// `UserDefinedTypeCatalog`, `UserDefinedTypeSchema`, `UserDefinedTypeName`,
    /// `MimeType`), which do not compile in this feature set.
    const ALL_DESC_VARIANTS: &[Desc] = &[
        Desc::Count,
        Desc::Type,
        Desc::Length,
        Desc::OctetLengthPtr,
        Desc::Precision,
        Desc::Scale,
        Desc::DatetimeIntervalCode,
        Desc::Nullable,
        Desc::IndicatorPtr,
        Desc::DataPtr,
        Desc::Name,
        Desc::Unnamed,
        Desc::OctetLength,
        Desc::AllocType,
        Desc::ArraySize,
        Desc::ArrayStatusPtr,
        Desc::AutoUniqueValue,
        Desc::BaseColumnName,
        Desc::BaseTableName,
        Desc::BindOffsetPtr,
        Desc::BindType,
        Desc::CaseSensitive,
        Desc::CatalogName,
        Desc::ConciseType,
        Desc::DatetimeIntervalPrecision,
        Desc::DisplaySize,
        Desc::FixedPrecScale,
        Desc::Label,
        Desc::LiteralPrefix,
        Desc::LiteralSuffix,
        Desc::LocalTypeName,
        Desc::MaximumScale,
        Desc::MinimumScale,
        Desc::NumPrecRadix,
        Desc::ParameterType,
        Desc::RowsProcessedPtr,
        Desc::RowVer,
        Desc::SchemaName,
        Desc::Searchable,
        Desc::TypeName,
        Desc::TableName,
        Desc::Unsigned,
        Desc::Updatable,
    ];

    #[test]
    fn every_desc_variant_round_trips() {
        for &variant in ALL_DESC_VARIANTS {
            assert_eq!(
                desc_from_raw(variant as u16),
                Some(variant),
                "{variant:?} is not handled by desc_from_raw"
            );
        }
    }

    #[test]
    fn desc_from_raw_valid() {
        assert_eq!(desc_from_raw(2), Some(Desc::ConciseType));
        assert_eq!(desc_from_raw(1001), Some(Desc::Count));
        assert_eq!(desc_from_raw(1003), Some(Desc::Length));
        assert_eq!(desc_from_raw(1010), Some(Desc::DataPtr));
        assert_eq!(desc_from_raw(1099), Some(Desc::AllocType));
    }

    #[test]
    fn desc_from_raw_invalid() {
        assert_eq!(desc_from_raw(0), None);
        assert_eq!(desc_from_raw(1), None);
        assert_eq!(desc_from_raw(9999), None);
    }

    // --- info_type_from_raw ---

    /// Every `InfoType` variant compiled under `odbc_version_3_80`. `info_type.rs`
    /// in `odbc-sys` 0.31.0 has no `odbc_version_4`-gated variants, so this list
    /// is the complete enum (109 variants). Hand-maintained so a future addition
    /// to `odbc_sys::InfoType` that isn't also added to `info_type_from_raw` fails
    /// a test instead of silently staying unnamed.
    const ALL_INFO_TYPE_VARIANTS: &[InfoType] = &[
        InfoType::MaxDriverConnections,
        InfoType::MaxConcurrentActivities,
        InfoType::DataSourceName,
        InfoType::DriverName,
        InfoType::DriverVer,
        InfoType::ServerName,
        InfoType::SearchPatternEscape,
        InfoType::DbmsName,
        InfoType::DbmsVer,
        InfoType::AccessibleTables,
        InfoType::AccessibleProcedures,
        InfoType::ConcatNullBehavior,
        InfoType::CursorCommitBehaviour,
        InfoType::DataSourceReadOnly,
        InfoType::DefaultTxnIsolation,
        InfoType::ExpressionsInOrderBy,
        InfoType::IdentifierCase,
        InfoType::IdentifierQuoteChar,
        InfoType::MaxColumnNameLen,
        InfoType::MaxCursorNameLen,
        InfoType::MaxSchemaNameLen,
        InfoType::MaxCatalogNameLen,
        InfoType::MaxTableNameLen,
        InfoType::MultResultSets,
        InfoType::OuterJoins,
        InfoType::SchemaTerm,
        InfoType::CatalogNameSeparator,
        InfoType::CatalogTerm,
        InfoType::ScrollOptions,
        InfoType::TransactionCapable,
        InfoType::UserName,
        InfoType::ConvertFunctions,
        InfoType::NumericFunctions,
        InfoType::StringFunctions,
        InfoType::SystemFunctions,
        InfoType::TimedateFunctions,
        InfoType::TransactionIsolationProtocol,
        InfoType::Integrity,
        InfoType::CorrelationName,
        InfoType::NonNullableColumns,
        InfoType::DriverOdbcVer,
        InfoType::GetDataExtensions,
        InfoType::SqlFileUsage,
        InfoType::NullCollation,
        InfoType::AlterTable,
        InfoType::ColumnAlias,
        InfoType::GroupBy,
        InfoType::OrderByColumnsInSelect,
        InfoType::SchemaUsage,
        InfoType::CatalogUsage,
        InfoType::SqlQuotedIdentifierCase,
        InfoType::SpecialCharacters,
        InfoType::Subqueries,
        InfoType::UnionStatement,
        InfoType::MaxColumnsInGroupBy,
        InfoType::MaxColumnsInIndex,
        InfoType::MaxColumnsInOrderBy,
        InfoType::MaxColumnsInSelect,
        InfoType::MaxColumnsInTable,
        InfoType::MaxIndexSize,
        InfoType::MaxRowSizeIncludesLong,
        InfoType::MaxRowSize,
        InfoType::MaxStatementLen,
        InfoType::MaxTablesInSelect,
        InfoType::MaxUserNameLen,
        InfoType::TimedateAddIntervals,
        InfoType::TimedateDiffIntervals,
        InfoType::NeedLongDataLen,
        InfoType::LikeEscapeClause,
        InfoType::CatalogLocation,
        InfoType::OuterJoinCapabilities,
        InfoType::ActiveEnvironments,
        InfoType::SqlConformance,
        InfoType::BatchRowCount,
        InfoType::BatchSupport,
        InfoType::DynamicCursorAttributes1,
        InfoType::DynamicCursorAttributes2,
        InfoType::ForwardOnlyCursorAttributes1,
        InfoType::ForwardOnlyCursorAttributes2,
        InfoType::KeysetCursorAttributes1,
        InfoType::KeysetCursorAttributes2,
        InfoType::OdbcInterfaceConformance,
        InfoType::ParamArrayRowCounts,
        InfoType::ParamArraySelects,
        InfoType::Sql92DatetimeFunctions,
        InfoType::Sql92ForeignKeyDeleteRule,
        InfoType::Sql92ForeignKeyUpdateRule,
        InfoType::Sql92Grant,
        InfoType::Sql92NumericValueFunctions,
        InfoType::Sql92Predicates,
        InfoType::Sql92RelationalJoinOperators,
        InfoType::Sql92Revoke,
        InfoType::Sql92RowValueConstructor,
        InfoType::Sql92StringFunctions,
        InfoType::Sql92ValueExpressions,
        InfoType::StaticCursorAttributes1,
        InfoType::StaticCursorAttributes2,
        InfoType::AggregateFunctions,
        InfoType::XopenCliYear,
        InfoType::CursorSensitivity,
        InfoType::DescribeParameter,
        InfoType::CatalogName,
        InfoType::CollationSeq,
        InfoType::MaxIdentifierLen,
        InfoType::AsyncMode,
        InfoType::MaxAsyncConcurrentStatements,
        InfoType::AsyncDbcFunctions,
        InfoType::DriverAwarePoolingSupported,
        InfoType::AsyncNotification,
    ];

    #[test]
    fn every_info_type_variant_round_trips() {
        for &variant in ALL_INFO_TYPE_VARIANTS {
            assert_eq!(
                info_type_from_raw(variant as u16),
                Some(variant),
                "{variant:?} is not handled by info_type_from_raw"
            );
        }
    }

    #[test]
    fn info_type_from_raw_valid() {
        assert_eq!(info_type_from_raw(0), Some(InfoType::MaxDriverConnections));
        assert_eq!(info_type_from_raw(2), Some(InfoType::DataSourceName));
        assert_eq!(info_type_from_raw(77), Some(InfoType::DriverOdbcVer));
        assert_eq!(info_type_from_raw(114), Some(InfoType::CatalogLocation));
        assert_eq!(info_type_from_raw(10023), Some(InfoType::AsyncDbcFunctions));
    }

    #[test]
    fn info_type_from_raw_invalid() {
        assert_eq!(info_type_from_raw(3), None);
        assert_eq!(info_type_from_raw(24), None); // Not in our match (handled via get_info_raw)
        assert_eq!(info_type_from_raw(9999), None);
    }

    // --- c_data_type_from_raw ---

    /// Every `CDataType` variant compiled under `odbc_version_3_80`. Hand-maintained
    /// so a future addition to `odbc_sys::CDataType` that isn't also added to
    /// `c_data_type_from_raw` fails a test instead of silently staying unnamed.
    /// Excludes `odbc_version_4`-gated variants (`TypeTimeWithTimezone`,
    /// `TypeTimestampWithTimezone`), which do not compile in this feature set.
    const ALL_C_DATA_TYPE_VARIANTS: &[CDataType] = &[
        CDataType::Ard,
        CDataType::Apd,
        CDataType::UTinyInt,
        CDataType::UBigInt,
        CDataType::STinyInt,
        CDataType::SBigInt,
        CDataType::ULong,
        CDataType::UShort,
        CDataType::SLong,
        CDataType::SShort,
        CDataType::Guid,
        CDataType::WChar,
        CDataType::Bit,
        CDataType::Binary,
        CDataType::Char,
        CDataType::Numeric,
        CDataType::Float,
        CDataType::Double,
        CDataType::Date,
        CDataType::Time,
        CDataType::TimeStamp,
        CDataType::TypeDate,
        CDataType::TypeTime,
        CDataType::TypeTimestamp,
        CDataType::Default,
        CDataType::IntervalYear,
        CDataType::IntervalMonth,
        CDataType::IntervalDay,
        CDataType::IntervalHour,
        CDataType::IntervalMinute,
        CDataType::IntervalSecond,
        CDataType::IntervalYearToMonth,
        CDataType::IntervalDayToHour,
        CDataType::IntervalDayToMinute,
        CDataType::IntervalDayToSecond,
        CDataType::IntervalHourToMinute,
        CDataType::IntervalHourToSecond,
        CDataType::IntervalMinuteToSecond,
        CDataType::SsTime2,
        CDataType::SsTimestampOffset,
    ];

    #[test]
    fn every_c_data_type_variant_round_trips() {
        for &variant in ALL_C_DATA_TYPE_VARIANTS {
            assert_eq!(
                c_data_type_from_raw(variant as i16),
                Some(variant),
                "{variant:?} is not handled by c_data_type_from_raw"
            );
        }
    }

    #[test]
    fn c_data_type_valid() {
        assert_eq!(c_data_type_from_raw(-16), Some(CDataType::SLong));
        assert_eq!(c_data_type_from_raw(-8), Some(CDataType::WChar));
        assert_eq!(c_data_type_from_raw(-7), Some(CDataType::Bit));
        assert_eq!(c_data_type_from_raw(-2), Some(CDataType::Binary));
        assert_eq!(c_data_type_from_raw(1), Some(CDataType::Char));
        assert_eq!(c_data_type_from_raw(8), Some(CDataType::Double));
        assert_eq!(c_data_type_from_raw(91), Some(CDataType::TypeDate));
        assert_eq!(c_data_type_from_raw(93), Some(CDataType::TypeTimestamp));
        assert_eq!(c_data_type_from_raw(99), Some(CDataType::Default));
        // ODBC 2.x deprecated aliases must map to their 3.x equivalents
        assert_eq!(c_data_type_from_raw(4), Some(CDataType::SLong));
        assert_eq!(c_data_type_from_raw(5), Some(CDataType::SShort));
    }

    // --- interval_from_raw ---

    #[test]
    fn interval_from_raw_accepts_the_thirteen_subcodes() {
        assert_eq!(interval_from_raw(1), Some(Interval::Year));
        assert_eq!(interval_from_raw(6), Some(Interval::Second));
        assert_eq!(interval_from_raw(13), Some(Interval::MinuteToSecond));
    }

    #[test]
    fn interval_from_raw_rejects_everything_else() {
        assert_eq!(interval_from_raw(0), None);
        assert_eq!(interval_from_raw(14), None);
        assert_eq!(interval_from_raw(-1), None);
        // 101 is `SQL_INTERVAL_YEAR`, the *concise SQL type*, not a subcode.
        // Accepting it here would silently conflate the two numbering schemes.
        assert_eq!(interval_from_raw(101), None);
    }

    /// Every accepted value must return to the integer it came from. This is
    /// the property AGENTS.md requires before a conversion is worth having:
    /// `odbc_sys::Operation` and `Lock` fail it, which is why they are named
    /// constants instead.
    #[test]
    fn interval_from_raw_round_trips() {
        for raw in 1..=13_i16 {
            let converted = interval_from_raw(raw).expect("1..=13 are all valid subcodes");
            assert_eq!(converted as i16, raw);
        }
    }

    /// `SQL_C_TINYINT` is the ODBC 2.x spelling of `SQL_C_STINYINT`, exactly as
    /// `SQL_C_LONG` and `SQL_C_SHORT` are of their signed counterparts. All
    /// three default to signed, and an application still emitting one must not
    /// be refused. Pinned together so the asymmetry cannot come back.
    #[test]
    fn the_deprecated_unsuffixed_integer_spellings_normalise_to_signed() {
        assert_eq!(c_data_type_from_raw(-6), Some(CDataType::STinyInt));
        assert_eq!(c_data_type_from_raw(4), Some(CDataType::SLong));
        assert_eq!(c_data_type_from_raw(5), Some(CDataType::SShort));
    }

    #[test]
    fn c_data_type_invalid() {
        assert_eq!(c_data_type_from_raw(0), None);
        assert_eq!(c_data_type_from_raw(3), None);
        assert_eq!(c_data_type_from_raw(999), None);
        assert_eq!(c_data_type_from_raw(-999), None);
    }

    // --- param_type_from_raw ---

    /// Every `ParamType` variant compiled under `odbc_version_3_80`. Hand-maintained
    /// so a future addition to `odbc_sys::ParamType` that isn't also added to
    /// `param_type_from_raw` fails a test instead of silently staying unnamed.
    const ALL_PARAM_TYPE_VARIANTS: &[ParamType] = &[
        ParamType::Unknown,
        ParamType::Input,
        ParamType::InputOutput,
        ParamType::ResultCol,
        ParamType::Output,
        ParamType::ReturnValue,
        ParamType::InputOutputStream,
        ParamType::OutputStream,
    ];

    #[test]
    fn every_param_type_variant_round_trips() {
        for &variant in ALL_PARAM_TYPE_VARIANTS {
            assert_eq!(param_type_from_raw(variant as i16), Some(variant));
        }
    }

    #[test]
    fn param_type_valid() {
        assert_eq!(param_type_from_raw(0), Some(ParamType::Unknown));
        assert_eq!(param_type_from_raw(1), Some(ParamType::Input));
        assert_eq!(param_type_from_raw(2), Some(ParamType::InputOutput));
        assert_eq!(param_type_from_raw(3), Some(ParamType::ResultCol));
        assert_eq!(param_type_from_raw(4), Some(ParamType::Output));
        assert_eq!(param_type_from_raw(5), Some(ParamType::ReturnValue));
    }

    #[test]
    fn param_type_invalid() {
        assert_eq!(param_type_from_raw(-1), None);
        assert_eq!(param_type_from_raw(6), None);
    }

    // --- environment_attribute_from_raw ---

    #[test]
    fn environment_attribute_valid() {
        assert_eq!(
            environment_attribute_from_raw(200),
            Some(EnvironmentAttribute::OdbcVersion)
        );
        assert_eq!(
            environment_attribute_from_raw(201),
            Some(EnvironmentAttribute::ConnectionPooling)
        );
        assert_eq!(
            environment_attribute_from_raw(202),
            Some(EnvironmentAttribute::CpMatch)
        );
        assert_eq!(
            environment_attribute_from_raw(10001),
            Some(EnvironmentAttribute::OutputNts)
        );
    }

    #[test]
    fn environment_attribute_invalid() {
        assert_eq!(environment_attribute_from_raw(0), None);
        assert_eq!(environment_attribute_from_raw(199), None);
        assert_eq!(environment_attribute_from_raw(9999), None);
    }

    // --- attr_odbc_version_from_raw ---

    #[test]
    fn attr_odbc_version_valid() {
        assert_eq!(attr_odbc_version_from_raw(3), Some(AttrOdbcVersion::Odbc3));
        assert_eq!(
            attr_odbc_version_from_raw(380),
            Some(AttrOdbcVersion::Odbc3_80)
        );
    }

    /// `AttrOdbcVersion` has no `SQL_OV_ODBC2` variant — `odbc-sys` omits it
    /// deliberately, with `// Not supported by this crate` above a
    /// commented-out `SQL_OV_ODBC2 = 2` in its `attributes.rs` — so this
    /// function cannot name it and answers `None`. That is a fact about the
    /// *type*, not a ruling that core rejects ODBC 2.x: `SQLSetEnvAttr` goes
    /// through `declared_odbc_version_from_raw`, which accepts all three.
    #[test]
    fn attr_odbc_version_rejects_the_values_odbc_sys_cannot_name() {
        assert_eq!(attr_odbc_version_from_raw(0), None);
        assert_eq!(attr_odbc_version_from_raw(SQL_OV_ODBC2), None);
        assert_eq!(attr_odbc_version_from_raw(4), None);
    }

    // --- declared_odbc_version_from_raw ---

    #[test]
    fn declared_odbc_version_accepts_all_three_spec_values() {
        assert_eq!(
            declared_odbc_version_from_raw(SQL_OV_ODBC2),
            Some(DeclaredOdbcVersion::Odbc2)
        );
        assert_eq!(
            declared_odbc_version_from_raw(SQL_OV_ODBC3),
            Some(DeclaredOdbcVersion::Odbc3)
        );
        assert_eq!(
            declared_odbc_version_from_raw(SQL_OV_ODBC3_80),
            Some(DeclaredOdbcVersion::Odbc3_80)
        );
        assert_eq!(declared_odbc_version_from_raw(0), None);
        assert_eq!(declared_odbc_version_from_raw(4), None);
    }

    /// The value the Driver Manager reads back must be the one it set, so the
    /// pair has to round-trip for every variant.
    #[test]
    fn declared_odbc_version_round_trips_through_its_raw_value() {
        for version in [
            DeclaredOdbcVersion::Odbc2,
            DeclaredOdbcVersion::Odbc3,
            DeclaredOdbcVersion::Odbc3_80,
        ] {
            assert_eq!(declared_odbc_version_from_raw(version.raw()), Some(version));
        }
    }

    // --- free_stmt_option_from_raw ---

    #[test]
    fn free_stmt_option_valid() {
        assert_eq!(free_stmt_option_from_raw(0), Some(FreeStmtOption::Close));
        assert_eq!(free_stmt_option_from_raw(2), Some(FreeStmtOption::Unbind));
        assert_eq!(
            free_stmt_option_from_raw(3),
            Some(FreeStmtOption::ResetParams)
        );
    }

    #[test]
    fn free_stmt_option_invalid() {
        // 1 is SQL_DROP (handled separately), not a FreeStmtOption
        assert_eq!(free_stmt_option_from_raw(1), None);
        assert_eq!(free_stmt_option_from_raw(4), None);
    }

    // --- driver_connect_option_from_raw ---

    #[test]
    fn driver_connect_option_valid() {
        assert_eq!(
            driver_connect_option_from_raw(0),
            Some(DriverConnectOption::NoPrompt)
        );
        assert_eq!(
            driver_connect_option_from_raw(1),
            Some(DriverConnectOption::Complete)
        );
        assert_eq!(
            driver_connect_option_from_raw(2),
            Some(DriverConnectOption::Prompt)
        );
        assert_eq!(
            driver_connect_option_from_raw(3),
            Some(DriverConnectOption::CompleteRequired)
        );
    }

    #[test]
    fn driver_connect_option_invalid() {
        assert_eq!(driver_connect_option_from_raw(4), None);
        assert_eq!(driver_connect_option_from_raw(u16::MAX), None);
    }

    // --- statement_attribute_from_raw ---

    /// Every `StatementAttribute` variant compiled under `odbc_version_3_80`.
    /// Hand-maintained so a future addition to `odbc_sys::StatementAttribute`
    /// that isn't also added to `statement_attribute_from_raw` fails a test
    /// instead of silently staying unnamed. Excludes `odbc_version_4`-gated
    /// variants (`SampleSize`, `DynamicColumns`, `TypeExceptionBehaviour`,
    /// `LengthExceptionBehaviour`), which do not compile in this feature set.
    const ALL_STATEMENT_ATTRIBUTE_VARIANTS: &[StatementAttribute] = &[
        StatementAttribute::AppRowDesc,
        StatementAttribute::AppParamDesc,
        StatementAttribute::ImpRowDesc,
        StatementAttribute::ImpParamDesc,
        StatementAttribute::CursorScrollable,
        StatementAttribute::CursorSensitivity,
        StatementAttribute::AsyncEnable,
        StatementAttribute::Concurrency,
        StatementAttribute::CursorType,
        StatementAttribute::EnableAutoIpd,
        StatementAttribute::FetchBookmarkPtr,
        StatementAttribute::KeysetSize,
        StatementAttribute::MaxLength,
        StatementAttribute::MaxRows,
        StatementAttribute::NoScan,
        StatementAttribute::ParamBindOffsetPtr,
        StatementAttribute::ParamBindType,
        StatementAttribute::ParamOpterationPtr,
        StatementAttribute::ParamStatusPtr,
        StatementAttribute::ParamsProcessedPtr,
        StatementAttribute::ParamsetSize,
        StatementAttribute::QueryTimeout,
        StatementAttribute::RetrieveData,
        StatementAttribute::RowBindOffsetPtr,
        StatementAttribute::RowBindType,
        StatementAttribute::RowNumber,
        StatementAttribute::RowOperationPtr,
        StatementAttribute::RowStatusPtr,
        StatementAttribute::RowsFetchedPtr,
        StatementAttribute::RowArraySize,
        StatementAttribute::SimulateCursor,
        StatementAttribute::UseBookmarks,
        StatementAttribute::AsyncStmtEvent,
        StatementAttribute::MetadataId,
    ];

    #[test]
    fn every_statement_attribute_variant_round_trips() {
        for &variant in ALL_STATEMENT_ATTRIBUTE_VARIANTS {
            assert_eq!(statement_attribute_from_raw(variant as i32), Some(variant));
        }
    }

    // --- completion_type_from_raw ---

    #[test]
    fn completion_type_valid() {
        assert_eq!(completion_type_from_raw(0), Some(CompletionType::Commit));
        assert_eq!(completion_type_from_raw(1), Some(CompletionType::Rollback));
    }

    #[test]
    fn completion_type_invalid() {
        assert_eq!(completion_type_from_raw(-1), None);
        assert_eq!(completion_type_from_raw(2), None);
    }

    // --- fetch_orientation_from_raw ---

    #[test]
    fn fetch_orientation_valid() {
        assert_eq!(fetch_orientation_from_raw(1), Some(FetchOrientation::Next));
        assert_eq!(fetch_orientation_from_raw(2), Some(FetchOrientation::First));
        assert_eq!(fetch_orientation_from_raw(3), Some(FetchOrientation::Last));
        assert_eq!(fetch_orientation_from_raw(4), Some(FetchOrientation::Prior));
        assert_eq!(
            fetch_orientation_from_raw(5),
            Some(FetchOrientation::Absolute)
        );
        assert_eq!(
            fetch_orientation_from_raw(6),
            Some(FetchOrientation::Relative)
        );
    }

    #[test]
    fn fetch_orientation_invalid() {
        assert_eq!(fetch_orientation_from_raw(0), None);
        assert_eq!(fetch_orientation_from_raw(7), None);
        assert_eq!(fetch_orientation_from_raw(-1), None);
    }

    // --- identifier_type_from_raw ---

    #[test]
    fn identifier_type_from_raw_maps_known_values() {
        assert_eq!(
            identifier_type_from_raw(SQL_BEST_ROWID),
            Some(IdentifierType::BestRowId)
        );
        assert_eq!(
            identifier_type_from_raw(SQL_ROWVER),
            Some(IdentifierType::RowVer)
        );
        assert_eq!(identifier_type_from_raw(99), None);
    }

    // --- scope_from_raw ---

    #[test]
    fn scope_from_raw_maps_known_values() {
        assert_eq!(scope_from_raw(SQL_SCOPE_CURROW), Some(Scope::CurRow));
        assert_eq!(
            scope_from_raw(SQL_SCOPE_TRANSACTION),
            Some(Scope::Transaction)
        );
        assert_eq!(scope_from_raw(SQL_SCOPE_SESSION), Some(Scope::Session));
        assert_eq!(scope_from_raw(99), None);
        assert!(Scope::CurRow < Scope::Session);
    }

    // --- nullable_from_raw ---

    #[test]
    fn nullable_from_raw_maps_known_values() {
        assert_eq!(nullable_from_raw(0), Some(Nullable::SqlNoNulls));
        assert_eq!(nullable_from_raw(1), Some(Nullable::SqlNullable));
        assert_eq!(nullable_from_raw(2), Some(Nullable::SqlNullableUnknown));
        assert_eq!(nullable_from_raw(99), None);
    }
}
