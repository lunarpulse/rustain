//! Daily-budget tracking handlers — Story 7.5.
//!
//! Two `handle_*` functions return `HandlerOutcome::Quiet` (pure state mutation
//! on `state.feedback_blocks` + `state.active_feedback_id`):
//! - `handle_upsert_daily_budget_warning` (formerly `upsert_daily_budget_warning`)
//! - `handle_clear_daily_budget_warning` (formerly `clear_daily_budget_warning`)
//!
//! Plus one `pub(super)` async helper per DGI-A (returns `Option<DailyBudgetState>`,
//! NOT a handler returning `HandlerOutcome`):
//! - `recompute_daily_budget` — uses `&dyn UsageLedgerPort` for entry reads
//!   (domain port — satisfies AC-5 isolation without `&AppState` import)

#![allow(dead_code)]

use crate::adapters::tui::state::{DailyBudgetState, TuiState};
use crate::domain::models::{AppConfig, FeedbackAction, FeedbackBlock, FeedbackLevel};
use crate::domain::ports::{ProviderInfoPort, UsageLedgerPort};
use crate::domain::services::cost_calculator;

use super::HandlerOutcome;

/// Story 7.5 AC5: upsert a daily-budget warning feedback block when over the
/// daily limit. Removes any prior `dailybudget-*` block first (at most one
/// exists at a time). Suppressed when `unix_now <= dismissed_until_unix`.
pub fn handle_upsert_daily_budget_warning(
    state: &mut TuiState,
    budget: &DailyBudgetState,
    info: &dyn ProviderInfoPort,
) -> HandlerOutcome {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static BUDGET_WARN_COUNTER: AtomicUsize = AtomicUsize::new(0);

    let now = info.now_unix();
    let pct = budget.percent();
    let paused = now <= budget.dismissed_until_unix;

    if pct < 100 || paused {
        return handle_clear_daily_budget_warning(state);
    }

    // Remove any existing dailybudget-* block (at-most-one invariant)
    state
        .feedback_blocks
        .retain(|id, _| !id.starts_with("dailybudget-"));
    if state
        .active_feedback_id
        .as_deref()
        .is_some_and(|id| id.starts_with("dailybudget-"))
    {
        state.active_feedback_id = None;
    }

    let fb_id = format!(
        "dailybudget-{}",
        BUDGET_WARN_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let fb = FeedbackBlock {
        id: fb_id.clone(),
        level: FeedbackLevel::Error,
        message: format!("Daily budget (${:.2}) reached.", budget.limit_usd),
        actions: vec![
            FeedbackAction::BudgetContinue,
            FeedbackAction::BudgetSwitchCheaper,
            FeedbackAction::BudgetPause,
        ],
    };
    state.feedback_blocks.insert(fb_id.clone(), fb);
    // Note: this unconditionally sets active_feedback_id. If a ctxwarn-*
    // block from 7-4 was active, it gets displaced (remains in feedback_blocks
    // but is no longer "active"). Budget warning takes priority per AC5.
    state.active_feedback_id = Some(fb_id);
    HandlerOutcome::Quiet
}

/// Story 7.5 AC5: clear any daily-budget warning block.
pub fn handle_clear_daily_budget_warning(state: &mut TuiState) -> HandlerOutcome {
    state
        .feedback_blocks
        .retain(|id, _| !id.starts_with("dailybudget-"));
    if state
        .active_feedback_id
        .as_deref()
        .is_some_and(|id| id.starts_with("dailybudget-"))
    {
        state.active_feedback_id = None;
    }
    HandlerOutcome::Quiet
}

/// Story 7.5 AC5: recompute spent-today + percent against `config.budget.daily_limit_usd`.
/// Returns `None` when budget alerting is disabled (no `daily_limit_usd`).
///
/// Per DGI-A (Winston Task 1 sign-off): pure async compute helper, NOT a handler.
/// Callers consume the returned `Option<DailyBudgetState>` and decide what to do
/// (typically: feed into `handle_upsert_daily_budget_warning` + assign to
/// `state.daily_budget`).
pub(crate) async fn recompute_daily_budget(
    usage_ledger: &dyn UsageLedgerPort,
    config: &AppConfig,
    prior_dismissed_until_unix: i64,
    info: &dyn ProviderInfoPort,
) -> Option<DailyBudgetState> {
    let limit = config.budget.daily_limit_usd?;
    let since = info.today_start_unix_ms();
    let entries = usage_ledger.read_since(since).await.unwrap_or_default();
    let spent = cost_calculator::cumulative_cost(&entries, &config.pricing);
    Some(DailyBudgetState {
        spent_today_usd: spent,
        limit_usd: limit,
        computed_at_ms: chrono::Utc::now().timestamp_millis(),
        dismissed_until_unix: prior_dismissed_until_unix,
    })
}
