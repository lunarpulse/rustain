//! Phase B `MetaSearchExposure` for the Tool side per ADR-09-02 v2
//! §Phased Implementation. When the user opts in via `[search] tools = "on"`
//! AND `[tools].exposure = "meta-search"` AND the `meta-search` feature is
//! compiled in, this adapter substitutes the full N-tool catalog with the
//! single `search_capabilities` meta-tool entry per turn.

use async_trait::async_trait;
use std::sync::Arc;

use super::{ExposureError, ExposureKind, ExposurePayload, RenderDiagnostics, RenderOutcome};
use crate::domain::models::filtered_catalog::FilteredCatalog;
use crate::domain::models::provider_capabilities::ProviderCapabilities;
use crate::domain::models::tool_descriptor::ToolDescriptor;
use crate::domain::ports::ToolExposurePort;
use crate::domain::ports::search::MetaSearchEngine;

pub struct MetaSearchExposure {
    engine: Arc<dyn MetaSearchEngine>,
    /// Cached descriptor for the `search_capabilities` meta-tool. Built
    /// once at construction time (the schema is static — `query` + `kind?` +
    /// `top_k?` parameters) so render() does not pay JSON-schema-build cost
    /// every turn.
    meta_tool_descriptor: ToolDescriptor,
}

impl MetaSearchExposure {
    pub fn new(engine: Arc<dyn MetaSearchEngine>) -> Self {
        Self {
            engine,
            meta_tool_descriptor: build_search_capabilities_descriptor(),
        }
    }
}

#[async_trait]
impl ToolExposurePort for MetaSearchExposure {
    fn kind(&self) -> ExposureKind {
        ExposureKind::MetaSearch
    }

    async fn render(
        &self,
        _catalog: &FilteredCatalog,
        _provider: &ProviderCapabilities,
    ) -> Result<RenderOutcome, ExposureError> {
        // The full catalog is replaced by the single meta-tool entry.
        // The engine itself was populated at construction time (or via
        // CatalogObserverRegistry reindex per AC-9-7-8); render is just
        // transport.
        Ok(RenderOutcome {
            payload: ExposurePayload::MetaTool(self.meta_tool_descriptor.clone()),
            diagnostics: RenderDiagnostics::clean(),
        })
    }

    async fn on_catalog_changed(
        &self,
        _delta: &crate::domain::models::catalog_delta::CatalogDelta,
    ) -> Result<(), ExposureError> {
        // The CatalogObserverRegistry (AC-9-7-8) handles broadcast routing.
        // This trait method is the contract that lets the registry subscribe
        // via Arc<dyn ToolExposurePort>; the actual reindex work happens in
        // the owned task per Winston Risk 4 contract.
        Ok(())
    }
}

pub fn build_search_capabilities_descriptor() -> ToolDescriptor {
    ToolDescriptor {
        id: crate::domain::models::tool_descriptor::ToolId("builtin::search_capabilities".into()),
        name: "search_capabilities".into(),
        description: SEARCH_CAPABILITIES_DESCRIPTION.into(),
        input_schema: SEARCH_CAPABILITIES_SCHEMA.clone(),
        provider_id: "builtin".into(),
        annotations: crate::domain::models::tool_descriptor::ToolAnnotations::default(),
    }
}

const SEARCH_CAPABILITIES_DESCRIPTION: &str = "\
Search the merged capability catalog (MCP tools + skills) by natural-language query. \
Returns ranked hits with `name`, `kind`, `terse`, and `score`. \
After choosing a hit, call the tool directly (for `kind: tool`) or call `skill_view` \
(for `kind: skill`) to retrieve the full body. Use this when you need to discover \
capabilities by intent rather than by name.";

use std::sync::LazyLock;
static SEARCH_CAPABILITIES_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "query": {
                "type": "string",
                "description": "Natural-language description of the capability you need."
            },
            "kind": {
                "type": "string",
                "enum": ["tool", "skill"],
                "description": "Optional filter: 'tool' returns only MCP/builtin tool hits; 'skill' returns only skill hits; omit for both."
            },
            "top_k": {
                "type": "integer",
                "minimum": 1,
                "maximum": 20,
                "default": 5,
                "description": "Maximum hits to return (clamped to [1, 20])."
            }
        },
        "required": ["query"]
    })
});

pub fn build_search_capabilities_tool_definition() -> crate::domain::models::ToolDefinition {
    crate::domain::models::ToolDefinition {
        name: "search_capabilities".into(),
        description: SEARCH_CAPABILITIES_DESCRIPTION.into(),
        input_schema: SEARCH_CAPABILITIES_SCHEMA.clone(),
        parallel_safe: true,
    }
}
