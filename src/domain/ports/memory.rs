#![allow(dead_code)]
//! SDK STABILITY: New methods MUST carry a default impl. Removing a method
//! or changing a signature is a MAJOR version bump. See
//! docs/adapter-composition.md § Adapter SDK Compatibility (Story 8.3 AC-6).
/// Conversation memory storage and retrieval.
///
/// Claudian equivalent: `src/core/memory/memoryManager.ts`
// 2026-05-17 — Story 8.4 added prepare_detach/receive_state/post_transition_verify (warm tier)
// following the additive-with-defaults discipline. No existing adapters needed changes.
// 2026-05-19 — Story 8.5 added health_snapshot() with default HealthSummary::unknown() impl
// following additive-with-defaults discipline. No existing adapters needed changes.
// Real metrics ship with real adapters in Epic 12.
// 2026-05-31 — Story 11.1 lit up the v1.0 store/recent/search surface as
// defaulted methods (additive-with-defaults). NoOpMemory and every existing
// adapter keep compiling untouched; DailyLogMemory is the first real override.
// 2026-05-31 — Story 11.2 added remember_fact(MemoryFact) with a default no-op,
// following the same additive-with-defaults discipline. The curated long-term
// tier (LongTermMemory → {workspace}/.rustain/MEMORY.md) and the project-scoped
// composite override it; NoOpMemory and DailyLogMemory keep compiling untouched
// (the default no-op covers them — DailyLogMemory deliberately ignores durable
// facts, which belong in MEMORY.md, not the append-only daily log).
#[async_trait::async_trait]
pub trait MemoryPort: Send + Sync {
    /// Append a notable entry to durable memory. Default is a no-op (NoOpMemory).
    async fn store(
        &self,
        _entry: crate::domain::models::MemoryEntry,
    ) -> Result<(), crate::domain::errors::MemoryError> {
        Ok(())
    }

    /// Curate a durable fact into the long-term tier (`MEMORY.md`), upserting by
    /// topic. Default is a no-op so `NoOpMemory`, `DailyLogMemory`, and every
    /// existing adapter keep compiling untouched (additive-with-defaults, Story
    /// 11.2). Only `LongTermMemory` and the `project-scoped` composite override
    /// it. AC2's "updated / removed" is satisfied by human-editing + reload plus
    /// this method's upsert semantics — there are deliberately no separate
    /// `update_fact` / `remove_fact` trait methods (Q1).
    async fn remember_fact(
        &self,
        _fact: crate::domain::models::MemoryFact,
    ) -> Result<(), crate::domain::errors::MemoryError> {
        Ok(())
    }

    /// Return up to `limit` most-recent loaded entries (newest-first).
    /// For the daily-log adapter this is the current + previous day (AC3),
    /// satisfying the epic's "recall" behavior. Default returns empty.
    async fn recent(
        &self,
        _limit: usize,
    ) -> Result<Vec<crate::domain::models::MemoryEntry>, crate::domain::errors::MemoryError> {
        Ok(Vec::new())
    }

    /// Case-insensitive keyword search over loaded entries, capped at `limit`.
    /// Default returns empty.
    async fn search(
        &self,
        _query: &str,
        _limit: usize,
    ) -> Result<Vec<crate::domain::models::MemoryEntry>, crate::domain::errors::MemoryError> {
        Ok(Vec::new())
    }

    /// Flush any pending/in-flight durable write to its backing store. Default
    /// is a no-op: the daily-log (append-on-`store` + `file.flush()`, 11.1) and
    /// long-term (`fs::write` on `remember_fact`, 11.2) tiers write synchronously,
    /// so there is no buffer to drain (Q1). It exists as an explicit NFR58
    /// ordering barrier (Story 11.4): a durable fact written this turn MUST be
    /// flushed BEFORE context compaction rebuilds the window, so the fact is
    /// never lost to compaction. A future buffered adapter overrides this; the
    /// barrier semantics (await-before-compact) hold regardless.
    async fn flush(&self) -> Result<(), crate::domain::errors::MemoryError> {
        Ok(())
    }

    fn health_snapshot(&self) -> crate::domain::models::HealthSummary {
        crate::domain::models::HealthSummary::unknown()
    }
    async fn prepare_detach(
        &self,
    ) -> Result<crate::domain::models::TransitionState, crate::domain::errors::TransitionError>
    {
        Ok(crate::domain::models::TransitionState::empty("memory"))
    }
    async fn receive_state(
        &self,
        _state: crate::domain::models::TransitionState,
    ) -> Result<(), crate::domain::errors::TransitionError> {
        Ok(())
    }
    async fn post_transition_verify(&self) -> Result<(), crate::domain::errors::TransitionError> {
        Ok(())
    }
}
