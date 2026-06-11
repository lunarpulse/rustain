use serde::{Deserialize, Serialize};

use crate::domain::models::tool_descriptor::{ToolDescriptor, ToolId};

/// Catalog delta — additions and removals since the previous snapshot.
///
/// # Phase A (Story 9.3b, RELAXED form per ADR-09-01 v2.2 §Phased Implementation)
///
/// The type is defined with three fields, and `CompositeToolsetAdapter::emit_catalog_delta`
/// is defined as `Ok(())` no-op. Internal computation happens (the version
/// counter monotonically increments, the added/removed sets are computed
/// against the previous snapshot) but NO subscriber consumes the delta —
/// the `tokio::sync::broadcast::Sender<CatalogDelta>` wiring is deferred
/// to Story 9.7 Phase B (which subsumes the original 9.4b scope per
/// ADR-09-02 v1 + sprint-status note 2026-05-21).
///
/// # Phase B (Story 9.7 — DEFERRED)
///
/// `CompositeToolsetAdapter` will gain a `tokio::sync::broadcast::Sender<CatalogDelta>`
/// field; `emit_catalog_delta` will route the delta through the owned-task
/// debounce (250ms) per ADR-09-01 v2.2 §W3. `CatalogObserverRegistry` at
/// `src/infrastructure/composition/catalog_observer_registry.rs` subscribes
/// `Vec<Arc<dyn CatalogObserver>>` and calls `observer.on_catalog_changed(delta).await`.
///
/// # Version monotonicity invariant
///
/// `version` increments monotonically across the composite's lifetime,
/// EVEN IN PHASE A. The counter is owned by `CompositeToolsetAdapter`
/// (single-emitter — only the composite calls `emit_catalog_delta`).
/// The invariant is enforced by `tests/conformance_capability_registry.rs::test_catalog_delta_version_monotonic`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogDelta {
    /// Tools added since the previous snapshot.
    pub added: Vec<ToolDescriptor>,
    /// Tool ids removed since the previous snapshot.
    pub removed: Vec<ToolId>,
    /// Monotonically increasing version (per-composite, lifetime-stable).
    pub version: u64,
}

impl CatalogDelta {
    /// Construct an empty delta (no changes). Use for the first emit after
    /// `populate_registry` when there's no previous snapshot to diff against —
    /// the `added` set is populated via the registered capabilities directly,
    /// not via a diff.
    pub fn empty(version: u64) -> Self {
        Self {
            added: Vec::new(),
            removed: Vec::new(),
            version,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_catalog_delta_serde_round_trip() {
        let delta = CatalogDelta {
            added: vec![],
            removed: vec![ToolId("builtin::Bash".into())],
            version: 7,
        };
        let json = serde_json::to_string(&delta).unwrap();
        let back: CatalogDelta = serde_json::from_str(&json).unwrap();
        assert_eq!(back.version, 7);
        assert_eq!(back.removed.len(), 1);
    }

    #[test]
    fn test_empty_constructor_sets_version_only() {
        let delta = CatalogDelta::empty(42);
        assert!(delta.added.is_empty());
        assert!(delta.removed.is_empty());
        assert_eq!(delta.version, 42);
    }
}
