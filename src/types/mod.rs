//! `odbc-sys` re-exports and the driver-side types built on top of them.

// Re-export odbc-sys types used throughout the driver.
// stackable-odbc-core is the driver-side convenience layer on top of odbc-sys.
// See AGENTS.md for the rationale.
pub use odbc_sys::{
    AttrOdbcVersion, CDataType, CompletionType, ConnectionAttribute, Date, Desc,
    EnvironmentAttribute, FreeStmtOption, Guid, HandleType, HeaderDiagnosticIdentifier, InfoType,
    Len, NO_TOTAL, Numeric, ParamType, Pointer, SqlDataType, SqlReturn, StatementAttribute, Time,
    Timestamp, ULen, WChar,
};

// ---------------------------------------------------------------------------
// ODBC Constants
// ---------------------------------------------------------------------------

// `constants` is a private module and this list is exhaustive, which together
// make the crate's constant surface a deliberate choice rather than a
// by-product. It replaces a `pub use constants::*` glob, under which anything
// added to that file joined the public API silently.
//
// The list is long because most of it is vocabulary core never uses itself: the
// `SQL_AT_*`, `SQL_OJ_*`, `SQL_FN_*`, `SQL_SQ_*` and similar bitmask families
// exist for *drivers* to build the values that `Backend`'s required capability
// methods return as bare integers (`alter_table_support(conn) -> u32` and its
// neighbours). Core never constructs those values, so an unreferenced constant
// here is the expected state, not a dead one — do not prune by reference count.
//
// Leaving a new constant out of this list is caught rather than missed: an
// unexported, internally-unused `pub const` in a private module is `dead_code`,
// and the clippy hook runs with `-D warnings`.
mod constants;
pub use constants::{
    ODBC_RESERVED_KEYWORDS, SQL_AF_ALL, SQL_AF_AVG, SQL_AF_COUNT, SQL_AF_DISTINCT, SQL_AF_MAX,
    SQL_AF_MIN, SQL_AF_SUM, SQL_AGGREGATE_FUNCTIONS, SQL_ALL_CATALOGS, SQL_ALL_SCHEMAS,
    SQL_ALL_TABLE_TYPES, SQL_AM_CONNECTION, SQL_AM_NONE, SQL_AM_STATEMENT,
    SQL_ASYNC_NOTIFICATION_CAPABLE, SQL_ASYNC_NOTIFICATION_NOT_CAPABLE, SQL_AT_ADD_COLUMN,
    SQL_AT_ADD_COLUMN_COLLATION, SQL_AT_ADD_COLUMN_DEFAULT, SQL_AT_ADD_COLUMN_SINGLE,
    SQL_AT_ADD_CONSTRAINT, SQL_AT_ADD_TABLE_CONSTRAINT, SQL_AT_CONSTRAINT_DEFERRABLE,
    SQL_AT_CONSTRAINT_INITIALLY_DEFERRED, SQL_AT_CONSTRAINT_INITIALLY_IMMEDIATE,
    SQL_AT_CONSTRAINT_NAME_DEFINITION, SQL_AT_CONSTRAINT_NON_DEFERRABLE, SQL_AT_DROP_COLUMN,
    SQL_AT_DROP_COLUMN_CASCADE, SQL_AT_DROP_COLUMN_DEFAULT, SQL_AT_DROP_COLUMN_RESTRICT,
    SQL_AT_DROP_TABLE_CONSTRAINT_CASCADE, SQL_AT_DROP_TABLE_CONSTRAINT_RESTRICT,
    SQL_AT_SET_COLUMN_DEFAULT, SQL_ATTR_READONLY, SQL_ATTR_READWRITE_UNKNOWN, SQL_ATTR_WRITE,
    SQL_AUTOCOMMIT_OFF, SQL_AUTOCOMMIT_ON, SQL_BEST_ROWID, SQL_CA1_NEXT, SQL_CASCADE, SQL_CB_CLOSE,
    SQL_CB_DELETE, SQL_CB_NON_NULL, SQL_CB_NULL, SQL_CB_PRESERVE, SQL_CD_FALSE, SQL_CD_TRUE,
    SQL_CL_END, SQL_CL_START, SQL_CN_ANY, SQL_CN_DIFFERENT, SQL_CN_NONE, SQL_CODE_DATE,
    SQL_CODE_TIME, SQL_CODE_TIMESTAMP, SQL_CONVERT_FUNCTIONS_FIRST, SQL_CONVERT_FUNCTIONS_LAST,
    SQL_CONVERT_GUID, SQL_CONVERT_WCHAR, SQL_CONVERT_WVARCHAR, SQL_CU_DML_STATEMENTS,
    SQL_CU_INDEX_DEFINITION, SQL_CU_PRIVILEGE_DEFINITION, SQL_CU_PROCEDURE_INVOCATION,
    SQL_CU_TABLE_DEFINITION, SQL_CURSOR_COMMIT_BEHAVIOR, SQL_CURSOR_FORWARD_ONLY,
    SQL_CURSOR_ROLLBACK_BEHAVIOR, SQL_DATA_AT_EXEC, SQL_DATABASE_NAME, SQL_DATETIME,
    SQL_DEFAULT_PARAM_SIZE, SQL_DELETE, SQL_DRIVER_ODBC_VER_STRING, SQL_DROP, SQL_ENSURE,
    SQL_FALSE, SQL_FILE_USAGE, SQL_FN_CVT_CAST, SQL_FN_CVT_CONVERT, SQL_FN_NUM_ABS,
    SQL_FN_NUM_ACOS, SQL_FN_NUM_ASIN, SQL_FN_NUM_ATAN, SQL_FN_NUM_ATAN2, SQL_FN_NUM_CEILING,
    SQL_FN_NUM_COS, SQL_FN_NUM_COT, SQL_FN_NUM_DEGREES, SQL_FN_NUM_EXP, SQL_FN_NUM_FLOOR,
    SQL_FN_NUM_LOG, SQL_FN_NUM_LOG10, SQL_FN_NUM_MOD, SQL_FN_NUM_PI, SQL_FN_NUM_POWER,
    SQL_FN_NUM_RADIANS, SQL_FN_NUM_RAND, SQL_FN_NUM_ROUND, SQL_FN_NUM_SIGN, SQL_FN_NUM_SIN,
    SQL_FN_NUM_SQRT, SQL_FN_NUM_TAN, SQL_FN_NUM_TRUNCATE, SQL_FN_STR_ASCII, SQL_FN_STR_BIT_LENGTH,
    SQL_FN_STR_CHAR, SQL_FN_STR_CHAR_LENGTH, SQL_FN_STR_CHARACTER_LENGTH, SQL_FN_STR_CONCAT,
    SQL_FN_STR_DIFFERENCE, SQL_FN_STR_INSERT, SQL_FN_STR_LCASE, SQL_FN_STR_LEFT, SQL_FN_STR_LENGTH,
    SQL_FN_STR_LOCATE, SQL_FN_STR_LOCATE_2, SQL_FN_STR_LTRIM, SQL_FN_STR_OCTET_LENGTH,
    SQL_FN_STR_POSITION, SQL_FN_STR_REPEAT, SQL_FN_STR_REPLACE, SQL_FN_STR_RIGHT, SQL_FN_STR_RTRIM,
    SQL_FN_STR_SOUNDEX, SQL_FN_STR_SPACE, SQL_FN_STR_SUBSTRING, SQL_FN_STR_UCASE,
    SQL_FN_SYS_DBNAME, SQL_FN_SYS_IFNULL, SQL_FN_SYS_USERNAME, SQL_FN_TD_CURDATE,
    SQL_FN_TD_CURRENT_DATE, SQL_FN_TD_CURRENT_TIME, SQL_FN_TD_CURRENT_TIMESTAMP, SQL_FN_TD_CURTIME,
    SQL_FN_TD_DAYNAME, SQL_FN_TD_DAYOFMONTH, SQL_FN_TD_DAYOFWEEK, SQL_FN_TD_DAYOFYEAR,
    SQL_FN_TD_EXTRACT, SQL_FN_TD_HOUR, SQL_FN_TD_MINUTE, SQL_FN_TD_MONTH, SQL_FN_TD_MONTHNAME,
    SQL_FN_TD_NOW, SQL_FN_TD_QUARTER, SQL_FN_TD_SECOND, SQL_FN_TD_TIMESTAMPADD,
    SQL_FN_TD_TIMESTAMPDIFF, SQL_FN_TD_WEEK, SQL_FN_TD_YEAR, SQL_FN_TSI_DAY,
    SQL_FN_TSI_FRAC_SECOND, SQL_FN_TSI_HOUR, SQL_FN_TSI_MINUTE, SQL_FN_TSI_MONTH,
    SQL_FN_TSI_QUARTER, SQL_FN_TSI_SECOND, SQL_FN_TSI_WEEK, SQL_FN_TSI_YEAR, SQL_GB_COLLATE,
    SQL_GB_GROUP_BY_CONTAINS_SELECT, SQL_GB_GROUP_BY_EQUALS_SELECT, SQL_GB_NO_RELATION,
    SQL_GB_NOT_SUPPORTED, SQL_GD_ANY_COLUMN, SQL_GD_ANY_ORDER, SQL_GD_BLOCK, SQL_GD_BOUND,
    SQL_IC_LOWER, SQL_IC_MIXED, SQL_IC_SENSITIVE, SQL_IC_UPPER, SQL_INDEX_ALL, SQL_INDEX_CLUSTERED,
    SQL_INDEX_HASHED, SQL_INDEX_OTHER, SQL_INDEX_UNIQUE, SQL_INSENSITIVE, SQL_INTERVAL,
    SQL_KEYWORDS, SQL_LEN_DATA_AT_EXEC_OFFSET, SQL_LIKE_ESCAPE_CLAUSE, SQL_LOCK_EXCLUSIVE,
    SQL_LOCK_NO_CHANGE, SQL_LOCK_UNLOCK, SQL_MAX_CURSOR_NAME_LEN, SQL_MAX_OPTION_STRING_VALUE,
    SQL_MAX_PROCEDURE_NAME_LEN, SQL_MULTIPLE_ACTIVE_TXN, SQL_NAMED, SQL_NC_END, SQL_NC_HIGH,
    SQL_NC_LOW, SQL_NC_START, SQL_NNC_NON_NULL, SQL_NNC_NULL, SQL_NO_ACTION, SQL_NOT_DEFERRABLE,
    SQL_NTS, SQL_NULL_DATA, SQL_NUMERIC_FUNCTIONS, SQL_ODBC_API_CONFORMANCE,
    SQL_ODBC_SAG_CLI_CONFORMANCE, SQL_ODBC_SQL_CONFORMANCE, SQL_OIC_CORE,
    SQL_OJ_ALL_COMPARISON_OPS, SQL_OJ_FULL, SQL_OJ_INNER, SQL_OJ_LEFT, SQL_OJ_NESTED,
    SQL_OJ_NOT_ORDERED, SQL_OJ_RIGHT, SQL_OUTER_JOINS, SQL_PARAM_DIAG_UNAVAILABLE, SQL_PARAM_ERROR,
    SQL_PARAM_SUCCESS, SQL_PARAM_SUCCESS_WITH_INFO, SQL_PARAM_UNUSED, SQL_PARC_BATCH,
    SQL_PARC_NO_BATCH, SQL_PAS_BATCH, SQL_PAS_NO_BATCH, SQL_PAS_NO_SELECT, SQL_PC_NOT_PSEUDO,
    SQL_PC_PSEUDO, SQL_PC_UNKNOWN, SQL_POSITION, SQL_PRED_BASIC, SQL_PRED_CHAR, SQL_PRED_NONE,
    SQL_PROCEDURE_TERM, SQL_PROCEDURES, SQL_PT_FUNCTION, SQL_PT_PROCEDURE, SQL_PT_UNKNOWN,
    SQL_QUICK, SQL_QUOTED_IDENTIFIER_CASE, SQL_REFRESH, SQL_RESTRICT, SQL_ROW_UPDATES, SQL_ROWVER,
    SQL_SC_FIPS127_2_TRANSITIONAL, SQL_SC_SQL92_ENTRY, SQL_SC_SQL92_FULL,
    SQL_SC_SQL92_INTERMEDIATE, SQL_SCOPE_CURROW, SQL_SCOPE_SESSION, SQL_SCOPE_TRANSACTION,
    SQL_SEARCHABLE, SQL_SENSITIVE, SQL_SET_DEFAULT, SQL_SET_NULL, SQL_SO_FORWARD_ONLY,
    SQL_SP_BETWEEN, SQL_SP_COMPARISON, SQL_SP_EXISTS, SQL_SP_IN, SQL_SP_ISNOTNULL, SQL_SP_ISNULL,
    SQL_SP_LIKE, SQL_SP_MATCH_FULL, SQL_SP_MATCH_PARTIAL, SQL_SP_MATCH_UNIQUE_FULL,
    SQL_SP_MATCH_UNIQUE_PARTIAL, SQL_SP_OVERLAPS, SQL_SP_QUANTIFIED_COMPARISON, SQL_SP_UNIQUE,
    SQL_SQ_COMPARISON, SQL_SQ_CORRELATED_SUBQUERIES, SQL_SQ_EXISTS, SQL_SQ_IN, SQL_SQ_QUANTIFIED,
    SQL_SQL92_PREDICATES, SQL_SQL92_RELATIONAL_JOIN_OPERATORS, SQL_SQL92_VALUE_EXPRESSIONS,
    SQL_SRJO_CORRESPONDING_CLAUSE, SQL_SRJO_CROSS_JOIN, SQL_SRJO_EXCEPT_JOIN,
    SQL_SRJO_FULL_OUTER_JOIN, SQL_SRJO_INNER_JOIN, SQL_SRJO_INTERSECT_JOIN,
    SQL_SRJO_LEFT_OUTER_JOIN, SQL_SRJO_NATURAL_JOIN, SQL_SRJO_RIGHT_OUTER_JOIN,
    SQL_SRJO_UNION_JOIN, SQL_STRING_FUNCTIONS, SQL_SU_DML_STATEMENTS, SQL_SU_INDEX_DEFINITION,
    SQL_SU_PRIVILEGE_DEFINITION, SQL_SU_PROCEDURE_INVOCATION, SQL_SU_TABLE_DEFINITION,
    SQL_SVE_CASE, SQL_SVE_CAST, SQL_SVE_COALESCE, SQL_SVE_NULLIF, SQL_SYSTEM_FUNCTIONS,
    SQL_TABLE_STAT, SQL_TABLE_TERM, SQL_TC_ALL, SQL_TC_DDL_COMMIT, SQL_TC_DDL_IGNORE, SQL_TC_DML,
    SQL_TC_NONE, SQL_TIMEDATE_FUNCTIONS, SQL_TRUE, SQL_TXN_READ_COMMITTED,
    SQL_TXN_READ_UNCOMMITTED, SQL_TXN_REPEATABLE_READ, SQL_TXN_SERIALIZABLE, SQL_U_UNION,
    SQL_U_UNION_ALL, SQL_UNNAMED, SQL_UNSPECIFIED, SQL_UPDATE,
};

