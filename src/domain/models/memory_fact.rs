//! `MemoryFact` — a single curated, topic-organized long-term memory record.
//!
//! Pure domain value object (no I/O imports). The long-term tier's record,
//! distinct from `MemoryEntry` (Story 11.1): `MemoryEntry` is the append-only
//! *operational* record (a timestamped daily-log line); `MemoryFact` is the
//! curated *durable* fact, organized by topic in `{workspace}/.rustain/MEMORY.md`
//! and rewritten on every upsert. Story 11.2 (Epic 11 — Memory Adapters &
//! Context Assembly).
//!
//! Q1 (Story 11.2): this is a SEPARATE value type rather than a `category`
//! field on `MemoryEntry`. Adding a field to `MemoryEntry` would churn ~12
//! just-shipped 11.1 construction sites and conflate two semantically distinct
//! records. The clean seam is this value object + the additive `remember_fact`
//! serde is now derived for wire-protocol use (Story 12.2d), but the on-disk
//! representation is still human-readable markdown (see `adapters/long_term_memory.rs`);
//! serde is NOT used for persistence.

/// One durable fact, preference, or piece of project knowledge the agent (or
/// the user, by hand-editing) curates into long-term memory. Produced by
/// `MemoryPort::remember_fact` and surfaced (mapped to `MemoryEntry`) by
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryFact {
    /// The topic / section this fact lives under (e.g. "Preferences",
    /// "Database"). Renders as a `## {category}` H2 in `MEMORY.md`.
    pub category: String,
    /// The fact itself — a short, human-readable statement. Renders as a
    /// `- {fact}` bullet under its category.
    pub fact: String,
    /// Optional supporting detail. Renders as an indented continuation line
    /// under the fact's bullet.
    pub detail: Option<String>,
}
