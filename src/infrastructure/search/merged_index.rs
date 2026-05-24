//! `MergedIndex` — ONE BM25 index over BOTH tools and skills, per Amelia
//! non-negotiable IDF correctness argument (ADR-09-02 §Why one merged index).
//!
//! # Caching layer
//!
//! `cached: BTreeMap<DocKey, CachedProjection>` is consulted by the
//! `SearchHit` projection path. The `compute_terse` cost is paid ONCE at
//! index time and amortized across every query that hits the same `DocKey`.

use std::collections::BTreeMap;

use crate::domain::models::capability_kind::CapabilityKind;
use crate::domain::models::doc_key::DocKey;
use crate::domain::models::search_hit::SearchHit;
use crate::domain::ports::search::IndexableItem;

/// Cached projection per `DocKey` — `terse` + `kind` + `provider`.
///
/// Built at INDEX TIME via `compute_terse(item.searchable_text(), item.doc_key().name)`.
/// Consumed at PROJECTION TIME via `IndexableItem::to_search_hit` which looks
/// up the cached `CachedProjection` rather than recomputing.
#[derive(Debug, Clone)]
pub struct CachedProjection {
    pub terse: String,
    pub kind: CapabilityKind,
    pub provider: Option<String>,
}

/// The merged BM25 index over both kinds. Built once per catalog snapshot
/// and stored behind `ArcSwap<MergedIndex>` for lock-free hot-swap on
/// reindex.
pub struct MergedIndex {
    bm25: bm25::SearchEngine<DocKey>,
    cached: BTreeMap<DocKey, CachedProjection>,
}

impl MergedIndex {
    /// Build a fresh index from the given indexable items.
    pub fn from_items<I: IndexableItem + ?Sized>(items: &[&I]) -> Self {
        let mut cached: BTreeMap<DocKey, CachedProjection> = BTreeMap::new();
        let documents: Vec<bm25::Document<DocKey>> = items
            .iter()
            .map(|item| {
                let dk = item.doc_key();
                let text = item.searchable_text().into_owned();
                let desc = item.description().into_owned();
                cached.insert(
                    dk.clone(),
                    CachedProjection {
                        terse: crate::domain::services::meta_search::compute_terse(&desc,
                            &dk.name,
                        ),
                        kind: dk.kind,
                        provider: None, // populated by caller after collision detection
                    },
                );
                bm25::Document::new(dk, text)
            })
            .collect();
        let bm25 = bm25::SearchEngineBuilder::with_documents(
            bm25::Language::English,
            documents,
        )
        .build();
        Self { bm25, cached }
    }

    /// Empty index (no documents). Used for the first warm-up before
    /// `populate_from_registry` completes.
    pub fn empty() -> Self {
        let bm25 = bm25::SearchEngineBuilder::<DocKey>::with_avgdl(1.0).build();
        Self {
            bm25,
            cached: BTreeMap::new(),
        }
    }

    /// Build a fresh index from the given indexable items, with optional
    /// per-item terse overrides. When `overrides` contains a `DocKey` that
    /// matches an item, the override value is used verbatim instead of
    /// `compute_terse`. This supports `SkillDef.terse: Option<String>`
    /// frontmatter overrides per AC-9-7-9.
    pub fn from_items_with_overrides<I: IndexableItem + ?Sized>(
        items: &[&I],
        terse_overrides: &BTreeMap<DocKey, String>,
    ) -> Self {
        let mut cached: BTreeMap<DocKey, CachedProjection> = BTreeMap::new();
        let documents: Vec<bm25::Document<DocKey>> = items
            .iter()
            .map(|item| {
                let dk = item.doc_key();
                let text = item.searchable_text().into_owned();
                let desc = item.description().into_owned();
                let terse = terse_overrides
                    .get(&dk)
                    .cloned()
                    .unwrap_or_else(|| {
                        crate::domain::services::meta_search::compute_terse(&desc, &dk.name)
                    });
                cached.insert(
                    dk.clone(),
                    CachedProjection {
                        terse,
                        kind: dk.kind,
                        provider: None,
                    },
                );
                bm25::Document::new(dk, text)
            })
            .collect();
        let bm25 = bm25::SearchEngineBuilder::with_documents(
            bm25::Language::English,
            documents,
        )
        .build();
        Self { bm25, cached }
    }

