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

    /// The shape every FFI entry point will use, exercised once here against a
    /// real `Backend` rather than a hand-set flag: the backend signals its own
    /// token mid-call and then fails, which is what core sees when another
    /// thread cancelled it. Pins `MockCancelAwareBackend`'s switches too, so a
    /// broken mock fails here rather than in twenty entry-point tests.
    #[test]
    fn a_backend_that_cancels_itself_mid_call_produces_hy008() {
        use crate::backend::Backend;
        use crate::test_utils::{MockCancelAwareBackend, MockConnection};

        MockCancelAwareBackend::fail_next_execution();
        MockCancelAwareBackend::cancel_before_returning();

        let token = MockCancelAwareBackend::cancel_token(&MockConnection);
        let result = MockCancelAwareBackend::exec_direct(&MockConnection, &token, "SELECT 1");
        assert!(result.is_err(), "the mock was told to fail");
        assert!(
            MockCancelAwareBackend::is_cancelled(&token),
            "the mock was told to cancel itself before returning"
        );

        let err = reclassify_cancelled::<MockCancelAwareBackend, _, _>(result, &token)
            .expect_err("the input was an error");
        assert_eq!(err.sqlstate().as_str(), "HY008");
    }

    /// The same mock without the cancel switch: a plain failure keeps its own
    /// state. Guards against the switches leaking between calls, which would
    /// make every entry-point test pass for the wrong reason.
    #[test]
    fn the_same_backend_failing_without_a_cancel_keeps_its_own_state() {
        use crate::backend::Backend;
        use crate::test_utils::{MockCancelAwareBackend, MockConnection};

        MockCancelAwareBackend::fail_next_execution();

        let token = MockCancelAwareBackend::cancel_token(&MockConnection);
        let result = MockCancelAwareBackend::exec_direct(&MockConnection, &token, "SELECT 1");
        assert!(result.is_err());
        assert!(!MockCancelAwareBackend::is_cancelled(&token));

        let err = reclassify_cancelled::<MockCancelAwareBackend, _, _>(result, &token)
            .expect_err("the input was an error");
        assert_ne!(err.sqlstate().as_str(), "HY008");
    }

    /// The cursor half of the same shape, for the entry points that consume a
    /// cursor rather than produce one. `fetch`'s failure is reclassified
    /// against the token the *producing* execution minted, which is what
    /// `handles::current_cancel_token` hands them.
    #[test]
    fn a_cancelled_fetch_produces_hy008() {
        use crate::backend::{Backend, StatementBackend};
        use crate::test_utils::{MockCancelAwareBackend, MockConnection};

        let token = MockCancelAwareBackend::cancel_token(&MockConnection);
        let mut stmt = MockCancelAwareBackend::exec_direct(&MockConnection, &token, "SELECT 1")
            .expect("the mock was not told to fail");

        MockCancelAwareBackend::fail_next_fetch();
        MockCancelAwareBackend::cancel(&token).expect("mock cancel succeeds");

        let err = reclassify_cancelled::<MockCancelAwareBackend, _, _>(stmt.fetch(), &token)
            .expect_err("the fetch was told to fail");
        assert_eq!(err.sqlstate().as_str(), "HY008");
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
