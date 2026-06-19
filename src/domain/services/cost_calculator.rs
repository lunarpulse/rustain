//! Pure cost-arithmetic for usage-ledger entries (Story 7.5 AC10).
//!
//! ## Contract
//!
//! - **Pure** — no I/O, no clock, no logging, no `tokio`.
//! - **Cost is `Option<f64>`** — `None` means the model has no pricing entry,
//!   NOT zero. UI renders `None` as `"n/a"` (AC6).
//! - **Multiplier defaults** apply when `PricingConfig.cache_creation_per_million` /
//!   `cache_read_per_million` / `reasoning_per_million` are `None` (AC8):
//!   - `cache_creation` → 1.25 × input_per_million (Anthropic cache-write surcharge)
//!   - `cache_read`     → 0.10 × input_per_million (Anthropic 90%-off cache-read)
//!   - `reasoning`      → output_per_million         (billed at output rate)

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::domain::models::pricing::PricingConfig;
use crate::domain::models::usage::UsageLedgerEntry;

/// Per-model aggregate (counts + summed cost).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelCost {
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// `None` if any contributing entry's model has no pricing entry.
    pub cost_usd: Option<f64>,
    pub call_count: u32,
}

/// Aggregated cost breakdown across a slice of ledger entries.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CostBreakdown {
    pub total_usd: f64,
    pub per_model: BTreeMap<String, ModelCost>,
    /// Deduplicated list of model_ids that lacked a pricing entry.
    pub missing_pricing_models: Vec<String>,
}

/// Compute cost in USD for a single ledger entry, or `None` if the model
/// has no pricing entry (AC6 graceful degradation).
pub fn cost_for_entry(
    entry: &UsageLedgerEntry,
    pricing: &HashMap<String, PricingConfig>,
) -> Option<f64> {
    let p = crate::domain::services::pricing_resolver::lookup_pricing(pricing, &entry.model)?;
    let u = &entry.usage;

    let input_rate = p.input_per_million / 1_000_000.0;
    let output_rate = p.output_per_million / 1_000_000.0;
    let cache_creation_rate = p
        .cache_creation_per_million
        .unwrap_or(p.input_per_million * 1.25)
        / 1_000_000.0;
    let cache_read_rate = p
        .cache_read_per_million
        .unwrap_or(p.input_per_million * 0.10)
        / 1_000_000.0;
    let reasoning_rate = p.reasoning_per_million.unwrap_or(p.output_per_million) / 1_000_000.0;

    let cost = u.tokens_in as f64 * input_rate
        + u.tokens_out as f64 * output_rate
        + u.cache_creation_tokens.unwrap_or(0) as f64 * cache_creation_rate
        + u.cache_read_tokens.unwrap_or(0) as f64 * cache_read_rate
        + u.reasoning_tokens.unwrap_or(0) as f64 * reasoning_rate;

    Some(cost)
}

/// Aggregate `entries` into per-model totals + cumulative cost, collecting
/// any model_ids with missing pricing into `missing_pricing_models`.
pub fn cost_breakdown(
    entries: &[UsageLedgerEntry],
    pricing: &HashMap<String, PricingConfig>,
) -> CostBreakdown {
    let mut per_model: BTreeMap<String, ModelCost> = BTreeMap::new();
    let mut missing: HashSet<String> = HashSet::new();
    let mut total: f64 = 0.0;

    for entry in entries {
        let row = per_model.entry(entry.model.clone()).or_default();
        row.tokens_in = row.tokens_in.saturating_add(entry.usage.tokens_in as u64);
        row.tokens_out = row.tokens_out.saturating_add(entry.usage.tokens_out as u64);
        row.call_count = row.call_count.saturating_add(1);

        match cost_for_entry(entry, pricing) {
            Some(c) => {
                row.cost_usd = Some(row.cost_usd.unwrap_or(0.0) + c);
                total += c;
            }
            None => {
                missing.insert(entry.model.clone());
            }
        }
    }

    let mut missing_vec: Vec<String> = missing.into_iter().collect();
    missing_vec.sort();

    CostBreakdown {
        total_usd: total,
        per_model,
        missing_pricing_models: missing_vec,
    }
}

