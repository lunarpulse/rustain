//! Removal-integrity regression (R-001 from `test-design-epic-11.md`, Story 11.4a).
//!
//! Black-box proof over the PUBLIC `MemoryPort` + `VectorSearchMemory` surface
//! that a forgotten entry never resurfaces — across the hybrid retrieval boundary
//! (vector + BM25), a reindex from a still-dirty source, and a cold process
//! restart. The adapter's inline tests cover the same invariant white-box; this
//! pins it as a named, CI-visible regression through the public API, so a refactor
//! of the adapter internals that breaks the tombstone gate fails here too.
//!
//! Run: `cargo test --features vector-search --test regression_removal_integrity`
#![cfg(feature = "vector-search")]

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{Local, TimeZone};

use rustain::adapters::vector_search::{
    EmbeddingError, EmbeddingProvider, ProbeReport, ProviderKind, VectorSearchMemory,
};
use rustain::domain::errors::MemoryError;
use rustain::domain::models::MemoryEntry;
use rustain::domain::ports::MemoryPort;

const SECRET: &str = "the secret password is hunter2";
const NEIGHBOUR: &str = "the database uses postgres";

/// Deterministic bag-of-words embedder — no model, no network. Query sharing
/// words with an entry ranks it near; sufficient for a retrieval-boundary oracle.
struct StubEmbedder {
    dim: usize,
}

#[async_trait]
impl EmbeddingProvider for StubEmbedder {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        Ok(texts
            .iter()
            .map(|t| {
                let mut v = vec![0.0f32; self.dim];
                for word in t.split_whitespace() {
                    // FNV-1a over the lowercased word → a fixed bucket.
                    let mut h: u64 = 0xcbf29ce484222325;
                    for b in word.to_lowercase().bytes() {
                        h ^= b as u64;
                        h = h.wrapping_mul(0x100000001b3);
                    }
                    v[(h as usize) % self.dim] += 1.0;
                }
                v
            })
            .collect())
    }
    fn dimension(&self) -> usize {
        self.dim
    }
    fn model_id(&self) -> &str {
        "stub-removal-v1"
    }
    fn kind(&self) -> ProviderKind {
        ProviderKind::Local
    }
    async fn probe(&self) -> Result<ProbeReport, EmbeddingError> {
        Ok(ProbeReport {
            model_id: self.model_id().to_string(),
            dimension: self.dim,
            kind: ProviderKind::Local,
            healthy: true,
            detail: None,
        })
    }
}

/// The "dirty" content source — append-only, NEVER cleared. The redacted row
/// stays here forever, so any test step that re-reads the source is a trap for a
/// delete-only (no-tombstone) implementation.
struct DirtyInner {
    entries: Vec<MemoryEntry>,
}

#[async_trait]
impl MemoryPort for DirtyInner {
    async fn recent(&self, limit: usize) -> Result<Vec<MemoryEntry>, MemoryError> {
        Ok(self.entries.iter().take(limit).cloned().collect())
    }
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<MemoryEntry>, MemoryError> {
        let needle = query.to_lowercase();
        Ok(self
            .entries
            .iter()
            .filter(|e| e.summary.to_lowercase().contains(&needle))
            .take(limit)
            .cloned()
            .collect())
    }
}

/// FIXED timestamps so the `blake3(timestamp_ms || summary)` content key is
/// IDENTICAL across the original and the post-restart adapter — otherwise the
/// tombstone (keyed by content key) would not match on reload.
fn entry(ms: i64, summary: &str) -> MemoryEntry {
    MemoryEntry {
        timestamp: Local
            .timestamp_millis_opt(ms)
            .single()
            .expect("valid fixed timestamp"),
        summary: summary.to_string(),
        context: None,
    }
}

fn dirty_inner() -> Arc<dyn MemoryPort> {
    Arc::new(DirtyInner {
        entries: vec![entry(1_000, SECRET), entry(2_000, NEIGHBOUR)],
    }) as Arc<dyn MemoryPort>
}

fn embedder() -> Arc<dyn EmbeddingProvider> {
    Arc::new(StubEmbedder { dim: 64 })
}

/// Retrieval-boundary oracle: is `summary` reachable via hybrid (vector+BM25)
/// search? If `search` does not return it, it is redacted at the boundary.
async fn retrievable(mem: &VectorSearchMemory, query: &str, summary: &str) -> bool {
    mem.search(query, 10)
        .await
        .unwrap()
        .iter()
        .any(|e| e.summary == summary)
}

#[tokio::test]
async fn forgotten_entry_never_resurfaces_across_reindex_and_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let index_path = tmp.path().join("memory").join("index.bin");

    // ── Instance 1: seed + embed ──────────────────────────────────────────
    let mem = VectorSearchMemory::new(dirty_inner(), embedder(), index_path.clone());
    mem.initialize().await.unwrap();

    assert!(
        retrievable(&mem, "secret password hunter2", SECRET).await,
        "precondition: the secret is indexed and retrievable before redaction"
    );
    assert!(
        retrievable(&mem, "database postgres", NEIGHBOUR).await,
        "precondition: the neighbour is also indexed"
    );

    // ── Forget the secret (tombstone-first purge across vector + BM25) ─────
    let candidates = mem
        .forget_candidates("secret password hunter2", 10)
        .await
        .unwrap();
    let keys: Vec<u64> = candidates
        .into_iter()
        .filter(|(_, e)| e.summary == SECRET)
        .map(|(k, _)| k)
        .collect();
    assert!(!keys.is_empty(), "forget_candidates located the secret");
    mem.forget(&keys).await.unwrap();

    assert!(
        !retrievable(&mem, "secret password hunter2", SECRET).await,
        "AC-R1/R2: secret gone at the hybrid retrieval boundary after forget"
    );
    assert!(
        retrievable(&mem, "database postgres", NEIGHBOUR).await,
        "purge is surgical — the neighbour survives"
    );

    // ── Cold restart + reindex from the STILL-DIRTY source ────────────────
    // Dropping all in-memory state and reconstructing over the same paths
    // re-runs build_or_load: it loads the tombstone sidecar BEFORE refreshing
    // against the dirty source, so the secret must not re-embed (AC-R3/R5).
    drop(mem);

    let mem2 = VectorSearchMemory::new(dirty_inner(), embedder(), index_path);
    mem2.initialize().await.unwrap();

    assert!(
        !retrievable(&mem2, "secret password hunter2", SECRET).await,
        "AC-R3/R5 LINCHPIN: still gone after cold restart + reindex from dirty source"
    );
    assert!(
        retrievable(&mem2, "database postgres", NEIGHBOUR).await,
        "the rebuild re-indexed the non-redacted neighbour correctly"
    );
}
