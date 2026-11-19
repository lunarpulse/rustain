//! Pure daily-budget calculations shared by interactive and unattended turns.

use crate::domain::models::{AppConfig, usage::UsageLedgerEntry};
use crate::domain::services::cost_calculator;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BudgetStop {
    pub limit_usd: f64,
    pub spent_usd: f64,
}

/// Sum priced ledger entries; callers select the relevant day.
pub fn daily_spend(entries: &[UsageLedgerEntry], config: &AppConfig) -> f64 {
    cost_calculator::cumulative_cost(entries, &config.pricing)
}

/// An unset or non-finite limit disables the unattended-turn gate.
pub fn daily_budget_stop(entries: &[UsageLedgerEntry], config: &AppConfig) -> Option<BudgetStop> {
    let limit_usd = config
        .budget
        .daily_limit_usd
        .filter(|limit| limit.is_finite())?;
    let spent_usd = daily_spend(entries, config);
    (spent_usd >= limit_usd).then_some(BudgetStop {
        limit_usd,
        spent_usd,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::usage::UsageInfo;
    use crate::domain::services::cost_calculator::test_support::{ledger_row, pricing};

    #[test]
    fn daily_stop_threshold_and_disabled_limits() {
        let mut config = AppConfig::default();
        config.pricing = pricing("model", 1.0, 0.0, 0.0, None);
        let usage = UsageInfo {
            input_tokens: 1_000_000,
            output_tokens: 0,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            reasoning_tokens: None,
            counters_observed: false,
        };
        let entries = [ledger_row(&usage, "model")];
        for (limit, stopped) in [
            (None, false),
            (Some(1.01), false),
            (Some(1.0), true),
            (Some(0.99), true),
            (Some(f64::NAN), false),
            (Some(f64::INFINITY), false),
            (Some(f64::NEG_INFINITY), false),
        ] {
            config.budget.daily_limit_usd = limit;
            let result = daily_budget_stop(&entries, &config);
            assert_eq!(result.is_some(), stopped, "limit: {limit:?}");
            if let Some(stop) = result {
                assert_eq!(stop.limit_usd, limit.unwrap());
                assert_eq!(stop.spent_usd, 1.0);
            }
        }
    }
}
