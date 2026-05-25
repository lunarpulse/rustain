//! `StaticFullExposure` — Phase A opt-in codex-parity fallback per ADR-09-02
//! §Decision.
//!
//! Renders FULL SKILL.md bodies inline in the per-turn prefix (codex parity).
//! NOT the default per ADR-09-02 §Decision — users opt in deliberately via
//! `[skill_exposure].kind = "static-full"`.

use async_trait::async_trait;
use std::sync::Arc;

use super::{
    SkillExposureError, SkillExposureKind, SkillExposurePayload, SkillFullEntry,
    SkillRenderDiagnostics, SkillRenderOutcome,
};
use crate::domain::models::filtered_skill_catalog::FilteredSkillCatalog;
use crate::domain::models::provider_capabilities::ProviderCapabilities;
use crate::domain::models::skill_catalog_delta::SkillCatalogDelta;
use crate::domain::ports::SkillExposurePort;
use crate::infrastructure::skill_cache::SkillCache;

/// Phase A opt-in fallback exposure: full SKILL.md bodies inline.
pub struct StaticFullExposure {
    cache: Arc<SkillCache>,
}

impl StaticFullExposure {
    pub fn new(cache: Arc<SkillCache>) -> Self {
        Self { cache }
    }
}

#[async_trait]
impl SkillExposurePort for StaticFullExposure {
    fn kind(&self) -> SkillExposureKind {
        SkillExposureKind::StaticFull
    }

    async fn render(
        &self,
        catalog: &FilteredSkillCatalog,
        _provider: &ProviderCapabilities,
    ) -> Result<SkillRenderOutcome, SkillExposureError> {
        let mut bodies = Vec::with_capacity(catalog.len());
        let mut tokens_estimate = 0usize;

        for meta in catalog.metadata() {
            match self.cache.body(&meta.name).await {
                Ok(body) => {
                    tokens_estimate += body.len() / 4 + meta.estimated_tokens();
                    bodies.push(SkillFullEntry {
                        metadata: meta.clone(),
                        body,
                    });
                }
                Err(e) => {
                    tracing::warn!(
                        skill = meta.name,
                        error = %e,
                        "StaticFullExposure: failed to load body from cache — skill omitted from render"
                    );
                }
            }
        }

        let dropped_count = catalog.len() - bodies.len();
        let reason = if dropped_count > 0 {
            Some(format!(
                "{} skill bod(y/ies) failed to load from cache",
                dropped_count
            ))
        } else {
            None
        };

        Ok(SkillRenderOutcome {
            payload: SkillExposurePayload::Bodies(bodies),
            diagnostics: SkillRenderDiagnostics {
                truncated: false,
                dropped_count,
                reason,
                catalog_size: catalog.len(),
                definition_tokens_estimate: tokens_estimate,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::SkillSource;
    use crate::domain::models::skill_metadata::SkillMetadata;
    use crate::infrastructure::skill_cache::SkillCache;

    #[test]
    fn test_kind_returns_static_full() {
        let cache = Arc::new(SkillCache::new_in_memory());
        assert_eq!(
            StaticFullExposure::new(cache).kind(),
            SkillExposureKind::StaticFull
        );
    }

    #[tokio::test]
    async fn test_render_skips_missing_bodies() {
        let cache = Arc::new(SkillCache::new_in_memory());
        cache
            .insert(
                "present",
                SkillMetadata {
                    name: "present".into(),
                    description: "Present skill when needed".into(),
                    source: SkillSource::WorkspaceAgents,
                    terse: None,
                },
                "body content".into(),
            )
            .await;

        let exposure = StaticFullExposure::new(cache);
        let catalog = FilteredSkillCatalog::from_metadata(vec![
            SkillMetadata {
                name: "present".into(),
                description: "Present skill when needed".into(),
                source: SkillSource::WorkspaceAgents,
                terse: None,
            },
            SkillMetadata {
                name: "missing".into(),
                description: "Missing skill when needed".into(),
                source: SkillSource::WorkspaceAgents,
                terse: None,
            },
        ]);

        let outcome = exposure
            .render(
                &catalog,
                &crate::domain::models::provider_capabilities::ProviderCapabilities {
                    supports_streaming: true,
                    supports_list_changed: true,
                    supports_native_retrieval: None,
                    max_tool_count: None,
                    transport_kind:
                        crate::domain::models::provider_capabilities::TransportKind::Stdio,
                },
            )
            .await
            .unwrap();

        match outcome.payload {
            SkillExposurePayload::Bodies(bodies) => {
                assert_eq!(bodies.len(), 1);
                assert_eq!(bodies[0].metadata.name, "present");
            }
            _ => panic!("StaticFullExposure must emit Bodies variant"),
        }
        assert_eq!(outcome.diagnostics.dropped_count, 1);
        assert!(outcome.diagnostics.reason.is_some());
    }
}
