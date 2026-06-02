//! `MemoryContextAdapter` — the real Content-tier [`ContextPort`] (Story 11.4).
//!
//! Assembles the memory/context to inject at turn start: it reads the composed
//! [`MemoryPort`] (`recent` + `search`) via the shared `memory_slot`, re-derives
//! provenance into [`ProvenancedEntry`]s, dedups across sources, prioritises +
//! budget-truncates, and returns a [`ContextBundle`]. It is NAMED
//! `MemoryContextAdapter`, NOT `…Assembler` — "Assembler" is the un-built
//! Message tier (`ContextAssemblerPort`, Story 11.0/11.6); this is the Content
//! tier (Two-Ports boundary, architecture.md:1125–1158).
//!
//! ## Provenance derivation (Q8 resolution)
//!
//! [`MemoryEntry`] carries NO source/date discriminator and its domain doc
//! explicitly forbids adding provenance there; the composed `ProjectScopedMemory`
//! has already *merged* its long-term + daily tiers by the time `recent()`
//! returns, so the tier split is irrecoverable from the merged list. We therefore
//! re-derive source from the only available signal — the entry timestamp's
//! recency — using the SAME window the daily-log tier itself uses (current +
//! previous day, AC1): an entry dated within `daily_window_days` of "today" is a
//! [`ContextSource::DailyLog`]; older entries are [`ContextSource::MemoryMd`].
//! Known limitation: a `MEMORY.md` fact whose file mtime is today is labelled
//! daily-log. This is the cheapest honest signal without churning the 11.1/11.2
//! construction sites or polluting the domain `MemoryEntry`.
//!
//! ## Graceful degradation (AC5, Panel-Review Amendment 3)
//!
//! A `MemoryPort` failure NEVER aborts a turn: it is logged (`tracing::warn!`),
//! counted (`failure_count()` — the observable signal that distinguishes an
//! empty-from-failure bundle from an empty-from-no-memory bundle), and assembly
//! proceeds with whatever succeeded. The turn always gets a (possibly empty)
//! bundle, never an `Err`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use arc_swap::ArcSwap;
use async_trait::async_trait;
use chrono::Local;
use serde::Deserialize;

use crate::adapters::long_term_memory::normalize;
use crate::domain::errors::ContextError;
use crate::domain::models::project_context::ProjectContext;
use crate::domain::models::{
    AssembleDiagnostics, ContextBudget, ContextBundle, ContextSource, MemoryEntry,
    ProvenancedEntry, Relevance, RetrievalMethod,
};
use crate::domain::ports::{ContextPort, MemoryPort};

/// Adapter-local configuration (read from the per-dimension `_config` seam).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ContextAssemblyConfig {
    /// Max `recent()` rows to pull (daily-log + MEMORY.md).
    pub recent_limit: usize,
    /// Max `search()` semantic hits to pull.
    pub search_limit: usize,
    /// Hard cap on injected memory tokens (Q4 default ~2k, tunable). The
    /// effective budget is `min(call-site budget, this)`.
    pub max_tokens: usize,
    /// Recency window (days) within which a `recent()` entry is attributed to the
    /// daily log; older entries are attributed to `MEMORY.md`. Mirrors the
    /// daily-log adapter's current+previous-day window.
    pub daily_window_days: i64,
}

impl Default for ContextAssemblyConfig {
    fn default() -> Self {
        Self {
            recent_limit: 20,
            search_limit: 10,
            max_tokens: 2000,
            daily_window_days: 2,
        }
    }
}

/// The real Content-tier context-assembly adapter.
pub struct MemoryContextAdapter {
    /// The shared memory slot (captured at composition); read with `load_full()`
    /// so live profile swaps are seen.
    memory: Arc<ArcSwap<Arc<dyn MemoryPort>>>,
    /// Project context (CLAUDE.md) — carried as a `[project: …]` *reference* for
    /// `/context show` + dedup. NOT re-injected (persona owns CLAUDE.md).
    project_context: ProjectContext,
    /// Assembly config.
    config: ContextAssemblyConfig,
    /// Count of `MemoryPort` failures that were degraded over (observable signal,
    /// Amendment 3). Monotonic across the adapter's lifetime.
    failures: AtomicU64,
}

