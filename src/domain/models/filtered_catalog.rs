use serde::{Deserialize, Serialize};

use crate::domain::models::tool_descriptor::ToolDescriptor;

/// The filtered tool catalog presented to `ToolExposurePort::render`.
///
/// Distinct from:
/// - `ToolSetPort::available_tools() -> Vec<ToolDefinition>` — the LLM-wire
///   shape (Anthropic / OpenAI / Ollama tool schemas).
/// - `ToolSetPort::describe() -> Vec<ToolDescriptor>` — the unfiltered domain
///   catalog from the `CapabilityRegistry::snapshot()` projection (Story 9.3b).
/// - `Vec<RegisteredCapability>` — the registry's working shape.
///
/// `FilteredCatalog` is the SHAPE OF THE RENDER INPUT: it has been filtered
/// by `ToolSetPort` policy (agent tool filter ∩ skill allowed_tools filter
/// from `event_loop.rs:7795-7813`) before reaching the exposure strategy.
///
/// # Phase A (Story 9.4)
///
/// Phase A construction is via `FilteredCatalog::from_tool_descriptors(tools)`
/// — no integration with `event_loop.rs:7783` request path is required
/// (the seam is exercised in unit/integration tests at the trait level;
/// Story 9.7 Phase B threads the seam through the request path when it
/// replaces `StaticFullExposure` with `MetaSearchExposure`).
///
/// # Phase B (Story 9.7 — DEFERRED)
///
/// Phase B's `MetaSearchExposure::render` consumes `FilteredCatalog` as the
/// BM25 indexing input via `IndexableItem` trait (`src/domain/ports/search/indexable.rs`,
/// new in Story 9.7). The element type stays `ToolDescriptor`; only the
/// consumer changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilteredCatalog {
    tools: Vec<ToolDescriptor>,
}

impl FilteredCatalog {
    /// Construct from an unfiltered descriptor list. The caller is responsible
    /// for applying `ToolSetPort` policy filters (agent_filter ∩ skill_filter)
    /// before constructing this value.
    pub fn from_tool_descriptors(tools: Vec<ToolDescriptor>) -> Self {
        Self { tools }
    }

    /// Empty filtered catalog. Used by Phase A unit tests + the eval-harness
    /// path (where the port itself is `None` per ADR v2.1 §W1, but a defensive
    /// `FilteredCatalog::empty()` constructor is useful for benches).
    pub fn empty() -> Self {
        Self { tools: Vec::new() }
    }

    /// Borrow the filtered descriptor list.
    pub fn tools(&self) -> &[ToolDescriptor] {
        &self.tools
    }

    /// Number of tools in the filtered catalog. Used by Story 9.5 telemetry
    /// (`tool_exposure.catalog_size` metric per ADR-09-01 v2.2 §Phase A
    /// metrics — narrowed from 5 metrics to 3 per PM-ack C3).
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// True when no tools are present.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::tool_descriptor::{ToolAnnotations, ToolDescriptor, ToolId};

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
    fn test_from_descriptors_round_trip() {
        let catalog = FilteredCatalog::from_tool_descriptors(vec![
            test_descriptor("Bash"),
            test_descriptor("Read"),
            test_descriptor("Write"),
        ]);
        assert_eq!(catalog.len(), 3);
        assert_eq!(catalog.tools().len(), 3);
    }

    #[test]
    fn test_empty_constructor() {
        let catalog = FilteredCatalog::empty();
        assert!(catalog.is_empty());
        assert_eq!(catalog.len(), 0);
    }

    #[test]
    fn test_serde_round_trip() {
        let catalog = FilteredCatalog::from_tool_descriptors(vec![ToolDescriptor {
            id: ToolId("builtin::Test".into()),
            name: "Test".into(),
            description: "Test tool".into(),
            input_schema: serde_json::json!({"type": "object"}),
            provider_id: "builtin".into(),
            annotations: ToolAnnotations {
                title: Some("Test Title".into()),
                read_only_hint: Some(true),
                destructive_hint: Some(false),
                idempotent_hint: None,
                open_world_hint: Some(true),
            },
        }]);
        let json = serde_json::to_string(&catalog).unwrap();
        let back: FilteredCatalog = serde_json::from_str(&json).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back.tools()[0].name, "Test");
        assert_eq!(back.tools()[0].annotations.title, Some("Test Title".into()));
    }
}
