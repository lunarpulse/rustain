//! Durable session-boundary intents (Story 12.1c AC2 + AC3) — the two records the
//! headless daemon QUEUES at a `SessionBoundary` for an interactive consumer (a
//! TUI attach, Story 12.2) to surface later.
//!
//! **Pure domain types** (serde + `std::path` only — no I/O). The adapter
//! (`adapters::daemon::session_queue`) owns atomic file I/O; this module owns only
//! the shapes. Both follow the 12.1b `DaemonCrashRecord` discipline: latest-only,
//! atomic temp→rename, daily logs NEVER touched.
//!
//! ## Why queue instead of act?
//! The headless daemon composes the **memory port only** — no `StreamingProvider`,
//! no conversation, no TUI until Story 12.2 (verified: `DaemonRuntime`). So:
//! - **AC2** (`ConsolidationDueMarker`): the boundary cannot run an LLM
//!   consolidation sub-turn (no provider) — it records a *trigger + a reference to
//!   the daily-log slice* to consolidate. 12.2 generates the suggestion and renders
//!   it through the existing 11.2a `PendingConsolidationCard` grammar. Never
//!   auto-applied; daily logs never deleted.
//! - **AC3** (`MemoryMdPurgeNotice`): the file-edit purge runs LIVE at the boundary
//!   (hand-edit = consent), but "never silent" is satisfied by queuing this audit
//!   notice for the next attach — NOT by withholding the purge.

use serde::{Deserialize, Serialize};

/// AC2 — a durable "consolidation is due" trigger queued at a session boundary.
/// Carries NO generated suggestion (the daemon has no engine to produce one); only
/// the trigger + a reference to the daily-log slice 12.2 should consolidate.
/// Latest-only: one pending marker, overwritten each boundary (idempotent, no
/// unbounded growth).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsolidationDueMarker {
    /// Which boundary raised it (`daily_reset` / `idle_timeout` / `shutdown`) —
    /// diagnostic, so 12.2 can show "consolidation suggested at shutdown".
    pub boundary: String,
    /// Unix seconds the marker was queued.
    pub queued_at_unix: u64,
    /// Reference to the daily-log slice to consolidate — the local date
    /// (`YYYY-MM-DD`) whose operational records 12.2 should propose promoting to
    /// durable `MEMORY.md` facts. NOT a generated suggestion.
    pub daily_log_ref: String,
}

/// AC3 — a durable audit notice that the file-edit-honor path purged `n` facts the
/// user hand-deleted from `MEMORY.md`. Surfaced (not gated) at the next attach so
/// the live purge is "never silent". Latest-only with a bounded `recent_summaries`
/// preview (full detail lives in the search index's redaction sidecar).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryMdPurgeNotice {
    /// How many facts were purged in the most recent honor pass.
    pub purged_count: usize,
    /// Unix seconds the notice was queued.
    pub queued_at_unix: u64,
    /// A bounded preview of the purged fact summaries (capped at
    /// [`PURGE_NOTICE_PREVIEW_CAP`]) for a human-readable attach message.
    pub recent_summaries: Vec<String>,
}

/// Cap on the purge-notice summary preview — enough to be informative without
/// growing the queue file unbounded on a large hand-edit.
pub const PURGE_NOTICE_PREVIEW_CAP: usize = 10;

impl MemoryMdPurgeNotice {
    /// The human-facing one-line message surfaced at attach (Story 12.2).
    pub fn message(&self) -> String {
        format!(
            "{} fact{} removed from MEMORY.md — purged from search index",
            self.purged_count,
            if self.purged_count == 1 { "" } else { "s" }
        )
    }
}
