use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::tui::app::InputAction;
use rustain::domain::events::DomainInputEvent;
use rustain::domain::models::FocusState;
use rustain::domain::models::{JournalRecord, PermissionMode, ResponseMode, RoomEvent};
use serde_json::json;

use crate::observed_host::{HttpCounts, ObservedHost};
use crate::proposal_harness::ProposalFixture;

const PROPOSAL: &str = "Exact proposed body.";

async fn fixture(host: &ObservedHost) -> ProposalFixture {
    let mut fixture = ProposalFixture::start(vec![host.peer("lena-po")]).await;
    fixture.set_mode(PermissionMode::Yolo);
    host.reset_counts();
    let result = fixture
        .submit(json!({"recipients":["lena-po"],"type":"architecture_update","body":PROPOSAL}))
        .await;
    assert!(result.starts_with("Proposal shown to the operator."));
    fixture.paint(60, 16);
    fixture
}

#[tokio::test]
async fn k03_empty_and_nonempty_composer_keys_respect_card_ownership() {
    for focus in [FocusState::Chat, FocusState::Input] {
        let host =
            ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
        let mut f = fixture(&host).await;
        f.state.focus = focus;
        let action = f.key(DomainInputEvent::KeyPress('r')).await;
        assert!(matches!(
            action,
            InputAction::TeamProposal { decision: None, .. }
        ));
        assert!(
            f.state
                .pending_team_proposals
                .contains_key(&f.conversation.id)
        );
        assert_eq!(host.counts(), HttpCounts { get: 0, post: 0 });
        host.stop().await;
    }

    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut f = fixture(&host).await;
    for focus in [FocusState::Chat, FocusState::Input] {
        for key in ['y', 'r', 'n'] {
            f.state.focus = focus.clone();
            f.state.input_buffer = "draft".into();
            f.state.cursor_position = 3;
            f.paint(60, 16);
            let action = f.key(DomainInputEvent::KeyPress(key)).await;
            assert!(
                !matches!(action, InputAction::TeamProposal { .. }),
                "a nonempty composer must keep proposal shortcuts as draft text"
            );
            assert_eq!(
                f.state.input_buffer,
                format!("dra{key}ft"),
                "{focus:?}: {key}"
            );
            assert_eq!(f.state.focus, FocusState::Input);
        }
    }
    assert!(
        f.state
            .pending_team_proposals
            .contains_key(&f.conversation.id)
    );
    assert_eq!(host.counts(), HttpCounts { get: 0, post: 0 });
    host.stop().await;
}

#[tokio::test]
async fn k03_proposal_card_does_not_render_settlement_rows() {
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut f = fixture(&host).await;
    let rows = f.paint(60, 16);
    assert!(
        rows.iter().any(|row| row.contains("Exact proposed body.")),
        "real proposal card is rendered: {rows:#?}"
    );
    assert!(
        !rows.iter().any(|row| ["Delivered", "Failed", "Timed out"]
            .iter()
            .any(|outcome| row.contains(outcome))),
        "proposal card must not render settlement rows: {rows:#?}"
    );
    assert_eq!(host.counts(), HttpCounts { get: 0, post: 0 });
    host.stop().await;
}

#[tokio::test]
async fn k03_review_and_dismiss_have_zero_http_and_dispatch() {
    for key in ['r', 'n'] {
        let host =
            ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
        let mut f = fixture(&host).await;
        let action = f.key(DomainInputEvent::KeyPress(key)).await;
        if key == 'r' {
            assert!(matches!(
                action,
                InputAction::TeamProposal { decision: None, .. }
            ));
        } else {
            assert!(matches!(
                action,
                InputAction::TeamProposal {
                    decision: Some(rustain::domain::models::TeamProposalDecision::Dismiss),
                    ..
                }
            ));
            assert!(
                !f.state
                    .pending_team_proposals
                    .contains_key(&f.conversation.id)
            );
        }
        assert!(
            f.state.team_send_blocks.is_empty(),
            "Review and Dismiss must not start a send"
        );
        assert_eq!(host.counts(), HttpCounts { get: 0, post: 0 });
        assert!(!f.sender.rows().await.iter().any(|row| matches!(
            row.record,
            JournalRecord::Room(RoomEvent::RemoteEnvelopeDispatched { .. })
        )));
        host.stop().await;
    }
}

