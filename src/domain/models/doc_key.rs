use serde::{Deserialize, Serialize};

use crate::domain::models::capability_kind::CapabilityKind;

/// Stable index key for a capability — carries `kind` so collisions across
/// kinds are impossible by construction (per ADR-09-02 §Why one merged index).
///
/// # Phase B (Story 9.7)
///
/// Constructed by `IndexableItem::doc_key` impls on `ToolDescriptor` and
/// `SkillMetadata`. Consumed by `MergedIndex` as the registry key + by
/// `MetaSearchEngine` as the corpus-agnostic identifier.
///
/// # Phase C (deferred)
///
/// `CapabilityKind::Subagent` (Epic 10) and `CapabilityKind::A2a` (Epic 14)
/// extend the discriminator without breaking existing `DocKey` semantics —
/// `CapabilityKind` is `#[non_exhaustive]`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub struct DocKey {
    pub kind: CapabilityKind,
    pub name: String,
}

impl DocKey {
    pub fn new(kind: CapabilityKind, name: impl Into<String>) -> Self {
        Self {
            kind,
            name: name.into(),
        }
    }

    /// Display form for logs + dev-tool surfaces: `tool::name` / `skill::name`.
    /// NOT the LLM-wire form — the wire form is `SearchHit.name` directly
    /// (which round-trips through `CapabilityId::from_mcp_wire_name` for
    /// MCP tools per AC-9-7-3 roundtrip test).
    pub fn display(&self) -> String {
        format!("{}::{}", self.kind.as_str(), self.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_doc_key_display_form() {
        let dk = DocKey::new(CapabilityKind::Tool, "query");
        assert_eq!(dk.display(), "tool::query");

        let dk2 = DocKey::new(CapabilityKind::Skill, "review-code");
        assert_eq!(dk2.display(), "skill::review-code");
    }

    #[test]
    fn test_doc_key_stable_across_clone() {
        let dk = DocKey::new(CapabilityKind::Tool, "Bash");
        assert_eq!(dk, dk.clone());
    }
}
