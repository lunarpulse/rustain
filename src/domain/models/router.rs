//! Tiered model router domain types.
//!
//! The tier model splits LLM calls into two cost/quality bands:
//! - `CheapAgentic` — fast, cheap, good enough for routine edits/tests.
//! - `Flagship` — slower, expensive, reserved for codegen, plan, review,
//!   or when budget/retry escalation forces it.
//!
//! Resolution is a **pure domain service** (`domain/services/model_router.rs`);
//! no routing logic lives in adapters (epics.md:2767).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Cost/quality tier for a model call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTier {
    CheapAgentic,
    Flagship,
}

/// Kind of step being executed by an agent.
///
/// Used to map step types to default tiers via `RouterConfig.step_tiers`.
/// Becomes load-bearing in Story 10.7 (sub-agent dispatch).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    Codegen,
    Edit,
    Test,
    Plan,
    Review,
}

/// Why a call was escalated to a higher tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EscalationReason {
    None,
    Budget,
    Retry,
}

/// Router configuration — loaded from `[router]` in `rustain.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouterConfig {
    #[serde(default = "RouterConfig::default_default_tier")]
    pub default_tier: ModelTier,
    #[serde(default = "RouterConfig::default_threshold_tokens")]
    pub threshold_tokens: u32,
    #[serde(default = "RouterConfig::default_max_retries")]
    pub max_retries: u32,
    #[serde(default = "RouterConfig::default_tier_models")]
    pub tier_models: HashMap<ModelTier, String>,
    #[serde(default = "RouterConfig::default_step_tiers")]
    pub step_tiers: HashMap<StepKind, ModelTier>,
}

impl RouterConfig {
    fn default_default_tier() -> ModelTier {
        ModelTier::CheapAgentic
    }
    fn default_threshold_tokens() -> u32 {
        100_000
    }
    fn default_max_retries() -> u32 {
        2
    }
    fn default_tier_models() -> HashMap<ModelTier, String> {
        // Intentionally empty: when the user hasn't configured [router],
        // the caller's fallback_model (typically config.model) is used.
        // This avoids hardcoding provider-specific model names that would
        // silently override the user's configured model.
        HashMap::new()
    }
    fn default_step_tiers() -> HashMap<StepKind, ModelTier> {
        let mut m = HashMap::new();
        m.insert(StepKind::Codegen, ModelTier::Flagship);
        m.insert(StepKind::Edit, ModelTier::CheapAgentic);
        m.insert(StepKind::Test, ModelTier::CheapAgentic);
        m.insert(StepKind::Plan, ModelTier::Flagship);
        m.insert(StepKind::Review, ModelTier::Flagship);
        m
    }
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            default_tier: Self::default_default_tier(),
            threshold_tokens: Self::default_threshold_tokens(),
            max_retries: Self::default_max_retries(),
            tier_models: Self::default_tier_models(),
            step_tiers: Self::default_step_tiers(),
        }
    }
}