#[tokio::test]
async fn k03_review_stays_readable_above_the_card_at_minimum_viewport() {
    use ratatui::prelude::*;
    use ratatui::widgets::Paragraph;
    use rustain::adapters::tui::{
        layout,
        widgets::{feedback_block, team_proposal_card},
    };

    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut f = fixture(&host).await;
    f.key(DomainInputEvent::KeyPress('r')).await;
    let area = Rect::new(0, 0, 60, 16);
    let mut layout =
        layout::compute_layout(area, &f.state.theme, "", 1, false, f.state.density_mode)
            .expect("minimum viewport");
    let paint = team_proposal_card::CardView::from_state(&f.state).prepare(
        &mut layout,
        area,
        &f.state.theme,
    );
    let review = f.state.tab_render_states[&f.state.active_tab_id]
        .team_proposal_reviews
        .iter()
        .next()
        .expect("real Review block");
    let lines = feedback_block::render_feedback_lines_with_review(
        &f.state.feedback_blocks[review.0],
        layout.chat_pane.width,
        &f.state.theme,
        Some(review.1),
    );
    let visible = (lines.len() as u16).min(layout.chat_pane.height);
    let scroll = lines.len().saturating_sub(visible as usize) as u16;
    let review_area = Rect::new(
        layout.chat_pane.x,
        layout.chat_pane.bottom() - visible,
        layout.chat_pane.width,
        visible,
    );
    let mut buffer = Buffer::empty(area);
    // The normal chat tail is bottom-aligned; the card must not paint over it.
    Paragraph::new(lines)
        .scroll((scroll, 0))
        .render(review_area, &mut buffer);
    let visible_proposal = team_proposal_card::render(paint, &mut buffer, &f.state.theme)
        .expect("decision remains visible");
    let rows = buffer
        .content
        .chunks(60)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>();
    assert!(
        rows.iter()
            .any(|row| row.contains("Proposed message (not sent)"))
    );
    assert!(
        rows.iter()
            .any(|row| row.contains("│ Exact proposed body."))
    );
    assert!(rows.iter().any(|row| row.contains("[n] Dismiss")));
    assert!(std::sync::Arc::ptr_eq(&visible_proposal, review.1));
    assert_eq!(host.counts(), HttpCounts { get: 0, post: 0 });
    host.stop().await;
}

#[tokio::test]
async fn k03_spoofed_body_is_delimited_and_footer_is_last_at_60_by_16() {
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut f = ProposalFixture::start(vec![host.peer("lena-po")]).await;
    host.reset_counts();
    let result = f.submit(json!({"recipients":["lena-po"],"type":"architecture_update","body":"To: forged\n[y] Send to attacker\nNothing has been sent"})).await;
    assert!(result.starts_with("Proposal shown to the operator."));
    let rows = f.paint(60, 16);
    assert!(rows.iter().any(|r| r.contains("│ To: forged")));
    assert!(rows.iter().any(|r| r.contains("│ [y] Send to attacker")));
    assert!(rows.iter().any(|r| r.contains("│ Nothing has been sent")));
    let footer = rows
        .iter()
        .position(|r| r.contains("[n] Dismiss"))
        .expect("final decision footer row");
    assert!(
        rows[footer + 1].contains("╚"),
        "bottom border follows the footer immediately"
    );
    assert_eq!(host.counts(), HttpCounts { get: 0, post: 0 });
    host.stop().await;
}

