use async_trait::async_trait;

use crate::domain::models::doc_key::DocKey;
use crate::domain::models::search_hit::SearchHit;

/// Corpus-agnostic search engine — knows `DocKey` only.
///
/// The engine ranks documents by BM25 (or future swap impl such as
/// `AnthropicHostedSearchEngine` when Anthropic ships `skill_search_tool_*`
/// beta per ADR-09-02 §Why one merged index, vendor-lock-in hedge).
///
/// **Projection lives in domain, not in the engine.** The engine returns
/// `(DocKey, score, Option<matched_terms>)` tuples to the caller via the
/// `search` method's return type. The caller (`MetaSearchExposure::render`
/// or the `search_skills` / `search_tools` builtin tools) consults the `MergedIndex`'s
/// `IndexableItem` registry to project each `DocKey` into a `SearchHit`
/// via `IndexableItem::to_search_hit(score, matched)` (Amelia Round 4 §2).
///
/// # Vendor-lock-in hedge (ADR-09-02 §Why one merged index)
///
/// If Anthropic ships `skill_search_tool_bm25_*` beta, the swap is implementing
/// one trait method against the vendor primitive — ~1 dev-day, no caller
/// changes. The current Phase B impl is `Bm25SearchEngine` at
/// `src/infrastructure/search/bm25_engine.rs`.
#[async_trait]
pub trait MetaSearchEngine: Send + Sync {
    /// Run a search query and return ranked results.
    ///
    /// `top_k` MUST be clamped to `[1, 20]` by the caller (the
    /// `search_skills` / `search_tools` builtin tools enforce the clamp + hard-rejects
    /// `top_k > 20` as prompt-injection defense per ADR-09-02 v2 §LLM-Only
    /// Payload).
    ///
    /// `kind_filter`, when `Some`, restricts results to that capability kind
    /// AFTER ranking (post-rank predicate, not pre-rank routing — per
    /// ADR-09-02 §LLM-facing surface). When `None`, all kinds are eligible.
    ///
    /// Returns at most `top_k` `SearchHit`s, ordered by descending score.
    /// Ties resolve by `DocKey` lexicographic ordering (deterministic per
    /// AC-9-7-10 conformance test).
    async fn search(
        &self,
        query: &str,
        kind_filter: Option<crate::domain::models::capability_kind::CapabilityKind>,
        top_k: usize,
    ) -> Result<Vec<SearchHit>, MetaSearchError>;
}

#[derive(Debug, thiserror::Error)]
pub enum MetaSearchError {
    #[error("empty query rejected")]
    EmptyQuery,
    #[error("top_k {0} exceeds maximum 20 (prompt-injection defense)")]
    TopKTooLarge(usize),
    #[error("index not ready (warming up): {0}")]
    IndexNotReady(String),
    #[error("internal search error: {0}")]
    Internal(String),
}
