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

/// What kind of removal produced this tombstone. Only `Forget` exists — BOTH the
/// `/memory forget` funnel (AC-R0) AND the Story 12.1c `MEMORY.md` file-edit-honor
/// path emit it: there is NO new variant, because both produce the byte-identical
/// record for the same fact (the `forget_and_fileedit_emit_identical_record` parity
/// test — no divergent removal path).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedactionOp {
    /// The user removed the entry — explicitly via `/memory forget`, or by
    /// hand-deleting it from `MEMORY.md` (the file-edit-honor path). Same record.
    Forget,
}

/// A durable record that the entry with this stable content key is permanently
/// redacted. The **source of truth** for removal (AC-R6): written and persisted
/// FIRST, before the one-time index purge, so "redacted ⇒ never retrievable"
/// holds even if the purge is interrupted — the next refresh/restart converges.
///
/// ## Two suppression identities (Story 12.1c AC3)
/// 11.4a keyed removal purely on the `u64` `key` (`blake3(ts_ms || summary)`).
/// That key is **timestamp-bearing**, so the SAME fact present in two timestamp
/// namespaces — a `MEMORY.md` copy keyed by the file mtime AND its append-only
/// daily-log copy keyed by the daily-log's real timestamp — hashes to two
/// different keys, and a single key-tombstone cannot suppress both (the
/// daily-log re-derivation leak, `project_scoped_memory::merge_dedup`). 12.1c
/// adds a **content-stable [`token`](Self::token)**: the same `normalize`d
/// summary identity `merge_dedup` already dedups on, so ONE tombstone suppresses
/// every copy across both namespaces, with NO index re-keying (11.4a invariants
/// intact). The `refresh()` gate drops a candidate if its `key` is redacted OR
/// its normalized text matches a redacted `token`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactionRecord {
    /// The redacted entry's stable `u64` content key (`blake3(ts_ms || summary)`).
    /// The 11.4a identity — still honored by the key-gate (explicit forget-by-key,
    /// within-window self-heal). For a content-stable record it is one of the
    /// possibly-many keys the fact occupies; the [`token`](Self::token) is what
    /// suppresses the rest.
    pub key: u64,
    /// **Content-stable suppression identity (Story 12.1c AC3).** The `normalize`d
    /// summary text (`adapters::long_term_memory::normalize` — the SAME function
    /// `merge_dedup` dedups on), so it matches the fact across every timestamp
    /// namespace (the `MEMORY.md`-mtime copy AND the daily-log-realts copy). Empty
    /// (`""`) means a legacy / key-only record (pre-12.1c `forget`, or a tombstone
    /// injected by key) — the key-gate alone applies, exactly as in 11.4a.
    ///
    /// NOTE (12.1c reconciliation): the token is `normalize(summary)`, NOT
    /// `normalize(category ‖ fact)` as the story draft first proposed. It is FORCED
    /// to `normalize(summary)`: the `/memory forget` producer only ever has a
    /// `MemoryEntry` (which carries no `category` field), so `normalize(summary)`
    /// is the ONLY identity both producers (forget + file-edit-honor) can compute
    /// identically — which is exactly what the `forget_and_fileedit_emit_identical_
    /// record` parity test asserts. It is also the identity `merge_dedup` already
    /// uses, so redaction stays consistent with cross-tier dedup.
    pub token: String,
    /// What removal produced this record.
    pub op: RedactionOp,
    /// When the redaction was issued, in the user's LOCAL timezone (consistent
    /// with [`MemoryEntry::timestamp`]). Diagnostic only — the `key`/`token` are
    /// identity (the parity test ignores `timestamp`).
    ///
    /// [`MemoryEntry::timestamp`]: crate::domain::models::MemoryEntry::timestamp
    pub timestamp: DateTime<Local>,
}

impl RedactionRecord {
    /// A key-only `Forget` tombstone (no content-stable token) for `key`, stamped
    /// at `timestamp`. Back-compat with 11.4a (the key-gate suppresses); a record
    /// built this way will NOT suppress a daily-log re-derivation copy — use
    /// [`Self::redact`] for the content-stable path.
    #[deprecated(
        note = "Use `redact` with a content-stable token instead. Key-only tombstones do not suppress daily-log re-derivation copies."
    )]
    pub fn forget(key: u64, timestamp: DateTime<Local>) -> Self {
        Self {
            key,
            token: String::new(),
            op: RedactionOp::Forget,
            timestamp,
        }
    }

    /// A content-stable `Forget` tombstone (Story 12.1c AC3): suppresses by BOTH
    /// the `u64` `key` AND the normalized-summary `token`. Both the `/memory
    /// forget` and the `MEMORY.md` file-edit-honor producers build their record
    /// this way, so the same fact yields a byte-identical record (modulo the
    /// diagnostic `timestamp`).
    pub fn redact(key: u64, token: String, timestamp: DateTime<Local>) -> Self {
        Self {
            key,
            token,
            op: RedactionOp::Forget,
            timestamp,
        }
    }
}