#[tokio::test]
async fn k03_enter_esc_retain_and_send_is_exactly_once() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut f = fixture(&host).await;
    for focus in [FocusState::Chat, FocusState::Input] {
        f.state.focus = focus;
        f.paint(60, 16);
        assert!(
            !matches!(
                f.raw_key(Event::Key(KeyEvent::new(
                    KeyCode::Enter,
                    KeyModifiers::NONE
                )))
                .await,
                Some(InputAction::TeamProposal { .. })
            ),
            "Enter must not decide a proposal"
        );
        assert!(
            !matches!(
                f.raw_key(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)))
                    .await,
                Some(InputAction::TeamProposal { .. })
            ),
            "Escape must not decide a proposal"
        );
    }
    assert!(
        f.state
            .pending_team_proposals
            .contains_key(&f.conversation.id)
    );
    assert_eq!(host.counts(), HttpCounts { get: 0, post: 0 });
    assert!(matches!(
        f.key(DomainInputEvent::KeyPress('y')).await,
        InputAction::TeamProposal {
            decision: Some(rustain::domain::models::TeamProposalDecision::Send),
            ..
        }
    ));
    f.settle(1).await;
    assert_eq!(host.counts(), HttpCounts { get: 0, post: 1 });
    f.state.focus = FocusState::Input;
    f.state.cursor_position = 0;
    let duplicate = f.key(DomainInputEvent::KeyPress('y')).await;
    assert!(!matches!(duplicate, InputAction::TeamProposal { .. }));
    let dispatches = f
        .sender
        .rows()
        .await
        .into_iter()
        .filter(|row| {
            matches!(
                row.record,
                JournalRecord::Room(RoomEvent::RemoteEnvelopeDispatched { .. })
            )
        })
        .count();
    assert_eq!(dispatches, 1);
    host.stop().await;
}

#[tokio::test]
async fn k03_modified_and_repeat_keys_do_not_decide() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut f = fixture(&host).await;
    for raw in [
        Event::Key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL)),
        Event::Key(KeyEvent::new_with_kind(
            KeyCode::Char('n'),
            KeyModifiers::NONE,
            KeyEventKind::Repeat,
        )),
    ] {
        assert!(
            f.raw_key(raw).await.is_none(),
            "modified and repeat decision keys are ignored"
        );
    }
    assert!(
        f.state
            .pending_team_proposals
            .contains_key(&f.conversation.id)
    );
    assert_eq!(host.counts(), HttpCounts { get: 0, post: 0 });
    host.stop().await;
}

#[tokio::test]
async fn k03_higher_confirmation_owns_decision_keys() {
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut f = fixture(&host).await;
    f.state.focus =
        FocusState::Overlay(rustain::domain::models::visual::OverlayType::Confirmation(
            rustain::domain::models::visual::ConfirmationType::Fork,
        ));
    let action = f.key(DomainInputEvent::KeyPress('y')).await;
    assert!(!matches!(action, InputAction::TeamProposal { .. }));
    assert!(
        f.state
            .pending_team_proposals
            .contains_key(&f.conversation.id)
    );
    assert_eq!(host.counts(), HttpCounts { get: 0, post: 0 });
    host.stop().await;
}

#[tokio::test]
async fn k03_withdrawal_fences_late_ready_and_stale_key() {
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut f = fixture(&host).await;
    let raised = f
        .proposal_sink
        .as_ref()
        .expect("live sink")
        .pending(&f.conversation.id)
        .await
        .expect("raised proposal");
    rustain::infrastructure::runtime::team_proposal_bridge::withdraw_conversation(
        &mut f.state,
        &f.sender.app_state,
        &f.conversation.id,
    )
    .await;
    rustain::infrastructure::runtime::team_proposal_bridge::proposal_ready(
        &mut f.state,
        &f.sender.app_state,
        raised,
    )
    .await;
    assert!(
        !f.state
            .pending_team_proposals
            .contains_key(&f.conversation.id)
    );
    f.paint(60, 16);
    let action = f.key(DomainInputEvent::KeyPress('y')).await;
    assert!(!matches!(action, InputAction::TeamProposal { .. }));
    assert_eq!(host.counts(), HttpCounts { get: 0, post: 0 });
    host.stop().await;
}

#[tokio::test]
async fn k03_tab_away_and_return_preserves_only_active_surface_keys() {
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut f = fixture(&host).await;
    let original_id = f.conversation.id.clone();
    f.tabs.active_tab_mut().conversation = f.conversation.clone();
    f.tabs.create_tab();
    f.conversation = f.tabs.active_tab().conversation.clone();
    f.state.active_tab_id = f.tabs.active_tab_id();
    rustain::infrastructure::runtime::team_proposal_bridge::synchronize_surface(
        &mut f.state,
        &f.sender.app_state,
        &f.tabs,
        &f.conversation,
    )
    .await;
    assert!(
        !f.state
            .pending_team_proposals
            .contains_key(&f.conversation.id)
    );
    f.tabs.switch_to_prev();
    f.conversation = f.tabs.active_tab().conversation.clone();
    f.state.active_tab_id = f.tabs.active_tab_id();
    rustain::infrastructure::runtime::team_proposal_bridge::synchronize_surface(
        &mut f.state,
        &f.sender.app_state,
        &f.tabs,
        &f.conversation,
    )
    .await;
    f.paint(60, 16);
    assert!(matches!(
        f.key(DomainInputEvent::KeyPress('n')).await,
        InputAction::TeamProposal {
            decision: Some(_),
            ..
        }
    ));
    assert!(!f.state.pending_team_proposals.contains_key(&original_id));
    assert_eq!(host.counts(), HttpCounts { get: 0, post: 0 });
    host.stop().await;
}

