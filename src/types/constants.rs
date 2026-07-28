//! Named constants for ODBC spec-defined values.
//!
//! These mirror the definitions in the ODBC 3.x specification and in the
//! system headers `sql.h` / `sqlext.h`. They supplement `odbc-sys` for
//! values that `odbc-sys` does not expose as constants.
//!
//! Where `odbc-sys` *does* carry the value, derive from it rather than
//! restating the literal — `SQL_DATETIME` and the `InfoType`-backed constants
//! below are written that way. A restated literal is a second place for the
//! value to be wrong, and a value that only one of the two places corrects is
//! how a spec-compliance fix silently fails to take effect.

use odbc_sys::{InfoType, SqlDataType};

/// SQL_ATTR_AUTOCOMMIT: manual-commit mode — the application commits or rolls
/// back explicitly with `SQLEndTran`.
pub const SQL_AUTOCOMMIT_OFF: usize = 0;

/// SQL_ATTR_AUTOCOMMIT: autocommit mode (the ODBC default) — each statement is
/// committed as it completes.
pub const SQL_AUTOCOMMIT_ON: usize = 1;
/// Sentinel string length meaning "null-terminated string" (SQL_NTS).
pub const SQL_NTS: i32 = -3;

/// ODBC boolean false (SQL_FALSE). Used in attribute value parameters.
pub const SQL_FALSE: u32 = 0;

/// ODBC boolean true (SQL_TRUE). Used in attribute value parameters.
pub const SQL_TRUE: u32 = 1;

/// ODBC indicator value meaning the data is NULL (SQL_NULL_DATA).
/// Used in `StrLen_or_IndPtr` output parameters.
pub const SQL_NULL_DATA: isize = -1;

/// Deprecated `SQLFreeStmt` option meaning "free the statement handle" (SQL_DROP = 1).
/// Omitted from `odbc_sys::FreeStmtOption` as deprecated. The ODBC 3.x spec requires the
/// Driver Manager to map this to `SQLFreeHandle(SQL_HANDLE_STMT)` before reaching the driver,
/// but the Windows DM passes it through directly, so drivers must handle it explicitly.
pub const SQL_DROP: u16 = 1;

/// `SQL_GD_ANY_COLUMN` — `SQLGetData` can retrieve any column, not just unbound ones after bound ones.
pub const SQL_GD_ANY_COLUMN: u32 = 0x0001;
/// `SQL_GD_ANY_ORDER` — `SQLGetData` can be called for columns in any order.
pub const SQL_GD_ANY_ORDER: u32 = 0x0002;
/// `SQL_GD_BLOCK` — `SQLGetData` can be called for a row in a block cursor after a bulk fetch.
pub const SQL_GD_BLOCK: u32 = 0x0004;
/// `SQL_GD_BOUND` — `SQLGetData` can be called for bound columns in addition to unbound ones.
pub const SQL_GD_BOUND: u32 = 0x0008;

/// `SQL_TC_NONE` — transactions are not supported.
pub const SQL_TC_NONE: u32 = 0;
/// `SQL_TC_DML` — transactions are supported for DML statements (`SELECT`, `INSERT`, `UPDATE`, `DELETE`) only.
pub const SQL_TC_DML: u32 = 1;
/// `SQL_TC_ALL` — transactions are supported for all statements including DDL.
pub const SQL_TC_ALL: u32 = 2;
/// `SQL_TC_DDL_COMMIT` — transactions supported; DDL statements cause a commit.
pub const SQL_TC_DDL_COMMIT: u32 = 3;
/// `SQL_TC_DDL_IGNORE` — transactions supported; DDL statements are ignored within transactions.
pub const SQL_TC_DDL_IGNORE: u32 = 4;

/// `SQL_DATETIME` — the *verbose* type of every datetime SQL type, which is
/// what `SQL_DESC_TYPE` reports where `SQL_DESC_CONCISE_TYPE` reports
/// `SQL_TYPE_DATE` / `SQL_TYPE_TIME` / `SQL_TYPE_TIMESTAMP`.
///
/// Derived from `odbc-sys` rather than restated, per the module doc.
pub const SQL_DATETIME: i16 = SqlDataType::DATETIME.0;
/// `SQL_INTERVAL` — the *verbose* type of every interval SQL type, the
/// counterpart of [`SQL_DATETIME`] for the `SQL_INTERVAL_*` concise types.
///
/// Stated as a literal because `odbc-sys` has no `SqlDataType::INTERVAL`, only
/// the concise `EXT_INTERVAL_*` codes. This is the asymmetry the module doc's
/// rule permits: derive where `odbc-sys` carries the value, state it where it
/// does not.
pub const SQL_INTERVAL: i16 = 10;

/// `SQL_PARC_BATCH` — each parameter set in an array produces its own row count.
pub const SQL_PARC_BATCH: u32 = 1;
/// `SQL_PARC_NO_BATCH` — one row count covers the whole parameter array.
pub const SQL_PARC_NO_BATCH: u32 = 2;

/// `SQL_PAS_BATCH` — each parameter set in an array produces its own result set.
pub const SQL_PAS_BATCH: u32 = 1;
/// `SQL_PAS_NO_BATCH` — one result set covers the whole parameter array.
pub const SQL_PAS_NO_BATCH: u32 = 2;
/// `SQL_PAS_NO_SELECT` — a result-set-generating statement cannot be executed
/// with an array of parameters.
pub const SQL_PAS_NO_SELECT: u32 = 3;

/// `SQL_ASYNC_NOTIFICATION_NOT_CAPABLE` — the driver does not support
/// asynchronous execution notification.
pub const SQL_ASYNC_NOTIFICATION_NOT_CAPABLE: u32 = 0x0000_0000;
/// `SQL_ASYNC_NOTIFICATION_CAPABLE` — the driver supports asynchronous
/// execution notification.
pub const SQL_ASYNC_NOTIFICATION_CAPABLE: u32 = 0x0000_0001;

/// `SQL_PRED_NONE` / `SQL_UNSEARCHABLE` — column is not usable in a `WHERE` clause.
/// Used in the `SEARCHABLE` column of `SQLGetTypeInfo` and `SQLColAttribute`.
pub const SQL_PRED_NONE: i16 = 0;
/// `SQL_PRED_CHAR` / `SQL_LIKE_ONLY` — column is usable in a `WHERE` clause only with the `LIKE` predicate.
pub const SQL_PRED_CHAR: i16 = 1;
/// `SQL_PRED_BASIC` / `SQL_ALL_EXCEPT_LIKE` — column is usable in a `WHERE` clause with all comparison
/// operators except `LIKE`.
pub const SQL_PRED_BASIC: i16 = 2;
/// `SQL_SEARCHABLE` / `SQL_PRED_SEARCHABLE` — column is usable in a `WHERE` clause with any predicate.
pub const SQL_SEARCHABLE: i16 = 3;

/// `SQL_CODE_DATE` — `SQL_DATETIME_SUB` subtype for `SQL_TYPE_DATE` (91).
pub const SQL_CODE_DATE: i16 = 1;
/// `SQL_CODE_TIME` — `SQL_DATETIME_SUB` subtype for `SQL_TYPE_TIME` (92).
pub const SQL_CODE_TIME: i16 = 2;
/// `SQL_CODE_TIMESTAMP` — `SQL_DATETIME_SUB` subtype for `SQL_TYPE_TIMESTAMP` (93).
pub const SQL_CODE_TIMESTAMP: i16 = 3;

/// `SQL_CASCADE` — referential action: propagate the change to dependent rows.
/// Used in `UPDATE_RULE` / `DELETE_RULE` columns of `SQLForeignKeys`.
pub const SQL_CASCADE: i16 = 0;
/// `SQL_RESTRICT` — referential action: reject the change if dependent rows exist.
pub const SQL_RESTRICT: i16 = 1;
/// `SQL_SET_NULL` — referential action: set dependent foreign key columns to `NULL`.
pub const SQL_SET_NULL: i16 = 2;
/// `SQL_NO_ACTION` — referential action: no action (error raised by DBMS if dependents exist).
pub const SQL_NO_ACTION: i16 = 3;
/// `SQL_SET_DEFAULT` — referential action: set dependent foreign key columns to their default value.
pub const SQL_SET_DEFAULT: i16 = 4;

/// `SQL_NOT_DEFERRABLE` — constraint deferrability: the constraint is not deferrable.
/// Used in the `DEFERRABILITY` column of `SQLForeignKeys`.
pub const SQL_NOT_DEFERRABLE: i16 = 7;

/// `SQL_ATTR_READONLY` — column is not updateable (`SQL_DESC_UPDATABLE`).
pub const SQL_ATTR_READONLY: i16 = 0;
/// `SQL_ATTR_WRITE` — column is updateable (`SQL_DESC_UPDATABLE`).
pub const SQL_ATTR_WRITE: i16 = 1;
/// `SQL_ATTR_READWRITE_UNKNOWN` — it is unknown whether the column is updateable (`SQL_DESC_UPDATABLE`).
pub const SQL_ATTR_READWRITE_UNKNOWN: i16 = 2;

/// `SQL_GB_NOT_SUPPORTED` — `GROUP BY` is not supported.
pub const SQL_GB_NOT_SUPPORTED: u16 = 0;
/// `SQL_GB_GROUP_BY_EQUALS_SELECT` — all non-aggregated `SELECT` columns must appear in `GROUP BY`.
pub const SQL_GB_GROUP_BY_EQUALS_SELECT: u16 = 1;
/// `SQL_GB_GROUP_BY_CONTAINS_SELECT` — `GROUP BY` must contain all non-aggregated `SELECT` columns but may include additional ones.
pub const SQL_GB_GROUP_BY_CONTAINS_SELECT: u16 = 2;
/// `SQL_GB_NO_RELATION` — `GROUP BY` and `SELECT` are independent; extra or missing columns are allowed in either.
pub const SQL_GB_NO_RELATION: u16 = 3;
/// `SQL_GB_COLLATE` — a `COLLATE` clause may be specified at the end of each
/// grouping column. Returned by a SQL-92 Full level-conformant driver.
pub const SQL_GB_COLLATE: u16 = 4;

// --- SQL_CONCAT_NULL_BEHAVIOR (22) values ---

/// `SQL_CB_NULL` — concatenating a NULL character column with a non-NULL one
/// yields NULL. Returned by a SQL-92 Entry level-conformant driver.
pub const SQL_CB_NULL: u16 = 0x0000;
/// `SQL_CB_NON_NULL` — the result is the concatenation of the non-NULL
/// column or columns.
pub const SQL_CB_NON_NULL: u16 = 0x0001;

// --- SQL_CORRELATION_NAME (74) values ---

/// `SQL_CN_NONE` — table correlation names are not supported.
pub const SQL_CN_NONE: u16 = 0x0000;
/// `SQL_CN_DIFFERENT` — correlation names are supported but must differ from
/// the names of the tables they represent.
pub const SQL_CN_DIFFERENT: u16 = 0x0001;
/// `SQL_CN_ANY` — correlation names are supported and may be any valid
/// user-defined name. Returned by a SQL-92 Entry level-conformant driver.
pub const SQL_CN_ANY: u16 = 0x0002;

// --- SQL_NON_NULLABLE_COLUMNS (75) values ---

/// `SQL_NNC_NULL` — all columns must be nullable; the data source does not
/// support the `NOT NULL` column constraint.
pub const SQL_NNC_NULL: u16 = 0x0000;
/// `SQL_NNC_NON_NULL` — the data source supports the `NOT NULL` column
/// constraint in `CREATE TABLE`. Returned by a SQL-92 Entry level-conformant
/// driver.
pub const SQL_NNC_NON_NULL: u16 = 0x0001;

/// `SQL_IC_UPPER` — identifiers are case-insensitive and stored in upper case.
pub const SQL_IC_UPPER: u16 = 1;
/// `SQL_IC_LOWER` — identifiers are case-insensitive and stored in lower case.
pub const SQL_IC_LOWER: u16 = 2;
/// `SQL_IC_SENSITIVE` — identifiers are case-sensitive and stored in mixed case.
pub const SQL_IC_SENSITIVE: u16 = 3;
/// `SQL_IC_MIXED` — identifiers are case-insensitive and stored in mixed case.
pub const SQL_IC_MIXED: u16 = 4;
// SQL_NULL_COLLATION (85) values. HIGH/LOW are defined in sql.h, START/END in
// sqlext.h; all four are values of the same info type. HIGH and LOW depend on
// the ASC/DESC keywords, START and END override them.

/// `SQL_NC_HIGH` — NULL values sort at the high end of the result set (last in ASC order).
pub const SQL_NC_HIGH: u16 = 0;
/// `SQL_NC_LOW` — NULL values sort at the low end of the result set (first in ASC order).
pub const SQL_NC_LOW: u16 = 1;
/// `SQL_NC_START` — NULL values sort at the start of the result set, regardless
/// of the `ASC` / `DESC` keywords.
pub const SQL_NC_START: u16 = 0x0002;
/// `SQL_NC_END` — NULL values sort at the end of the result set, regardless of
/// the `ASC` / `DESC` keywords.
pub const SQL_NC_END: u16 = 0x0004;

