//! loom models for the handle registry's lock discipline.
//!
//! Only built under `--cfg loom`; see AGENTS.md "Concurrency testing".
#![cfg(loom)]

use loom::sync::{Arc, Mutex};
use loom::thread;

/// Proves the harness itself works: two threads incrementing under one mutex
/// always land on 2, on every interleaving loom explores.
#[test]
fn loom_harness_runs() {
    loom::model(|| {
        let counter = Arc::new(Mutex::new(0u32));
        let other = Arc::clone(&counter);
        let handle = thread::spawn(move || {
            *other.lock().unwrap() += 1;
        });
        *counter.lock().unwrap() += 1;
        handle.join().unwrap();
        assert_eq!(*counter.lock().unwrap(), 2);
    });
}
