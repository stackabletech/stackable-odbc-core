//! Core-side enforcement of `SQL_ATTR_QUERY_TIMEOUT`.
//!
//! A backend that answers [`QueryTimeout::CoreCancels`] can be cancelled but
//! cannot set a server-side deadline, so core supplies the deadline: it arms a
//! timer before the backend call and cancels the statement if the call has not
//! returned by the time the timer expires.
//!
//! This is the only mechanism available. Every statement-producing `Backend`
//! method is synchronous and blocks the calling thread, so core cannot abandon
//! one — it can only ask the backend to stop the work, which is exactly what
//! [`Backend::cancel`] does. The timer therefore runs on its own thread; the
//! calling thread is inside the backend and cannot check a clock.
//!
//! [`QueryTimeout::CoreCancels`]: crate::types::QueryTimeout::CoreCancels
//! [`Backend::cancel`]: crate::backend::Backend::cancel

use std::any::Any;
use std::time::Duration;

use crate::backend::Backend;
use crate::errors::OdbcError;
use crate::sync::{Arc, Condvar, Mutex};
use crate::types::SqlState;

/// The state a [`QueryTimer`] shares with its timer thread.
///
/// `done` is the guard's "the call returned, stand down" signal and the
/// thread's "I fired" record, under one mutex so the two cannot race into both
/// happening. The `Condvar` is what makes standing down immediate: a timer
/// armed for thirty minutes on a query that finishes in a millisecond must not
/// keep a thread alive for the remaining thirty minutes, which is what a plain
/// `thread::sleep` would do.
struct Shared {
    state: Mutex<State>,
    signal: Condvar,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum State {
    /// The backend call is still running and the deadline has not passed.
    Running,
    /// The backend call returned in time; the timer thread should exit without
    /// cancelling.
    Finished,
    /// The deadline passed first and the timer signalled `Backend::cancel`.
    FiredCancel,
}

/// An armed query-timeout deadline, disarmed by dropping it.
///
/// Held across exactly one backend call. `Drop` stands the timer down, so an
/// early return — including one caused by a panic that `panic_safe` catches —
/// cannot leave a timer running against a statement that is no longer
/// executing.
pub(crate) struct QueryTimer {
    /// `None` when no deadline applies, which is the overwhelmingly common
    /// case: no timeout set, or a backend that enforces its own. Nothing is
    /// spawned and every method is a no-op, so an untimed statement pays only
    /// a null check.
    shared: Option<Arc<Shared>>,
}

impl QueryTimer {
    /// A timer that never fires, for a call with no core-enforced deadline.
    pub(crate) fn disarmed() -> Self {
        Self { shared: None }
    }

    /// Arm a deadline of `seconds` that cancels `token` when it expires.
    ///
    /// `token` is the type-erased cancel token from the registry rather than a
    /// `&B::CancelToken`, because it has to outlive this call on another
    /// thread. That is sound for the same reason `SQLCancel`'s cross-thread
    /// branch is: `Backend::CancelToken` is `Send + Sync + 'static`, and the
    /// `Arc` keeps it alive even if the statement is freed while the timer is
    /// still armed.
    pub(crate) fn arm<B: Backend>(
        seconds: Option<usize>,
        token: &Arc<dyn Any + Send + Sync>,
    ) -> Self {
        let Some(seconds) = seconds.filter(|s| *s > 0) else {
            return Self::disarmed();
        };
        let deadline = Duration::from_secs(seconds as u64);

        let shared = Arc::new(Shared {
            state: Mutex::new(State::Running),
            signal: Condvar::new(),
        });
        let thread_shared = Arc::clone(&shared);
        let thread_token = Arc::clone(token);

        // A detached thread, deliberately: `Drop` below waits for the state to
        // settle rather than joining, so a `Backend::cancel` that blocks on a
        // network round-trip cannot hold up the entry point that is returning.
        let spawned = std::thread::Builder::new()
            .name("odbc-query-timeout".into())
            .spawn(move || {
                let Ok(guard) = thread_shared.state.lock() else {
                    // The guard's thread panicked while holding the lock. The
                    // call it was timing is over, so there is nothing to cancel.
                    return;
                };
                let Ok((mut state, wait)) =
                    thread_shared
                        .signal
                        .wait_timeout_while(guard, deadline, |s| *s == State::Running)
                else {
                    return;
                };
                if !wait.timed_out() {
                    // Disarmed: the backend call returned first.
                    return;
                }
                *state = State::FiredCancel;
                // Released before calling into the backend. `Backend::cancel`
                // may block on the data source, and holding this would make the
                // returning entry point wait for it in `Drop`.
                drop(state);

                match thread_token.downcast_ref::<B::CancelToken>() {
                    Some(cancel) => {
                        tracing::warn!(
                            "SQL_ATTR_QUERY_TIMEOUT of {}s expired; cancelling the statement",
                            seconds
                        );
                        if let Err(e) = B::cancel(cancel) {
                            // Nothing to report to: the application is blocked
                            // inside the backend call this was meant to stop,
                            // and the statement's diagnostic queue belongs to
                            // that thread.
                            tracing::error!("query-timeout cancel failed: {}", e.into());
                        }
                    }
                    // Unreachable for the same reason `handles::cancel_as`'s
                    // error arm is: every stored token was built by
                    // `mint_cancel_token::<B>` for this same `B`.
                    None => tracing::error!(
                        "query-timeout cancel token is not this backend's CancelToken type"
                    ),
                }
                thread_shared.signal.notify_all();
            });

        match spawned {
            Ok(_handle) => Self {
                shared: Some(shared),
            },
            Err(e) => {
                // Out of threads. The call still runs, just without a deadline
                // — strictly better than refusing to execute at all, and the
                // application already holds a `SQL_SUCCESS` for the attribute.
                tracing::error!("could not spawn the query-timeout thread: {e}; running untimed");
                Self::disarmed()
            }
        }
    }

