//! Plan-family decisions raised into the decision queue (Story 21.6 slice 6-C).
//!
//! The deviation, cancel-plan and skip-cascade cards used to read their counts from
//! the conversation at every paint. A queue item holds a value, so the producer takes
//! the snapshot here, once, from the conversation it holds (a recorded deviation from
//! the paint-time read). Every body string is rendered through the frame's sanitizer;
//! the payload keeps the original text.

use crate::adapters::tui::decision_queue::{
    self, CancelPlanPayload, PendingDelegationCard, PlanDeviationPayload, SkipCascadePending,
};
use crate::adapters::tui::state::TuiState;
use crate::domain::models::Conversation;
use crate::domain::models::plan::{PlanDeviationKind, PlanTaskStatus};
use crate::domain::services::plan_runtime::is_terminal_pub;

/// A delegation suggestion needs the user's choice (Automatic arrival).
pub(crate) fn arrive_delegation_card(state: &mut TuiState, card: PendingDelegationCard) {
    decision_queue::raise_delegation(state, card);
}

/// A plan deviation needs the user's choice: the original step count, the steps the
/// runtime auto-skipped and the summary line are snapshotted from `conversation`.
pub(crate) fn arrive_plan_deviation(
    state: &mut TuiState,
    conversation: &Conversation,
    plan_id: String,
    kind: PlanDeviationKind,
) {
    let (original, changed, summary) = match conversation.plans.get(&plan_id) {
        Some(plan) => {
            let changed: Vec<u32> = plan
                .tasks
                .iter()
                .filter(|task| task.status == PlanTaskStatus::Skipped)
                .map(|task| task.number)
                .collect();
            let summary = format!(
                "{} task(s) auto-skipped due to upstream failure.",
                changed.len()
            );
            (plan.tasks.len() as u32, changed, summary)
        }
        None => (0, Vec::new(), String::new()),
    };
    decision_queue::raise_plan_deviation(
        state,
        &conversation.id,
        PlanDeviationPayload {
            plan_id,
            kind,
            original,
            changed,
            summary,
        },
    );
}

/// The user asked to cancel a plan: the title and the pending/completed counts are
/// snapshotted from `conversation` (Requested arrival).
pub(crate) fn arrive_cancel_plan_confirmation(
    state: &mut TuiState,
    conversation: &Conversation,
    plan_id: String,
) {
    let (title, pending, completed) = conversation
        .plans
        .get(&plan_id)
        .map(|plan| {
            let pending = plan
                .tasks
                .iter()
                .filter(|task| !is_terminal_pub(task.status))
                .count() as u32;
            let completed = plan
                .tasks
                .iter()
                .filter(|task| task.status == PlanTaskStatus::Completed)
                .count() as u32;
            (plan.title.clone(), pending, completed)
        })
        .unwrap_or_default();
    decision_queue::raise_cancel_plan(
        state,
        &conversation.id,
        CancelPlanPayload {
            plan_id,
            title,
            pending,
            completed,
        },
    );
}

/// The user asked to skip a task that has dependants (Requested arrival).
pub(crate) fn arrive_skip_cascade(
    state: &mut TuiState,
    conversation_id: &str,
    pending: SkipCascadePending,
) {
    decision_queue::raise_skip_cascade(state, conversation_id, pending);
}

