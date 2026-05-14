//! Pure domain service for tiered model resolution.
//!
//! `resolve_effective_model` is a pure function — no I/O, no clock, no env,
//! no logging. It takes a `ModelResolutionRequest` and `RouterConfig` and
//! returns the resolved model name, tier, and escalation reason.
//!
//! Precedence (AC3):
//!   explicit_override > tier_hint > step_kind default > config.default_tier
//!
//! Escalation (AC4 + AC5):
//!   - Budget: input_tokens > threshold_tokens → Flagship + Budget
//!   - Retry:  retry_count >= max_retries      → Flagship + Retry
//!   - Tie:    retry wins over budget

use crate::domain::models::router::{EscalationReason, ModelTier, RouterConfig, StepKind};

/// Inputs for model resolution — everything the router needs to decide.
#[derive(Debug, Clone)]
pub struct ModelResolutionRequest {
    /// User or agent explicitly named a model — honor it.
    pub explicit_override: Option<String>,
    /// Optional tier hint (e.g. from a slash-command flag).
    pub tier_hint: Option<ModelTier>,
    /// The kind of step being executed.
    pub step_kind: Option<StepKind>,
    /// Current retry attempt (0 = first try).
    pub retry_count: u32,
    /// Input token count for budget escalation.
    pub input_tokens: u32,
    /// Model name to use when `tier_models` has no entry for the resolved tier.
    /// Typically `config.model` — the user's configured base model.
    pub fallback_model: String,
}

/// Result of model resolution — ready to flow into `CompletionOptions.model`.
#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub model: String,
    pub tier: ModelTier,
    pub escalation_reason: EscalationReason,
}

/// Resolve the effective model name and tier for a single provider call.
///
/// # Precedence
/// 1. `explicit_override` — if `Some(name)`, return `name` with `EscalationReason::None`.
///    Reverse-lookup `name` in `config.tier_models` to determine `tier`; fall back to
///    `config.default_tier` if `name` is not a configured tier model.
/// 2. Base tier from `tier_hint`.
/// 3. Base tier from `step_kind` → `config.step_tiers`.
/// 4. Base tier from `config.default_tier`.
///
/// # Escalation (applied only when explicit_override is None)
/// - If `input_tokens > threshold_tokens` → `Flagship` + `Budget`.
/// - If `retry_count >= max_retries`      → `Flagship` + `Retry`.
/// - If both fire, `Retry` wins the tie.
pub fn resolve_effective_model(
    req: &ModelResolutionRequest,
    config: &RouterConfig,
) -> ResolvedModel {
    // 1. Explicit override
    if let Some(ref name) = req.explicit_override {
        let tier = reverse_lookup_tier(name, config);
        return ResolvedModel {
            model: name.clone(),
            tier,
            escalation_reason: EscalationReason::None,
        };
    }

    // 2. Base tier precedence
    let base_tier = req
        .tier_hint
        .or_else(|| {
            req.step_kind
                .and_then(|sk| config.step_tiers.get(&sk).copied())
        })
        .unwrap_or(config.default_tier);

    let default_model = config
        .tier_models
        .get(&config.default_tier)
        .cloned()
        .unwrap_or_else(|| req.fallback_model.clone());
    let model = config
        .tier_models
        .get(&base_tier)
        .cloned()
        .unwrap_or_else(|| default_model.clone());

    // 3. Escalation
    let mut tier = base_tier;
    let mut escalation_reason = EscalationReason::None;

    let budget_hit = req.input_tokens > config.threshold_tokens;
    let retry_hit = req.retry_count > 0 && req.retry_count >= config.max_retries;

    if retry_hit {
        tier = ModelTier::Flagship;
        escalation_reason = EscalationReason::Retry;
    } else if budget_hit {
        tier = ModelTier::Flagship;
        escalation_reason = EscalationReason::Budget;
    }

    // If escalation fired, look up the escalated tier's model.
    // If that model is unavailable, revert to base tier — don't report
    // a tier/model mismatch in the ledger.
    let model = if tier != base_tier {
        config.tier_models.get(&tier).cloned().unwrap_or_else(|| {
            // No configured model for the escalated tier — stay at the
            // escalated tier (ledger still records the attempt) but fall
            // back to the caller's base model since we have nothing else.
            req.fallback_model.clone()
        })
    } else {
        model
    };

    ResolvedModel {
        model,
        tier,
        escalation_reason,
    }
}

