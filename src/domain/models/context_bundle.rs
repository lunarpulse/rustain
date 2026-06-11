//! `ContextBundle` and friends — the Content-tier boundary object the
//! context-assembly adapter (`ContextPort`, Story 11.4) returns.
//!
//! This is the forward-compatible value object Stories 11.5 (`RecallBlock` →
//! `[recall: <provider>]`) and 11.6 (`WindowingAssembler` → `[group: {N}]`)
//! both consume, so three design constraints are baked in NOW (epics.md
//! 5172–5182, architecture.md:173–176):
//!
//! 1. **`ContextSource` is OPEN-ENDED** (`#[non_exhaustive]` + named variants
//!    covering the 11.5 / 11.6 forward arms). The `Custom(String)` catch-all
//!    from the original sketch is deliberately DROPPED (Panel-Review Amendment
//!    4, 2026-06-01): a `Custom` arm smuggles untyped sources past exhaustive
//!    matches; `#[non_exhaustive]` + named variants already cover the future.
//! 2. **`RetrievalMethod` is CLOSED** (architecture.md:175) — a fixed set so
//!    precedence / badge `match`es stay exhaustive. `source` (where it is
//!    *labelled from*) and `retrieval_method` (how it was *found*) are TWO
//!    different axes; do not conflate them.
//! 3. **`AssembleDiagnostics` is a STRUCT**, not a free-form string, so 11.6
//!    can extend it with `active_group_id` / `group_count` / `tokens_saved_*`
//!    without churning every call site.
//!
//! Pure domain value objects: **serde-free** (architecture.md:175 — the domain
//! layer is depended on by everyone; persist via a private DTO only if these
//! ever hit disk/wire, which they do not in 11.4).

use std::sync::Arc;

use chrono::NaiveDate;

use crate::domain::models::turn_group::GroupId;

/// Rough token estimate: 1 token ≈ 4 bytes (the established heuristic, mirrors
/// `event_loop.rs` parent-context sizing and `LongTermMemory`'s size warning).
pub fn estimate_tokens(text: &str) -> usize {
    text.len() / 4
}

/// The token budget a `ContextBundle` competes for — the slice of the context
/// window left AFTER fixed content (conversation history + project context,
/// owned elsewhere). A thin Content-tier struct, deliberately NOT the Message-
/// tier `ContextBudget` of the un-built `ContextAssemblerPort` sketch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextBudget {
    /// Max estimated tokens the assembled memory block may occupy.
    pub max_tokens: usize,
}

impl ContextBudget {
    pub fn new(max_tokens: usize) -> Self {
        Self { max_tokens }
    }
}

/// How an entry was found. **CLOSED** (architecture.md:175): the variant set is
/// fixed so precedence / badge matches stay exhaustive. Distinct from
/// [`ContextSource`], which is open-ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetrievalMethod {
    /// Pulled from a dated daily-log row (`recent`).
    DailyLog,
    /// Pulled from the curated `MEMORY.md` long-term tier (`recent`).
    MemoryMd,
    /// A semantic / vector-index hit (`search`, hybrid retrieval on).
    VectorHit,
    /// A lexical / keyword hit (`search`, keyword fallback).
    Keyword,
    /// A structural reference (e.g. the project-context pointer) — not retrieved
    /// from memory, carried for the unified `/context show` view.
    Structural,
}

impl RetrievalMethod {
    /// Lower = higher priority. Drives cross-source dedup (keep the lowest) and
    /// budget truncation (drop the highest first). Encodes AC2's order:
    /// project/structural > long-term memory > daily logs > search results.
    pub fn precedence(self) -> u8 {
        match self {
            RetrievalMethod::Structural => 0,
            RetrievalMethod::MemoryMd => 1,
            RetrievalMethod::DailyLog => 2,
            RetrievalMethod::VectorHit => 3,
            RetrievalMethod::Keyword => 4,
        }
    }
}

