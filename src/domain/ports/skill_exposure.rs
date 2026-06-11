use async_trait::async_trait;

use crate::domain::models::filtered_skill_catalog::FilteredSkillCatalog;
use crate::domain::models::provider_capabilities::ProviderCapabilities;
use crate::domain::models::skill_catalog_delta::SkillCatalogDelta;

/// Per-turn skill exposure strategy: translates the filtered skill catalog
/// into the payload shape the provider expects this turn.
///
/// **Sibling** to `SkillsProvider` / `ToolExposurePort` (NOT a widening, NOT a
/// child): `SkillsProvider` (9.3b) answers *"which skills are installed and
/// allowed?"*; `SkillExposurePort` answers *"of those, what does the model
/// actually see this turn — L1 metadata, full bodies, or a search stub?"*.
///
/// # Phase A (Story 9.6)
///
/// Two impls ship:
/// - `L1MetadataExposure` (DEFAULT — ~100 tok/skill prefix, body fetched
///    on-demand via `skill_view` meta-tool). The default per ADR-09-02
///    §Decision based on 7-signal ecosystem evidence saturation.
/// - `StaticFullExposure` (opt-in codex-parity fallback — full SKILL.md bodies
///    inline). Opt-in via `[skill_exposure].kind = "static-full"`.
///
/// `on_catalog_changed()` is a no-op `Ok(())` for both Phase A impls.
///
/// # Phase B (Story 9.7 — DEFERRED per ADR-09-02 v1 §Phased Implementation)
///
/// `MetaSearchExposure` will render
/// `SkillExposurePayload::SearchStub(search_skills)` — the skill-side
/// search door that `SkillExposurePort::MetaSearchExposure`
/// renders — backed by the shared `MetaSearchEngine` trait + merged BM25
/// corpus. Reindexing on `on_catalog_changed(delta)` with 250ms debounce.
#[async_trait]
pub trait SkillExposurePort: Send + Sync {
    /// Stable identifier for logs, telemetry (Story 9.5 `skill_exposure.kind`
    /// metric), and status panels (Story 8.5 widget).
    fn kind(&self) -> crate::adapters::skill_exposure::SkillExposureKind;

    /// Build the per-turn skill payload for a given provider.
    ///
    /// Returns `SkillRenderOutcome { payload, diagnostics }` so callers see
    /// fidelity loss (truncation for prefix budget, frontmatter parse skips)
    /// as a structured signal per ADR-09-01 v2.1 §W2 LSP-restoration pattern
    /// (inherited from sibling port).
    ///
    /// Returns `SkillExposureError::Incompatible` if the strategy is
    /// incompatible with the provider (caught at handshake, never mid-turn).
    async fn render(
        &self,
        catalog: &FilteredSkillCatalog,
        provider: &ProviderCapabilities,
    ) -> Result<
        crate::adapters::skill_exposure::SkillRenderOutcome,
        crate::adapters::skill_exposure::SkillExposureError,
    >;

    /// React to upstream catalog changes (filesystem watcher, profile switch
    /// via Epic 8 community-profile install, manual skill add/remove).
    ///
    /// **Phase A:** both impls return `Ok(())` no-op — the catalog is re-read
    /// on each `render()` call via the two-layer cache.
    ///
    /// **Phase B:** `MetaSearchExposure` reindexes the merged BM25 corpus in
    /// a background task.
    async fn on_catalog_changed(
        &self,
        delta: &SkillCatalogDelta,
    ) -> Result<(), crate::adapters::skill_exposure::SkillExposureError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    // Compile-only proof that the trait is object-safe.
    // Uses a fake impl to avoid violating the hexagonal dependency rule
    // (domain must NOT import from adapters per conformance tests).
    struct FakeSkillExposure;

    #[async_trait::async_trait]
    impl SkillExposurePort for FakeSkillExposure {
        fn kind(&self) -> crate::adapters::skill_exposure::SkillExposureKind {
            crate::adapters::skill_exposure::SkillExposureKind::L1Metadata
        }

        async fn render(
            &self,
            _catalog: &FilteredSkillCatalog,
            _provider: &ProviderCapabilities,
        ) -> Result<
            crate::adapters::skill_exposure::SkillRenderOutcome,
            crate::adapters::skill_exposure::SkillExposureError,
        > {
            unimplemented!()
        }

        async fn on_catalog_changed(
            &self,
            _delta: &SkillCatalogDelta,
        ) -> Result<(), crate::adapters::skill_exposure::SkillExposureError> {
            unimplemented!()
        }
    }

    #[test]
    fn test_trait_object_safe() {
        let _: Box<dyn SkillExposurePort> = Box::new(FakeSkillExposure);
    }
}