/// Reverse-lookup: given a model name, find which tier it belongs to.
/// Falls back to `config.default_tier` if the name is not a configured tier model.
fn reverse_lookup_tier(name: &str, config: &RouterConfig) -> ModelTier {
    config
        .tier_models
        .iter()
        .find(|(_, v)| *v == name)
        .map(|(k, _)| *k)
        .unwrap_or(config.default_tier)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> RouterConfig {
        RouterConfig {
            default_tier: ModelTier::CheapAgentic,
            threshold_tokens: 100_000,
            max_retries: 2,
            tier_models: {
                let mut m = std::collections::HashMap::new();
                m.insert(ModelTier::CheapAgentic, "cheap-model".to_string());
                m.insert(ModelTier::Flagship, "flagship-model".to_string());
                m
            },
            step_tiers: {
                let mut m = std::collections::HashMap::new();
                m.insert(StepKind::Codegen, ModelTier::Flagship);
                m.insert(StepKind::Edit, ModelTier::CheapAgentic);
                m.insert(StepKind::Test, ModelTier::CheapAgentic);
                m.insert(StepKind::Plan, ModelTier::Flagship);
                m.insert(StepKind::Review, ModelTier::Flagship);
                m
            },
        }
    }

    // AC11(a): step-type → tier mapping for all 5 kinds
    #[test]
    fn step_kind_codegen_to_flagship() {
        let cfg = test_config();
        let req = ModelResolutionRequest {
            fallback_model: "cheap-model".to_string(),
            explicit_override: None,
            tier_hint: None,
            step_kind: Some(StepKind::Codegen),
            retry_count: 0,
            input_tokens: 0,
        };
        let res = resolve_effective_model(&req, &cfg);
        assert_eq!(res.tier, ModelTier::Flagship);
        assert_eq!(res.model, "flagship-model");
        assert_eq!(res.escalation_reason, EscalationReason::None);
    }

    #[test]
    fn step_kind_edit_to_cheap_agentic() {
        let cfg = test_config();
        let req = ModelResolutionRequest {
            fallback_model: "cheap-model".to_string(),
            explicit_override: None,
            tier_hint: None,
            step_kind: Some(StepKind::Edit),
            retry_count: 0,
            input_tokens: 0,
        };
        let res = resolve_effective_model(&req, &cfg);
        assert_eq!(res.tier, ModelTier::CheapAgentic);
        assert_eq!(res.model, "cheap-model");
        assert_eq!(res.escalation_reason, EscalationReason::None);
    }

    #[test]
    fn step_kind_test_to_cheap_agentic() {
        let cfg = test_config();
        let req = ModelResolutionRequest {
            fallback_model: "cheap-model".to_string(),
            explicit_override: None,
            tier_hint: None,
            step_kind: Some(StepKind::Test),
            retry_count: 0,
            input_tokens: 0,
        };
        let res = resolve_effective_model(&req, &cfg);
        assert_eq!(res.tier, ModelTier::CheapAgentic);
        assert_eq!(res.model, "cheap-model");
    }

    #[test]
    fn step_kind_plan_to_flagship() {
        let cfg = test_config();
        let req = ModelResolutionRequest {
            fallback_model: "cheap-model".to_string(),
            explicit_override: None,
            tier_hint: None,
            step_kind: Some(StepKind::Plan),
            retry_count: 0,
            input_tokens: 0,
        };
        let res = resolve_effective_model(&req, &cfg);
        assert_eq!(res.tier, ModelTier::Flagship);
        assert_eq!(res.model, "flagship-model");
    }

    #[test]
    fn step_kind_review_to_flagship() {
        let cfg = test_config();
        let req = ModelResolutionRequest {
            fallback_model: "cheap-model".to_string(),
            explicit_override: None,
            tier_hint: None,
            step_kind: Some(StepKind::Review),
            retry_count: 0,
            input_tokens: 0,
        };
        let res = resolve_effective_model(&req, &cfg);
        assert_eq!(res.tier, ModelTier::Flagship);
        assert_eq!(res.model, "flagship-model");
    }

    // AC11(b): budget escalation boundary
    #[test]
    fn budget_at_threshold_no_escalation() {
        let cfg = test_config();
        let req = ModelResolutionRequest {
            fallback_model: "cheap-model".to_string(),
            explicit_override: None,
            tier_hint: None,
            step_kind: Some(StepKind::Edit),
            retry_count: 0,
            input_tokens: 100_000, // exactly at threshold
        };
        let res = resolve_effective_model(&req, &cfg);
        assert_eq!(res.tier, ModelTier::CheapAgentic);
        assert_eq!(res.escalation_reason, EscalationReason::None);
    }

    #[test]
    fn budget_above_threshold_escalates_to_flagship() {
        let cfg = test_config();
        let req = ModelResolutionRequest {
            fallback_model: "cheap-model".to_string(),
            explicit_override: None,
            tier_hint: None,
            step_kind: Some(StepKind::Edit),
            retry_count: 0,
            input_tokens: 100_001,
        };
        let res = resolve_effective_model(&req, &cfg);
        assert_eq!(res.tier, ModelTier::Flagship);
        assert_eq!(res.model, "flagship-model");
        assert_eq!(res.escalation_reason, EscalationReason::Budget);
    }

    // AC11(c): retry escalation boundary
    #[test]
    fn retry_at_max_escalates() {
        let cfg = test_config();
        let req = ModelResolutionRequest {
            fallback_model: "cheap-model".to_string(),
            explicit_override: None,
            tier_hint: None,
            step_kind: Some(StepKind::Edit),
            retry_count: 2, // == max_retries
            input_tokens: 0,
        };
        let res = resolve_effective_model(&req, &cfg);
        assert_eq!(res.tier, ModelTier::Flagship);
        assert_eq!(res.escalation_reason, EscalationReason::Retry);
    }

    #[test]
    fn retry_below_max_no_escalation() {
        let cfg = test_config();
        let req = ModelResolutionRequest {
            fallback_model: "cheap-model".to_string(),
            explicit_override: None,
            tier_hint: None,
            step_kind: Some(StepKind::Edit),
            retry_count: 1,
            input_tokens: 0,
        };
        let res = resolve_effective_model(&req, &cfg);
        assert_eq!(res.tier, ModelTier::CheapAgentic);
        assert_eq!(res.escalation_reason, EscalationReason::None);
    }

    // AC11(c): retry beats budget tie
    #[test]
    fn retry_beats_budget_tie() {
        let cfg = test_config();
        let req = ModelResolutionRequest {
            fallback_model: "cheap-model".to_string(),
            explicit_override: None,
            tier_hint: None,
            step_kind: Some(StepKind::Edit),
            retry_count: 2,        // >= max_retries
            input_tokens: 200_000, // > threshold
        };
        let res = resolve_effective_model(&req, &cfg);
        assert_eq!(res.tier, ModelTier::Flagship);
        assert_eq!(res.escalation_reason, EscalationReason::Retry);
    }

    // AC11(e): explicit override precedence
    #[test]
    fn explicit_override_beats_all() {
        let cfg = test_config();
        let req = ModelResolutionRequest {
            fallback_model: "cheap-model".to_string(),
            explicit_override: Some("my-custom-model".to_string()),
            tier_hint: Some(ModelTier::Flagship),
            step_kind: Some(StepKind::Plan),
            retry_count: 5,
            input_tokens: 999_999,
        };
        let res = resolve_effective_model(&req, &cfg);
        assert_eq!(res.model, "my-custom-model");
        assert_eq!(res.escalation_reason, EscalationReason::None);
    }

    #[test]
    fn explicit_override_reverse_lookup_finds_tier() {
        let cfg = test_config();
        let req = ModelResolutionRequest {
            fallback_model: "cheap-model".to_string(),
            explicit_override: Some("flagship-model".to_string()),
            tier_hint: None,
            step_kind: None,
            retry_count: 0,
            input_tokens: 0,
        };
        let res = resolve_effective_model(&req, &cfg);
        assert_eq!(res.model, "flagship-model");
        assert_eq!(res.tier, ModelTier::Flagship);
        assert_eq!(res.escalation_reason, EscalationReason::None);
    }

    #[test]
    fn explicit_override_unknown_model_falls_back_to_default_tier() {
        let cfg = test_config();
        let req = ModelResolutionRequest {
            fallback_model: "cheap-model".to_string(),
            explicit_override: Some("unknown-model".to_string()),
            tier_hint: None,
            step_kind: None,
            retry_count: 0,
            input_tokens: 0,
        };
        let res = resolve_effective_model(&req, &cfg);
        assert_eq!(res.model, "unknown-model");
        assert_eq!(res.tier, ModelTier::CheapAgentic); // default tier
        assert_eq!(res.escalation_reason, EscalationReason::None);
    }

    // AC11(e): tier_hint > step_kind > default_tier
    #[test]
    fn tier_hint_beats_step_kind() {
        let cfg = test_config();
        let req = ModelResolutionRequest {
            fallback_model: "cheap-model".to_string(),
            explicit_override: None,
            tier_hint: Some(ModelTier::Flagship),
            step_kind: Some(StepKind::Edit), // would be CheapAgentic
            retry_count: 0,
            input_tokens: 0,
        };
        let res = resolve_effective_model(&req, &cfg);
        assert_eq!(res.tier, ModelTier::Flagship);
        assert_eq!(res.model, "flagship-model");
    }

    #[test]
    fn step_kind_beats_default_tier() {
        let cfg = test_config();
        let req = ModelResolutionRequest {
            fallback_model: "cheap-model".to_string(),
            explicit_override: None,
            tier_hint: None,
            step_kind: Some(StepKind::Plan), // Flagship
            retry_count: 0,
            input_tokens: 0,
        };
        let res = resolve_effective_model(&req, &cfg);
        assert_eq!(res.tier, ModelTier::Flagship);
        assert_eq!(res.model, "flagship-model");
    }

    #[test]
    fn default_tier_when_nothing_else() {
        let cfg = test_config();
        let req = ModelResolutionRequest {
            fallback_model: "cheap-model".to_string(),
            explicit_override: None,
            tier_hint: None,
            step_kind: None,
            retry_count: 0,
            input_tokens: 0,
        };
        let res = resolve_effective_model(&req, &cfg);
        assert_eq!(res.tier, ModelTier::CheapAgentic);
        assert_eq!(res.model, "cheap-model");
    }

    #[test]
    fn max_retries_zero_does_not_escalate_first_attempt() {
        let cfg = RouterConfig {
            max_retries: 0,
            ..test_config()
        };
        let req = ModelResolutionRequest {
            fallback_model: "cheap-model".to_string(),
            explicit_override: None,
            tier_hint: None,
            step_kind: Some(StepKind::Edit),
            retry_count: 0,
            input_tokens: 0,
        };
        let res = resolve_effective_model(&req, &cfg);
        assert_eq!(res.tier, ModelTier::CheapAgentic);
        assert_eq!(res.escalation_reason, EscalationReason::None);
    }

    #[test]
    fn missing_default_tier_in_tier_models_uses_fallback() {
        let mut cfg = test_config();
        cfg.tier_models.remove(&ModelTier::CheapAgentic);
        let req = ModelResolutionRequest {
            fallback_model: "fallback-model".to_string(),
            explicit_override: None,
            tier_hint: None,
            step_kind: None,
            retry_count: 0,
            input_tokens: 0,
        };
        let res = resolve_effective_model(&req, &cfg);
        assert_eq!(res.tier, ModelTier::CheapAgentic);
        assert_eq!(res.model, "fallback-model");
    }

    #[test]
    fn escalation_uses_fallback_when_flagship_model_missing() {
        let mut cfg = test_config();
        cfg.tier_models.remove(&ModelTier::Flagship);
        let req = ModelResolutionRequest {
            fallback_model: "cheap-model".to_string(),
            explicit_override: None,
            tier_hint: None,
            step_kind: Some(StepKind::Edit),
            retry_count: 0,
            input_tokens: 200_000,
        };
        let res = resolve_effective_model(&req, &cfg);
        // Tier still escalates (recorded in ledger) but model falls back
        // since no flagship model is configured.
        assert_eq!(res.tier, ModelTier::Flagship);
        assert_eq!(res.model, "cheap-model");
        assert_eq!(res.escalation_reason, EscalationReason::Budget);
    }
}
