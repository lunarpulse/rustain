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
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Local};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::{Mutex, OnceCell, RwLock};

use crate::domain::errors::{MemoryError, TransitionError};
use crate::domain::events::AppEvent;
use crate::domain::models::{
    HealthSummary, MemoryEntry, MemoryFact, NoticeLevel, RedactionRecord, TransitionState,
};
use crate::domain::ports::MemoryPort;
use crate::domain::services::normalize::normalize;
use crate::domain::services::redaction_mask;
// The SAME normalization `ProjectScopedMemory::merge_dedup` dedups on — reused as
// the content-stable redaction token identity (Story 12.1c AC3), so one tombstone
// suppresses a fact across every timestamp namespace.
use super::EmbeddingProvider;
use super::fusion;
use super::index::{INDEX_VERSION, IndexMeta, IndexedEntry, VectorIndex, load_index};
use super::redaction::{RedactionStore, load_redactions, sidecar_path};

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
    /// Durable redaction tombstones (Story 11.4a / FR122). Loaded from the
    /// `redactions.bin` sidecar BEFORE the first refresh, the source of truth for
    /// removal: written FIRST on `forget`, consulted by `refresh()`/rebuild to
    /// skip redacted keys at embed time, and by `search` for read-time masking.
    redactions: RwLock<RedactionStore>,
    /// Sibling sidecar path (`…/memory/redactions.bin`). Lives OUTSIDE `index.bin`
    /// so a full index rebuild from source cannot lose the gravestone (AC-R3).
    redactions_path: PathBuf,
    init: OnceCell<()>,
    refresh_limit: usize,
    /// Drain gate for in-flight refresh/forget operations (Story 12.0 review
    /// patch). `prepare_detach()` acquires this lock so the swap cannot complete
    /// until any local `refresh()` or `forget()` currently in progress has
    /// finished writing `index.bin` / `redactions.bin`. Reuses `tokio::sync`
    // (ratchet-neutral) per CLAUDE.md Async Lock Policy.
    drain_lock: Mutex<()>,
    /// Surfaces the guided-reindex notices (AC1). `None` (headless/eval) stays
    /// silent. Wired via [`Self::set_event_tx`] so `new()` is untouched.
    domain_tx: Option<UnboundedSender<AppEvent>>,
    /// Temporal-decay half-life in days (Q4). Adjustable in code; no user knob.
    half_life_days: f64,
    /// Test-only deterministic suspension seam (Story 12.0 AC10) — parks
    /// `refresh()` between the embed await and the final tombstone gate so a
    /// concurrent `forget()` tombstone can be injected. Compiled out of release.
    #[cfg(test)]
    refresh_seam: RefreshSeam,
}

