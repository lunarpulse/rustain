//! Daemon crash record (Story 12.1b AC-12-1b-5) — the structured post-mortem state
//! `daemon status` reads after an unclean daemon exit.
//!
//! **Pure domain type** (no I/O crate imports — only serde + `std::path`). The
//! adapter (`adapters::daemon::crash`) owns serialization + atomic file I/O; this
//! module owns only the shape and the bounded-history fold logic.
//!
//! **Retention (party-mode Q2, 2026-06-06): latest-only.** `daemon-crash.json` holds
//! exactly ONE record (overwritten each crash) so `status` reads a single
//! deterministic shape/location — no ring-buffer-in-JSON eviction bug surface. The
//! crash-*loop* signal John flagged is preserved by a bounded `restart_count` +
//! `last_n_crash_unix` ring here, plus the immutable timestamped `crash-<ts>.log`
//! backtrace files the adapter writes (the real forensic trail, capped/rotated on
//! disk). Daily logs and memory are NEVER touched by this path.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Cap on the bounded crash-timestamp ring. Five recent crash times make a crash
/// loop visible in a single `status` read without unbounded growth.
pub const LAST_N_CRASH_CAP: usize = 5;

/// A single daemon crash, persisted latest-only to `daemon-crash.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonCrashRecord {
    /// PID of the daemon instance that died.
    pub pid: u32,
    /// Active profile of the dead instance.
    pub profile: String,
    /// Workspace the daemon operated in.
    pub workspace: PathBuf,
    /// Unix seconds the dead instance started (from its PID file). 0 if unknown.
    pub started_at_unix: u64,
    /// Unix seconds the crash was detected (restart-side) or the panic fired.
    pub detected_at_unix: u64,
    /// Computed uptime of the dead instance in seconds (`detected - started`).
    pub uptime_secs: u64,
    /// Why this is a crash record: `"stale-pidfile"` (restart-side detection of a
    /// leftover PID file whose process is dead) or `"panic: <message>"` (the daemon
    /// panic hook, carrying a backtrace in the sibling `crash-<ts>.log`).
    pub reason: String,
    /// Total crashes recorded for this workspace's daemon (monotonic counter; a
    /// high value here is the crash-loop signal). First crash = 1.
    pub restart_count: u32,
    /// The most recent crash-detection timestamps, newest last, capped at
    /// [`LAST_N_CRASH_CAP`]. A tight cluster of values is a crash loop.
    pub last_n_crash_unix: Vec<u64>,
}

impl DaemonCrashRecord {
    /// Fold this fresh crash into the prior record (if any), producing the record to
    /// persist: carry `restart_count` up by one and push `detected_at_unix` into the
    /// bounded ring (newest last, capped). **Pure** — no I/O; the caller reads the
    /// prior record and writes the result.
    pub fn fold_from_prior(mut self, prior: Option<&DaemonCrashRecord>) -> Self {
        let mut history = prior
            .map(|p| p.last_n_crash_unix.clone())
            .unwrap_or_default();
        history.push(self.detected_at_unix);
        let overflow = history.len().saturating_sub(LAST_N_CRASH_CAP);
        if overflow > 0 {
            history.drain(0..overflow);
        }
        self.restart_count = prior.map(|p| p.restart_count).unwrap_or(0) + 1;
        self.last_n_crash_unix = history;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(detected: u64) -> DaemonCrashRecord {
        DaemonCrashRecord {
            pid: 42,
            profile: "coding".into(),
            workspace: PathBuf::from("/ws"),
            started_at_unix: detected.saturating_sub(10),
            detected_at_unix: detected,
            uptime_secs: 10,
            reason: "stale-pidfile".into(),
            restart_count: 0,
            last_n_crash_unix: Vec::new(),
        }
    }

    #[test]
    fn first_crash_starts_count_at_one() {
        let folded = rec(1_000).fold_from_prior(None);
        assert_eq!(folded.restart_count, 1);
        assert_eq!(folded.last_n_crash_unix, vec![1_000]);
    }

    #[test]
    fn subsequent_crash_increments_and_appends() {
        let first = rec(1_000).fold_from_prior(None);
        let second = rec(2_000).fold_from_prior(Some(&first));
        assert_eq!(second.restart_count, 2);
        assert_eq!(second.last_n_crash_unix, vec![1_000, 2_000]);
    }

    #[test]
    fn ring_is_bounded_to_cap_newest_last() {
        let mut prior = rec(0).fold_from_prior(None); // count=1, [0]
        for t in 1..=LAST_N_CRASH_CAP as u64 + 2 {
            prior = rec(t).fold_from_prior(Some(&prior));
        }
        // restart_count keeps counting past the ring cap (loop signal stays honest).
        assert_eq!(prior.restart_count as usize, LAST_N_CRASH_CAP + 3);
        // The ring keeps only the newest CAP timestamps.
        assert_eq!(prior.last_n_crash_unix.len(), LAST_N_CRASH_CAP);
        let last = LAST_N_CRASH_CAP as u64 + 2;
        assert_eq!(*prior.last_n_crash_unix.last().unwrap(), last);
        assert_eq!(
            prior.last_n_crash_unix.first().unwrap(),
            &(last - LAST_N_CRASH_CAP as u64 + 1)
        );
    }
}