/// Relevance score state. **Named-state**, never `Option<f32>` defaulted via
/// `unwrap_or` — a missing score must never be silently treated as a trusted
/// one (architecture.md:175, security/trust nuance).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Relevance {
    /// A real similarity / rank score was produced.
    Scored(f32),
    /// No score is available (e.g. `recent()` rows, or 11.3b `search()` which
    /// does not surface scores yet). NOT zero, NOT a defaulted score.
    Unscored,
}

/// Where an entry is *labelled from* for attribution. **OPEN-ENDED**
/// (`#[non_exhaustive]`): 11.5 populates `Recall`, 11.6 populates `Group`; new
/// labels can be added without breaking downstream matches. The `Custom(String)`
/// catch-all is intentionally absent (Panel-Review Amendment 4).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ContextSource {
    /// A dated daily-log entry → `[daily-log: 2026-03-25]`.
    DailyLog(NaiveDate),
    /// The curated long-term `MEMORY.md` tier → `[memory]`.
    MemoryMd,
    /// A project-context file reference → `[project: CLAUDE.md]`.
    Project(String),
    /// (Story 11.5) A recall-provider block → `[recall: <provider>]`.
    Recall(String),
    /// (Story 11.6) A cold-group gist → `[group: {N}]`.
    Group(u64),
}

impl ContextSource {
    /// The attribution label prefixing this source's content on injection and in
    /// the `/context show` card.
    pub fn attribution(&self) -> String {
        match self {
            ContextSource::DailyLog(date) => format!("[daily-log: {}]", date.format("%Y-%m-%d")),
            ContextSource::MemoryMd => "[memory]".to_string(),
            ContextSource::Project(name) => format!("[project: {name}]"),
            ContextSource::Recall(provider) => format!("[recall: {provider}]"),
            ContextSource::Group(n) => format!("[group: {n}]"),
        }
    }

    /// Coarse dedup class. Entries collide in dedup ONLY within the same class:
    /// daily-log and `MEMORY.md` rows dedup together (AC4 — same fact appears
    /// once, `MEMORY.md` wins), but a 11.6 `Group` gist is a *summary* of turn
    /// material that may also appear as a daily-log row of the same date and is
    /// NOT a duplicate (forward-compat note 2). So `Group`/`Recall`/`Project`
    /// each get their own class and never collide with memory rows.
    pub fn dedup_class(&self) -> u8 {
        match self {
            ContextSource::DailyLog(_) | ContextSource::MemoryMd => 0,
            ContextSource::Project(_) => 1,
            ContextSource::Recall(_) => 2,
            ContextSource::Group(_) => 3,
        }
    }

    /// Whether this source is injected into the model's `context_prefix`.
    /// Project-context is owned by `PersonaPort` (it injects `CLAUDE.md` into the
    /// cacheable system prompt — prefix-cache correctness, ADR-11-1), so a
    /// `Project` reference is carried in the bundle for `/context show` + dedup
    /// but is NEVER re-injected here.
    pub fn is_injectable(&self) -> bool {
        !matches!(self, ContextSource::Project(_))
    }
}

/// One assembled context entry with full provenance — the unit of dedup,
/// prioritisation, attribution, and (later) redaction keying. Serde-free
/// domain value object. `PartialEq` only — `Relevance::Scored(f32)` blocks
/// `Eq` (Q5); intern via a content hash if hashing is ever needed.
#[derive(Debug, Clone, PartialEq)]
pub struct ProvenancedEntry {
    /// Where it is labelled from (open-ended) — drives attribution + dedup class.
    pub source: ContextSource,
    /// The entry text (cheap to clone across the bundle).
    pub content: Arc<str>,
    /// Unix seconds (local-clock derived); 0 when unknown.
    pub timestamp: i64,
    /// How it was found (closed) — drives precedence.
    pub retrieval_method: RetrievalMethod,
    /// Relevance score state (named, never defaulted).
    pub relevance: Relevance,
}

impl ProvenancedEntry {
    /// Estimated token cost of the rendered `[attribution] content` line.
    pub fn estimated_tokens(&self) -> usize {
        estimate_tokens(&self.render_line())
    }

