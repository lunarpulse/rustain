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

use crate::adapters::tui::decision_queue::{self, Arrival, DecisionDraft, DecisionKind};
use crate::adapters::tui::state::{DailyBudgetState, TuiState};
use crate::domain::models::AppConfig;
use crate::domain::ports::{ProviderInfoPort, UsageLedgerPort};
use crate::domain::services::cost_calculator;

use super::HandlerOutcome;

/// Story 7.5 AC5: hold the daily-budget decision when over the daily limit.
/// At most one exists at a time (an update replaces its text in place; a
/// deferral is never resurrected). Suppressed when `unix_now <= dismissed_until_unix`
/// and while the user's `Continue` of this day and limit holds (A18).
pub fn handle_upsert_daily_budget_warning(
    state: &mut TuiState,
    budget: &DailyBudgetState,
    info: &dyn ProviderInfoPort,
) -> HandlerOutcome {
    let now = info.now_unix();
    let pct = budget.percent();
    let paused = now <= budget.dismissed_until_unix;

    let acknowledged =
        state.budget_acknowledged == Some((info.today_start_unix_ms(), budget.limit_usd.to_bits()));
    if pct < 100 || paused || acknowledged {
        return handle_clear_daily_budget_warning(state);
    }

    decision_queue::raise(
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
    decision_queue::withdraw(state, DecisionKind::Budget);
    HandlerOutcome::Quiet
}

/// `Continue` on the budget decision: the user chose to go on past this limit for
/// the rest of the day, so the next recompute does not raise it again while the
/// day and the limit both hold (A18, closes DF-21-1-BUDGET-CONTINUE-RESOLVES-ONLY).
pub fn acknowledge_daily_budget(state: &mut TuiState, info: &dyn ProviderInfoPort) {
    if let Some(budget) = &state.daily_budget {
        state.budget_acknowledged = Some((info.today_start_unix_ms(), budget.limit_usd.to_bits()));
    }
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
    use crate::adapters::tui::decision_queue::{DecisionKind, PaneKey, PaneOutcome, handle_key};
    use crate::adapters::tui::notification_log::LogFacts;
    use crate::adapters::tui::notifications::NotificationStore;
    use crate::adapters::tui::presentation::{self, Surface, is_open};
    use crate::domain::clock::MockClock;
    use crate::domain::models::NotificationsConfig;
    use crate::domain::models::provider::{ModelDescriptor, ProviderDescriptor};
    use crate::domain::ports::StreamingProvider;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// `(now_unix, today_start_unix_ms)`.
    struct FixedNow(i64, i64);
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
            self.1
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
        let info = FixedNow(1_000, 0);
        handle_upsert_daily_budget_warning(&mut state, &budget(11.0, 0), &info);
        let item = state.decisions.items().next().expect("the budget decision");
        assert_eq!(item.kind, DecisionKind::Budget);
        assert!(item.detail.contains("$10.00"), "{}", item.detail);
        assert!(
            is_open(&state, Surface::Decision),
            "0→1 on an idle composer takes the frame"
        );
        assert!(presentation::consistent(&state));
        assert_eq!(state.modal_stack.top().unwrap().surface, Surface::Decision);

        // The next turn's recompute updates the text in place: still one item.
        handle_upsert_daily_budget_warning(&mut state, &budget(12.0, 0), &info);
        assert_eq!(state.decisions.len(), 1);

        // Paused until after now → the open frame is not yanked: the item turns
        // stale (A7) and waits for the user to close it.
        handle_upsert_daily_budget_warning(&mut state, &budget(12.0, 2_000), &info);
        assert_eq!(state.decisions.len(), 1, "paused keeps the stale copy");
        assert!(state.decisions.items().next().unwrap().is_stale(), "paused");
        assert!(is_open(&state, Surface::Decision), "the frame stays open");
        assert!(presentation::consistent(&state));
        assert_eq!(
            handle_key(&mut state, PaneKey::Esc, &LogFacts::default()),
            PaneOutcome::Consumed
        );
        assert!(state.decisions.is_empty(), "Esc removes the stale item");
        assert!(
            state.modal_stack.is_empty(),
            "Esc closes the stale decision frame"
        );
        assert!(presentation::consistent(&state));

        // Under the limit → stale again, closed the same way.
        handle_upsert_daily_budget_warning(&mut state, &budget(11.0, 0), &info);
        assert!(is_open(&state, Surface::Decision));
        handle_upsert_daily_budget_warning(&mut state, &budget(1.0, 0), &info);
        assert!(
            state.decisions.items().next().unwrap().is_stale(),
            "under the limit"
        );
        assert!(is_open(&state, Surface::Decision));
        handle_key(&mut state, PaneKey::Esc, &LogFacts::default());
        assert!(state.decisions.is_empty());
        assert!(
            state.modal_stack.is_empty(),
            "Esc closes the under-limit stale frame"
        );
        assert!(presentation::consistent(&state));
    }

    #[test]
    fn story21_1_a_budget_arrival_never_steals_a_typing_composer_and_a_deferral_is_not_resurrected()
    {
        let (mut state, clock) = clocked();
        let info = FixedNow(1_000, 0);
        state.focus = crate::domain::models::FocusState::Input;
        state.input_buffer = "half a sentence".into();
        handle_upsert_daily_budget_warning(&mut state, &budget(11.0, 0), &info);
        assert!(
            !is_open(&state, Surface::Decision),
            "a non-empty composer is protected"
        );
        assert!(state.modal_stack.is_empty());
        assert!(presentation::consistent(&state));
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
        assert!(!is_open(&state, Surface::Decision));
        assert!(state.modal_stack.is_empty());
        assert!(presentation::consistent(&state));
        assert_eq!(
            state
                .notifications
                .visible()
                .filter(|r| r.message.contains("Ctrl+X q"))
                .count(),
            1
        );
    }

    #[test]
    fn story21_6_continue_holds_for_this_day_and_limit_and_rearms_otherwise() {
        use crate::adapters::tui::decision_queue::{
            DecisionEffect, INERT_AFTER_PAINT, note_painted,
        };
        let (mut state, clock) = clocked();
        let f = LogFacts::default();
        let day_one = FixedNow(1_000, 86_400_000);
        let over = budget(11.0, 0);
        handle_upsert_daily_budget_warning(&mut state, &over, &day_one);
        assert!(state.decisions.holds(DecisionKind::Budget));
        note_painted(&mut state);
        clock.advance(INERT_AFTER_PAINT);
        assert_eq!(
            handle_key(&mut state, PaneKey::Char('y'), &f),
            PaneOutcome::Effect(DecisionEffect::BudgetContinue)
        );
        // The effect arm records the acknowledgement for this day and limit.
        state.daily_budget = Some(over.clone());
        acknowledge_daily_budget(&mut state, &day_one);
        assert_eq!(
            state.budget_acknowledged,
            Some((86_400_000, 10.0_f64.to_bits()))
        );
        // The next recompute, same day and limit, however much more was spent: no re-raise.
        handle_upsert_daily_budget_warning(&mut state, &budget(25.0, 0), &day_one);
        assert!(state.decisions.is_empty(), "same day and limit: suppressed");
        // A new day re-arms it.
        let day_two = FixedNow(1_000, 2 * 86_400_000);
        handle_upsert_daily_budget_warning(&mut state, &budget(11.0, 0), &day_two);
        assert!(
            state.decisions.holds(DecisionKind::Budget),
            "next day: raised again"
        );
        // So does a changed limit on the acknowledged day.
        state.decisions = Default::default();
        presentation::reducer::close(&mut state, Surface::Decision);
        let raised_limit = DailyBudgetState {
            spent_today_usd: 21.0,
            limit_usd: 20.0,
            computed_at_ms: 0,
            dismissed_until_unix: 0,
        };
        handle_upsert_daily_budget_warning(&mut state, &raised_limit, &day_one);
        assert!(
            state.decisions.holds(DecisionKind::Budget),
            "changed limit: raised again"
        );
    }
}