/// The plan finished or was cancelled (whichever conversation's tab is in front): the
/// confirmations queued for it can only be stale. An open frame turns stale and waits
/// for Close; a queued item leaves.
pub(crate) fn plan_settled(state: &mut TuiState, plan_id: &str) {
    decision_queue::withdraw_plan(state, plan_id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tui::decision_queue::DecisionKind;
    use crate::domain::models::FocusState;
    use crate::domain::models::plan::{Plan, PlanStatus, PlanTask};

    fn conversation_with_plan() -> Conversation {
        let mut conversation = Conversation::default();
        conversation.id = "c1".into();
        let task = |number: u32| -> PlanTask {
            serde_json::from_value(serde_json::json!({"number": number, "title": "t"})).unwrap()
        };
        let mut tasks = vec![task(1), task(2), task(3)];
        tasks[0].status = PlanTaskStatus::Completed;
        tasks[1].status = PlanTaskStatus::Skipped;
        conversation.plans.insert(
            "p1".into(),
            Plan {
                id: "p1".into(),
                title: "Ship it".into(),
                tasks,
                estimated_effort: None,
                status: PlanStatus::Executing,
                created_at: 0,
                resolved_at: None,
                host_message_id: None,
            },
        );
        conversation
    }

    #[test]
    fn story21_6_plan_decisions_snapshot_their_counts_and_leave_the_draft_alone() {
        let conversation = conversation_with_plan();
        let mut state = TuiState::new(80, 24);
        state.focus = FocusState::Input;
        state.input_buffer = "draft".into();
        state.cursor_position = 5;
        arrive_plan_deviation(
            &mut state,
            &conversation,
            "p1".into(),
            PlanDeviationKind::AutoSkipBlockedTasks { source_task: 1 },
        );
        arrive_cancel_plan_confirmation(&mut state, &conversation, "p1".into());
        assert_eq!(state.decisions.count(DecisionKind::PlanDeviation), 1);
        assert_eq!(state.decisions.count(DecisionKind::CancelPlan), 1);
        assert_eq!(
            state.input_buffer, "draft",
            "an arrival never edits the draft"
        );
        assert_eq!(state.cursor_position, 5);
    }

    #[test]
    fn story21_6_plan_settlement_retires_only_that_plans_confirmations() {
        use crate::domain::services::delegation_decider::{DelegationReason, DelegationSuggestion};
        let conversation = conversation_with_plan();
        let mut state = TuiState::new(80, 24);
        let kinds = [
            DecisionKind::PlanDeviation,
            DecisionKind::SkipCascade,
            DecisionKind::CancelPlan,
            DecisionKind::Delegation,
        ];
        arrive_plan_deviation(
            &mut state,
            &conversation,
            "p1".into(),
            PlanDeviationKind::AutoSkipBlockedTasks { source_task: 1 },
        );
        arrive_skip_cascade(
            &mut state,
            "c1",
            SkipCascadePending {
                plan_id: "p1".into(),
                source_task: 2,
                source_prior_status: PlanTaskStatus::Pending,
                source_prior_error: None,
                downstream: vec![3],
            },
        );
        arrive_cancel_plan_confirmation(&mut state, &conversation, "p1".into());
        arrive_delegation_card(
            &mut state,
            PendingDelegationCard {
                conversation_id: "c1".into(),
                plan_id: "p1".into(),
                task_number: 3,
                suggestion: DelegationSuggestion {
                    task_number: 3,
                    agent_name: "reviewer".into(),
                    reason: DelegationReason::DescriptionMatch { overlap_score: 2 },
                    auto_proceed: false,
                },
            },
        );
        // Another plan's deviation survives the settlement of p1.
        arrive_plan_deviation(
            &mut state,
            &conversation,
            "p2".into(),
            PlanDeviationKind::AutoSkipBlockedTasks { source_task: 1 },
        );
        assert_eq!(state.decisions.len(), 5);
        plan_settled(&mut state, "p1");
        // The open frame (the first arrival) turns stale and waits for Close; the queued
        // ones leave; p2's deviation stays live.
        assert_eq!(
            kinds
                .iter()
                .map(|kind| state.decisions.count_live(*kind))
                .sum::<usize>(),
            1,
            "only p2's deviation is live"
        );
        assert!(
            state
                .decisions
                .items()
                .all(|item| item.is_stale() || item.source == "p2"),
            "every p1 item left the queue or is Close-only stale"
        );
        assert!(
            state.decisions.len() <= 2,
            "at most p2's and one open frame"
        );
    }
}
