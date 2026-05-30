//! Debug-only scope timing.
//!
//! Drop a [`ScopeTimer::new`] call (or the [`time_scope!`] macro, which
//! is just a one-liner wrapper) at the top of any function or block.
//! When `MIMIR_DEBUG_TIMING=1` is set in the server environment, an
//! info-level `tracing` event fires on scope exit with the elapsed
//! wall-clock milliseconds — letting you find hot paths without
//! shipping a profiler.
//!
//! When the env var is unset (the default), the only overhead per
//! scope is one atomic-cached `bool` read and an [`Instant::now()`]
//! that the `Drop` impl never formats. The env var is read **once per
//! process** via [`OnceLock`], so flipping it requires a restart.
//!
//! # Example
//!
//! ```ignore
//! fn assemble_elaborate_params(/* ... */) {
//!     mimir_core::time_scope!("assemble_elaborate_params");
//!     // ... slow work ...
//! }   // logs `debug_timer scope=assemble_elaborate_params ms=N` on drop
//!     // (only when MIMIR_DEBUG_TIMING=1)
//! ```
//!
//! Output reaches the same `tracing` subscriber as every other log line
//! — for `mimir-server` that's stderr.

use std::sync::OnceLock;
use std::time::Instant;

/// Cached result of the `MIMIR_DEBUG_TIMING` env-var lookup.
///
/// Read once per process. Subsequent calls to [`enabled`] are an atomic
/// load and a branch — cheap enough to drop into hot loops if the
/// caller wanted to (we don't, but it's safe).
static ENABLED: OnceLock<bool> = OnceLock::new();

fn enabled() -> bool {
    *ENABLED.get_or_init(|| {
        matches!(std::env::var("MIMIR_DEBUG_TIMING").as_deref(), Ok(v) if v != "0")
    })
}

/// RAII scope timer. Logs elapsed milliseconds on drop when
/// `MIMIR_DEBUG_TIMING=1` is set; no-op otherwise.
///
/// Prefer the [`time_scope!`](crate::time_scope) macro at call sites —
/// it picks a hygienic binding name and avoids the user having to spell
/// the type.
pub struct ScopeTimer {
    name: &'static str,
    start: Instant,
    on: bool,
}

impl ScopeTimer {
    /// Construct a timer named `name`. The name appears verbatim in the
    /// log line so prefer short identifier-style labels.
    #[must_use]
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            start: Instant::now(),
            on: enabled(),
        }
    }
}

impl Drop for ScopeTimer {
    fn drop(&mut self) {
        if !self.on {
            return;
        }
        let ms = self.start.elapsed().as_millis() as u64;
        tracing::info!(scope = self.name, ms, "debug_timer");
    }
}

/// Start a [`ScopeTimer`] bound to a hygienic local. The timer logs
/// elapsed milliseconds when the enclosing scope ends.
///
/// ```ignore
/// fn slow_thing() {
///     mimir_core::time_scope!("slow_thing");
///     // ...
/// }
/// ```
#[macro_export]
macro_rules! time_scope {
    ($name:literal) => {
        let __mimir_scope_timer = $crate::debug_timer::ScopeTimer::new($name);
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Constructing a timer with the env unset is essentially free —
    /// no allocation, no formatting on drop. We can't observe the
    /// log directly without a subscriber, but we can verify `enabled()`
    /// returns the cached value and `Drop` runs without panic.
    #[test]
    fn timer_no_panic_when_disabled() {
        // Force ENABLED to be initialised before we observe it; the
        // test process has no env var, so this caches `false`.
        let _ = enabled();
        let t = ScopeTimer::new("test_disabled");
        std::thread::sleep(std::time::Duration::from_millis(1));
        drop(t);
    }
}
