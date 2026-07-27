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
    /// `SQL_API_SQLALLOCCONNECT` (1) — identifies `SQLAllocConnect`.
    ///
    /// Deprecated.
    AllocConnect = 1,
    /// `SQL_API_SQLALLOCENV` (2) — identifies `SQLAllocEnv`.
    ///
    /// Deprecated.
    AllocEnv = 2,
    /// `SQL_API_SQLALLOCSTMT` (3) — identifies `SQLAllocStmt`.
    ///
    /// Deprecated.
    AllocStmt = 3,
    /// `SQL_API_SQLBINDCOL` (4) — identifies `SQLBindCol`.
    BindCol = 4,
    /// `SQL_API_SQLCANCEL` (5) — identifies `SQLCancel`.
    Cancel = 5,
    /// `SQL_API_SQLCOLATTRIBUTE` (6) — identifies `SQLColAttribute`.
    ColAttribute = 6,
    /// `SQL_API_SQLCONNECT` (7) — identifies `SQLConnect`.
    Connect = 7,
    /// `SQL_API_SQLDESCRIBECOL` (8) — identifies `SQLDescribeCol`.
    DescribeCol = 8,
    /// `SQL_API_SQLDISCONNECT` (9) — identifies `SQLDisconnect`.
    Disconnect = 9,
    /// `SQL_API_SQLERROR` (10) — identifies `SQLError`.
    ///
    /// Deprecated, superseded by SQLGetDiagRec.
    Error = 10,
    /// `SQL_API_SQLEXECDIRECT` (11) — identifies `SQLExecDirect`.
    ExecDirect = 11,
    /// `SQL_API_SQLEXECUTE` (12) — identifies `SQLExecute`.
    Execute = 12,
    /// `SQL_API_SQLFETCH` (13) — identifies `SQLFetch`.
    Fetch = 13,
    /// `SQL_API_SQLFREECONNECT` (14) — identifies `SQLFreeConnect`.
    ///
    /// Deprecated, superseded by SQLFreeHandle.
    FreeConnect = 14,
    /// `SQL_API_SQLFREEENV` (15) — identifies `SQLFreeEnv`.
    ///
    /// Deprecated, superseded by SQLFreeHandle.
    FreeEnv = 15,
    /// `SQL_API_SQLFREESTMT` (16) — identifies `SQLFreeStmt`.
    FreeStmt = 16,
    /// `SQL_API_SQLGETCURSORNAME` (17) — identifies `SQLGetCursorName`.
    GetCursorName = 17,
    /// `SQL_API_SQLNUMRESULTCOLS` (18) — identifies `SQLNumResultCols`.
    NumResultCols = 18,
    /// `SQL_API_SQLPREPARE` (19) — identifies `SQLPrepare`.
    Prepare = 19,
    /// `SQL_API_SQLROWCOUNT` (20) — identifies `SQLRowCount`.
    RowCount = 20,
    /// `SQL_API_SQLSETCURSORNAME` (21) — identifies `SQLSetCursorName`.
    SetCursorName = 21,
    /// `SQL_API_SQLSETPARAM` (22) — identifies `SQLSetParam`.
    ///
    /// Deprecated, superseded by SQLBindParameter.
    SetParam = 22,
    /// `SQL_API_SQLTRANSACT` (23) — identifies `SQLTransact`.
    ///
    /// Deprecated, superseded by SQLEndTran.
    Transact = 23,

    // Extended API (sqlext.h)
    /// `SQL_API_SQLBULKOPERATIONS` (24) — identifies `SQLBulkOperations`.
    BulkOperations = 24,
    /// `SQL_API_SQLCOLUMNS` (40) — identifies `SQLColumns`.
    Columns = 40,
    /// `SQL_API_SQLDRIVERCONNECT` (41) — identifies `SQLDriverConnect`.
    DriverConnect = 41,
    /// `SQL_API_SQLGETCONNECTOPTION` (42) — identifies `SQLGetConnectOption`.
    ///
    /// Deprecated, superseded by SQLGetConnectAttr.
    GetConnectOption = 42,
    /// `SQL_API_SQLGETDATA` (43) — identifies `SQLGetData`.
    GetData = 43,
    /// `SQL_API_SQLGETFUNCTIONS` (44) — identifies `SQLGetFunctions`.
    GetFunctions = 44,
    /// `SQL_API_SQLGETINFO` (45) — identifies `SQLGetInfo`.
    GetInfo = 45,
    /// `SQL_API_SQLGETSTMTOPTION` (46) — identifies `SQLGetStmtOption`.
    ///
    /// Deprecated, superseded by SQLGetStmtAttr.
    GetStmtOption = 46,
    /// `SQL_API_SQLGETTYPEINFO` (47) — identifies `SQLGetTypeInfo`.
    GetTypeInfo = 47,
    /// `SQL_API_SQLPARAMDATA` (48) — identifies `SQLParamData`.
    ParamData = 48,
    /// `SQL_API_SQLPUTDATA` (49) — identifies `SQLPutData`.
    PutData = 49,
    /// `SQL_API_SQLSETCONNECTOPTION` (50) — identifies `SQLSetConnectOption`.
    ///
    /// Deprecated, superseded by SQLSetConnectAttr.
    SetConnectOption = 50,
    /// `SQL_API_SQLSETSTMTOPTION` (51) — identifies `SQLSetStmtOption`.
    ///
    /// Deprecated, superseded by SQLSetStmtAttr.
    SetStmtOption = 51,
    /// `SQL_API_SQLSPECIALCOLUMNS` (52) — identifies `SQLSpecialColumns`.
    SpecialColumns = 52,
    /// `SQL_API_SQLSTATISTICS` (53) — identifies `SQLStatistics`.
    Statistics = 53,
    /// `SQL_API_SQLTABLES` (54) — identifies `SQLTables`.
    Tables = 54,
    /// `SQL_API_SQLBROWSECONNECT` (55) — identifies `SQLBrowseConnect`.
    BrowseConnect = 55,
    /// `SQL_API_SQLCOLUMNPRIVILEGES` (56) — identifies `SQLColumnPrivileges`.
    ColumnPrivileges = 56,
    /// `SQL_API_SQLDATASOURCES` (57) — identifies `SQLDataSources`.
    DataSources = 57,
    /// `SQL_API_SQLDESCRIBEPARAM` (58) — identifies `SQLDescribeParam`.
    DescribeParam = 58,
    /// `SQL_API_SQLEXTENDEDFETCH` (59) — identifies `SQLExtendedFetch`.
    ///
    /// Deprecated, superseded by SQLFetchScroll.
    ExtendedFetch = 59,
    /// `SQL_API_SQLFOREIGNKEYS` (60) — identifies `SQLForeignKeys`.
    ForeignKeys = 60,
    /// `SQL_API_SQLMORERESULTS` (61) — identifies `SQLMoreResults`.
    MoreResults = 61,
    /// `SQL_API_SQLNATIVESQL` (62) — identifies `SQLNativeSql`.
    NativeSql = 62,
    /// `SQL_API_SQLNUMPARAMS` (63) — identifies `SQLNumParams`.
    NumParams = 63,
    /// `SQL_API_SQLPARAMOPTIONS` (64) — identifies `SQLParamOptions`.
    ///
    /// Deprecated, superseded by SQLSetStmtAttr.
    ParamOptions = 64,
    /// `SQL_API_SQLPRIMARYKEYS` (65) — identifies `SQLPrimaryKeys`.
    PrimaryKeys = 65,
    /// `SQL_API_SQLPROCEDURECOLUMNS` (66) — identifies `SQLProcedureColumns`.
    ProcedureColumns = 66,
    /// `SQL_API_SQLPROCEDURES` (67) — identifies `SQLProcedures`.
    Procedures = 67,
    /// `SQL_API_SQLSETPOS` (68) — identifies `SQLSetPos`.
    SetPos = 68,
    /// `SQL_API_SQLSETSCROLLOPTIONS` (69) — identifies `SQLSetScrollOptions`.
    ///
    /// Deprecated, superseded by SQLSetStmtAttr.
    SetScrollOptions = 69,
    /// `SQL_API_SQLTABLEPRIVILEGES` (70) — identifies `SQLTablePrivileges`.
    TablePrivileges = 70,
    /// `SQL_API_SQLDRIVERS` (71) — identifies `SQLDrivers`.
    Drivers = 71,
    /// `SQL_API_SQLBINDPARAMETER` (72) — identifies `SQLBindParameter`.
    ///
    /// Deprecated ODBC 2.x functions. Present so the `SQL_API_ALL_FUNCTIONS`.
    BindParameter = 72,
    /// `SQL_API_SQLALLOCHANDLESTD` (73) — identifies `SQLAllocHandleStd`.
    AllocHandleStd = 73,

    // ODBC 3.x API (sql.h, IDs >= 1000)
    /// `SQL_API_SQLALLOCHANDLE` (1001) — identifies `SQLAllocHandle`.
    AllocHandle = 1001,
    /// `SQL_API_SQLBINDPARAM` (1002) — identifies `SQLBindParam`.
    BindParam = 1002,
    /// `SQL_API_SQLCLOSECURSOR` (1003) — identifies `SQLCloseCursor`.
    CloseCursor = 1003,
    /// `SQL_API_SQLCOPYDESC` (1004) — identifies `SQLCopyDesc`.
    CopyDesc = 1004,
    /// `SQL_API_SQLENDTRAN` (1005) — identifies `SQLEndTran`.
    EndTran = 1005,
    /// `SQL_API_SQLFREEHANDLE` (1006) — identifies `SQLFreeHandle`.
    FreeHandle = 1006,
    /// `SQL_API_SQLGETCONNECTATTR` (1007) — identifies `SQLGetConnectAttr`.
    GetConnectAttr = 1007,
    /// `SQL_API_SQLGETDESCFIELD` (1008) — identifies `SQLGetDescField`.
    GetDescField = 1008,
    /// `SQL_API_SQLGETDESCREC` (1009) — identifies `SQLGetDescRec`.
    GetDescRec = 1009,
    /// `SQL_API_SQLGETDIAGFIELD` (1010) — identifies `SQLGetDiagField`.
    GetDiagField = 1010,
    /// `SQL_API_SQLGETDIAGREC` (1011) — identifies `SQLGetDiagRec`.
    GetDiagRec = 1011,
    /// `SQL_API_SQLGETENVATTR` (1012) — identifies `SQLGetEnvAttr`.
    GetEnvAttr = 1012,
    /// `SQL_API_SQLGETSTMTATTR` (1014) — identifies `SQLGetStmtAttr`.
    GetStmtAttr = 1014,
    /// `SQL_API_SQLSETCONNECTATTR` (1016) — identifies `SQLSetConnectAttr`.
    SetConnectAttr = 1016,
    /// `SQL_API_SQLSETDESCFIELD` (1017) — identifies `SQLSetDescField`.
    SetDescField = 1017,
    /// `SQL_API_SQLSETDESCREC` (1018) — identifies `SQLSetDescRec`.
    SetDescRec = 1018,
    /// `SQL_API_SQLSETENVATTR` (1019) — identifies `SQLSetEnvAttr`.
    SetEnvAttr = 1019,
    /// `SQL_API_SQLSETSTMTATTR` (1020) — identifies `SQLSetStmtAttr`.
    SetStmtAttr = 1020,
    /// `SQL_API_SQLFETCHSCROLL` (1021) — identifies `SQLFetchScroll`.
    FetchScroll = 1021,
    /// `SQL_API_SQLCANCELHANDLE` (1022) — identifies `SQLCancelHandle`.
    CancelHandle = 1022,
}

