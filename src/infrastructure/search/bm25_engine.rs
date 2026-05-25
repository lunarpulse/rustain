//! BM25-backed `MetaSearchEngine` impl per ADR-09-02 v2 §Phased Implementation
//! Phase B. Wraps `MergedIndex` (the corpus-agnostic index over both kinds)
//! and applies `top_k` clamping + kind-filter post-rank predicate.

use async_trait::async_trait;
use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::domain::models::capability_kind::CapabilityKind;
use crate::domain::models::search_hit::SearchHit;
use crate::domain::ports::search::{MetaSearchEngine, MetaSearchError};
use crate::infrastructure::search::merged_index::MergedIndex;

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
}

impl Bm25SearchEngine {
    pub fn new(index: Arc<ArcSwap<MergedIndex>>) -> Self {
        Self { index }
    }

    /// Replace the index atomically. Called by the reindex owned task
    /// from `CatalogObserverRegistry` after a catalog delta + 250ms debounce.
    pub fn swap_index(&self, new_index: Arc<MergedIndex>) {
        self.index.store(new_index);
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

        let snapshot = self.index.load_full();
        // `snapshot.search` is sync (BM25 score computation is CPU-bound,
        // no I/O); we do not `.await` inside it.
        let hits = snapshot.search(query, kind_filter, clamped_k);
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

        fn to_search_hit(&self, score: f32, matched_terms: Option<Vec<String>>) -> SearchHit {
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
}
