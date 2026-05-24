use serde::{Deserialize, Serialize};

use crate::domain::models::skill_metadata::SkillMetadata;

/// The filtered skill catalog presented to `SkillExposurePort::render`.
///
/// Distinct from:
/// - `SkillRegistry::skills() -> &[SkillDef]` — the raw filesystem-loaded
///    catalog (no filter applied).
/// - `SkillsProvider::discover() -> Vec<Capability>` — the registry-wire
///    projection.
/// - `Vec<RegisteredCapability>` — the registry's working shape.
///
/// `FilteredSkillCatalog` is the SHAPE OF THE RENDER INPUT: it has been
/// filtered by skill activation policy before reaching the exposure strategy.
///
/// # Phase A (Story 9.6)
///
/// Phase A construction is via `FilteredSkillCatalog::from_metadata(metas)`
/// — the composition root constructs the catalog by reading `SkillsProvider`
/// through the two-layer skill cache.
///
/// # Phase B (Story 9.7 — DEFERRED)
///
/// Phase B's `MetaSearchExposure::render` consumes `FilteredSkillCatalog` as
/// the BM25 indexing input via `IndexableItem` trait (Story 9.7). The element
/// type stays `SkillMetadata`; only the consumer changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilteredSkillCatalog {
    metadata: Vec<SkillMetadata>,
}

impl FilteredSkillCatalog {
    /// Construct from a metadata list. The caller is responsible for
    /// applying activation-policy filters before constructing this value.
    pub fn from_metadata(metadata: Vec<SkillMetadata>) -> Self {
        Self { metadata }
    }

    /// Empty filtered catalog. Used by Phase A unit tests + the eval-harness
    /// path (where the port itself is `None` per ADR-09-01 v2.1 §W1 inherited).
    pub fn empty() -> Self {
        Self {
            metadata: Vec::new(),
        }
    }

    /// Borrow the filtered metadata list.
    pub fn metadata(&self) -> &[SkillMetadata] {
        &self.metadata
    }

    /// Number of skills in the filtered catalog. Used by Story 9.5
    /// `skill_exposure.catalog_size{provider_id}` metric (AC-9-5-7 extended).
    pub fn len(&self) -> usize {
        self.metadata.len()
    }

    /// True when no skills are present.
    pub fn is_empty(&self) -> bool {
        self.metadata.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::skill::SkillSource;

    fn test_metadata(name: &str, desc: &str) -> SkillMetadata {
        SkillMetadata {
            name: name.into(),
            description: desc.into(),
            source: SkillSource::WorkspaceAgents,
            terse: None,
        }
    }

    #[test]
    fn test_from_metadata_round_trip() {
        let catalog = FilteredSkillCatalog::from_metadata(vec![
            test_metadata("a", "Skill A when needed"),
            test_metadata("b", "Skill B when needed"),
            test_metadata("c", "Skill C when needed"),
        ]);
        assert_eq!(catalog.len(), 3);
        assert_eq!(catalog.metadata().len(), 3);
    }

    #[test]
    fn test_empty_constructor() {
        let catalog = FilteredSkillCatalog::empty();
        assert!(catalog.is_empty());
        assert_eq!(catalog.len(), 0);
    }

    #[test]
    fn test_serde_round_trip() {
        let catalog = FilteredSkillCatalog::from_metadata(vec![SkillMetadata {
            name: "test-skill".into(),
            description: "A test skill for when testing is needed".into(),
            source: SkillSource::WorkspaceClaude,
            terse: None,
        }]);
        let json = serde_json::to_string(&catalog).unwrap();
        let back: FilteredSkillCatalog = serde_json::from_str(&json).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back.metadata()[0].name, "test-skill");
    }
}
