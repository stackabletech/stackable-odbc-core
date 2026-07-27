//! ODBC function ID enum for `SQLGetFunctions`.
//!
//! These are the `SQL_API_*` values from `sql.h` and `sqlext.h`. Not provided
//! by `odbc-sys` because it focuses on types, not driver-side constants.
//!
//! Reference: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlgetfunctions-function>

/// ODBC function identifiers used in `SQLGetFunctions` bitmaps and queries.
///
/// Values sourced from `/usr/include/sql.h` and `/usr/include/sqlext.h`.
#[repr(u16)]
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
#[non_exhaustive]
pub enum FunctionId {
    // Core API (sql.h, ODBC 2.x legacy IDs)
    AllocConnect = 1, // deprecated
    AllocEnv = 2,     // deprecated
    AllocStmt = 3,    // deprecated
    BindCol = 4,
    Cancel = 5,
    ColAttribute = 6,
    Connect = 7,
    DescribeCol = 8,
    Disconnect = 9,
    Error = 10, // deprecated, superseded by SQLGetDiagRec
    ExecDirect = 11,
    Execute = 12,
    Fetch = 13,
    FreeConnect = 14, // deprecated, superseded by SQLFreeHandle
    FreeEnv = 15,     // deprecated, superseded by SQLFreeHandle
    FreeStmt = 16,
    GetCursorName = 17,
    NumResultCols = 18,
    Prepare = 19,
    RowCount = 20,
    SetCursorName = 21,
    SetParam = 22, // deprecated, superseded by SQLBindParameter
    Transact = 23, // deprecated, superseded by SQLEndTran

    // Extended API (sqlext.h)
    BulkOperations = 24,
    Columns = 40,
    DriverConnect = 41,
    GetConnectOption = 42, // deprecated, superseded by SQLGetConnectAttr
    GetData = 43,
    GetFunctions = 44,
    GetInfo = 45,
    GetStmtOption = 46, // deprecated, superseded by SQLGetStmtAttr
    GetTypeInfo = 47,
    ParamData = 48,
    PutData = 49,
    SetConnectOption = 50, // deprecated, superseded by SQLSetConnectAttr
    SetStmtOption = 51,    // deprecated, superseded by SQLSetStmtAttr
    SpecialColumns = 52,
    Statistics = 53,
    Tables = 54,
    BrowseConnect = 55,
    ColumnPrivileges = 56,
    DataSources = 57,
    DescribeParam = 58,
    ExtendedFetch = 59, // deprecated, superseded by SQLFetchScroll
    ForeignKeys = 60,
    MoreResults = 61,
    NativeSql = 62,
    NumParams = 63,
    ParamOptions = 64, // deprecated, superseded by SQLSetStmtAttr
    PrimaryKeys = 65,
    ProcedureColumns = 66,
    Procedures = 67,
    SetPos = 68,
    SetScrollOptions = 69, // deprecated, superseded by SQLSetStmtAttr
    TablePrivileges = 70,
    Drivers = 71,
    BindParameter = 72, // Deprecated ODBC 2.x functions. Present so the `SQL_API_ALL_FUNCTIONS`
    AllocHandleStd = 73,

    // ODBC 3.x API (sql.h, IDs >= 1000)
    AllocHandle = 1001,
    BindParam = 1002,
    CloseCursor = 1003,
    CopyDesc = 1004,
    EndTran = 1005,
    FreeHandle = 1006,
    GetConnectAttr = 1007,
    GetDescField = 1008,
    GetDescRec = 1009,
    GetDiagField = 1010,
    GetDiagRec = 1011,
    GetEnvAttr = 1012,
    GetStmtAttr = 1014,
    SetConnectAttr = 1016,
    SetDescField = 1017,
    SetDescRec = 1018,
    SetEnvAttr = 1019,
    SetStmtAttr = 1020,
    FetchScroll = 1021,
    CancelHandle = 1022,
}

/// `SQLGetFunctions` special values.
pub const SQL_API_ODBC3_ALL_FUNCTIONS: u16 = 999;
/// Size of the `SQL_API_ODBC3_ALL_FUNCTIONS` bitmap, in `u16` words (4000 bits).
pub const SQL_API_ODBC3_ALL_FUNCTIONS_SIZE: usize = 250;

