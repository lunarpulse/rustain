//! Skill exposure strategies — adapter module per ADR-09-02 v1.
//!
//! Phase A ships `L1MetadataExposure` (the spec-aligned default per ADR-09-02
//! §Decision, INVERTED from `ToolExposurePort`'s `StaticFullExposure` default
//! per evidence asymmetry — 7-signal ecosystem saturation + Anthropic Skills
//! spec mandate for Skills; partial ecosystem with Arcade single-anchor for
//! Tools) AND `StaticFullExposure` (codex-parity opt-in fallback for users
//! who explicitly prefer eager loading). Phase B (Story 9.7) ships
//! `MetaSearchExposure` + shared `MetaSearchEngine` infrastructure.

pub mod l1_metadata;
#[cfg(feature = "meta-search")]
pub mod meta_search;
pub mod static_full;

pub use l1_metadata::L1MetadataExposure;
#[cfg(feature = "meta-search")]
pub use meta_search::MetaSearchExposure;
pub use static_full::StaticFullExposure;

use serde::{Deserialize, Serialize};

use crate::domain::models::skill_metadata::SkillMetadata;
use crate::domain::models::tool_descriptor::ToolDescriptor;

/// Stable identifier for a skill exposure strategy. Used by logs, telemetry
/// (Story 9.5 `skill_exposure.kind` metric), and status panels (Story 8.5).
///
/// **Phase A:** `L1Metadata` and `StaticFull` are constructible. `MetaSearch`
/// is RESERVED in the enum (forward-compat) but no Phase A impl constructs
/// it; Story 9.7 Phase B's `MetaSearchExposure` constructs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum SkillExposureKind {
    /// `L1MetadataExposure` — the DEFAULT per ADR-09-02 §Decision. Renders
    /// only `{name, description}` per skill (~100 tok each). Body fetched
    /// on-demand via the `skill_view` meta-tool.
    L1Metadata,
    /// `StaticFullExposure` — codex-parity opt-in fallback. Renders full
    /// SKILL.md bodies inline. NOT the default per ADR-09-02 §Decision.
    StaticFull,
    /// `MetaSearchExposure` — RESERVED for Story 9.7 Phase B. No Phase A
    /// impl constructs this variant.
    MetaSearch,
}

/// Note: NO `Disabled` variant per ADR-09-01 v2.1 §W1 (inherited). Headless
/// / eval = `Option<Arc<dyn SkillExposurePort>>::None` in the composition
/// root — no trait impl exists solely to no-op.

/// Per-turn skill payload produced by `SkillExposurePort::render`.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum SkillExposurePayload {
    /// `L1MetadataExposure` — ~100 tok/skill prefix payload. Phase A's
    /// default constructible variant.
    Metadata(Vec<SkillMetadata>),
    /// `StaticFullExposure` — full SKILL.md body per skill. Phase A's opt-in
    /// constructible variant.
    Bodies(Vec<SkillFullEntry>),
    /// `MetaSearchExposure` — single `search_capabilities` meta-tool entry.
    /// RESERVED for Phase B; no Phase A code constructs it.
    SearchStub(ToolDescriptor),
}

/// Full-body entry for `StaticFullExposure`. Carries the SkillMetadata for
/// the L1 header line PLUS the body string so the rendered prefix contains
/// both the addressable name and the recipe inline.
#[derive(Debug, Clone)]
pub struct SkillFullEntry {
    pub metadata: SkillMetadata,
    pub body: String,
}

/// Result returned by `SkillExposurePort::render`. Carries payload +
/// fidelity-loss diagnostics + telemetry hooks.
#[derive(Debug, Clone)]
pub struct SkillRenderOutcome {
    pub payload: SkillExposurePayload,
    pub diagnostics: SkillRenderDiagnostics,
}

/// Fidelity-loss diagnostics + telemetry hooks surfaced from the render path
/// to telemetry (Story 9.5 — 3 Skill-side metrics per AC-9-5-7 extended) and
/// the adapter-status panel (Story 8.5 widget).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillRenderDiagnostics {
    /// True when the renderer dropped skills to fit a provider-imposed prefix
    /// budget. Phase A: always false.
    pub truncated: bool,
    /// Count of skills omitted from `payload`.
    pub dropped_count: usize,
    /// Human-readable explanation surfaced to telemetry + adapter-status panel.
    pub reason: Option<String>,
    /// Total skills in the catalog before any rendering decisions.
    pub catalog_size: usize,
    /// Estimated definition-token cost of the rendered payload.
    pub definition_tokens_estimate: usize,
}

impl SkillRenderDiagnostics {
    /// Construct a clean (no-loss) diagnostics with catalog_size + estimate
    /// populated. Used by both Phase A impls for the happy path.
    pub fn clean(catalog_size: usize, definition_tokens_estimate: usize) -> Self {
        Self {
            truncated: false,
            dropped_count: 0,
            reason: None,
            catalog_size,
            definition_tokens_estimate,
        }
    }
}

/// Errors from skill exposure strategy operations.
#[derive(Debug, thiserror::Error)]
pub enum SkillExposureError {
    /// Strategy is incompatible with the provider — caught at session
    /// handshake, never mid-turn.
    #[error(
        "skill exposure strategy {strategy:?} is incompatible with provider {provider}: {reason}"
    )]
    Incompatible {
        strategy: SkillExposureKind,
        provider: String,
        reason: String,
    },
    /// Catalog change failed to apply.
    #[error("failed to apply skill catalog change: {0}")]
    CatalogChangeFailed(String),
    /// Render failed.
    #[error("skill render failed: {0}")]
    RenderFailed(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_skill_exposure_kind_serde_round_trip() {
        let json = serde_json::to_string(&SkillExposureKind::L1Metadata).unwrap();
        assert_eq!(json, "\"l1-metadata\"");
        let back: SkillExposureKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back, SkillExposureKind::L1Metadata);

        let json_sf = serde_json::to_string(&SkillExposureKind::StaticFull).unwrap();
        assert_eq!(json_sf, "\"static-full\"");
        let back_sf: SkillExposureKind = serde_json::from_str(&json_sf).unwrap();
        assert_eq!(back_sf, SkillExposureKind::StaticFull);

        let json_ms = serde_json::to_string(&SkillExposureKind::MetaSearch).unwrap();
        assert_eq!(json_ms, "\"meta-search\"");
        let back_ms: SkillExposureKind = serde_json::from_str(&json_ms).unwrap();
        assert_eq!(back_ms, SkillExposureKind::MetaSearch);
    }

    #[test]
    fn test_skill_render_diagnostics_clean_constructor() {
        let d = SkillRenderDiagnostics::clean(3, 300);
        assert!(!d.truncated);
        assert_eq!(d.dropped_count, 0);
        assert_eq!(d.reason, None);
        assert_eq!(d.catalog_size, 3);
        assert_eq!(d.definition_tokens_estimate, 300);
    }
}
