//! `ProjectScopedMemory` — the `project-scoped` composite `MemoryPort` adapter
//! (Story 11.2). Merges the two real tiers behind one port:
//!
//! - **daily-log** ([`DailyLogMemory`], 11.1) — append-only operational records
//!   (`store` / `recent` / `search` over `memory/YYYY-MM-DD.md`).
//! - **long-term** ([`LongTermMemory`], this story) — curated durable facts
//!   (`remember_fact` over `MEMORY.md`).
//!
//! Routing (AC6):
//! - `store(entry)`        → daily-log  (notable operational records)
//! - `remember_fact(fact)` → long-term  (durable facts → MEMORY.md)
//! - `recent` / `search`   → **long-term first, then daily, deduped, capped**
//!
//! Dedup is by normalized `summary` ([`normalize`]); when the same information
//! is present in both tiers, the long-term copy wins (appears once, first).
//!
//! This is the only place besides the composition root that names both concrete
//! children together (CLAUDE.md hexagonal rule). Modeled on the composite-adapter
//! precedent in `composite_toolset_adapter.rs` (owns children, delegates trait
//! methods).
//!
//! Scope guard (architecture.md:1125–1158): the composite's merge/dedup is a
//! WITHIN-memory convenience (long-term vs daily), NOT the cross-source
//! `ContextPort` dedup (Story 11.4). It stays ignorant of injection / ranking /
//! attribution.

use std::collections::HashSet;
use std::path::Path;

use async_trait::async_trait;

use crate::adapters::daily_log_memory::DailyLogMemory;
use crate::adapters::long_term_memory::LongTermMemory;
use crate::domain::errors::{MemoryError, TransitionError};
use crate::domain::events::AppEvent;
use crate::domain::models::{HealthLevel, HealthSummary, MemoryEntry, MemoryFact, TransitionState};
use crate::domain::ports::MemoryPort;
use crate::domain::services::normalize::normalize;

/// Composite of the daily-log and long-term memory tiers.
pub struct ProjectScopedMemory {
    daily: DailyLogMemory,
    long_term: LongTermMemory,
}

impl ProjectScopedMemory {
    /// Construct the composite, building both children rooted at `workspace_path`.
    /// Does NO I/O (each child constructs lazily).
    pub fn new(workspace_path: &Path) -> Self {
        Self {
            daily: DailyLogMemory::new(workspace_path),
            long_term: LongTermMemory::new(workspace_path),
        }
    }

    /// Forward the event bus to the long-term child (the size warning surface).
    pub fn set_event_tx(&mut self, event_tx: tokio::sync::mpsc::UnboundedSender<AppEvent>) {
        self.long_term.set_event_tx(event_tx);
    }

    /// Merge two tiers long-term-first, dropping any later entry whose normalized
    /// summary already appeared (long-term wins), capped at `limit`.
    fn merge_dedup(
        long_term: Vec<MemoryEntry>,
        daily: Vec<MemoryEntry>,
        limit: usize,
    ) -> Vec<MemoryEntry> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut out: Vec<MemoryEntry> =
            Vec::with_capacity(limit.min(long_term.len() + daily.len()));
        for entry in long_term.into_iter().chain(daily.into_iter()) {
            if out.len() >= limit {
                break;
            }
            if seen.insert(normalize(&entry.summary)) {
                out.push(entry);
            }
        }
        out
    }

    fn health_rank(level: HealthLevel) -> u8 {
        match level {
            HealthLevel::Error => 3,
            HealthLevel::Degraded => 2,
            HealthLevel::Unknown => 1,
            HealthLevel::Healthy => 0,
        }
    }
}

#[async_trait]
impl MemoryPort for ProjectScopedMemory {
    /// Operational records go to the daily log.
    async fn store(&self, entry: MemoryEntry) -> Result<(), MemoryError> {
        self.daily.store(entry).await
    }

    /// Durable facts go to long-term `MEMORY.md`.
    async fn remember_fact(&self, fact: MemoryFact) -> Result<(), MemoryError> {
        self.long_term.remember_fact(fact).await
    }

    /// Long-term facts first, then daily entries, deduped, capped at `limit` (AC6).
    async fn recent(&self, limit: usize) -> Result<Vec<MemoryEntry>, MemoryError> {
        let lt = self.long_term.recent(limit).await?;
        let dl = self.daily.recent(limit).await?;
        Ok(Self::merge_dedup(lt, dl, limit))
    }

    /// Same merge/dedup shape over both tiers' `search`.
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<MemoryEntry>, MemoryError> {
        let lt = self.long_term.search(query, limit).await?;
        let dl = self.daily.search(query, limit).await?;
        Ok(Self::merge_dedup(lt, dl, limit))
    }

    /// Story 12.1c AC3 — hand-deletions are a long-term (`MEMORY.md`) concern;
    /// delegate detection to that tier. The daily log is append-only and never the
    /// source of a hand-deletion.
    async fn drain_md_removals(&self) -> Result<Vec<MemoryEntry>, MemoryError> {
        self.long_term.drain_md_removals().await
    }

