use std::sync::Arc;

use ratatui::{Terminal, backend::TestBackend};
use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::tui::{
    app::InputAction,
    widgets::{chat_pane, task_detail},
};
use rustain::domain::events::DomainInputEvent;
use rustain::domain::models::{
    FocusState, Plan, PlanStatus, PlanTask, PlanTaskStatus, ResponseMode, StreamingState, ViewState,
};
use serde_json::json;

use super::observed_host::{HttpCounts, ObservedHost};
use super::proposal_harness::ProposalFixture;

async fn raise(f: &mut ProposalFixture, body: &str) {
    let result = f
        .submit(json!({
            "recipients": ["lena-po"], "type": "architecture_update", "body": body
        }))
        .await;
    assert!(
        result.starts_with("Proposal shown to the operator."),
        "{result}"
    );
    f.paint(100, 32);
}

fn paint_chat_surface(f: &mut ProposalFixture, width: u16, height: u16) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| {
            let area = frame.area();
            if let Some(number) = f.state.task_panel_state.drill_down_task {
                let plan = &f.conversation.plans[f
                    .state
                    .task_panel_state
                    .last_executed_plan_id
                    .as_ref()
                    .unwrap()];
                task_detail::render(
                    area,
                    frame.buffer_mut(),
                    plan,
                    &plan.tasks[(number - 1) as usize],
                    &f.state.theme,
                    height,
                    f.state.task_panel_state.expanded_detail,
                    &mut f.state.task_panel_state.detail_scroll_offset,
                );
                return;
            }
            let render = f
                .state
                .tab_render_states
                .entry(f.state.active_tab_id)
                .or_default();
            chat_pane::render_with_search(
                frame,
                area,
                &f.conversation,
                None,
                &StreamingState::default(),
                &ViewState::default(),
                &rustain::domain::clock::SystemClock::default(),
                0,
                true,
                &f.state.theme,
                render,
                &Default::default(),
                &f.state.feedback_blocks,
                None,
                None,
                &[],
                &[],
                None,
                None,
                None,
                None,
                f.state.active_feedback_id.as_deref(),
            );
        })
        .unwrap();
    terminal
        .backend()
        .buffer()
        .content
        .chunks(width as usize)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect())
        .collect()
}

fn rekey_review(
    f: &mut ProposalFixture,
    replacement: &str,
) -> Arc<rustain::domain::models::TeamProposal> {
    let original = f
        .state
        .active_feedback_id
        .take()
        .expect("real Review feedback");
    let mut feedback = f.state.feedback_blocks.remove(&original).unwrap();
    feedback.id = replacement.to_owned();
    f.state
        .feedback_blocks
        .insert(replacement.to_owned(), feedback);
    let reviews = &mut f
        .state
        .tab_render_states
        .get_mut(&f.state.active_tab_id)
        .unwrap()
        .team_proposal_reviews;
    let proposal = reviews.remove(&original).unwrap();
    reviews.insert(replacement.to_owned(), proposal.clone());
    f.state.active_feedback_id = Some(replacement.to_owned());
    proposal
}

#[tokio::test]
async fn k03_current_review_is_visible_after_longer_reverse_lexical_retained_review() {
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut f = ProposalFixture::start(vec![host.peer("lena-po")]).await;
    host.reset_counts();
    let old_body = (0..40)
        .map(|n| format!("OLDER_REVIEW_{n}"))
        .collect::<Vec<_>>()
        .join("\n");
    raise(&mut f, &old_body).await;
    assert!(matches!(
        f.key(DomainInputEvent::KeyPress('r')).await,
        InputAction::TeamProposal { decision: None, .. }
    ));
    let old_snapshot = rekey_review(&mut f, "team-proposal-review-zzzz-old");
    f.key(DomainInputEvent::KeyPress('n')).await;

    let body = "CURRENT_REVIEW_FIRST\nCurrent exact café body.\nCURRENT_REVIEW_LAST";
    raise(&mut f, body).await;
    let pending = f.state.pending_team_proposals[&f.conversation.id].clone();
    f.key(DomainInputEvent::KeyPress('r')).await;
    // IDs are random in production. Rekey only the renderer's already-raised
    // feedback/snapshot pairs to reproduce the adverse lexical ordering exactly.
    let current_snapshot = rekey_review(&mut f, "team-proposal-review-aaaa-current");
    assert!(Arc::ptr_eq(&pending, &current_snapshot));
    assert!(!Arc::ptr_eq(&old_snapshot, &current_snapshot));
    let rows = paint_chat_surface(&mut f, 80, 12);
    for line in body.lines() {
        assert!(
            rows.iter().any(|row| row.contains(line)),
            "{line}: {rows:#?}"
        );
    }
    let current_start = rows
        .iter()
        .position(|row| row.contains("CURRENT_REVIEW_FIRST"))
        .unwrap();
    assert!(
        rows.iter()
            .enumerate()
            .all(|(index, row)| !row.contains("OLDER_REVIEW_") || index < current_start),
        "{rows:#?}"
    );
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
    raise(&mut f, body).await;
    let pending = f.state.pending_team_proposals[&f.conversation.id].clone();
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
        },
    );
    f.state.task_panel_state.last_executed_plan_id = Some("detail-plan".into());
    f.state.task_panel_state.drill_down_task = Some(1);
    f.state.task_panel_state.expanded_detail = true;
    f.state.focus = FocusState::Chat;
    let before = paint_chat_surface(&mut f, 80, 16);
    assert!(
        before.iter().any(|row| row.contains("TASK_DETAIL_ONLY")),
        "{before:#?}"
    );

    assert!(matches!(
        f.key(DomainInputEvent::KeyPress('r')).await,
        InputAction::TeamProposal { decision: None, .. }
    ));
    let after = paint_chat_surface(&mut f, 80, 16);
    for line in body.lines() {
        assert!(
            after.iter().any(|row| row.contains(line)),
            "{line}: {after:#?}"
        );
    }
    assert!(
        !after.iter().any(|row| row.contains("TASK_DETAIL_ONLY")),
        "{after:#?}"
    );
    let review = &f.state.tab_render_states[&f.state.active_tab_id].team_proposal_reviews
        [f.state.active_feedback_id.as_ref().unwrap()];
    assert!(Arc::ptr_eq(review, &pending));
    assert_eq!(review.body, body);
    assert!(Arc::ptr_eq(
        &f.state.pending_team_proposals[&f.conversation.id],
        &pending
    ));
    assert!(f.state.team_send_blocks.is_empty());
    assert_eq!(host.counts(), HttpCounts { get: 0, post: 0 });
    host.stop().await;
}
