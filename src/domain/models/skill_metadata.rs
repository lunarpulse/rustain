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
    pub name: String,
    pub description: String,
    pub source: SkillSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terse: Option<String>,
}

impl SkillMetadata {
    /// Construct from a `SkillDef`, dropping the file/directory/allowed_tools
    /// fields that don't belong in the L1 prefix.
    pub fn from_def(def: &crate::domain::models::skill::SkillDef) -> Self {
        Self {
            name: def.name.clone(),
            description: def.description.clone(),
            source: def.source,
            terse: def.terse.clone(),
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
            terse: None,
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
            terse: None,
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
            terse: None,
        };
        let ws_rustain = SkillMetadata {
            name: "b".into(),
            description: "Skill B does something when asked".into(),
            source: SkillSource::WorkspaceRustain,
            terse: None,
        };
        let ws_claude = SkillMetadata {
            name: "c".into(),
            description: "Skill C does something when asked".into(),
            source: SkillSource::WorkspaceClaude,
            terse: None,
        };
        let global = SkillMetadata {
            name: "d".into(),
            description: "Skill D does something when asked".into(),
            source: SkillSource::GlobalAgents,
            terse: None,
        };
        assert!(ws_agents.is_workspace_scoped());
        assert!(ws_rustain.is_workspace_scoped());
        assert!(ws_claude.is_workspace_scoped());
        assert!(!global.is_workspace_scoped());
    }
}

#[cfg(feature = "meta-search")]
impl crate::domain::ports::search::IndexableItem for SkillMetadata {
    fn doc_key(&self) -> crate::domain::models::doc_key::DocKey {
        crate::domain::models::doc_key::DocKey::new(
            crate::domain::models::capability_kind::CapabilityKind::Skill,
            self.name.clone(),
        )
    }

    fn searchable_text(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Owned(format!("{} {}", self.name, self.description))
    }

    fn description(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Borrowed(&self.description)
    }

    fn to_search_hit(&self, score: f32, matched_terms: Option<Vec<String>>) -> crate::domain::models::search_hit::SearchHit {
        // SkillMetadata does NOT carry the optional SkillDef.terse override
        // — only SkillDef does (AC-9-7-9). The composition root projects
        // SkillDef → SkillMetadata + (separately) carries the optional terse
        // override into the MergedIndex's CachedProjection at index time.
        // Here we re-derive from description as the default; the index-time
        // override is applied by MergedIndex::index_with_override (see
        // Decision Gate 9.7.8 — note the override path is OWNED BY THE
        // INDEX, not by the projection method, so SkillMetadata.terse does
        // NOT need to exist as a field).
        let terse = crate::domain::services::meta_search::compute_terse(
            &self.description,
            &self.name,
        );
        crate::domain::models::search_hit::SearchHit {
            name: self.name.clone(),
            kind: crate::domain::models::capability_kind::CapabilityKind::Skill,
            terse,
            score,
            provider: None,
            matched_terms,
        }
    }
}