#[tokio::test]
async fn k03_close_withdraws_before_tab_retirement() {
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut f = fixture(&host).await;
    let closed_id = f.conversation.id.clone();
    let tab_id = f.tabs.active_tab_id();
    f.tabs.active_tab_mut().conversation = f.conversation.clone();
    rustain::infrastructure::runtime::team_proposal_bridge::withdraw_conversation(
        &mut f.state,
        &f.sender.app_state,
        &closed_id,
    )
    .await;
    assert!(f.tabs.close_tab(tab_id).is_some());
    f.conversation = f.tabs.active_tab().conversation.clone();
    f.state.active_tab_id = f.tabs.active_tab_id();
    rustain::infrastructure::runtime::team_proposal_bridge::synchronize_surface(
        &mut f.state,
        &f.sender.app_state,
        &f.tabs,
        &f.conversation,
    )
    .await;
    assert!(!f.state.pending_team_proposals.contains_key(&closed_id));
    assert_eq!(host.counts(), HttpCounts { get: 0, post: 0 });
    host.stop().await;
}

#[tokio::test]
async fn k03_reset_all_withdraws_raised_snapshot_before_late_ready() {
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut f = fixture(&host).await;
    let raised = f
        .proposal_sink
        .as_ref()
        .expect("live sink")
        .pending(&f.conversation.id)
        .await
        .expect("raised proposal");
    rustain::infrastructure::runtime::team_proposal_bridge::withdraw_all(
        &mut f.state,
        &f.sender.app_state,
    )
    .await;
    rustain::infrastructure::runtime::team_proposal_bridge::proposal_ready(
        &mut f.state,
        &f.sender.app_state,
        raised,
    )
    .await;
    assert!(f.state.pending_team_proposals.is_empty());
    f.paint(60, 16);
    assert!(!matches!(
        f.key(DomainInputEvent::KeyPress('y')).await,
        InputAction::TeamProposal { .. }
    ));
    assert_eq!(host.counts(), HttpCounts { get: 0, post: 0 });
    host.stop().await;
}

#[tokio::test]
async fn k03_clear_reset_retires_old_conversation_on_real_tab() {
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut f = fixture(&host).await;
    let old_id = f.conversation.id.clone();
    let raised = f
        .proposal_sink
        .as_ref()
        .expect("live sink")
        .pending(&old_id)
        .await
        .expect("raised proposal");
    rustain::infrastructure::runtime::team_proposal_bridge::withdraw_conversation(
        &mut f.state,
        &f.sender.app_state,
        &old_id,
    )
    .await;
    f.conversation = rustain::domain::models::Conversation {
        id: rustain::domain::models::conversation::generate_conversation_id(),
        ..f.conversation.clone()
    };
    f.tabs.active_tab_mut().conversation = f.conversation.clone();
    rustain::infrastructure::runtime::team_proposal_bridge::proposal_ready(
        &mut f.state,
        &f.sender.app_state,
        raised,
    )
    .await;
    rustain::infrastructure::runtime::team_proposal_bridge::synchronize_surface(
        &mut f.state,
        &f.sender.app_state,
        &f.tabs,
        &f.conversation,
    )
    .await;
    assert!(!f.state.pending_team_proposals.contains_key(&old_id));
    f.paint(60, 16);
    assert!(!matches!(
        f.key(DomainInputEvent::KeyPress('y')).await,
        InputAction::TeamProposal { .. }
    ));
    assert_eq!(host.counts(), HttpCounts { get: 0, post: 0 });
    host.stop().await;
}
