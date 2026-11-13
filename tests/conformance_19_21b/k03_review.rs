use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::tui::{app::InputAction, decision_queue::DecisionEffect};
use rustain::domain::events::{DomainInputEvent, DomainKey};
use rustain::domain::models::{Plan, PlanStatus, PlanTask, PlanTaskStatus, ResponseMode};
use serde_json::json;
use std::sync::Arc;

use super::observed_host::{HttpCounts, ObservedHost};
use super::proposal_harness::ProposalFixture;

async fn raise(f: &mut ProposalFixture, body: &str) {
    let result = f
        .submit(json!({"recipients":["lena-po"],"type":"architecture_update","body":body}))
        .await;
    assert!(
        result.starts_with("Proposal shown to the operator."),
        "{result}"
    );
    f.paint(100, 32);
}

#[tokio::test]
async fn story21_6_current_review_uses_current_queued_snapshot() {
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut f = ProposalFixture::start(vec![host.peer("lena-po")]).await;
    host.reset_counts();
    let old_body = (0..40)
        .map(|n| format!("OLDER_REVIEW_{n}"))
        .collect::<Vec<_>>()
        .join("\n");
    raise(&mut f, &old_body).await;
    let old = f.proposal(&f.conversation.id).unwrap().clone();
    assert!(matches!(
        f.key(DomainInputEvent::KeyPress('r')).await,
        InputAction::Consumed
    ));
    f.key(DomainInputEvent::SpecialKey(DomainKey::Esc)).await;
    assert!(matches!(
        f.key(DomainInputEvent::KeyPress('n')).await,
        InputAction::Decision(DecisionEffect::TeamProposalDismiss)
    ));
    let body = "CURRENT_REVIEW_FIRST\nCurrent exact café body.\nCURRENT_REVIEW_LAST";
    raise(&mut f, body).await;
    // The second proposal arrives right after the user's keys (typing grace), so it queues;
    // the user opens it explicitly.
    let queued = f
        .state
        .decisions
        .items()
        .find(|item| {
            item.kind == rustain::adapters::tui::decision_queue::DecisionKind::TeamProposal
                && !item.is_stale()
        })
        .map(|item| rustain::adapters::tui::decision_queue::DecisionKey {
            kind: item.kind,
            source: item.source.clone(),
            conversation_id: item.conversation_id.clone(),
        })
        .expect("the second proposal is queued");
    if !rustain::adapters::tui::decision_queue::open(&mut f.state, &queued) {
        panic!("the queued proposal opens explicitly");
    }
    f.paint(100, 32);
    let pending = f.proposal(&f.conversation.id).unwrap().clone();
    assert!(!Arc::ptr_eq(&old, &pending));
    assert!(matches!(
        f.key(DomainInputEvent::KeyPress('r')).await,
        InputAction::Consumed
    ));
    let rows = f.paint(100, 32);
    for line in body.lines() {
        assert!(
            rows.iter().any(|row| row.contains(line)),
            "{line}: {rows:#?}"
        );
    }
    assert!(
        !rows.iter().any(|row| row.contains("OLDER_REVIEW_")),
        "{rows:#?}"
    );
    assert!(Arc::ptr_eq(
        &pending,
        f.proposal(&f.conversation.id).unwrap()
    ));
    assert_eq!(host.counts(), HttpCounts { get: 0, post: 0 });
    assert!(f.state.team_send_blocks.is_empty());
    host.stop().await;
}

#[tokio::test]
async fn k03_review_from_task_drilldown_restores_full_body_without_sending() {
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut f = ProposalFixture::start(vec![host.peer("lena-po")]).await;
    host.reset_counts();
    let body = "READABLE_REVIEW_FIRST\nExact operator-review text.\nREADABLE_REVIEW_LAST";
    f.conversation.plans.insert(
        "detail-plan".into(),
        Plan {
            id: "detail-plan".into(),
            title: "Existing task plan".into(),
            tasks: vec![PlanTask {
                number: 1,
                title: "TASK_DETAIL_ONLY".into(),
                description: "Task details".into(),
                depends_on: vec![],
                status: PlanTaskStatus::Completed,
                started_at_ms: None,
                completed_at_ms: None,
                result: None,
                error: None,
                waiting_on: vec![],
                delegated_to: None,
                sub_tasks: vec![],
            }],
            estimated_effort: None,
            status: PlanStatus::Executing,
            created_at: 1_700_000_000,
            resolved_at: None,
            host_message_id: None,
            origin_restrictions: None,
        },
    );
    f.state.task_panel_state.last_executed_plan_id = Some("detail-plan".into());
    // A task window is open beneath the proposal card (the old drill-down painted under it).
    let task = rustain::adapters::tui::mission_control::tasks::TaskRef {
        conversation_id: f.conversation.id.clone(),
        plan_id: "detail-plan".into(),
        number: 1,
    };
    assert!(rustain::adapters::tui::task_window::open(
        &mut f.state,
        &f.conversation,
        &task
    ));
    assert_eq!(
        f.state.task_window.as_ref().map(|w| w.title()).as_deref(),
        Some("Task 1 \u{b7} TASK_DETAIL_ONLY")
    );
    raise(&mut f, body).await;
    let pending = f.proposal(&f.conversation.id).unwrap().clone();
    assert!(matches!(
        f.key(DomainInputEvent::KeyPress('r')).await,
        InputAction::Consumed
    ));
    let after = f.paint(100, 32);
    for line in body.lines() {
        assert!(
            after.iter().any(|row| row.contains(line)),
            "{line}: {after:#?}"
        );
    }
    assert!(Arc::ptr_eq(
        f.proposal(&f.conversation.id).unwrap(),
        &pending
    ));
    assert_eq!(pending.body, body);
    assert!(
        f.state.task_window.is_some(),
        "the task window stays open beneath the card"
    );
    assert!(f.state.team_send_blocks.is_empty());
    assert_eq!(host.counts(), HttpCounts { get: 0, post: 0 });
    host.stop().await;
}
