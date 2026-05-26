//! Phase B `MetaSearchExposure` for the Skill side per ADR-09-02 v2
//! §Phased Implementation. When `[search] skills = "on"` AND
//! `[skill_exposure].kind = "meta-search"` AND the `meta-search` feature is
//! compiled in, this adapter substitutes the per-turn skill metadata block
//! with the single `search_skills` meta-tool entry. The LLM finds
//! skills via the meta-tool, not via prefix injection.

use async_trait::async_trait;
use std::sync::Arc;

use super::{
    SkillExposureError, SkillExposureKind, SkillExposurePayload, SkillRenderDiagnostics,
    SkillRenderOutcome,
};
use crate::domain::models::filtered_skill_catalog::FilteredSkillCatalog;
use crate::domain::models::provider_capabilities::ProviderCapabilities;
use crate::domain::models::skill_catalog_delta::SkillCatalogDelta;
use crate::domain::models::tool_descriptor::ToolDescriptor;
use crate::domain::ports::SkillExposurePort;
use crate::domain::ports::search::MetaSearchEngine;

pub struct MetaSearchExposure {
    engine: Arc<dyn MetaSearchEngine>,
    meta_tool_descriptor: ToolDescriptor,
}

impl MetaSearchExposure {
    pub fn new(engine: Arc<dyn MetaSearchEngine>) -> Self {
        Self {
            engine,
            meta_tool_descriptor:
                crate::adapters::tool_exposure::meta_search::build_search_skills_descriptor(),
        }
    }
}

#[async_trait]
impl SkillExposurePort for MetaSearchExposure {
    fn kind(&self) -> SkillExposureKind {
        SkillExposureKind::MetaSearch
    }

    async fn render(
        &self,
        catalog: &FilteredSkillCatalog,
        _provider: &ProviderCapabilities,
    ) -> Result<SkillRenderOutcome, SkillExposureError> {
        Ok(SkillRenderOutcome {
            payload: SkillExposurePayload::SearchStub(self.meta_tool_descriptor.clone()),
            diagnostics: SkillRenderDiagnostics {
                truncated: false,
                dropped_count: 0,
                reason: None,
                catalog_size: catalog.len(),
                definition_tokens_estimate: 200, // ~200 tok for the single meta-tool entry
            },
        })
    }

    async fn on_catalog_changed(
        &self,
        _delta: &SkillCatalogDelta,
    ) -> Result<(), SkillExposureError> {
        Ok(())
    }
}
