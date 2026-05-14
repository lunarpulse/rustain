#![allow(dead_code)]
use serde::{Deserialize, Serialize};

use super::provider::ModelCapability;
use super::router::{EscalationReason, ModelTier, StepKind};

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

/// Token usage triple for the ledger — distinct from the raw provider
/// `UsageInfo` which carries cache/reasoning breakdowns.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub parent_ctx: u32,
}

/// A single entry in the usage ledger — one row per provider call.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageLedgerEntry {
    pub timestamp_ms: i64,
    pub session_id: String,
    pub conversation_id: String,
    pub provider_id: String,
    pub model: String,
    pub tier: ModelTier,
    pub step_kind: Option<StepKind>,
    pub escalation_reason: EscalationReason,
    pub usage: TokenUsage,
}

/// Information about an available model from a provider.
///
/// **Legacy struct** — kept for backward compatibility until a dedicated
/// story migrates all consumers to `ModelDescriptor`. Both live side-by-side
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
