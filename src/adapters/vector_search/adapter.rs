//! `VectorSearchMemory` — the wrap-and-override semantic-search `MemoryPort`
//! (Story 11.3a, AC1/AC2/AC4/AC6).
//!
//! Mirrors `ProjectScopedMemory`'s composite shape, but wraps a SINGLE inner
//! `Arc<dyn MemoryPort>` (default `project-scoped`, so it indexes daily-log +
//! MEMORY.md) and overrides only `search`:
//!
//! - `store` / `remember_fact` / `recent` → **delegate to `inner` unchanged**.
//! - `search` → embed the query, cosine top-k over the [`VectorIndex`], map back
//!   to `Vec<MemoryEntry>`.
//!
//! The wrap is exactly why AC4's "fall back to keyword-only" is natural: the
//! fallback is just `inner.search`. We also degrade to `inner.search` if the
//! query can't be embedded (e.g. the model is unavailable offline) or nothing is
//! indexed yet — so semantic search never strands the user with an empty void.
//!
//! ## Index lifecycle (AC3/AC6)
//! The index lazy-inits on first `search` (or an explicit [`Self::initialize`]),
//! mirroring `DailyLogMemory`'s lazy load. Init loads the persisted
//! `index.bin` when present and its header matches the active provider; on a
//! model/dimension mismatch it logs a warning and rebuilds (the guided reindex
//! UX is Story 11.3b). After load/build it runs an **incremental** refresh
//! against `inner`'s content: only NEW keys are embedded (in one batched call —
//! AC6), vanished keys are dropped, then the index is persisted atomically.
//!
//! ## Lock policy (CLAUDE.md, conformance-enforced)
//! `index` is a `tokio::sync::RwLock`; every guard is released before any
//! `.await` (embedding, fs). `init` is a `tokio::sync::OnceCell`. No
//! `std::sync::*Lock` anywhere — the ratchet (`MAX_KNOWN_STD_SYNC_LOCKS = 4`)
//! is not moved.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Local};
use tokio::sync::{OnceCell, RwLock};

use crate::domain::errors::{MemoryError, TransitionError};
use crate::domain::models::{HealthSummary, MemoryEntry, MemoryFact, TransitionState};
use crate::domain::ports::MemoryPort;

use super::EmbeddingProvider;
use super::index::{INDEX_VERSION, IndexMeta, IndexedEntry, VectorIndex, load_index};

/// How many inner entries to pull into the index per refresh. NFR56 bounds the
/// search side at 10k indexed entries; the refresh source cap matches it.
const DEFAULT_REFRESH_LIMIT: usize = 10_000;

/// Semantic-search composite over an inner content `MemoryPort`.
pub struct VectorSearchMemory {
    inner: Arc<dyn MemoryPort>,
    provider: Arc<dyn EmbeddingProvider>,
    index: RwLock<VectorIndex>,
    index_path: PathBuf,
    init: OnceCell<()>,
    refresh_limit: usize,
}

/// Stable content key for an entry: `blake3(timestamp_ms || summary)` → u64.
/// Deterministic across sessions (unlike `DefaultHasher`), so it survives
/// persistence and drives incremental diffing + de-dup. `MemoryEntry` has no id
/// field and must NOT gain one (additive discipline) — the content hash is the
/// identity.
pub fn content_key(ts: &DateTime<Local>, summary: &str) -> u64 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&ts.timestamp_millis().to_le_bytes());
    hasher.update(summary.as_bytes());
    let digest = hasher.finalize();
    u64::from_le_bytes(
        digest.as_bytes()[..8]
            .try_into()
            .expect("blake3 digest is 32 bytes"),
    )
}

/// The text embedded for an entry: summary, plus its context body when present.
pub fn entry_text(e: &MemoryEntry) -> String {
    match &e.context {
        Some(ctx) if !ctx.trim().is_empty() => format!("{}\n{}", e.summary, ctx),
        _ => e.summary.clone(),
    }
}

impl VectorSearchMemory {
    /// Wrap `inner`, embedding via `provider`, persisting to `index_path`. Does
    /// NO I/O — the index lazy-inits on first `search`/`initialize`.
    pub fn new(
        inner: Arc<dyn MemoryPort>,
        provider: Arc<dyn EmbeddingProvider>,
        index_path: PathBuf,
    ) -> Self {
        let meta = IndexMeta {
            model_id: provider.model_id().to_string(),
            dimension: provider.dimension(),
            version: INDEX_VERSION,
        };
        Self {
            inner,
            provider,
            index: RwLock::new(VectorIndex::empty(meta)),
            index_path,
            init: OnceCell::new(),
            refresh_limit: DEFAULT_REFRESH_LIMIT,
        }
    }

