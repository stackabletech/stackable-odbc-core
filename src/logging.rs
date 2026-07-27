//! `tracing`-based logging, configured by `ODBC_LOG_LEVEL` and `ODBC_LOG_FILE`.

use std::sync::Once;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

static INIT: Once = Once::new();

/// Initialize logging for the ODBC driver.
/// Call this once during driver startup (e.g., in SQLAllocHandle for SQL_HANDLE_ENV).
/// Subsequent calls are no-ops.
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
                    tracing_subscriber::registry()
                        .with(filter)
                        .with(
                            fmt::layer()
                                .compact()
                                .with_ansi(false)
                                .with_span_events(span_events)
                                .with_writer(std::sync::Mutex::new(file)),
                        )
                        .init();
                }
                Err(_) => {
                    // Fallback to stderr
                    tracing_subscriber::registry()
                        .with(filter)
                        .with(
                            fmt::layer()
                                .compact()
                                .with_span_events(span_events)
                                .with_writer(std::io::stderr),
                        )
                        .init();
                }
            }
        } else {
            tracing_subscriber::registry()
                .with(filter)
                .with(
                    fmt::layer()
                        .compact()
                        .with_span_events(span_events)
                        .with_writer(std::io::stderr),
                )
                .init();
        }
    });
}