/// The [`FunctionId`]s whose C entry point `forward_ffi!` actually exports.
///
/// A driver's `Backend::get_functions` should be built from this rather than
/// hand-listed. `SQLGetFunctions` is what the Windows Driver Manager uses to
/// build its dispatch table, so naming a function whose symbol does not exist
/// gives the DM a null pointer to call.
pub const CORE_EXPORTED_FUNCTIONS: &[FunctionId] = &[
    FunctionId::AllocConnect,
    FunctionId::AllocEnv,
    FunctionId::AllocStmt,
    FunctionId::BindCol,
    FunctionId::Cancel,
    FunctionId::ColAttribute,
    FunctionId::Connect,
    FunctionId::DescribeCol,
    FunctionId::Disconnect,
    FunctionId::Error,
    FunctionId::ExecDirect,
    FunctionId::Execute,
    FunctionId::Fetch,
    FunctionId::FreeConnect,
    FunctionId::FreeEnv,
    FunctionId::FreeStmt,
    FunctionId::GetCursorName,
    FunctionId::NumResultCols,
    FunctionId::Prepare,
    FunctionId::RowCount,
    FunctionId::SetCursorName,
    FunctionId::Transact,
    FunctionId::BulkOperations,
    FunctionId::Columns,
    FunctionId::DriverConnect,
    FunctionId::GetConnectOption,
    FunctionId::GetData,
    FunctionId::GetFunctions,
    FunctionId::GetInfo,
    FunctionId::GetStmtOption,
    FunctionId::GetTypeInfo,
    FunctionId::ParamData,
    FunctionId::PutData,
    FunctionId::SetConnectOption,
    FunctionId::SetStmtOption,
    FunctionId::SpecialColumns,
    FunctionId::Statistics,
    FunctionId::Tables,
    FunctionId::BrowseConnect,
    FunctionId::ColumnPrivileges,
    FunctionId::DescribeParam,
    FunctionId::ExtendedFetch,
    FunctionId::ForeignKeys,
    FunctionId::MoreResults,
    FunctionId::NativeSql,
    FunctionId::NumParams,
    FunctionId::PrimaryKeys,
    FunctionId::ProcedureColumns,
    FunctionId::Procedures,
    FunctionId::SetPos,
    FunctionId::SetScrollOptions,
    FunctionId::TablePrivileges,
    FunctionId::BindParameter,
    FunctionId::AllocHandle,
    FunctionId::CloseCursor,
    FunctionId::EndTran,
    FunctionId::FreeHandle,
    FunctionId::GetConnectAttr,
    FunctionId::GetDescField,
    FunctionId::GetDiagField,
    FunctionId::GetDiagRec,
    FunctionId::GetEnvAttr,
    FunctionId::GetStmtAttr,
    FunctionId::SetConnectAttr,
    FunctionId::SetDescField,
    FunctionId::SetDescRec,
    FunctionId::SetEnvAttr,
    FunctionId::SetStmtAttr,
    FunctionId::FetchScroll,
];

