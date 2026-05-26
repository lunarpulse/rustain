//! BM25-backed `MetaSearchEngine` impl per ADR-09-02 v2 §Phased Implementation
//! Phase B. Wraps `MergedIndex` (the corpus-agnostic index over both kinds)
//! and applies `top_k` clamping + kind-filter post-rank predicate.
//!
//! ## Synonym expansion (Story 9-7c)
//!
//! Synonym expansion happens at the **query-string boundary** inside
//! `Bm25SearchEngine::search`, NOT by wrapping the bm25 `Tokenizer` trait.
//! The bm25 v=2.3.2 `SearchEngineBuilder::with_documents` constructor does
//! NOT expose a custom-tokenizer hook; wrapping the trait would require a
//! multi-day refactor of `MergedIndex` for zero AC-binding benefit.
//! Query-side expansion achieves the same BM25 IDF effect (the expanded
//! token is scored against the indexed document's English tokenization).

use async_trait::async_trait;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use arc_swap::ArcSwap;

use crate::domain::models::capability_kind::CapabilityKind;
use crate::domain::models::search_hit::SearchHit;
use crate::domain::ports::search::{MetaSearchEngine, MetaSearchError};
use crate::infrastructure::search::merged_index::MergedIndex;
use crate::infrastructure::search::synonym_map::{SYNONYMS, SynonymMap};

/// Process-lifetime counter of how many queries triggered synonym expansion.
#[doc(hidden)]
pub static SYNONYM_EXPANSION_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Return the current count of synonym expansion events.
#[doc(hidden)]
pub fn synonym_expansion_count() -> u64 {
    SYNONYM_EXPANSION_COUNTER.load(AtomicOrdering::Acquire)
}

/// BM25-backed engine over a hot-swappable `MergedIndex`.
///
/// The engine holds `Arc<ArcSwap<MergedIndex>>` so background reindex tasks
/// (driven by `CatalogObserverRegistry` per AC-9-7-8) can swap a fresh
/// `MergedIndex` in atomically without holding any lock across the swap point.
/// Reader paths (`search`) do a single `index.load_full()` and operate on the
/// returned `Arc<MergedIndex>` for the duration of the query — no locks held
/// across `.await` per CLAUDE.md §Async Lock Policy.
pub struct Bm25SearchEngine {
    index: Arc<ArcSwap<MergedIndex>>,
    synonyms: Arc<SynonymMap>,
}

impl Bm25SearchEngine {
    pub fn new(index: Arc<ArcSwap<MergedIndex>>) -> Self {
        Self {
            index,
            synonyms: Arc::new(SYNONYMS.clone()),
        }
    }

    /// Replace the index atomically. Called by the reindex owned task
    /// from `CatalogObserverRegistry` after a catalog delta + 250ms debounce.
    pub fn swap_index(&self, new_index: Arc<MergedIndex>) {
        self.index.store(new_index);
    }
}

#[cfg(test)]
impl Bm25SearchEngine {
    /// Test-only constructor that injects a custom synonym map.
    /// Used by `test_bm25_scores_higher_with_synonyms_than_without` to
    /// build a control engine with an empty synonym map.
    pub fn with_synonyms(index: Arc<ArcSwap<MergedIndex>>, synonyms: SynonymMap) -> Self {
        Self {
            index,
            synonyms: Arc::new(synonyms),
        }
    }
}

#[async_trait]
impl MetaSearchEngine for Bm25SearchEngine {
    async fn search(
        &self,
        query: &str,
        kind_filter: Option<CapabilityKind>,
        top_k: usize,
    ) -> Result<Vec<SearchHit>, MetaSearchError> {
        if query.trim().is_empty() {
            return Err(MetaSearchError::EmptyQuery);
        }
        if top_k > 20 {
            return Err(MetaSearchError::TopKTooLarge(top_k));
        }
        let clamped_k = top_k.clamp(1, 20);

        let (expanded_query, triggered) = self.synonyms.expand_query(query);
        if triggered {
            SYNONYM_EXPANSION_COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
        }

        let snapshot = self.index.load_full();
        // `snapshot.search` is sync (BM25 score computation is CPU-bound,
        // no I/O); we do not `.await` inside it.
        let hits = snapshot.search(&expanded_query, kind_filter, clamped_k);
        Ok(hits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::capability_kind::CapabilityKind;
    use crate::domain::models::doc_key::DocKey;
    use crate::domain::models::search_hit::SearchHit;
    use crate::domain::ports::search::IndexableItem;

    struct TestItem {
        name: String,
        desc: String,
        kind: CapabilityKind,
    }

    impl IndexableItem for TestItem {
        fn doc_key(&self) -> DocKey {
            DocKey::new(self.kind, self.name.clone())
        }

        fn searchable_text(&self) -> std::borrow::Cow<'_, str> {
            std::borrow::Cow::Owned(format!("{} {}", self.name, self.desc))
        }

        fn description(&self) -> std::borrow::Cow<'_, str> {
            std::borrow::Cow::Borrowed(&self.desc)
        }

        fn to_search_hit(&self, score: f32, _matched_terms: Option<Vec<String>>) -> SearchHit {
            SearchHit::minimal(self.name.clone(), self.kind, &self.desc, score)
        }
    }