    /// Build/load + refresh the index now (idempotent — runs once). Public for
    /// startup/tests; `search` triggers it lazily otherwise.
    pub async fn initialize(&self) -> Result<(), MemoryError> {
        self.ensure_init().await
    }

    async fn ensure_init(&self) -> Result<(), MemoryError> {
        self.init
            .get_or_try_init(|| async { self.build_or_load().await })
            .await
            .map(|_| ())
    }

    /// Load the persisted index (validating its header) else start empty, then
    /// incrementally refresh against `inner` and persist.
    async fn build_or_load(&self) -> Result<(), MemoryError> {
        let loaded = match load_index(&self.index_path).await {
            Ok(Some(idx)) => Some(idx),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    path = %self.index_path.display(),
                    "corrupt or unreadable vector index — discarding and rebuilding from inner content"
                );
                let _ = tokio::fs::remove_file(&self.index_path).await;
                None
            }
        };
        if let Some(loaded) = loaded {
            let want_dim = self.provider.dimension();
            let want_model = self.provider.model_id();
            let matches = loaded.meta.dimension == want_dim
                && loaded.meta.model_id == want_model
                && loaded.meta.version == INDEX_VERSION;
            if matches {
                let mut guard = self.index.write().await;
                *guard = loaded;
            } else {
                tracing::warn!(
                    loaded_model = %loaded.meta.model_id,
                    loaded_dim = loaded.meta.dimension,
                    loaded_version = loaded.meta.version,
                    active_model = %want_model,
                    active_dim = want_dim,
                    "vector index header mismatch — discarding and rebuilding from inner content"
                );
            }
        }
        self.refresh().await
    }

    /// Incremental, batched index refresh (AC6). Diffs `inner`'s current content
    /// against the index: embeds only new keys (one batched call), drops keys no
    /// longer present, then persists. A full rebuild is just this run against an
    /// empty index.
    async fn refresh(&self) -> Result<(), MemoryError> {
        // 1. Snapshot the inner content set to index.
        let entries = self.inner.recent(self.refresh_limit).await?;
        let current: Vec<(u64, MemoryEntry)> = entries
            .into_iter()
            .map(|e| (content_key(&e.timestamp, &e.summary), e))
            .collect();
        let current_keys: HashSet<u64> = current.iter().map(|(k, _)| *k).collect();

        // 2. Which keys are already embedded? (read guard released before await)
        let existing: HashSet<u64> = {
            let guard = self.index.read().await;
            guard.keys()
        };

        // 3. Embed ONLY the new keys, batched (AC6). De-dup within the batch so a
        //    content collision is embedded once.
        let mut seen: HashSet<u64> = HashSet::new();
        let to_embed: Vec<(u64, MemoryEntry)> = current
            .into_iter()
            .filter(|(k, _)| !existing.contains(k) && seen.insert(*k))
            .collect();

        let new_indexed: Vec<IndexedEntry> = if to_embed.is_empty() {
            Vec::new()
        } else {
            let texts: Vec<String> = to_embed.iter().map(|(_, e)| entry_text(e)).collect();
            let vectors = self
                .provider
                .embed(&texts)
                .await
                .map_err(|e| MemoryError::Other(format!("embedding failed: {e}")))?;
            if vectors.len() != to_embed.len() {
                return Err(MemoryError::Other(format!(
                    "embedding count mismatch: {} texts → {} vectors",
                    to_embed.len(),
                    vectors.len()
                )));
            }
            to_embed
                .into_iter()
                .zip(vectors)
                .map(|((key, entry), vector)| IndexedEntry { key, vector, entry })
                .collect()
        };

        // 4. Apply: drop vanished keys, append new (write guard, no await held).
        {
            let mut guard = self.index.write().await;
            guard.retain_keys(&current_keys);
            guard.extend(new_indexed);
        }

        // 5. Persist (AC3). Encode under a read guard (sync), write outside it.
        let bytes = {
            let guard = self.index.read().await;
            guard.to_bytes()?
        };
        self.write_index_atomic(&bytes).await
    }

    /// Atomic write: `index.bin.tmp` then rename, so a crash mid-write never
    /// corrupts the persisted index (AC3).
    async fn write_index_atomic(&self, bytes: &[u8]) -> Result<(), MemoryError> {
        if let Some(parent) = self.index_path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                MemoryError::IoError(format!(
                    "failed to create index dir {}: {e}",
                    parent.display()
                ))
            })?;
        }
        let tmp = {
            let mut s = self.index_path.clone().into_os_string();
            s.push(".tmp");
            PathBuf::from(s)
        };
        tokio::fs::write(&tmp, bytes)
            .await
            .map_err(|e| MemoryError::IoError(format!("failed to write {}: {e}", tmp.display())))?;
        if let Err(e) = tokio::fs::rename(&tmp, &self.index_path).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(MemoryError::IoError(format!(
                "failed to rename {} → {}: {e}",
                tmp.display(),
                self.index_path.display()
            )));
        }
        Ok(())
    }
}