/// Convert a raw `u16` from the ODBC ABI into a [`FunctionId`].
///
/// Returns `None` for values that are not a recognized function ID.
pub fn function_id_from_raw(value: u16) -> Option<FunctionId> {
    match value {
        1 => Some(FunctionId::AllocConnect),
        2 => Some(FunctionId::AllocEnv),
        3 => Some(FunctionId::AllocStmt),
        4 => Some(FunctionId::BindCol),
        5 => Some(FunctionId::Cancel),
        6 => Some(FunctionId::ColAttribute),
        7 => Some(FunctionId::Connect),
        8 => Some(FunctionId::DescribeCol),
        9 => Some(FunctionId::Disconnect),
        10 => Some(FunctionId::Error),
        11 => Some(FunctionId::ExecDirect),
        12 => Some(FunctionId::Execute),
        13 => Some(FunctionId::Fetch),
        14 => Some(FunctionId::FreeConnect),
        15 => Some(FunctionId::FreeEnv),
        16 => Some(FunctionId::FreeStmt),
        17 => Some(FunctionId::GetCursorName),
        18 => Some(FunctionId::NumResultCols),
        19 => Some(FunctionId::Prepare),
        20 => Some(FunctionId::RowCount),
        21 => Some(FunctionId::SetCursorName),
        22 => Some(FunctionId::SetParam),
        23 => Some(FunctionId::Transact),
        24 => Some(FunctionId::BulkOperations),
        40 => Some(FunctionId::Columns),
        41 => Some(FunctionId::DriverConnect),
        42 => Some(FunctionId::GetConnectOption),
        43 => Some(FunctionId::GetData),
        44 => Some(FunctionId::GetFunctions),
        45 => Some(FunctionId::GetInfo),
        46 => Some(FunctionId::GetStmtOption),
        47 => Some(FunctionId::GetTypeInfo),
        48 => Some(FunctionId::ParamData),
        49 => Some(FunctionId::PutData),
        50 => Some(FunctionId::SetConnectOption),
        51 => Some(FunctionId::SetStmtOption),
        52 => Some(FunctionId::SpecialColumns),
        53 => Some(FunctionId::Statistics),
        54 => Some(FunctionId::Tables),
        55 => Some(FunctionId::BrowseConnect),
        56 => Some(FunctionId::ColumnPrivileges),
        57 => Some(FunctionId::DataSources),
        58 => Some(FunctionId::DescribeParam),
        59 => Some(FunctionId::ExtendedFetch),
        60 => Some(FunctionId::ForeignKeys),
        61 => Some(FunctionId::MoreResults),
        62 => Some(FunctionId::NativeSql),
        63 => Some(FunctionId::NumParams),
        64 => Some(FunctionId::ParamOptions),
        65 => Some(FunctionId::PrimaryKeys),
        66 => Some(FunctionId::ProcedureColumns),
        67 => Some(FunctionId::Procedures),
        68 => Some(FunctionId::SetPos),
        69 => Some(FunctionId::SetScrollOptions),
        70 => Some(FunctionId::TablePrivileges),
        71 => Some(FunctionId::Drivers),
        72 => Some(FunctionId::BindParameter),
        73 => Some(FunctionId::AllocHandleStd),
        1001 => Some(FunctionId::AllocHandle),
        1002 => Some(FunctionId::BindParam),
        1003 => Some(FunctionId::CloseCursor),
        1004 => Some(FunctionId::CopyDesc),
        1005 => Some(FunctionId::EndTran),
        1006 => Some(FunctionId::FreeHandle),
        1007 => Some(FunctionId::GetConnectAttr),
        1008 => Some(FunctionId::GetDescField),
        1009 => Some(FunctionId::GetDescRec),
        1010 => Some(FunctionId::GetDiagField),
        1011 => Some(FunctionId::GetDiagRec),
        1012 => Some(FunctionId::GetEnvAttr),
        1014 => Some(FunctionId::GetStmtAttr),
        1016 => Some(FunctionId::SetConnectAttr),
        1017 => Some(FunctionId::SetDescField),
        1018 => Some(FunctionId::SetDescRec),
        1019 => Some(FunctionId::SetEnvAttr),
        1020 => Some(FunctionId::SetStmtAttr),
        1021 => Some(FunctionId::FetchScroll),
        1022 => Some(FunctionId::CancelHandle),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `SQL_API_*` value, transcribed from `/usr/include/sql.h` and
    /// `/usr/include/sqlext.h`.
    ///
    /// This exists because a hand-copied ID is invisible when wrong: it is a
    /// plausible number in a valid range. `SQLGetConnectOption` was recorded as
    /// `30` rather than `42` in `sql_get_info`'s `SQL_API_ALL_FUNCTIONS` array,
    /// so the Windows Driver Manager -- which dispatches from that array -- was
    /// told the driver did not support a function it exports, while an
    /// unassigned slot was marked present.
    const SPEC_IDS: &[(u16, FunctionId)] = &[
        (1, FunctionId::AllocConnect),
        (2, FunctionId::AllocEnv),
        (3, FunctionId::AllocStmt),
        (4, FunctionId::BindCol),
        (5, FunctionId::Cancel),
        (6, FunctionId::ColAttribute),
        (7, FunctionId::Connect),
        (8, FunctionId::DescribeCol),
        (9, FunctionId::Disconnect),
        (10, FunctionId::Error),
        (11, FunctionId::ExecDirect),
        (12, FunctionId::Execute),
        (13, FunctionId::Fetch),
        (14, FunctionId::FreeConnect),
        (15, FunctionId::FreeEnv),
        (16, FunctionId::FreeStmt),
        (17, FunctionId::GetCursorName),
        (18, FunctionId::NumResultCols),
        (19, FunctionId::Prepare),
        (20, FunctionId::RowCount),
        (21, FunctionId::SetCursorName),
        (22, FunctionId::SetParam),
        (23, FunctionId::Transact),
        (24, FunctionId::BulkOperations),
        (40, FunctionId::Columns),
        (41, FunctionId::DriverConnect),
        (42, FunctionId::GetConnectOption),
        (43, FunctionId::GetData),
        (44, FunctionId::GetFunctions),
        (45, FunctionId::GetInfo),
        (46, FunctionId::GetStmtOption),
        (47, FunctionId::GetTypeInfo),
        (48, FunctionId::ParamData),
        (49, FunctionId::PutData),
        (50, FunctionId::SetConnectOption),
        (51, FunctionId::SetStmtOption),
        (52, FunctionId::SpecialColumns),
        (53, FunctionId::Statistics),
        (54, FunctionId::Tables),
        (55, FunctionId::BrowseConnect),
        (56, FunctionId::ColumnPrivileges),
        (57, FunctionId::DataSources),
        (58, FunctionId::DescribeParam),
        (59, FunctionId::ExtendedFetch),
        (60, FunctionId::ForeignKeys),
        (61, FunctionId::MoreResults),
        (62, FunctionId::NativeSql),
        (63, FunctionId::NumParams),
        (64, FunctionId::ParamOptions),
        (65, FunctionId::PrimaryKeys),
        (66, FunctionId::ProcedureColumns),
        (67, FunctionId::Procedures),
        (68, FunctionId::SetPos),
        (69, FunctionId::SetScrollOptions),
        (70, FunctionId::TablePrivileges),
        (71, FunctionId::Drivers),
        (72, FunctionId::BindParameter),
        (73, FunctionId::AllocHandleStd),
        (1001, FunctionId::AllocHandle),
        (1002, FunctionId::BindParam),
        (1003, FunctionId::CloseCursor),
        (1004, FunctionId::CopyDesc),
        (1005, FunctionId::EndTran),
        (1006, FunctionId::FreeHandle),
        (1007, FunctionId::GetConnectAttr),
        (1008, FunctionId::GetDescField),
        (1009, FunctionId::GetDescRec),
        (1010, FunctionId::GetDiagField),
        (1011, FunctionId::GetDiagRec),
        (1012, FunctionId::GetEnvAttr),
        (1014, FunctionId::GetStmtAttr),
        (1016, FunctionId::SetConnectAttr),
        (1017, FunctionId::SetDescField),
        (1018, FunctionId::SetDescRec),
        (1019, FunctionId::SetEnvAttr),
        (1020, FunctionId::SetStmtAttr),
        (1021, FunctionId::FetchScroll),
        (1022, FunctionId::CancelHandle),
    ];

    #[test]
    fn every_function_id_matches_the_sql_headers() {
        for &(value, id) in SPEC_IDS {
            assert_eq!(id as u16, value, "{id:?} has the wrong SQL_API_* value");
        }
    }

    #[test]
    fn function_id_from_raw_round_trips_every_id() {
        // Every other enum in the crate has a round-trip test; this one did not,
        // so a variant added to the enum but forgotten in the match would have
        // been silently unrecognised on the ABI.
        for &(value, id) in SPEC_IDS {
            assert_eq!(
                function_id_from_raw(value),
                Some(id),
                "function_id_from_raw({value}) did not yield {id:?}"
            );
        }
    }

    #[test]
    fn function_id_from_raw_rejects_unassigned_values() {
        // 30 is the slot SQLGetConnectOption was wrongly recorded at.
        for value in [0u16, 25, 30, 39, 74, 1000, 1013, 1015, 1023, 9999] {
            assert_eq!(
                function_id_from_raw(value),
                None,
                "{value} is not an assigned SQL_API_* value"
            );
        }
    }
}
