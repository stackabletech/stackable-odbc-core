//! Turning a cancelled backend failure into the spec's `HY008`.
//!
//! `SQLCancel` signals the backend's token; the in-flight call then fails with
//! whatever its client library reported, which carries no hint that a
//! cancellation caused it. This module is the one place that asks the token and
//! relabels such a failure, so the answer cannot drift between the ~23 backend
//! call sites in `ffi/`.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::backend::Backend;
use crate::errors::OdbcError;
use crate::types::SqlState;

/// A backend's cancel token, plus core's record of *what* signalled it.
///
/// `Backend::CancelToken` answers "was this cancelled"; it cannot answer "by
/// whom", because a backend has one `cancel` method and both callers use it —
/// `SQLCancel` on another thread, and the query timer in
/// [`crate::query_timer`]. The two are different events with different
/// SQLSTATEs (`HY008` against `HYT00`), so core records the second one here.
///
/// # Why the flag lives inside the token's own allocation
///
/// This is what `mint_cancel_token` stores in the registry, so the flag has
/// exactly the token's lifetime and no other:
///
/// - **It is minted per execution**, with the token, so a deadline that expired
///   on one execution cannot relabel a failure on the next. That is the same
///   property `mint_cancel_token` exists to give the token itself, inherited
///   rather than restated.
/// - **It survives the statement**, with the token, because both are one
///   allocation behind the `Arc` that `Registry::cancel_of` clones out. The
///   timer thread already holds that clone, so it needs no registry access, no
///   lock, and cannot touch a statement that another thread has freed.
/// - **It needs no lock**, being one `AtomicBool`. Nothing here goes through
///   `crate::sync`, which is the crate's import path for *locks*; the timer
///   thread holds none, exactly as `SQLCancel`'s cross-thread branch holds
///   none.
///
/// A flag on the statement handle would have failed the second point (the
/// timer thread must never reach handle state), and a second field on
/// `Registry::Slot` would have failed the first two only by keeping two things
/// in step that this keeps in step by construction.
pub(crate) struct CancelState<T> {
    token: T,
    /// Set by [`crate::query_timer::QueryTimer`]'s thread when a core-enforced
    /// `SQL_ATTR_QUERY_TIMEOUT` expired and it cancelled `token`.
    timed_out: AtomicBool,
}

impl<T> CancelState<T> {
    pub(crate) fn new(token: T) -> Self {
        Self {
            token,
            timed_out: AtomicBool::new(false),
        }
    }

    pub(crate) fn token(&self) -> &T {
        &self.token
    }

    /// Record that a core-side deadline is why this token was signalled.
    ///
    /// Called before `Backend::cancel`, so any thread that can observe the
    /// cancellation can already observe its cause.
    pub(crate) fn mark_timed_out(&self) {
        self.timed_out.store(true, Ordering::SeqCst);
    }

    pub(crate) fn timed_out(&self) -> bool {
        self.timed_out.load(Ordering::SeqCst)
    }
}

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
///
/// **This function does not consult [`CancelState::timed_out`], and that is
/// deliberate.** Relabelling a timer-signalled cancellation `HYT00` happens in
/// [`crate::query_timer::QueryTimer::reclassify`], which only the entry points
/// that hold a `QueryTimer` reach. The four that reach *this* function with no
/// timer are `SQLGetData`, `SQLDescribeParam`, `SQLDescribeCol` and
/// `SQLColAttribute`, and **none of the four has an `HYT00` row** — each has
/// `HYT01`, the connection timeout, which is a different state. So a
/// `SQLGetData` failing on a timed-out cursor keeps `HY008`, which its table
/// does list; moving the check here would hand all four a SQLSTATE their spec
/// pages do not allow. That is also why `SQLGetData` is deliberately unarmed.
///
/// The rule is *not* "a timer-holding entry point always has an `HYT00` row",
/// and `SQLParamData` is the exception to check before restating it that way.
/// It holds a timer and relabels (`ffi::params::sql_param_data`), yet its own
/// diagnostics table has no `HYT00` row either. It is entitled to one anyway,
/// by the sentence its page carries after the table: "If **SQLParamData** is
/// called while sending data for a parameter in a SQL statement, it can return
/// any SQLSTATE that can be returned by the function called to execute the
/// statement (**SQLExecute** or **SQLExecDirect**)" — and both of those do list
/// `HYT00`. That function's own doc comment documents the whole inherited set
/// on the same grounds.
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

/// [`reclassify_cancelled`] for a caller that may not have a token.
///
/// The cursor-consuming entry points — `SQLFetch`, `SQLGetData` and their
/// neighbours — read their token from the registry rather than minting one, and
/// `None` there means no backend call has run on this statement yet. Nothing
/// could have been cancelled in that case, so the error passes through with its
/// own SQLSTATE.
pub(crate) fn reclassify_cancelled_opt<B: Backend, T, E: Into<OdbcError>>(
    result: Result<T, E>,
    cancel: Option<&B::CancelToken>,
) -> Result<T, OdbcError> {
    match cancel {
        Some(cancel) => reclassify_cancelled::<B, _, _>(result, cancel),
        None => result.map_err(Into::into),
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