    /// Render the injectable `[attribution] content` line.
    pub fn render_line(&self) -> String {
        format!("{} {}", self.source.attribution(), self.content)
    }
}

/// Structured assembly diagnostics (NOT a free-form string) so 11.6 can extend
/// it (`active_group_id`, `group_count`, `tokens_saved_*`). `#[non_exhaustive]`
/// so adding fields is non-breaking.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct AssembleDiagnostics {
    /// Token cost per source, in injection order (drives the `/context show` card).
    pub per_source_tokens: Vec<(ContextSource, usize)>,
    /// Total estimated injected token cost (sum of injectable entries).
    pub total_tokens: usize,
    /// Whether budget truncation dropped any entries.
    pub truncated: bool,
    /// How many entries were removed by cross-source dedup.
    pub deduped_count: usize,
    // ── Story 11.6 (WindowingAssembler / Message tier) ──────────────────
    // These are populated ONLY by `WindowingAssembler`. The Content-tier
    // `MemoryContextAdapter` (and `StaticPassthroughAssembler`) leave them at
    // their `Default` (`None` / `0` / `0.0`), so `/context show` shows group
    // info only when the windowing strategy is selected.
    /// (AC-11.6.6) The active group (the one containing the last turn).
    /// `GroupId(0)` for passthrough / empty / single-group degenerate assemblies.
    pub active_group_id: GroupId,
    /// (AC-11.6.6) Total number of groups the session's turns folded into.
    pub group_count: usize,
    /// (AC-11.6.6) `passthrough_tokens − bundle_tokens` (may be ≤ 0 when the
    /// gist overhead exceeds the turns it replaced).
    pub tokens_saved_vs_passthrough: i64,
    /// (AC-11.6.6) `tokens_saved / passthrough_tokens`, 2-dp rounded percentage.
    /// NaN-safe: `0.0` when `passthrough_tokens == 0`.
    pub tokens_saved_pct: f32,
}

/// The boundary object `ContextPort::assemble` returns. Carries the assembled,
/// deduped, prioritised entries plus structured diagnostics. Never carries
/// `Vec<Message>` — selecting *what* content is the Content tier's job; building
/// the wire payload is the existing inline assembly's job (Two-Ports guard,
/// architecture.md:1157).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ContextBundle {
    pub entries: Vec<ProvenancedEntry>,
    pub diagnostics: AssembleDiagnostics,
}

impl ContextBundle {
    /// An empty bundle (base profile / disabled injection / failed assemble).
    pub fn empty() -> Self {
        Self::default()
    }

