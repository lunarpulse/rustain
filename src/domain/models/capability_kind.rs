use serde::{Deserialize, Serialize};

/// Discriminator for the merged search corpus — distinguishes tools from
/// skills (and future kinds: Subagent, A2A).
///
/// # Phase B (Story 9.7)
///
/// `Tool` and `Skill` are constructible. Both impls of `IndexableItem`
/// (`ToolDescriptor` and `SkillMetadata`) construct one or the other in
/// their `doc_key` method.
///
/// # Phase C (deferred — Epics 10, 14)
///
/// `Subagent` (subagent dispatch) and `A2a` (peer-agent capability)
/// variants extend this enum additively. `#[non_exhaustive]` ensures all
/// match sites carry a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum CapabilityKind {
    Tool,
    Skill,
}

impl CapabilityKind {
    /// Stable string label for logs, status panels, and the `SearchHit.kind`
    /// serialization (`kebab-case` via serde).
    pub fn as_str(&self) -> &'static str {
        match self {
            CapabilityKind::Tool => "tool",
            CapabilityKind::Skill => "skill",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_capability_kind_serde_kebab_case() {
        let tool = CapabilityKind::Tool;
        let json = serde_json::to_string(&tool).unwrap();
        assert_eq!(json, "\"tool\"");

        let skill = CapabilityKind::Skill;
        let json = serde_json::to_string(&skill).unwrap();
        assert_eq!(json, "\"skill\"");

        // Round-trip
        let parsed: CapabilityKind = serde_json::from_str("\"tool\"").unwrap();
        assert_eq!(parsed, CapabilityKind::Tool);
    }
}