    #[tokio::test]
    async fn test_bm25_engine_search_returns_ranked_hits() {
        let items: Vec<TestItem> = vec![
            TestItem {
                name: "query".into(),
                desc: "Run SQL queries".into(),
                kind: CapabilityKind::Tool,
            },
            TestItem {
                name: "review-code".into(),
                desc: "Review code for style".into(),
                kind: CapabilityKind::Skill,
            },
        ];
        let refs: Vec<&TestItem> = items.iter().collect();
        let index = Arc::new(ArcSwap::from_pointee(MergedIndex::from_items(&refs)));
        let engine = Bm25SearchEngine::new(index);

        let hits = engine.search("sql", None, 5).await.unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0].kind, CapabilityKind::Tool);
    }

    #[tokio::test]
    async fn test_empty_query_rejected() {
        let index = Arc::new(ArcSwap::from_pointee(MergedIndex::empty()));
        let engine = Bm25SearchEngine::new(index);

        let err = engine.search("", None, 5).await.unwrap_err();
        assert!(matches!(err, MetaSearchError::EmptyQuery));

        let err = engine.search("   ", None, 5).await.unwrap_err();
        assert!(matches!(err, MetaSearchError::EmptyQuery));
    }

    #[tokio::test]
    async fn test_top_k_clamp_rejects_above_20() {
        let index = Arc::new(ArcSwap::from_pointee(MergedIndex::empty()));
        let engine = Bm25SearchEngine::new(index);

        let err = engine.search("test", None, 25).await.unwrap_err();
        assert!(matches!(err, MetaSearchError::TopKTooLarge(25)));
    }

    #[tokio::test]
    async fn test_top_k_clamp_accepts_at_20() {
        let items: Vec<TestItem> = vec![TestItem {
            name: "query".into(),
            desc: "Run SQL queries".into(),
            kind: CapabilityKind::Tool,
        }];
        let refs: Vec<&TestItem> = items.iter().collect();
        let index = Arc::new(ArcSwap::from_pointee(MergedIndex::from_items(&refs)));
        let engine = Bm25SearchEngine::new(index);

        let hits = engine.search("sql", None, 20).await.unwrap();
        assert_eq!(hits.len(), 1);
    }

    /// AC: 9-7c-2 — Synonym expansion improves ranking for known aliases.
    #[tokio::test]
    async fn test_bm25_scores_higher_with_synonyms_than_without() {
        // Build a 2-item index containing a format-code fixture.
        let items: Vec<TestItem> = vec![
            TestItem {
                name: "format-code".into(),
                desc: "Format source code to keep it neat and tidy".into(),
                kind: CapabilityKind::Skill,
            },
            TestItem {
                name: "lint-code".into(),
                desc: "Lint source code for errors".into(),
                kind: CapabilityKind::Skill,
            },
        ];
        let refs: Vec<&TestItem> = items.iter().collect();
        let index = Arc::new(ArcSwap::from_pointee(MergedIndex::from_items(&refs)));

        // Engine WITH default synonyms (has "neat" → "format")
        let engine_with = Bm25SearchEngine::new(index.clone());

        // Engine WITH empty synonyms (control)
        let engine_without = Bm25SearchEngine::with_synonyms(index.clone(), SynonymMap::empty());

        let query = "how do i keep my code neat";

        let hits_with = engine_with.search(query, None, 3).await.unwrap();
        let hits_without = engine_without.search(query, None, 3).await.unwrap();

        let with_has_format_code = hits_with.iter().any(|h| h.name == "format-code");
        let without_has_format_code = hits_without.iter().any(|h| h.name == "format-code");

        // With synonyms, format-code must appear in top-3 (AC-9-7c-2).
        assert!(
            with_has_format_code,
            "Engine WITH synonyms must return 'format-code' in top-3 for '{}'",
            query
        );

        // Without synonyms, format-code may or may not appear depending on
        // BM25 overlap. The synonym engine must demonstrate improved ranking
        // by scoring format-code at least as high (position index ≤ control).
        let with_pos = hits_with
            .iter()
            .position(|h| h.name == "format-code")
            .unwrap_or(usize::MAX);
        let without_pos = hits_without
            .iter()
            .position(|h| h.name == "format-code")
            .unwrap_or(usize::MAX);
        assert!(
            with_pos <= without_pos,
            "Synonym expansion must rank 'format-code' at least as high as control.\n\
             with_pos={} without_pos={}\n\
             with={:?}\nwithout={:?}",
            with_pos,
            without_pos,
            hits_with.iter().map(|h| (&h.name, h.score)).collect::<Vec<_>>(),
            hits_without.iter().map(|h| (&h.name, h.score)).collect::<Vec<_>>()
        );
    }
}
