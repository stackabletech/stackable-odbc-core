//! Turning a cancelled backend failure into the spec's `HY008`.
//!
//! `SQLCancel` signals the backend's token; the in-flight call then fails with
//! whatever its client library reported, which carries no hint that a
//! cancellation caused it. This module is the one place that asks the token and
//! relabels such a failure, so the answer cannot drift between the ~23 backend
//! call sites in `ffi/`.

use crate::backend::Backend;
use crate::errors::OdbcError;
use crate::types::SqlState;

/// Relabel a failed backend call as `HY008` when its cancel token was signalled.
///
/// Spec, `SQLCancel`: "If the original function is canceled, it returns
/// SQL_ERROR and SQLSTATE HY008 (Operation canceled)."
///
/// **Only the error half is examined.** The spec allows a cancelled execution
/// to complete anyway — "it is possible for the execution to succeed and return
/// SQL_SUCCESS while the cancel is also successful" — so `Ok` is returned
/// untouched no matter what the token says. This relabels an error core already
/// has; it never manufactures one.
///
/// The backend's own error is dropped rather than chained, because it describes
/// the *symptom* of a cancellation (a closed socket, an aborted query) and not
/// the cause. Its SQLSTATE is what the application would otherwise see, and it
/// is exactly what the spec says must not be reported here.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "called by the ffi/ backend call sites once they are wired up"
    )
)]
pub(crate) fn reclassify_cancelled<B: Backend, T, E: Into<OdbcError>>(
    result: Result<T, E>,
    cancel: &B::CancelToken,
) -> Result<T, OdbcError> {
    match result {
        Ok(value) => Ok(value),
        Err(e) => {
            if B::is_cancelled(cancel) {
                tracing::debug!("backend call failed with its token signalled; reporting HY008");
                Err(OdbcError::general(
                    "Operation canceled",
                    SqlState::operation_canceled(),
                ))
            } else {
                Err(e.into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{MockBackend, MockCancelToken, MockError};
    use std::sync::atomic::Ordering;

    #[test]
    fn an_error_becomes_hy008_when_the_token_is_signalled() {
        let token = MockCancelToken::default();
        token.cancelled.store(true, Ordering::SeqCst);
        let err = reclassify_cancelled::<MockBackend, (), MockError>(Err(MockError), &token)
            .expect_err("the input was an error");
        assert_eq!(err.sqlstate().as_str(), "HY008");
    }

    #[test]
    fn an_error_keeps_its_own_state_when_the_token_is_not_signalled() {
        let token = MockCancelToken::default();
        let err = reclassify_cancelled::<MockBackend, (), MockError>(Err(MockError), &token)
            .expect_err("the input was an error");
        assert_ne!(
            err.sqlstate().as_str(),
            "HY008",
            "an uncancelled failure must keep the backend's own SQLSTATE"
        );
    }

    #[test]
    fn success_stays_successful_even_when_the_token_is_signalled() {
        // Spec, SQLCancel: "it is possible for the execution to succeed and
        // return SQL_SUCCESS while the cancel is also successful." Turning a
        // successful call into HY008 would contradict that outright.
        let token = MockCancelToken::default();
        token.cancelled.store(true, Ordering::SeqCst);
        let ok = reclassify_cancelled::<MockBackend, u8, MockError>(Ok(7), &token)
            .expect("a successful call must stay successful");
        assert_eq!(ok, 7);
    }
}
