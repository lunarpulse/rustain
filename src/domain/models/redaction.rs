//! `RedactionRecord` — a durable tombstone marking a memory entry as permanently
//! removed (Story 11.4a / FR122 — the Epic-11 "removal-integrity" closure gate).
//!
//! Pure domain value object (no I/O imports). Like [`MemoryEntry`] it is
//! intentionally **serde-free**: the search-index adapter persists it through a
//! private bincode DTO (mirroring 11.3a's `PersistedEntry`), so the domain type
//! never gains a serialization surface.
//!
//! ## Why a tombstone (architecture.md:175-176 — "the one must-test of this epic")
//! A removed fact must be gone — not just hidden. The persistent vector + BM25
//! index (Story 11.3a/b) re-derives itself from an append-only source on every
//! `refresh()`, so a one-time delete is silently undone ("the ghost re-embeds").
//! The fix is a tombstone persisted *with the index, never in the source*: it
//! gates `refresh()`/rebuild to skip the redacted key at embed time, making the
//! purge idempotent under reindex. The tombstone is keyed by the entry's stable
//! `u64` content key (`blake3(timestamp_ms || summary)`), the same identity the
//! index uses for incremental diffing — `MemoryEntry` has no id field and must
//! NOT gain one.
//!
//! [`MemoryEntry`]: crate::domain::models::MemoryEntry

use chrono::{DateTime, Local};

/// What kind of removal produced this tombstone. Only `Forget` exists today
/// (the `/memory forget` funnel — AC-R0); the enum is the seam for a future
/// file-edit-honoring path, which MUST emit the *identical* record through the
/// same funnel (no divergent removal path — `divergent_removal_paths`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedactionOp {
    /// The user explicitly removed the entry via `/memory forget`.
    Forget,
}

/// A durable record that the entry with this stable content key is permanently
/// redacted. The **source of truth** for removal (AC-R6): written and persisted
/// FIRST, before the one-time index purge, so "redacted ⇒ never retrievable"
/// holds even if the purge is interrupted — the next refresh/restart converges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactionRecord {
    /// The redacted entry's stable `u64` content key (`blake3(ts_ms || summary)`).
    pub key: u64,
    /// What removal produced this record.
    pub op: RedactionOp,
    /// When the redaction was issued, in the user's LOCAL timezone (consistent
    /// with [`MemoryEntry::timestamp`]). Diagnostic only — the `key` is identity.
    ///
    /// [`MemoryEntry::timestamp`]: crate::domain::models::MemoryEntry::timestamp
    pub timestamp: DateTime<Local>,
}

impl RedactionRecord {
    /// A `Forget` tombstone for `key`, stamped at `timestamp`.
    pub fn forget(key: u64, timestamp: DateTime<Local>) -> Self {
        Self {
            key,
            op: RedactionOp::Forget,
            timestamp,
        }
    }
}
