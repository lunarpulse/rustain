use std::borrow::Cow;

use crate::domain::models::doc_key::DocKey;
use crate::domain::models::search_hit::SearchHit;

/// Trait for any catalog item that can be indexed in the merged BM25 corpus
/// AND projected into a `SearchHit` for LLM consumption.
///
/// Implemented by:
/// - `ToolDescriptor` (`src/domain/models/tool_descriptor.rs`) — MCP +
///    builtin tools. UNCHANGED 9.3b contract; `terse` derives from
///    `description` only.
/// - `SkillMetadata` (`src/domain/models/skill_metadata.rs`) — skill L1
///    metadata. `terse` derives from `description` per `compute_terse`,
///    with optional `SkillDef.terse: Option<String>` frontmatter override
///    (additive Phase B).
///
/// # Why projection lives in domain (Amelia Round 4 §2)
///
/// If projection lived in the adapter (`MetaSearchExposure::render`) it would
/// couple each adapter to the kind-specific fields of the source type. If
/// projection lived in the engine (`Bm25SearchEngine`) the engine would have
/// to know about `ToolDescriptor.provider_id` and `SkillMetadata.source` —
/// breaking the corpus-agnostic engine surface. The domain trait method
/// keeps projection where it belongs: with the type, not with the consumer
/// or the engine.
pub trait IndexableItem: Send + Sync {
    /// Stable index key. Carries `kind` so two items with the same `name`
    /// across different kinds (e.g. a skill named "format" and a tool named
    /// "format") do not collide in the merged index.
    fn doc_key(&self) -> DocKey;

    /// Full searchable text — what BM25 tokenizes and indexes. Returns
    /// `Cow<'_, str>` to avoid per-index allocation on items that already
    /// own their text (per ADR-09-02 §Pinned file layout).
    ///
    /// Typical implementation: concatenate `name` + `description` (+ for
    /// skills: optional body excerpt up to a budget — Decision Gate 9.7.9).
    fn searchable_text(&self) -> Cow<'_, str>;

    /// Description-only text — used by `compute_terse` to derive the terse
    /// projection. Returns `Cow<'_, str>` to avoid per-call allocation.
    ///
    /// Per party-mode consensus 4/4 (Winston/Amelia/Mary/John) on
    /// 2026-05-24: AC-9-7-2 field semantics ("first-sentence of
    /// `description`") are normative. AC-9-7-4's code block was
    /// illustrative and accidentally used `searchable_text()` (which
    /// concatenates name + description). Feeding `searchable_text()`
    /// to `compute_terse` wastes the 120-byte budget on a duplicate
    /// of the name that the LLM already has in the `name` field.
    fn description(&self) -> Cow<'_, str>;

    /// Project this item into the LLM-facing `SearchHit` shape, given a
    /// score and optional matched-terms list.
    ///
    /// **NOTE:** This method is NOT used by the production search hot path.
    /// `MergedIndex::search` builds `SearchHit` directly from
    /// `CachedProjection` (populated at index time) for performance —
    /// avoiding per-query recomputation of `terse`. The trait method exists
    /// as the canonical projection specification and for testing/verification.
    /// Do NOT call this on the search hot path; use `CachedProjection` instead.
    ///
    /// **Schema lock-down:** the returned `SearchHit` MUST NOT carry
    /// `description`, `input_schema`, `parameters`, `version`, `category`,
    /// or `icon` — enforced at type level (`SearchHit` does not have those
    /// fields) AND at conformance-test level
    /// (`tests/conformance_search_hit_schema.rs` per AC-9-7-3).
    ///
    /// `terse` is computed at INDEX TIME (cached in `MergedIndex` per
    /// `DocKey`) — the production path uses `CachedProjection.terse` directly.
    /// This trait method recomputes `terse` from `description` and should only
    /// be used for testing/specification purposes.
    fn to_search_hit(&self, score: f32, matched_terms: Option<Vec<String>>) -> SearchHit;
}
