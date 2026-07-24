//! [`OdbcError`] and its mapping to SQLSTATEs and [`crate::types::SqlReturn`].

use snafu::Snafu;

use crate::types::{SqlReturn, SqlState};

/// ODBC error types that map to SQLSTATEs and [`SqlReturn`] codes.
///
/// Each variant corresponds to a distinct failure mode. Use [`OdbcError::sqlstate`]
/// and [`OdbcError::sql_return`] to convert to the values the ODBC spec requires.
///
/// `#[non_exhaustive]`: this error type is part of the published `stackable-odbc-core` API
/// that out-of-tree driver crates depend on. Marking it non-exhaustive lets us
/// add a new failure mode in a minor release without breaking a driver that
/// matches on it (the driver keeps its `_` arm). Driver crates can still
/// construct any existing variant directly.
#[derive(Debug, Snafu)]
#[non_exhaustive]
pub enum OdbcError {
    #[snafu(display("Invalid handle"))]
    InvalidHandle,

    #[snafu(display("{feature} is not implemented"))]
    NotImplemented { feature: String },

    #[snafu(display("Connection not established"))]
    NotConnected,

    #[snafu(display("No result set available"))]
    NoResultSet,

    #[snafu(display("String data truncated"))]
    StringTruncated,

    #[snafu(display("Fractional truncation"))]
    FractionalTruncation,

    #[snafu(display("{message}"))]
    General { message: String, sqlstate: SqlState },

    #[snafu(display("Panic in driver: {message}"))]
    Panic { message: String },

    #[snafu(display("Missing required parameter: {name}"))]
    MissingParameter { name: String },
}

impl OdbcError {
    /// Convenience constructor for `General` variant with a message and SQLSTATE.
    pub fn general(message: impl Into<String>, sqlstate: SqlState) -> Self {
        OdbcError::General {
            message: message.into(),
            sqlstate,
        }
    }

    /// Returns the SQLSTATE code corresponding to this error variant.
    ///
    /// `InvalidHandle` has no SQLSTATE of its own: `SQL_INVALID_HANDLE` posts no
    /// diagnostic record (see `panic_safe`), so the `HY000` below is a
    /// never-surfaced placeholder rather than a code the driver reports.
    pub fn sqlstate(&self) -> SqlState {
        match self {
            OdbcError::InvalidHandle => SqlState::general_error(),
            OdbcError::NotImplemented { .. } => SqlState::optional_feature_not_implemented(),
            OdbcError::NotConnected => SqlState::connection_not_open(),
            OdbcError::NoResultSet => SqlState::invalid_cursor_state(),
            OdbcError::StringTruncated => SqlState::string_data_right_truncated(),
            OdbcError::FractionalTruncation => SqlState::fractional_truncation(),
            OdbcError::General { sqlstate, .. } => sqlstate.clone(),
            OdbcError::Panic { .. } => SqlState::general_error(),
            OdbcError::MissingParameter { .. } => SqlState::general_error(),
        }
    }

    /// Returns the ODBC `SqlReturn` code corresponding to this error variant.
    pub fn sql_return(&self) -> SqlReturn {
        match self {
            OdbcError::InvalidHandle => SqlReturn::INVALID_HANDLE,
            OdbcError::StringTruncated | OdbcError::FractionalTruncation => {
                SqlReturn::SUCCESS_WITH_INFO
            }
            _ => SqlReturn::ERROR,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::sql_state;

    #[test]
    fn sqlstate_mapping() {
        assert_eq!(
            OdbcError::InvalidHandle.sqlstate().as_str(),
            sql_state::GENERAL_ERROR
        );
        assert_eq!(
            OdbcError::NotImplemented {
                feature: "x".into()
            }
            .sqlstate()
            .as_str(),
            sql_state::OPTIONAL_FEATURE_NOT_IMPLEMENTED
        );
        assert_eq!(
            OdbcError::NotConnected.sqlstate().as_str(),
            sql_state::CONNECTION_NOT_OPEN
        );
        assert_eq!(
            OdbcError::NoResultSet.sqlstate().as_str(),
            sql_state::INVALID_CURSOR_STATE
        );
        assert_eq!(
            OdbcError::StringTruncated.sqlstate().as_str(),
            sql_state::STRING_DATA_RIGHT_TRUNCATED
        );
        assert_eq!(
            OdbcError::Panic {
                message: "boom".into()
            }
            .sqlstate()
            .as_str(),
            sql_state::GENERAL_ERROR
        );
        assert_eq!(
            OdbcError::MissingParameter {
                name: "host".into()
            }
            .sqlstate()
            .as_str(),
            sql_state::GENERAL_ERROR
        );
    }

    #[test]
    fn sql_return_mapping() {
        assert_eq!(
            OdbcError::InvalidHandle.sql_return(),
            SqlReturn::INVALID_HANDLE
        );
        assert_eq!(
            OdbcError::StringTruncated.sql_return(),
            SqlReturn::SUCCESS_WITH_INFO
        );
        assert_eq!(OdbcError::NotConnected.sql_return(), SqlReturn::ERROR);
    }
}
