//! The plan family as queue items (Story 21.6 6-C): plan approval, the plan card, plan
//! deviation, skip cascade and cancel plan. The payloads moved here from the singleton
//! slots of `TuiState` / `TaskPanelState` (AD-2110).
//!
//! Plan card, deviation, skip cascade, cancel plan and the plan approval belong to one
//! conversation (their effects act on that tab's plan, so an item for another tab waits,
//! counted). The deviation, skip cascade and cancel plan bodies are snapshotted at
//! raise time from what the producer held.

use std::path::PathBuf;

use super::{
    Arrival, DecisionDraft, DecisionKey, DecisionKind, DecisionSubject, NO_LONGER_PENDING, Raised,
    raise, withdraw_key,
};
use crate::adapters::tui::state::TuiState;
use crate::domain::models::Plan;
use crate::domain::models::plan::{PlanDeviationKind, PlanTaskStatus};

/// A plan waiting for approval (the plan-mode `y` / `a` / `n` / `e`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanApprovalPayload {
    pub conversation_id: String,
    pub plan_path: PathBuf,
    pub contents: String,
    pub summary: String,
}

/// The inline plan card waiting for `y` / `e` / `n` (Story 6-1a). At most one per plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanCardPayload {
    pub conversation_id: String,
    pub plan_id: String,
    pub plan_snapshot: Plan,
}

/// A running plan deviated and needs reapproval (Story 6.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanDeviationPayload {
    pub plan_id: String,
    pub kind: PlanDeviationKind,
    /// The task count before the deviation.
    pub original: u32,
    /// The tasks the deviation changed.
    pub changed: Vec<u32>,
    pub summary: String,
}

/// Story 6.4: pending skip cascade confirmation state (AC2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkipCascadePending {
    pub plan_id: String,
    pub source_task: u32,
    pub source_prior_status: PlanTaskStatus,
    pub source_prior_error: Option<String>,
    pub downstream: Vec<u32>,
}

/// Skipping a task with dependants: skip them too, continue anyway, or keep the task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkipCascadePayload(pub SkipCascadePending);

/// Cancelling a plan: the counts are those of the conversation when it was raised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelPlanPayload {
    pub plan_id: String,
    pub title: String,
    pub pending: u32,
    pub completed: u32,
}

/// The kinds of this family keyed to the plan they concern.
const PLAN_KINDS: [DecisionKind; 5] = [
    DecisionKind::PlanCard,
    DecisionKind::PlanDeviation,
    DecisionKind::SkipCascade,
    DecisionKind::CancelPlan,
    DecisionKind::Delegation,
];

fn plan_id_of(subject: &DecisionSubject) -> Option<&str> {
    match subject {
        DecisionSubject::PlanCard(payload) => Some(&payload.plan_id),
        DecisionSubject::PlanDeviation(payload) => Some(&payload.plan_id),
        DecisionSubject::SkipCascade(payload) => Some(&payload.0.plan_id),
        DecisionSubject::CancelPlan(payload) => Some(&payload.plan_id),
        DecisionSubject::Delegation(card) => Some(&card.plan_id),
        _ => None,
    }
}

/// Withdraw the items of `kinds` that concern `plan_id`: the plan they were raised for
/// is gone or settled, so they can only be stale (an open frame turns stale and waits for
/// Close, a queued item leaves).
fn withdraw_kinds(state: &mut TuiState, plan_id: &str, kinds: &[DecisionKind]) {
    let keys: Vec<DecisionKey> = state
        .decisions
        .items()
        .filter(|item| kinds.contains(&item.kind) && plan_id_of(&item.subject) == Some(plan_id))
        .map(super::DecisionItem::key)
        .collect();
    for key in keys {
        withdraw_key(state, &key, NO_LONGER_PENDING);
    }
}

