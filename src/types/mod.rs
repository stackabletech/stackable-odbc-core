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

mod constants;
pub use constants::*;

// ---------------------------------------------------------------------------
// Conversions for odbc-sys types
// ---------------------------------------------------------------------------

mod conversions;
pub use conversions::*;

mod value;
pub use value::{
    ColumnDescriptor, ColumnValue, ExecuteOutcome, FetchResult, IdentifierType, Nullable,
    OutputParam, Scope, TypeInfoRow,
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

mod connect_params;
pub use connect_params::ConnectParams;

mod redacted;
pub use redacted::Redacted;

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
