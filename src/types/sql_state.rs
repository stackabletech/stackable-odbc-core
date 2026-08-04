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

/// General error (HY000).
pub const GENERAL_ERROR: &str = "HY000";

/// Optional feature not implemented (HYC00).
pub const OPTIONAL_FEATURE_NOT_IMPLEMENTED: &str = "HYC00";

/// Option value changed (01S02). A warning, so the function returns
/// SQL_SUCCESS_WITH_INFO.
pub const OPTION_VALUE_CHANGED: &str = "01S02";

/// Client unable to establish connection (08001).
///
/// Only valid from the connection functions (`SQLConnect`,
/// `SQLDriverConnect`, `SQLBrowseConnect`). A link that fails *after* the
/// connection was established is [`COMMUNICATION_LINK_FAILURE`].
pub const CLIENT_UNABLE_TO_ESTABLISH_CONNECTION: &str = "08001";

/// Communication link failure (08S01).
///
/// The link between the driver and the data source failed before the function
/// completed processing. Unlike [`CLIENT_UNABLE_TO_ESTABLISH_CONNECTION`] this
/// applies once a connection exists, and it is the code listed by the
/// diagnostics tables of `SQLExecute`, `SQLFetch`, `SQLGetInfo` and friends.
pub const COMMUNICATION_LINK_FAILURE: &str = "08S01";

/// Invalid catalog name (3D000).
///
/// `SQLSetConnectAttr`'s row is "the *Attribute* argument was
/// SQL_CURRENT_CATALOG, and the specified catalog name was invalid", and it
/// carries no `(DM)` marker, so it is the driver's to return. Core cannot
/// produce it: only the data source knows which catalogs exist, and the
/// attribute's own description has the driver send something to it: "in SQL
/// Server, the catalog is a database, so the driver sends a **USE** *database*
/// statement". A backend's [`crate::backend::Backend::set_current_catalog`]
/// maps "no such catalog" to this, and core propagates it unchanged.
///
/// Also listed by `SQLExecDirect` and `SQLPrepare`, for a catalog named in the
/// SQL text itself.
pub const INVALID_CATALOG_NAME: &str = "3D000";

/// Invalid cursor name (34000).
///
/// `SQLSetCursorName`: the name "exceeded the maximum length as defined by the
/// driver, or it started with `SQLCUR` or `SQL_CUR`". Those two prefixes are
/// reserved for the driver's own generated names, which is why an application
/// may not claim one.
pub const INVALID_CURSOR_NAME: &str = "34000";

/// Duplicate cursor name (3C000).
///
/// `SQLSetCursorName`: "the cursor name specified in \*CursorName already
/// exists". The scope is the connection: "All cursor names within the
/// connection must be unique."
pub const DUPLICATE_CURSOR_NAME: &str = "3C000";

/// Connection name in use (08002).
pub const CONNECTION_IN_USE: &str = "08002";

/// Connection not open (08003).
pub const CONNECTION_NOT_OPEN: &str = "08003";

/// Prepared statement not a cursor-specification (07005).
pub const PREPARED_STATEMENT_NOT_CURSOR_SPEC: &str = "07005";

/// Restricted data type attribute violation (07006).
pub const RESTRICTED_DATA_TYPE_ATTRIBUTE_VIOLATION: &str = "07006";

/// Integrity constraint violation (23000).
pub const INTEGRITY_CONSTRAINT_VIOLATION: &str = "23000";

/// Syntax error or access violation (42000).
pub const SYNTAX_ERROR_OR_ACCESS_VIOLATION: &str = "42000";

/// Base table or view not found (42S02).
pub const BASE_TABLE_OR_VIEW_NOT_FOUND: &str = "42S02";

/// Column not found (42S22).
pub const COLUMN_NOT_FOUND: &str = "42S22";

/// Invalid descriptor index (07009).
pub const INVALID_DESCRIPTOR_INDEX: &str = "07009";

