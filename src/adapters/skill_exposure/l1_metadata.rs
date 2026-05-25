//! `L1MetadataExposure` — the Phase A DEFAULT per ADR-09-02 §Decision.
//!
//! Renders only `{name, description}` per skill (~100 tok each) into the
//! per-turn prefix. Body fetch is the LLM's job via the `skill_view` meta-tool
//! (AC-9-6-5). Spec-aligned with the Anthropic Skills progressive-disclosure
//! mandate; ecosystem-aligned with 3-of-4 surveyed harnesses (gemini-cli,
//! hermes-agent, opencode).
//!
//! # Phase A
//!
//! `render(catalog, _provider)` returns `SkillExposurePayload::Metadata(
//! catalog.metadata().to_vec())` with diagnostics carrying the L1
//! definition-token estimate.
//!
//! # Asymmetric default with Tools track
//!
//! `ToolExposurePort` (Story 9.4) defaults to `StaticFullExposure` — Tools
//! ecosystem evidence is partial. `SkillExposurePort` (this story) defaults
//! to `L1MetadataExposure` — Skills ecosystem evidence saturates. The
//! asymmetry is BY DESIGN per ADR-09-02 §Decision.

use async_trait::async_trait;
use std::sync::Arc;

use super::{
    SkillExposureError, SkillExposureKind, SkillExposurePayload, SkillRenderDiagnostics,
    SkillRenderOutcome,
};
use crate::domain::models::filtered_skill_catalog::FilteredSkillCatalog;
use crate::domain::models::provider_capabilities::ProviderCapabilities;
use crate::domain::models::skill_catalog_delta::SkillCatalogDelta;
use crate::domain::ports::SkillExposurePort;
use crate::infrastructure::skill_cache::SkillCache;

/// Phase A default exposure: L1 metadata only (~100 tok/skill prefix).
pub struct L1MetadataExposure {
    cache: Arc<SkillCache>,
}

impl L1MetadataExposure {
    pub fn new(cache: Arc<SkillCache>) -> Self {
        Self { cache }
    }
}

#[async_trait]
impl SkillExposurePort for L1MetadataExposure {
    fn kind(&self) -> SkillExposureKind {
        SkillExposureKind::L1Metadata
    }

    async fn render(
        &self,
        catalog: &FilteredSkillCatalog,
        _provider: &ProviderCapabilities,
    ) -> Result<SkillRenderOutcome, SkillExposureError> {
        let metadata = catalog.metadata().to_vec();
        let definition_tokens_estimate: usize = metadata.iter().map(|m| m.estimated_tokens()).sum();
        let catalog_size = metadata.len();

        Ok(SkillRenderOutcome {
            payload: SkillExposurePayload::Metadata(metadata),
            diagnostics: SkillRenderDiagnostics::clean(catalog_size, definition_tokens_estimate),
        })
    }

    async fn on_catalog_changed(
        &self,
        _delta: &SkillCatalogDelta,
    ) -> Result<(), SkillExposureError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::SkillSource;
    use crate::domain::models::provider_capabilities::TransportKind;
    use crate::domain::models::skill_metadata::SkillMetadata;
    use crate::infrastructure::skill_cache::SkillCache;

    fn test_caps() -> ProviderCapabilities {
        ProviderCapabilities {
            supports_streaming: true,
            supports_list_changed: true,
            supports_native_retrieval: None,
            max_tool_count: None,
            transport_kind: TransportKind::Stdio,
        }
    }

    fn test_metadata(name: &str, description: &str) -> SkillMetadata {
        SkillMetadata {
            name: name.into(),
            description: description.into(),
            source: SkillSource::WorkspaceAgents,
            terse: None,
        }
    }

    fn test_cache() -> Arc<SkillCache> {
        Arc::new(SkillCache::new_in_memory())
    }

    #[test]
    fn test_kind_returns_l1_metadata() {
        assert_eq!(
            L1MetadataExposure::new(test_cache()).kind(),
            SkillExposureKind::L1Metadata
        );
    }

    #[tokio::test]
    async fn test_render_projects_metadata_only() {
        let exposure = L1MetadataExposure::new(test_cache());
        let catalog = FilteredSkillCatalog::from_metadata(vec![
            test_metadata("review-code", "Reviews code when the user requests review"),
            test_metadata(
                "write-docs",
                "Writes documentation when the user requests docs",
            ),
        ]);
        let outcome = exposure.render(&catalog, &test_caps()).await.unwrap();
        match outcome.payload {
            SkillExposurePayload::Metadata(metas) => {
                assert_eq!(metas.len(), 2);
                assert_eq!(metas[0].name, "review-code");
                assert_eq!(metas[1].name, "write-docs");
            }
            _ => panic!("L1MetadataExposure must always emit Metadata variant"),
        }
        assert_eq!(outcome.diagnostics.catalog_size, 2);
        assert!(outcome.diagnostics.definition_tokens_estimate > 0);
        assert!(!outcome.diagnostics.truncated);
        assert_eq!(outcome.diagnostics.dropped_count, 0);
    }

    #[tokio::test]
    async fn test_render_empty_catalog_emits_empty_metadata() {
        let exposure = L1MetadataExposure::new(test_cache());
        let outcome = exposure
            .render(&FilteredSkillCatalog::empty(), &test_caps())
            .await
            .unwrap();
        match outcome.payload {
            SkillExposurePayload::Metadata(metas) => assert!(metas.is_empty()),
            _ => unreachable!(),
        }
        assert_eq!(outcome.diagnostics.catalog_size, 0);
        assert_eq!(outcome.diagnostics.definition_tokens_estimate, 0);
    }

    #[tokio::test]
    async fn test_on_catalog_changed_is_ok_no_op() {
        let exposure = L1MetadataExposure::new(test_cache());
        let delta = SkillCatalogDelta::empty(1);
        let result = exposure.on_catalog_changed(&delta).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_definition_tokens_estimate_scales_with_catalog_size() {
        let exposure = L1MetadataExposure::new(test_cache());
        let small_catalog = FilteredSkillCatalog::from_metadata(vec![test_metadata(
            "a",
            "Skill A is invoked when condition X holds",
        )]);
        let big_catalog = FilteredSkillCatalog::from_metadata(vec![
            test_metadata("a", "Skill A is invoked when condition X holds"),
            test_metadata("b", "Skill B is invoked when condition Y holds"),
            test_metadata("c", "Skill C is invoked when condition Z holds"),
        ]);
        let small = exposure.render(&small_catalog, &test_caps()).await.unwrap();
        let big = exposure.render(&big_catalog, &test_caps()).await.unwrap();
        assert!(
            big.diagnostics.definition_tokens_estimate
                > small.diagnostics.definition_tokens_estimate
        );
    }
}