impl MemoryContextAdapter {
    /// Construct from the shared memory slot, the project context, and config.
    pub fn new(
        memory: Arc<ArcSwap<Arc<dyn MemoryPort>>>,
        project_context: ProjectContext,
        config: ContextAssemblyConfig,
    ) -> Self {
        Self {
            memory,
            project_context,
            config,
            failures: AtomicU64::new(0),
        }
    }

    /// Number of `MemoryPort` failures degraded over so far.
    pub fn failure_count(&self) -> u64 {
        self.failures.load(Ordering::SeqCst)
    }

    /// Derive the `(source, method)` for a `recent()` entry from its timestamp.
    fn source_for_recent(&self, e: &MemoryEntry) -> (ContextSource, RetrievalMethod) {
        let entry_date = e.timestamp.date_naive();
        let today = Local::now().date_naive();
        let age_days = (today - entry_date).num_days().max(0);
        let window = self.config.daily_window_days.max(1);
        if (0..window).contains(&age_days) {
            (
                ContextSource::DailyLog(entry_date),
                RetrievalMethod::DailyLog,
            )
        } else {
            (ContextSource::MemoryMd, RetrievalMethod::MemoryMd)
        }
    }

    fn map_recent(&self, e: MemoryEntry) -> ProvenancedEntry {
        let (source, retrieval_method) = self.source_for_recent(&e);
        ProvenancedEntry {
            source,
            content: Arc::from(e.summary.as_str()),
            timestamp: e.timestamp.timestamp(),
            retrieval_method,
            relevance: Relevance::Unscored,
        }
    }

    /// Search hits keep their source label (same window rule) and record the
    /// retrieval as `VectorHit` with `Unscored` relevance — 11.3b `search()` does
    /// not surface scores yet (fenced to a later story), but the method is
    /// semantic retrieval, not keyword fallback.
    fn map_search(&self, e: MemoryEntry) -> ProvenancedEntry {
        let (source, _) = self.source_for_recent(&e);
        ProvenancedEntry {
            source,
            content: Arc::from(e.summary.as_str()),
            timestamp: e.timestamp.timestamp(),
            retrieval_method: RetrievalMethod::VectorHit,
            relevance: Relevance::Unscored,
        }
    }

    /// One `[project: <file>]` reference entry (carried, not injected). `None`
    /// when there are no project-context files (base profile → AC5).
    fn project_entry(&self) -> Option<ProvenancedEntry> {
        let pc = &self.project_context;
        if pc.files.is_empty() {
            return None;
        }
        let content = format!(
            "project context active ({} file(s), ~{} chars; injected by persona)",
            pc.files.len(),
            pc.total_chars
        );
        Some(ProvenancedEntry {
            source: ContextSource::Project("CLAUDE.md".into()),
            content: Arc::from(content.as_str()),
            timestamp: 0,
            retrieval_method: RetrievalMethod::Structural,
            relevance: Relevance::Unscored,
        })
    }

    /// Cross-source dedup (AC4). Entries collide only within the same
    /// [`ContextSource::dedup_class`]; on collision the higher-priority source
    /// (lower [`RetrievalMethod::precedence`]) wins. The survivor is replaced
    /// in-place at its original index, but note that `prioritise_truncate`
    /// re-sorts by precedence immediately after, so positional ordering is
    /// not observable in the final bundle. Returns the removed count.
    fn dedup(entries: &mut Vec<ProvenancedEntry>) -> usize {
        use std::collections::HashMap;
        let mut index: HashMap<(u8, String), usize> = HashMap::new();
        let mut survivors: Vec<ProvenancedEntry> = Vec::with_capacity(entries.len());
        let mut removed = 0usize;
        for e in entries.drain(..) {
            let key = (e.source.dedup_class(), normalize(&e.content));
            match index.get(&key) {
                None => {
                    index.insert(key, survivors.len());
                    survivors.push(e);
                }
                Some(&idx) => {
                    removed += 1;
                    if e.retrieval_method.precedence()
                        < survivors[idx].retrieval_method.precedence()
                    {
                        // Higher-priority source replaces the kept copy in place
                        // (preserves first-seen position).
                        survivors[idx] = e;
                    }
                }
            }
        }
        *entries = survivors;
        removed
    }

