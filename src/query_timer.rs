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
// Deliberately `std::sync::Arc`, not `crate::sync::Arc`, for the type-erased
// cancel token below — the same exception `Slot::cancel` documents in
// `handles/registry.rs`. It is a refcounted payload rather than a lock, so
// instrumenting it under loom would buy nothing, and loom's `Arc` cannot hold a
// `dyn Any` at all: it has no `CoerceUnsized` impl, so
// `Arc::new(x) as Arc<dyn Any + Send + Sync>` does not compile for it. Using
// `crate::sync::Arc` here made the whole crate fail to build under
// `--cfg loom`, which took every loom model down with it.
use std::sync::Arc as StdArc;
use std::time::Duration;

use crate::backend::Backend;
use crate::cancel::CancelState;
use crate::errors::OdbcError;
// `std::sync`, not `crate::sync`, and this is the crate's one documented
// exception to that rule — see `sync.rs`, which records it too.
//
// Two facts make it the right call rather than a shortcut. loom's `Condvar`
// has no `wait_timeout_while`, and its `wait_timeout` **ignores the duration
// entirely**, always reporting `WaitTimeoutResult(false)` (loom 0.7.2's own
// source says "TODO: implement timing out"). So an instrumented query timer
// could not model a timeout — the only thing about it worth modelling. And no
// loom model reaches this code: the models are of `Registry` and `GroupLock`
// in `handles/registry.rs`, and this timer participates in neither.
//
// Importing `crate::sync` here bought nothing and cost everything: it made the
// whole crate fail to compile under `--cfg loom`, which took down the models
// that do matter. `Arc` stays `std`'s for the separate reason above.
use crate::types::SqlState;
use std::sync::{Condvar, Mutex};

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
    shared: Option<StdArc<Shared>>,
    /// This execution's cancel token, held so [`Self::reclassify`] can ask it
    /// whether an *earlier* call's timer already cancelled it.
    ///
    /// Kept even when `shared` is `None`, which is not redundant: the
    /// application may clear `SQL_ATTR_QUERY_TIMEOUT` between the execute that
    /// timed out and the fetch that fails because of it, and that fetch is
    /// still reporting a failure the deadline caused.
    token: Option<StdArc<dyn Any + Send + Sync>>,
}

impl QueryTimer {
    /// A timer that never fires, for a call with no core-enforced deadline.
    ///
    /// For a call that has no cancel token at all — nothing has ever run on
    /// this statement, so no earlier deadline can have signalled anything
    /// either. A call that *has* a token but no deadline goes through
    /// [`Self::arm`], which keeps the token.
    pub(crate) fn disarmed() -> Self {
        Self {
            shared: None,
            token: None,
        }
    }