/// String data, right truncated (01004).
pub const STRING_DATA_RIGHT_TRUNCATED: &str = "01004";

/// Indicator variable required but not supplied (22002).
pub const INDICATOR_VARIABLE_REQUIRED: &str = "22002";

/// Invalid cursor state (24000).
pub const INVALID_CURSOR_STATE: &str = "24000";

/// Transaction is rolled back (25S03).
///
/// `SQLEndTran`'s diagnostics table describes this as a global-transaction
/// outcome, but its `Suspended State` section gives it a second, sharper role:
/// `25S03`, `40001`, `40002` and `HYC00` are the four SQLSTATEs that "confirm
/// that the transaction did not complete". A driver returning `SQL_ERROR` from
/// `SQLEndTran` with any *other* SQLSTATE puts the connection into a suspended
/// state, where only read-only functions are allowed until `SQLDisconnect`.
///
/// So a backend that could not commit, rolled back, and is left with a
/// perfectly usable connection reports this rather than `HY000`. The
/// difference is not cosmetic: `HY000` would suspend a healthy connection.
pub const TRANSACTION_ROLLED_BACK: &str = "25S03";

/// Invalid authorization specification (28000).
pub const INVALID_AUTH_SPEC: &str = "28000";

/// Invalid application buffer type (HY003).
pub const INVALID_APPLICATION_BUFFER_TYPE: &str = "HY003";

/// Associated statement is not prepared (HY007).
pub const ASSOCIATED_STATEMENT_NOT_PREPARED: &str = "HY007";

/// Operation canceled (HY008).
pub const OPERATION_CANCELED: &str = "HY008";

/// Invalid use of null pointer (HY009).
pub const INVALID_USE_OF_NULL_POINTER: &str = "HY009";

/// Function sequence error (HY010).
pub const FUNCTION_SEQUENCE_ERROR: &str = "HY010";

/// Attribute cannot be set now (HY011).
pub const ATTRIBUTE_CANNOT_BE_SET_NOW: &str = "HY011";

/// Limit on the number of handles exceeded (HY014).
pub const LIMIT_ON_HANDLES_EXCEEDED: &str = "HY014";

/// Cannot modify an implementation row descriptor (HY016).
pub const CANNOT_MODIFY_IRD: &str = "HY016";

/// Attempt to concatenate a null value (HY020).
pub const ATTEMPT_TO_CONCATENATE_A_NULL_VALUE: &str = "HY020";

/// Inconsistent descriptor information (HY021).
pub const INCONSISTENT_DESCRIPTOR_INFORMATION: &str = "HY021";

/// Invalid attribute value (HY024).
pub const INVALID_ATTRIBUTE_VALUE: &str = "HY024";

/// Invalid string or buffer length (HY090).
pub const INVALID_STRING_OR_BUFFER_LENGTH: &str = "HY090";

/// Invalid descriptor field identifier (HY091).
pub const INVALID_DESCRIPTOR_FIELD_IDENTIFIER: &str = "HY091";

/// Invalid attribute/option identifier (HY092).
pub const INVALID_ATTRIBUTE_OPTION_IDENTIFIER: &str = "HY092";

/// Invalid parameter type (HY105).
pub const INVALID_PARAMETER_TYPE: &str = "HY105";

/// Fetch type out of range (HY106).
pub const FETCH_TYPE_OUT_OF_RANGE: &str = "HY106";

/// Numeric value out of range (22003).
pub const NUMERIC_VALUE_OUT_OF_RANGE: &str = "22003";

/// Invalid character value for cast specification (22018).
pub const INVALID_CHARACTER_VALUE_FOR_CAST: &str = "22018";

/// COUNT field incorrect (07002).
pub const COUNT_FIELD_INCORRECT: &str = "07002";

/// String data, right truncation (22001).
pub const STRING_DATA_RIGHT_TRUNCATION: &str = "22001";

/// Datetime field overflow (22008).
pub const DATETIME_FIELD_OVERFLOW: &str = "22008";