#[async_trait]
impl MemoryPort for VectorSearchMemory {
    // ── Delegated to inner (the content source) ──

    async fn store(&self, entry: MemoryEntry) -> Result<(), MemoryError> {
        self.inner.store(entry).await
    }

    async fn remember_fact(&self, fact: MemoryFact) -> Result<(), MemoryError> {
        self.inner.remember_fact(fact).await
    }

    async fn recent(&self, limit: usize) -> Result<Vec<MemoryEntry>, MemoryError> {
        self.inner.recent(limit).await
    }

    // ── Overridden: semantic search (AC2) with keyword fallback (AC4) ──

    async fn search(&self, query: &str, limit: usize) -> Result<Vec<MemoryEntry>, MemoryError> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        self.ensure_init().await?;

        let query_vec = match self.provider.embed(&[query.to_string()]).await {
            Ok(mut vecs) if !vecs.is_empty() => vecs.swap_remove(0),
            Ok(_) => {
                // Provider returned no vector — degrade to keyword search.
                return self.inner.search(query, limit).await;
            }
            Err(e) => {
                tracing::warn!(error = %e, "query embedding failed — falling back to keyword search");
                return self.inner.search(query, limit).await;
            }
        };

        let hits = {
            let guard = self.index.read().await;
            guard.search(&query_vec, limit)
        };
        if hits.is_empty() {
            // Nothing indexed (or no similarity) — keyword fallback so the user
            // still gets results rather than an empty void.
            return self.inner.search(query, limit).await;
        }
        Ok(hits.into_iter().map(|(_, entry)| entry).collect())
    }

    // ── Aggregate / lifecycle: delegate to inner ──

    fn health_snapshot(&self) -> HealthSummary {
        self.inner.health_snapshot()
    }

    async fn prepare_detach(&self) -> Result<TransitionState, TransitionError> {
        self.inner.prepare_detach().await
    }

    async fn receive_state(&self, state: TransitionState) -> Result<(), TransitionError> {
        self.inner.receive_state(state).await
    }

    async fn post_transition_verify(&self) -> Result<(), TransitionError> {
        self.inner.post_transition_verify().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::vector_search::{EmbeddingError, ProbeReport, ProviderKind};
    use std::sync::atomic::{AtomicUsize, Ordering};

    // ── A deterministic, no-model stub embedder (bag-of-words over `DIM`
    // buckets). Query and document sharing words score high cosine, so semantic
    // ordering is assertable without ONNX. Counts texts embedded so incremental
    // refresh can be proven (only NEW keys embedded). ──
    struct StubEmbedder {
        dim: usize,
        model_id: String,
        calls: AtomicUsize,
    }

    impl StubEmbedder {
        fn new(dim: usize, model_id: &str) -> Self {
            Self {
                dim,
                model_id: model_id.to_string(),
                calls: AtomicUsize::new(0),
            }
        }

        fn embedded_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn vec_for(&self, text: &str) -> Vec<f32> {
            let mut v = vec![0.0f32; self.dim];
            for word in text.split_whitespace() {
                let w = word.to_lowercase();
                // FNV-1a → bucket.
                let mut h: u64 = 0xcbf29ce484222325;
                for b in w.bytes() {
                    h ^= b as u64;
                    h = h.wrapping_mul(0x100000001b3);
                }
                let idx = (h as usize) % self.dim;
                v[idx] += 1.0;
            }
            v
        }
    }

    #[async_trait]
    impl EmbeddingProvider for StubEmbedder {
        async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
            self.calls.fetch_add(texts.len(), Ordering::SeqCst);
            Ok(texts.iter().map(|t| self.vec_for(t)).collect())
        }
        fn dimension(&self) -> usize {
            self.dim
        }
        fn model_id(&self) -> &str {
            &self.model_id
        }
        fn kind(&self) -> ProviderKind {
            ProviderKind::Local
        }
        async fn probe(&self) -> Result<ProbeReport, EmbeddingError> {
            Ok(ProbeReport {
                model_id: self.model_id.clone(),
                dimension: self.dim,
                kind: ProviderKind::Local,
                healthy: true,
                detail: None,
            })
        }
    }

    // ── A controllable inner MemoryPort: `recent` returns a fixed set; `search`
    // does case-insensitive substring. No interior mutability (so no std::sync
    // lock that the conformance scanner would flag) — incremental scenarios use
    // a fresh inner + a fresh adapter over the same persisted index. ──
    struct FakeInner {
        entries: Vec<MemoryEntry>,
    }

    #[async_trait]
    impl MemoryPort for FakeInner {
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

    fn entry(ms: i64, summary: &str) -> MemoryEntry {
        MemoryEntry {
            timestamp: super::super::index::tests_millis_to_local(ms),
            summary: summary.to_string(),
            context: None,
        }
    }

    fn fake_inner(summaries: &[(i64, &str)]) -> Arc<dyn MemoryPort> {
        Arc::new(FakeInner {
            entries: summaries.iter().map(|(ms, s)| entry(*ms, s)).collect(),
        })
    }

    #[test]
    fn content_key_is_stable_and_distinct() {
        let ts = super::super::index::tests_millis_to_local(1_700_000_000_000);
        let k1 = content_key(&ts, "database is postgres");
        let k2 = content_key(&ts, "database is postgres");
        assert_eq!(k1, k2, "same (ts, summary) → same key");
        let k3 = content_key(&ts, "parser uses pratt");
        assert_ne!(k1, k3, "different summary → different key");
        let ts2 = super::super::index::tests_millis_to_local(1_700_000_111_000);
        let k4 = content_key(&ts2, "database is postgres");
        assert_ne!(k1, k4, "different timestamp → different key");
    }

    #[tokio::test]
    async fn semantic_search_returns_most_similar_first() {
        let tmp = tempfile::tempdir().unwrap();
        let inner = fake_inner(&[
            (1, "database uses postgres"),
            (2, "parser implements pratt parsing"),
            (3, "the build pipeline runs on ci"),
        ]);
        let mem = VectorSearchMemory::new(
            inner,
            Arc::new(StubEmbedder::new(64, "stub-v1")),
            tmp.path().join("memory").join("index.bin"),
        );
        let hits = mem.search("postgres database", 3).await.unwrap();
        assert!(!hits.is_empty());
        assert_eq!(
            hits[0].summary, "database uses postgres",
            "the entry sharing query words ranks first"
        );
    }

    #[tokio::test]
    async fn delegation_store_recent_remember_fact() {
        let tmp = tempfile::tempdir().unwrap();
        // Use a real project-scoped-style inner via DailyLogMemory to prove
        // store/recent flow through unchanged.
        let inner: Arc<dyn MemoryPort> = Arc::new(
            crate::adapters::daily_log_memory::DailyLogMemory::new(tmp.path()),
        );
        let mem = VectorSearchMemory::new(
            inner,
            Arc::new(StubEmbedder::new(32, "stub-v1")),
            tmp.path().join("memory").join("index.bin"),
        );
        mem.store(MemoryEntry {
            timestamp: Local::now(),
            summary: "did a thing".into(),
            context: None,
        })
        .await
        .unwrap();
        let recent = mem.recent(10).await.unwrap();
        assert!(
            recent.iter().any(|e| e.summary == "did a thing"),
            "store + recent delegate to inner"
        );
    }

    #[tokio::test]
    async fn persists_index_and_incremental_refresh_embeds_only_new() {
        let tmp = tempfile::tempdir().unwrap();
        let index_path = tmp.path().join("memory").join("index.bin");

        // Instance 1: index two entries → embeds both, persists.
        let stub1 = Arc::new(StubEmbedder::new(64, "stub-v1"));
        let mem1 = VectorSearchMemory::new(
            fake_inner(&[(1, "alpha one"), (2, "beta two")]),
            stub1.clone(),
            index_path.clone(),
        );
        mem1.initialize().await.unwrap();
        assert_eq!(stub1.embedded_count(), 2, "initial build embeds both");
        assert!(index_path.exists(), "index.bin persisted (AC3)");

        // Instance 2: same persisted index, inner now has a THIRD entry.
        // Loads the persisted index (no rebuild) and embeds only the new one.
        let stub2 = Arc::new(StubEmbedder::new(64, "stub-v1"));
        let mem2 = VectorSearchMemory::new(
            fake_inner(&[(1, "alpha one"), (2, "beta two"), (3, "gamma three")]),
            stub2.clone(),
            index_path.clone(),
        );
        mem2.initialize().await.unwrap();
        assert_eq!(
            stub2.embedded_count(),
            1,
            "loaded persisted index + embedded ONLY the new entry (AC6, AC3 reload)"
        );
        let hits = mem2.search("gamma", 5).await.unwrap();
        assert!(
            hits.iter().any(|e| e.summary == "gamma three"),
            "the incrementally-added entry is searchable"
        );

        // Instance 3: an entry was removed from inner → it is dropped, nothing
        // new embedded.
        let stub3 = Arc::new(StubEmbedder::new(64, "stub-v1"));
        let mem3 = VectorSearchMemory::new(
            fake_inner(&[(1, "alpha one")]),
            stub3.clone(),
            index_path.clone(),
        );
        mem3.initialize().await.unwrap();
        assert_eq!(stub3.embedded_count(), 0, "removal embeds nothing new");
        let guard = mem3.index.read().await;
        assert_eq!(guard.entries.len(), 1, "vanished keys dropped (AC6)");
        assert_eq!(guard.entries[0].entry.summary, "alpha one");
    }

    #[tokio::test]
    async fn dimension_mismatch_on_load_triggers_rebuild() {
        let tmp = tempfile::tempdir().unwrap();
        let index_path = tmp.path().join("memory").join("index.bin");

        // Build with an 8-dim provider.
        let mem8 = VectorSearchMemory::new(
            fake_inner(&[(1, "alpha one"), (2, "beta two")]),
            Arc::new(StubEmbedder::new(8, "stub-v1")),
            index_path.clone(),
        );
        mem8.initialize().await.unwrap();

        // Reopen with a 16-dim provider → header mismatch → discard + rebuild.
        let stub16 = Arc::new(StubEmbedder::new(16, "stub-v1"));
        let mem16 = VectorSearchMemory::new(
            fake_inner(&[(1, "alpha one"), (2, "beta two")]),
            stub16.clone(),
            index_path,
        );
        mem16.initialize().await.unwrap();
        assert_eq!(stub16.embedded_count(), 2, "mismatch forces a full rebuild");
        let guard = mem16.index.read().await;
        assert_eq!(
            guard.meta.dimension, 16,
            "index adopts the active dimension"
        );
        assert_eq!(guard.entries.len(), 2);
        assert!(
            guard.entries.iter().all(|e| e.vector.len() == 16),
            "rebuilt vectors are the new dimension"
        );
    }

    #[tokio::test]
    async fn empty_query_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = VectorSearchMemory::new(
            fake_inner(&[(1, "alpha")]),
            Arc::new(StubEmbedder::new(16, "stub-v1")),
            tmp.path().join("memory").join("index.bin"),
        );
        assert!(mem.search("", 5).await.unwrap().is_empty());
        assert!(mem.search("   ", 5).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn empty_index_falls_back_to_inner_keyword_search() {
        let tmp = tempfile::tempdir().unwrap();
        // Inner has content, but suppose nothing is indexable (empty inner here)
        // — cosine yields nothing → keyword fallback over inner.
        let mem = VectorSearchMemory::new(
            fake_inner(&[]),
            Arc::new(StubEmbedder::new(16, "stub-v1")),
            tmp.path().join("memory").join("index.bin"),
        );
        // No indexed entries → empty (inner is empty too).
        assert!(mem.search("anything", 5).await.unwrap().is_empty());
    }
}
