//! `tracing`-based logging, configured by `ODBC_LOG_LEVEL` and `ODBC_LOG_FILE`.
//!
//! # Threat model
//!
//! A log file is a disclosure surface, so what reaches it and who controls its
//! path are stated here rather than left to be re-derived.
//!
//! **What a log can contain.** Connection parameters other than credentials,
//! meaning host, port, database and user, plus the catalog functions' filter
//! arguments, which are schema, table and column names. Passwords are not among
//! them: `ConnectParams` implements `Debug` by hand to redact credential
//! keywords, and `debug_redacts_password_key` and
//! `debug_redacts_credential_keywords_beyond_password_and_pwd` pin it. Neither
//! statement text nor parameter or column *values* are logged at any level, so a
//! log is schema-revealing and not data-revealing.
//!
//! **Who chooses the path.** Whoever can set `ODBC_LOG_FILE` in the environment
//! of the process the driver is loaded into, which is the application itself, or
//! someone who can already run code as that user and therefore read anything the
//! driver can. The variable is not a privilege boundary, and enabling logging is
//! a decision by the process rather than an attacker-reachable toggle.
//!
//! **Symlinks are followed.** The file is opened with `create(true)`
//! and `append(true)` and no `O_NOFOLLOW`, so a pre-existing symlink at the
//! configured path is followed and a pre-existing regular file is appended to.
//! Given the paragraph above that is not a privilege escalation; what remains is
//! the classic world-writable-directory case, where a legitimate user configures
//! a path under `/tmp` and another user pre-creates a link there. The
//! mitigation is operational: do not configure a log path in a world-writable
//! directory. Core does not refuse one, because it cannot tell a shared
//! directory from an intended one, and a driver that declines to log is its own
//! denial of service.
//!
//! **Mode `0o600` applies at creation only.** An existing file keeps whatever
//! mode it already has, which the comment at the `open` call repeats where it is
//! actionable.

use std::sync::Once;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

static INIT: Once = Once::new();

/// Initialize logging for the ODBC driver.
/// Call this once during driver startup (e.g., in SQLAllocHandle for SQL_HANDLE_ENV).
/// Subsequent calls are no-ops.
///
/// Installing the subscriber is best-effort. A driver shares its process with
/// the application that loaded it, and that application may have installed a
/// global `tracing` subscriber of its own; so may a second driver built on this
/// crate, loaded into the same Driver Manager. Only one global subscriber can
/// exist, so this function keeps whichever one got there first and gives up
/// silently rather than failing.
///
/// It must not panic. `SQLAllocHandle(SQL_HANDLE_ENV, ...)` is the first call
/// every ODBC application makes, and it runs this before entering `panic_safe`.
/// A panic here would unwind across the `extern "system"` boundary, which is
/// undefined behaviour, and would poison the `Once` so that every later call
/// panics too.
///
/// Environment variables:
/// - `ODBC_LOG_LEVEL`: tracing filter (e.g. "info", "debug", "trace"). Default: "off".
/// - `ODBC_LOG_FILE`: log file path. Default: stderr.
/// - `ODBC_PROFILING`: when set to "1", enables `FmtSpan::CLOSE` on the fmt layer
///   so that span exit events include `time.busy` and `time.idle` durations. This
///   is the mechanism used to measure per-phase timing in a backend's client.
///   When unset, no span events are emitted and overhead is near-zero.
pub fn init_logging() {
    INIT.call_once(|| {
        let filter =
            EnvFilter::try_from_env("ODBC_LOG_LEVEL").unwrap_or_else(|_| EnvFilter::new("off"));

        let profiling = std::env::var("ODBC_PROFILING")
            .map(|v| v == "1")
            .unwrap_or(false);

        let span_events = if profiling {
            FmtSpan::CLOSE
        } else {
            FmtSpan::NONE
        };

        if let Ok(log_file) = std::env::var("ODBC_LOG_FILE") {
            let mut options = std::fs::OpenOptions::new();
            options.create(true).append(true);
            // The log records connection parameters, so it must not be
            // world-readable. Without an explicit mode it inherits the umask,
            // which is commonly 0644. This applies at creation only: an
            // existing log file keeps whatever mode it already has.
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let file = options.open(&log_file);
            match file {
                Ok(file) => {
                    // `try_init` rather than `init`: see the note on this
                    // function. `init` is `try_init().expect(...)`.
                    let _ = tracing_subscriber::registry()
                        .with(filter)
                        .with(
                            fmt::layer()
                                .compact()
                                .with_ansi(false)
                                .with_span_events(span_events)
                                // `std::sync::Mutex`, not `crate::sync::Mutex`, and
                                // the crate's second stated exception to that rule
                                // (see `sync.rs`): `tracing_subscriber` implements
                                // `MakeWriter` for `std::sync::Mutex<W>` specifically,
                                // so loom's unrelated `Mutex` type would not compile
                                // here. No loom model reaches logging.
                                .with_writer(std::sync::Mutex::new(file)),
                        )
                        .try_init();
                }
                Err(_) => {
                    // Fallback to stderr
                    let _ = tracing_subscriber::registry()
                        .with(filter)
                        .with(
                            fmt::layer()
                                .compact()
                                .with_span_events(span_events)
                                .with_writer(std::io::stderr),
                        )
                        .try_init();
                }
            }
        } else {
            let _ = tracing_subscriber::registry()
                .with(filter)
                .with(
                    fmt::layer()
                        .compact()
                        .with_span_events(span_events)
                        .with_writer(std::io::stderr),
                )
                .try_init();
        }
    });
}
