//! `MemoryEntry` — a single append-only operational memory record.
//!
//! Pure domain value object (no I/O imports). The on-disk representation is
//! human-readable markdown (see `adapters/daily_log_memory.rs`), NOT serde-JSON,
//! so `serde` is intentionally NOT derived — entries round-trip through the
//! adapter's markdown parser. Story 11.1 (Epic 11 — Memory Adapters & Context
//! Assembly): this is the epic's only new value object. Provenance / redaction /
//! relevance (`ProvenancedEntry`, `RedactionRecord`, `Relevance`) are LATER
//! stories (11.4+) and MUST NOT be added here.

use chrono::{DateTime, Local};

/// One notable outcome the agent decided to record (a decision, file change, or
/// completed task). Produced by `MemoryPort::store` and returned by
/// `MemoryPort::recent` / `MemoryPort::search`.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryEntry {
    /// When the entry was recorded, in the user's LOCAL timezone. Local is used
    /// deliberately — the user reasons about "today" locally, consistent with
    /// `infrastructure/clock_util.rs`. The daily-log filename derives from this.
    pub timestamp: DateTime<Local>,
    /// Short, human-readable summary of the action / decision.
    pub summary: String,
    /// Optional longer context body (multi-line markdown permitted).
    pub context: Option<String>,
}