/// `SQL_INSENSITIVE` — cursors are insensitive to changes made by any other transaction or cursor.
pub const SQL_INSENSITIVE: u16 = 1;
/// `SQL_SENSITIVE` — cursors reflect changes made by other transactions or other cursors.
pub const SQL_SENSITIVE: u16 = 2;

/// `SQL_TXN_READ_UNCOMMITTED` — dirty reads, non-repeatable reads, and phantoms are possible.
pub const SQL_TXN_READ_UNCOMMITTED: u32 = 0x0000_0001;
/// `SQL_TXN_READ_COMMITTED` — dirty reads are not possible; non-repeatable reads and phantoms are possible.
pub const SQL_TXN_READ_COMMITTED: u32 = 0x0000_0002;
/// `SQL_TXN_REPEATABLE_READ` — dirty reads and non-repeatable reads are not possible; phantoms are possible.
pub const SQL_TXN_REPEATABLE_READ: u32 = 0x0000_0004;
/// `SQL_TXN_SERIALIZABLE` — transactions are serializable; dirty reads, non-repeatable reads, and phantoms are not possible.
pub const SQL_TXN_SERIALIZABLE: u32 = 0x0000_0008;

/// `SQL_SO_FORWARD_ONLY` — only forward-scrolling cursors are supported.
pub const SQL_SO_FORWARD_ONLY: u32 = 0x0000_0001;

/// `SQL_FN_CVT_CONVERT` — the `CONVERT` scalar function is supported.
pub const SQL_FN_CVT_CONVERT: u32 = 0x0000_0001;
/// `SQL_FN_CVT_CAST` — the `CAST` expression is supported.
pub const SQL_FN_CVT_CAST: u32 = 0x0000_0002;

/// `SQL_CONVERT_BIGINT` — the first `SQL_CONVERT_*` info type in the
/// contiguous per-source-type conversion-support block (`sqlext.h`: 53
/// through 71, `SQL_CONVERT_BIGINT` .. `SQL_CONVERT_LONGVARBINARY`). Each
/// info type in this block is a bitmask of which *other* SQL types a source
/// column of that type can be `CONVERT`ed/`CAST`ed to.
///
/// Not in `odbc-sys::InfoType`, which only has named variants for the
/// non-per-type info types among 48-73 (`ConvertFunctions` = 48,
/// `NumericFunctions` = 49, ..., `Integrity` = 73); the 53-71 range itself,
/// and `SQL_CONVERT_GUID` (173, see below), have no `InfoType` variants at
/// all and must be matched as raw `u16` values via
/// [`crate::backend::Backend::get_info_raw`].
///
/// This is deliberately distinct from `SQL_CONVERT_FUNCTIONS` (48,
/// `InfoType::ConvertFunctions`, a bitmask of `SQL_FN_CVT_*`) and from
/// `SQL_NUMERIC_FUNCTIONS`/`SQL_STRING_FUNCTIONS`/`SQL_SYSTEM_FUNCTIONS`/
/// `SQL_TIMEDATE_FUNCTIONS` (49-52, scalar-function bitmasks unrelated to
/// `CONVERT`/`CAST` at all); see `info_type_default_response` in
/// `stackable-odbc-core/src/ffi/info.rs`, which must match each of these codes
/// individually, not as one homogeneous "all conversions supported" 48-73
/// block.
pub const SQL_CONVERT_FUNCTIONS_FIRST: u16 = 53;
/// `SQL_CONVERT_LONGVARBINARY` — the last info type in the contiguous
/// per-source-type conversion-support block. See
/// [`SQL_CONVERT_FUNCTIONS_FIRST`] for the full explanation.
pub const SQL_CONVERT_FUNCTIONS_LAST: u16 = 71;
/// `SQL_CONVERT_GUID` (173) — a genuine per-source-type conversion-support
/// info type (see [`SQL_CONVERT_FUNCTIONS_FIRST`]), but numbered outside the
/// contiguous 53-71 block (`sqlext.h` places it far later, alongside
/// `SQL_ODBC_SQL_OPT_IEF`/`SQL_INTEGRITY`'s renumbering history) rather than
/// immediately after `SQL_CONVERT_LONGVARBINARY`.
pub const SQL_CONVERT_GUID: u16 = 173;

/// `SQL_CONVERT_WCHAR` — the first info type in a *second* contiguous
/// per-source-type conversion-support block (`sqlext.h`: 122 through 126,
/// `SQL_CONVERT_WCHAR` .. `SQL_CONVERT_WVARCHAR`). Same shape and meaning as
/// [`SQL_CONVERT_FUNCTIONS_FIRST`]'s block (a bitmask of which *other* SQL
/// types a source column of this type can be `CONVERT`ed/`CAST`ed to), just
/// numbered in a separate range because these five were added later (ODBC
/// 3.x Unicode types and SQL-92 interval types) than the original 1.0 block.
///
/// Not in `odbc_sys::InfoType`, same as the 53-71 block and
/// [`SQL_CONVERT_GUID`]; must be matched as a raw `u16` value.
///
/// These five codes are genuine `SQL_CONVERT_*` info types that fall outside
/// the 53-71/173 range, so `info_type_default_response`'s convert-range check
/// must cover this range too: if any of them resolves to `0`, the Windows
/// Driver Manager blocks `SQLGetData` with `HYC00` (`AGENTS.md`'s Windows
/// Driver Manager compatibility checklist).
pub const SQL_CONVERT_WCHAR: u16 = 122;
/// `SQL_CONVERT_WVARCHAR` — the last info type in the 122-126 block. See
/// [`SQL_CONVERT_WCHAR`] for the full explanation.
pub const SQL_CONVERT_WVARCHAR: u16 = 126;

/// `SQL_SC_SQL92_ENTRY` — entry level SQL-92 conformance.
pub const SQL_SC_SQL92_ENTRY: u32 = 0x0000_0001;
/// `SQL_SC_FIPS127_2_TRANSITIONAL` — FIPS 127-2 transitional level conformance.
pub const SQL_SC_FIPS127_2_TRANSITIONAL: u32 = 0x0000_0002;
/// `SQL_SC_SQL92_INTERMEDIATE` — intermediate level SQL-92 conformance.
pub const SQL_SC_SQL92_INTERMEDIATE: u32 = 0x0000_0004;
/// `SQL_SC_SQL92_FULL` — full level SQL-92 conformance.
pub const SQL_SC_SQL92_FULL: u32 = 0x0000_0008;

// --- SQL_TIMEDATE_ADD_INTERVALS (109) / SQL_TIMEDATE_DIFF_INTERVALS (110) ---
// The interval units the `TIMESTAMPADD` and `TIMESTAMPDIFF` scalar functions
// accept. A backend claiming `SQL_FN_TD_TIMESTAMPADD` or
// `SQL_FN_TD_TIMESTAMPDIFF` in `SQL_TIMEDATE_FUNCTIONS` must also name the
// units here; see `Backend::timedate_add_intervals`.

/// `SQL_FN_TSI_FRAC_SECOND` — the `SQL_TSI_FRAC_SECOND` interval is supported.
pub const SQL_FN_TSI_FRAC_SECOND: u32 = 0x0000_0001;
/// `SQL_FN_TSI_SECOND` — the `SQL_TSI_SECOND` interval is supported.
pub const SQL_FN_TSI_SECOND: u32 = 0x0000_0002;
/// `SQL_FN_TSI_MINUTE` — the `SQL_TSI_MINUTE` interval is supported.
pub const SQL_FN_TSI_MINUTE: u32 = 0x0000_0004;
/// `SQL_FN_TSI_HOUR` — the `SQL_TSI_HOUR` interval is supported.
pub const SQL_FN_TSI_HOUR: u32 = 0x0000_0008;
/// `SQL_FN_TSI_DAY` — the `SQL_TSI_DAY` interval is supported.
pub const SQL_FN_TSI_DAY: u32 = 0x0000_0010;
/// `SQL_FN_TSI_WEEK` — the `SQL_TSI_WEEK` interval is supported.
pub const SQL_FN_TSI_WEEK: u32 = 0x0000_0020;
/// `SQL_FN_TSI_MONTH` — the `SQL_TSI_MONTH` interval is supported.
pub const SQL_FN_TSI_MONTH: u32 = 0x0000_0040;
/// `SQL_FN_TSI_QUARTER` — the `SQL_TSI_QUARTER` interval is supported.
pub const SQL_FN_TSI_QUARTER: u32 = 0x0000_0080;
/// `SQL_FN_TSI_YEAR` — the `SQL_TSI_YEAR` interval is supported.
pub const SQL_FN_TSI_YEAR: u32 = 0x0000_0100;

/// `SQL_OIC_CORE` — ODBC 3.x core interface conformance.
pub const SQL_OIC_CORE: u32 = 1;

/// `SQL_AM_NONE` — asynchronous execution is not supported.
pub const SQL_AM_NONE: u32 = 0;
/// `SQL_AM_CONNECTION` — connection-level asynchronous execution is supported (`SQLSetConnectAttr SQL_ATTR_ASYNC_ENABLE`).
pub const SQL_AM_CONNECTION: u32 = 1;
/// `SQL_AM_STATEMENT` — statement-level asynchronous execution is supported (`SQLSetStmtAttr SQL_ATTR_ASYNC_ENABLE`).
pub const SQL_AM_STATEMENT: u32 = 2;

/// `SQL_CA1_NEXT` — `SQL_FETCH_NEXT` orientation is supported in `SQLFetchScroll`.
pub const SQL_CA1_NEXT: u32 = 0x0000_0001;

/// `SQL_SQ_COMPARISON` — subqueries are supported in comparison predicates.
pub const SQL_SQ_COMPARISON: u32 = 0x0000_0001;
/// `SQL_SQ_EXISTS` — subqueries are supported in `EXISTS` predicates.
pub const SQL_SQ_EXISTS: u32 = 0x0000_0002;
/// `SQL_SQ_IN` — subqueries are supported in `IN` predicates.
pub const SQL_SQ_IN: u32 = 0x0000_0004;
/// `SQL_SQ_QUANTIFIED` — subqueries are supported in quantified predicates (`ANY`, `ALL`).
pub const SQL_SQ_QUANTIFIED: u32 = 0x0000_0008;
/// `SQL_SQ_CORRELATED_SUBQUERIES` — correlated subqueries are supported.
pub const SQL_SQ_CORRELATED_SUBQUERIES: u32 = 0x0000_0010;

/// `SQL_U_UNION` — the `UNION` clause is supported.
pub const SQL_U_UNION: u32 = 0x0000_0001;
/// `SQL_U_UNION_ALL` — the `UNION ALL` clause is supported.
pub const SQL_U_UNION_ALL: u32 = 0x0000_0002;

// --- SQL_ALTER_TABLE (86) flags ---
// The low three bits are defined in sql.h; every other bit is defined in
// sqlext.h. Each header comments out the other's block and defers to it.
//
// Only two of the three are ODBC 2.x leftovers. `SQL_AT_ADD_COLUMN` and
// `SQL_AT_DROP_COLUMN` do not appear in the ODBC 3.0 SQL_ALTER_TABLE value
// list at all — the 3.x replacements are `SQL_AT_ADD_COLUMN_SINGLE` and the
// `SQL_AT_DROP_COLUMN_*` pair. `SQL_AT_ADD_CONSTRAINT` is *not* deprecated:
// the 3.0 list carries it, and a data source that accepts
// `ADD COLUMN f integer NOT NULL` needs exactly that bit.

/// `SQL_AT_ADD_COLUMN` — the `<add column>` clause is supported (sql.h).
/// An ODBC 2.x name, absent from the ODBC 3.0 `SQL_ALTER_TABLE` value list;
/// a 3.x driver reports [`SQL_AT_ADD_COLUMN_SINGLE`] instead.
pub const SQL_AT_ADD_COLUMN: u32 = 0x0000_0001;
/// `SQL_AT_DROP_COLUMN` — the `<drop column>` clause is supported (sql.h).
/// An ODBC 2.x name, absent from the ODBC 3.0 `SQL_ALTER_TABLE` value list; a
/// 3.x driver reports [`SQL_AT_DROP_COLUMN_CASCADE`] or
/// [`SQL_AT_DROP_COLUMN_RESTRICT`] instead.
pub const SQL_AT_DROP_COLUMN: u32 = 0x0000_0002;
/// `SQL_AT_ADD_CONSTRAINT` — the `<add column>` clause is supported with the
/// facility to specify column constraints (FIPS Transitional level, sql.h).
///
/// Live in ODBC 3.0, unlike the two above: the 3.0 `SQL_ALTER_TABLE` value
/// list names it, and a data source accepting `ADD COLUMN <col> <type> NOT
/// NULL` must set this bit. Setting it is also what makes the four
/// `SQL_AT_CONSTRAINT_*` deferrability bits below meaningful.
pub const SQL_AT_ADD_CONSTRAINT: u32 = 0x0000_0008;