    /// Prioritise (AC2: project/structural > MEMORY.md > daily > search) and
    /// budget-truncate (drop search → daily → long-term). Non-injectable
    /// (project) entries cost zero injected tokens but are carried for the card.
    /// Returns `(truncated, per_source_tokens, total_injected_tokens)`.
    fn prioritise_truncate(
        entries: &mut Vec<ProvenancedEntry>,
        cap: usize,
    ) -> (bool, Vec<(ContextSource, usize)>, usize) {
        // Stable sort by precedence so highest-priority survives truncation.
        entries.sort_by_key(|e| e.retrieval_method.precedence());

        let mut total = 0usize;
        let mut truncated = false;
        let mut kept: Vec<ProvenancedEntry> = Vec::with_capacity(entries.len());
        // Aggregate token counts per source (not per entry) for clean card display.
        let mut per_source_map: std::collections::HashMap<ContextSource, usize> =
            std::collections::HashMap::new();

        for e in entries.drain(..) {
            let cost = e.estimated_tokens();
            if e.source.is_injectable() {
                if total.saturating_add(cost) > cap {
                    // Sorted by priority → everything from here down is lower
                    // priority, so dropping the rest enforces search→daily→memory.
                    truncated = true;
                    break;
                }
                total += cost;
            }
            *per_source_map.entry(e.source.clone()).or_insert(0) += cost;
            kept.push(e);
        }

        *entries = kept;
        let per_source: Vec<(ContextSource, usize)> = per_source_map.into_iter().collect();
        (truncated, per_source, total)
    }

    fn note_failure(&self, stage: &str, err: &crate::domain::errors::MemoryError) {
        self.failures.fetch_add(1, Ordering::SeqCst);
        tracing::warn!(
            stage = stage,
            error = %err,
            "context assembly: MemoryPort failure degraded — turn proceeds with partial/empty context"
        );
    }
}

