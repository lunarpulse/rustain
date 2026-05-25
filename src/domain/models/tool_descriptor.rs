use serde::{Deserialize, Serialize};

use crate::domain::models::capability_registry::ProviderId;

/// Typed identifier for a tool. Newtype around `String` for type safety —
/// distinct from `CapabilityId` (the registry key, which carries `protocol` +
/// `server` + `tool`). `ToolId` is the stable catalog-side identity that
/// `CatalogDelta::removed` references.
///
/// Format: same as `CapabilityId::as_string()` — `{protocol}::{server}::{tool}`
/// or `{protocol}::{tool}` when server is empty. Construction from a
/// `CapabilityId` via the `From` impl below is the canonical path.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub struct ToolId(pub String);

impl ToolId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&crate::domain::models::capability_id::CapabilityId> for ToolId {
    fn from(id: &crate::domain::models::capability_id::CapabilityId) -> Self {
        ToolId(id.as_string())
    }
}

impl std::fmt::Display for ToolId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Tool annotations — domain-local copy of the rmcp annotation shape.
///
/// rmcp's `ToolAnnotations` lives in the adapter layer (`rmcp::model::ToolAnnotations`)
/// and cannot be imported into `src/domain/` per the hexagonal architecture
/// constraint (CLAUDE.md §"Dependency rule"). This is the domain-local
/// equivalent — same field names + types as the MCP spec for direct field-by-field
/// copy in `McpProvider`-side projection.
///
/// All fields are `Option<bool>` to preserve the "unknown" distinction
/// (the MCP spec defines unset as semantically distinct from `Some(false)`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolAnnotations {
    /// `title` — human-friendly display name (MCP spec).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// `readOnlyHint` — tool does not modify external state.
    /// Used by `parallel_safe` derivation (Story 9.2 ADR-06-08).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only_hint: Option<bool>,
    /// `destructiveHint` — tool may perform destructive changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destructive_hint: Option<bool>,
    /// `idempotentHint` — calling the tool repeatedly with the same args
    /// has no additional effect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotent_hint: Option<bool>,
    /// `openWorldHint` — tool interacts with an unbounded external system.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_world_hint: Option<bool>,
}

/// Canonical domain catalog shape for a tool — used by `FilteredCatalog`
/// (Story 9.4 Phase A), `IndexableItem` (Story 9.7 Phase B), and
/// `SkillExposurePort::render` consumers (Story 9.6 Phase A).
///
/// Distinct from:
/// - `ToolDefinition` (`src/domain/models/tools.rs:7-15`): the LLM-wire shape
///   serialized to Anthropic / OpenAI / Ollama tool schemas.
/// - `RegisteredCapability` (`src/domain/models/capability_registry.rs:51-66`):
///   the registry's working shape; carries `protocol`, `provider_id`, and
///   `parallel_safe` only (no rich annotations).
///
/// Conversion `From<&RegisteredCapability> for ToolDescriptor` synthesizes
/// `ToolAnnotations` with `read_only_hint = Some(parallel_safe)` and
/// other fields left as `None` (best-effort; richer annotations only
/// available from MCP tools via the `McpProvider::discover` projection).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDescriptor {
    pub id: ToolId,
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub provider_id: ProviderId,
    pub annotations: ToolAnnotations,
}