/// `SQL_AT_ADD_COLUMN_SINGLE` — `ALTER TABLE ... ADD COLUMN` is supported for
/// a single column (FIPS Transitional level).
pub const SQL_AT_ADD_COLUMN_SINGLE: u32 = 0x0000_0020;
/// `SQL_AT_ADD_COLUMN_DEFAULT` — the `<default value>` clause is supported on
/// `ADD COLUMN` (FIPS Transitional level).
pub const SQL_AT_ADD_COLUMN_DEFAULT: u32 = 0x0000_0040;
/// `SQL_AT_ADD_COLUMN_COLLATION` — the `<column collation>` clause is supported
/// on `ADD COLUMN` (FIPS Full level).
pub const SQL_AT_ADD_COLUMN_COLLATION: u32 = 0x0000_0080;
/// `SQL_AT_SET_COLUMN_DEFAULT` — the `<alter column> <set column default>`
/// clause is supported (FIPS Intermediate level).
pub const SQL_AT_SET_COLUMN_DEFAULT: u32 = 0x0000_0100;
/// `SQL_AT_DROP_COLUMN_DEFAULT` — the `<alter column> <drop column default>`
/// clause is supported (FIPS Intermediate level).
pub const SQL_AT_DROP_COLUMN_DEFAULT: u32 = 0x0000_0200;
/// `SQL_AT_DROP_COLUMN_CASCADE` — the `<drop column> CASCADE` clause is
/// supported (FIPS Transitional level).
pub const SQL_AT_DROP_COLUMN_CASCADE: u32 = 0x0000_0400;
/// `SQL_AT_DROP_COLUMN_RESTRICT` — the `<drop column> RESTRICT` clause is
/// supported (FIPS Transitional level).
pub const SQL_AT_DROP_COLUMN_RESTRICT: u32 = 0x0000_0800;
/// `SQL_AT_ADD_TABLE_CONSTRAINT` — the `<add table constraint>` clause is
/// supported (FIPS Transitional level).
pub const SQL_AT_ADD_TABLE_CONSTRAINT: u32 = 0x0000_1000;
/// `SQL_AT_DROP_TABLE_CONSTRAINT_CASCADE` — the
/// `<drop table constraint> CASCADE` clause is supported (FIPS Transitional
/// level).
pub const SQL_AT_DROP_TABLE_CONSTRAINT_CASCADE: u32 = 0x0000_2000;
/// `SQL_AT_DROP_TABLE_CONSTRAINT_RESTRICT` — the
/// `<drop table constraint> RESTRICT` clause is supported (FIPS Transitional
/// level).
pub const SQL_AT_DROP_TABLE_CONSTRAINT_RESTRICT: u32 = 0x0000_4000;
/// `SQL_AT_CONSTRAINT_NAME_DEFINITION` — the `<constraint name definition>`
/// clause is supported for naming column and table constraints (FIPS
/// Intermediate level).
pub const SQL_AT_CONSTRAINT_NAME_DEFINITION: u32 = 0x0000_8000;
/// `SQL_AT_CONSTRAINT_INITIALLY_DEFERRED` — the `INITIALLY DEFERRED`
/// constraint attribute is supported (FIPS Full level). Only meaningful when
/// [`SQL_AT_ADD_CONSTRAINT`] is also set.
pub const SQL_AT_CONSTRAINT_INITIALLY_DEFERRED: u32 = 0x0001_0000;
/// `SQL_AT_CONSTRAINT_INITIALLY_IMMEDIATE` — the `INITIALLY IMMEDIATE`
/// constraint attribute is supported (FIPS Full level). Only meaningful when
/// [`SQL_AT_ADD_CONSTRAINT`] is also set.
pub const SQL_AT_CONSTRAINT_INITIALLY_IMMEDIATE: u32 = 0x0002_0000;
/// `SQL_AT_CONSTRAINT_DEFERRABLE` — the `DEFERRABLE` constraint attribute is
/// supported (FIPS Full level). Only meaningful when
/// [`SQL_AT_ADD_CONSTRAINT`] is also set.
pub const SQL_AT_CONSTRAINT_DEFERRABLE: u32 = 0x0004_0000;
/// `SQL_AT_CONSTRAINT_NON_DEFERRABLE` — the `NOT DEFERRABLE` constraint
/// attribute is supported (FIPS Full level). Only meaningful when
/// [`SQL_AT_ADD_CONSTRAINT`] is also set.
pub const SQL_AT_CONSTRAINT_NON_DEFERRABLE: u32 = 0x0008_0000;

// --- SQL_SCHEMA_USAGE (91) flags (sqlext.h: aliases of SQL_OU_*) ---
/// `SQL_SU_DML_STATEMENTS` — a schema name can be used in a DML statement
/// (`SELECT`/`INSERT`/`UPDATE`/`DELETE`).
pub const SQL_SU_DML_STATEMENTS: u32 = 0x0000_0001;
/// `SQL_SU_PROCEDURE_INVOCATION` — a schema name can be used in a procedure
/// invocation (the `{CALL ...}` escape / `CALL` statement).
pub const SQL_SU_PROCEDURE_INVOCATION: u32 = 0x0000_0002;
/// `SQL_SU_TABLE_DEFINITION` — a schema name can be used in a table
/// definition statement (`CREATE`/`ALTER`/`DROP TABLE`).
pub const SQL_SU_TABLE_DEFINITION: u32 = 0x0000_0004;
/// `SQL_SU_INDEX_DEFINITION` — a schema name can be used in an index
/// definition statement (`CREATE`/`DROP INDEX`).
pub const SQL_SU_INDEX_DEFINITION: u32 = 0x0000_0008;
/// `SQL_SU_PRIVILEGE_DEFINITION` — a schema name can be used in a privilege
/// definition statement (`GRANT`/`REVOKE`).
pub const SQL_SU_PRIVILEGE_DEFINITION: u32 = 0x0000_0010;

// --- SQL_CATALOG_USAGE (92) flags (sqlext.h: aliases of SQL_QU_*) ---
/// `SQL_CU_DML_STATEMENTS` — a catalog name can be used in a DML statement.
pub const SQL_CU_DML_STATEMENTS: u32 = 0x0000_0001;
/// `SQL_CU_PROCEDURE_INVOCATION` — a catalog name can be used in a procedure
/// invocation.
pub const SQL_CU_PROCEDURE_INVOCATION: u32 = 0x0000_0002;
/// `SQL_CU_TABLE_DEFINITION` — a catalog name can be used in a table
/// definition statement.
pub const SQL_CU_TABLE_DEFINITION: u32 = 0x0000_0004;
/// `SQL_CU_INDEX_DEFINITION` — a catalog name can be used in an index
/// definition statement.
pub const SQL_CU_INDEX_DEFINITION: u32 = 0x0000_0008;
/// `SQL_CU_PRIVILEGE_DEFINITION` — a catalog name can be used in a privilege
/// definition statement.
pub const SQL_CU_PRIVILEGE_DEFINITION: u32 = 0x0000_0010;

// --- SQL_CATALOG_LOCATION (114) values (sqlext.h: aliases of SQL_QL_*) ---
/// `SQL_CL_START` — the catalog name appears at the start of a qualified
/// table name (e.g. `catalog.schema.table`).
pub const SQL_CL_START: u16 = 0x0001;
/// `SQL_CL_END` — the catalog name appears at the end of a qualified table
/// name (e.g. `table.catalog`).
pub const SQL_CL_END: u16 = 0x0002;

// --- SQL_OJ_CAPABILITIES (115) flags (sql.h) ---
/// `SQL_OJ_LEFT` — `LEFT OUTER JOIN` is supported.
pub const SQL_OJ_LEFT: u32 = 0x0000_0001;
/// `SQL_OJ_RIGHT` — `RIGHT OUTER JOIN` is supported.
pub const SQL_OJ_RIGHT: u32 = 0x0000_0002;
/// `SQL_OJ_FULL` — `FULL OUTER JOIN` is supported.
pub const SQL_OJ_FULL: u32 = 0x0000_0004;
/// `SQL_OJ_NESTED` — nested outer joins are supported.
pub const SQL_OJ_NESTED: u32 = 0x0000_0008;
/// `SQL_OJ_NOT_ORDERED` — the columns in the `ON` clause of an outer join do
/// not have to be in the same order as their respective table names.
pub const SQL_OJ_NOT_ORDERED: u32 = 0x0000_0010;
/// `SQL_OJ_INNER` — the inner table (the one that does not have every row
/// returned) can also be used in an inner join.
pub const SQL_OJ_INNER: u32 = 0x0000_0020;
/// `SQL_OJ_ALL_COMPARISON_OPS` — the comparison operator in the `ON` clause
/// can be any of the ODBC comparison operators, not just `=`.
pub const SQL_OJ_ALL_COMPARISON_OPS: u32 = 0x0000_0040;

/// `SQL_MAX_CURSOR_NAME_LEN` (`SQLGetInfo` `InfoType::MaxCursorNameLen`) —
/// the maximum length of a cursor name the driver itself assigns
/// (`SQLSetCursorName`/`SQLGetCursorName`).
///
/// Deliberately **not** one of [`crate::types::CatalogResultColumnWidths`]'s
/// widths: every other `SQL_MAX_*_NAME_LEN` info type (column, schema,
/// catalog, table, plain identifier) bounds a *data-source* identifier,
/// something the backend's catalog actually stores and could, in principle,
/// impose a shorter limit on (see `CatalogResultColumnWidths::identifier_len`'s
/// own doc for PostgreSQL's 63-character example). A cursor name is not a
/// data-source object at all (the ODBC application invents it, and the
/// Driver Manager/driver only need to remember it for the lifetime of the
/// statement), so it is an ODBC-level convention rather than a per-backend
/// catalog limit, and 128 (the spec's own suggested default) is used
/// unconditionally instead of being threaded through the widths struct.
pub const SQL_MAX_CURSOR_NAME_LEN: u16 = 128;

/// Default column size reported by `SQLDescribeParam` when the driver cannot determine
/// the actual parameter type. 4000 is the conventional fallback used by most ODBC drivers,
/// large enough for practical VARCHAR values without implying unlimited length.
pub const SQL_DEFAULT_PARAM_SIZE: u32 = 4000;

/// Maximum byte length of a string value returned by a deprecated ODBC 2.x
/// `SQLGetConnectOption` / `SQLGetStmtOption` call (`SQL_MAX_OPTION_STRING_VALUE`).
/// The 2.x ABI provides no `buffer_length` parameter, so the spec fixes the limit at 256.
pub const SQL_MAX_OPTION_STRING_VALUE: i32 = 256;

/// `SQL_DATA_AT_EXEC` — indicator value signalling that parameter data will be
/// sent via `SQLPutData` at execution time.
pub const SQL_DATA_AT_EXEC: isize = -2;

/// Offset used by the `SQL_LEN_DATA_AT_EXEC(length)` macro.
/// `SQL_LEN_DATA_AT_EXEC(len)` = `-(len) + SQL_LEN_DATA_AT_EXEC_OFFSET`.
pub const SQL_LEN_DATA_AT_EXEC_OFFSET: isize = -100;

// ---------------------------------------------------------------------------
// SQLSetPos operation and lock type codes (sql.h)
// ---------------------------------------------------------------------------
//
// These restate values that `odbc_sys::Operation` and `odbc_sys::Lock` also
// carry, which the crate's odbc-sys rule would normally forbid. The exception
// is deliberate: both are newtype structs over a *private* `i16` with no
// accessor, no `From`, and no `#[repr]` enum to cast through, so an
// `odbc_sys::Operation` can be compared against its own associated constants
// and used for nothing else. The raw `SQLUSMALLINT` these functions actually
// receive cannot be produced from, or recovered out of, either type. Until
// odbc-sys exposes the inner value these are the only usable spelling.

/// `SQL_POSITION` — position the cursor on the specified row.
pub const SQL_POSITION: u16 = 0;
/// `SQL_REFRESH` — refresh the current row from the data source.
pub const SQL_REFRESH: u16 = 1;
/// `SQL_UPDATE` — update the current row with values from bound buffers.
pub const SQL_UPDATE: u16 = 2;
/// `SQL_DELETE` — delete the current row.
pub const SQL_DELETE: u16 = 3;

/// `SQL_LOCK_NO_CHANGE` — leave the lock state unchanged.
pub const SQL_LOCK_NO_CHANGE: u16 = 0;
/// `SQL_LOCK_EXCLUSIVE` — lock the row exclusively.
pub const SQL_LOCK_EXCLUSIVE: u16 = 1;
/// `SQL_LOCK_UNLOCK` — unlock the row.
pub const SQL_LOCK_UNLOCK: u16 = 2;

// `SQLBulkOperations`'s operation codes are deliberately absent, unlike the two
// groups above: `odbc_sys::BulkOperation` is a `#[repr(u16)]` enum, so it casts
// to the ABI width and is usable end to end. Convert with
// `bulk_operation_from_raw`; name a value with `BulkOperation::Add as i16`.
//
// `SQL_DIAG_MESSAGE_TEXT` is likewise absent: it is
// `odbc_sys::HeaderDiagnosticIdentifier::MessageText`.