    /// Whether this timer fired, i.e. the deadline passed before the backend
    /// call returned.
    fn fired(&self) -> bool {
        self.shared.as_ref().is_some_and(|shared| {
            shared
                .state
                .lock()
                .is_ok_and(|state| *state == State::FiredCancel)
        })
    }

    /// Relabel a failed backend call as `HYT00` when this timer cancelled it.
    ///
    /// Sits *outside* [`crate::cancel::reclassify_cancelled`] and runs first,
    /// because the two describe different events through the same mechanism.
    /// Both end in a cancelled statement, so both would otherwise report
    /// `HY008` "operation canceled" — but an application that set a timeout is
    /// waiting to distinguish "my deadline passed" from "another thread called
    /// `SQLCancel`", and only `HYT00` says the first.
    ///
    /// As with `reclassify_cancelled`, only the error half is examined: the
    /// spec allows a cancelled execution to succeed anyway, and a query that
    /// beat its deadline to the finish line has not timed out.
    /// [`crate::cancel::reclassify_cancelled`] followed by [`Self::reclassify`]
    /// — the form every statement-producing call site uses.
    ///
    /// The order matters and is the reason this exists rather than two nested
    /// calls at each site. A timed-out call has a signalled token, so the
    /// cancel pass would label it `HY008`; running the timeout pass second lets
    /// the more specific `HYT00` win. Reversing them would report every expired
    /// deadline as a plain cancellation.
    pub(crate) fn check<B: Backend, T, E: Into<OdbcError>>(
        &self,
        result: Result<T, E>,
        cancel: &B::CancelToken,
    ) -> Result<T, OdbcError> {
        self.reclassify(crate::cancel::reclassify_cancelled::<B, _, _>(
            result, cancel,
        ))
    }

