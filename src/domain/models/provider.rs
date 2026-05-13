//! Value objects for provider and model metadata.
//!
//! `ModelDescriptor` and `ProviderDescriptor` are domain value objects that drive
//! model routing (Story 7.1c), token-tracking ledger (Story 7.1c), and the UI
//! model-picker (Story 7.2). They carry business knowledge (context window,
//! capabilities, pricing tier) without depending on any adapter or infrastructure
//! crate — see ADR-07-01 (§Multi-Provider Architecture) in the competitive review.

use serde::{Deserialize, Serialize};

/// Capabilities a model advertises.
///
/// Adapters populate this from provider metadata (API docs or config).
/// The registry filters by capability for tool-use vs vision vs thinking routing.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ModelCapability {
    /// Model can accept images in the prompt (e.g., Claude, GPT-4o).
    Vision,
    /// Model supports tool/function calling.
    ToolUse,
    /// Model can emit an internal reasoning/thinking block (Claude extended thinking).
    Thinking,
    /// Model supports multiple parallel tool calls in a single turn.
    ParallelToolCalls,
    /// Forward-compatible catch-all for capabilities not yet known at compile time.
    Unknown(String),
}

/// Metadata for a single model exposed by a provider.
///
/// Lives in the `ProviderRegistry` (adapters/provider/) and is queryable from
/// any call-site that needs model characteristics without crossing the port
/// boundary (AC6). Replaces the legacy `ModelInfo` (which is kept for backward
/// compat until 7.1c migrates all consumers).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelDescriptor {
    /// Unique model identifier (e.g., `"claude-sonnet-4-20250514"`).
    pub model_id: String,
    /// Human-readable display name (e.g., `"Claude Sonnet 4"`).
    pub display_name: String,
    /// Provider this model belongs to (e.g., `"anthropic"`).
    pub provider_id: String,
    /// Maximum context window in tokens (e.g., `200_000`).
    pub context_window: u32,
    /// Capabilities this model supports.
    pub capabilities: std::collections::HashSet<ModelCapability>,
    /// Optional pricing tier for UI grouping (`"cheap"`, `"flagship"`, `None`).
    pub pricing_tier: Option<String>,
}

/// Lightweight descriptor for a registered provider.
///
/// Used by the provider-selector UI and health-check reporting.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderDescriptor {
    /// Unique provider identifier (e.g., `"anthropic"`).
    pub provider_id: String,
    /// Whether the provider passed `health_check()` at startup.
    pub healthy: bool,
    /// Number of models registered by this provider.
    pub model_count: usize,
    /// Human-readable display name (e.g., `"Anthropic"`).
    pub display_name: String,
}