// ---------------------------------------------------------------------------
// SQLColAttribute SQL_DESC_UNNAMED values (sql.h)
// ---------------------------------------------------------------------------

/// `SQL_NAMED` — the `SQL_DESC_UNNAMED` descriptor field value meaning the
/// column has a name. Spec: `SQLColAttribute`, `SQL_DESC_UNNAMED`.
pub const SQL_NAMED: isize = 0;

/// `SQL_UNNAMED` — the `SQL_DESC_UNNAMED` descriptor field value meaning the
/// column has no name (`SQL_DESC_NAME` is the empty string).
pub const SQL_UNNAMED: isize = 1;

// ---------------------------------------------------------------------------
// SQL_CURSOR_COMMIT_BEHAVIOR / SQL_CURSOR_ROLLBACK_BEHAVIOR values (sql.h)
// ---------------------------------------------------------------------------

/// `SQL_CB_DELETE` (0) — `SQLEndTran` closes and deletes all open cursors on
/// the connection and discards the access plans of all prepared statements,
/// leaving each statement in the allocated (unprepared) state.
///
/// Not modelled by `odbc-sys`, which has no cursor-behaviour enum.
pub const SQL_CB_DELETE: u16 = 0;

/// `SQL_CB_CLOSE` (1) — `SQLEndTran` closes all open cursors on the connection
/// but leaves prepared statements prepared, so `SQLExecute` can be called
/// again without re-preparing.
pub const SQL_CB_CLOSE: u16 = 1;

/// `SQL_CB_PRESERVE` (2) — `SQLEndTran` leaves open cursors untouched. Cursors
/// remain positioned on the row they pointed at before the call.
pub const SQL_CB_PRESERVE: u16 = 2;

// ---------------------------------------------------------------------------
// SQLGetInfo info types (sql.h / sqlext.h)
// ---------------------------------------------------------------------------

/// `SQL_CURSOR_COMMIT_BEHAVIOR` (23) — `SQLGetInfo` type describing what
/// happens to open cursors when a transaction is committed.
///
/// Derived from `odbc_sys::InfoType` rather than restating the number; see
/// [`SQL_FILE_USAGE`]. Needed as a raw `u16` because
/// `info_type_default_response` matches on the raw value.
pub const SQL_CURSOR_COMMIT_BEHAVIOR: u16 = InfoType::CursorCommitBehaviour as u16;

/// `SQL_CURSOR_ROLLBACK_BEHAVIOR` (24) — `SQLGetInfo` type describing what
/// happens to open cursors when a transaction is rolled back. Not modelled by
/// `odbc_sys::InfoType`, which has `CursorCommitBehaviour` but no rollback
/// counterpart.
pub const SQL_CURSOR_ROLLBACK_BEHAVIOR: u16 = 24;

/// `SQL_ROW_UPDATES` (11) — `SQLGetInfo` type reporting whether the data
/// source supports row updates such as positioned update statements. A `"Y"` /
/// `"N"` character string. Not modelled by `odbc_sys::InfoType`, so it can only
/// be answered through the raw-`u16` path.
pub const SQL_ROW_UPDATES: u16 = 11;

/// `SQL_PROCEDURES` (21) — `SQLGetInfo` type reporting whether the data source
/// supports procedures *and* the driver supports the ODBC procedure invocation
/// syntax. A `"Y"` / `"N"` character string. Not modelled by
/// `odbc_sys::InfoType`; see [`SQL_ROW_UPDATES`].
pub const SQL_PROCEDURES: u16 = 21;

/// `SQL_MULTIPLE_ACTIVE_TXN` (37) — `SQLGetInfo` type reporting whether the
/// driver supports more than one active transaction at a time. A `"Y"` /
/// `"N"` character string. Not modelled by `odbc_sys::InfoType`; see
/// [`SQL_ROW_UPDATES`].
pub const SQL_MULTIPLE_ACTIVE_TXN: u16 = 37;

/// `SQL_DATABASE_NAME` (16) — `SQLGetInfo` type carrying the name of the
/// database currently in use, where the data source has a named object called
/// a "database". A character string. Superseded in ODBC 3.x by
/// `SQLGetConnectAttr(SQL_ATTR_CURRENT_CATALOG)`, but still queried. Not
/// modelled by `odbc_sys::InfoType`; see [`SQL_ROW_UPDATES`].
pub const SQL_DATABASE_NAME: u16 = 16;

/// `SQL_PROCEDURE_TERM` (40) — `SQLGetInfo` type carrying the data source
/// vendor's name for a procedure. A character string, empty when the data
/// source has no procedures. Not modelled by `odbc_sys::InfoType`; see
/// [`SQL_ROW_UPDATES`].
pub const SQL_PROCEDURE_TERM: u16 = 40;

/// `SQL_TABLE_TERM` (45) — `SQLGetInfo` type carrying the data source vendor's
/// name for a table (e.g. `"table"` or `"file"`). A character string. Unlike
/// the catalog, schema and procedure terms it has no "empty if unsupported"
/// case, because every data source has tables. Not modelled by
/// `odbc_sys::InfoType`; see [`SQL_ROW_UPDATES`].
pub const SQL_TABLE_TERM: u16 = 45;

/// `SQL_KEYWORDS` (89) — `SQLGetInfo` type carrying a comma-separated list of
/// the data source's own reserved keywords, excluding those it shares with
/// ODBC. A character string; an empty list is valid. Not modelled by
/// `odbc_sys::InfoType`; see [`SQL_ROW_UPDATES`].
pub const SQL_KEYWORDS: u16 = 89;

/// `SQL_FILE_USAGE` (84) — `SQLGetInfo` type describing how a single-tier
/// driver treats files in the data source.
///
/// Derived from `odbc_sys::InfoType` rather than restating the number:
/// `get_info_raw` matches on a raw `u16`, which needs a `const` pattern, but
/// odbc-sys stays the single source of truth for the value.
pub const SQL_FILE_USAGE: u16 = InfoType::SqlFileUsage as u16;

/// `SQL_QUOTED_IDENTIFIER_CASE` (93) — `SQLGetInfo` type describing how the
/// data source treats the case of quoted identifiers.
///
/// Derived from `odbc_sys::InfoType` rather than restating the number; see
/// [`SQL_FILE_USAGE`].
pub const SQL_QUOTED_IDENTIFIER_CASE: u16 = InfoType::SqlQuotedIdentifierCase as u16;

// ---------------------------------------------------------------------------
// SQLUSMALLINT info types that odbc-sys does not model
// ---------------------------------------------------------------------------
//
// These four matter beyond naming. `SQLGetInfo`'s `BufferLength` is *ignored*
// for a non-string value — the spec has the driver assume the buffer matches
// the type it declares for that info type — so returning a `SQLUINTEGER` where
// the spec declares `SQLUSMALLINT` writes four bytes into the two an
// application correctly allocated. Being absent from `odbc_sys::InfoType`,
// they have no variant for the shape-aware fallback in
// `info_type_default_response` to consult, which is exactly how they reached
// the `U32(0)` default. `SMALLINT_SHAPED_UNMODELLED_INFO_TYPES` there lists
// them; keep the two in step.

/// `SQL_ODBC_API_CONFORMANCE` (9) — the ODBC API conformance level
/// (`SQL_OAC_*`). ODBC 2.x; still queried. `SQLUSMALLINT`.
pub const SQL_ODBC_API_CONFORMANCE: u16 = 9;

/// `SQL_ODBC_SAG_CLI_CONFORMANCE` (12) — whether the driver conforms to the SAG
/// CLI specification (`SQL_OSCC_*`). ODBC 2.x. `SQLUSMALLINT`.
pub const SQL_ODBC_SAG_CLI_CONFORMANCE: u16 = 12;

/// `SQL_ODBC_SQL_CONFORMANCE` (15) — the ODBC SQL grammar conformance level
/// (`SQL_OSC_*`). ODBC 2.x, superseded by `SQL_SQL_CONFORMANCE` (118) but still
/// queried by 2.x-era clients. `SQLUSMALLINT`.
pub const SQL_ODBC_SQL_CONFORMANCE: u16 = 15;

/// `SQL_MAX_PROCEDURE_NAME_LEN` (33) — the maximum length of a procedure name,
/// `0` for "no limit or unknown". `SQLUSMALLINT`, like every other
/// `SQL_MAX_*_NAME_LEN`, all of which odbc-sys does model.
pub const SQL_MAX_PROCEDURE_NAME_LEN: u16 = 33;

/// `SQL_DRIVER_ODBC_VER` — the ODBC version this driver conforms to, in the
/// spec's `##.##` form. Reported identically by the driver crates, and read
/// by the Windows Driver Manager *before* `SQLDriverConnectW` to decide
/// whether to allow ODBC 3.x features such as `SQL_C_SBIGINT` (see AGENTS.md,
/// "Windows Driver Manager compatibility").
pub const SQL_DRIVER_ODBC_VER_STRING: &str = "03.80";

// --- SQLGetInfo capability info types, derived from odbc_sys::InfoType ---
//
// Each of these names a genuine `odbc_sys::InfoType` variant (odbc-sys 0.31
// added named variants for all of them). They stay as `u16` constants,
// rather than being matched as `InfoType` directly, because `get_info_raw`
// matches on the raw `u16` info type (before the typed `InfoType` dispatch
// runs), so the driver-specific capability bitmaps below are reachable even
// though `default_get_info`/the per-driver typed `get_info` have no arms for
// them. See [`SQL_FILE_USAGE`] for why the value is derived rather than
// restated.

/// `SQL_OUTER_JOINS` (38) — "Y" if the data source supports outer joins.
pub const SQL_OUTER_JOINS: u16 = InfoType::OuterJoins as u16;
/// `SQL_NUMERIC_FUNCTIONS` (49) — bitmap of supported numeric scalar functions.
pub const SQL_NUMERIC_FUNCTIONS: u16 = InfoType::NumericFunctions as u16;
/// `SQL_STRING_FUNCTIONS` (50) — bitmap of supported string scalar functions.
pub const SQL_STRING_FUNCTIONS: u16 = InfoType::StringFunctions as u16;
/// `SQL_SYSTEM_FUNCTIONS` (51) — bitmap of supported system scalar functions.
pub const SQL_SYSTEM_FUNCTIONS: u16 = InfoType::SystemFunctions as u16;
/// `SQL_TIMEDATE_FUNCTIONS` (52) — bitmap of supported date/time scalar functions.
pub const SQL_TIMEDATE_FUNCTIONS: u16 = InfoType::TimedateFunctions as u16;
/// `SQL_LIKE_ESCAPE_CLAUSE` (113) — "Y" if `LIKE ... ESCAPE` is supported.
pub const SQL_LIKE_ESCAPE_CLAUSE: u16 = InfoType::LikeEscapeClause as u16;
/// `SQL_SQL92_PREDICATES` (160) — bitmap of supported SQL-92 predicates.
pub const SQL_SQL92_PREDICATES: u16 = InfoType::Sql92Predicates as u16;
/// `SQL_SQL92_RELATIONAL_JOIN_OPERATORS` (161) — bitmap of supported SQL-92 join operators.
pub const SQL_SQL92_RELATIONAL_JOIN_OPERATORS: u16 = InfoType::Sql92RelationalJoinOperators as u16;
/// `SQL_SQL92_VALUE_EXPRESSIONS` (165) — bitmap of supported SQL-92 value expressions.
pub const SQL_SQL92_VALUE_EXPRESSIONS: u16 = InfoType::Sql92ValueExpressions as u16;
/// `SQL_AGGREGATE_FUNCTIONS` (169) — bitmap of supported aggregate functions.
pub const SQL_AGGREGATE_FUNCTIONS: u16 = InfoType::AggregateFunctions as u16;

// --- SQL_AGGREGATE_FUNCTIONS (169) flags ---
/// `SQL_AF_AVG` — the `AVG` aggregate.
pub const SQL_AF_AVG: u32 = 0x0000_0001;
/// `SQL_AF_COUNT` — the `COUNT` aggregate.
pub const SQL_AF_COUNT: u32 = 0x0000_0002;
/// `SQL_AF_MAX` — the `MAX` aggregate.
pub const SQL_AF_MAX: u32 = 0x0000_0004;
/// `SQL_AF_MIN` — the `MIN` aggregate.
pub const SQL_AF_MIN: u32 = 0x0000_0008;
/// `SQL_AF_SUM` — the `SUM` aggregate.
pub const SQL_AF_SUM: u32 = 0x0000_0010;
/// `SQL_AF_DISTINCT` — the `DISTINCT` set quantifier in an aggregate.
pub const SQL_AF_DISTINCT: u32 = 0x0000_0020;
/// `SQL_AF_ALL` — the `ALL` set quantifier in an aggregate.
pub const SQL_AF_ALL: u32 = 0x0000_0040;