/// The [`FunctionId`]s core knows the number for but does not export.
///
/// They exist so `function_id_from_raw` recognises every assigned `SQL_API_*`
/// value and so the ODBC 2.x `SQL_API_ALL_FUNCTIONS` array can be filled from
/// named values. A driver must not report any of them as supported.
pub const CORE_UNEXPORTED_FUNCTIONS: &[(FunctionId, &str)] = &[
    (
        FunctionId::SetParam,
        "superseded by SQLBindParameter, which is exported",
    ),
    (
        FunctionId::DataSources,
        "enumerated by the Driver Manager, never by a driver",
    ),
    (
        FunctionId::ParamOptions,
        "superseded by SQLSetStmtAttr, which is exported",
    ),
    (
        FunctionId::Drivers,
        "enumerated by the Driver Manager, never by a driver",
    ),
    (
        FunctionId::AllocHandleStd,
        "an ODBC 3.x standards-compliance entry point the DM calls, not a driver export",
    ),
    (
        FunctionId::BindParam,
        "superseded by SQLBindParameter, which is exported",
    ),
    (FunctionId::CopyDesc, "descriptors are not implemented"),
    (FunctionId::GetDescRec, "descriptors are not implemented"),
    (
        FunctionId::CancelHandle,
        "not implemented; SQLCancel is exported instead",
    ),
];
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
    fn every_function_id_is_declared_exported_or_not() {
        // The two lists together are the answer to "does this symbol exist?",
        // which `Backend::get_functions` turns into a Driver Manager dispatch
        // entry. A variant in neither list, or in both, means that question has
        // no single answer.
        for &(_, id) in SPEC_IDS {
            let exported = CORE_EXPORTED_FUNCTIONS.contains(&id);
            let unexported = CORE_UNEXPORTED_FUNCTIONS.iter().any(|(u, _)| *u == id);
            assert!(
                exported ^ unexported,
                "{id:?} must appear in exactly one of CORE_EXPORTED_FUNCTIONS \
                 (exported={exported}) and CORE_UNEXPORTED_FUNCTIONS \
                 (unexported={unexported})"
            );
        }
        assert_eq!(
            CORE_EXPORTED_FUNCTIONS.len() + CORE_UNEXPORTED_FUNCTIONS.len(),
            SPEC_IDS.len(),
            "the two lists must partition FunctionId with nothing left over"
        );
    }

    #[test]
    fn function_id_from_raw_rejects_unassigned_values() {
        // 30 sits in the middle of the assigned 2.x range without being
        // assigned itself, so it is the kind of value a mistranscribed function
        // id lands on -- `SQL_API_SQLGETCONNECTOPTION` is 42, not 30.
        for value in [0u16, 25, 30, 39, 74, 1000, 1013, 1015, 1023, 9999] {
            assert_eq!(
                function_id_from_raw(value),
                None,
                "{value} is not an assigned SQL_API_* value"
            );
        }
    }
}