// ---------------------------------------------------------------------------
// Conversions for odbc-sys types
// ---------------------------------------------------------------------------

mod conversions;
pub use conversions::*;

mod value;
pub use value::{
    ColumnDescriptor, ColumnValue, ExecuteOutcome, FetchResult, IdentifierType, Nullable,
    OutputParam, ParamDescriptor, Scope, TypeInfoRow,
};

mod column_size;
pub use column_size::{
    MaxPrecision, MaxScale, PRECISION_UNDETERMINABLE, catalog_column_size, column_size,
    resolve_precision_isize, resolve_precision_ulen,
};

pub mod info_type_shape;
pub use info_type_shape::{InfoValueKind, expected_kind};

mod version;
pub use version::{format_odbc_version, parse_dotted_version};

mod result_cols;
// Shared descriptor constructors and spec-fixed widths. Crate-internal: they
// are how `ffi/metadata.rs` and `ffi/info.rs` build the catalog result sets
// that have no `*ResultCol` enum of their own, so that every catalog result
// set derives its widths from one `CatalogResultColumnWidths`.
pub(crate) use result_cols::{
    CREATE_PARAMS_LEN, LITERAL_AFFIX_LEN, PRIVILEGE_LEN, YES_NO_LEN, character, identifier,
    integer, smallint,
};
pub use result_cols::{
    CatalogResultColumnWidths, ColumnsResultCol, DEFAULT_IDENTIFIER_LEN, ForeignKeysResultCol,
    PrimaryKeysResultCol, TablesResultCol, special_columns_columns, statistics_columns,
};

