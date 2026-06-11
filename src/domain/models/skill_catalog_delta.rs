use serde::{Deserialize, Serialize};

use crate::domain::models::skill_metadata::SkillMetadata;

/// Delta record for upstream skill catalog changes (filesystem watcher,
/// profile switch via Epic 8 community-profile install, manual skill
/// add/remove via CLI).
///
/// Mirrors `CatalogDelta` from `src/domain/models/catalog_delta.rs`
/// (Story 9.3b) but with `SkillMetadata` element type instead of
/// `ToolDescriptor` / `ToolId`.
///
/// # Phase A (Story 9.6)
///
/// **TYPE-ONLY introduction.** The struct exists so call sites can name it
/// (the trait method `SkillExposurePort::on_catalog_changed` requires the type)
/// but NO broadcast wiring is built in Phase A. Both Phase A impls'
/// `on_catalog_changed` are `Ok(())` no-ops.
///
/// # Phase B (Story 9.7 — DEFERRED)
///
/// Phase B adds the shared `CatalogObserverRegistry` carrying
/// TWO `tokio::sync::broadcast::Sender` channels — one `Sender<CatalogDelta>`
/// (Tools) + one `Sender<SkillCatalogDelta>` (Skills). `MetaSearchExposure`
/// subscribes and reindexes BM25 with 250ms debounce.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillCatalogDelta {
    /// Skills added since the previous version.
    pub added: Vec<SkillMetadata>,
    /// Skills removed since the previous version. Element type is the skill
    /// `name: String`.
    pub removed: Vec<String>,
    /// Monotonic version counter — increments on each catalog mutation.
    pub version: u64,
}

impl SkillCatalogDelta {
    /// Empty delta with a given version. Used by Phase A no-op call sites
    /// + Phase B initial-subscriber bootstrap.
    pub fn empty(version: u64) -> Self {
        Self {
            added: Vec::new(),
            removed: Vec::new(),
            version,
        }
    }

    /// True when no skills changed in this delta.
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::skill::SkillSource;

    #[test]
    fn test_empty_constructor() {
        let delta = SkillCatalogDelta::empty(7);
        assert!(delta.is_empty());
        assert_eq!(delta.version, 7);
    }

    #[test]
    fn test_added_removed_round_trip() {
        let delta = SkillCatalogDelta {
            added: vec![SkillMetadata {
                name: "new-skill".into(),
                description: "A new skill when called".into(),
                source: SkillSource::WorkspaceAgents,
                terse: None,
            }],
            removed: vec!["old-skill".into()],
            version: 42,
        };
        let json = serde_json::to_string(&delta).unwrap();
        let back: SkillCatalogDelta = serde_json::from_str(&json).unwrap();
        assert_eq!(back.version, 42);
        assert_eq!(back.added.len(), 1);
        assert_eq!(back.removed.len(), 1);
        assert_eq!(back.added[0].name, "new-skill");
    }
}
