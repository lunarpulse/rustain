//! `SessionBoundary` — the single daemon-lifecycle signal (Story 12.1a).
//!
//! Story 12.1a AC-12-1a-7 establishes **one code path, three triggers**: a
//! `daily_reset` timer, an `idle_timeout` timer, and graceful shutdown all emit
//! exactly one `SessionBoundary` through a single shared seam (see
//! `adapters::daemon::lifecycle::emit_session_boundary`), never three divergent
//! ones.
//!
//! This type is **pure domain** — no I/O, no adapter imports — so it can be the
//! shared vocabulary between the daemon adapter (which emits it) and the future
//! Story 12.1c hooks (memory consolidation / MEMORY.md-honor / `on_session_end`)
//! that will hang off the boundary's documented no-op extension point. 12.1a
//! MUST NOT implement any 12.1c behaviour here.
//!
//! In non-daemon (one-shot TUI) mode this signal is **not** emitted —
//! process-exit remains the only boundary there (the hooks stay daemon-gated).

/// A daemon session boundary and which trigger raised it.
///
/// The three variants are the *only* triggers in 12.1a. Adding a fourth means a
/// new lifecycle event — route it through the same `emit_session_boundary` seam,
/// never a parallel path (the AC-12-1a-7 invariant).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionBoundary {
    /// The configured `[daemon] daily_reset = "HH:MM"` wall-clock time arrived.
    /// 12.1a action: finalize the daily log + reset conversation context.
    DailyReset,
    /// No activity occurred for `[daemon] idle_timeout`; the daemon enters its
    /// low-power state. Emitting a boundary here is configurable (default on).
    IdleTimeout,
    /// Graceful shutdown began (SIGTERM/SIGINT or `rustain daemon stop`).
    Shutdown,
}

impl SessionBoundary {
    /// Stable, log-friendly identifier for the trigger.
    pub fn as_str(self) -> &'static str {
        match self {
            SessionBoundary::DailyReset => "daily_reset",
            SessionBoundary::IdleTimeout => "idle_timeout",
            SessionBoundary::Shutdown => "shutdown",
        }
    }
}

impl std::fmt::Display for SessionBoundary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_str_is_stable_per_variant() {
        assert_eq!(SessionBoundary::DailyReset.as_str(), "daily_reset");
        assert_eq!(SessionBoundary::IdleTimeout.as_str(), "idle_timeout");
        assert_eq!(SessionBoundary::Shutdown.as_str(), "shutdown");
    }

    #[test]
    fn display_matches_as_str() {
        assert_eq!(SessionBoundary::Shutdown.to_string(), "shutdown");
    }
}
