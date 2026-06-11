//! `StaticFullExposure` — the Phase A default. Behavior-identical passthrough
//! that serializes the full filtered catalog every turn.
//!
//! # Phase A (Story 9.4)
//!
//! Pure passthrough: `render(catalog, _provider)` returns
//! `ExposurePayload::Tools(catalog.tools().to_vec())` with clean diagnostics.
//! `on_catalog_changed(_delta)` is `Ok(())` no-op — the next `render()` call
//! re-reads the catalog, so the trait method has no work to do until a
//! strategy with internal state ships (Phase B's `MetaSearchExposure`).
//!
//! Zero behavior change for users vs today's static-full injection at
//! `event_loop.rs:7783-7877` — this is the LOAD-BEARING property that lets
//! Phase A ship safely per ADR-09-01 v2.2 §Phased Implementation.

use async_trait::async_trait;

use super::{ExposureError, ExposureKind, ExposurePayload, RenderDiagnostics, RenderOutcome};
use crate::domain::models::catalog_delta::CatalogDelta;
use crate::domain::models::filtered_catalog::FilteredCatalog;
use crate::domain::models::provider_capabilities::ProviderCapabilities;
use crate::domain::ports::ToolExposurePort;

/// Phase A default exposure: full filtered catalog every turn.
#[derive(Debug, Default)]
pub struct StaticFullExposure;

impl StaticFullExposure {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ToolExposurePort for StaticFullExposure {
    fn kind(&self) -> ExposureKind {
        ExposureKind::StaticFull
    }

    async fn render(
        &self,
        catalog: &FilteredCatalog,
        _provider: &ProviderCapabilities,
    ) -> Result<RenderOutcome, ExposureError> {
        // Phase A: pure passthrough. No truncation (provider caps surface in
        // Phase B when concrete capability matrices replace the
        // `Capability::Full` stub per ADR-09-01 v2.2 §Per-provider wiring).
        // No incompatibility (the `CapabilityMatrix` stub returns
        // `Capability::Full` for every provider — Phase B differentiates).
        Ok(RenderOutcome {
            payload: ExposurePayload::Tools(catalog.tools().to_vec()),
            diagnostics: RenderDiagnostics::clean(),
        })
    }

    async fn on_catalog_changed(&self, _delta: &CatalogDelta) -> Result<(), ExposureError> {
        // Phase A: no-op. The catalog is re-read on each render() call
        // (no internal state), so the trait method has no work to do until a
        // strategy with internal state (Phase B's MetaSearchExposure) ships.
        // See ADR-09-01 v2.2 §Catalog change handling Phase A note.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::capability_id::CapabilityId;
    use crate::domain::models::provider_capabilities::TransportKind;
    use crate::domain::models::tool_descriptor::{ToolAnnotations, ToolDescriptor, ToolId};

    fn test_caps() -> ProviderCapabilities {
        ProviderCapabilities {
            supports_streaming: true,
            supports_list_changed: true,
            supports_native_retrieval: None,
            max_tool_count: None,
            transport_kind: TransportKind::Stdio,
        }
    }

    fn test_descriptor(name: &str) -> ToolDescriptor {
        ToolDescriptor {
            id: ToolId(format!("builtin::{name}")),
            name: name.into(),
            description: format!("{name} description"),
            input_schema: serde_json::json!({"type": "object"}),
            provider_id: "builtin".into(),
            annotations: ToolAnnotations::default(),
        }
    }

    #[test]
    fn test_kind_returns_static_full() {
        assert_eq!(StaticFullExposure::new().kind(), ExposureKind::StaticFull);
    }

    #[tokio::test]
    async fn test_render_passthrough_preserves_all_tools() {
        let exposure = StaticFullExposure::new();
        let catalog = FilteredCatalog::from_tool_descriptors(vec![
            test_descriptor("Bash"),
            test_descriptor("Read"),
            test_descriptor("Write"),
        ]);
        let outcome = exposure
            .render(&catalog, &test_caps())
            .await
            .expect("phase A passthrough never fails");
        match outcome.payload {
            ExposurePayload::Tools(tools) => {
                assert_eq!(tools.len(), 3);
                assert_eq!(tools[0].name, "Bash");
                assert_eq!(tools[1].name, "Read");
                assert_eq!(tools[2].name, "Write");
            }
            ExposurePayload::MetaTool(_) => panic!("StaticFullExposure must never emit MetaTool"),
        }
        assert_eq!(outcome.diagnostics, RenderDiagnostics::clean());
    }

    #[tokio::test]
    async fn test_render_empty_catalog_returns_empty_tools() {
        let exposure = StaticFullExposure::new();
        let outcome = exposure
            .render(&FilteredCatalog::empty(), &test_caps())
            .await
            .unwrap();
        match outcome.payload {
            ExposurePayload::Tools(tools) => assert!(tools.is_empty()),
            ExposurePayload::MetaTool(_) => unreachable!(),
        }
    }

    #[tokio::test]
    async fn test_on_catalog_changed_is_ok_no_op() {
        let exposure = StaticFullExposure::new();
        let delta = CatalogDelta::empty(1);
        let result = exposure.on_catalog_changed(&delta).await;
        assert!(result.is_ok());
    }
}
