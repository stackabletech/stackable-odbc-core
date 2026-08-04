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
    /// The handle the application passed is not one this driver handed out, or
    /// was freed. Maps to `SQL_INVALID_HANDLE`, which posts no diagnostic
    /// record at all: see [`OdbcError::sqlstate`].
    #[snafu(display("Invalid handle"))]
    InvalidHandle,

    /// An optional ODBC feature this driver does not provide (`HYC00`).
    ///
    /// Also what a defaulted `Backend` method returns, which is how core tells
    /// "the backend did not implement this" from "the backend failed": several
    /// FFI paths match on this variant to fall back to a benign default rather
    /// than propagate an error.
    #[snafu(display("{feature} is not implemented"))]
    NotImplemented {
        /// The feature named in the diagnostic message.
        feature: String,
    },

    /// An operation that needs a live connection was attempted on a handle that
    /// has none (`08003`).
    #[snafu(display("Connection not established"))]
    NotConnected,

    /// A cursor operation was attempted with no result set open (`24000`).
    #[snafu(display("No result set available"))]
    NoResultSet,

    /// A string did not fit the application's buffer and was truncated
    /// (`01004`). Reported as `SQL_SUCCESS_WITH_INFO`, not an error, so the
    /// application can retry with a larger buffer.
    #[snafu(display("String data truncated"))]
    StringTruncated,

    /// A numeric or timestamp value lost fractional precision on conversion
    /// (`01S07`). Like [`OdbcError::StringTruncated`], this is
    /// `SQL_SUCCESS_WITH_INFO` rather than a failure.
    #[snafu(display("Fractional truncation"))]
    FractionalTruncation,

    /// A failure carrying an explicit SQLSTATE, optionally the data source's own
    /// error code and the error that caused it.
    ///
    /// `#[non_exhaustive]` on the *variant* as well as the enum: the enum-level
    /// attribute stops a driver matching it exhaustively, but does nothing to
    /// stop a driver constructing it with struct-literal syntax, which would
    /// make every future field a breaking change. Construct it through
    /// [`OdbcError::general`] and the [`OdbcError::with_native_error`] /
    /// [`OdbcError::with_source`] builders instead, which stay source-compatible
    /// as fields are added.
    #[snafu(display("{message}"))]
    #[non_exhaustive]
    General {
        /// The diagnostic message the application reads through
        /// `SQLGetDiagRec`.
        message: String,
        /// The five-character SQLSTATE this failure reports.
        sqlstate: SqlState,
        /// The data source's own error code, reported verbatim as
        /// `SQLGetDiagRec`'s `NativeErrorPtr`. `0` means "none", which is what
        /// the ODBC spec defines for a diagnostic with no data-source code.
        native_error: i32,
        /// The error this one wrapped, preserved so a driver's causal chain
        /// survives the FFI boundary instead of being flattened to a string.
        ///
        /// Named `cause` rather than `source`, because `snafu` special-cases a
        /// field called `source` and requires it to implement
        /// [`std::error::Error`], which `Box<dyn Error + Send + Sync>` does not,
        /// there being no such impl in `std`. Reach it through
        /// [`OdbcError::cause`].
        cause: Option<Box<dyn std::error::Error + Send + Sync>>,
    },

    /// A panic that core's panic guard caught before it could unwind
    /// across the C ABI, which would be undefined behaviour. Reported as
    /// `HY000`.
    #[snafu(display("Panic in driver: {message}"))]
    Panic {
        /// The panic payload, as far as it could be recovered.
        message: String,
    },

    /// A connection-string keyword the backend requires was absent.
    #[snafu(display("Missing required parameter: {name}"))]
    MissingParameter {
        /// The keyword that was missing.
        name: String,
    },
}

impl OdbcError {
    /// Convenience constructor for `General` variant with a message and SQLSTATE.
    ///
    /// The data-source error code defaults to `0` ("none") and the causal chain
    /// to empty; add them with [`OdbcError::with_native_error`] and
    /// [`OdbcError::with_source`].
    pub fn general(message: impl Into<String>, sqlstate: SqlState) -> Self {
        OdbcError::General {
            message: message.into(),
            sqlstate,
            native_error: 0,
            cause: None,
        }
    }

    /// Attaches the data source's own error code, which `SQLGetDiagRec` reports
    /// verbatim as `NativeErrorPtr`.
    ///
    /// A no-op on any variant other than `General`, which is the only one with a
    /// data source behind it: `InvalidHandle` and `Panic` are driver-side
    /// conditions with no native code to carry.
    #[must_use]
    pub fn with_native_error(mut self, code: i32) -> Self {
        if let OdbcError::General { native_error, .. } = &mut self {
            *native_error = code;
        }
        self
    }

