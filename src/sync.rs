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
