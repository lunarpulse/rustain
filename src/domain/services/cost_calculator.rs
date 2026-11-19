//! Pure cost-arithmetic for usage-ledger entries (Story 7.5 AC10).
//!
//! ## Contract
//!
//! - **Pure** — no I/O, no clock, no logging, no `tokio`.
//! - **Cost is `Option<f64>`** — `None` means the model has no pricing entry,
//!   NOT zero. UI renders `None` as `"n/a"` (AC6).
//! - **Counting** — a ledger row's counts are disjoint (see
//!   `domain::models::usage::UsageInfo`): `tokens_in`, cache reads and cache
//!   writes are separate prompt tokens, and `reasoning_tokens` is a part of
//!   `tokens_out`. Every token is priced once, at its own rate.
//! - **Multiplier defaults** apply when `PricingConfig.cache_creation_per_million` /
//!   `cache_read_per_million` / `reasoning_per_million` are `None` (AC8):
//!   - `cache_creation` → 1.25 × input_per_million (Anthropic cache-write surcharge)
//!   - `cache_read`     → 0.10 × input_per_million (Anthropic 90%-off cache-read)
//!   - `reasoning`      → output_per_million         (the reasoning share of the
//!     output is billed at the output rate, so it adds nothing on top)

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

    // Reasoning is a part of the output: price the rest at the output rate and
    // the reasoning share at its own rate (the output rate unless overridden).
    let output = u.tokens_out as f64;
    let reasoning = u.reasoning_tokens.unwrap_or(0).min(u.tokens_out) as f64;
    let cost = u.tokens_in as f64 * input_rate
        + (output - reasoning) * output_rate
        + reasoning * reasoning_rate
        + u.cache_creation_tokens.unwrap_or(0) as f64 * cache_creation_rate
        + u.cache_read_tokens.unwrap_or(0) as f64 * cache_read_rate;

    Some(cost)
}