/// Test-only suspension seam pinning the C4 concurrency window between
/// `refresh()`'s `embed()` await and its final persist (Story 12.0 AC10). Lets a
/// test land a `forget()` tombstone DURING the embed and prove the refresh
/// re-consults the LIVE redaction set before writing `index.bin` (no `sleep`).
#[cfg(test)]
#[derive(Default)]
struct RefreshSeam {
    armed: std::sync::atomic::AtomicBool,
    reached: tokio::sync::Notify,
    proceed: tokio::sync::Notify,
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

/// Fuzzy-match score for `query` against `text`. Returns `Some(score)` if the
/// query is "close enough" (Levenshtein distance ≤ 2 OR the query is a
/// substring), `None` otherwise. The threshold is intentionally generous so
/// typo-tolerant matching works (e.g., "projcet" → "project").
fn fuzzy_match_score(query: &str, text: &str) -> Option<u32> {
    let q = query.to_lowercase();
    let t = text.to_lowercase();

    // Exact substring match: highest score.
    if t.contains(&q) {
        return Some(100);
    }

    // Levenshtein distance for typo tolerance.
    let dist = levenshtein_distance(&q, &t);
    // Allow up to 2 edits for short queries, up to 3 for longer ones.
    let max_dist = if q.len() <= 4 { 1 } else { 2 };
    if dist <= max_dist {
        // Score inversely proportional to distance (closer = higher).
        return Some((max_dist as u32 + 1 - dist as u32) * 25);
    }

    // Word-level substring match: each word of the query is a substring of text.
    let q_words: Vec<&str> = q.split_whitespace().collect();
    if !q_words.is_empty() && q_words.iter().all(|w| t.contains(w)) {
        return Some(50);
    }

    None
}

/// Levenshtein distance between two strings (classic DP, O(n·m)).
fn levenshtein_distance(a: &str, b: &str) -> usize {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let alen = a_chars.len();
    let blen = b_chars.len();

    if alen == 0 {
        return blen;
    }
    if blen == 0 {
        return alen;
    }

    let mut prev: Vec<usize> = (0..=blen).collect();
    let mut curr = vec![0usize; blen + 1];

    for i in 1..=alen {
        curr[0] = i;
        for j in 1..=blen {
            let cost = if a_chars[i - 1] == b_chars[j - 1] {
                0
            } else {
                1
            };
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[blen]
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
        let redactions_path = sidecar_path(&index_path);
        Self {
            inner,
            provider,
            index: RwLock::new(VectorIndex::empty(meta)),
            bm25: RwLock::new(Bm25Index::empty()),
            index_path,
            redactions: RwLock::new(RedactionStore::empty()),
            redactions_path,
            init: OnceCell::new(),
            refresh_limit: DEFAULT_REFRESH_LIMIT,
            drain_lock: Mutex::new(()),
            domain_tx: None,
            half_life_days: fusion::DEFAULT_HALF_LIFE_DAYS,
            #[cfg(test)]
            refresh_seam: RefreshSeam::default(),
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

    /// Test-only: shrink the refresh window so a `> refresh_limit` scenario can be
    /// constructed without seeding 10k entries (Story 12.0 C4 / AC4). Lets a test
    /// age an entry PAST the window — the precondition Murat's exit gate requires
    /// (a record aged only WITHIN the window would pass vacuously on broken code).
    #[cfg(test)]
    fn set_refresh_limit(&mut self, limit: usize) {
        self.refresh_limit = limit;
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
        // Load redaction tombstones FIRST (Story 11.4a) — before any refresh or
        // rebuild, so the embed-time gate is armed for the very first index build.
        // This is what makes a redaction survive a full reindex from a still-dirty
        // source (AC-R3): the sidecar outlives `index.bin`.
        {
            let store = load_redactions(&self.redactions_path).await?;
            let mut guard = self.redactions.write().await;
            *guard = store;
        }

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
        // Drain gate: hold the local mutex so `prepare_detach` cannot return until
        // any in-flight refresh/forget has finished writing index.bin (Story 12.0
        // review patch). Callers that ALREADY hold this gate (`forget` /
        // `honor_md_removals`) MUST call [`Self::refresh_inner`] directly — re-locking
        // this non-reentrant `tokio::sync::Mutex` from the same task deadlocks
        // (Story 12.1c: surfaced when the file-edit-honor path added a second
        // lock-holding caller; the fix also resolves the latent `forget`→`refresh`
        // self-deadlock under a `current_thread` runtime).
        let _drain = self.drain_lock.lock().await;
        self.refresh_inner().await
    }

    /// The lock-free incremental-refresh body. Either reached through
    /// [`Self::refresh`] (which acquires `drain_lock` first) or called directly by a
    /// caller that ALREADY holds `drain_lock` (`forget` / `honor_md_removals`), so
    /// the gate is held continuously across tombstone-then-purge without re-entry.
    async fn refresh_inner(&self) -> Result<(), MemoryError> {
        // 0. Snapshot the redaction tombstone set (read guard released before any
        //    await). THE fix for the self-heal (Story 11.4a, Task 3): the source
        //    row of a redacted entry is still present (daily-log is append-only;
        //    a MEMORY.md fact may be outside the window), so without this gate the
        //    next refresh re-embeds it ("the ghost re-embeds"). Dropping redacted
        //    keys here makes the one-time purge idempotent under refresh + rebuild.
        // Snapshot BOTH suppression identities (Story 12.1c AC3): the u64 key set
        // (11.4a) AND the content-stable token set. The token set is what kills the
        // daily-log re-derivation copy — a different timestamp namespace than the
        // MEMORY.md-mtime copy, so a key alone cannot reach it.
        let (redacted, redacted_tokens): (HashSet<u64>, HashSet<String>) = {
            let guard = self.redactions.read().await;
            (guard.keys(), guard.tokens())
        };

        // 1. Snapshot the inner content set to index, MINUS any redacted key OR any
        //    entry whose normalized text matches a content-stable redaction token.
        let entries = self.inner.recent(self.refresh_limit).await?;
        let current: Vec<(u64, MemoryEntry)> = entries
            .into_iter()
            .map(|e| (content_key(&e.timestamp, &e.summary), e))
            .filter(|(k, e)| {
                !redacted.contains(k) && !redacted_tokens.contains(&normalize(&e.summary))
            })
            .collect();
        // `current_keys` excludes redacted keys AND token-matched entries → the
        // `retain_keys` below drops any such entry already in the index (the
        // one-time purge), and `to_embed` can never re-embed one. This is the
        // single change that makes ONE tombstone suppress BOTH the MEMORY.md-mtime
        // copy and the daily-log-realts copy.
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

        // Test-only deterministic suspension seam (Story 12.0 AC10): park between
        // the embed await above and the final tombstone gate below, so a test can
        // land a `forget()` tombstone DURING the embed and prove the gate honors
        // the LIVE redaction set (not the stale start-of-refresh snapshot).
        #[cfg(test)]
        if self
            .refresh_seam
            .armed
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            self.refresh_seam.reached.notify_one();
            self.refresh_seam.proceed.notified().await;
        }

        // 4. Apply: drop vanished keys, append new (write guard, no await held).
        //
        // SINGLE HARDENED PURGE SINK (Story 12.0 C4 / AC4, AC9). Re-consult the
        // LIVE redaction set at the LAST moment before persisting: a `forget()`
        // that landed during the (long) embed await above is NOT in the
        // start-of-refresh `redacted` snapshot, so without this its key would be
        // retained/embedded and written into `index.bin` — surviving until the
        // next refresh. Re-reading `redactions` here and dropping those keys from
        // BOTH the freshly-embedded set and the retained index makes this one gate
        // the sole concurrency-controlled path every redaction funnels through,
        // regardless of producer. The 12.1c file-edit-honor path
        // (`honor_md_removals`) reaches purge through THIS seam too, and the gate
        // now also drops by the content-stable `token` (Story 12.1c AC3), so one
        // tombstone suppresses a fact across every timestamp namespace. (AC9
        // rationale — Winston.)
        {
            // Acquire the index write lock FIRST, THEN read the LIVE redaction set
            // so a concurrent `forget()` tombstone cannot land in the window between
            // snapshot and write (Story 12.0 review patch).
            let mut guard = self.index.write().await;
            let (redacted_final, tokens_final): (HashSet<u64>, HashSet<String>) = {
                let rguard = self.redactions.read().await;
                (rguard.keys(), rguard.tokens())
            };
            let new_indexed: Vec<IndexedEntry> = new_indexed
                .into_iter()
                .filter(|e| {
                    !redacted_final.contains(&e.key)
                        && !tokens_final.contains(&normalize(&e.entry.summary))
                })
                .collect();
            guard.retain_keys(&current_keys);
            guard.extend(new_indexed);
            // Final tombstone gate: drop any key OR content-stable token redacted
            // concurrently during the embed await, independent of the recency
            // window (the whole index, not just the refreshed slice).
            if !redacted_final.is_empty() || !tokens_final.is_empty() {
                guard.entries.retain(|e| {
                    !redacted_final.contains(&e.key)
                        && !tokens_final.contains(&normalize(&e.entry.summary))
                });
            }
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
        let path = self.index_path.clone();
        write_atomic(&path, bytes).await
    }

    /// Persist the redaction tombstone set to its sidecar (`redactions.bin`),
    /// atomically (tmp→rename). Called BEFORE the one-time purge on `forget`
    /// (AC-R6 — the tombstone is the source of truth and must land first).
    async fn persist_redactions(&self) -> Result<(), MemoryError> {
        let bytes = {
            let guard = self.redactions.read().await;
            guard.to_bytes()?
        };
        let path = self.redactions_path.clone();
        write_atomic(&path, &bytes).await
    }
}

/// Atomic write: `<path>.tmp.<random>` then rename, so a crash mid-write never
/// corrupts the target. The random suffix prevents concurrent writers from
/// racing on the same temp name. Shared by `index.bin` (AC3) and
/// `redactions.bin` (AC-R6).
async fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), MemoryError> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|e| {
            MemoryError::IoError(format!("failed to create dir {}: {e}", parent.display()))
        })?;
    }
    // Unique temp name: pid + monotonic counter to avoid concurrent collisions
    // and symlink-attack predictability.
    let tmp = {
        let mut s = path.to_path_buf().into_os_string();
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let pid = std::process::id();
        s.push(format!(".tmp.{pid}.{n:016x}",));
        PathBuf::from(s)
    };
    tokio::fs::write(&tmp, bytes)
        .await
        .map_err(|e| MemoryError::IoError(format!("failed to write {}: {e}", tmp.display())))?;
    // On Windows, rename fails if the target exists; remove it first.
    #[cfg(windows)]
    {
        let _ = tokio::fs::remove_file(path).await;
    }
    if let Err(e) = tokio::fs::rename(&tmp, path).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(MemoryError::IoError(format!(
            "failed to rename {} → {}: {e}",
            tmp.display(),
            path.display()
        )));
    }
    Ok(())
}

#[async_trait]
impl MemoryPort for VectorSearchMemory {
    // ── Delegated to inner (the content source) ──

    /// **In-session staleness contract (Story 12.0 C5 — documented P2, AC5).**
    /// `store` writes only to `inner`; the vector + BM25 indexes update solely on
    /// [`Self::refresh`]. So an entry stored mid-session is NOT semantically
    /// searchable until the next refresh (lazy `OnceCell` init never re-runs a
    /// refresh on its own). This is a CONSCIOUS, tested scope limit
    /// (`store_is_stale_until_refresh_documented_p2`), not a silent gap: it is
    /// conversation-scoped, not cross-job, and the keyword fallback still covers
    /// the empty-index case. Re-open ONLY if Story 12.4 cron telemetry shows
    /// cross-job staleness (then a post-`store` refresh hook or a dirty-flag would
    /// be the fix). No behavioural change in 12.0.
    async fn store(&self, entry: MemoryEntry) -> Result<(), MemoryError> {
        self.inner.store(entry).await
    }

    async fn remember_fact(&self, fact: MemoryFact) -> Result<(), MemoryError> {
        self.inner.remember_fact(fact).await
    }

    async fn recent(&self, limit: usize) -> Result<Vec<MemoryEntry>, MemoryError> {
        self.inner.recent(limit).await
    }

    // ── Overridden: removal-integrity (Story 11.4a / FR122) ──

