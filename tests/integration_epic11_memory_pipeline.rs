//! Epic 11 cross-story golden path (R-003 from `test-design-epic-11.md`).
//!
//! Per-story coverage is strong, but NO single test wires the memory CONTENT
//! pipeline together through the real adapters, so a change to a shared type
//! (`MemoryEntry`, `ProvenancedEntry`, `ContextBundle`) can break the assembled
//! output while every isolated unit test stays green. This pins the seam:
//!
//!   store (11.1) + remember_fact (11.2)
//!     → ProjectScopedMemory composite merge/dedup (11.2)
//!     → MemoryContextAdapter content-tier assemble (11.4)
//!     → injectable prefix with provenance.
//!
//! The Message-tier "window" leg (11.6) is exercised by the dedicated windowing
//! tests (`windowing_assembler.rs`, `windowing_causal_chain.rs`, incl. the
//! topic-return test) and is intentionally not re-coupled here — wiring the
//! content prefix through `build_api_messages` would assert plumbing the
//! event-loop owns, not the pipeline contract this test guards.

use std::sync::Arc;

use arc_swap::ArcSwap;
use chrono::Local;

use rustain::adapters::memory_context::{ContextAssemblyConfig, MemoryContextAdapter};
use rustain::adapters::project_scoped_memory::ProjectScopedMemory;
use rustain::domain::models::project_context::ProjectContext;
use rustain::domain::models::{ContextBudget, MemoryEntry, MemoryFact};
use rustain::domain::ports::{ContextPort, MemoryPort};

/// Wrap a concrete memory adapter in the hot-swappable DI slot the
/// `MemoryContextAdapter` consumes (`Arc<ArcSwap<Arc<dyn MemoryPort>>>` — the
/// composition root's pattern for swapping the active memory provider at
/// runtime without rebuilding dependents).
fn slot(mem: ProjectScopedMemory) -> Arc<ArcSwap<Arc<dyn MemoryPort>>> {
    Arc::new(ArcSwap::from_pointee(Arc::new(mem) as Arc<dyn MemoryPort>))
}

/// 11.1 + 11.2 → 11.4: a durable fact remembered this session surfaces in the
/// content-tier injectable prefix, with provenance, through the real composite.
#[tokio::test]
async fn golden_path_remembered_fact_flows_into_assembled_context() {
    let tmp = tempfile::tempdir().unwrap();
    let mem = ProjectScopedMemory::new(tmp.path());

    // 11.2 — curate a durable fact (→ MEMORY.md).
    mem.remember_fact(MemoryFact {
        category: "Architecture".into(),
        fact: "rustain uses hexagonal ports and adapters".into(),
        detail: Some("domain stays pure; I/O at the edges".into()),
    })
    .await
    .unwrap();

    // 11.1 — append an operational entry (→ daily log).
    mem.store(MemoryEntry {
        timestamp: Local::now(),
        summary: "refactored the windowing assembler".into(),
        context: None,
    })
    .await
    .unwrap();

    // 11.2 composite recall merges both tiers.
    let recent = mem.recent(50).await.unwrap();
    let joined: String = recent
        .iter()
        .map(|e| e.summary.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("hexagonal"),
        "remembered fact recalled via composite; got:\n{joined}"
    );
    assert!(
        joined.contains("windowing assembler"),
        "stored entry recalled via composite; got:\n{joined}"
    );

    // 11.4 — content-tier assembly over the REAL composite.
    let assembler = MemoryContextAdapter::new(
        slot(mem),
        ProjectContext::empty(),
        ContextAssemblyConfig::default(),
    );
    let bundle = assembler
        .assemble("hexagonal architecture", ContextBudget::new(4000))
        .await
        .unwrap();

    let prefix = bundle
        .to_prefix()
        .expect("non-empty memory → an injectable prefix");
    assert!(
        prefix.contains("hexagonal"),
        "assembled prefix carries the remembered fact:\n{prefix}"
    );
    // Provenance: every assembled entry carries a non-empty source attribution.
    // Guard non-emptiness locally so the `.all(...)` below can never pass
    // vacuously on an empty bundle (the `to_prefix().expect()` above already
    // implies it, but make the precondition explicit and adjacent).
    assert!(
        !bundle.entries.is_empty(),
        "assembled bundle must contain entries before the provenance check"
    );
    assert!(
        bundle
            .entries
            .iter()
            .all(|e| !e.source.attribution().is_empty()),
        "every assembled entry carries provenance"
    );
    assert_eq!(
        assembler.failure_count(),
        0,
        "no MemoryPort failures on the happy path"
    );
}

/// 11.2 → 11.4 dedup seam: identical content in BOTH tiers (daily + long-term)
/// collapses to a single assembled entry before injection (no double-billing).
#[tokio::test]
async fn golden_path_composite_dedups_across_tiers_before_assembly() {
    const DUP: &str = "shared dedup probe alpha";
    let tmp = tempfile::tempdir().unwrap();
    let mem = ProjectScopedMemory::new(tmp.path());

    mem.remember_fact(MemoryFact {
        category: "Probe".into(),
        fact: DUP.into(),
        detail: None,
    })
    .await
    .unwrap();
    mem.store(MemoryEntry {
        timestamp: Local::now(),
        summary: DUP.into(),
        context: None,
    })
    .await
    .unwrap();

    let assembler = MemoryContextAdapter::new(
        slot(mem),
        ProjectContext::empty(),
        ContextAssemblyConfig::default(),
    );
    // Empty query → recent-only path (the composite-deduped list).
    let bundle = assembler
        .assemble("", ContextBudget::new(4000))
        .await
        .unwrap();

    let occurrences = bundle
        .entries
        .iter()
        .filter(|e| e.content.contains(DUP))
        .count();
    assert_eq!(
        occurrences, 1,
        "same content in daily + long-term tiers must dedup to one before injection; \
         entries: {:?}",
        bundle.entries.iter().map(|e| &e.content).collect::<Vec<_>>()
    );
}
