//! `init_logging` must not panic when a global `tracing` subscriber already exists.
//!
//! A driver shares its process with the application that loaded it, and with any
//! other driver the Driver Manager has loaded. Only one global subscriber can be
//! installed, so `init_logging` has to tolerate losing that race.
//!
//! This is an integration test rather than a unit test because both the
//! subscriber and `init_logging`'s `Once` are process-global, and core's own
//! test binary already calls `init_logging` from every `sql_alloc_handle` test.
//! Installing a competing subscriber there would race with it.

use stackable_odbc_core::logging::init_logging;

#[test]
fn init_logging_survives_an_application_that_already_installed_a_subscriber() {
    // Stand in for the host application, or for a second driver built on this
    // crate that got to the global subscriber slot first.
    tracing_subscriber::fmt()
        .with_writer(std::io::sink)
        .try_init()
        .expect("this test owns the process, so nothing else has set a subscriber yet");

    // Before the fix this panicked inside `SubscriberInitExt::init`, which is
    // `try_init().expect(...)`. `SQLAllocHandle(SQL_HANDLE_ENV, ...)` runs it
    // outside `panic_safe`, so the panic unwound across `extern "system"`.
    init_logging();

    // The first panic also poisoned the `Once`, so every later call panicked
    // with "Once instance has previously been poisoned" for the life of the
    // process. That turned one lost race into a permanently dead driver.
    init_logging();
}