    /// Worst-of the two children (minimal aggregate).
    fn health_snapshot(&self) -> HealthSummary {
        let lt = self.long_term.health_snapshot();
        let dl = self.daily.health_snapshot();
        if Self::health_rank(lt.level) >= Self::health_rank(dl.level) {
            lt
        } else {
            dl
        }
    }

    async fn prepare_detach(&self) -> Result<TransitionState, TransitionError> {
        // Call both (propagate the first error); return a combined empty marker.
        self.long_term.prepare_detach().await?;
        self.daily.prepare_detach().await
    }

    async fn receive_state(&self, state: TransitionState) -> Result<(), TransitionError> {
        self.long_term.receive_state(state.clone()).await?;
        self.daily.receive_state(state).await
    }

    async fn post_transition_verify(&self) -> Result<(), TransitionError> {
        self.long_term.post_transition_verify().await?;
        self.daily.post_transition_verify().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Local;

    fn fact(category: &str, fact: &str) -> MemoryFact {
        MemoryFact {
            category: category.to_string(),
            fact: fact.to_string(),
            detail: None,
        }
    }

    fn entry(summary: &str) -> MemoryEntry {
        MemoryEntry {
            timestamp: Local::now(),
            summary: summary.to_string(),
            context: None,
        }
    }

    // 10. recent/search order: long-term facts FIRST, then daily (AC6).
    #[tokio::test]
    async fn recent_long_term_first_then_daily() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = ProjectScopedMemory::new(tmp.path());

        mem.store(entry("daily decision")).await.unwrap();
        mem.remember_fact(fact("Preferences", "prefers snake_case"))
            .await
            .unwrap();

        let recent = mem.recent(10).await.unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].summary, "prefers snake_case", "long-term first");
        assert_eq!(recent[1].summary, "daily decision", "daily second");

        // search has the same ordering.
        mem.store(entry("touched the parser")).await.unwrap();
        mem.remember_fact(fact("Parser", "parser uses pratt parsing"))
            .await
            .unwrap();
        let hits = mem.search("parser", 10).await.unwrap();
        assert_eq!(
            hits[0].summary, "parser uses pratt parsing",
            "long-term match first"
        );
    }

    // 11. Dedup across tiers: same info in both → appears once (long-term wins).
    #[tokio::test]
    async fn dedup_across_tiers_long_term_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = ProjectScopedMemory::new(tmp.path());

        // Same normalized summary in both tiers (different whitespace/case).
        mem.store(entry("Database is   PostgreSQL 15"))
            .await
            .unwrap();
        mem.remember_fact(fact("Database", "database is postgresql 15"))
            .await
            .unwrap();

        let recent = mem.recent(10).await.unwrap();
        let matches: Vec<&MemoryEntry> = recent
            .iter()
            .filter(|e| normalize(&e.summary) == normalize("database is postgresql 15"))
            .collect();
        assert_eq!(matches.len(), 1, "deduped to one");
        assert_eq!(
            matches[0].summary, "database is postgresql 15",
            "long-term copy wins"
        );
    }

    // 12. Routing: store→daily-only; remember_fact→long-term-only.
    #[tokio::test]
    async fn routing_store_daily_remember_fact_long_term() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = ProjectScopedMemory::new(tmp.path());

        mem.store(entry("only in daily")).await.unwrap();
        mem.remember_fact(fact("Topic", "only in long-term"))
            .await
            .unwrap();

        // store wrote a daily-log file, NOT MEMORY.md.
        let memory_md = tmp.path().join(".rustain").join("MEMORY.md");
        let memory_dir = tmp.path().join(".rustain").join("memory");
        assert!(memory_dir.is_dir(), "daily-log dir created by store");
        assert!(memory_md.exists(), "MEMORY.md created by remember_fact");

        // MEMORY.md contains only the long-term fact, not the daily entry.
        let md = std::fs::read_to_string(&memory_md).unwrap();
        assert!(md.contains("only in long-term"));
        assert!(
            !md.contains("only in daily"),
            "store did not leak into MEMORY.md"
        );

        // The daily-log files contain only the daily entry, not the fact.
        let day_files: String = std::fs::read_dir(&memory_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| std::fs::read_to_string(e.path()).unwrap_or_default())
            .collect();
        assert!(day_files.contains("only in daily"));
        assert!(
            !day_files.contains("only in long-term"),
            "remember_fact did not leak into daily-log"
        );
    }

    // Cap respected across the merged result.
    #[tokio::test]
    async fn merge_respects_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = ProjectScopedMemory::new(tmp.path());
        mem.remember_fact(fact("A", "fact one")).await.unwrap();
        mem.remember_fact(fact("A", "fact two")).await.unwrap();
        mem.store(entry("daily one")).await.unwrap();
        mem.store(entry("daily two")).await.unwrap();

        let recent = mem.recent(3).await.unwrap();
        assert_eq!(recent.len(), 3, "cap respected across tiers");
        // Long-term first.
        assert!(recent[0].summary == "fact one" || recent[0].summary == "fact two");
    }
}
