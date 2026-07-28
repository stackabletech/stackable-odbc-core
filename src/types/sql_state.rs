//! The [`SqlState`] type and SQLSTATE code constants.
//!
//! The named constants below are the single source of truth for every
//! five-character SQLSTATE string used by this driver. Every factory method
//! on [`SqlState`] delegates to them, and every test assertion references
//! them instead of raw literals.

use std::fmt;

// ---------------------------------------------------------------------------
// SQLSTATE constants
// ---------------------------------------------------------------------------

/// General error — HY000
pub const GENERAL_ERROR: &str = "HY000";

/// Optional feature not implemented — HYC00
pub const OPTIONAL_FEATURE_NOT_IMPLEMENTED: &str = "HYC00";

/// Option value changed — 01S02 (warning; function returns SQL_SUCCESS_WITH_INFO)
pub const OPTION_VALUE_CHANGED: &str = "01S02";

/// Client unable to establish connection — 08001
///
/// Only valid from the connection functions (`SQLConnect`,
/// `SQLDriverConnect`, `SQLBrowseConnect`). A link that fails *after* the
/// connection was established is [`COMMUNICATION_LINK_FAILURE`].
pub const CLIENT_UNABLE_TO_ESTABLISH_CONNECTION: &str = "08001";

/// Communication link failure — 08S01
///
/// The link between the driver and the data source failed before the function
/// completed processing. Unlike [`CLIENT_UNABLE_TO_ESTABLISH_CONNECTION`] this
/// applies once a connection exists, and it is the code listed by the
/// diagnostics tables of `SQLExecute`, `SQLFetch`, `SQLGetInfo` and friends.
pub const COMMUNICATION_LINK_FAILURE: &str = "08S01";

/// Connection name in use — 08002
pub const CONNECTION_IN_USE: &str = "08002";

/// Connection not open — 08003
pub const CONNECTION_NOT_OPEN: &str = "08003";

/// Prepared statement not a cursor-specification — 07005
pub const PREPARED_STATEMENT_NOT_CURSOR_SPEC: &str = "07005";

/// Restricted data type attribute violation — 07006
pub const RESTRICTED_DATA_TYPE_ATTRIBUTE_VIOLATION: &str = "07006";

/// Integrity constraint violation — 23000
pub const INTEGRITY_CONSTRAINT_VIOLATION: &str = "23000";

/// Syntax error or access violation — 42000
pub const SYNTAX_ERROR_OR_ACCESS_VIOLATION: &str = "42000";

/// Base table or view not found — 42S02
pub const BASE_TABLE_OR_VIEW_NOT_FOUND: &str = "42S02";

/// Column not found — 42S22
pub const COLUMN_NOT_FOUND: &str = "42S22";

/// Invalid descriptor index — 07009
pub const INVALID_DESCRIPTOR_INDEX: &str = "07009";

/// String data, right truncated — 01004
pub const STRING_DATA_RIGHT_TRUNCATED: &str = "01004";

/// Indicator variable required but not supplied — 22002
pub const INDICATOR_VARIABLE_REQUIRED: &str = "22002";

/// Invalid cursor state — 24000
pub const INVALID_CURSOR_STATE: &str = "24000";

/// Invalid authorization specification — 28000
pub const INVALID_AUTH_SPEC: &str = "28000";

/// Invalid application buffer type — HY003
pub const INVALID_APPLICATION_BUFFER_TYPE: &str = "HY003";

/// Operation canceled — HY008
pub const OPERATION_CANCELED: &str = "HY008";

/// Invalid use of null pointer — HY009
pub const INVALID_USE_OF_NULL_POINTER: &str = "HY009";

/// Function sequence error — HY010
pub const FUNCTION_SEQUENCE_ERROR: &str = "HY010";

/// Attribute cannot be set now — HY011
pub const ATTRIBUTE_CANNOT_BE_SET_NOW: &str = "HY011";

/// Invalid attribute value — HY024
pub const INVALID_ATTRIBUTE_VALUE: &str = "HY024";

/// Invalid string or buffer length — HY090
pub const INVALID_STRING_OR_BUFFER_LENGTH: &str = "HY090";

/// Invalid attribute/option identifier — HY092
pub const INVALID_ATTRIBUTE_OPTION_IDENTIFIER: &str = "HY092";

/// Invalid parameter type — HY105
pub const INVALID_PARAMETER_TYPE: &str = "HY105";

/// Fetch type out of range — HY106
pub const FETCH_TYPE_OUT_OF_RANGE: &str = "HY106";

/// Numeric value out of range — 22003
pub const NUMERIC_VALUE_OUT_OF_RANGE: &str = "22003";

/// Invalid character value for cast specification — 22018
pub const INVALID_CHARACTER_VALUE_FOR_CAST: &str = "22018";