    /// A timer that never fires, but still carries this execution's token.
    fn untimed(token: &StdArc<dyn Any + Send + Sync>) -> Self {
        Self {
            shared: None,
            token: Some(StdArc::clone(token)),
        }
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
        token: &StdArc<dyn Any + Send + Sync>,
    ) -> Self {
        let Some(seconds) = seconds.filter(|s| *s > 0) else {
            return Self::untimed(token);
        };
        let deadline = Duration::from_secs(seconds as u64);

        let shared = StdArc::new(Shared {
            state: Mutex::new(State::Running),
            signal: Condvar::new(),
        });
        let thread_shared = StdArc::clone(&shared);
        let thread_token = StdArc::clone(token);

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

                // Through `cancel_state_as`, not a `downcast_ref` written out
                // here: the stored type is named in exactly one place, so this
                // cannot drift from what `mint_cancel_token` puts in the
                // registry.
                match crate::handles::cancel_state_as::<B>(&thread_token) {
                    Ok(cancel) => {
                        tracing::warn!(
                            "SQL_ATTR_QUERY_TIMEOUT of {}s expired; cancelling the statement",
                            seconds
                        );
                        // Before the cancel, not after: this is what any later
                        // call on the same cursor reads to tell a deadline
                        // apart from a `SQLCancel`, and it must be visible to
                        // anyone who can already see the cancellation itself.
                        cancel.mark_timed_out();
                        if let Err(e) = B::cancel(cancel.token()) {
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
                    Err(e) => tracing::error!("query-timeout cancel token unusable: {e}"),
                }
                thread_shared.signal.notify_all();
            });

        match spawned {
            Ok(_handle) => Self {
                shared: Some(shared),
                token: Some(StdArc::clone(token)),
            },
            Err(e) => {
                // Out of threads. The call still runs, just without a deadline
                // — strictly better than refusing to execute at all, and the
                // application already holds a `SQL_SUCCESS` for the attribute.
                tracing::error!("could not spawn the query-timeout thread: {e}; running untimed");
                Self::untimed(token)
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

    /// Whether a core timer — this call's or an earlier call's on the same
    /// cursor — cancelled the token this call is running against.
    ///
    /// Both halves are needed. [`Self::fired`] alone misses the window this
    /// method exists to close: a deadline that expires as the backend call is
    /// returning delivers its cancel, the call succeeds anyway (which the spec
    /// permits), and *that* timer is then dropped — so the next failing call on
    /// the cursor has a signalled token and a timer of its own that never
    /// fired. `CancelState::timed_out` alone would miss the other end of the
    /// same window, the instant after the thread records `FiredCancel` and
    /// before it has marked the token.
    fn timed_out<B: Backend>(&self) -> bool {
        self.fired()
            || self.token.as_ref().is_some_and(|token| {
                // `cancel_state_as`, for the reason its doc comment gives: the
                // stored type is named once, in that function.
                crate::handles::cancel_state_as::<B>(token).is_ok_and(CancelState::timed_out)
            })
    }

    /// Relabel a failed backend call as `HYT00` when a core-side deadline
    /// cancelled it — this call's, or an earlier one on the same token (see
    /// [`Self::timed_out`]).
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
        self.reclassify::<B, _>(crate::cancel::reclassify_cancelled::<B, _, _>(
            result, cancel,
        ))
    }

    /// [`Self::check`] for a caller that may not have a token.
    ///
    /// The cursor-consuming entry points read their token off the registry
    /// rather than minting one, so they hold an `Option` — see
    /// [`crate::cancel::reclassify_cancelled_opt`], whose `None` case this
    /// shares. The timeout pass still runs: `None` means no *cancellation* can
    /// be attributed, not that no deadline was armed, and a timer armed on this
    /// call is the one thing that could have signalled a token that was never
    /// minted.
    ///
    /// The `cancel` argument being `None` says nothing about the timeout pass
    /// either way. That pass reads the token this timer was *armed* with, which
    /// is the same one when there is one at all — the entry point resolves it
    /// once and hands it to both.
    pub(crate) fn check_opt<B: Backend, T, E: Into<OdbcError>>(
        &self,
        result: Result<T, E>,
        cancel: Option<&B::CancelToken>,
    ) -> Result<T, OdbcError> {
        self.reclassify::<B, _>(crate::cancel::reclassify_cancelled_opt::<B, _, _>(
            result, cancel,
        ))
    }

    /// The timeout pass on its own, generic over the backend because deciding
    /// whether a deadline caused this failure means reading the token, and the
    /// token is `B`'s.
    pub(crate) fn reclassify<B: Backend, T>(
        &self,
        result: Result<T, OdbcError>,
    ) -> Result<T, OdbcError> {
        match result {
            Ok(value) => Ok(value),
            Err(e) if self.timed_out::<B>() => {
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
        let token: StdArc<dyn Any + Send + Sync> = StdArc::new(CancelState::new(
            MockCancelAwareBackend::cancel_token(&MockConnection),
        ));
        let timer = QueryTimer::arm::<MockCancelAwareBackend>(Some(1), &token);

        // Longer than the deadline: stands in for a backend call that overruns.
        std::thread::sleep(Duration::from_millis(1500));

        let cancel = token
            .downcast_ref::<CancelState<crate::test_utils::MockCancelToken>>()
            .expect("the token this test built")
            .token();
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
        let token: StdArc<dyn Any + Send + Sync> = StdArc::new(CancelState::new(
            MockCancelAwareBackend::cancel_token(&MockConnection),
        ));
        let timer = QueryTimer::arm::<MockCancelAwareBackend>(Some(60), &token);
        assert!(!timer.fired());
        drop(timer);

        // Well past any plausible scheduling delay, but nowhere near the 60s
        // deadline: if the timer were still armed it would not have fired yet
        // either, so the assertion below is about `cancelled` staying false for
        // the right reason.
        std::thread::sleep(Duration::from_millis(200));

        let cancel = token
            .downcast_ref::<CancelState<crate::test_utils::MockCancelToken>>()
            .expect("the token this test built")
            .token();
        assert!(
            !cancel.cancelled.load(Ordering::SeqCst),
            "a disarmed timer must never cancel"
        );
    }

    #[test]
    fn no_deadline_arms_nothing() {
        let token: StdArc<dyn Any + Send + Sync> = StdArc::new(CancelState::new(
            MockCancelAwareBackend::cancel_token(&MockConnection),
        ));
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
            .reclassify::<MockCancelAwareBackend, ()>(Err(OdbcError::general(
                "backend said no",
                SqlState::general_error(),
            )))
            .expect_err("the input was an error");
        assert_eq!(err.sqlstate().as_str(), "HY000");
    }

    #[test]
    #[cfg_attr(miri, ignore = "wall-clock timing; no unsafe to check")]
    fn reclassify_turns_a_fired_timers_error_into_hyt00() {
        let token: StdArc<dyn Any + Send + Sync> = StdArc::new(CancelState::new(
            MockCancelAwareBackend::cancel_token(&MockConnection),
        ));
        let timer = QueryTimer::arm::<MockCancelAwareBackend>(Some(1), &token);
        std::thread::sleep(Duration::from_millis(1500));

        let err = timer
            .reclassify::<MockCancelAwareBackend, ()>(Err(OdbcError::general(
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

    /// The bug this pins: a deadline that expires as the backend call is
    /// returning signals the token but leaves the call successful (which the
    /// spec permits), and the token then stays signalled for the life of the
    /// cursor it opened. The next call that fails on that cursor — a
    /// `SQLFetch`, quite likely one the delivered cancel caused — must report
    /// the timeout that actually happened, not the `SQLCancel` that never did.
    #[test]
    #[cfg_attr(miri, ignore = "wall-clock timing; no unsafe to check")]
    fn a_token_signalled_by_the_timer_reports_hyt00_on_the_next_failing_call() {
        let token: StdArc<dyn Any + Send + Sync> = StdArc::new(CancelState::new(
            MockCancelAwareBackend::cancel_token(&MockConnection),
        ));
        let expired = QueryTimer::arm::<MockCancelAwareBackend>(Some(1), &token);
        std::thread::sleep(Duration::from_millis(1500));
        assert!(expired.fired(), "the deadline passed");
        drop(expired);

        let cancel = token
            .downcast_ref::<CancelState<crate::test_utils::MockCancelToken>>()
            .expect("the token this test built")
            .token();
        assert!(
            cancel.cancelled.load(Ordering::SeqCst),
            "the timer delivered its cancel"
        );

        // The later call: its own timer is armed and has not fired.
        let later = QueryTimer::arm::<MockCancelAwareBackend>(Some(60), &token);
        assert!(!later.fired(), "this call's own deadline has not passed");
        let err = later
            .check_opt::<MockCancelAwareBackend, (), crate::test_utils::MockError>(
                Err(crate::test_utils::MockError),
                Some(cancel),
            )
            .expect_err("the input was an error");
        assert_eq!(
            err.sqlstate().as_str(),
            "HYT00",
            "the application set a deadline and never called SQLCancel"
        );
    }

    /// The other side of the test above, and the reason the record lives in
    /// the token's own allocation: `mint_cancel_token` builds a new one per
    /// execution, so a deadline that expired on one execution must be invisible
    /// to the next. A flag on the statement, or anywhere process-wide, would
    /// fail this — and it is the same "cancelled forever" shape the spec rules
    /// quoted on `mint_cancel_token` rule out ("After the statement has been
    /// canceled, the application can call SQLExecute or SQLExecDirect again").
    ///
    /// No timer thread and no sleep: `mark_timed_out` is exactly what the timer
    /// thread does on expiry, and `an_expired_deadline_cancels_the_token`
    /// covers the thread reaching it.
    #[test]
    fn a_later_executions_token_carries_no_earlier_timeout() {
        let expired: StdArc<dyn Any + Send + Sync> = StdArc::new(CancelState::new(
            MockCancelAwareBackend::cancel_token(&MockConnection),
        ));
        expired
            .downcast_ref::<CancelState<crate::test_utils::MockCancelToken>>()
            .expect("the token this test built")
            .mark_timed_out();

        // What the next statement-producing call mints.
        let fresh: StdArc<dyn Any + Send + Sync> = StdArc::new(CancelState::new(
            MockCancelAwareBackend::cancel_token(&MockConnection),
        ));
        let timer = QueryTimer::arm::<MockCancelAwareBackend>(Some(60), &fresh);
        let cancel = fresh
            .downcast_ref::<CancelState<crate::test_utils::MockCancelToken>>()
            .expect("the token this test built")
            .token();

        let err = timer
            .check::<MockCancelAwareBackend, (), crate::test_utils::MockError>(
                Err(crate::test_utils::MockError),
                cancel,
            )
            .expect_err("the input was an error");
        // `MockError` converts to `OdbcError::NotImplemented`, so `HYC00` is
        // the backend's own state passing through untouched. Asserting that
        // rather than "not HYT00" fails for one reason instead of any.
        assert_eq!(
            err.sqlstate().as_str(),
            "HYC00",
            "the previous execution's expired deadline must not reach this one"
        );
    }

    /// `QueryTimer::check`'s two passes in one call: the token is signalled
    /// *and* this call's timer fired, so both would produce a SQLSTATE and only
    /// the second one to run survives. `HYT00` must win — reversing the passes
    /// would report every expired deadline as a plain cancellation.
    ///
    /// The ordering was **not** unpinned before this test existed, contrary to
    /// the test-gap audit that asked for it: swapping the two passes at the
    /// parent commit already failed
    /// `execute::an_execution_that_overruns_its_query_timeout_reports_hyt00`
    /// and `fetch::a_fetch_that_overruns_its_query_timeout_reports_hyt00`,
    /// both of which drive a real overrun through the FFI entry points. This
    /// is the unit-level restatement: same property, failing in `check` itself
    /// rather than three layers up, where the message names the pass ordering
    /// instead of a diagnostic record on a statement handle.
    #[test]
    #[cfg_attr(miri, ignore = "wall-clock timing; no unsafe to check")]
    fn simultaneous_cancel_and_timeout_reports_hyt00() {
        let token: StdArc<dyn Any + Send + Sync> = StdArc::new(CancelState::new(
            MockCancelAwareBackend::cancel_token(&MockConnection),
        ));
        let timer = QueryTimer::arm::<MockCancelAwareBackend>(Some(1), &token);
        std::thread::sleep(Duration::from_millis(1500));
        assert!(timer.fired(), "the deadline passed");

        let cancel = token
            .downcast_ref::<CancelState<crate::test_utils::MockCancelToken>>()
            .expect("the token this test built")
            .token();
        assert!(
            cancel.cancelled.load(Ordering::SeqCst),
            "the cancel pass has something to see"
        );

        let err = timer
            .check::<MockCancelAwareBackend, (), crate::test_utils::MockError>(
                Err(crate::test_utils::MockError),
                cancel,
            )
            .expect_err("the input was an error");
        assert_eq!(
            err.sqlstate().as_str(),
            "HYT00",
            "the timeout pass runs second so the more specific state wins"
        );
    }

    /// The spec's rule that a cancelled call may still succeed, applied to the
    /// timeout path: "it is possible for the execution to succeed and return
    /// SQL_SUCCESS while the cancel is also successful."
    #[test]
    #[cfg_attr(miri, ignore = "wall-clock timing; no unsafe to check")]
    fn a_query_that_beats_its_deadline_to_the_finish_line_still_succeeds() {
        let token: StdArc<dyn Any + Send + Sync> = StdArc::new(CancelState::new(
            MockCancelAwareBackend::cancel_token(&MockConnection),
        ));
        let timer = QueryTimer::arm::<MockCancelAwareBackend>(Some(1), &token);
        std::thread::sleep(Duration::from_millis(1500));
        assert!(timer.fired(), "the deadline passed");

        assert_eq!(
            timer
                .reclassify::<MockCancelAwareBackend, _>(Ok(7))
                .expect("success must stay success"),
            7
        );
    }
}