// --- SQL_SQL92_PREDICATES (160) flags ---
/// `SQL_SP_EXISTS` — the `EXISTS` predicate.
pub const SQL_SP_EXISTS: u32 = 0x0000_0001;
/// `SQL_SP_ISNOTNULL` — the `IS NOT NULL` predicate.
pub const SQL_SP_ISNOTNULL: u32 = 0x0000_0002;
/// `SQL_SP_ISNULL` — the `IS NULL` predicate.
pub const SQL_SP_ISNULL: u32 = 0x0000_0004;
/// `SQL_SP_MATCH_FULL` — the SQL-92 `MATCH FULL` predicate.
pub const SQL_SP_MATCH_FULL: u32 = 0x0000_0008;
/// `SQL_SP_MATCH_PARTIAL` — the SQL-92 `MATCH PARTIAL` predicate.
pub const SQL_SP_MATCH_PARTIAL: u32 = 0x0000_0010;
/// `SQL_SP_MATCH_UNIQUE_FULL` — the SQL-92 `MATCH UNIQUE FULL` predicate.
pub const SQL_SP_MATCH_UNIQUE_FULL: u32 = 0x0000_0020;
/// `SQL_SP_MATCH_UNIQUE_PARTIAL` — the SQL-92 `MATCH UNIQUE PARTIAL` predicate.
pub const SQL_SP_MATCH_UNIQUE_PARTIAL: u32 = 0x0000_0040;
/// `SQL_SP_OVERLAPS` — the SQL-92 temporal `OVERLAPS` predicate.
pub const SQL_SP_OVERLAPS: u32 = 0x0000_0080;
/// `SQL_SP_UNIQUE` — the SQL-92 `UNIQUE` predicate.
pub const SQL_SP_UNIQUE: u32 = 0x0000_0100;
/// `SQL_SP_LIKE` — the `LIKE` predicate.
pub const SQL_SP_LIKE: u32 = 0x0000_0200;
/// `SQL_SP_IN` — the `IN` predicate.
pub const SQL_SP_IN: u32 = 0x0000_0400;
/// `SQL_SP_BETWEEN` — the `BETWEEN` predicate.
pub const SQL_SP_BETWEEN: u32 = 0x0000_0800;
/// `SQL_SP_COMPARISON` — the comparison predicates (`=`, `<>`, `<`, `>`, `<=`, `>=`).
pub const SQL_SP_COMPARISON: u32 = 0x0000_1000;
/// `SQL_SP_QUANTIFIED_COMPARISON` — the quantified comparison predicates (`ALL`, `ANY`, `SOME`).
pub const SQL_SP_QUANTIFIED_COMPARISON: u32 = 0x0000_2000;

// --- SQL_SQL92_RELATIONAL_JOIN_OPERATORS (161) flags ---
/// `SQL_SRJO_CORRESPONDING_CLAUSE` — the `CORRESPONDING` clause on a set operation.
pub const SQL_SRJO_CORRESPONDING_CLAUSE: u32 = 0x0000_0001;
/// `SQL_SRJO_CROSS_JOIN` — `CROSS JOIN`.
pub const SQL_SRJO_CROSS_JOIN: u32 = 0x0000_0002;
/// `SQL_SRJO_EXCEPT_JOIN` — the `EXCEPT` set operator.
pub const SQL_SRJO_EXCEPT_JOIN: u32 = 0x0000_0004;
/// `SQL_SRJO_FULL_OUTER_JOIN` — `FULL OUTER JOIN`.
pub const SQL_SRJO_FULL_OUTER_JOIN: u32 = 0x0000_0008;
/// `SQL_SRJO_INNER_JOIN` — `INNER JOIN`.
pub const SQL_SRJO_INNER_JOIN: u32 = 0x0000_0010;
/// `SQL_SRJO_INTERSECT_JOIN` — the `INTERSECT` set operator.
pub const SQL_SRJO_INTERSECT_JOIN: u32 = 0x0000_0020;
/// `SQL_SRJO_LEFT_OUTER_JOIN` — `LEFT OUTER JOIN`.
pub const SQL_SRJO_LEFT_OUTER_JOIN: u32 = 0x0000_0040;
/// `SQL_SRJO_NATURAL_JOIN` — `NATURAL JOIN`.
pub const SQL_SRJO_NATURAL_JOIN: u32 = 0x0000_0080;
/// `SQL_SRJO_RIGHT_OUTER_JOIN` — `RIGHT OUTER JOIN`.
pub const SQL_SRJO_RIGHT_OUTER_JOIN: u32 = 0x0000_0100;
/// `SQL_SRJO_UNION_JOIN` — the SQL-92 `UNION JOIN`.
pub const SQL_SRJO_UNION_JOIN: u32 = 0x0000_0200;

// --- SQL_SQL92_VALUE_EXPRESSIONS (165) flags ---
/// `SQL_SVE_CASE` — the `CASE` expression.
pub const SQL_SVE_CASE: u32 = 0x0000_0001;
/// `SQL_SVE_CAST` — the `CAST` expression.
pub const SQL_SVE_CAST: u32 = 0x0000_0002;
/// `SQL_SVE_COALESCE` — the `COALESCE` expression.
pub const SQL_SVE_COALESCE: u32 = 0x0000_0004;
/// `SQL_SVE_NULLIF` — the `NULLIF` expression.
pub const SQL_SVE_NULLIF: u32 = 0x0000_0008;

// --- SQL_NUMERIC_FUNCTIONS (49) flags ---
/// `SQL_FN_NUM_ABS` — the `ABS` scalar function.
pub const SQL_FN_NUM_ABS: u32 = 0x0000_0001;
/// `SQL_FN_NUM_ACOS` — the `ACOS` scalar function.
pub const SQL_FN_NUM_ACOS: u32 = 0x0000_0002;
/// `SQL_FN_NUM_ASIN` — the `ASIN` scalar function.
pub const SQL_FN_NUM_ASIN: u32 = 0x0000_0004;
/// `SQL_FN_NUM_ATAN` — the `ATAN` scalar function.
pub const SQL_FN_NUM_ATAN: u32 = 0x0000_0008;
/// `SQL_FN_NUM_ATAN2` — the `ATAN2` scalar function.
pub const SQL_FN_NUM_ATAN2: u32 = 0x0000_0010;
/// `SQL_FN_NUM_CEILING` — the `CEILING` scalar function.
pub const SQL_FN_NUM_CEILING: u32 = 0x0000_0020;
/// `SQL_FN_NUM_COS` — the `COS` scalar function.
pub const SQL_FN_NUM_COS: u32 = 0x0000_0040;
/// `SQL_FN_NUM_COT` — the `COT` scalar function.
pub const SQL_FN_NUM_COT: u32 = 0x0000_0080;
/// `SQL_FN_NUM_EXP` — the `EXP` scalar function.
pub const SQL_FN_NUM_EXP: u32 = 0x0000_0100;
/// `SQL_FN_NUM_FLOOR` — the `FLOOR` scalar function.
pub const SQL_FN_NUM_FLOOR: u32 = 0x0000_0200;
/// `SQL_FN_NUM_LOG` — the `LOG` scalar function (natural logarithm).
pub const SQL_FN_NUM_LOG: u32 = 0x0000_0400;
/// `SQL_FN_NUM_MOD` — the `MOD` scalar function.
pub const SQL_FN_NUM_MOD: u32 = 0x0000_0800;
/// `SQL_FN_NUM_SIGN` — the `SIGN` scalar function.
pub const SQL_FN_NUM_SIGN: u32 = 0x0000_1000;
/// `SQL_FN_NUM_SIN` — the `SIN` scalar function.
pub const SQL_FN_NUM_SIN: u32 = 0x0000_2000;
/// `SQL_FN_NUM_SQRT` — the `SQRT` scalar function.
pub const SQL_FN_NUM_SQRT: u32 = 0x0000_4000;
/// `SQL_FN_NUM_TAN` — the `TAN` scalar function.
pub const SQL_FN_NUM_TAN: u32 = 0x0000_8000;
/// `SQL_FN_NUM_PI` — the `PI` scalar function.
pub const SQL_FN_NUM_PI: u32 = 0x0001_0000;
/// `SQL_FN_NUM_RAND` — the `RAND` scalar function.
pub const SQL_FN_NUM_RAND: u32 = 0x0002_0000;
/// `SQL_FN_NUM_DEGREES` — the `DEGREES` scalar function.
pub const SQL_FN_NUM_DEGREES: u32 = 0x0004_0000;
/// `SQL_FN_NUM_LOG10` — the `LOG10` scalar function.
pub const SQL_FN_NUM_LOG10: u32 = 0x0008_0000;
/// `SQL_FN_NUM_POWER` — the `POWER` scalar function.
pub const SQL_FN_NUM_POWER: u32 = 0x0010_0000;
/// `SQL_FN_NUM_RADIANS` — the `RADIANS` scalar function.
pub const SQL_FN_NUM_RADIANS: u32 = 0x0020_0000;
/// `SQL_FN_NUM_ROUND` — the `ROUND` scalar function.
pub const SQL_FN_NUM_ROUND: u32 = 0x0040_0000;
/// `SQL_FN_NUM_TRUNCATE` — the `TRUNCATE` scalar function.
pub const SQL_FN_NUM_TRUNCATE: u32 = 0x0080_0000;

// --- SQL_STRING_FUNCTIONS (50) flags ---
/// `SQL_FN_STR_CONCAT` — the `CONCAT` scalar function.
pub const SQL_FN_STR_CONCAT: u32 = 0x0000_0001;
/// `SQL_FN_STR_INSERT` — the `INSERT` scalar function.
pub const SQL_FN_STR_INSERT: u32 = 0x0000_0002;
/// `SQL_FN_STR_LEFT` — the `LEFT` scalar function.
pub const SQL_FN_STR_LEFT: u32 = 0x0000_0004;
/// `SQL_FN_STR_LTRIM` — the `LTRIM` scalar function.
pub const SQL_FN_STR_LTRIM: u32 = 0x0000_0008;
/// `SQL_FN_STR_LENGTH` — the `LENGTH` scalar function.
pub const SQL_FN_STR_LENGTH: u32 = 0x0000_0010;
/// `SQL_FN_STR_LOCATE` — the two-argument `LOCATE` scalar function.
pub const SQL_FN_STR_LOCATE: u32 = 0x0000_0020;
/// `SQL_FN_STR_LCASE` — the `LCASE` scalar function.
pub const SQL_FN_STR_LCASE: u32 = 0x0000_0040;
/// `SQL_FN_STR_REPEAT` — the `REPEAT` scalar function.
pub const SQL_FN_STR_REPEAT: u32 = 0x0000_0080;
/// `SQL_FN_STR_REPLACE` — the `REPLACE` scalar function.
pub const SQL_FN_STR_REPLACE: u32 = 0x0000_0100;
/// `SQL_FN_STR_RIGHT` — the `RIGHT` scalar function.
pub const SQL_FN_STR_RIGHT: u32 = 0x0000_0200;
/// `SQL_FN_STR_RTRIM` — the `RTRIM` scalar function.
pub const SQL_FN_STR_RTRIM: u32 = 0x0000_0400;
/// `SQL_FN_STR_SUBSTRING` — the `SUBSTRING` scalar function.
pub const SQL_FN_STR_SUBSTRING: u32 = 0x0000_0800;
/// `SQL_FN_STR_UCASE` — the `UCASE` scalar function.
pub const SQL_FN_STR_UCASE: u32 = 0x0000_1000;
/// `SQL_FN_STR_ASCII` — the `ASCII` scalar function.
pub const SQL_FN_STR_ASCII: u32 = 0x0000_2000;
/// `SQL_FN_STR_CHAR` — the `CHAR` scalar function.
pub const SQL_FN_STR_CHAR: u32 = 0x0000_4000;
/// `SQL_FN_STR_DIFFERENCE` — the `DIFFERENCE` scalar function.
pub const SQL_FN_STR_DIFFERENCE: u32 = 0x0000_8000;
/// `SQL_FN_STR_LOCATE_2` — the three-argument `LOCATE` scalar function.
pub const SQL_FN_STR_LOCATE_2: u32 = 0x0001_0000;
/// `SQL_FN_STR_SOUNDEX` — the `SOUNDEX` scalar function.
pub const SQL_FN_STR_SOUNDEX: u32 = 0x0002_0000;
/// `SQL_FN_STR_SPACE` — the `SPACE` scalar function.
pub const SQL_FN_STR_SPACE: u32 = 0x0004_0000;
/// `SQL_FN_STR_BIT_LENGTH` — the `BIT_LENGTH` scalar function.
pub const SQL_FN_STR_BIT_LENGTH: u32 = 0x0008_0000;
/// `SQL_FN_STR_CHAR_LENGTH` — the `CHAR_LENGTH` scalar function.
pub const SQL_FN_STR_CHAR_LENGTH: u32 = 0x0010_0000;
/// `SQL_FN_STR_CHARACTER_LENGTH` — the `CHARACTER_LENGTH` scalar function.
pub const SQL_FN_STR_CHARACTER_LENGTH: u32 = 0x0020_0000;
/// `SQL_FN_STR_OCTET_LENGTH` — the `OCTET_LENGTH` scalar function.
pub const SQL_FN_STR_OCTET_LENGTH: u32 = 0x0040_0000;
/// `SQL_FN_STR_POSITION` — the `POSITION` scalar function.
pub const SQL_FN_STR_POSITION: u32 = 0x0080_0000;

