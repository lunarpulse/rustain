//! Daily-budget tracking handlers — Story 7.5 (decision half moved to the
//! interim decision pane by Story 21.1 S2).
//!
//! Two `handle_*` functions return `HandlerOutcome::Quiet` (pure state mutation
//! on `state.decisions`, the interim decision pane):
//! - `handle_upsert_daily_budget_warning` (formerly `upsert_daily_budget_warning`)
//! - `handle_clear_daily_budget_warning` (formerly `clear_daily_budget_warning`)
//!
//! Plus one `pub(super)` async helper per DGI-A (returns `Option<DailyBudgetState>`,
//! NOT a handler returning `HandlerOutcome`):
//! - `recompute_daily_budget` — uses `&dyn UsageLedgerPort` for entry reads
//!   (domain port — satisfies AC-5 isolation without `&AppState` import)

#![allow(dead_code)]

use crate::adapters::tui::decision_pane::{self, Arrival, DecisionDraft, DecisionKind};
use crate::adapters::tui::state::{DailyBudgetState, TuiState};
use crate::domain::models::AppConfig;
use crate::domain::ports::{ProviderInfoPort, UsageLedgerPort};
use crate::domain::services::cost_calculator;

use super::HandlerOutcome;

/// Story 7.5 AC5: hold the daily-budget decision when over the daily limit.
/// At most one exists at a time (an update replaces its text in place; a
/// deferral is never resurrected). Suppressed when `unix_now <= dismissed_until_unix`.
pub fn handle_upsert_daily_budget_warning(
    state: &mut TuiState,
    budget: &DailyBudgetState,
    info: &dyn ProviderInfoPort,
) -> HandlerOutcome {
    let now = info.now_unix();
    let pct = budget.percent();
    let paused = now <= budget.dismissed_until_unix;

    if pct < 100 || paused {
        return handle_clear_daily_budget_warning(state);
    }

    decision_pane::raise(
        state,
        DecisionDraft::new(
            DecisionKind::Budget,
            format!("Daily budget (${:.2}) reached.", budget.limit_usd),
        ),
        Arrival::Automatic,
    );
    HandlerOutcome::Quiet
}

/// Story 7.5 AC5: clear any daily-budget decision.
pub fn handle_clear_daily_budget_warning(state: &mut TuiState) -> HandlerOutcome {
    decision_pane::withdraw(state, DecisionKind::Budget);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tui::decision_pane::DecisionKind;
    use crate::adapters::tui::notifications::NotificationStore;
    use crate::domain::clock::MockClock;
    use crate::domain::models::NotificationsConfig;
    use crate::domain::models::provider::{ModelDescriptor, ProviderDescriptor};
    use crate::domain::ports::StreamingProvider;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    struct FixedNow(i64);
    impl ProviderInfoPort for FixedNow {
        fn active_delegate_id(&self) -> String {
            String::new()
        }
        fn get_model(&self, _: &str, _: &str) -> Option<ModelDescriptor> {
            None
        }
        fn get_model_provider(&self, _: &str, _: Option<&str>) -> Option<String> {
            None
        }
        fn list_providers(&self) -> Vec<ProviderDescriptor> {
            vec![]
        }
        fn list_models_by_provider(&self, _: &str) -> Vec<ModelDescriptor> {
            vec![]
        }
        fn get_provider(&self, _: &str) -> Option<Arc<dyn StreamingProvider>> {
            None
        }
        fn set_active_provider(&self, _: &str) -> Result<(), crate::domain::errors::ProviderError> {
            Ok(())
        }
        fn now_unix(&self) -> i64 {
            self.0
        }
        fn today_start_unix_ms(&self) -> i64 {
            0
        }
    }

    fn budget(spent: f64, dismissed_until_unix: i64) -> DailyBudgetState {
        DailyBudgetState {
            spent_today_usd: spent,
            limit_usd: 10.0,
            computed_at_ms: 0,
            dismissed_until_unix,
        }
    }

    fn clocked() -> (TuiState, Arc<MockClock>) {
        let clock = Arc::new(MockClock::new(Instant::now()));
        let mut state = TuiState::new(80, 24);
        state.notifications =
            NotificationStore::from_config(&NotificationsConfig::default(), clock.clone());
        (state, clock)
    }

    #[test]
    fn story21_1_over_budget_raises_one_pane_item_and_paused_or_under_clears_it() {
        let (mut state, _clock) = clocked();
        let info = FixedNow(1_000);
        handle_upsert_daily_budget_warning(&mut state, &budget(11.0, 0), &info);
        let item = state.decisions.items().next().expect("the budget decision");
        assert_eq!(item.kind, DecisionKind::Budget);
        assert!(item.detail.contains("$10.00"), "{}", item.detail);
        assert!(
            state.decisions.is_focused(),
            "0→1 on an idle composer takes the pane"
        );

        // The next turn's recompute updates the text in place: still one item.
        handle_upsert_daily_budget_warning(&mut state, &budget(12.0, 0), &info);
        assert_eq!(state.decisions.len(), 1);

        // Paused until after now → cleared; under the limit → cleared.
        handle_upsert_daily_budget_warning(&mut state, &budget(12.0, 2_000), &info);
        assert!(state.decisions.is_empty(), "paused");
        handle_upsert_daily_budget_warning(&mut state, &budget(11.0, 0), &info);
        handle_upsert_daily_budget_warning(&mut state, &budget(1.0, 0), &info);
        assert!(state.decisions.is_empty(), "under the limit");
    }

    #[test]
    fn story21_1_a_budget_arrival_never_steals_a_typing_composer_and_a_deferral_is_not_resurrected()
    {
        let (mut state, clock) = clocked();
        let info = FixedNow(1_000);
        state.focus = crate::domain::models::FocusState::Input;
        state.input_buffer = "half a sentence".into();
        handle_upsert_daily_budget_warning(&mut state, &budget(11.0, 0), &info);
        assert!(
            !state.decisions.is_focused(),
            "a non-empty composer is protected"
        );
        assert!(
            state.decisions.holds(DecisionKind::Budget),
            "the item waits"
        );
        assert_eq!(
            state.input_buffer, "half a sentence",
            "the draft is untouched"
        );
        assert!(
            state
                .notifications
                .visible()
                .any(|r| r.message.contains("Ctrl+X q")),
            "one toast says a decision is waiting"
        );
        // Recomputing every turn never re-announces or re-focuses it.
        clock.advance(Duration::from_secs(30));
        handle_upsert_daily_budget_warning(&mut state, &budget(11.5, 0), &info);
        assert!(!state.decisions.is_focused());
        assert_eq!(
            state
                .notifications
                .visible()
                .filter(|r| r.message.contains("Ctrl+X q"))
                .count(),
            1
        );
    }
}