    /// Search synchronously — BM25 score computation is CPU-bound.
    /// Returns at most `top_k` hits, ordered by descending score. Ties
    /// resolve by `DocKey` lexicographic ordering (Phase B AC-9-7-10).
    pub fn search(
        &self,
        query: &str,
        kind_filter: Option<CapabilityKind>,
        top_k: usize,
    ) -> Vec<SearchHit> {
        let raw = self.bm25.search(query, Some(top_k * 3)); // over-query 3x to absorb kind-filter drop-rate
        let mut hits: Vec<(DocKey, f32, CachedProjection)> = raw
            .into_iter()
            .filter_map(|result| {
                let dk = result.document.id;
                let score = result.score;
                let proj = self.cached.get(&dk)?.clone();
                if let Some(kf) = kind_filter {
                    if proj.kind != kf {
                        return None;
                    }
                }
                Some((dk, score, proj))
            })
            .collect();
        // Stable sort with DocKey tie-break.
        hits.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        hits.truncate(top_k);
        hits.into_iter()
            .map(|(dk, score, proj)| SearchHit {
                name: dk.name,
                kind: proj.kind,
                terse: proj.terse,
                score,
                provider: proj.provider,
                matched_terms: None, // populated by debug-mode callers (Story 9.8)
            })
            .collect()
    }

    pub fn len(&self) -> usize {
        self.cached.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cached.is_empty()
    }

    /// Populate `provider` field on entries that share the same name across
    /// different providers. This is called after `from_items` to disambiguate
    /// colliding names.
    pub fn populate_provider_disambiguation(
        &mut self,
        tool_provider: &dyn Fn(&str) -> Option<String>,
        skill_source: &dyn Fn(&str) -> Option<String>,
    ) {
        // Build a map of name -> count
        let mut name_counts: std::collections::HashMap<(CapabilityKind, String), usize> =
            std::collections::HashMap::new();
        for dk in self.cached.keys() {
            *name_counts.entry((dk.kind, dk.name.clone())).or_insert(0) += 1;
        }

        // For entries with count > 1, populate provider
        for (dk, proj) in self.cached.iter_mut() {
            if name_counts.get(&(dk.kind, dk.name.clone())).copied().unwrap_or(0) > 1 {
                match dk.kind {
                    CapabilityKind::Tool => {
                        if let Some(pid) = tool_provider(&dk.name) {
                            proj.provider = Some(pid);
                        }
                    }
                    CapabilityKind::Skill => {
                        if let Some(src) = skill_source(&dk.name) {
                            proj.provider = Some(src);
                        }
                    }
                }
            }
        }
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

    #[test]
    fn test_merged_index_empty_returns_no_hits() {
        let index = MergedIndex::empty();
        let hits = index.search("test", None, 5);
        assert!(hits.is_empty());
    }

    #[test]
    fn test_merged_index_search_filters_by_kind() {
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
        let index = MergedIndex::from_items(&refs);

        let tool_hits = index.search("query", Some(CapabilityKind::Tool), 5);
        assert_eq!(tool_hits.len(), 1);
        assert_eq!(tool_hits[0].kind, CapabilityKind::Tool);

        let skill_hits = index.search("review", Some(CapabilityKind::Skill), 5);
        assert_eq!(skill_hits.len(), 1);
        assert_eq!(skill_hits[0].kind, CapabilityKind::Skill);
    }

    #[test]
    fn test_from_items_with_overrides_uses_override_verbatim() {
        let items: Vec<TestItem> = vec![
            TestItem {
                name: "review-code".into(),
                desc: "Review code for style violations and formatting issues. Very long description.".into(),
                kind: CapabilityKind::Skill,
            },
            TestItem {
                name: "query".into(),
                desc: "Run SQL queries against databases.".into(),
                kind: CapabilityKind::Tool,
            },
        ];
        let refs: Vec<&TestItem> = items.iter().collect();
        let mut overrides = BTreeMap::new();
        overrides.insert(
            DocKey::new(CapabilityKind::Skill, "review-code"),
            "Custom terse override".into(),
        );
        let index = MergedIndex::from_items_with_overrides(&refs, &overrides);
        let proj = index.cached.get(&DocKey::new(CapabilityKind::Skill, "review-code")).unwrap();
        assert_eq!(proj.terse, "Custom terse override");

        let tool_proj = index.cached.get(&DocKey::new(CapabilityKind::Tool, "query")).unwrap();
        assert_eq!(tool_proj.terse, "Run SQL queries against databases.");
    }

    #[test]
    fn test_rebuild_from_scratch_is_correct() {
        let items: Vec<TestItem> = vec![
            TestItem {
                name: "tool-a".into(),
                desc: "First tool description".into(),
                kind: CapabilityKind::Tool,
            },
            TestItem {
                name: "skill-b".into(),
                desc: "First skill description".into(),
                kind: CapabilityKind::Skill,
            },
        ];
        let refs: Vec<&TestItem> = items.iter().collect();
        let index = MergedIndex::from_items(&refs);
        assert_eq!(index.len(), 2);
        let hits = index.search("tool", None, 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "tool-a");
    }
}