// --- SQL_SYSTEM_FUNCTIONS (51) flags ---
/// `SQL_FN_SYS_USERNAME` — the `USERNAME` scalar function.
pub const SQL_FN_SYS_USERNAME: u32 = 0x0000_0001;
/// `SQL_FN_SYS_DBNAME` — the `DBNAME` scalar function.
pub const SQL_FN_SYS_DBNAME: u32 = 0x0000_0002;
/// `SQL_FN_SYS_IFNULL` — the `IFNULL` scalar function.
pub const SQL_FN_SYS_IFNULL: u32 = 0x0000_0004;

// --- SQL_TIMEDATE_FUNCTIONS (52) flags ---
/// `SQL_FN_TD_NOW` — the `NOW` scalar function.
pub const SQL_FN_TD_NOW: u32 = 0x0000_0001;
/// `SQL_FN_TD_CURDATE` — the `CURDATE` scalar function.
pub const SQL_FN_TD_CURDATE: u32 = 0x0000_0002;
/// `SQL_FN_TD_DAYOFMONTH` — the `DAYOFMONTH` scalar function.
pub const SQL_FN_TD_DAYOFMONTH: u32 = 0x0000_0004;
/// `SQL_FN_TD_DAYOFWEEK` — the `DAYOFWEEK` scalar function.
pub const SQL_FN_TD_DAYOFWEEK: u32 = 0x0000_0008;
/// `SQL_FN_TD_DAYOFYEAR` — the `DAYOFYEAR` scalar function.
pub const SQL_FN_TD_DAYOFYEAR: u32 = 0x0000_0010;
/// `SQL_FN_TD_MONTH` — the `MONTH` scalar function.
pub const SQL_FN_TD_MONTH: u32 = 0x0000_0020;
/// `SQL_FN_TD_QUARTER` — the `QUARTER` scalar function.
pub const SQL_FN_TD_QUARTER: u32 = 0x0000_0040;
/// `SQL_FN_TD_WEEK` — the `WEEK` scalar function.
pub const SQL_FN_TD_WEEK: u32 = 0x0000_0080;
/// `SQL_FN_TD_YEAR` — the `YEAR` scalar function.
pub const SQL_FN_TD_YEAR: u32 = 0x0000_0100;
/// `SQL_FN_TD_CURTIME` — the `CURTIME` scalar function.
pub const SQL_FN_TD_CURTIME: u32 = 0x0000_0200;
/// `SQL_FN_TD_HOUR` — the `HOUR` scalar function.
pub const SQL_FN_TD_HOUR: u32 = 0x0000_0400;
/// `SQL_FN_TD_MINUTE` — the `MINUTE` scalar function.
pub const SQL_FN_TD_MINUTE: u32 = 0x0000_0800;
/// `SQL_FN_TD_SECOND` — the `SECOND` scalar function.
pub const SQL_FN_TD_SECOND: u32 = 0x0000_1000;
/// `SQL_FN_TD_TIMESTAMPADD` — the `TIMESTAMPADD` scalar function.
pub const SQL_FN_TD_TIMESTAMPADD: u32 = 0x0000_2000;
/// `SQL_FN_TD_TIMESTAMPDIFF` — the `TIMESTAMPDIFF` scalar function.
pub const SQL_FN_TD_TIMESTAMPDIFF: u32 = 0x0000_4000;
/// `SQL_FN_TD_DAYNAME` — the `DAYNAME` scalar function.
pub const SQL_FN_TD_DAYNAME: u32 = 0x0000_8000;
/// `SQL_FN_TD_MONTHNAME` — the `MONTHNAME` scalar function.
pub const SQL_FN_TD_MONTHNAME: u32 = 0x0001_0000;
/// `SQL_FN_TD_CURRENT_DATE` — the `CURRENT_DATE` scalar function.
pub const SQL_FN_TD_CURRENT_DATE: u32 = 0x0002_0000;
/// `SQL_FN_TD_CURRENT_TIME` — the `CURRENT_TIME` scalar function.
pub const SQL_FN_TD_CURRENT_TIME: u32 = 0x0004_0000;
/// `SQL_FN_TD_CURRENT_TIMESTAMP` — the `CURRENT_TIMESTAMP` scalar function.
pub const SQL_FN_TD_CURRENT_TIMESTAMP: u32 = 0x0008_0000;
/// `SQL_FN_TD_EXTRACT` — the `EXTRACT` scalar function.
pub const SQL_FN_TD_EXTRACT: u32 = 0x0010_0000;

// --- SQLTables: the SQL_ALL_* enumeration sentinels (sqlext.h) ---
/// `SQL_ALL_CATALOGS` — `SQLTables`' catalog-enumeration sentinel.
///
/// All three `SQL_ALL_*` sentinels are the same string, `"%"`. Which
/// enumeration is meant depends on **which argument carries it while the
/// others are empty strings**, never on the `"%"` alone:
/// `SQLTables("%", "%", "%")` is an ordinary match-everything query, not an
/// enumeration.
pub const SQL_ALL_CATALOGS: &str = "%";
/// `SQL_ALL_SCHEMAS` — `SQLTables`' schema-enumeration sentinel. See
/// [`SQL_ALL_CATALOGS`] for the disambiguation rule; the two constants are the
/// same string.
pub const SQL_ALL_SCHEMAS: &str = "%";
/// `SQL_ALL_TABLE_TYPES` — `SQLTables`' table-type-enumeration sentinel. See
/// [`SQL_ALL_CATALOGS`] for the disambiguation rule; the two constants are the
/// same string.
pub const SQL_ALL_TABLE_TYPES: &str = "%";

// --- SQLStatistics: Unique argument (sqlext.h) ---
/// SQLStatistics `Unique`: return only unique indexes (SQL_INDEX_UNIQUE = 0).
pub const SQL_INDEX_UNIQUE: u16 = 0;
/// SQLStatistics `Unique`: return all indexes (SQL_INDEX_ALL = 1).
pub const SQL_INDEX_ALL: u16 = 1;

// --- SQLStatistics: Reserved argument (sqlext.h) ---
/// SQLStatistics `Reserved`: fetch cardinality-free ("quick") statistics
/// without ensuring they are current (SQL_QUICK = 0).
pub const SQL_QUICK: u16 = 0;
/// SQLStatistics `Reserved`: ensure statistics are current before returning
/// them (SQL_ENSURE = 1).
pub const SQL_ENSURE: u16 = 1;

// --- SQL_ATTR_CURSOR_TYPE values (sql.h) ---
/// SQL_ATTR_CURSOR_TYPE: forward-only cursor, the only type this framework
/// supports (SQL_CURSOR_FORWARD_ONLY = 0).
pub const SQL_CURSOR_FORWARD_ONLY: usize = 0;

// --- SQL_ATTR_CONNECTION_DEAD values (sqlext.h) ---
/// SQL_ATTR_CONNECTION_DEAD: the connection is still alive (SQL_CD_FALSE = 0).
pub const SQL_CD_FALSE: usize = 0;
/// SQL_ATTR_CONNECTION_DEAD: the connection is dead (SQL_CD_TRUE = 1).
pub const SQL_CD_TRUE: usize = 1;

// --- SQLStatistics: TYPE result column (sqlext.h) ---
/// SQLStatistics TYPE: a table statistic row (SQL_TABLE_STAT = 0).
pub const SQL_TABLE_STAT: i16 = 0;
/// SQLStatistics TYPE: clustered index (SQL_INDEX_CLUSTERED = 1).
pub const SQL_INDEX_CLUSTERED: i16 = 1;
/// SQLStatistics TYPE: hashed index (SQL_INDEX_HASHED = 2).
pub const SQL_INDEX_HASHED: i16 = 2;
/// SQLStatistics TYPE: other index type (SQL_INDEX_OTHER = 3).
///
/// `sql.h`/`sqlext.h` define no SQL_INDEX_BTREE numeric constant (the spec's
/// B-tree label is descriptive), so an index with no clustered/hashed
/// classification is reported as SQL_INDEX_OTHER.
pub const SQL_INDEX_OTHER: i16 = 3;

// --- SQLSpecialColumns: IdentifierType argument (sqlext.h) ---
/// SQLSpecialColumns `IdentifierType`: optimal row identifier (SQL_BEST_ROWID = 1).
pub const SQL_BEST_ROWID: u16 = 1;
/// SQLSpecialColumns `IdentifierType`: auto-updated row-version columns (SQL_ROWVER = 2).
pub const SQL_ROWVER: u16 = 2;

// --- SQLSpecialColumns: Scope argument / SCOPE result column (sql.h) ---
/// SQLSpecialColumns SCOPE: valid only while positioned on the row (SQL_SCOPE_CURROW = 0).
pub const SQL_SCOPE_CURROW: u16 = 0;
/// SQLSpecialColumns SCOPE: valid for the current transaction (SQL_SCOPE_TRANSACTION = 1).
pub const SQL_SCOPE_TRANSACTION: u16 = 1;
/// SQLSpecialColumns SCOPE: valid for the whole session (SQL_SCOPE_SESSION = 2).
pub const SQL_SCOPE_SESSION: u16 = 2;

// --- SQLSpecialColumns: PSEUDO_COLUMN result column (sql.h) ---
/// SQLSpecialColumns PSEUDO_COLUMN: unknown (SQL_PC_UNKNOWN = 0).
pub const SQL_PC_UNKNOWN: i16 = 0;
/// SQLSpecialColumns PSEUDO_COLUMN: a real, non-pseudo column (SQL_PC_NOT_PSEUDO = 1).
pub const SQL_PC_NOT_PSEUDO: i16 = 1;
/// SQLSpecialColumns PSEUDO_COLUMN: a pseudo-column such as `rowid` (SQL_PC_PSEUDO = 2).
pub const SQL_PC_PSEUDO: i16 = 2;

// --- SQLProcedures: PROCEDURE_TYPE result column (sqlext.h) ---
//
// `SQLProcedureColumns`' COLUMN_TYPE values are *not* here: they are exactly
// `odbc_sys::ParamType`'s discriminants (SQL_PARAM_TYPE_UNKNOWN, SQL_PARAM_INPUT,
// SQL_PARAM_INPUT_OUTPUT, SQL_RESULT_COL, SQL_PARAM_OUTPUT, SQL_RETURN_VALUE),
// so a backend spells that column `ParamType::Input as i16`.
/// SQLProcedures PROCEDURE_TYPE: cannot be determined whether the procedure
/// returns a value (SQL_PT_UNKNOWN = 0).
pub const SQL_PT_UNKNOWN: i16 = 0;
/// SQLProcedures PROCEDURE_TYPE: the object does not return a value
/// (SQL_PT_PROCEDURE = 1).
pub const SQL_PT_PROCEDURE: i16 = 1;
/// SQLProcedures PROCEDURE_TYPE: the object returns a value
/// (SQL_PT_FUNCTION = 2).
pub const SQL_PT_FUNCTION: i16 = 2;