impl From<&crate::domain::models::capability_registry::RegisteredCapability> for ToolDescriptor {
    fn from(rc: &crate::domain::models::capability_registry::RegisteredCapability) -> Self {
        ToolDescriptor {
            id: ToolId::from(&rc.id),
            name: rc.name.clone(),
            description: rc.description.clone(),
            input_schema: rc.input_schema.clone(),
            provider_id: rc.provider_id.clone(),
            annotations: ToolAnnotations {
                title: None,
                // Best-effort: parallel_safe is the proxy for read_only_hint.
                // McpProvider could be amended in Story 9.4 / 9.6 to populate
                // the full annotation set if needed (currently the projection
                // at mcp_provider.rs:62-83 reads only `parallel_safe`).
                read_only_hint: Some(rc.parallel_safe),
                destructive_hint: None,
                idempotent_hint: None,
                open_world_hint: None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::capability_id::CapabilityId;
    use crate::domain::models::capability_registry::RegisteredCapability;

    #[test]
    fn test_from_registered_capability_round_trip() {
        let rc = RegisteredCapability {
            id: CapabilityId {
                protocol: "builtin".into(),
                server: String::new(),
                tool: "Bash".into(),
            },
            protocol: "builtin".into(),
            provider_id: "builtin".into(),
            name: "Bash".into(),
            description: "Execute shell commands".into(),
            input_schema: serde_json::json!({"type": "object"}),
            parallel_safe: false,
        };
        let td = ToolDescriptor::from(&rc);
        assert_eq!(td.id, ToolId("builtin::Bash".into()));
        assert_eq!(td.name, "Bash");
        assert_eq!(td.provider_id, "builtin");
        assert_eq!(td.annotations.read_only_hint, Some(false));
        assert_eq!(td.annotations.title, None);
    }

    #[test]
    fn test_serde_round_trip() {
        let td = ToolDescriptor {
            id: ToolId("mcp::postgres::query".into()),
            name: "query".into(),
            description: "Run SQL".into(),
            input_schema: serde_json::json!({"type": "object"}),
            provider_id: "mcp:postgres".into(),
            annotations: ToolAnnotations {
                title: Some("PostgreSQL Query".into()),
                read_only_hint: Some(true),
                destructive_hint: Some(false),
                idempotent_hint: None,
                open_world_hint: Some(true),
            },
        };
        let json = serde_json::to_string(&td).unwrap();
        let back: ToolDescriptor = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, td.id);
        assert_eq!(back.name, td.name);
        assert_eq!(back.annotations.title, td.annotations.title);
        assert_eq!(
            back.annotations.read_only_hint,
            td.annotations.read_only_hint
        );
    }

    #[test]
    fn test_tool_id_from_capability_id() {
        let two = CapabilityId {
            protocol: "builtin".into(),
            server: String::new(),
            tool: "Bash".into(),
        };
        assert_eq!(ToolId::from(&two).as_str(), "builtin::Bash");

        let three = CapabilityId {
            protocol: "mcp".into(),
            server: "postgres".into(),
            tool: "query".into(),
        };
        assert_eq!(ToolId::from(&three).as_str(), "mcp::postgres::query");
    }

    #[test]
    fn test_annotations_default_all_none() {
        let ann = ToolAnnotations::default();
        assert!(ann.title.is_none());
        assert!(ann.read_only_hint.is_none());
        assert!(ann.destructive_hint.is_none());
        assert!(ann.idempotent_hint.is_none());
        assert!(ann.open_world_hint.is_none());
    }

    #[test]
    fn test_camelcase_serde() {
        let ann = ToolAnnotations {
            read_only_hint: Some(true),
            ..Default::default()
        };
        let json = serde_json::to_string(&ann).unwrap();
        assert!(json.contains("readOnlyHint"));
        assert!(!json.contains("\"readOnlyHint\":null"));
    }
}

#[cfg(feature = "meta-search")]
impl crate::domain::ports::search::IndexableItem for ToolDescriptor {
    fn doc_key(&self) -> crate::domain::models::doc_key::DocKey {
        crate::domain::models::doc_key::DocKey::new(
            crate::domain::models::capability_kind::CapabilityKind::Tool,
            self.name.clone(),
        )
    }

    fn searchable_text(&self) -> std::borrow::Cow<'_, str> {
        // Concatenate name + description for BM25 tokenization. Annotations
        // (title hint, read_only_hint, etc.) are NOT indexed Phase B — they
        // are metadata about parallelism + safety, not retrieval signal.
        // The 9.3b annotation set is preserved verbatim on the ToolDescriptor
        // struct; `IndexableItem` adds a projection method, not a field.
        std::borrow::Cow::Owned(format!("{} {}", self.name, self.description))
    }

    fn description(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Borrowed(&self.description)
    }

    fn to_search_hit(
        &self,
        score: f32,
        matched_terms: Option<Vec<String>>,
    ) -> crate::domain::models::search_hit::SearchHit {
        let terse =
            crate::domain::services::meta_search::compute_terse(&self.description, &self.name);
        let hit = crate::domain::models::search_hit::SearchHit {
            name: self.name.clone(),
            kind: crate::domain::models::capability_kind::CapabilityKind::Tool,
            terse,
            score,
            // provider: populated only on collision (post-rank pass in
            // MergedIndex::populate_provider_disambiguation per Decision
            // Gate 9.7.4). The canonical projection emits None; the merged
            // index populates Some after detecting same-name across providers.
            provider: None,
            matched_terms,
        };
        if !crate::domain::models::capability_id::CapabilityId::from_mcp_wire_name(&hit.name)
            .is_some()
            && !crate::domain::models::capability_id::CapabilityId::parse(&format!(
                "builtin::{}",
                hit.name
            ))
            .is_some()
        {
            debug_assert!(
                false,
                "ToolDescriptor.to_search_hit produced name '{}' that does not round-trip via CapabilityId — AC-9-7-3",
                hit.name
            );
            tracing::warn!(
                "ToolDescriptor.to_search_hit produced name '{}' that does not round-trip via CapabilityId — AC-9-7-3",
                hit.name
            );
        }
        hit
    }
}