/// Raise a plan awaiting approval (Automatic arrival). Its path is its source identity and
/// its conversation is its key: only the tab that proposed it (a writable one) can open
/// or answer it. A producer with no conversation context (empty id) is decidable anywhere.
pub fn raise_plan_approval(state: &mut TuiState, payload: PlanApprovalPayload) -> Raised {
    let stem = payload
        .plan_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("plan")
        .to_owned();
    let mut draft = DecisionDraft::new(DecisionKind::PlanApproval, "")
        .source(payload.plan_path.display().to_string())
        .summary(stem);
    if !payload.conversation_id.is_empty() {
        draft = draft.conversation(payload.conversation_id.clone());
    }
    let draft = draft.subject(DecisionSubject::PlanApproval(Box::new(payload)));
    raise(state, draft, Arrival::Automatic)
}

/// True when an approval raised in `origin` may be applied while `active` is in front: only
/// in its own tab, never in a read-only one (its answer submits a turn there).
#[must_use]
pub fn plan_approval_applies(origin: &str, active: &str, active_read_only: bool) -> bool {
    !active_read_only && (origin.is_empty() || origin == active)
}

/// The effect of an answered approval, before it acts: the payload when it applies in the
/// active tab, otherwise it goes back to the queue unanswered (its item is never lost).
pub fn settle_plan_approval(
    state: &mut TuiState,
    payload: Box<PlanApprovalPayload>,
    active: &str,
    active_read_only: bool,
) -> Option<Box<PlanApprovalPayload>> {
    if plan_approval_applies(&payload.conversation_id, active, active_read_only) {
        return Some(payload);
    }
    raise_plan_approval(state, *payload);
    None
}

/// The plan approval the `e` key edits: the focused one, else the oldest waiting. Its key
/// and the plan file the host opens in the editor.
#[must_use]
pub fn plan_approval_target(state: &TuiState) -> Option<(DecisionKey, PathBuf)> {
    let of = |item: &super::DecisionItem| match &item.subject {
        DecisionSubject::PlanApproval(payload) => Some((item.key(), payload.plan_path.clone())),
        _ => None,
    };
    super::focused_key(state)
        .and_then(|key| state.decisions.item(&key).and_then(of))
        .or_else(|| state.decisions.items().find_map(of))
}

/// The host re-read the edited plan file: the body shows the new contents.
pub fn set_plan_contents(state: &mut TuiState, key: &DecisionKey, contents: String) {
    let set = match state.decisions.item_mut(key).map(|item| &mut item.subject) {
        Some(DecisionSubject::PlanApproval(payload)) => {
            payload.contents = contents;
            true
        }
        _ => false,
    };
    if set {
        *state.decisions.body_layout.get_mut() = None;
        state.needs_redraw = true;
    }
}

/// Raise the plan card's decision (Automatic arrival). The card itself stays a Stage row
/// without a rail; this item is how it is answered (FR-21-6-f).
pub fn raise_plan_card(state: &mut TuiState, payload: PlanCardPayload) -> Raised {
    let draft = DecisionDraft::new(DecisionKind::PlanCard, "")
        .conversation(payload.conversation_id.clone())
        .source(payload.plan_id.clone())
        .summary(payload.plan_snapshot.title.clone())
        .subject(DecisionSubject::PlanCard(Box::new(payload)));
    raise(state, draft, Arrival::Automatic)
}

/// The plan left the pending state or was replaced: its decision is no longer pending.
pub fn withdraw_plan_card(state: &mut TuiState, plan_id: &str) {
    withdraw_kinds(state, plan_id, &[DecisionKind::PlanCard]);
}

/// True while a live plan-card decision for `plan_id` waits (the Stage row's marker).
#[must_use]
pub fn plan_card_pending(state: &TuiState, plan_id: &str) -> bool {
    state.decisions.items().any(|item| {
        item.kind == DecisionKind::PlanCard && !item.is_stale() && item.source == plan_id
    })
}

/// The plan snapshot the waiting card was raised with.
#[must_use]
pub fn plan_card_snapshot(state: &TuiState, plan_id: &str) -> Option<Plan> {
    state
        .decisions
        .items()
        .find_map(|item| match &item.subject {
            DecisionSubject::PlanCard(payload) if payload.plan_id == plan_id => {
                Some(payload.plan_snapshot.clone())
            }
            _ => None,
        })
}