    /// Attaches the underlying error, so a driver's causal chain survives the
    /// FFI boundary as an object rather than being flattened to a string.
    /// Read it back with [`OdbcError::cause`].
    ///
    /// A no-op on any variant other than `General`, none of which has a field to
    /// hold a cause.
    #[must_use]
    pub fn with_source(
        mut self,
        source: impl Into<Box<dyn std::error::Error + Send + Sync>>,
    ) -> Self {
        if let OdbcError::General { cause, .. } = &mut self {
            *cause = Some(source.into());
        }
        self
    }

    /// The error that caused this one, if it was recorded with
    /// [`OdbcError::with_source`].
    ///
    /// Deliberately not exposed through [`std::error::Error::source`]: see the
    /// `cause` field's documentation for why the field cannot be a `snafu`
    /// source.
    pub fn cause(&self) -> Option<&(dyn std::error::Error + Send + Sync + 'static)> {
        match self {
            OdbcError::General { cause, .. } => cause.as_deref(),
            _ => None,
        }
    }

    /// The data source's own error code, or `0` when there is none.
    ///
    /// This is what `SQLGetDiagRec` hands back through `NativeErrorPtr`. Every
    /// variant except `General` reports `0`, because none of them originates at
    /// the data source.
    pub fn native_error(&self) -> i32 {
        match self {
            OdbcError::General { native_error, .. } => *native_error,
            _ => 0,
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
            OdbcError::General { sqlstate, .. } => *sqlstate,
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

/// Moves a backend result into core's error space.
///
/// Core's generic FFI functions return `Result<_, OdbcError>` while every
/// [`Backend`](crate::backend::Backend) and
/// [`StatementBackend`](crate::backend::StatementBackend) method returns the
/// driver's own `Self::Error`, so each call across that boundary converts.
///
/// Plain `?` cannot do it. `?` needs `OdbcError: From<B::Error>`, and the
/// `B::Error: Into<OdbcError>` bound does not give the compiler that, because
/// the blanket `impl<T, U: From<T>> Into<U> for T` only runs in the other
/// direction. Stating the bound as `where OdbcError: From<Self::Error>` on the
/// trait does not help either: a `where` clause mentioning an associated type
/// is not elaborated into an implied bound, so every one of core's generic
/// functions would have to repeat it. This extension is the cheap way to keep
/// the call sites readable.
pub(crate) trait IntoOdbc<T> {
    /// Converts the error half into [`OdbcError`], leaving the value untouched.
    fn into_odbc(self) -> Result<T, OdbcError>;
}

impl<T, E: Into<OdbcError>> IntoOdbc<T> for Result<T, E> {
    fn into_odbc(self) -> Result<T, OdbcError> {
        self.map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::sql_state;

    /// A two-link chain, so the walk in `DiagnosticQueue::push` and
    /// `OdbcError::cause` have something with a real `source()` to follow.
    #[derive(Debug, Snafu)]
    #[snafu(display("outer"))]
    struct Outer {
        source: Inner,
    }

    #[derive(Debug, Snafu)]
    #[snafu(display("inner"))]
    struct Inner;

    #[test]
    fn general_defaults_to_no_native_code_and_no_cause() {
        let err = OdbcError::general("boom", SqlState::general_error());
        assert_eq!(
            err.native_error(),
            0,
            "a General built without a data-source code must report 0, which is \
             the spec's 'no native error'"
        );
        assert!(
            err.cause().is_none(),
            "a General built without a cause must not invent one"
        );
    }

    #[test]
    fn native_error_is_carried_to_the_accessor() {
        let err = OdbcError::general("boom", SqlState::general_error()).with_native_error(1555);
        assert_eq!(err.native_error(), 1555);
    }

    #[test]
    fn native_error_survives_a_later_with_source() {
        // The builders are order-independent; neither may clobber the other.
        let err = OdbcError::general("boom", SqlState::general_error())
            .with_native_error(19)
            .with_source(Inner);
        assert_eq!(err.native_error(), 19);
        assert!(err.cause().is_some());
    }

    #[test]
    fn native_error_is_zero_for_variants_with_no_data_source_behind_them() {
        // These originate in the driver, not the data source, so there is no
        // native code to report and the builder must not pretend otherwise.
        assert_eq!(
            OdbcError::InvalidHandle.with_native_error(7).native_error(),
            0
        );
        assert_eq!(
            OdbcError::NotConnected.with_native_error(7).native_error(),
            0
        );
        assert!(
            OdbcError::NotConnected.with_source(Inner).cause().is_none(),
            "a variant with no cause field must stay causeless"
        );
    }

    #[test]
    fn with_source_preserves_the_chain_rather_than_stringifying_it() {
        let err = OdbcError::general("wrapped", SqlState::general_error())
            .with_source(Outer { source: Inner });

        let cause = err.cause().expect("cause must be recorded");
        assert_eq!(cause.to_string(), "outer");
        assert_eq!(
            cause.source().expect("the chain must continue").to_string(),
            "inner",
            "the whole chain must survive, not just the outermost link"
        );
    }

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
