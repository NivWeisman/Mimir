//! Logging setup for the language server.
//!
//! ## Critical LSP rule
//!
//! **stdout is reserved for JSON-RPC**. Anything we print to stdout will be
//! interpreted by the editor as a (malformed) protocol message and either
//! crash the client or corrupt the session. Therefore the formatter is
//! pinned to **stderr**, and we never expose any way to log to stdout.
//!
//! ## Filter
//!
//! We honor the `RUST_LOG` env var via [`tracing_subscriber::EnvFilter`].
//! Examples:
//!
//! ```text
//! RUST_LOG=mimir=debug              # all mimir-* crates at DEBUG
//! RUST_LOG=mimir_syntax=trace,warn  # parser TRACE, everything else WARN
//! RUST_LOG=info                     # global INFO
//! ```
//!
//! ## Test usage
//!
//! Unit tests can call [`init_for_tests`] to get colored stderr output
//! during `cargo test -- --nocapture`. It's a no-op if the subscriber is
//! already installed (so multiple tests calling it doesn't panic).

use std::sync::Once;

use tracing_subscriber::{EnvFilter, fmt, prelude::*};

/// Default filter directive when `RUST_LOG` is unset.
///
/// We err on the side of quiet: a release-mode LSP that writes to stderr on
/// every keystroke clutters the editor's log pane.
const DEFAULT_FILTER: &str = "warn,mimir=info";

/// Install the global tracing subscriber, sending logs to stderr.
///
/// Call this exactly once, near the top of `main()`. Idempotent: subsequent
/// calls return without doing anything.
///
/// Returns an error if a subscriber is already installed (which can happen
/// in tests that wire up their own subscriber). Production callers can
/// `.ok()` the result.
pub fn init() -> Result<(), tracing_subscriber::util::TryInitError> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));

    tracing_subscriber::registry()
        .with(filter)
        .with(
            fmt::layer()
                .with_writer(std::io::stderr)  // <-- IMPORTANT: never stdout
                .with_target(true)
                .with_ansi(false)              // editor log panes don't render ANSI
                .with_line_number(true)
                .with_file(true),
        )
        .try_init()
}

/// Install a tracing subscriber suitable for tests. Safe to call multiple
/// times — internally guarded by [`Once`].
pub fn init_for_tests() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let filter = EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new("debug"));
        let _ = tracing_subscriber::registry()
            .with(filter)
            .with(
                fmt::layer()
                    .with_writer(std::io::stderr)
                    .with_test_writer()        // route through libtest's capture
                    .with_target(true)
                    .with_ansi(true),
            )
            .try_init();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Calling `init_for_tests` twice must not panic — the `Once` guard
    /// makes the second call a no-op.
    #[test]
    fn init_for_tests_is_idempotent() {
        init_for_tests();
        init_for_tests();
    }
}