/// The ODBC reserved keywords, from Appendix C of the specification.
///
/// [`SQL_KEYWORDS`] is defined as the data source's keywords *excluding*
/// these: "This list does not contain keywords specific to ODBC or keywords
/// used by both the data source and ODBC." Roughly the SQL-92 reserved list,
/// which is why one list suffices for every backend.
///
/// Written in the order the specification page lists them rather than sorted,
/// so a reviewer can diff it against the source. Nothing here depends on the
/// order — the subtraction in `common_get_info_raw` does a linear membership
/// test, and `odbc_reserved_keywords_are_unique` guards the one property that
/// matters.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/appendixes/reserved-keywords>
pub const ODBC_RESERVED_KEYWORDS: &[&str] = &[
    "ABSOLUTE",
    "ACTION",
    "ADA",
    "ADD",
    "ALL",
    "ALLOCATE",
    "ALTER",
    "AND",
    "ANY",
    "ARE",
    "AS",
    "ASC",
    "ASSERTION",
    "AT",
    "AUTHORIZATION",
    "AVG",
    "BEGIN",
    "BETWEEN",
    "BIT",
    "BIT_LENGTH",
    "BOTH",
    "BY",
    "CASCADE",
    "CASCADED",
    "CASE",
    "CAST",
    "CATALOG",
    "CHAR",
    "CHAR_LENGTH",
    "CHARACTER",
    "CHARACTER_LENGTH",
    "CHECK",
    "CLOSE",
    "COALESCE",
    "COLLATE",
    "COLLATION",
    "COLUMN",
    "COMMIT",
    "CONNECT",
    "CONNECTION",
    "CONSTRAINT",
    "CONSTRAINTS",
    "CONTINUE",
    "CONVERT",
    "CORRESPONDING",
    "COUNT",
    "CREATE",
    "CROSS",
    "CURRENT",
    "CURRENT_DATE",
    "CURRENT_TIME",
    "CURRENT_TIMESTAMP",
    "CURRENT_USER",
    "CURSOR",
    "DATE",
    "DAY",
    "DEALLOCATE",
    "DEC",
    "DECIMAL",
    "DECLARE",
    "DEFAULT",
    "DEFERRABLE",
    "DEFERRED",
    "DELETE",
    "DESC",
    "DESCRIBE",
    "DESCRIPTOR",
    "DIAGNOSTICS",
    "DISCONNECT",
    "DISTINCT",
    "DOMAIN",
    "DOUBLE",
    "DROP",
    "ELSE",
    "END",
    "END-EXEC",
    "ESCAPE",
    "EXCEPT",
    "EXCEPTION",
    "EXEC",
    "EXECUTE",
    "EXISTS",
    "EXTERNAL",
    "EXTRACT",
    "FALSE",
    "FETCH",
    "FIRST",
    "FLOAT",
    "FOR",
    "FOREIGN",
    "FORTRAN",
    "FOUND",
    "FROM",
    "FULL",
    "GET",
    "GLOBAL",
    "GO",
    "GOTO",
    "GRANT",
    "GROUP",
    "HAVING",
    "HOUR",
    "IDENTITY",
    "IMMEDIATE",
    "IN",
    "INCLUDE",
    "INDEX",
    "INDICATOR",
    "INITIALLY",
    "INNER",
    "INPUT",
    "INSENSITIVE",
    "INSERT",
    "INT",
    "INTEGER",
    "INTERSECT",
    "INTERVAL",
    "INTO",
    "IS",
    "ISOLATION",
    "JOIN",
    "KEY",
    "LANGUAGE",
    "LAST",
    "LEADING",
    "LEFT",
    "LEVEL",
    "LIKE",
    "LOCAL",
    "LOWER",
    "MATCH",
    "MAX",
    "MIN",
    "MINUTE",
    "MODULE",
    "MONTH",
    "NAMES",
    "NATIONAL",
    "NATURAL",
    "NCHAR",
    "NEXT",
    "NO",
    "NONE",
    "NOT",
    "NULL",
    "NULLIF",
    "NUMERIC",
    "OCTET_LENGTH",
    "OF",
    "ON",
    "ONLY",
    "OPEN",
    "OPTION",
    "OR",
    "ORDER",
    "OUTER",
    "OUTPUT",
    "OVERLAPS",
    "PAD",
    "PARTIAL",
    "PASCAL",
    "POSITION",
    "PRECISION",
    "PREPARE",
    "PRESERVE",
    "PRIMARY",
    "PRIOR",
    "PRIVILEGES",
    "PROCEDURE",
    "PUBLIC",
    "READ",
    "REAL",
    "REFERENCES",
    "RELATIVE",
    "RESTRICT",
    "REVOKE",
    "RIGHT",
    "ROLLBACK",
    "ROWS",
    "SCHEMA",
    "SCROLL",
    "SECOND",
    "SECTION",
    "SELECT",
    "SESSION",
    "SESSION_USER",
    "SET",
    "SIZE",
    "SMALLINT",
    "SOME",
    "SPACE",
    "SQL",
    "SQLCA",
    "SQLCODE",
    "SQLERROR",
    "SQLSTATE",
    "SQLWARNING",
    "SUBSTRING",
    "SUM",
    "SYSTEM_USER",
    "TABLE",
    "TEMPORARY",
    "THEN",
    "TIME",
    "TIMESTAMP",
    "TIMEZONE_HOUR",
    "TIMEZONE_MINUTE",
    "TO",
    "TRAILING",
    "TRANSACTION",
    "TRANSLATE",
    "TRANSLATION",
    "TRIM",
    "TRUE",
    "UNION",
    "UNIQUE",
    "UNKNOWN",
    "UPDATE",
    "UPPER",
    "USAGE",
    "USER",
    "USING",
    "VALUE",
    "VALUES",
    "VARCHAR",
    "VARYING",
    "VIEW",
    "WHEN",
    "WHENEVER",
    "WHERE",
    "WITH",
    "WORK",
    "WRITE",
    "YEAR",
    "ZONE",
];

#[cfg(test)]
mod tests {
    use super::*;

    /// These info-type constants are *derived* from `odbc_sys::InfoType`
    /// (e.g. `SQL_FILE_USAGE: u16 = InfoType::SqlFileUsage as u16`) rather
    /// than restating their spec numbers, so that odbc-sys stays the single
    /// source of truth for the value. But deriving them also means nothing
    /// checks the derivation is *right*: asserting a constant against the
    /// same expression that defines it (`assert_eq!(SQL_FILE_USAGE,
    /// InfoType::SqlFileUsage as u16)`) is a tautology: it cannot fail even
    /// if odbc-sys renumbers the variant tomorrow, because both sides of the
    /// assertion move together.
    ///
    /// This test deliberately breaks the project's usual "no spec literals
    /// in tests" rule (see AGENTS.md, "Named constants") by asserting each
    /// constant against the raw number from `/usr/include/sqlext.h`, the
    /// specification's own source. Here the literal *is* the check: it is
    /// the only thing in the workspace that ties an odbc-sys-derived value
    /// back to the ODBC spec, so it must fail loudly (by test name) if
    /// odbc-sys ever renumbers one of these variants.
    #[test]
    fn info_type_constants_match_sqlext_h() {
        assert_eq!(
            SQL_CURSOR_COMMIT_BEHAVIOR, 23,
            "SQL_CURSOR_COMMIT_BEHAVIOR (sqlext.h)"
        );
        assert_eq!(
            SQL_CURSOR_ROLLBACK_BEHAVIOR, 24,
            "SQL_CURSOR_ROLLBACK_BEHAVIOR (sqlext.h)"
        );
        assert_eq!(SQL_OUTER_JOINS, 38, "SQL_OUTER_JOINS (sqlext.h)");
        assert_eq!(
            SQL_NUMERIC_FUNCTIONS, 49,
            "SQL_NUMERIC_FUNCTIONS (sqlext.h)"
        );
        assert_eq!(SQL_STRING_FUNCTIONS, 50, "SQL_STRING_FUNCTIONS (sqlext.h)");
        assert_eq!(SQL_SYSTEM_FUNCTIONS, 51, "SQL_SYSTEM_FUNCTIONS (sqlext.h)");
        assert_eq!(
            SQL_TIMEDATE_FUNCTIONS, 52,
            "SQL_TIMEDATE_FUNCTIONS (sqlext.h)"
        );
        assert_eq!(SQL_FILE_USAGE, 84, "SQL_FILE_USAGE (sqlext.h)");
        assert_eq!(
            SQL_QUOTED_IDENTIFIER_CASE, 93,
            "SQL_QUOTED_IDENTIFIER_CASE (sqlext.h)"
        );
        assert_eq!(
            SQL_LIKE_ESCAPE_CLAUSE, 113,
            "SQL_LIKE_ESCAPE_CLAUSE (sqlext.h)"
        );
        assert_eq!(SQL_SQL92_PREDICATES, 160, "SQL_SQL92_PREDICATES (sqlext.h)");
        assert_eq!(
            SQL_SQL92_RELATIONAL_JOIN_OPERATORS, 161,
            "SQL_SQL92_RELATIONAL_JOIN_OPERATORS (sqlext.h)"
        );
        assert_eq!(
            SQL_SQL92_VALUE_EXPRESSIONS, 165,
            "SQL_SQL92_VALUE_EXPRESSIONS (sqlext.h)"
        );
        assert_eq!(
            SQL_AGGREGATE_FUNCTIONS, 169,
            "SQL_AGGREGATE_FUNCTIONS (sqlext.h)"
        );
    }

    /// The `SQL_SU_*` / `SQL_CU_*` / `SQL_CL_*` / `SQL_OJ_*` capability
    /// bitmaps are each asserted elsewhere only against the OR-expression
    /// used to build the driver's reported capability value, a tautology,
    /// since a typo in the constant's definition (e.g.
    /// `SQL_SU_PRIVILEGE_DEFINITION = 0x20` instead of `0x10`) moves both the
    /// implementation and that expectation together and is never caught. That
    /// is how a capability bitmap comes to report a value no header defines
    /// while every test around it stays green, so these are pinned against the
    /// headers directly.
    ///
    /// This test deliberately breaks the project's usual "no spec literals
    /// in tests" rule (see AGENTS.md, "Named constants") for the same reason
    /// as [`info_type_constants_match_sqlext_h`] above: the literal *is* the
    /// independent check that ties the named constant back to
    /// `/usr/include/sqlext.h` / `/usr/include/sql.h`, the specification's
    /// own source. Without it, nothing pins these flag values to the header.
    ///
    /// In the header, `SQL_SU_*`, `SQL_CU_*` and `SQL_CL_*` are `#define`d as
    /// aliases of the ODBC 3.x `SQL_OU_*`, `SQL_QU_*` and `SQL_QL_*` names
    /// respectively (e.g. `#define SQL_SU_DML_STATEMENTS
    /// SQL_OU_DML_STATEMENTS`); each assertion below uses the *resolved*
    /// numeric value, so a wrong alias target would still be caught.
    /// `SQL_OJ_*` is defined directly in `sql.h`, not `sqlext.h`.
    #[test]
    fn capability_flag_constants_match_sqlext_h() {
        // SQL_SCHEMA_USAGE (91) flags — sqlext.h aliases of SQL_OU_* (0x00000001..0x00000010)
        assert_eq!(
            SQL_SU_DML_STATEMENTS, 0x0000_0001,
            "SQL_SU_DML_STATEMENTS (sqlext.h, alias of SQL_OU_DML_STATEMENTS)"
        );
        assert_eq!(
            SQL_SU_PROCEDURE_INVOCATION, 0x0000_0002,
            "SQL_SU_PROCEDURE_INVOCATION (sqlext.h, alias of SQL_OU_PROCEDURE_INVOCATION)"
        );
        assert_eq!(
            SQL_SU_TABLE_DEFINITION, 0x0000_0004,
            "SQL_SU_TABLE_DEFINITION (sqlext.h, alias of SQL_OU_TABLE_DEFINITION)"
        );
        assert_eq!(
            SQL_SU_INDEX_DEFINITION, 0x0000_0008,
            "SQL_SU_INDEX_DEFINITION (sqlext.h, alias of SQL_OU_INDEX_DEFINITION)"
        );
        assert_eq!(
            SQL_SU_PRIVILEGE_DEFINITION, 0x0000_0010,
            "SQL_SU_PRIVILEGE_DEFINITION (sqlext.h, alias of SQL_OU_PRIVILEGE_DEFINITION)"
        );

        // SQL_CATALOG_USAGE (92) flags — sqlext.h aliases of SQL_QU_* (0x00000001..0x00000010)
        assert_eq!(
            SQL_CU_DML_STATEMENTS, 0x0000_0001,
            "SQL_CU_DML_STATEMENTS (sqlext.h, alias of SQL_QU_DML_STATEMENTS)"
        );
        assert_eq!(
            SQL_CU_PROCEDURE_INVOCATION, 0x0000_0002,
            "SQL_CU_PROCEDURE_INVOCATION (sqlext.h, alias of SQL_QU_PROCEDURE_INVOCATION)"
        );
        assert_eq!(
            SQL_CU_TABLE_DEFINITION, 0x0000_0004,
            "SQL_CU_TABLE_DEFINITION (sqlext.h, alias of SQL_QU_TABLE_DEFINITION)"
        );
        assert_eq!(
            SQL_CU_INDEX_DEFINITION, 0x0000_0008,
            "SQL_CU_INDEX_DEFINITION (sqlext.h, alias of SQL_QU_INDEX_DEFINITION)"
        );
        assert_eq!(
            SQL_CU_PRIVILEGE_DEFINITION, 0x0000_0010,
            "SQL_CU_PRIVILEGE_DEFINITION (sqlext.h, alias of SQL_QU_PRIVILEGE_DEFINITION)"
        );

        // SQL_CATALOG_LOCATION (114) values — sqlext.h aliases of SQL_QL_*
        assert_eq!(
            SQL_CL_START, 0x0001,
            "SQL_CL_START (sqlext.h, alias of SQL_QL_START)"
        );
        assert_eq!(
            SQL_CL_END, 0x0002,
            "SQL_CL_END (sqlext.h, alias of SQL_QL_END)"
        );

        // SQL_OJ_CAPABILITIES (115) flags — sql.h (not sqlext.h)
        assert_eq!(SQL_OJ_LEFT, 0x0000_0001, "SQL_OJ_LEFT (sql.h)");
        assert_eq!(SQL_OJ_RIGHT, 0x0000_0002, "SQL_OJ_RIGHT (sql.h)");
        assert_eq!(SQL_OJ_FULL, 0x0000_0004, "SQL_OJ_FULL (sql.h)");
        assert_eq!(SQL_OJ_NESTED, 0x0000_0008, "SQL_OJ_NESTED (sql.h)");
        assert_eq!(
            SQL_OJ_NOT_ORDERED, 0x0000_0010,
            "SQL_OJ_NOT_ORDERED (sql.h)"
        );
        assert_eq!(SQL_OJ_INNER, 0x0000_0020, "SQL_OJ_INNER (sql.h)");
        assert_eq!(
            SQL_OJ_ALL_COMPARISON_OPS, 0x0000_0040,
            "SQL_OJ_ALL_COMPARISON_OPS (sql.h)"
        );
    }

