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
    ExecDirect = 11,
    Execute = 12,
    Fetch = 13,
    FreeStmt = 16,
    GetCursorName = 17,
    NumResultCols = 18,
    Prepare = 19,
    RowCount = 20,
    SetCursorName = 21,

    // Extended API (sqlext.h)
    BulkOperations = 24,
    Columns = 40,
    DriverConnect = 41,
    GetData = 43,
    GetFunctions = 44,
    GetInfo = 45,
    GetTypeInfo = 47,
    ParamData = 48,
    PutData = 49,
    SpecialColumns = 52,
    Statistics = 53,
    Tables = 54,
    BrowseConnect = 55,
    ColumnPrivileges = 56,
    DataSources = 57,
    DescribeParam = 58,
    ForeignKeys = 60,
    MoreResults = 61,
    NativeSql = 62,
    NumParams = 63,
    PrimaryKeys = 65,
    ProcedureColumns = 66,
    Procedures = 67,
    SetPos = 68,
    TablePrivileges = 70,
    BindParameter = 72,

    // ODBC 3.x API (sql.h, IDs >= 1000)
    AllocHandle = 1001,
    CloseCursor = 1003,
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
        11 => Some(FunctionId::ExecDirect),
        12 => Some(FunctionId::Execute),
        13 => Some(FunctionId::Fetch),
        16 => Some(FunctionId::FreeStmt),
        17 => Some(FunctionId::GetCursorName),
        18 => Some(FunctionId::NumResultCols),
        19 => Some(FunctionId::Prepare),
        20 => Some(FunctionId::RowCount),
        21 => Some(FunctionId::SetCursorName),
        24 => Some(FunctionId::BulkOperations),
        40 => Some(FunctionId::Columns),
        41 => Some(FunctionId::DriverConnect),
        43 => Some(FunctionId::GetData),
        44 => Some(FunctionId::GetFunctions),
        45 => Some(FunctionId::GetInfo),
        47 => Some(FunctionId::GetTypeInfo),
        48 => Some(FunctionId::ParamData),
        49 => Some(FunctionId::PutData),
        52 => Some(FunctionId::SpecialColumns),
        53 => Some(FunctionId::Statistics),
        54 => Some(FunctionId::Tables),
        55 => Some(FunctionId::BrowseConnect),
        56 => Some(FunctionId::ColumnPrivileges),
        57 => Some(FunctionId::DataSources),
        58 => Some(FunctionId::DescribeParam),
        60 => Some(FunctionId::ForeignKeys),
        61 => Some(FunctionId::MoreResults),
        62 => Some(FunctionId::NativeSql),
        63 => Some(FunctionId::NumParams),
        65 => Some(FunctionId::PrimaryKeys),
        66 => Some(FunctionId::ProcedureColumns),
        67 => Some(FunctionId::Procedures),
        68 => Some(FunctionId::SetPos),
        70 => Some(FunctionId::TablePrivileges),
        72 => Some(FunctionId::BindParameter),
        1001 => Some(FunctionId::AllocHandle),
        1003 => Some(FunctionId::CloseCursor),
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
