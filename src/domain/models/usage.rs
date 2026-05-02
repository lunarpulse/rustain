#![allow(dead_code)]
use serde::{Deserialize, Serialize};

use super::provider::ModelCapability;

/// Token usage information from a provider response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageInfo {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_creation_input_tokens: Option<u32>,
    pub cache_read_input_tokens: Option<u32>,
    /// Reasoning tokens (e.g., Claude extended thinking).
    /// Added in S7.1a for cost tracking parity (FR36).
    pub reasoning_tokens: Option<u32>,
}

/// Information about an available model from a provider.
///
/// **Legacy struct** — kept for backward compatibility until S7.1c
/// migrates all consumers to `ModelDescriptor`. Both live side-by-side
/// in the provider registry; `ProviderRegistry::list_models()` returns
/// `ModelDescriptor`, but existing code reading `ModelInfo` still works.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    pub id: String,
    pub display_name: String,
    pub context_window: u32,
    pub max_output_tokens: u32,
    /// Capabilities this model supports (added in S7.1a).
    #[serde(default)]
    pub capabilities: std::collections::HashSet<ModelCapability>,
    /// Optional pricing tier for UI grouping (added in S7.1a).
    #[serde(default)]
    pub pricing_tier: Option<String>,
}
