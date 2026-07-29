//! The one place the crate imports locks from.
//!
//! Under `--cfg loom`, in a test build, these resolve to `loom`'s
//! instrumented primitives, which is what lets the loom models in
//! `handles::registry` (`#[cfg(all(test, loom))] mod loom_tests`) explore
//! every interleaving of the handle lock discipline. A lock imported directly
//! from `std::sync` is invisible to loom, so it would silently opt that code
//! out of the proof. There is no second import path on purpose. Outside a
//! test build, `--cfg loom` has no effect: these aliases always resolve to
//! `std::sync`, since loom's interleaving machinery only runs inside
//! `loom::model`, which is itself test-only.
//!
//! Run the models with `RUSTFLAGS="--cfg loom" cargo test --lib loom_tests`.
//! The `loom_tests` filter is required, not cosmetic: this cfg switch also
//! applies to every other unit test in the crate, none of which are wrapped
//! in a `loom::model`, and they call the process-wide registry outside one,
//! which panics as soon as it resolves to loom's `RwLock` (see
//! `handles::registry::registry`'s doc comment). The filter is what keeps
//! those tests out of a build they were never meant to run under.

#[cfg(all(loom, test))]
pub(crate) use loom::sync::{Arc, Mutex, MutexGuard, RwLock};

#[cfg(not(all(loom, test)))]
pub(crate) use std::sync::{Arc, Mutex, MutexGuard, RwLock};

// `Condvar` is deliberately absent, and that is the crate's one exception to
// "every lock comes from here". `query_timer.rs` uses `std::sync::Condvar`
// directly, for two reasons its own comment records in full: loom's `Condvar`
// has no `wait_timeout_while` and its `wait_timeout` ignores the duration
// outright (loom 0.7.2: "TODO: implement timing out"), so an instrumented
// query timer could not model a timeout at all — and no loom model reaches
// that code, the models being of `Registry` and `GroupLock`.
//
// The exception is recorded in both places on purpose. The rule this module
// exists to enforce is not "use these types" but "no lock silently opts itself
// out of the proof", and a lock that opts out for a stated, checkable reason
// does not violate it. One that does so quietly would. Before adding a second
// exception, check whether loom can model the primitive at all: if it can, the
// answer is to import it here.

// Not lock-cfg-dependent: loom's `Mutex`/`RwLock` reuse `std::sync::TryLockError`
// verbatim rather than defining their own (confirmed in loom 0.7.2's own
// source), so one spelling already covers both configurations. Re-exported
// here anyway so lock code never has a reason to reach around this module for
// a piece of the same API.
pub(crate) use std::sync::TryLockError;
