//! The one place the crate imports locks from.
//!
//! Under `--cfg loom` these resolve to `loom`'s instrumented primitives, which
//! is what lets `tests/loom_handles.rs` explore every interleaving of the
//! handle lock discipline. A lock imported directly from `std::sync` is
//! invisible to loom, so it would silently opt that code out of the proof.
//! There is no second import path on purpose.

#[cfg(loom)]
pub(crate) use loom::sync::{Arc, Mutex, MutexGuard, RwLock};

#[cfg(not(loom))]
pub(crate) use std::sync::{Arc, Mutex, MutexGuard, RwLock};

// Not lock-cfg-dependent: loom's `Mutex`/`RwLock` reuse `std::sync::TryLockError`
// verbatim rather than defining their own (confirmed in loom 0.7.2's own
// source), so one spelling already covers both configurations. Re-exported
// here anyway so lock code never has a reason to reach around this module for
// a piece of the same API.
pub(crate) use std::sync::TryLockError;
