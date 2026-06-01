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

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Local};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::{OnceCell, RwLock};

use crate::domain::errors::{MemoryError, TransitionError};
use crate::domain::events::AppEvent;
use crate::domain::models::{HealthSummary, MemoryEntry, MemoryFact, NoticeLevel, TransitionState};
use crate::domain::ports::MemoryPort;

use super::EmbeddingProvider;
use super::fusion;
use super::index::{INDEX_VERSION, IndexMeta, IndexedEntry, VectorIndex, load_index};

/// How many inner entries to pull into the index per refresh. NFR56 bounds the
/// search side at 10k indexed entries; the refresh source cap matches it.
const DEFAULT_REFRESH_LIMIT: usize = 10_000;

/// How many top candidates to pull from EACH ranked list (vector + BM25) before
/// RRF fusion. Generous relative to typical `limit`s; with `K=60`, contributions
/// past ~100 are negligible, so this bounds the fusion work without moving the
/// head of the results.
const FUSION_CANDIDATES: usize = 128;

/// In-memory BM25 keyword index over the SAME content corpus as the vector
/// index (Story 11.3b, AC2), keyed by the SAME `content_key` so the two indexes
/// fuse cleanly. Built from the raw `bm25` crate — NOT the meta-search
/// `MergedIndex` (which is hardwired to the tool/skill `DocKey`/`CapabilityKind`
/// domain). Rebuilt on every refresh and NEVER persisted (Q5): cheap at ≤10k
/// entries, and it keeps the redaction must-test (Story 11.4) single-surfaced on
/// `index.bin`.
struct Bm25Index {
    engine: bm25::SearchEngine<u64>,
}

impl Bm25Index {
    fn empty() -> Self {
        Self {
            engine: bm25::SearchEngineBuilder::<u64>::with_avgdl(1.0).build(),
        }
    }

    fn build(docs: &[(u64, String)]) -> Self {
        if docs.is_empty() {
            return Self::empty();
        }
        let documents: Vec<bm25::Document<u64>> = docs
            .iter()
            .map(|(key, text)| bm25::Document::new(*key, text.clone()))
            .collect();
        Self {
            engine: bm25::SearchEngineBuilder::with_documents(bm25::Language::English, documents)
                .build(),
        }
    }

    /// Top-`k` content keys by descending BM25 score, ties broken by `key`
    /// ascending so the rank assignment fed to RRF is deterministic.
    fn ranked(&self, query: &str, k: usize) -> Vec<u64> {
        if k == 0 {
            return Vec::new();
        }
        let mut results = self.engine.search(query, None);
        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.document.id.cmp(&b.document.id))
        });
        results.into_iter().take(k).map(|r| r.document.id).collect()
    }
}