/// COUNT field incorrect — 07002
pub const COUNT_FIELD_INCORRECT: &str = "07002";

/// String data, right truncation — 22001
pub const STRING_DATA_RIGHT_TRUNCATION: &str = "22001";

/// Datetime field overflow — 22008
pub const DATETIME_FIELD_OVERFLOW: &str = "22008";

/// Invalid datetime format — 22007
pub const INVALID_DATETIME_FORMAT: &str = "22007";

/// Fractional truncation — 01S07
pub const FRACTIONAL_TRUNCATION: &str = "01S07";

/// Timeout expired — HYT00
pub const TIMEOUT_EXPIRED: &str = "HYT00";

// ---------------------------------------------------------------------------
// SqlState type
// ---------------------------------------------------------------------------

/// A five-character ODBC diagnostic state code (e.g. `"HY000"`).
///
/// The five bytes are private. They were public, which froze `[u8; 5]` as API
/// and let a driver build a `SqlState` that was not five ASCII characters —
/// making [`SqlState::as_str`]'s otherwise-unreachable `"?????"` fallback
/// reachable. Build one with [`SqlState::new`] from a literal, with
/// `TryFrom<&str>` when the value is not known at compile time, or from one of
/// the named factory methods.
///
/// `PartialEq` is what lets a driver's tests say
/// `assert_eq!(err.sqlstate(), SqlState::general_error())` rather than
/// comparing strings.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SqlState([u8; 5]);

/// A string that is not a valid five-character ASCII SQLSTATE.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidSqlState {
    /// The rejected input.
    pub input: String,
}

impl fmt::Display for InvalidSqlState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "SQLSTATE must be exactly 5 ASCII characters, got {:?}",
            self.input
        )
    }
}

impl std::error::Error for InvalidSqlState {}

impl TryFrom<&str> for SqlState {
    type Error = InvalidSqlState;

    /// The checked constructor, for a SQLSTATE that is not a literal — one read
    /// from a data source's own error payload, say.
    fn try_from(s: &str) -> Result<Self, Self::Error> {
        if s.len() == 5 && s.is_ascii() {
            let mut buf = [0u8; 5];
            buf.copy_from_slice(s.as_bytes());
            Ok(Self(buf))
        } else {
            Err(InvalidSqlState {
                input: s.to_string(),
            })
        }
    }
}

impl SqlState {
    /// Creates a `SqlState` from a string slice.
    ///
    /// `s` must be exactly 5 ASCII characters. In debug builds a violated
    /// assertion will panic; in release builds shorter strings are zero-padded
    /// (producing an invalid state), so callers must always pass a valid 5-char
    /// SQLSTATE literal. All factory methods on this type satisfy this contract.
    ///
    /// Use `SqlState::try_from(s)` instead when `s` is not a literal.
    pub fn new(s: &str) -> Self {
        debug_assert!(
            s.len() == 5 && s.is_ascii(),
            "SQLSTATE must be exactly 5 ASCII characters, got {:?}",
            s
        );
        let bytes = s.as_bytes();
        let mut buf = [0u8; 5];
        let len = bytes.len().min(5);
        buf[..len].copy_from_slice(&bytes[..len]);
        Self(buf)
    }

    /// Returns the SQLSTATE as a `&str` (always 5 ASCII bytes).
    pub fn as_str(&self) -> &str {
        // `from_utf8` always succeeds here: the bytes are always 5 ASCII
        // characters. Clippy forbids `unwrap`, so `unwrap_or` supplies an
        // unreachable fallback.
        std::str::from_utf8(&self.0).unwrap_or("?????")
    }

    /// General error — HY000
    pub fn general_error() -> Self {
        Self::new(GENERAL_ERROR)
    }

    /// Optional feature not implemented — HYC00
    pub fn optional_feature_not_implemented() -> Self {
        Self::new(OPTIONAL_FEATURE_NOT_IMPLEMENTED)
    }

    /// Option value changed — 01S02
    ///
    /// A warning: the driver did not support the requested value and
    /// substituted a similar one. The caller returns `SQL_SUCCESS_WITH_INFO`.
    pub fn option_value_changed() -> Self {
        Self::new(OPTION_VALUE_CHANGED)
    }

    /// Connection not open — 08003
    pub fn connection_not_open() -> Self {
        Self::new(CONNECTION_NOT_OPEN)
    }

    /// Invalid cursor state — 24000
    pub fn invalid_cursor_state() -> Self {
        Self::new(INVALID_CURSOR_STATE)
    }

    /// String data, right truncated — 01004
    pub fn string_data_right_truncated() -> Self {
        Self::new(STRING_DATA_RIGHT_TRUNCATED)
    }

    /// Invalid attribute value — HY024
    pub fn invalid_attribute_value() -> Self {
        Self::new(INVALID_ATTRIBUTE_VALUE)
    }