/// Cost for a simple `(model, tokens_in, tokens_out)` pair.
///
/// Used by lightweight read-only projections such as the Agent Inspector,
/// which have real token counts and a resolved model id but no cache/reasoning
/// breakdown. Returns `None` when pricing for `model` is unavailable.
pub fn cost_for_model_tokens(
    model: &str,
    tokens_in: u32,
    tokens_out: u32,
    pricing: &HashMap<String, PricingConfig>,
) -> Option<f64> {
    let p = crate::domain::services::pricing_resolver::lookup_pricing(pricing, model)?;
    let input_rate = p.input_per_million / 1_000_000.0;
    let output_rate = p.output_per_million / 1_000_000.0;
    Some(tokens_in as f64 * input_rate + tokens_out as f64 * output_rate)
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
        let Some(p) =
            crate::domain::services::pricing_resolver::lookup_pricing(pricing, &entry.model)
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

/// Test-only bridge used by the per-family decoder tests of eval-2: the usage a
/// decoder emitted, as the ledger row `run_turn` would append for that call,
/// priced with [`cost_for_entry`].
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::domain::models::router::{EscalationReason, ModelTier};
    use crate::domain::models::usage::{TokenUsage, UsageInfo};

    pub(crate) fn pricing(
        model: &str,
        input: f64,
        output: f64,
        cache_read: f64,
        cache_creation: Option<f64>,
    ) -> HashMap<String, PricingConfig> {
        let mut m = HashMap::new();
        m.insert(
            model.to_string(),
            PricingConfig {
                input_per_million: input,
                output_per_million: output,
                cache_creation_per_million: cache_creation,
                cache_read_per_million: Some(cache_read),
                reasoning_per_million: None,
            },
        );
        m
    }

    /// The ledger row for one call (the mapping of `infrastructure/runtime/turn.rs`).
    pub(crate) fn ledger_row(usage: &UsageInfo, model: &str) -> UsageLedgerEntry {
        UsageLedgerEntry {
            timestamp_ms: 0,
            session_id: "s".into(),
            conversation_id: "c".into(),
            provider_id: "p".into(),
            model: model.to_string(),
            tier: ModelTier::Flagship,
            step_kind: None,
            escalation_reason: EscalationReason::None,
            usage: TokenUsage::from_usage(usage, 0),
            provenance: None,
        }
    }

    /// USD of one call: decoder output → ledger row → `cost_for_entry`.
    pub(crate) fn cost_of(
        usage: &UsageInfo,
        model: &str,
        pricing: &HashMap<String, PricingConfig>,
    ) -> f64 {
        cost_for_entry(&ledger_row(usage, model), pricing).expect("priced")
    }

    pub(crate) fn assert_usd(got: f64, want: f64, what: &str) {
        assert!((got - want).abs() < 1e-12, "{what}: got {got}, want {want}");
    }
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
            provenance: None,
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
        // sonnet: reasoning_per_million unset → output_per_million = 15.0.
        // Reasoning is a part of `tokens_out` (see `UsageInfo`), so 1M output
        // tokens of which 1M reasoning cost $15 once, not $30.
        let mut e = entry("sonnet", 0, 1_000_000);
        e.usage.reasoning_tokens = Some(1_000_000);
        let cost = cost_for_entry(&e, &test_pricing()).expect("priced");
        assert!(
            (cost - 15.0).abs() < 1e-9,
            "reasoning counted once at the output rate failed: cost={cost}"
        );
    }

    #[test]
    fn cost_for_entry_reasoning_is_clamped_to_output() {
        // A reasoning count above the output it is part of cannot be billed.
        let mut e = entry("sonnet", 0, 0);
        e.usage.reasoning_tokens = Some(1_000_000);
        let cost = cost_for_entry(&e, &test_pricing()).expect("priced");
        assert!(cost.abs() < 1e-12, "clamp failed: cost={cost}");
    }

    #[test]
    fn cost_for_entry_clamps_over_reported_reasoning_at_its_own_rate() {
        // Reasoning above the output it is part of is billed as at most the
        // output: with output 0 nothing is owed, whatever the reasoning rate.
        // (Output rate $15, reasoning rate $20: an unclamped count would give
        // (0 − 1M) × 15/M + 1M × 20/M = $5.)
        let mut pricing = test_pricing();
        pricing.get_mut("sonnet").unwrap().reasoning_per_million = Some(20.0);
        let mut e = entry("sonnet", 0, 0);
        e.usage.reasoning_tokens = Some(1_000_000);
        assert_usd(
            cost_for_entry(&e, &pricing).expect("priced"),
            0.0,
            "output 0",
        );
        // Partly over-reported: output 400k, reasoning 1M → 400k at the reasoning rate.
        let mut e = entry("sonnet", 0, 400_000);
        e.usage.reasoning_tokens = Some(1_000_000);
        assert_usd(
            cost_for_entry(&e, &pricing).expect("priced"),
            8.0,
            "output 400k",
        );
    }

    /// DeepSeek list prices (SCP 2026-11-13 §1.2): input $0.30, cache hit
    /// $0.006, output $1.20 per million; `reasoning_per_million` as given.
    fn deepseek_pricing(reasoning: Option<f64>) -> HashMap<String, PricingConfig> {
        let mut m = HashMap::new();
        m.insert(
            "deepseek-v4-flash".to_string(),
            PricingConfig {
                input_per_million: 0.30,
                output_per_million: 1.20,
                cache_creation_per_million: None,
                cache_read_per_million: Some(0.006),
                reasoning_per_million: reasoning,
            },
        );
        m
    }

    fn deepseek_entry(
        tokens_in: u32,
        tokens_out: u32,
        cache_read: u32,
        reasoning: u32,
    ) -> UsageLedgerEntry {
        let mut e = entry("deepseek-v4-flash", tokens_in, tokens_out);
        e.usage.cache_read_tokens = Some(cache_read);
        e.usage.reasoning_tokens = Some(reasoning);
        e
    }

    use super::test_support::assert_usd;

    #[test]
    fn cost_for_entry_counts_each_token_once_for_a_deepseek_row() {
        // Normalised row of the owner-style call: 1289 uncached input, 3959
        // output (3803 of it reasoning), 2176 cache reads.
        let e = deepseek_entry(1289, 3959, 2176, 3803);
        assert_usd(
            cost_for_entry(&e, &deepseek_pricing(None)).expect("priced"),
            0.005150556,
            "default reasoning rate",
        );
        // An explicit reasoning rate re-prices the reasoning part of the output only.
        assert_usd(
            cost_for_entry(&e, &deepseek_pricing(Some(2.00))).expect("priced"),
            0.008192956,
            "reasoning_per_million = 2.00",
        );
    }

    #[test]
    fn cost_for_entry_anthropic_row_control_is_unchanged() {
        // Disjoint Anthropic row: 141 input, 10 output, 1408 read, 2000 written.
        let mut e = entry("sonnet", 141, 10);
        e.usage.cache_read_tokens = Some(1408);
        e.usage.cache_creation_tokens = Some(2000);
        assert_usd(
            cost_for_entry(&e, &test_pricing()).expect("priced"),
            0.0084954,
            "anthropic control",
        );
    }

    /// The five eval-1 baseline trials, normalised (`tokens_in` = uncached):
    /// `(input, output, cache_read, reasoning, correct USD)`. Aggregates of the
    /// 97 archived `call_usage` lines; every token is priced once.
    const BASELINE_TRIALS: [(u32, u32, u32, u32, f64); 5] = [
        (7_282, 6_396, 71_424, 4_744, 0.010288344),
        (5_317, 2_819, 88_448, 1_177, 0.005508588),
        (17_520, 29_320, 450_304, 23_177, 0.043141824),
        (9_702, 18_402, 149_248, 12_331, 0.025888488),
        (33_703, 48_043, 1_430_784, 37_572, 0.076347204),
    ];

    #[test]
    fn baseline_trials_cost_what_the_provider_bills() {
        let pricing = deepseek_pricing(None);
        let entries: Vec<_> = BASELINE_TRIALS
            .iter()
            .map(|&(i, o, c, r, _)| deepseek_entry(i, o, c, r))
            .collect();
        for (e, t) in entries.iter().zip(BASELINE_TRIALS.iter()) {
            assert_usd(cost_for_entry(e, &pricing).expect("priced"), t.4, "trial");
        }
        let per_entry: f64 = entries
            .iter()
            .map(|e| cost_for_entry(e, &pricing).unwrap())
            .sum();
        assert_usd(per_entry, 0.161174448, "sum of the five trials");

        // The breakdown, the cumulative cost and one summed row agree with the sum.
        let bd = cost_breakdown(&entries, &pricing);
        assert_usd(bd.total_usd, per_entry, "cost_breakdown total");
        assert_usd(cumulative_cost(&entries, &pricing), per_entry, "cumulative");
        let summed = deepseek_entry(73_524, 104_980, 2_190_208, 79_001);
        assert_usd(
            cost_for_entry(&summed, &pricing).expect("priced"),
            per_entry,
            "one summed row (linearity)",
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