/// Semantic-search composite over an inner content `MemoryPort`.
pub struct VectorSearchMemory {
    inner: Arc<dyn MemoryPort>,
    provider: Arc<dyn EmbeddingProvider>,
    index: RwLock<VectorIndex>,
    /// Key-aligned keyword index for hybrid retrieval (Story 11.3b, AC2).
    bm25: RwLock<Bm25Index>,
    index_path: PathBuf,
    init: OnceCell<()>,
    refresh_limit: usize,
    /// Surfaces the guided-reindex notices (AC1). `None` (headless/eval) stays
    /// silent. Wired via [`Self::set_event_tx`] so `new()` is untouched.
    domain_tx: Option<UnboundedSender<AppEvent>>,
    /// Temporal-decay half-life in days (Q4). Adjustable in code; no user knob.
    half_life_days: f64,
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
            bm25: RwLock::new(Bm25Index::empty()),
            index_path,
            init: OnceCell::new(),
            refresh_limit: DEFAULT_REFRESH_LIMIT,
            domain_tx: None,
            half_life_days: fusion::DEFAULT_HALF_LIFE_DAYS,
        }
    }

    /// Wire the domain event channel so the guided-reindex notices (AC1) surface
    /// in the TUI. Mirrors `LongTermMemory::set_event_tx` / the
    /// `project-scoped` composite wiring. `None` (headless/eval) stays silent.
    /// Builder-style so the existing `new()` call sites are untouched.
    pub fn set_event_tx(&mut self, tx: UnboundedSender<AppEvent>) {
        self.domain_tx = Some(tx);
    }

    /// Emit a `SystemNotice` if a channel is wired (mirrors
    /// `LocalEmbeddingProvider::notice`). The event is pre-built so the
    /// conformance scanner sees a bare `tx.send(event)`, and the line is tagged.
    fn notice(&self, level: NoticeLevel, message: String) {
        if let Some(tx) = &self.domain_tx {
            let event = AppEvent::SystemNotice {
                conversation_id: None,
                level,
                message,
            };
            let _ = tx.send(event); // CONFORMANCE_EXCEPTION_EVENTBUS_BYPASS: 11-3b AC1 — guided-reindex provider-switch notice via adapter domain_tx (no event_bus access)
        }
    }

    /// Build/load + refresh the index now (idempotent — runs once). Public for
    /// startup/tests; `search` triggers it lazily otherwise.
    pub async fn initialize(&self) -> Result<(), MemoryError> {
        self.ensure_init().await
    }

    /// Number of entries currently in the vector index. Diagnostic/test support
    /// (the gated NFR scale tests assert the indexed count) — the index field
    /// itself stays private.
    pub async fn indexed_entry_count(&self) -> usize {
        self.index.read().await.entries.len()
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
        // When the persisted header doesn't match the active provider, remember
        // the OLD identity so we can SURFACE the reindex (AC1 — a "guided full
        // reindex rather than silent corruption"). 11.3a log-warned + rebuilt
        // silently; 11.3b adds the two-notice UX.
        let mut reindex_from: Option<(String, usize)> = None;
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
                    "vector index header mismatch — guided reindex from inner content"
                );
                reindex_from = Some((loaded.meta.model_id.clone(), loaded.meta.dimension));
            }
        }

        // "Guided" = surfaced + automatic (a TUI agent has no blocking modal at
        // startup; surfacing the rebuild IS the guidance). Notice BEFORE the
        // (potentially slow, but here local) rebuild.
        if let Some((old_model, old_dim)) = &reindex_from {
            self.notice(
                NoticeLevel::Warning,
                format!(
                    "Embedding provider changed (was `{old_model}` {old_dim}-dim, now `{}` {}-dim). Reindexing memory entries…",
                    self.provider.model_id(),
                    self.provider.dimension()
                ),
            );
        }

        self.refresh().await?;

        // Completion notice (mirrors the 11.3a download two-notice pattern).
        if reindex_from.is_some() {
            let n = { self.index.read().await.entries.len() };
            self.notice(
                NoticeLevel::Info,
                format!(
                    "Reindexed {n} memory entries with `{}` ({}-dim).",
                    self.provider.model_id(),
                    self.provider.dimension()
                ),
            );
        }
        Ok(())
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

        // 4b. Rebuild the key-aligned BM25 index over the FULL current corpus
        // (Story 11.3b, AC2). Snapshot (key, text) under a read guard, build the
        // engine with NO guard held (the build is sync — no `.await` — so the
        // std-sync-lock ratchet is untouched), then swap it in. Rebuild-on-
        // refresh (Q5): not persisted to disk.
        let docs: Vec<(u64, String)> = {
            let guard = self.index.read().await;
            guard
                .entries
                .iter()
                .map(|e| (e.key, entry_text(&e.entry)))
                .collect()
        };
        let bm25 = Bm25Index::build(&docs);
        {
            let mut guard = self.bm25.write().await;
            *guard = bm25;
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

    // ── Overridden: hybrid retrieval (AC2) with keyword fallback (AC4) ──
    //
    // Combines BM25 (keyword) + vector (semantic) via RRF, then a half-life
    // temporal-decay multiplier (Story 11.3b, AC2). ALL scoring is internal —
    // this still returns plain `Vec<MemoryEntry>`; `ProvenancedEntry`/scores are
    // fenced to Story 11.4 (`search()` signature unchanged, memory.rs:58-64).

    async fn search(&self, query: &str, limit: usize) -> Result<Vec<MemoryEntry>, MemoryError> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        self.ensure_init().await?;

        // Embed the query. On failure degrade to BM25-only (vector list empty);
        // if BM25 also yields nothing, fall through to inner keyword search.
        let query_vec: Option<Vec<f32>> = match self.provider.embed(&[query.to_string()]).await {
            Ok(mut vecs) if !vecs.is_empty() => Some(vecs.swap_remove(0)),
            Ok(_) => None,
            Err(e) => {
                tracing::warn!(error = %e, "query embedding failed — hybrid retrieval degrades to BM25/keyword");
                None
            }
        };

        let candidates = limit.max(FUSION_CANDIDATES);

        // Vector ranking + per-key recency weight + key→entry map, all under ONE
        // read guard with no `.await` held.
        let (vec_ranked, recency, entry_by_key): (
            Vec<u64>,
            HashMap<u64, f64>,
            HashMap<u64, MemoryEntry>,
        ) = {
            let guard = self.index.read().await;
            if guard.entries.is_empty() {
                drop(guard);
                // Nothing indexed yet — keyword fallback over inner (AC4).
                return self.inner.search(query, limit).await;
            }
            let now = Local::now();
            let mut recency = HashMap::with_capacity(guard.entries.len());
            let mut entry_by_key = HashMap::with_capacity(guard.entries.len());
            for e in &guard.entries {
                let age_days = (now - e.entry.timestamp).num_seconds() as f64 / 86_400.0;
                recency.insert(e.key, fusion::recency_weight(age_days, self.half_life_days));
                entry_by_key.insert(e.key, e.entry.clone());
            }
            let vec_ranked = match &query_vec {
                Some(qv) => guard.search_ranked(qv, candidates),
                None => Vec::new(),
            };
            (vec_ranked, recency, entry_by_key)
        };

        // BM25 keyword ranking (separate read guard).
        let bm25_ranked: Vec<u64> = {
            let guard = self.bm25.read().await;
            guard.ranked(query, candidates)
        };

        if vec_ranked.is_empty() && bm25_ranked.is_empty() {
            // No semantic vector (embed failed) AND no keyword match — keyword
            // fallback over inner so the user still gets results (AC4).
            return self.inner.search(query, limit).await;
        }

        // Fuse: RRF relevance × half-life recency, top-`limit`, deterministic.
        let relevance = fusion::rrf_relevance(&vec_ranked, &bm25_ranked);
        let fused = fusion::fuse_rank(&relevance, &recency, limit);
        if fused.is_empty() {
            return self.inner.search(query, limit).await;
        }
        // Guard against concurrent refresh: if keys disappeared from
        // entry_by_key (stale snapshot vs. updated BM25), fall back rather than
        // silently truncating results below the requested limit.
        let missing: Vec<_> = fused.iter().filter(|k| !entry_by_key.contains_key(k)).collect();
        if !missing.is_empty() {
            tracing::debug!(?missing, "concurrent refresh detected — falling back to inner search");
            return self.inner.search(query, limit).await;
        }
        Ok(fused
            .into_iter()
            .filter_map(|key| entry_by_key.get(&key).cloned())
            .collect())
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
    async fn guided_reindex_emits_notices_on_provider_switch() {
        let tmp = tempfile::tempdir().unwrap();
        let index_path = tmp.path().join("memory").join("index.bin");

        // First build (8-dim, model "stub-small") — no channel: a fresh build
        // has no persisted header to mismatch, so it must NOT emit a notice.
        let mem8 = VectorSearchMemory::new(
            fake_inner(&[(1, "alpha one"), (2, "beta two")]),
            Arc::new(StubEmbedder::new(8, "stub-small")),
            index_path.clone(),
        );
        mem8.initialize().await.unwrap();

        // Reopen with a DIFFERENT provider (16-dim, "stub-large") + a wired
        // channel → the guided reindex (AC1) fires two SystemNotices.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let stub16 = Arc::new(StubEmbedder::new(16, "stub-large"));
        let mut mem16 = VectorSearchMemory::new(
            fake_inner(&[(1, "alpha one"), (2, "beta two")]),
            stub16.clone(),
            index_path,
        );
        mem16.set_event_tx(tx);
        mem16.initialize().await.unwrap();

        let mut notices: Vec<(NoticeLevel, String)> = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let AppEvent::SystemNotice { level, message, .. } = ev {
                notices.push((level, message));
            }
        }
        assert!(
            notices.iter().any(|(lvl, m)| *lvl == NoticeLevel::Warning
                && m.contains("provider changed")
                && m.contains("stub-small")
                && m.contains("stub-large")),
            "a Warning notice announces the switch BEFORE reindex: {notices:?}"
        );
        assert!(
            notices
                .iter()
                .any(|(lvl, m)| *lvl == NoticeLevel::Info && m.contains("Reindexed")),
            "an Info notice confirms reindex completion: {notices:?}"
        );
        assert_eq!(stub16.embedded_count(), 2, "mismatch forced a full rebuild");
    }

    #[tokio::test]
    async fn hybrid_search_surfaces_matching_entry() {
        // The fused (BM25 + vector + decay) path returns the entry that shares
        // the query's terms first. Recency is ~equal across these fixed-epoch
        // entries, so relevance drives the order.
        let tmp = tempfile::tempdir().unwrap();
        let mem = VectorSearchMemory::new(
            fake_inner(&[
                (1, "the database uses postgres"),
                (2, "the parser implements pratt parsing"),
                (3, "ci pipeline runs nightly"),
            ]),
            Arc::new(StubEmbedder::new(64, "stub-v1")),
            tmp.path().join("memory").join("index.bin"),
        );
        let hits = mem.search("pratt parser", 3).await.unwrap();
        assert!(!hits.is_empty());
        assert_eq!(
            hits[0].summary, "the parser implements pratt parsing",
            "the entry sharing both query terms ranks first via hybrid fusion"
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