    /// Function sequence error — HY010
    pub fn function_sequence_error() -> Self {
        Self::new(FUNCTION_SEQUENCE_ERROR)
    }

    /// Operation canceled — HY008
    ///
    /// Returned by a statement-level call that a `SQLCancel` on another thread
    /// interrupted, which is the second of the two clauses the spec's `HY008`
    /// row states: the function "was called, and before it completed execution,
    /// `SQLCancel` … was called on the `StatementHandle` from a different
    /// thread in a multithread application".
    ///
    /// The row's first clause — asynchronous processing, then the function
    /// called again — cannot arise here: core implements no asynchronous
    /// execution and never returns `SQL_STILL_EXECUTING`.
    pub fn operation_canceled() -> Self {
        Self::new(OPERATION_CANCELED)
    }

    /// Connection name in use — 08002
    pub fn connection_in_use() -> Self {
        Self::new(CONNECTION_IN_USE)
    }

    /// Invalid string or buffer length — HY090
    pub fn invalid_string_or_buffer_length() -> Self {
        Self::new(INVALID_STRING_OR_BUFFER_LENGTH)
    }

    /// Invalid use of null pointer — HY009
    pub fn invalid_use_of_null_pointer() -> Self {
        Self::new(INVALID_USE_OF_NULL_POINTER)
    }

    /// Invalid descriptor index — 07009
    pub fn invalid_descriptor_index() -> Self {
        Self::new(INVALID_DESCRIPTOR_INDEX)
    }

    /// Invalid application buffer type — HY003
    pub fn invalid_application_buffer_type() -> Self {
        Self::new(INVALID_APPLICATION_BUFFER_TYPE)
    }

    /// Invalid attribute/option identifier — HY092
    pub fn invalid_attribute_option_identifier() -> Self {
        Self::new(INVALID_ATTRIBUTE_OPTION_IDENTIFIER)
    }

    /// Invalid parameter type — HY105
    pub fn invalid_parameter_type() -> Self {
        Self::new(INVALID_PARAMETER_TYPE)
    }

    /// Fetch type out of range — HY106
    pub fn fetch_type_out_of_range() -> Self {
        Self::new(FETCH_TYPE_OUT_OF_RANGE)
    }

    /// Indicator variable required but not supplied — 22002
    pub fn indicator_variable_required() -> Self {
        Self::new(INDICATOR_VARIABLE_REQUIRED)
    }

    /// Attribute cannot be set now — HY011
    pub fn attribute_cannot_be_set_now() -> Self {
        Self::new(ATTRIBUTE_CANNOT_BE_SET_NOW)
    }

    /// Client unable to establish connection — 08001
    ///
    /// Use only from the connection functions. Once a connection exists, a
    /// failing link is [`SqlState::communication_link_failure`].
    pub fn client_unable_to_establish_connection() -> Self {
        Self::new(CLIENT_UNABLE_TO_ESTABLISH_CONNECTION)
    }

    /// Communication link failure — 08S01
    pub fn communication_link_failure() -> Self {
        Self::new(COMMUNICATION_LINK_FAILURE)
    }

    /// Numeric value out of range — 22003
    pub fn numeric_value_out_of_range() -> Self {
        Self::new(NUMERIC_VALUE_OUT_OF_RANGE)
    }

    /// COUNT field incorrect — 07002
    ///
    /// Returned when the statement contains more parameter markers than the
    /// application bound values for, which is the first clause of the `07002`
    /// row on the `SQLExecute` and `SQLExecDirect` diagnostics tables: "The
    /// number of parameters specified in `SQLBindParameter` was less than the
    /// number of parameters in the SQL statement". Neither row carries a
    /// `(DM)` marker, so this is the driver's to report.
    pub fn count_field_incorrect() -> Self {
        Self::new(COUNT_FIELD_INCORRECT)
    }

    /// String data, right truncation — 22001
    ///
    /// The error-severity truncation code, as distinct from the
    /// warning-severity `01004` that [`SqlState::string_data_right_truncated`]
    /// carries. The C-to-SQL conversion tables use this one: converting
    /// character parameter data to a numeric SQL type that cannot hold all of
    /// its digits loses data the application meant to send, so the value is
    /// not sent at all.
    pub fn string_data_right_truncation() -> Self {
        Self::new(STRING_DATA_RIGHT_TRUNCATION)
    }

    /// Datetime field overflow — 22008
    pub fn datetime_field_overflow() -> Self {
        Self::new(DATETIME_FIELD_OVERFLOW)
    }

    /// Invalid character value for cast specification — 22018
    ///
    /// The character data could not be converted to the C type the application
    /// requested, for example `SQL_C_SLONG` against a column holding `"abc"`.
    pub fn invalid_character_value_for_cast() -> Self {
        Self::new(INVALID_CHARACTER_VALUE_FOR_CAST)
    }

