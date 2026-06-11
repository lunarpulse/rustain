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
///
/// **Serialization:** snake_case canonical (matches TOML idiom + user configs).
/// The prior `rename_all = "camelCase"` + alias dual-form support was dropped
/// post-Epic-7 to fix a figment merge conflict — when both the defaults layer
/// (camelCase canonical) and a user TOML layer (snake_case alias) define the
/// same field, figment produces a value tree with both keys and serde rejects
/// it as a duplicate field. See Epic 7 retro AI-7.2 root-cause analysis.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BudgetConfig {
    /// Daily spending limit in USD. `None` disables budget alerting.
    #[serde(default, alias = "dailyLimitUsd")]
    pub daily_limit_usd: Option<f64>,
}