    /// Whether there are no entries at all.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Materialise the injectable entries into a labelled `context_prefix` block:
    /// one `[attribution] content` line per entry, in bundle order. Project
    /// references are skipped (persona owns `CLAUDE.md` injection). Returns
    /// `None` when nothing is injectable — the caller distinguishes this
    /// *legitimately empty* case from a *failed* assemble (which logs + counts).
    ///
    /// `to_prefix(&self)` BORROWS (Panel-Review Amendment 2): the status bar and
    /// NFR58 telemetry read the bundle after injection, so it must not consume.
    pub fn to_prefix(&self) -> Option<String> {
        let lines: Vec<String> = self
            .entries
            .iter()
            .filter(|e| e.source.is_injectable())
            .map(|e| e.render_line())
            .collect();
        if lines.is_empty() {
            None
        } else {
            Some(lines.join("\n"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn entry(source: ContextSource, content: &str, method: RetrievalMethod) -> ProvenancedEntry {
        ProvenancedEntry {
            source,
            content: Arc::from(content),
            timestamp: 0,
            retrieval_method: method,
            relevance: Relevance::Unscored,
        }
    }

    #[test]
    fn attribution_strings_are_exact() {
        assert_eq!(
            ContextSource::DailyLog(date(2026, 3, 25)).attribution(),
            "[daily-log: 2026-03-25]"
        );
        assert_eq!(ContextSource::MemoryMd.attribution(), "[memory]");
        assert_eq!(
            ContextSource::Project("CLAUDE.md".into()).attribution(),
            "[project: CLAUDE.md]"
        );
        assert_eq!(
            ContextSource::Recall("honcho".into()).attribution(),
            "[recall: honcho]"
        );
        assert_eq!(ContextSource::Group(7).attribution(), "[group: 7]");
    }

    #[test]
    fn precedence_orders_memory_over_daily_over_search() {
        assert!(RetrievalMethod::Structural.precedence() < RetrievalMethod::MemoryMd.precedence());
        assert!(RetrievalMethod::MemoryMd.precedence() < RetrievalMethod::DailyLog.precedence());
        assert!(RetrievalMethod::DailyLog.precedence() < RetrievalMethod::VectorHit.precedence());
        assert!(RetrievalMethod::VectorHit.precedence() < RetrievalMethod::Keyword.precedence());
    }

    #[test]
    fn dedup_class_groups_memory_rows_but_isolates_group_gists() {
        // AC4: daily-log + MEMORY.md rows dedup together.
        assert_eq!(
            ContextSource::DailyLog(date(2026, 3, 25)).dedup_class(),
            ContextSource::MemoryMd.dedup_class()
        );
        // Forward note 2: a 11.6 group gist never collides with a memory row.
        assert_ne!(
            ContextSource::Group(1).dedup_class(),
            ContextSource::MemoryMd.dedup_class()
        );
    }

    #[test]
    fn project_is_carried_but_not_injectable() {
        assert!(!ContextSource::Project("CLAUDE.md".into()).is_injectable());
        assert!(ContextSource::MemoryMd.is_injectable());
        assert!(ContextSource::DailyLog(date(2026, 3, 25)).is_injectable());
    }

    #[test]
    fn to_prefix_renders_injectable_lines_and_skips_project() {
        let bundle = ContextBundle {
            entries: vec![
                entry(
                    ContextSource::Project("CLAUDE.md".into()),
                    "project body",
                    RetrievalMethod::Structural,
                ),
                entry(
                    ContextSource::MemoryMd,
                    "prefers snake_case",
                    RetrievalMethod::MemoryMd,
                ),
                entry(
                    ContextSource::DailyLog(date(2026, 3, 25)),
                    "touched the parser",
                    RetrievalMethod::DailyLog,
                ),
            ],
            diagnostics: AssembleDiagnostics::default(),
        };
        let prefix = bundle.to_prefix().expect("has injectable entries");
        assert_eq!(
            prefix,
            "[memory] prefers snake_case\n[daily-log: 2026-03-25] touched the parser"
        );
        // Project body is NOT injected (persona owns CLAUDE.md).
        assert!(!prefix.contains("project body"));
    }

    #[test]
    fn to_prefix_is_none_when_only_project_or_empty() {
        let empty = ContextBundle::empty();
        assert!(empty.to_prefix().is_none());
        assert!(empty.is_empty());

        let project_only = ContextBundle {
            entries: vec![entry(
                ContextSource::Project("CLAUDE.md".into()),
                "body",
                RetrievalMethod::Structural,
            )],
            diagnostics: AssembleDiagnostics::default(),
        };
        assert!(
            project_only.to_prefix().is_none(),
            "a project-only bundle injects nothing"
        );
        assert!(
            !project_only.is_empty(),
            "but it still carries the reference"
        );
    }

    #[test]
    fn estimate_tokens_is_four_chars_per_token() {
        assert_eq!(estimate_tokens("12345678"), 2);
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn to_prefix_borrows_and_leaves_bundle_readable() {
        let bundle = ContextBundle {
            entries: vec![entry(
                ContextSource::MemoryMd,
                "fact",
                RetrievalMethod::MemoryMd,
            )],
            diagnostics: AssembleDiagnostics::default(),
        };
        let _ = bundle.to_prefix();
        // Still usable after to_prefix() (borrowing, not consuming — Amendment 2).
        assert_eq!(bundle.entries.len(), 1);
    }
}