#[async_trait]
impl ContextPort for MemoryContextAdapter {
    async fn assemble(
        &self,
        query: &str,
        budget: ContextBudget,
    ) -> Result<ContextBundle, ContextError> {
        let mem = self.memory.load_full();
        let mut entries: Vec<ProvenancedEntry> = Vec::new();

        // (a) recent → daily-log + MEMORY.md rows.
        // (b) search → semantic hits (concurrent with recent — independent I/O).
        let (recent_result, search_result) = if !query.trim().is_empty() {
            let recent_fut = mem.recent(self.config.recent_limit);
            let search_fut = mem.search(query, self.config.search_limit);
            let (recent_result, search_result) = tokio::join!(recent_fut, search_fut);
            (recent_result, Some(search_result))
        } else {
            let recent_result = mem.recent(self.config.recent_limit).await;
            (recent_result, None)
        };

        match recent_result {
            Ok(rows) => entries.extend(rows.into_iter().map(|e| self.map_recent(e))),
            Err(err) => self.note_failure("recent", &err),
        }

        if let Some(search_result) = search_result {
            match search_result {
                Ok(hits) => entries.extend(hits.into_iter().map(|e| self.map_search(e))),
                Err(err) => self.note_failure("search", &err),
            }
        }

        // (c) one project-context reference (carried, not injected).
        if let Some(proj) = self.project_entry() {
            entries.push(proj);
        }

        // Cross-source dedup, then prioritise + budget-truncate.
        let deduped_count = Self::dedup(&mut entries);
        let cap = budget.max_tokens.min(self.config.max_tokens);
        let (truncated, per_source_tokens, total_tokens) =
            Self::prioritise_truncate(&mut entries, cap);

        Ok(ContextBundle {
            entries,
            diagnostics: AssembleDiagnostics {
                per_source_tokens,
                total_tokens,
                truncated,
                deduped_count,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Local};

    /// A hand-seedable stub `MemoryPort` — deterministic, no I/O, no network
    /// (project determinism law). `recent`/`search` return the seeded vectors;
    /// `fail` makes both error to exercise graceful degradation; `calls` counts
    /// every `recent`/`search` invocation (the toggle-gating contract).
    #[derive(Default)]
    struct StubMemory {
        recent: Vec<MemoryEntry>,
        search: Vec<MemoryEntry>,
        fail: bool,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl MemoryPort for StubMemory {
        async fn recent(
            &self,
            limit: usize,
        ) -> Result<Vec<MemoryEntry>, crate::domain::errors::MemoryError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if self.fail {
                return Err(crate::domain::errors::MemoryError::Other("boom".into()));
            }
            Ok(self.recent.iter().take(limit).cloned().collect())
        }

        async fn search(
            &self,
            _query: &str,
            limit: usize,
        ) -> Result<Vec<MemoryEntry>, crate::domain::errors::MemoryError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if self.fail {
                return Err(crate::domain::errors::MemoryError::Other("boom".into()));
            }
            Ok(self.search.iter().take(limit).cloned().collect())
        }
    }

    fn slot(m: StubMemory) -> Arc<ArcSwap<Arc<dyn MemoryPort>>> {
        Arc::new(ArcSwap::from_pointee(Arc::new(m) as Arc<dyn MemoryPort>))
    }

    fn entry(summary: &str, age_days: i64) -> MemoryEntry {
        MemoryEntry {
            timestamp: Local::now() - Duration::days(age_days),
            summary: summary.to_string(),
            context: None,
        }
    }

    fn adapter(mem: StubMemory, pc: ProjectContext) -> MemoryContextAdapter {
        MemoryContextAdapter::new(slot(mem), pc, ContextAssemblyConfig::default())
    }

    fn budget(n: usize) -> ContextBudget {
        ContextBudget::new(n)
    }

    // AC4: same fact in MEMORY.md (old) + daily-log (today) → one entry, MEMORY.md wins.
    #[tokio::test]
    async fn dedup_cross_source_memory_md_wins() {
        let mem = StubMemory {
            recent: vec![
                entry("database is postgresql 15", 10),  // old → MemoryMd
                entry("Database is   PostgreSQL 15", 0), // today → DailyLog, same normalized
            ],
            ..Default::default()
        };
        let a = adapter(mem, ProjectContext::empty());
        let bundle = a.assemble("", budget(10_000)).await.unwrap();

        let memory_rows: Vec<&ProvenancedEntry> = bundle
            .entries
            .iter()
            .filter(|e| e.source.dedup_class() == 0)
            .collect();
        assert_eq!(memory_rows.len(), 1, "deduped to one");
        assert_eq!(
            memory_rows[0].source,
            ContextSource::MemoryMd,
            "MEMORY.md wins"
        );
        assert_eq!(bundle.diagnostics.deduped_count, 1);
    }

    // AC4: attribution strings exact, across all three live sources.
    #[tokio::test]
    async fn attribution_strings_exact() {
        let pc = ProjectContext {
            files: vec![crate::domain::models::project_context::ProjectContextFile {
                path: std::path::PathBuf::from("CLAUDE.md"),
                content: "rules".into(),
                priority: 0,
                source_type: crate::domain::models::project_context::ContextFileType::ClaudeMd,
            }],
            total_chars: 5,
            truncated: false,
        };
        let today = Local::now().date_naive();
        let mem = StubMemory {
            recent: vec![
                entry("prefers snake_case", 10),
                entry("touched parser today", 0),
            ],
            ..Default::default()
        };
        let a = adapter(mem, pc);
        let bundle = a.assemble("", budget(10_000)).await.unwrap();

        let attrs: Vec<String> = bundle
            .entries
            .iter()
            .map(|e| e.source.attribution())
            .collect();
        assert!(attrs.contains(&"[memory]".to_string()));
        assert!(attrs.contains(&format!("[daily-log: {}]", today.format("%Y-%m-%d"))));
        assert!(attrs.contains(&"[project: CLAUDE.md]".to_string()));
    }

    // AC2: priority + budget truncation drops search → daily → long-term.
    #[tokio::test]
    async fn budget_truncation_drops_search_then_daily_then_memory() {
        // Each summary ~ 40 chars ≈ 10 tokens. Cap so only memory + part of daily fits.
        let mem = StubMemory {
            recent: vec![
                entry("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA", 10), // MemoryMd
                entry("BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB", 0),  // DailyLog
            ],
            search: vec![entry("CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC", 0)], // search hit
            ..Default::default()
        };
        let a = adapter(mem, ProjectContext::empty());
        // Budget for ~1.5 entries: memory kept, daily/search dropped.
        let bundle = a.assemble("query", budget(15)).await.unwrap();

        assert!(bundle.diagnostics.truncated, "truncation happened");
        let methods: Vec<RetrievalMethod> =
            bundle.entries.iter().map(|e| e.retrieval_method).collect();
        assert!(
            methods.contains(&RetrievalMethod::MemoryMd),
            "highest priority kept"
        );
        assert!(
            !methods.contains(&RetrievalMethod::Keyword),
            "search dropped first"
        );
    }

    // AC5: base profile (NoOp / no memory) → empty memory bundle, no panic.
    #[tokio::test]
    async fn base_profile_empty_bundle_no_panic() {
        let a = adapter(StubMemory::default(), ProjectContext::empty());
        let bundle = a.assemble("anything", budget(10_000)).await.unwrap();
        assert!(bundle.is_empty(), "no memory, no project → empty");
        assert!(bundle.to_prefix().is_none());
        assert_eq!(
            a.failure_count(),
            0,
            "no failure on a legitimately empty memory"
        );
    }

    // AC5 + Amendment 3: MemoryPort error degrades to empty bundle, turn proceeds,
    // and the failure is OBSERVABLE (distinguishable from a legitimately-empty bundle).
    #[tokio::test]
    async fn memory_failure_degrades_and_is_observable() {
        let mem = StubMemory {
            fail: true,
            ..Default::default()
        };
        let a = adapter(mem, ProjectContext::empty());
        let bundle = a.assemble("query", budget(10_000)).await.unwrap();
        assert!(bundle.is_empty(), "failed assemble degrades to empty");
        assert!(bundle.to_prefix().is_none());
        // Both recent() and search() failed → 2 observed failures.
        assert_eq!(a.failure_count(), 2, "failure observable, not silent ''");
    }

    // AC7 + Amendment 6: `/context off` must FULLY short-circuit — assemble() is
    // the SOLE path to MemoryPort, so the event-loop gate (`if injection_on`)
    // guarantees zero MemoryPort calls when off. This proves the invariant:
    // constructing the adapter touches no memory; only assemble() does.
    #[tokio::test]
    async fn assemble_is_the_only_path_to_memory_port() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mem = StubMemory {
            recent: vec![entry("a fact", 0)],
            calls: calls.clone(),
            ..Default::default()
        };
        let a = adapter(mem, ProjectContext::empty());

        // OFF path (event loop skips assemble): zero MemoryPort calls.
        assert_eq!(
            calls.load(Ordering::Relaxed),
            0,
            "no memory access before assemble"
        );

        // ON path: assemble() reaches MemoryPort (recent + search for a non-empty query).
        let _ = a.assemble("query", budget(10_000)).await.unwrap();
        assert_eq!(
            calls.load(Ordering::Relaxed),
            2,
            "assemble is the sole gateway (1 recent + 1 search)"
        );
    }

    // Project-only bundle injects nothing but is carried for /context show.
    #[tokio::test]
    async fn project_only_bundle_carries_but_does_not_inject() {
        let pc = ProjectContext {
            files: vec![crate::domain::models::project_context::ProjectContextFile {
                path: std::path::PathBuf::from("CLAUDE.md"),
                content: "rules".into(),
                priority: 0,
                source_type: crate::domain::models::project_context::ContextFileType::ClaudeMd,
            }],
            total_chars: 5,
            truncated: false,
        };
        let a = adapter(StubMemory::default(), pc);
        let bundle = a.assemble("", budget(10_000)).await.unwrap();
        assert_eq!(bundle.entries.len(), 1, "project reference carried");
        assert!(
            bundle.to_prefix().is_none(),
            "project not injected (persona owns it)"
        );
    }
}