/// Raise a plan deviation's reapproval (Automatic arrival).
pub fn raise_plan_deviation(
    state: &mut TuiState,
    conversation_id: &str,
    payload: PlanDeviationPayload,
) -> Raised {
    let draft = DecisionDraft::new(DecisionKind::PlanDeviation, "")
        .conversation(conversation_id)
        .source(payload.plan_id.clone())
        .summary(payload.summary.clone())
        .subject(DecisionSubject::PlanDeviation(Box::new(payload)));
    raise(state, draft, Arrival::Automatic)
}

/// Raise the skip-cascade question (the user skipped a task: Requested arrival).
pub fn raise_skip_cascade(
    state: &mut TuiState,
    conversation_id: &str,
    pending: SkipCascadePending,
) -> Raised {
    let draft = DecisionDraft::new(DecisionKind::SkipCascade, "")
        .conversation(conversation_id)
        .source(format!("{}:{}", pending.plan_id, pending.source_task))
        .summary(format!("task {}", pending.source_task))
        .subject(DecisionSubject::SkipCascade(Box::new(SkipCascadePayload(
            pending,
        ))));
    raise(state, draft, Arrival::Requested)
}

/// Raise the cancel-plan confirmation (the user asked: Requested arrival).
pub fn raise_cancel_plan(
    state: &mut TuiState,
    conversation_id: &str,
    payload: CancelPlanPayload,
) -> Raised {
    let draft = DecisionDraft::new(DecisionKind::CancelPlan, "")
        .conversation(conversation_id)
        .source(payload.plan_id.clone())
        .summary(payload.title.clone())
        .subject(DecisionSubject::CancelPlan(Box::new(payload)));
    raise(state, draft, Arrival::Requested)
}

/// The plan `plan_id` finished or was cancelled: every decision concerning it is moot.
pub fn withdraw_plan(state: &mut TuiState, plan_id: &str) {
    withdraw_kinds(state, plan_id, &PLAN_KINDS);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approval(conversation: &str, path: &str) -> PlanApprovalPayload {
        PlanApprovalPayload {
            conversation_id: conversation.into(),
            plan_path: PathBuf::from(path),
            contents: "# plan".into(),
            summary: "s".into(),
        }
    }

    #[test]
    fn story21_6_an_answered_approval_applies_only_in_its_writable_origin_tab() {
        // The answered item already left the queue: anywhere else it goes back, unanswered.
        for (active, read_only) in [("c2", true), ("c2", false), ("c1", true)] {
            let mut state = TuiState::new(80, 24);
            let settled = settle_plan_approval(
                &mut state,
                Box::new(approval("c1", "a.md")),
                active,
                read_only,
            );
            assert!(settled.is_none(), "{active} read_only={read_only}");
            assert_eq!(state.decisions.count_live(DecisionKind::PlanApproval), 1);
            let item = state.decisions.items().next().unwrap();
            assert_eq!(item.conversation_id.as_deref(), Some("c1"));
        }
        // Controls: the writable origin applies it; a producer with no conversation
        // context (empty id) applies in any writable tab and stays decidable anywhere.
        let mut state = TuiState::new(80, 24);
        assert!(
            settle_plan_approval(&mut state, Box::new(approval("c1", "a.md")), "c1", false)
                .is_some()
        );
        assert!(
            settle_plan_approval(&mut state, Box::new(approval("", "a.md")), "c9", false).is_some()
        );
        assert!(state.decisions.is_empty(), "nothing is re-queued");
        raise_plan_approval(&mut state, approval("", "b.md"));
        assert_eq!(
            state.decisions.items().next().unwrap().conversation_id,
            None
        );
    }

    #[test]
    fn story21_6_equal_approvals_of_two_tabs_stay_two_items() {
        let mut state = TuiState::new(80, 24);
        raise_plan_approval(&mut state, approval("c1", "plan.md"));
        raise_plan_approval(&mut state, approval("c2", "plan.md"));
        assert_eq!(state.decisions.count(DecisionKind::PlanApproval), 2);
    }
}