    /// Fuzzy-match candidate entries to forget, returning each with its stable
    /// `u64` content key for the `/memory forget` confirm card (AC-R0). Matches
    /// case-insensitively over the SAME corpus `refresh()` indexes (`inner`'s
    /// recent content), skipping anything already redacted. Deterministic — no
    /// model call — so the disambiguation card is reproducible.
    async fn forget_candidates(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<(u64, MemoryEntry)>, MemoryError> {
        let needle = query.trim().to_lowercase();
        if needle.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        self.ensure_init().await?;
        let redacted: HashSet<u64> = {
            let guard = self.redactions.read().await;
            guard.keys()
        };
        let entries = self.inner.recent(self.refresh_limit).await?;
        let mut out: Vec<(u64, MemoryEntry)> = Vec::new();
        // Use a HashMap<key, Vec<entry>> to detect collisions — two distinct
        // entries with the same content_key are shown separately so the user
        // can choose which to forget (Story 11.4a CR fix).
        let mut seen: HashMap<u64, Vec<MemoryEntry>> = HashMap::new();
        for e in entries {
            let text = entry_text(&e);
            if fuzzy_match_score(&needle, &text).is_some() {
                let key = content_key(&e.timestamp, &e.summary);
                if !redacted.contains(&key) {
                    seen.entry(key).or_default().push(e);
                }
            }
        }
        // Flatten, keeping all collisions visible. If the same key appears
        // multiple times, each gets a synthetic disambiguating suffix so the
        // user sees every candidate.
        for (key, group) in seen {
            if group.len() == 1 {
                out.push((key, group.into_iter().next().unwrap()));
            } else {
                let group_len = group.len();
                for (idx, mut e) in group.into_iter().enumerate() {
                    let summary = e.summary.clone();
                    e.summary = format!("{} [{}/{}]", summary, idx + 1, group_len);
                    out.push((key, e));
                }
            }
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    /// Permanently purge the given content keys (Story 11.4a, AC-R1/R2/R6). The
    /// tombstone is written and persisted FIRST (source of truth), THEN the
    /// one-time purge runs via `refresh()` — which now filters the redacted keys,
    /// so it drops them from the vector index, rebuilds BM25 excluding them, and
    /// re-persists `index.bin`. If the purge is interrupted after the tombstone
    /// lands, read-time masking + the next refresh converge (AC-R6).
    async fn forget(&self, keys: &[u64]) -> Result<(), MemoryError> {
        if keys.is_empty() {
            return Ok(());
        }
        // Drain gate: hold the local mutex so `prepare_detach` cannot return until
        // any in-flight forget/refresh has finished (Story 12.0 review patch).
        let _drain = self.drain_lock.lock().await;
        self.ensure_init().await?;
        // Resolve each key's LIVE text so the tombstone carries the content-stable
        // token (Story 12.1c AC3) = `normalize(summary)`, the SAME identity the
        // file-edit honor path uses. This (a) makes `/memory forget X` and a
        // hand-delete of X emit an IDENTICAL `RedactionRecord` (the parity test),
        // and (b) suppresses the daily-log re-derivation copy of X — forget's own
        // latent #4 leak — not just the keyed copy. A key with no live entry (the
        // Resolve each key's LIVE text so the tombstone carries the content-stable
        // token (Story 12.1c AC3) = `normalize(summary)`, the SAME identity the
        // file-edit honor path uses. This (a) makes `/memory forget X` and a
        // hand-delete of X emit an IDENTICAL `RedactionRecord` (the parity test),
        // and (b) suppresses the daily-log re-derivation copy of X — forget's own
        // latent #4 leak — not just the keyed copy.
        //
        // First check `inner.recent()` (the live source), then fall back to the
        // persisted index for keys outside the refresh window — a key with no live
        // entry in EITHER place tombstones key-only, exactly as 11.4a did.
        let mut summary_by_key: HashMap<u64, String> = self
            .inner
            .recent(self.refresh_limit)
            .await?
            .into_iter()
            .map(|e| (content_key(&e.timestamp, &e.summary), e.summary))
            .collect();
        // Fall back to the persisted index for stale keys (review patch).
        {
            let guard = self.index.read().await;
            for entry in &guard.entries {
                summary_by_key
                    .entry(entry.key)
                    .or_insert_with(|| entry.entry.summary.clone());
            }
        }
        // 1. Tombstone FIRST + persist to the sidecar (AC-R6).
        let now = Local::now();
        {
            let mut guard = self.redactions.write().await;
            for &key in keys {
                let token = summary_by_key
                    .get(&key)
                    .map(|s| normalize(s))
                    .unwrap_or_default();
                guard.insert(RedactionRecord::redact(key, token, now));
            }
        }
        self.persist_redactions().await?;
        // 2. One-time purge (vector retain + BM25 rebuild-excluding + persist). We
        // ALREADY hold `drain_lock`, so call the lock-free body (re-locking would
        // self-deadlock).
        self.refresh_inner().await
    }

    /// Story 12.1c AC3 — honor `MEMORY.md` hand-deletions (the file-edit auto-honor
    /// path; re-homes 11.4a AC-R0). Detects the facts the user removed from
    /// `MEMORY.md` (draining the curated tier's removal buffer, which reloads on
    /// mtime change first), writes a content-stable `RedactionRecord` per removed
    /// fact FIRST, then purges through the SAME hardened `refresh()` sink
    /// `/memory forget` uses — never a parallel purge path (12.0 AC9). The
    /// hand-edit IS the consent (party-mode 2026-06-07): the purge proceeds live,
    /// end-to-end, headless. Returns the purged entries so the daemon can queue the
    /// "N facts removed" audit notice. Daily logs are NEVER touched.
    async fn honor_md_removals(&self) -> Result<Vec<MemoryEntry>, MemoryError> {
        // Detect first (drains the long-term removal buffer; triggers its reload).
        let removed = self.inner.drain_md_removals().await?;
        if removed.is_empty() {
            return Ok(Vec::new());
        }
        // Drain gate (same discipline as `forget`).
        let _drain = self.drain_lock.lock().await;
        self.ensure_init().await?;
        // Tombstone FIRST (AC-R6), content-stable — byte-identical to a `/memory
        // forget` of the same fact (parity).
        let now = Local::now();
        {
            let mut guard = self.redactions.write().await;
            for e in &removed {
                let key = content_key(&e.timestamp, &e.summary);
                let token = normalize(&e.summary);
                guard.insert(RedactionRecord::redact(key, token, now));
            }
        }
        self.persist_redactions().await?;
        // Purge through the single hardened sink. We hold `drain_lock`, so call the
        // lock-free body (re-locking would self-deadlock).
        self.refresh_inner().await?;
        Ok(removed)
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

        // Read-time redaction mask (Story 11.4a, Task 5 — defense-in-depth). After
        // a successful purge the index holds no redacted key, but during the
        // AC-R6 window (tombstone persisted, purge not yet complete) it may — so
        // we mask redacted keys out of the ranked lists + the key→entry map so a
        // redacted entry is NEVER retrievable, even mid-purge. Story 12.1c AC3
        // adds the content-stable token mask alongside the key mask so a daily-log
        // re-derivation copy is masked in the same window too.
        let (redacted, redacted_tokens): (HashSet<u64>, HashSet<String>) = {
            let guard = self.redactions.read().await;
            (guard.keys(), guard.tokens())
        };
        let token_masked =
            |e: &MemoryEntry| -> bool { redacted_tokens.contains(&normalize(&e.summary)) };

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
                // Nothing indexed yet — keyword fallback over inner (AC4),
                // but redaction-masked so redacted entries never leak.
                let redacted: HashSet<u64> = {
                    let guard = self.redactions.read().await;
                    guard.keys()
                };
                let mut hits = self.inner.search(query, limit).await?;
                hits.retain(|e| {
                    let key = content_key(&e.timestamp, &e.summary);
                    !redacted.contains(&key) && !token_masked(e)
                });
                return Ok(hits);
            }
            let now = Local::now();
            let mut recency = HashMap::with_capacity(guard.entries.len());
            let mut entry_by_key = HashMap::with_capacity(guard.entries.len());
            for e in &guard.entries {
                // Skip redacted entries so they never rank, never map back — by
                // key (11.4a) OR content-stable token (12.1c AC3, masks a
                // daily-log re-derivation copy in the AC-R6 window).
                if redacted.contains(&e.key) || token_masked(&e.entry) {
                    continue;
                }
                let age_days = (now - e.entry.timestamp).num_seconds() as f64 / 86_400.0;
                recency.insert(e.key, fusion::recency_weight(age_days, self.half_life_days));
                entry_by_key.insert(e.key, e.entry.clone());
            }
            let vec_ranked = match &query_vec {
                Some(qv) => {
                    redaction_mask::retain_visible(guard.search_ranked(qv, candidates), &redacted)
                }
                None => Vec::new(),
            };
            (vec_ranked, recency, entry_by_key)
        };

        // BM25 keyword ranking (separate read guard), redaction-masked.
        let bm25_ranked: Vec<u64> = {
            let guard = self.bm25.read().await;
            redaction_mask::retain_visible(guard.ranked(query, candidates), &redacted)
        };

        if vec_ranked.is_empty() && bm25_ranked.is_empty() {
            // No semantic vector (embed failed) AND no keyword match — keyword
            // fallback over inner so the user still gets results (AC4),
            // redaction-masked so redacted entries never leak.
            let mut hits = self.inner.search(query, limit).await?;
            hits.retain(|e| {
                let key = content_key(&e.timestamp, &e.summary);
                !redacted.contains(&key) && !token_masked(e)
            });
            return Ok(hits);
        }

        // Fuse: RRF relevance × half-life recency, top-`limit`, deterministic.
        let relevance = fusion::rrf_relevance(&vec_ranked, &bm25_ranked);
        let fused = fusion::fuse_rank(&relevance, &recency, limit);
        if fused.is_empty() {
            let mut hits = self.inner.search(query, limit).await?;
            hits.retain(|e| {
                let key = content_key(&e.timestamp, &e.summary);
                !redacted.contains(&key) && !token_masked(e)
            });
            return Ok(hits);
        }
        // Guard against concurrent refresh: if keys disappeared from
        // entry_by_key (stale snapshot vs. updated BM25), fall back rather than
        // silently truncating results below the requested limit.
        let missing: Vec<_> = fused
            .iter()
            .filter(|k| !entry_by_key.contains_key(k))
            .collect();
        if !missing.is_empty() {
            tracing::debug!(
                ?missing,
                "concurrent refresh detected — falling back to inner search"
            );
            let mut hits = self.inner.search(query, limit).await?;
            hits.retain(|e| {
                let key = content_key(&e.timestamp, &e.summary);
                !redacted.contains(&key) && !token_masked(e)
            });
            return Ok(hits);
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
        // Drain any in-flight refresh/forget before delegating to inner (Story 12.0
        // review patch). Acquiring (and dropping) the drain_lock cannot return until
        // any concurrent `refresh()` or `forget()` has finished writing index.bin /
        // redactions.bin, so the swap only proceeds once this adapter is quiescent.
        let _drained = self.drain_lock.lock().await;
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

    // ───────────────────────── Story 11.4a — removal-integrity ─────────────────
    //
    // The deterministic StubEmbedder (bag-of-words) means a query sharing the
    // entry's words ranks it first, so the RETRIEVAL boundary (AC-R1/R3 oracle)
    // is assertable without a model. Tests live in the adapter's own test module,
    // so they reach private `refresh()`/`redactions`/`index` directly — letting us
    // drive the exact AC-R3/R6 states (forced refresh, source-still-dirty reindex,
    // tombstone-written-but-purge-not-yet-run) without faking them.

    /// Reload `index.bin` from disk and return its key set (the AC-R1 "absent from
    /// the persisted bytes after reload" oracle — not an in-memory flag).
    async fn keys_on_disk(path: &std::path::Path) -> HashSet<u64> {
        load_index(path)
            .await
            .unwrap()
            .map(|i| i.keys())
            .unwrap_or_default()
    }

    /// Does a hybrid search for `query` return an entry whose summary == `summary`?
    /// This is the RETRIEVAL-boundary oracle across BOTH vector + BM25 (search()
    /// fuses them), as AC-R1/R2 demand — never a flag-only assertion.
    async fn retrievable(mem: &VectorSearchMemory, query: &str, summary: &str) -> bool {
        mem.search(query, 10)
            .await
            .unwrap()
            .iter()
            .any(|e| e.summary == summary)
    }

    const SECRET: &str = "the secret password is hunter2";
    const NEIGHBOUR: &str = "the database uses postgres";

    /// AC-R3 LINCHPIN — `redaction_survives_refresh`. The full linear arc:
    /// seed → embed → assert retrievable → redact → assert gone (AC-R1+R2, both
    /// modes, retrieval boundary, absent from index.bin bytes) → FORCE refresh
    /// with the source STILL dirty → still gone → PURGE index.bin + reindex from
    /// the still-secret-bearing source → STILL gone. A tombstone-only OR
    /// delete-only impl passes the first asserts and FAILS the last — that is the
    /// false-green this test exists to catch.
    #[tokio::test]
    async fn redaction_survives_refresh() {
        let tmp = tempfile::tempdir().unwrap();
        let index_path = tmp.path().join("memory").join("index.bin");
        let redactions_path = sidecar_path(&index_path);

        // The source is STILL DIRTY throughout (append-only daily-log semantics):
        // the secret row is never removed from `inner`. This is the explicit
        // precondition — without it the reindex step would be a vacuous_pass.
        let dirty_inner = || fake_inner(&[(1, SECRET), (2, NEIGHBOUR)]);
        let stub = || Arc::new(StubEmbedder::new(64, "stub-v1"));

        let mem = VectorSearchMemory::new(dirty_inner(), stub(), index_path.clone());
        mem.initialize().await.unwrap();
        assert!(
            retrievable(&mem, "secret password", SECRET).await,
            "pre-redaction the secret IS retrievable (precondition for the gap)"
        );

        // Redact it.
        let secret_key = content_key(&super::super::index::tests_millis_to_local(1), SECRET);
        mem.forget(&[secret_key]).await.unwrap();

        // AC-R1: absent from the persisted index.bin bytes after reload.
        assert!(
            !keys_on_disk(&index_path).await.contains(&secret_key),
            "AC-R1: redacted key is absent from index.bin after reload"
        );
        // AC-R1 + AC-R2: gone at the retrieval boundary (vector + BM25 fused).
        assert!(
            !retrievable(&mem, "secret password", SECRET).await,
            "AC-R1/R2: a query that WOULD have matched returns the secret absent"
        );
        // AC-R3 step "purge is surgical" (also 6.5): the neighbour survives.
        assert!(
            retrievable(&mem, "database postgres", NEIGHBOUR).await,
            "purge is surgical — the non-redacted neighbour is still retrievable"
        );

        // FORCE refresh() again with the source STILL dirty (the self-heal trap).
        mem.refresh().await.unwrap();
        assert!(
            !keys_on_disk(&index_path).await.contains(&secret_key),
            "AC-R3: after a forced refresh over the still-dirty source, still absent on disk"
        );
        assert!(
            !retrievable(&mem, "secret password", SECRET).await,
            "AC-R3: refresh did NOT re-embed the still-present source row (no ghost re-embed)"
        );

        // PURGE index.bin + reindex from the STILL-dirty source (the false-green
        // catcher). A fresh adapter over the same paths: index.bin gone → rebuild
        // from inner — but the redactions sidecar survives and gates the rebuild.
        tokio::fs::remove_file(&index_path).await.unwrap();
        assert!(
            redactions_path.exists(),
            "the tombstone sidecar OUTLIVES index.bin (it is not inside it)"
        );
        let mem2 = VectorSearchMemory::new(dirty_inner(), stub(), index_path.clone());
        mem2.initialize().await.unwrap();
        assert!(
            !keys_on_disk(&index_path).await.contains(&secret_key),
            "AC-R3 LINCHPIN: rebuilt-from-source index STILL excludes the redacted key"
        );
        assert!(
            !retrievable(&mem2, "secret password", SECRET).await,
            "AC-R3 LINCHPIN: still gone at retrieval after a full reindex from the dirty source"
        );
        assert!(
            retrievable(&mem2, "database postgres", NEIGHBOUR).await,
            "the neighbour is re-indexed by the rebuild (gate is surgical)"
        );
    }

    // ───────────────────── Story 12.0 C4 — full-set tombstone gate ─────────────
    //
    // RED-FIRST PROBE (Murat's exit gate): a redacted entry aged PAST
    // `refresh_limit` must be purged from vector + BM25 + index.bin. The record is
    // the LAST inner entry with `refresh_limit = 2`, so `inner.recent(2)` never
    // visits it. Per Task 3, this MUST be proven red on the unfixed gate first; if
    // it does NOT go red, STOP and re-scope (the bug is not where we think).

    /// DIAGNOSTIC — characterises the ACTUAL `refresh()` window behaviour so the
    /// C4 scope is grounded in observed code, not assumption. Asserts that a
    /// non-redacted entry aged PAST `refresh_limit` is DROPPED by refresh (because
    /// `retain_keys(current_keys)` retains only the window). If this passes, the
    /// hypothesised "redacted out-of-window entry LINGERS" cannot occur — the
    /// window-limited retain already drops every out-of-window key, redacted or
    /// not. (The inverse over-drop, not PII re-exposure.)
    #[tokio::test]
    async fn refresh_window_drops_out_of_window_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let index_path = tmp.path().join("memory").join("index.bin");
        // SECRET (ms=1) is the LAST inner entry → outside `recent(2)`.
        let inner = || fake_inner(&[(2, NEIGHBOUR), (3, "the cache is redis"), (1, SECRET)]);
        let stub = || Arc::new(StubEmbedder::new(64, "stub-v1"));

        // Full index first (default window indexes all three).
        let mem0 = VectorSearchMemory::new(inner(), stub(), index_path.clone());
        mem0.initialize().await.unwrap();
        let secret_key = content_key(&super::super::index::tests_millis_to_local(1), SECRET);
        assert!(
            keys_on_disk(&index_path).await.contains(&secret_key),
            "precondition: SECRET is in index.bin before any windowing"
        );
        drop(mem0);

        // Reopen with a SMALL window; the load+refresh applies `retain_keys`.
        let mut mem = VectorSearchMemory::new(inner(), stub(), index_path.clone());
        mem.set_refresh_limit(2);
        mem.initialize().await.unwrap();
        mem.refresh().await.unwrap();

        // OBSERVED: the out-of-window (un-redacted) SECRET is dropped by the
        // window-limited retain — the inverse of the "lingers" hypothesis.
        assert!(
            !keys_on_disk(&index_path).await.contains(&secret_key),
            "window-limited retain_keys drops out-of-window entries (over-drop, not lingering)"
        );
    }

    /// AI-12.0a tracked follow-up (party-mode consensus 2026-06-05, Murat/Winston;
    /// checked in #[ignore], NOT deleted). The over-drop above is a latent
    /// correctness bug: `refresh()` conflates "what to EMBED = the recency window"
    /// with "what to RETAIN = the full live set", so any non-redacted entry aged
    /// past `refresh_limit` is silently dropped from the index. Today
    /// `refresh_limit = 10_000 = NFR56` cap ≥ corpus, so it never fires in prod —
    /// but lower the cap below the corpus and refresh amputates live memory. The
    /// long-term-correct fix: retain `full_live − redacted`, embed only `window`,
    /// as two distinct inputs. This invariant test asserts the DESIRED behaviour
    /// (a live out-of-window entry survives refresh) and currently FAILS — it goes
    /// green when the conflation is fixed, so a future cap-lowering can't ship a
    /// silent amputation.
    #[tokio::test]
    #[ignore = "AI-12.0a: refresh() retain-set/embed-window conflation; fails until retain=full_live−redacted lands"]
    async fn refresh_must_not_drop_live_out_of_window_entries_12_0a() {
        let tmp = tempfile::tempdir().unwrap();
        let index_path = tmp.path().join("memory").join("index.bin");
        let inner = || fake_inner(&[(2, NEIGHBOUR), (3, "the cache is redis"), (1, SECRET)]);
        let stub = || Arc::new(StubEmbedder::new(64, "stub-v1"));

        // Full index first; SECRET (ms=1) is the LAST inner entry.
        let mem0 = VectorSearchMemory::new(inner(), stub(), index_path.clone());
        mem0.initialize().await.unwrap();
        let secret_key = content_key(&super::super::index::tests_millis_to_local(1), SECRET);
        drop(mem0);

        // Shrink the window below the corpus; SECRET is now out-of-window but it is
        // NOT redacted — a correct refresh MUST keep it.
        let mut mem = VectorSearchMemory::new(inner(), stub(), index_path.clone());
        mem.set_refresh_limit(2);
        mem.initialize().await.unwrap();
        mem.refresh().await.unwrap();

        assert!(
            keys_on_disk(&index_path).await.contains(&secret_key),
            "AI-12.0a: a non-redacted out-of-window entry MUST survive refresh (retain ≠ embed-window)"
        );
    }

    /// C4 (AC4) red-first oracle, exactly as Task 3 Test 1 specifies: redact the
    /// out-of-window entry, run a scheduled-style refresh, assert absent from
    /// index.bin AND retrieval. On the CURRENT gate this is GREEN — the entry was
    /// already dropped by the window retain before redaction even applied — which
    /// is the vacuous pass the exit gate warns about. Kept as the documented probe
    /// (see Dev Agent Record / Debug Log: the C4 hypothesis did NOT reproduce).
    #[tokio::test]
    async fn redaction_survives_refresh_beyond_window() {
        let tmp = tempfile::tempdir().unwrap();
        let index_path = tmp.path().join("memory").join("index.bin");
        let inner = || fake_inner(&[(2, NEIGHBOUR), (3, "the cache is redis"), (1, SECRET)]);
        let stub = || Arc::new(StubEmbedder::new(64, "stub-v1"));

        let mem0 = VectorSearchMemory::new(inner(), stub(), index_path.clone());
        mem0.initialize().await.unwrap();
        let secret_key = content_key(&super::super::index::tests_millis_to_local(1), SECRET);
        drop(mem0);

        let mut mem = VectorSearchMemory::new(inner(), stub(), index_path.clone());
        mem.set_refresh_limit(2);
        mem.initialize().await.unwrap();
        mem.forget(&[secret_key]).await.unwrap();
        mem.refresh().await.unwrap();

        assert!(
            !keys_on_disk(&index_path).await.contains(&secret_key),
            "C4: out-of-window redacted key absent from index.bin"
        );
        assert!(
            !retrievable(&mem, "secret password", SECRET).await,
            "C4: out-of-window redacted entry absent from vector+BM25 retrieval"
        );
    }

    /// C4 (AC4) RED-FIRST ORACLE — the REAL concurrency edge (scope confirmed with
    /// the author after the window hypothesis proved non-reproducing): a `forget()`
    /// whose tombstone lands DURING `refresh()`'s long `embed()` await is missed by
    /// the start-of-refresh redaction snapshot, so without the final live-set gate
    /// the freshly-embedded redacted key is written into `index.bin` and survives
    /// until the next refresh. The interior `refresh_seam` parks the build refresh
    /// after embed; the tombstone is injected exactly then (tombstone-FIRST, like
    /// `forget`, AC-R6); on resume the refresh MUST re-consult the live set and
    /// purge the key. **Verified RED on the reverted gate** (index.bin retained the
    /// redacted key) — see Dev Agent Record / Debug Log. No `sleep`.
    #[tokio::test]
    async fn refresh_honors_concurrent_forget() {
        use std::sync::Arc;
        use std::sync::atomic::Ordering;

        let tmp = tempfile::tempdir().unwrap();
        let index_path = tmp.path().join("memory").join("index.bin");
        // SECRET is NEW (empty index → it WILL be embedded by the build refresh).
        let inner = fake_inner(&[(2, NEIGHBOUR), (1, SECRET)]);
        let stub = Arc::new(StubEmbedder::new(64, "stub-v1"));
        let mem = Arc::new(VectorSearchMemory::new(inner, stub, index_path.clone()));
        let secret_key = content_key(&super::super::index::tests_millis_to_local(1), SECRET);

        // Arm the seam, kick the build (refresh embeds NEIGHBOUR + SECRET).
        mem.refresh_seam.armed.store(true, Ordering::SeqCst);
        let m = Arc::clone(&mem);
        let init = tokio::spawn(async move { m.initialize().await.unwrap() });

        // Refresh is now parked AFTER embedding SECRET, BEFORE the final gate.
        mem.refresh_seam.reached.notified().await;

        // A concurrent forget() lands its tombstone during the embed window
        // (tombstone FIRST + sidecar persist, exactly as `forget` does).
        {
            let mut g = mem.redactions.write().await;
            g.insert(RedactionRecord::forget(secret_key, Local::now()));
        }
        mem.persist_redactions().await.unwrap();

        // Resume: the final tombstone gate must honor the live set.
        mem.refresh_seam.proceed.notify_one();
        init.await.unwrap();

        assert!(
            !keys_on_disk(&index_path).await.contains(&secret_key),
            "C4: forget() during refresh's embed is honored — redacted key absent from index.bin"
        );
        assert!(
            !retrievable(&mem, "secret password", SECRET).await,
            "C4: redacted entry not retrievable after the concurrent forget"
        );
        // The non-redacted neighbour is still indexed (the gate is surgical).
        assert!(
            keys_on_disk(&index_path).await.contains(&content_key(
                &super::super::index::tests_millis_to_local(2),
                NEIGHBOUR
            )),
            "C4: the concurrent purge is surgical — the neighbour survives"
        );
    }

    /// C5 (AC5) — the in-session staleness contract is DELIBERATE and tested (a
    /// documented P2 scope limit, not a silent gap). A `store()` mid-session lands
    /// in `inner` but is NOT in the vector/BM25 index until the next `refresh()`.
    /// The index is seeded non-empty first so the empty-index keyword fallback does
    /// not mask the contract. No behavioural change — this pins the documented
    /// limit so any future regression (or an accidental auto-refresh) is caught.
    #[tokio::test]
    async fn store_is_stale_until_refresh_documented_p2() {
        use std::sync::Arc;

        let tmp = tempfile::tempdir().unwrap();
        let index_path = tmp.path().join("memory").join("index.bin");
        // A real inner that actually persists `store()`s.
        let inner = Arc::new(crate::adapters::daily_log_memory::DailyLogMemory::new(
            tmp.path(),
        ));
        // Seed ONE entry so the index is non-empty (else `search` keyword-falls-
        // back to `inner`, which WOULD see the fresh store and mask the contract).
        inner
            .store(MemoryEntry {
                timestamp: Local::now(),
                summary: "seed entry about postgres".into(),
                context: None,
            })
            .await
            .unwrap();

        let stub = Arc::new(StubEmbedder::new(64, "stub-v1"));
        let mem = VectorSearchMemory::new(inner.clone(), stub, index_path.clone());
        mem.initialize().await.unwrap(); // index = {seed}

        // A mid-session store lands in the inner source…
        mem.store(MemoryEntry {
            timestamp: Local::now(),
            summary: "fresh insight just now".into(),
            context: None,
        })
        .await
        .unwrap();

        // …but is stale-until-refresh: not yet in the vector/BM25 index (C5 / P2).
        let stale = mem.search("fresh insight", 10).await.unwrap();
        assert!(
            !stale.iter().any(|e| e.summary == "fresh insight just now"),
            "C5 (P2): a store() is stale-until-refresh — not yet semantically searchable"
        );

        // After a refresh it converges (the documented re-indexing point).
        mem.refresh().await.unwrap();
        let fresh = mem.search("fresh insight", 10).await.unwrap();
        assert!(
            fresh.iter().any(|e| e.summary == "fresh insight just now"),
            "C5: refresh() converges — the entry is now indexed and retrievable"
        );
    }

    /// AC6 (regression guard) — the vector index and the key-aligned BM25 index
    /// stay aligned under concurrent `search()` + `refresh()` (guards the Epic 11
    /// review fix at the `refresh()` BM25-rebuild path). Many concurrent refreshes
    /// (each rebuilds BM25 over the FULL current corpus) interleave with many
    /// concurrent searches (each fuses vector + BM25 by `content_key`). Every op
    /// MUST succeed and the fused key→entry map MUST resolve — a desync would
    /// surface as an `Err`, a panic, or a vanished seeded entry. Asserts
    /// interleaving-invariant properties (all-Ok + seeded entry retrievable), so
    /// it is not flaky.
    #[tokio::test]
    async fn concurrent_search_and_refresh_stay_aligned() {
        use std::sync::Arc;

        let tmp = tempfile::tempdir().unwrap();
        let index_path = tmp.path().join("memory").join("index.bin");
        let inner = fake_inner(&[
            (1, "alpha postgres database"),
            (2, "beta redis cache"),
            (3, "gamma kafka stream"),
            (4, "delta object store"),
        ]);
        let stub = Arc::new(StubEmbedder::new(64, "stub-v1"));
        let mem = Arc::new(VectorSearchMemory::new(inner, stub, index_path.clone()));
        mem.initialize().await.unwrap();

        let mut handles = Vec::new();
        // Concurrent refreshers — each rebuilds BM25 over the full corpus.
        for _ in 0..4 {
            let m = Arc::clone(&mem);
            handles.push(tokio::spawn(async move {
                for _ in 0..10 {
                    m.refresh().await.unwrap();
                }
            }));
        }
        // Concurrent searchers — each fuses the two indexes; none may error/panic.
        for _ in 0..8 {
            let m = Arc::clone(&mem);
            handles.push(tokio::spawn(async move {
                for _ in 0..10 {
                    let hits = m.search("postgres database", 5).await.unwrap();
                    // A desynced pair would map a ranked key to no/garbled entry.
                    for h in &hits {
                        assert!(!h.summary.is_empty(), "fused key resolved to a real entry");
                    }
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        // After the churn the seeded entry is still retrievable through the fused
        // path — alignment held end-to-end.
        assert!(
            retrievable(&mem, "postgres database", "alpha postgres database").await,
            "AC6: vector/BM25 stay aligned under concurrent search()+refresh()"
        );
    }

    /// AC-R5 — `redaction_survives_restart`. Redact, drop all in-memory state,
    /// reload index + tombstone COLD from disk, assert still gone AND the refresh
    /// filter is still active (a forced refresh does not resurrect it).
    #[tokio::test]
    async fn redaction_survives_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let index_path = tmp.path().join("memory").join("index.bin");
        let dirty_inner = || fake_inner(&[(1, SECRET), (2, NEIGHBOUR)]);

        let mem = VectorSearchMemory::new(
            dirty_inner(),
            Arc::new(StubEmbedder::new(64, "stub-v1")),
            index_path.clone(),
        );
        mem.initialize().await.unwrap();
        let secret_key = content_key(&super::super::index::tests_millis_to_local(1), SECRET);
        mem.forget(&[secret_key]).await.unwrap();

        // Drop ALL in-memory state.
        drop(mem);

        // Cold reload — a brand-new adapter reads index.bin + redactions.bin.
        let mem = VectorSearchMemory::new(
            dirty_inner(),
            Arc::new(StubEmbedder::new(64, "stub-v1")),
            index_path.clone(),
        );
        mem.initialize().await.unwrap();
        assert!(
            !retrievable(&mem, "secret password", SECRET).await,
            "AC-R5: still gone after a cold restart"
        );
        // Filter still active: a forced refresh over the dirty source keeps it out.
        mem.refresh().await.unwrap();
        assert!(
            !keys_on_disk(&index_path).await.contains(&secret_key),
            "AC-R5: the refresh filter is still armed after restart"
        );
    }

    /// AC-R6 — cross-store consistency + partial-failure. Simulate "tombstone
    /// written but the one-time purge did NOT run": insert the tombstone directly
    /// (index.bin still holds the vector). Read-time masking MUST hide it anyway,
    /// then the next `refresh()` converges the on-disk index to the tombstone set.
    #[tokio::test]
    async fn redaction_partial_failure_masks_then_converges() {
        let tmp = tempfile::tempdir().unwrap();
        let index_path = tmp.path().join("memory").join("index.bin");
        let mem = VectorSearchMemory::new(
            fake_inner(&[(1, SECRET), (2, NEIGHBOUR)]),
            Arc::new(StubEmbedder::new(64, "stub-v1")),
            index_path.clone(),
        );
        mem.initialize().await.unwrap();
        let secret_key = content_key(&super::super::index::tests_millis_to_local(1), SECRET);

        // Tombstone lands but the purge is "interrupted": the index STILL has it.
        {
            let mut g = mem.redactions.write().await;
            g.insert(RedactionRecord::forget(secret_key, Local::now()));
        }
        assert!(
            mem.index.read().await.keys().contains(&secret_key),
            "precondition: the half-state — index still holds the redacted vector"
        );
        // Read-time masking hides it despite the un-purged index (AC-R6).
        assert!(
            !retrievable(&mem, "secret password", SECRET).await,
            "AC-R6: redacted ⇒ never retrievable, even before the purge completes"
        );

        // Next refresh converges the on-disk index (idempotent re-purge).
        mem.refresh().await.unwrap();
        assert!(
            !mem.index.read().await.keys().contains(&secret_key),
            "AC-R6: the next refresh converges the in-memory index"
        );
        assert!(
            !keys_on_disk(&index_path).await.contains(&secret_key),
            "AC-R6: and persists the converged state to disk"
        );
    }

    /// AC-R0 — `forget_candidates` is the disambiguation/confirm card's data
    /// source: fuzzy text → (stable key, entry) pairs, redaction-aware.
    #[tokio::test]
    async fn forget_candidates_match_fuzzy_text_and_skip_already_redacted() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = VectorSearchMemory::new(
            fake_inner(&[(1, SECRET), (2, NEIGHBOUR), (3, "secret handshake ritual")]),
            Arc::new(StubEmbedder::new(64, "stub-v1")),
            tmp.path().join("memory").join("index.bin"),
        );
        mem.initialize().await.unwrap();

        let cands = mem.forget_candidates("secret", 10).await.unwrap();
        assert_eq!(cands.len(), 2, "two entries contain 'secret'");
        assert!(cands.iter().all(|(k, _)| *k != 0));

        // Forget one of them, then it must no longer be a candidate.
        let key = cands[0].0;
        mem.forget(&[key]).await.unwrap();
        let after = mem.forget_candidates("secret", 10).await.unwrap();
        assert_eq!(after.len(), 1, "the redacted entry drops out of candidates");
        assert!(after.iter().all(|(k, _)| *k != key));

        // Empty / blank query → no candidates.
        assert!(mem.forget_candidates("", 10).await.unwrap().is_empty());
        assert!(mem.forget_candidates("   ", 10).await.unwrap().is_empty());
    }

    // ───────────────── Story 12.1c AC3 — content-stable redaction ──────────────
    //
    // These exercise the file-edit-honor path over a REAL `project-scoped` inner
    // (long-term `MEMORY.md` + daily-log), so `recent()` reflects hand-edits and the
    // daily-log re-derivation leak (#4) is genuinely reproducible.

    use std::time::{Duration as StdDuration, SystemTime};

    fn memory_md_path(workspace: &std::path::Path) -> PathBuf {
        workspace.join(".rustain").join("MEMORY.md")
    }

    /// Set a file's mtime (std-only; mirrors the long_term_memory test helper).
    fn filetime_set(path: &std::path::Path, when: SystemTime) {
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(when).unwrap();
    }

    fn project_scoped_inner(workspace: &std::path::Path) -> Arc<dyn MemoryPort> {
        Arc::new(crate::adapters::project_scoped_memory::ProjectScopedMemory::new(workspace))
    }

    /// Total bytes across the daily-log files — an append-only proxy: a tombstone
    /// that ever touched a source log line would SHRINK this (Murat's invariant).
    fn daily_log_bytes(workspace: &std::path::Path) -> u64 {
        let dir = workspace.join(".rustain").join("memory");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return 0;
        };
        entries
            .flatten()
            .filter_map(|e| e.metadata().ok().map(|m| m.len()))
            .sum()
    }

    /// **AC3 parity — `forget_and_fileedit_emit_identical_record`.** ONE removal
    /// identity, two producers: `/memory forget X` and a hand-delete of X both
    /// tombstone the SAME content-stable token AND the same `u64` key, producing an
    /// identical `RedactionRecord` (timestamp ignored — diagnostic only). Seeding X
    /// at a FIXED mtime makes the `content_key(entry_timestamp(mtime), summary)`
    /// deterministic across the two independent setups.
    #[tokio::test]
    async fn forget_and_fileedit_emit_identical_record() {
        const X: &str = "the launch code is alpha";
        let fixed = SystemTime::UNIX_EPOCH + StdDuration::from_secs(1_700_000_000);
        let key = content_key(&DateTime::<Local>::from(fixed), X);

        // Producer A — `/memory forget X`.
        let ws_a = tempfile::tempdir().unwrap();
        let inner_a = project_scoped_inner(ws_a.path());
        inner_a
            .remember_fact(MemoryFact {
                category: "Secrets".into(),
                fact: X.into(),
                detail: None,
            })
            .await
            .unwrap();
        filetime_set(&memory_md_path(ws_a.path()), fixed);
        let mem_a = VectorSearchMemory::new(
            inner_a.clone(),
            Arc::new(StubEmbedder::new(64, "stub-v1")),
            ws_a.path().join("memory").join("index.bin"),
        );
        mem_a.initialize().await.unwrap();
        mem_a.forget(&[key]).await.unwrap();
        let rec_a = {
            let g = mem_a.redactions.read().await;
            g.get(key).cloned()
        }
        .expect("forget wrote a tombstone for the key");

        // Producer B — hand-delete X from MEMORY.md, then honor the file edit.
        let ws_b = tempfile::tempdir().unwrap();
        let inner_b = project_scoped_inner(ws_b.path());
        inner_b
            .remember_fact(MemoryFact {
                category: "Secrets".into(),
                fact: X.into(),
                detail: None,
            })
            .await
            .unwrap();
        filetime_set(&memory_md_path(ws_b.path()), fixed);
        let mem_b = VectorSearchMemory::new(
            inner_b.clone(),
            Arc::new(StubEmbedder::new(64, "stub-v1")),
            ws_b.path().join("memory").join("index.bin"),
        );
        mem_b.initialize().await.unwrap(); // loads X at the fixed mtime
        // Hand-delete: rewrite MEMORY.md without X, bump mtime so the reload fires.
        std::fs::write(memory_md_path(ws_b.path()), "# MEMORY\n").unwrap();
        filetime_set(
            &memory_md_path(ws_b.path()),
            fixed + StdDuration::from_secs(2),
        );
        let purged = mem_b.honor_md_removals().await.unwrap();
        assert_eq!(
            purged.len(),
            1,
            "the hand-deleted fact is detected + purged"
        );
        let rec_b = {
            let g = mem_b.redactions.read().await;
            g.get(key).cloned()
        }
        .expect("file-edit honor wrote a tombstone for the same key");

        // Byte-identical modulo the diagnostic timestamp.
        assert_eq!(rec_a.key, rec_b.key, "same u64 content key");
        assert_eq!(rec_a.op, rec_b.op, "same op (no new RedactionOp variant)");
        assert_eq!(
            rec_a.token, rec_b.token,
            "same content-stable token (normalize(summary))"
        );
        assert_eq!(rec_a.token, normalize(X), "token IS normalize(summary)");
    }

    /// **AC3 discriminator — `daily_log_copy_stays_redacted_after_forget`.** Proves
    /// the fix is REAL, not a false-green. Seed X in BOTH the daily-log AND
    /// `MEMORY.md`; redact X; then hand-delete the `MEMORY.md` line (dropping the
    /// long-term suppressor so the append-only daily-log copy would otherwise
    /// re-derive under a DIFFERENT timestamp key). RED under timestamp-only keying
    /// (the daily copy re-embeds); GREEN only with the content-stable token. A
    /// sibling Y in the same category is the negative control (must survive).
    #[tokio::test]
    async fn daily_log_copy_stays_redacted_after_forget() {
        const X: &str = "the launch code is alpha";
        const Y: &str = "the backup code is beta";

        let ws = tempfile::tempdir().unwrap();
        let inner = project_scoped_inner(ws.path());

        // Daily-log copy of X at a DISTINCT timestamp namespace (different key than
        // the MEMORY.md-mtime copy) — this is what re-derives after the hand-delete.
        inner
            .store(MemoryEntry {
                timestamp: super::super::index::tests_millis_to_local(1_600_000_000_000),
                summary: X.into(),
                context: None,
            })
            .await
            .unwrap();
        // MEMORY.md copy of X (suppresses the daily copy via merge_dedup) + sibling Y.
        inner
            .remember_fact(MemoryFact {
                category: "Secrets".into(),
                fact: X.into(),
                detail: None,
            })
            .await
            .unwrap();
        inner
            .remember_fact(MemoryFact {
                category: "Secrets".into(),
                fact: Y.into(),
                detail: None,
            })
            .await
            .unwrap();

        let mem = VectorSearchMemory::new(
            inner.clone(),
            Arc::new(StubEmbedder::new(64, "stub-v1")),
            ws.path().join("memory").join("index.bin"),
        );
        mem.initialize().await.unwrap();
        assert!(
            retrievable(&mem, "launch code", X).await,
            "precondition: X is retrievable before redaction"
        );

        // Redact X via `/memory forget` (sets the content-stable token normalize(X)).
        let cands = mem.forget_candidates("launch code", 10).await.unwrap();
        let xkey = cands
            .iter()
            .find(|(_, e)| e.summary == X)
            .map(|(k, _)| *k)
            .expect("X is a forget candidate");
        mem.forget(&[xkey]).await.unwrap();
        assert!(
            !retrievable(&mem, "launch code", X).await,
            "X gone right after forget"
        );

        // Hand-delete the MEMORY.md X line (keep Y), drop the long-term suppressor.
        std::fs::write(
            memory_md_path(ws.path()),
            format!("# MEMORY\n\n## Secrets\n\n- {Y}\n"),
        )
        .unwrap();
        let bump = SystemTime::now() + StdDuration::from_secs(2);
        filetime_set(&memory_md_path(ws.path()), bump);

        // Refresh: under timestamp-only keying the daily-log X (different key) would
        // re-embed → RED. The content-stable token suppresses it → GREEN.
        mem.refresh().await.unwrap();
        assert!(
            !retrievable(&mem, "launch code", X).await,
            "AC3 discriminator: the daily-log copy does NOT re-derive after the hand-delete"
        );
        assert!(
            !keys_on_disk(&ws.path().join("memory").join("index.bin"))
                .await
                .contains(&content_key(
                    &super::super::index::tests_millis_to_local(1_600_000_000_000),
                    X
                )),
            "the daily-log-keyed X copy is absent from index.bin (vector side)"
        );
        // Idempotent across a second refresh.
        mem.refresh().await.unwrap();
        assert!(
            !retrievable(&mem, "launch code", X).await,
            "still gone after a second refresh (idempotent)"
        );
        // Negative control: the sibling Y survives (no over-broad normalization).
        assert!(
            retrievable(&mem, "backup code", Y).await,
            "the sibling fact Y in the same category is unaffected"
        );
    }

    /// **AC1/AC3 invariant — `daily_log_is_append_only_across_all_mutations`** (Murat).
    /// Every story mutation (file-edit honor, `/memory forget`, refresh) leaves the
    /// daily-log byte-count MONOTONIC non-decreasing — a tombstone marks a derived
    /// fact dead, it NEVER touches a source log line. (The append-only log is the
    /// foundation the re-derivation suppression leans on.)
    #[tokio::test]
    async fn daily_log_is_append_only_across_all_mutations() {
        const X: &str = "the launch code is alpha";
        let ws = tempfile::tempdir().unwrap();
        let inner = project_scoped_inner(ws.path());
        inner
            .store(MemoryEntry {
                timestamp: Local::now(),
                summary: "operational record one".into(),
                context: None,
            })
            .await
            .unwrap();
        inner
            .store(MemoryEntry {
                timestamp: Local::now(),
                summary: X.into(),
                context: None,
            })
            .await
            .unwrap();
        inner
            .remember_fact(MemoryFact {
                category: "Secrets".into(),
                fact: X.into(),
                detail: None,
            })
            .await
            .unwrap();

        let mem = VectorSearchMemory::new(
            inner.clone(),
            Arc::new(StubEmbedder::new(64, "stub-v1")),
            ws.path().join("memory").join("index.bin"),
        );
        mem.initialize().await.unwrap();

        let mut floor = daily_log_bytes(ws.path());
        let mut check = |ws: &std::path::Path, label: &str, floor: &mut u64| {
            let now = daily_log_bytes(ws);
            assert!(
                now >= *floor,
                "daily-log shrank after {label}: {now} < {floor}"
            );
            *floor = now;
        };

        // /memory forget
        let cands = mem.forget_candidates("launch code", 10).await.unwrap();
        let xkey = cands
            .iter()
            .find(|(_, e)| e.summary == X)
            .map(|(k, _)| *k)
            .unwrap();
        mem.forget(&[xkey]).await.unwrap();
        check(ws.path(), "forget", &mut floor);

        // file-edit redaction-honor (hand-delete X from MEMORY.md)
        std::fs::write(memory_md_path(ws.path()), "# MEMORY\n").unwrap();
        filetime_set(
            &memory_md_path(ws.path()),
            SystemTime::now() + StdDuration::from_secs(2),
        );
        mem.honor_md_removals().await.unwrap();
        check(ws.path(), "honor_md_removals", &mut floor);

        // refresh
        mem.refresh().await.unwrap();
        check(ws.path(), "refresh", &mut floor);

        // consolidation-enqueue is a no-op for the daily log (writes to .rustain root).
        assert!(
            daily_log_bytes(ws.path()) >= floor,
            "daily-log untouched by the consolidation queue"
        );
    }
}
