//! Tool exposure strategies — adapter module per ADR-09-01 v2.2.
//!
//! Phase A ships `StaticFullExposure` (the passthrough default) and the
//! `CapabilityMatrix` stub (returns `Capability::Full` for every provider).
//! Phase B (Story 9.7) ships `MetaSearchExposure` + per-provider native
//! primitive wiring.

pub mod capability_matrix;
#[cfg(feature = "meta-search")]
pub mod meta_search;
pub mod static_full;

pub use capability_matrix::{Capability, CapabilityMatrix};
#[cfg(feature = "meta-search")]
pub use meta_search::MetaSearchExposure;
pub use static_full::StaticFullExposure;

use serde::{Deserialize, Serialize};

use crate::domain::models::tool_descriptor::ToolDescriptor;

/// Stable identifier for an exposure strategy. Used by logs, telemetry
/// (Story 9.5 `tool_exposure.kind` metric), and status panels (Story 8.5).
///
/// **Phase A:** only `StaticFull` is constructible. `MetaSearch` is RESERVED
/// in the enum (forward-compat) but no Phase A impl constructs it; Story 9.7
/// Phase B's `MetaSearchExposure` constructs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExposureKind {
    /// `StaticFullExposure` — the full filtered catalog serialized every turn.
    /// The default per ADR-09-01 v2.2 §Decision; zero behavior change vs
    /// today's static-full injection at `event_loop.rs:7783-7877`.
    StaticFull,
    /// `MetaSearchExposure` — RESERVED for Story 9.7 Phase B. No Phase A impl
    /// constructs this variant; Phase A's config parser rejects
    /// `[tools].exposure = "meta-search"` at startup with an actionable error
    /// pointing at ADR-09-01 v2.2 §Phase B Prerequisites.
    MetaSearch,
}

/// Note: NO `Disabled` variant per ADR-09-01 v2.1 §W1. Headless / eval =
/// `Option<Arc<dyn ToolExposurePort>>::None` in the composition root — no
/// trait impl exists solely to no-op. The ISP whiff that came from forcing
/// a `Disabled` impl to no-op `on_catalog_changed` forever is the reason.
/// Per-turn tool payload produced by `ToolExposurePort::render`.
#[derive(Debug, Clone)]
pub enum ExposurePayload {
    /// `StaticFullExposure` — full filtered catalog every turn.
    /// Phase A's only constructible variant.
    Tools(Vec<ToolDescriptor>),
    /// `MetaSearchExposure` — single `tool_search` (Phase B) or
    /// `search_tools` (Story 9.7 tool-side door per ADR-09-02 §Audience Split)
    /// meta-tool entry. RESERVED for Phase B; no Phase A code constructs it.
    MetaTool(ToolDescriptor),
}

/// Note: NO `Empty` variant per ADR-09-01 v2.1 §W1. Headless / eval path
/// returns no payload at all (the port itself is None) so adapters need no
/// special case here.
/// Result returned by `ToolExposurePort::render`. Carries payload +
/// fidelity-loss diagnostics per ADR-09-01 v2.1 §W2 (restores LSP under
/// provider caps).
#[derive(Debug, Clone)]
pub struct RenderOutcome {
    pub payload: ExposurePayload,
    pub diagnostics: RenderDiagnostics,
}

/// Fidelity-loss diagnostics surfaced from the render path to telemetry
/// (Story 9.5) and the adapter-status panel (Story 8.5).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RenderDiagnostics {
    /// True when the renderer dropped tools to fit a provider-imposed cap
    /// (e.g., Gemini's 64-tool cap, future Anthropic effective caps).
    /// Phase A: always false (passthrough cannot truncate).
    pub truncated: bool,
    /// Count of tools omitted from `payload` (0 for MetaSearch; 0 for Phase A
    /// StaticFull; may be >0 for Phase B `StaticFullExposure` on capped providers).
    pub dropped_count: usize,
    /// Human-readable explanation surfaced to telemetry and the adapter-status
    /// panel. `None` when no fidelity loss occurred.
    pub reason: Option<String>,
    /// Story 9.5 — estimated token count of serialized tool definitions.
    /// Populated by future provider-aware exposures in Phase B.
    /// Phase A: always `None` (passthrough doesn't pre-compute).
    pub definition_tokens_estimate: Option<usize>,
}

impl RenderDiagnostics {
    /// Construct a clean (no-loss) diagnostics. Used by Phase A's
    /// `StaticFullExposure` passthrough.
    pub fn clean() -> Self {
        Self {
            truncated: false,
            dropped_count: 0,
            reason: None,
            definition_tokens_estimate: None,
        }
    }
}

/// Errors from exposure strategy operations.
#[derive(Debug, thiserror::Error)]
pub enum ExposureError {
    /// Strategy is incompatible with the provider — caught at session
    /// handshake via `CapabilityMatrix`, never mid-turn per ADR §Capability
    /// matrix.
    #[error("exposure strategy {strategy:?} is incompatible with provider {provider}: {reason}")]
    Incompatible {
        strategy: ExposureKind,
        provider: String,
        reason: String,
    },
    /// Catalog change failed to apply. Phase A returns this only from
    /// fallible-typed `on_catalog_changed` impls; Phase A's `StaticFullExposure`
    /// always returns `Ok(())`.
    #[error("failed to apply catalog change: {0}")]
    CatalogChangeFailed(String),
    /// Render failed. Phase A's `StaticFullExposure` never returns this;
    /// Phase B's `MetaSearchExposure` can return it on indexing failure.
    #[error("render failed: {0}")]
    RenderFailed(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exposure_kind_serde_round_trip() {
        let json = serde_json::to_string(&ExposureKind::StaticFull).unwrap();
        assert_eq!(json, "\"static-full\"");
        let back: ExposureKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ExposureKind::StaticFull);
    }

    #[test]
    fn test_render_diagnostics_clean_constructor() {
        let d = RenderDiagnostics::clean();
        assert!(!d.truncated);
        assert_eq!(d.dropped_count, 0);
        assert_eq!(d.reason, None);
    }
}
