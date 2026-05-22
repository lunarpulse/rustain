use serde::{Deserialize, Serialize};

use crate::domain::models::SkillSource;

/// L1 metadata projection of a skill for per-turn prefix injection.
///
/// Distinct from:
/// - `SkillDef` (`rustain/src/domain/models/skill.rs`) — the full
///    filesystem-loaded record including file paths and allowed_tools.
/// - `Capability` (from 9.3a `CapabilityProvider::discover`) — the
///    registry-wire shape.
/// - `SkillFullEntry` (in `crate::adapters::skill_exposure`) — the
///    StaticFullExposure projection that bundles metadata + body.
///
/// `SkillMetadata` is the SHAPE OF THE L1 PREFIX ENTRY: name (for invocation
/// addressability), description (for LLM selection signal), source (for
/// trust-marker provenance — workspace-tier skills get `trust="workspace"`
/// XML attribute per ADR-09-02 §Consequences Negative).
///
/// Approximate token cost: ~100 tok per skill at L1 (name ~5 tok +
/// description ~80 tok per Anthropic anchor + 15 tok structural overhead).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillMetadata {
    /// Skill's invocation name (matches frontmatter `name` field; validated
    /// against `^[a-z][a-z0-9-]{0,63}$` per the Anthropic contract — see
    /// AC-9-6-6 frontmatter linter).
    pub name: String,
    /// One-line description (frontmatter `description` field; validated for
    /// length ∈ [20, 1024] and presence of "when" trigger phrase per AC-9-6-6).
    pub description: String,
    /// Provenance: which tier did the skill come from (workspace vs global).
    /// Drives the trust-marker XML attribute on rendered output.
    pub source: SkillSource,
}

impl SkillMetadata {
    /// Construct from a `SkillDef`, dropping the file/directory/allowed_tools
    /// fields that don't belong in the L1 prefix.
    pub fn from_def(def: &crate::domain::models::skill::SkillDef) -> Self {
        Self {
            name: def.name.clone(),
            description: def.description.clone(),
            source: def.source,
        }
    }

    /// Estimated token cost of this metadata entry at L1 (name + description
    /// + structural overhead). Used by `SkillRenderDiagnostics::definition_tokens_estimate`.
    /// Approximation: 4 chars/token (English heuristic) + 15 token overhead
    /// for XML structure / list bullets / newlines.
    pub fn estimated_tokens(&self) -> usize {
        (self.name.len() + self.description.len()) / 4 + 15
    }

    /// True if this skill came from workspace-tier (vs global-tier). Drives
    /// the trust-marker XML attribute on rendered output per ADR-09-02
    /// §Consequences Negative.
    pub fn is_workspace_scoped(&self) -> bool {
        use SkillSource::*;
        matches!(
            self.source,
            WorkspaceAgents | WorkspaceRustain | WorkspaceClaude
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::skill::SkillDef;
    use std::path::PathBuf;

    fn test_def(name: &str, desc: &str, source: SkillSource) -> SkillDef {
        SkillDef {
            name: name.into(),
            description: desc.into(),
            file: PathBuf::from(format!("/tmp/{name}/SKILL.md")),
            directory: PathBuf::from(format!("/tmp/{name}")),
            source,
            allowed_tools: None,
        }
    }

    #[test]
    fn test_from_def_round_trip() {
        let def = test_def(
            "review-code",
            "Reviews code when the user requests a review",
            SkillSource::WorkspaceClaude,
        );
        let meta = SkillMetadata::from_def(&def);
        assert_eq!(meta.name, "review-code");
        assert_eq!(
            meta.description,
            "Reviews code when the user requests a review"
        );
        assert_eq!(meta.source, SkillSource::WorkspaceClaude);
    }

    #[test]
    fn test_estimated_tokens_typical_skill() {
        let meta = SkillMetadata {
            name: "review-code".into(),
            description: "Reviews code for style issues when the user runs /review".into(),
            source: SkillSource::WorkspaceClaude,
        };
        let tokens = meta.estimated_tokens();
        assert!(tokens > 0, "token estimate must be positive");
        assert!(tokens <= 100, "typical skill L1 metadata should be well under 100 tokens");
    }

    #[test]
    fn test_is_workspace_scoped_workspace_variants() {
        let ws_agents = SkillMetadata {
            name: "a".into(),
            description: "Skill A does something when asked".into(),
            source: SkillSource::WorkspaceAgents,
        };
        let ws_rustain = SkillMetadata {
            name: "b".into(),
            description: "Skill B does something when asked".into(),
            source: SkillSource::WorkspaceRustain,
        };
        let ws_claude = SkillMetadata {
            name: "c".into(),
            description: "Skill C does something when asked".into(),
            source: SkillSource::WorkspaceClaude,
        };
        let global = SkillMetadata {
            name: "d".into(),
            description: "Skill D does something when asked".into(),
            source: SkillSource::GlobalAgents,
        };
        assert!(ws_agents.is_workspace_scoped());
        assert!(ws_rustain.is_workspace_scoped());
        assert!(ws_claude.is_workspace_scoped());
        assert!(!global.is_workspace_scoped());
    }
}
