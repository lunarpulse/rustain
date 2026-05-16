//! Daily budget configuration (Story 7.5 AC5).
//!
//! `BudgetConfig` is loaded from the `[budget]` TOML section on `AppConfig`.
//! When `daily_limit_usd` is `None`, all budget alerting is disabled
//! (the panel still shows cumulative cost for reference).
//!
//! The pause-state (dismissed_until_unix) lives separately in
//! `~/.rustain/budget_state.json`, persisted via `BudgetStateStore`.

use serde::{Deserialize, Serialize};

/// Daily budget configuration.
///
/// When `Some(limit)`, the runtime tracks cumulative cost across the current
/// calendar day (local TZ) and surfaces yellow (≥80%) / red (≥100%) warnings.
/// The advisory NEVER blocks sending a turn — see AC5 in story 7.5.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BudgetConfig {
    /// Daily spending limit in USD. `None` disables budget alerting.
    #[serde(default, alias = "daily_limit_usd")]
    pub daily_limit_usd: Option<f64>,
}
