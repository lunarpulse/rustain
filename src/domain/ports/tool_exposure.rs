use async_trait::async_trait;

use crate::domain::models::filtered_catalog::FilteredCatalog;
use crate::domain::models::provider_capabilities::ProviderCapabilities;

/// Per-turn exposure strategy: translates the filtered tool catalog into the
/// payload shape the provider expects this turn.
///
/// **Sibling** to `ToolSetPort` (NOT a widening): `ToolSetPort` answers
/// *"which tools are the user allowed to call?"*; `ToolExposurePort` answers
/// *"of those, what does the model actually see this turn?"*. Keeping the
/// concerns separate is the difference between a clean kitchen and one where
/// the dishwasher also does the taxes.
///
/// # Phase A (Story 9.4)
///
/// `StaticFullExposure` is the only impl. `render()` returns
/// `ExposurePayload::Tools(catalog.clone())` — pure passthrough; zero behavior
/// change vs today's static-full injection. `on_catalog_changed()` is a no-op
/// `Ok(())` (catalog re-read on each `render()` call has no work to do until
/// a strategy with internal state ships).
///
/// # Phase B (Story 9.7 — DEFERRED per ADR-09-01 v2.2 §Phased Implementation
/// + ADR-09-02 v1 §Decision shared infrastructure)
///
/// `MetaSearchExposure` will render `ExposurePayload::MetaTool(search_capabilities)`,
/// reindexing via shared `MetaSearchEngine` on `on_catalog_changed(delta)`
/// with 250ms debounce (owned task per ADR-09-01 v2.2 §W3).
#[async_trait]
pub trait ToolExposurePort: Send + Sync {
    /// Stable identifier for logs, status panels (Story 8.5 widget), and
    /// capability matching at session handshake.
    fn kind(&self) -> crate::adapters::tool_exposure::ExposureKind;

    /// Build the per-turn tool payload for a given provider.
    ///
    /// Returns `RenderOutcome { payload, diagnostics }` so callers see
    /// fidelity loss (e.g., provider tool-count cap forced truncation) as a
    /// structured signal per ADR-09-01 v2.1 §W2 (restores LSP under provider
    /// caps — Gemini's 64-tool cap is the canonical example, Anthropic's
    /// effective cap is documented at https://docs.anthropic.com per ADR
    /// Revisit Trigger #4).
    ///
    /// Returns `ExposureError::Incompatible` if the strategy is incompatible
    /// with the provider (caught at handshake via `CapabilityMatrix`, never
    /// mid-turn per ADR §Capability matrix).
    async fn render(
        &self,
        catalog: &FilteredCatalog,
        provider: &ProviderCapabilities,
    ) -> Result<
        crate::adapters::tool_exposure::RenderOutcome,
        crate::adapters::tool_exposure::ExposureError,
    >;

    /// React to upstream catalog changes (MCP `notifications/tools/list_changed`,
    /// builtin re-registration, skill discovery).
    ///
    /// **Phase A:** all current impls return `Ok(())` no-op — the catalog is
    /// re-read on each `render()` call so the trait method has no work to do
    /// until a strategy with internal state (Phase B `MetaSearchExposure`)
    /// ships.
    ///
    /// **Phase B:** `MetaSearchExposure` reindexes BM25 in a background task
    /// per ADR-09-01 v2.2 §Catalog change handling. `CatalogObserverRegistry`
    /// at `src/infrastructure/composition/catalog_observer_registry.rs` (Story
    /// 9.7) owns the `tokio::sync::broadcast::Receiver<CatalogDelta>` and
    /// fan-out task.
    async fn on_catalog_changed(
        &self,
        delta: &crate::domain::models::catalog_delta::CatalogDelta,
    ) -> Result<(), crate::adapters::tool_exposure::ExposureError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    // Compile-only proof that the trait is object-safe.
    // We use a fake impl instead of StaticFullExposure (which lives in adapters)
    // to avoid violating the hexagonal dependency rule (domain must NOT import
    // from adapters per tests/conformance.rs::test_domain_no_adapter_or_infra_imports).
    struct FakeExposure;

    #[async_trait::async_trait]
    impl ToolExposurePort for FakeExposure {
        fn kind(&self) -> crate::adapters::tool_exposure::ExposureKind {
            crate::adapters::tool_exposure::ExposureKind::StaticFull
        }

        async fn render(
            &self,
            _catalog: &FilteredCatalog,
            _provider: &ProviderCapabilities,
        ) -> Result<
            crate::adapters::tool_exposure::RenderOutcome,
            crate::adapters::tool_exposure::ExposureError,
        > {
            unimplemented!()
        }

        async fn on_catalog_changed(
            &self,
            _delta: &crate::domain::models::catalog_delta::CatalogDelta,
        ) -> Result<(), crate::adapters::tool_exposure::ExposureError> {
            unimplemented!()
        }
    }

    #[test]
    fn test_trait_object_safe() {
        let _: Box<dyn ToolExposurePort> = Box::new(FakeExposure);
    }
}