    /// Invalid datetime format — 22007
    ///
    /// The character column in the result set was bound to a C date, time, or
    /// timestamp structure, and the value in the column was not a valid
    /// date/time/timestamp, for example a field parsed but was out of range
    /// (month 13, day 32). Per the `SQLGetData` diagnostics table this code
    /// (not 22008, which the table does not list) is scoped to a character
    /// column source.
    pub fn invalid_datetime_format() -> Self {
        Self::new(INVALID_DATETIME_FORMAT)
    }

    /// Fractional truncation — 01S07
    pub fn fractional_truncation() -> Self {
        Self::new(FRACTIONAL_TRUNCATION)
    }

    /// Timeout expired — HYT00
    pub fn timeout_expired() -> Self {
        Self::new(TIMEOUT_EXPIRED)
    }

    /// Invalid authorization specification — 28000
    pub fn invalid_auth_spec() -> Self {
        Self::new(INVALID_AUTH_SPEC)
    }

    /// Prepared statement not a cursor-specification — 07005
    pub fn prepared_statement_not_cursor_spec() -> Self {
        Self::new(PREPARED_STATEMENT_NOT_CURSOR_SPEC)
    }

    /// Restricted data type attribute violation — 07006
    ///
    /// The value could not be converted to the C or SQL type the application
    /// asked for.
    pub fn restricted_data_type_attribute_violation() -> Self {
        Self::new(RESTRICTED_DATA_TYPE_ATTRIBUTE_VIOLATION)
    }

    /// Integrity constraint violation — 23000
    ///
    /// A primary key, foreign key, unique, check or NOT NULL constraint was
    /// violated by the statement.
    pub fn integrity_constraint_violation() -> Self {
        Self::new(INTEGRITY_CONSTRAINT_VIOLATION)
    }

    /// Syntax error or access violation — 42000
    pub fn syntax_error_or_access_violation() -> Self {
        Self::new(SYNTAX_ERROR_OR_ACCESS_VIOLATION)
    }

    /// Base table or view not found — 42S02
    pub fn base_table_or_view_not_found() -> Self {
        Self::new(BASE_TABLE_OR_VIEW_NOT_FOUND)
    }

    /// Column not found — 42S22
    pub fn column_not_found() -> Self {
        Self::new(COLUMN_NOT_FOUND)
    }
}

impl fmt::Debug for SqlState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SqlState(\"{}\")", self.as_str())
    }
}

impl fmt::Display for SqlState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlstate_roundtrip() {
        let state = SqlState::new(GENERAL_ERROR);
        assert_eq!(state.as_str(), GENERAL_ERROR);
    }

    #[test]
    fn operation_canceled_is_hy008() {
        // Spec, SQLCancel: "If the original function is canceled, it returns
        // SQL_ERROR and SQLSTATE HY008 (Operation canceled)."
        assert_eq!(SqlState::operation_canceled().as_str(), "HY008");
    }

    #[test]
    fn try_from_accepts_a_five_character_ascii_state() {
        let state = SqlState::try_from("08S01").expect("valid SQLSTATE");
        assert_eq!(state.as_str(), "08S01");
    }

    #[test]
    fn try_from_rejects_anything_that_is_not_five_ascii_characters() {
        // The reason the byte array is private: a public one makes every one of
        // these constructible, and each makes `as_str` return its "?????"
        // fallback in place of a diagnostic code.
        for bad in ["", "HY0", "HY0000", "HY00é"] {
            let err = SqlState::try_from(bad).expect_err("must reject {bad:?}");
            assert_eq!(err.input, bad);
        }
    }

    #[test]
    fn equality_lets_a_driver_compare_states_without_strings() {
        // The missing `PartialEq` was why no driver could write
        // `assert_eq!(err.sqlstate(), SqlState::general_error())`.
        assert_eq!(SqlState::general_error(), SqlState::new(GENERAL_ERROR));
        assert_ne!(SqlState::general_error(), SqlState::connection_not_open());
    }

    #[test]
    fn states_can_be_hashed_and_used_as_map_keys() {
        let set: std::collections::HashSet<SqlState> =
            [SqlState::general_error(), SqlState::general_error()]
                .into_iter()
                .collect();
        assert_eq!(set.len(), 1, "equal states must hash equally");
    }

    #[test]
    fn cast_and_datetime_sqlstates() {
        assert_eq!(
            SqlState::invalid_character_value_for_cast().as_str(),
            INVALID_CHARACTER_VALUE_FOR_CAST
        );
        assert_eq!(
            SqlState::invalid_datetime_format().as_str(),
            INVALID_DATETIME_FORMAT
        );
        assert_eq!(INVALID_CHARACTER_VALUE_FOR_CAST, "22018");
        assert_eq!(INVALID_DATETIME_FORMAT, "22007");
    }
}