    /// The `SQL_AT_*` bitmasks for `SQL_ALTER_TABLE` (86), asserted against the
    /// C headers for the same reason as
    /// [`capability_flag_constants_match_sqlext_h`] above: nothing else in the
    /// workspace ties them back to the specification's own source, and a
    /// driver that reports its `ALTER TABLE` support by name inherits any typo
    /// silently.
    ///
    /// The set is split across two headers. `SQL_AT_ADD_COLUMN`,
    /// `SQL_AT_DROP_COLUMN` and `SQL_AT_ADD_CONSTRAINT` are live `#define`s in
    /// `/usr/include/sql.h` (and commented out in `sqlext.h`, which points at
    /// sql.h for them); every other bit is a live `#define` in
    /// `/usr/include/sqlext.h` (and commented out in `sql.h`). Each assertion
    /// below names the header the value is actually defined in.
    #[test]
    fn sql_at_constants_match_sql_headers() {
        // Defined in sql.h; sqlext.h comments these out and defers to sql.h.
        assert_eq!(SQL_AT_ADD_COLUMN, 0x0000_0001, "SQL_AT_ADD_COLUMN (sql.h)");
        assert_eq!(
            SQL_AT_DROP_COLUMN, 0x0000_0002,
            "SQL_AT_DROP_COLUMN (sql.h)"
        );
        assert_eq!(
            SQL_AT_ADD_CONSTRAINT, 0x0000_0008,
            "SQL_AT_ADD_CONSTRAINT (sql.h)"
        );

        // Defined in sqlext.h; sql.h comments these out and defers to sqlext.h.
        assert_eq!(
            SQL_AT_ADD_COLUMN_SINGLE, 0x0000_0020,
            "SQL_AT_ADD_COLUMN_SINGLE (sqlext.h)"
        );
        assert_eq!(
            SQL_AT_ADD_COLUMN_DEFAULT, 0x0000_0040,
            "SQL_AT_ADD_COLUMN_DEFAULT (sqlext.h)"
        );
        assert_eq!(
            SQL_AT_ADD_COLUMN_COLLATION, 0x0000_0080,
            "SQL_AT_ADD_COLUMN_COLLATION (sqlext.h)"
        );
        assert_eq!(
            SQL_AT_SET_COLUMN_DEFAULT, 0x0000_0100,
            "SQL_AT_SET_COLUMN_DEFAULT (sqlext.h)"
        );
        assert_eq!(
            SQL_AT_DROP_COLUMN_DEFAULT, 0x0000_0200,
            "SQL_AT_DROP_COLUMN_DEFAULT (sqlext.h)"
        );
        assert_eq!(
            SQL_AT_DROP_COLUMN_CASCADE, 0x0000_0400,
            "SQL_AT_DROP_COLUMN_CASCADE (sqlext.h)"
        );
        assert_eq!(
            SQL_AT_DROP_COLUMN_RESTRICT, 0x0000_0800,
            "SQL_AT_DROP_COLUMN_RESTRICT (sqlext.h)"
        );
        assert_eq!(
            SQL_AT_ADD_TABLE_CONSTRAINT, 0x0000_1000,
            "SQL_AT_ADD_TABLE_CONSTRAINT (sqlext.h)"
        );
        assert_eq!(
            SQL_AT_DROP_TABLE_CONSTRAINT_CASCADE, 0x0000_2000,
            "SQL_AT_DROP_TABLE_CONSTRAINT_CASCADE (sqlext.h)"
        );
        assert_eq!(
            SQL_AT_DROP_TABLE_CONSTRAINT_RESTRICT, 0x0000_4000,
            "SQL_AT_DROP_TABLE_CONSTRAINT_RESTRICT (sqlext.h)"
        );
        assert_eq!(
            SQL_AT_CONSTRAINT_NAME_DEFINITION, 0x0000_8000,
            "SQL_AT_CONSTRAINT_NAME_DEFINITION (sqlext.h)"
        );
        assert_eq!(
            SQL_AT_CONSTRAINT_INITIALLY_DEFERRED, 0x0001_0000,
            "SQL_AT_CONSTRAINT_INITIALLY_DEFERRED (sqlext.h)"
        );
        assert_eq!(
            SQL_AT_CONSTRAINT_INITIALLY_IMMEDIATE, 0x0002_0000,
            "SQL_AT_CONSTRAINT_INITIALLY_IMMEDIATE (sqlext.h)"
        );
        assert_eq!(
            SQL_AT_CONSTRAINT_DEFERRABLE, 0x0004_0000,
            "SQL_AT_CONSTRAINT_DEFERRABLE (sqlext.h)"
        );
        assert_eq!(
            SQL_AT_CONSTRAINT_NON_DEFERRABLE, 0x0008_0000,
            "SQL_AT_CONSTRAINT_NON_DEFERRABLE (sqlext.h)"
        );
    }

    /// The enum values behind the info types that `Backend` now requires a
    /// backend to state (`SQL_NULL_COLLATION`, `SQL_CORRELATION_NAME`,
    /// `SQL_NON_NULLABLE_COLUMNS`, `SQL_GROUP_BY`, `SQL_SQL_CONFORMANCE`) plus
    /// the `SQL_TIMEDATE_*_INTERVALS` bitmaps, asserted against the C headers.
    ///
    /// These are the values where **zero is a substantive answer**, not
    /// "unknown": `SQL_NC_HIGH`, `SQL_CN_NONE` and `SQL_NNC_NULL` are all `0`.
    /// A typo in one of them therefore cannot show up as an obviously-empty
    /// value the way a wrong bitmask flag would — it shows up as a different,
    /// equally plausible claim. Same rationale as
    /// [`capability_flag_constants_match_sqlext_h`]: the literal is the only
    /// independent check tying the named constant to the specification's own
    /// source.
    #[test]
    fn info_type_value_constants_match_sql_headers() {
        // SQL_NULL_COLLATION (85) — SQL_NC_HIGH/LOW are sql.h, START/END sqlext.h
        assert_eq!(SQL_NC_HIGH, 0, "SQL_NC_HIGH (sql.h)");
        assert_eq!(SQL_NC_LOW, 1, "SQL_NC_LOW (sql.h)");
        assert_eq!(SQL_NC_START, 0x0002, "SQL_NC_START (sqlext.h)");
        assert_eq!(SQL_NC_END, 0x0004, "SQL_NC_END (sqlext.h)");

        // SQL_CONCAT_NULL_BEHAVIOR (22) values — sqlext.h
        assert_eq!(SQL_CB_NULL, 0x0000, "SQL_CB_NULL (sqlext.h)");
        assert_eq!(SQL_CB_NON_NULL, 0x0001, "SQL_CB_NON_NULL (sqlext.h)");

        // SQL_CONVERT_FUNCTIONS (48) values — sqlext.h
        assert_eq!(
            SQL_FN_CVT_CONVERT, 0x0000_0001,
            "SQL_FN_CVT_CONVERT (sqlext.h)"
        );
        assert_eq!(SQL_FN_CVT_CAST, 0x0000_0002, "SQL_FN_CVT_CAST (sqlext.h)");

        // SQL_CORRELATION_NAME (74) values — sqlext.h
        assert_eq!(SQL_CN_NONE, 0x0000, "SQL_CN_NONE (sqlext.h)");
        assert_eq!(SQL_CN_DIFFERENT, 0x0001, "SQL_CN_DIFFERENT (sqlext.h)");
        assert_eq!(SQL_CN_ANY, 0x0002, "SQL_CN_ANY (sqlext.h)");

        // SQL_NON_NULLABLE_COLUMNS (75) values — sqlext.h
        assert_eq!(SQL_NNC_NULL, 0x0000, "SQL_NNC_NULL (sqlext.h)");
        assert_eq!(SQL_NNC_NON_NULL, 0x0001, "SQL_NNC_NON_NULL (sqlext.h)");

        // SQL_GROUP_BY (88) values — sqlext.h
        assert_eq!(
            SQL_GB_NOT_SUPPORTED, 0x0000,
            "SQL_GB_NOT_SUPPORTED (sqlext.h)"
        );
        assert_eq!(
            SQL_GB_GROUP_BY_EQUALS_SELECT, 0x0001,
            "SQL_GB_GROUP_BY_EQUALS_SELECT (sqlext.h)"
        );
        assert_eq!(
            SQL_GB_GROUP_BY_CONTAINS_SELECT, 0x0002,
            "SQL_GB_GROUP_BY_CONTAINS_SELECT (sqlext.h)"
        );
        assert_eq!(SQL_GB_NO_RELATION, 0x0003, "SQL_GB_NO_RELATION (sqlext.h)");
        assert_eq!(SQL_GB_COLLATE, 0x0004, "SQL_GB_COLLATE (sqlext.h)");

        // SQL_SQL_CONFORMANCE (118) values — sqlext.h
        assert_eq!(
            SQL_SC_SQL92_ENTRY, 0x0000_0001,
            "SQL_SC_SQL92_ENTRY (sqlext.h)"
        );
        assert_eq!(
            SQL_SC_FIPS127_2_TRANSITIONAL, 0x0000_0002,
            "SQL_SC_FIPS127_2_TRANSITIONAL (sqlext.h)"
        );
        assert_eq!(
            SQL_SC_SQL92_INTERMEDIATE, 0x0000_0004,
            "SQL_SC_SQL92_INTERMEDIATE (sqlext.h)"
        );
        assert_eq!(
            SQL_SC_SQL92_FULL, 0x0000_0008,
            "SQL_SC_SQL92_FULL (sqlext.h)"
        );

        // SQL_TIMEDATE_ADD_INTERVALS (109) / SQL_TIMEDATE_DIFF_INTERVALS (110)
        // flags — sqlext.h. Both info types use this one set of values.
        assert_eq!(
            SQL_FN_TSI_FRAC_SECOND, 0x0000_0001,
            "SQL_FN_TSI_FRAC_SECOND (sqlext.h)"
        );
        assert_eq!(
            SQL_FN_TSI_SECOND, 0x0000_0002,
            "SQL_FN_TSI_SECOND (sqlext.h)"
        );
        assert_eq!(
            SQL_FN_TSI_MINUTE, 0x0000_0004,
            "SQL_FN_TSI_MINUTE (sqlext.h)"
        );
        assert_eq!(SQL_FN_TSI_HOUR, 0x0000_0008, "SQL_FN_TSI_HOUR (sqlext.h)");
        assert_eq!(SQL_FN_TSI_DAY, 0x0000_0010, "SQL_FN_TSI_DAY (sqlext.h)");
        assert_eq!(SQL_FN_TSI_WEEK, 0x0000_0020, "SQL_FN_TSI_WEEK (sqlext.h)");
        assert_eq!(SQL_FN_TSI_MONTH, 0x0000_0040, "SQL_FN_TSI_MONTH (sqlext.h)");
        assert_eq!(
            SQL_FN_TSI_QUARTER, 0x0000_0080,
            "SQL_FN_TSI_QUARTER (sqlext.h)"
        );
        assert_eq!(SQL_FN_TSI_YEAR, 0x0000_0100, "SQL_FN_TSI_YEAR (sqlext.h)");

        // Info type numbers with no odbc_sys::InfoType variant — sqlext.h.
        // Everything else in this file derives its number from odbc-sys; these
        // two restate it, so they need the same header check as the values.
        assert_eq!(SQL_ROW_UPDATES, 11, "SQL_ROW_UPDATES (sqlext.h)");
        assert_eq!(SQL_PROCEDURES, 21, "SQL_PROCEDURES (sqlext.h)");
        assert_eq!(
            SQL_MULTIPLE_ACTIVE_TXN, 37,
            "SQL_MULTIPLE_ACTIVE_TXN (sqlext.h)"
        );
        assert_eq!(SQL_DATABASE_NAME, 16, "SQL_DATABASE_NAME (sqlext.h)");
        assert_eq!(SQL_PROCEDURE_TERM, 40, "SQL_PROCEDURE_TERM (sqlext.h)");
        assert_eq!(SQL_TABLE_TERM, 45, "SQL_TABLE_TERM (sqlext.h)");
        assert_eq!(SQL_KEYWORDS, 89, "SQL_KEYWORDS (sqlext.h)");
    }

    /// A duplicate in [`ODBC_RESERVED_KEYWORDS`] is invisible at a glance in a
    /// list this long and harmless to the subtraction itself, but it means the
    /// list was transcribed with an error — and the next error may be a
    /// *missing* entry, which silently leaks an ODBC keyword into
    /// `SQL_KEYWORDS`.
    #[test]
    fn odbc_reserved_keywords_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for k in ODBC_RESERVED_KEYWORDS {
            assert!(
                seen.insert(*k),
                "{k} appears twice in ODBC_RESERVED_KEYWORDS"
            );
        }
    }
}