/// Invalid datetime format (22007).
pub const INVALID_DATETIME_FORMAT: &str = "22007";

/// Interval field overflow (22015).
pub const INTERVAL_FIELD_OVERFLOW: &str = "22015";

/// Fractional truncation (01S07).
pub const FRACTIONAL_TRUNCATION: &str = "01S07";

/// Timeout expired (HYT00).
pub const TIMEOUT_EXPIRED: &str = "HYT00";

// ---------------------------------------------------------------------------
// SqlState type
// ---------------------------------------------------------------------------

/// A five-character ODBC diagnostic state code (e.g. `"HY000"`).
///
/// The five bytes are private, so `[u8; 5]` is not frozen as API and no driver
/// can build a `SqlState` that is not five ASCII characters. That keeps
/// [`SqlState::as_str`]'s `"?????"` fallback unreachable. Build one with
/// [`SqlState::new`] from a literal, with `TryFrom<&str>` when the value is not
/// known at compile time, or from one of the named factory methods.
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

    /// The checked constructor, for a SQLSTATE that is not a literal, such as
    /// one read from a data source's own error payload.
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

    /// General error (HY000).
    pub fn general_error() -> Self {
        Self::new(GENERAL_ERROR)
    }

    /// Optional feature not implemented (HYC00).
    pub fn optional_feature_not_implemented() -> Self {
        Self::new(OPTIONAL_FEATURE_NOT_IMPLEMENTED)
    }

    /// Option value changed (01S02).
    ///
    /// A warning: the driver did not support the requested value and
    /// substituted a similar one. The caller returns `SQL_SUCCESS_WITH_INFO`.
    pub fn option_value_changed() -> Self {
        Self::new(OPTION_VALUE_CHANGED)
    }

    /// Connection not open (08003).
    pub fn connection_not_open() -> Self {
        Self::new(CONNECTION_NOT_OPEN)
    }

    /// Transaction is rolled back (25S03).
    ///
    /// See [`TRANSACTION_ROLLED_BACK`] for why a backend that rolled back
    /// instead of committing must report this rather than `HY000`.
    pub fn transaction_rolled_back() -> Self {
        Self::new(TRANSACTION_ROLLED_BACK)
    }

    /// Invalid cursor state (24000).
    pub fn invalid_cursor_state() -> Self {
        Self::new(INVALID_CURSOR_STATE)
    }

    /// Invalid cursor name (34000).
    pub fn invalid_cursor_name() -> Self {
        Self::new(INVALID_CURSOR_NAME)
    }

    /// Duplicate cursor name (3C000).
    pub fn duplicate_cursor_name() -> Self {
        Self::new(DUPLICATE_CURSOR_NAME)
    }

    /// String data, right truncated (01004).
    pub fn string_data_right_truncated() -> Self {
        Self::new(STRING_DATA_RIGHT_TRUNCATED)
    }

    /// Invalid attribute value (HY024).
    pub fn invalid_attribute_value() -> Self {
        Self::new(INVALID_ATTRIBUTE_VALUE)
    }

    /// Function sequence error (HY010).
    pub fn function_sequence_error() -> Self {
        Self::new(FUNCTION_SEQUENCE_ERROR)
    }

    /// Limit on the number of handles exceeded (HY014).
    ///
    /// The handle registry has no slot left. A token packs a slot index into
    /// half a `usize`, so the ceiling is `2^32 - 1` on a 64-bit target and
    /// 65 535 on a 32-bit one. ODBC still has 32-bit targets, since Excel and
    /// Access are 32-bit on Windows.
    pub fn limit_on_handles_exceeded() -> Self {
        Self::new(LIMIT_ON_HANDLES_EXCEEDED)
    }

    /// Operation canceled (HY008).
    ///
    /// Returned by a statement-level call that a `SQLCancel` on another thread
    /// interrupted, which is the second of the two clauses the spec's `HY008`
    /// row states: the function "was called, and before it completed execution,
    /// `SQLCancel` … was called on the `StatementHandle` from a different
    /// thread in a multithread application".
    ///
    /// The row's first clause (asynchronous processing, then the function
    /// called again) cannot arise here, because core implements no asynchronous
    /// execution and never returns `SQL_STILL_EXECUTING`.
    pub fn operation_canceled() -> Self {
        Self::new(OPERATION_CANCELED)
    }

    /// Connection name in use (08002).
    pub fn connection_in_use() -> Self {
        Self::new(CONNECTION_IN_USE)
    }

    /// Invalid string or buffer length (HY090).
    pub fn invalid_string_or_buffer_length() -> Self {
        Self::new(INVALID_STRING_OR_BUFFER_LENGTH)
    }

    /// Invalid use of null pointer (HY009).
    pub fn invalid_use_of_null_pointer() -> Self {
        Self::new(INVALID_USE_OF_NULL_POINTER)
    }

    /// Invalid descriptor index (07009).
    pub fn invalid_descriptor_index() -> Self {
        Self::new(INVALID_DESCRIPTOR_INDEX)
    }

    /// Invalid application buffer type (HY003).
    pub fn invalid_application_buffer_type() -> Self {
        Self::new(INVALID_APPLICATION_BUFFER_TYPE)
    }

    /// Invalid attribute/option identifier (HY092).
    pub fn invalid_attribute_option_identifier() -> Self {
        Self::new(INVALID_ATTRIBUTE_OPTION_IDENTIFIER)
    }

    /// Associated statement is not prepared (HY007).
    pub fn associated_statement_not_prepared() -> Self {
        Self::new(ASSOCIATED_STATEMENT_NOT_PREPARED)
    }

    /// Cannot modify an implementation row descriptor (HY016).
    pub fn cannot_modify_ird() -> Self {
        Self::new(CANNOT_MODIFY_IRD)
    }

    /// Attempt to concatenate a null value (HY020).
    ///
    /// `SQLPutData`'s row, which carries no `(DM)` marker: "SQLPutData was
    /// called more than once since the call that returned SQL_NEED_DATA, and in
    /// one of those calls, the *StrLen_or_Ind* argument contained SQL_NULL_DATA
    /// or SQL_DEFAULT_PARAM." A NULL is the whole value of a parameter, so it
    /// can neither follow data nor be followed by it.
    pub fn attempt_to_concatenate_a_null_value() -> Self {
        Self::new(ATTEMPT_TO_CONCATENATE_A_NULL_VALUE)
    }

    /// Inconsistent descriptor information (HY021).
    pub fn inconsistent_descriptor_information() -> Self {
        Self::new(INCONSISTENT_DESCRIPTOR_INFORMATION)
    }

    /// Invalid descriptor field identifier (HY091).
    pub fn invalid_descriptor_field_identifier() -> Self {
        Self::new(INVALID_DESCRIPTOR_FIELD_IDENTIFIER)
    }

    /// Invalid parameter type (HY105).
    pub fn invalid_parameter_type() -> Self {
        Self::new(INVALID_PARAMETER_TYPE)
    }

    /// Fetch type out of range (HY106).
    pub fn fetch_type_out_of_range() -> Self {
        Self::new(FETCH_TYPE_OUT_OF_RANGE)
    }

    /// Indicator variable required but not supplied (22002).
    pub fn indicator_variable_required() -> Self {
        Self::new(INDICATOR_VARIABLE_REQUIRED)
    }

    /// Attribute cannot be set now (HY011).
    pub fn attribute_cannot_be_set_now() -> Self {
        Self::new(ATTRIBUTE_CANNOT_BE_SET_NOW)
    }

    /// Client unable to establish connection (08001).
    ///
    /// Use only from the connection functions. Once a connection exists, a
    /// failing link is [`SqlState::communication_link_failure`].
    pub fn client_unable_to_establish_connection() -> Self {
        Self::new(CLIENT_UNABLE_TO_ESTABLISH_CONNECTION)
    }

    /// Communication link failure (08S01).
    pub fn communication_link_failure() -> Self {
        Self::new(COMMUNICATION_LINK_FAILURE)
    }

    /// Invalid catalog name (3D000).
    ///
    /// For a backend's [`crate::backend::Backend::set_current_catalog`] to
    /// report that the data source has no such catalog, and for a driver
    /// mapping the same complaint out of `SQLExecDirect` or `SQLPrepare`. Core
    /// never constructs it: see [`INVALID_CATALOG_NAME`].
    pub fn invalid_catalog_name() -> Self {
        Self::new(INVALID_CATALOG_NAME)
    }

    /// Numeric value out of range (22003).
    pub fn numeric_value_out_of_range() -> Self {
        Self::new(NUMERIC_VALUE_OUT_OF_RANGE)
    }

    /// COUNT field incorrect (07002).
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

    /// String data, right truncation (22001).
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

    /// Datetime field overflow (22008).
    pub fn datetime_field_overflow() -> Self {
        Self::new(DATETIME_FIELD_OVERFLOW)
    }

    /// Invalid character value for cast specification (22018).
    ///
    /// The character data could not be converted to the C type the application
    /// requested, for example `SQL_C_SLONG` against a column holding `"abc"`.
    pub fn invalid_character_value_for_cast() -> Self {
        Self::new(INVALID_CHARACTER_VALUE_FOR_CAST)
    }

    /// Invalid datetime format (22007).
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

    /// Interval field overflow (22015).
    ///
    /// The *C to SQL: Numeric* table's interval row: an exact numeric value
    /// whose magnitude exceeds the target interval's leading precision, or
    /// which carries a fraction an interval field cannot hold. The row's test
    /// is "data truncated", and this is the state it names.
    pub fn interval_field_overflow() -> Self {
        Self::new(INTERVAL_FIELD_OVERFLOW)
    }

    /// Fractional truncation (01S07).
    pub fn fractional_truncation() -> Self {
        Self::new(FRACTIONAL_TRUNCATION)
    }

    /// Timeout expired (HYT00).
    pub fn timeout_expired() -> Self {
        Self::new(TIMEOUT_EXPIRED)
    }

    /// Invalid authorization specification (28000).
    pub fn invalid_auth_spec() -> Self {
        Self::new(INVALID_AUTH_SPEC)
    }

    /// Prepared statement not a cursor-specification (07005).
    pub fn prepared_statement_not_cursor_spec() -> Self {
        Self::new(PREPARED_STATEMENT_NOT_CURSOR_SPEC)
    }

    /// Restricted data type attribute violation (07006).
    ///
    /// The value could not be converted to the C or SQL type the application
    /// asked for.
    pub fn restricted_data_type_attribute_violation() -> Self {
        Self::new(RESTRICTED_DATA_TYPE_ATTRIBUTE_VIOLATION)
    }

    /// Integrity constraint violation (23000).
    ///
    /// A primary key, foreign key, unique, check or NOT NULL constraint was
    /// violated by the statement.
    pub fn integrity_constraint_violation() -> Self {
        Self::new(INTEGRITY_CONSTRAINT_VIOLATION)
    }

    /// Syntax error or access violation (42000).
    pub fn syntax_error_or_access_violation() -> Self {
        Self::new(SYNTAX_ERROR_OR_ACCESS_VIOLATION)
    }

    /// Base table or view not found (42S02).
    pub fn base_table_or_view_not_found() -> Self {
        Self::new(BASE_TABLE_OR_VIEW_NOT_FOUND)
    }

    /// Column not found (42S22).
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

    #[test]
    fn interval_field_overflow_is_22015() {
        assert_eq!(
            SqlState::interval_field_overflow().as_str(),
            INTERVAL_FIELD_OVERFLOW
        );
        assert_eq!(INTERVAL_FIELD_OVERFLOW, "22015");
    }
}