    pub(crate) fn reclassify<T>(&self, result: Result<T, OdbcError>) -> Result<T, OdbcError> {
        match result {
            Ok(value) => Ok(value),
            Err(e) if self.fired() => {
                tracing::debug!(
                    "backend call failed after its query timeout fired; reporting HYT00"
                );
                let _ = e;
                Err(OdbcError::general(
                    "Timeout expired",
                    SqlState::timeout_expired(),
                ))
            }
            Err(e) => Err(e),
        }
    }
}

impl Drop for QueryTimer {
    fn drop(&mut self) {
        let Some(shared) = self.shared.as_ref() else {
            return;
        };
        let Ok(mut state) = shared.state.lock() else {
            return;
        };
        if *state == State::Running {
            *state = State::Finished;
        }
        // Wakes the timer thread so it exits now rather than at its deadline.
        // Nothing is joined: a fired timer may be inside `Backend::cancel`, and
        // waiting for that would make every timed-out call block on the cancel
        // round-trip before returning to the application.
        shared.signal.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{MockCancelAwareBackend, MockConnection};
    use std::sync::atomic::Ordering;

    /// The whole point, end to end: a deadline that passes signals the token.
    ///
    /// Not run under Miri — it is a wall-clock test with a real sleep, so
    /// Miri's slowdown would stretch it unpredictably, and there is no `unsafe`
    /// here for Miri to check.
    #[test]
    #[cfg_attr(miri, ignore = "wall-clock timing; no unsafe to check")]
    fn an_expired_deadline_cancels_the_token() {
        let token: Arc<dyn Any + Send + Sync> =
            Arc::new(MockCancelAwareBackend::cancel_token(&MockConnection));
        let timer = QueryTimer::arm::<MockCancelAwareBackend>(Some(1), &token);

        // Longer than the deadline: stands in for a backend call that overruns.
        std::thread::sleep(Duration::from_millis(1500));

        let cancel = token
            .downcast_ref::<crate::test_utils::MockCancelToken>()
            .expect("the token this test built");
        assert!(
            cancel.cancelled.load(Ordering::SeqCst),
            "the deadline passed but Backend::cancel was never called"
        );
        assert!(timer.fired(), "the timer must record that it fired");
    }

    /// The disarm path: a call that returns before its deadline must not be
    /// cancelled. This is the case that would break every fast query if `Drop`
    /// failed to stand the timer down.
    #[test]
    #[cfg_attr(miri, ignore = "wall-clock timing; no unsafe to check")]
    fn a_call_that_returns_in_time_is_not_cancelled() {
        let token: Arc<dyn Any + Send + Sync> =
            Arc::new(MockCancelAwareBackend::cancel_token(&MockConnection));
        let timer = QueryTimer::arm::<MockCancelAwareBackend>(Some(60), &token);
        assert!(!timer.fired());
        drop(timer);

        // Well past any plausible scheduling delay, but nowhere near the 60s
        // deadline: if the timer were still armed it would not have fired yet
        // either, so the assertion below is about `cancelled` staying false for
        // the right reason.
        std::thread::sleep(Duration::from_millis(200));

        let cancel = token
            .downcast_ref::<crate::test_utils::MockCancelToken>()
            .expect("the token this test built");
        assert!(
            !cancel.cancelled.load(Ordering::SeqCst),
            "a disarmed timer must never cancel"
        );
    }

    #[test]
    fn no_deadline_arms_nothing() {
        let token: Arc<dyn Any + Send + Sync> =
            Arc::new(MockCancelAwareBackend::cancel_token(&MockConnection));
        for seconds in [None, Some(0)] {
            let timer = QueryTimer::arm::<MockCancelAwareBackend>(seconds, &token);
            assert!(
                timer.shared.is_none(),
                "{seconds:?} must not spawn a timer thread"
            );
            assert!(!timer.fired());
        }
    }

    #[test]
    fn reclassify_leaves_an_untimed_error_alone() {
        let timer = QueryTimer::disarmed();
        let err = timer
            .reclassify::<()>(Err(OdbcError::general(
                "backend said no",
                SqlState::general_error(),
            )))
            .expect_err("the input was an error");
        assert_eq!(err.sqlstate().as_str(), "HY000");
    }

    #[test]
    #[cfg_attr(miri, ignore = "wall-clock timing; no unsafe to check")]
    fn reclassify_turns_a_fired_timers_error_into_hyt00() {
        let token: Arc<dyn Any + Send + Sync> =
            Arc::new(MockCancelAwareBackend::cancel_token(&MockConnection));
        let timer = QueryTimer::arm::<MockCancelAwareBackend>(Some(1), &token);
        std::thread::sleep(Duration::from_millis(1500));

        let err = timer
            .reclassify::<()>(Err(OdbcError::general(
                "socket closed",
                SqlState::communication_link_failure(),
            )))
            .expect_err("the input was an error");
        assert_eq!(
            err.sqlstate().as_str(),
            "HYT00",
            "a cancellation caused by the deadline is a timeout, not a link failure"
        );
    }

    /// The spec's rule that a cancelled call may still succeed, applied to the
    /// timeout path: "it is possible for the execution to succeed and return
    /// SQL_SUCCESS while the cancel is also successful."
    #[test]
    #[cfg_attr(miri, ignore = "wall-clock timing; no unsafe to check")]
    fn a_query_that_beats_its_deadline_to_the_finish_line_still_succeeds() {
        let token: Arc<dyn Any + Send + Sync> =
            Arc::new(MockCancelAwareBackend::cancel_token(&MockConnection));
        let timer = QueryTimer::arm::<MockCancelAwareBackend>(Some(1), &token);
        std::thread::sleep(Duration::from_millis(1500));
        assert!(timer.fired(), "the deadline passed");

        assert_eq!(
            timer.reclassify(Ok(7)).expect("success must stay success"),
            7
        );
    }
}