/// Sum of `cost_for_entry` over `entries`, skipping `None`s (Story 7.5 AC10).
pub fn cumulative_cost(
    entries: &[UsageLedgerEntry],
    pricing: &HashMap<String, PricingConfig>,
) -> f64 {
    entries
        .iter()
        .filter_map(|e| cost_for_entry(e, pricing))
        .sum()
}

/// Estimated USD saved by Anthropic prompt-caching across a slice of entries
/// (Story 7.5 AC3, Dev Notes §"Cache savings calculation"). Pure.
///
/// For each entry with `cache_read_tokens > 0`, savings = `cache_read_tokens *
/// (input_rate - cache_read_rate)`. Entries without pricing contribute 0.
pub fn cache_savings(
    entries: &[UsageLedgerEntry],
    pricing: &HashMap<String, PricingConfig>,
) -> f64 {
    let mut saved: f64 = 0.0;
    for entry in entries {
        let Some(p) = crate::domain::services::pricing_resolver::lookup_pricing(pricing, &entry.model)
        else {
            continue;
        };
        let cache_read = entry.usage.cache_read_tokens.unwrap_or(0) as f64;
        if cache_read <= 0.0 {
            continue;
        }
        let input_rate = p.input_per_million / 1_000_000.0;
        let cache_read_rate = p
            .cache_read_per_million
            .unwrap_or(p.input_per_million * 0.10)
            / 1_000_000.0;
        saved += cache_read * (input_rate - cache_read_rate);
    }
    saved
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::router::{EscalationReason, ModelTier};
    use crate::domain::models::usage::TokenUsage;

    fn test_pricing() -> HashMap<String, PricingConfig> {
        let mut m = HashMap::new();
        m.insert(
            "sonnet".to_string(),
            PricingConfig {
                input_per_million: 3.0,
                output_per_million: 15.0,
                cache_creation_per_million: None, // → 1.25 × 3.0 = 3.75
                cache_read_per_million: None,     // → 0.10 × 3.0 = 0.30
                reasoning_per_million: None,      // → 15.0
            },
        );
        m.insert(
            "haiku".to_string(),
            PricingConfig {
                input_per_million: 0.80,
                output_per_million: 4.00,
                cache_creation_per_million: Some(1.00),
                cache_read_per_million: Some(0.08),
                reasoning_per_million: None, // → 4.00
            },
        );
        m
    }

    fn entry(model: &str, tokens_in: u32, tokens_out: u32) -> UsageLedgerEntry {
        UsageLedgerEntry {
            timestamp_ms: 0,
            session_id: "s".into(),
            conversation_id: "c".into(),
            provider_id: "p".into(),
            model: model.to_string(),
            tier: ModelTier::Flagship,
            step_kind: None,
            escalation_reason: EscalationReason::None,
            usage: TokenUsage {
                tokens_in,
                tokens_out,
                parent_ctx: 0,
                cache_creation_tokens: None,
                cache_read_tokens: None,
                reasoning_tokens: None,
            },
        }
    }

    #[test]
    fn cost_for_entry_returns_none_when_pricing_missing() {
        let entry = entry("unknown-model", 1000, 500);
        let empty: HashMap<String, PricingConfig> = HashMap::new();
        assert_eq!(cost_for_entry(&entry, &empty), None);
        let pricing = test_pricing();
        assert_eq!(cost_for_entry(&entry, &pricing), None);
    }

    #[test]
    fn cost_for_entry_basic_arithmetic() {
        // sonnet: $3/$15 per million
        // 1_000_000 input + 500_000 output = $3 + $7.50 = $10.50
        let e = entry("sonnet", 1_000_000, 500_000);
        let cost = cost_for_entry(&e, &test_pricing()).expect("priced");
        assert!((cost - 10.50).abs() < 1e-9, "cost was {cost}, want 10.50");
    }

    #[test]
    fn cost_for_entry_cache_creation_default_is_1_25x_input() {
        // sonnet: cache_creation_per_million unset → 1.25 × 3.0 = 3.75/M
        // 1M cache_creation tokens at $3.75/M = $3.75
        let mut e = entry("sonnet", 0, 0);
        e.usage.cache_creation_tokens = Some(1_000_000);
        let cost = cost_for_entry(&e, &test_pricing()).expect("priced");
        assert!(
            (cost - 3.75).abs() < 1e-9,
            "cache_creation default 1.25× failed: cost={cost}"
        );
    }

    #[test]
    fn cost_for_entry_cache_read_default_is_0_10x_input() {
        // sonnet: cache_read_per_million unset → 0.10 × 3.0 = 0.30/M
        let mut e = entry("sonnet", 0, 0);
        e.usage.cache_read_tokens = Some(1_000_000);
        let cost = cost_for_entry(&e, &test_pricing()).expect("priced");
        assert!(
            (cost - 0.30).abs() < 1e-9,
            "cache_read default 0.10× failed: cost={cost}"
        );
    }

    #[test]
    fn cost_for_entry_reasoning_default_is_output_rate() {
        // sonnet: reasoning_per_million unset → output_per_million = 15.0
        let mut e = entry("sonnet", 0, 0);
        e.usage.reasoning_tokens = Some(1_000_000);
        let cost = cost_for_entry(&e, &test_pricing()).expect("priced");
        assert!(
            (cost - 15.0).abs() < 1e-9,
            "reasoning default = output rate failed: cost={cost}"
        );
    }

    #[test]
    fn cost_breakdown_groups_by_model() {
        let entries = vec![
            entry("sonnet", 1_000_000, 0),  // $3.00
            entry("sonnet", 0, 1_000_000),  // $15.00
            entry("haiku", 1_000_000, 0),   // $0.80
            entry("unknown", 1_000_000, 0), // None
        ];
        let bd = cost_breakdown(&entries, &test_pricing());

        let sonnet = bd.per_model.get("sonnet").expect("sonnet row");
        assert_eq!(sonnet.tokens_in, 1_000_000);
        assert_eq!(sonnet.tokens_out, 1_000_000);
        assert_eq!(sonnet.call_count, 2);
        assert!((sonnet.cost_usd.unwrap() - 18.0).abs() < 1e-9);

        let haiku = bd.per_model.get("haiku").expect("haiku row");
        assert!((haiku.cost_usd.unwrap() - 0.80).abs() < 1e-9);

        let unknown = bd.per_model.get("unknown").expect("unknown row exists");
        assert_eq!(unknown.tokens_in, 1_000_000);
        assert_eq!(unknown.cost_usd, None);

        assert!((bd.total_usd - 18.80).abs() < 1e-9);
        assert_eq!(bd.missing_pricing_models, vec!["unknown".to_string()]);
    }

    #[test]
    fn cost_breakdown_missing_pricing_models_is_deduplicated() {
        let entries = vec![
            entry("unknown", 100, 0),
            entry("unknown", 200, 0),
            entry("also-unknown", 50, 0),
            entry("unknown", 10, 0),
        ];
        let bd = cost_breakdown(&entries, &test_pricing());
        assert_eq!(
            bd.missing_pricing_models,
            vec!["also-unknown".to_string(), "unknown".to_string()]
        );
        assert_eq!(bd.total_usd, 0.0);
    }

    #[test]
    fn cumulative_cost_skips_missing_pricing() {
        let entries = vec![
            entry("sonnet", 1_000_000, 0), // $3.00
            entry("unknown", 999_999, 0),  // skipped
            entry("haiku", 1_000_000, 0),  // $0.80
        ];
        let total = cumulative_cost(&entries, &test_pricing());
        assert!((total - 3.80).abs() < 1e-9, "got {total}");
    }

    #[test]
    fn cache_savings_zero_when_no_cache_reads() {
        let entries = vec![entry("sonnet", 1_000_000, 1_000_000)];
        assert_eq!(cache_savings(&entries, &test_pricing()), 0.0);
    }

    #[test]
    fn cache_savings_uses_input_minus_cache_read_rate() {
        // sonnet: input_rate $3/M, cache_read default 0.30/M → savings $2.70/M
        let mut e = entry("sonnet", 0, 0);
        e.usage.cache_read_tokens = Some(1_000_000);
        let saved = cache_savings(&[e], &test_pricing());
        assert!(
            (saved - 2.70).abs() < 1e-9,
            "cache_savings expected $2.70, got {saved}"
        );
    }
}