mod catalog_rows;
pub use catalog_rows::{
    ColumnPrivilegeRow, ColumnRow, ForeignKeyRow, PrimaryKeyRow, ProcedureColumnRow, ProcedureRow,
    SpecialColumnRow, StatisticsRow, TablePrivilegeRow, TableRow,
};

mod connect_params;
pub use connect_params::ConnectParams;

mod redacted;
pub use redacted::Redacted;

mod cursor_behavior;
pub use cursor_behavior::CursorBehavior;

mod query_timeout;
pub use query_timeout::QueryTimeout;

pub mod col_attr;

pub mod sql_state;
pub use sql_state::SqlState;

// ---------------------------------------------------------------------------
// InfoValue
// ---------------------------------------------------------------------------

/// A value returned by a backend for `SQLGetInfo`, tagged with the C type the
/// spec assigns that info type. The FFI layer marshals the variant into the
/// caller's buffer with the correct width.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub enum InfoValue {
    /// A character-string info value (e.g. `SQL_DBMS_NAME`).
    String(String),
    /// A 16-bit integer info value (e.g. `SQL_MAX_CONCURRENT_ACTIVITIES`).
    U16(u16),
    /// A 32-bit integer or bitmask info value (e.g. `SQL_GETDATA_EXTENSIONS`).
    U32(u32),
}
