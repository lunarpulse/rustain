use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::tui::app::InputAction;
use rustain::adapters::tui::decision_queue::{self, DecisionEffect, DecisionKey, DecisionKind};
use rustain::adapters::tui::presentation::{self, Surface};
use rustain::domain::models::{
    FocusState, ImageAttachment, InvocationStatus, JournalRecord, MessageRole, PermissionMode,
    ResponseMode, RoomEvent, StopReason,
};
use serde_json::json;

use super::observed_host::{HttpCounts, ObservedHost};
use super::proposal_harness::ProposalFixture;

#[allow(clippy::duplicate_mod)]
#[path = "../common/render_helpers.rs"]
mod stage;

async fn fixture(host: &ObservedHost) -> ProposalFixture {
    let mut f = ProposalFixture::start(vec![host.peer("lena-po")]).await;
    f.set_mode(PermissionMode::Yolo);
    host.reset_counts();
    let result = f
        .submit(json!({
            "recipients": ["lena-po"],
            "type": "architecture_update",
            "body": "Exact proposed body."
        }))
        .await;
    assert!(result.starts_with("Proposal shown to the operator."));
    assert!(f.proposal(&f.conversation.id).is_some());
    f
}

fn proposal_key(f: &ProposalFixture) -> DecisionKey {
    DecisionKey {
        kind: DecisionKind::TeamProposal,
        source: f.proposal(&f.conversation.id).unwrap().id.clone(),
        conversation_id: Some(f.conversation.id.clone()),
    }
}

fn open_proposal(f: &mut ProposalFixture) {
    let key = proposal_key(f);
    assert!(decision_queue::open(&mut f.state, &key));
}

fn press(code: KeyCode, modifiers: KeyModifiers) -> Event {
    Event::Key(KeyEvent::new(code, modifiers))
}

async fn assert_not_sent(f: &ProposalFixture, host: &ObservedHost) {
    assert!(f.proposal(&f.conversation.id).is_some());
    assert!(f.state.team_send_blocks.is_empty());
    assert_eq!(host.counts(), HttpCounts { get: 0, post: 0 });
    assert!(!f.sender.rows().await.iter().any(|row| matches!(
        row.record,
        JournalRecord::Room(RoomEvent::RemoteEnvelopeDispatched { .. })
    )));
}

async fn send_once(f: &mut ProposalFixture, host: &ObservedHost) {
    open_proposal(f);
    f.paint(60, 16);
    assert!(matches!(
        f.raw_key(press(KeyCode::Char('y'), KeyModifiers::NONE))
            .await,
        Some(InputAction::Decision(DecisionEffect::TeamProposalSend))
    ));
    f.settle(1).await;
    assert_eq!(host.counts(), HttpCounts { get: 0, post: 1 });
    assert_eq!(
        f.sender
            .rows()
            .await
            .iter()
            .filter(|row| matches!(
                row.record,
                JournalRecord::Room(RoomEvent::RemoteEnvelopeDispatched { .. })
            ))
            .count(),
        1
    );
}

#[tokio::test]
async fn k03_proposal_keys_are_dropped_during_first_paint_window() {
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut f = fixture(&host).await;
    open_proposal(&mut f);
    f.paint_unready(60, 16);
    for key in ['y', 'r', 'n'] {
        assert!(matches!(
            f.raw_key(press(KeyCode::Char(key), KeyModifiers::NONE))
                .await,
            Some(InputAction::Consumed)
        ));
        assert_not_sent(&f, &host).await;
    }
    f.raw_key(press(KeyCode::Char('c'), KeyModifiers::NONE))
        .await;
    assert_eq!(&*f.copies.lock(), &["Exact proposed body.".to_string()]);
    assert_not_sent(&f, &host).await;
    f.clock.advance(decision_queue::INERT_AFTER_PAINT);
    assert!(matches!(
        f.raw_key(press(KeyCode::Char('y'), KeyModifiers::NONE))
            .await,
        Some(InputAction::Decision(DecisionEffect::TeamProposalSend))
    ));
    f.settle(1).await;
    assert_eq!(host.counts(), HttpCounts { get: 0, post: 1 });
    host.stop().await;
}

#[tokio::test]
async fn k03_large_image_confirmation_owns_y_and_n_before_proposal_send() {
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut f = fixture(&host).await;
    for (key, attach) in [('y', true), ('n', false)] {
        decision_queue::raise_large_image(
            &mut f.state,
            ImageAttachment {
                media_type: "image/png".into(),
                data: "large-base64-image".into(),
            },
            "Large image. Attach it?",
        );
        let rows = f.paint_unready(60, 16);
        assert!(!rows.iter().any(|row| row.contains("[y] Send")));
        assert!(matches!(
            f.raw_key(press(KeyCode::Char(key), KeyModifiers::NONE))
                .await,
            Some(InputAction::Consumed)
        ));
        assert_not_sent(&f, &host).await;
        f.clock.advance(decision_queue::INERT_AFTER_PAINT);
        let action = f
            .raw_key(press(KeyCode::Char(key), KeyModifiers::NONE))
            .await;
        if attach {
            assert!(matches!(action, Some(InputAction::ImageConfirmAttach)));
        } else {
            assert!(matches!(action, Some(InputAction::ImageConfirmCancel)));
        }
        assert!(!f.state.decisions.holds(DecisionKind::LargeImage));
        assert_not_sent(&f, &host).await;
    }
    send_once(&mut f, &host).await;
    host.stop().await;
}

#[tokio::test]
async fn k03_proposal_frame_restores_help_and_tool_inspector_exactly() {
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut f = fixture(&host).await;
    // Defer the automatic frame so each explicitly opened owner is underneath it.
    open_proposal(&mut f);
    f.raw_key(press(KeyCode::Esc, KeyModifiers::NONE)).await;
    presentation::reducer::open(&mut f.state, Surface::Help);
    let help = f.state.modal_stack.top().unwrap().clone();
    open_proposal(&mut f);
    f.paint(60, 16);
    f.raw_key(press(KeyCode::Esc, KeyModifiers::NONE)).await;
    assert_eq!(f.state.modal_stack.top(), Some(&help));
    assert_not_sent(&f, &host).await;
    f.raw_key(press(KeyCode::Esc, KeyModifiers::NONE)).await;

    f.conversation
        .messages
        .push(stage::make_msg("inspected-tool", MessageRole::Assistant));
    f.conversation.turns.push(stage::make_turn(
        "inspected-tool",
        vec![stage::tool("Read", InvocationStatus::Success)],
        Some(StopReason::EndTurn),
    ));
    stage::inspect_stage_tool(&mut f.state, &f.conversation, None);
    let owner = f.state.modal_stack.top().unwrap().clone();
    assert!(matches!(owner.surface, Surface::ToolInspector(_)));
    for repaint in [false, true] {
        if repaint {
            f.paint(60, 16);
        }
        f.raw_key(press(KeyCode::Char('y'), KeyModifiers::NONE))
            .await;
        assert_eq!(f.state.modal_stack.top(), Some(&owner));
        assert_not_sent(&f, &host).await;
    }
    open_proposal(&mut f);
    f.paint(60, 16);
    f.raw_key(press(KeyCode::Esc, KeyModifiers::NONE)).await;
    assert_eq!(f.state.modal_stack.top(), Some(&owner));
    assert_not_sent(&f, &host).await;
    f.raw_key(press(KeyCode::Esc, KeyModifiers::NONE)).await;
    send_once(&mut f, &host).await;
    host.stop().await;
}

#[tokio::test]
async fn k03_raw_guards_and_composer_readline_keep_their_owners() {
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut f = fixture(&host).await;
    open_proposal(&mut f);
    f.paint(60, 16);
    for key in ['y', 'r', 'n'] {
        for kind in [
            KeyEventKind::Press,
            KeyEventKind::Repeat,
            KeyEventKind::Release,
        ] {
            for modifiers in [
                KeyModifiers::NONE,
                KeyModifiers::SHIFT,
                KeyModifiers::ALT,
                KeyModifiers::CONTROL,
            ] {
                if kind == KeyEventKind::Press && modifiers == KeyModifiers::NONE {
                    continue;
                }
                let event =
                    Event::Key(KeyEvent::new_with_kind(KeyCode::Char(key), modifiers, kind));
                assert!(
                    f.raw_key(event).await.is_none(),
                    "{key} {kind:?} {modifiers:?}"
                );
            }
        }
    }
    assert_not_sent(&f, &host).await;
    // Plain r is a positive control: it opens Review, not a send.
    f.raw_key(press(KeyCode::Char('r'), KeyModifiers::NONE))
        .await;
    let rows = f.paint(60, 16);
    assert!(rows.iter().any(|row| row.contains("Exact proposed body.")));
    f.raw_key(press(KeyCode::Esc, KeyModifiers::NONE)).await;
    f.raw_key(press(KeyCode::Esc, KeyModifiers::NONE)).await;
    presentation::reducer::set_base(&mut f.state, FocusState::Input);
    f.raw_key(press(KeyCode::Char('r'), KeyModifiers::CONTROL))
        .await;
    assert!(f.state.reverse_search.is_open());
    f.raw_key(press(KeyCode::Esc, KeyModifiers::NONE)).await;
    assert!(!f.state.reverse_search.is_open());
    f.raw_key(press(KeyCode::Char('Y'), KeyModifiers::SHIFT))
        .await;
    assert_eq!(f.state.input_buffer, "Y");
    f.raw_key(press(KeyCode::Char('a'), KeyModifiers::CONTROL))
        .await;
    f.raw_key(press(KeyCode::Char('x'), KeyModifiers::NONE))
        .await;
    assert_eq!(f.state.input_buffer, "xY");
    assert_not_sent(&f, &host).await;
    f.state.input_buffer.clear();
    f.state.cursor_position = 0;
    send_once(&mut f, &host).await;
    host.stop().await;
}

#[tokio::test]
async fn k03_proposal_cannot_be_decided_from_another_tab() {
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut f = fixture(&host).await;
    let origin = f.conversation.id.clone();
    let key = proposal_key(&f);
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
    decision_queue::prune_closed(
        &mut f.state,
        &rustain::adapters::tui::notification_log::LogFacts {
            active_conversation_id: Some(f.conversation.id.clone()),
            ..Default::default()
        },
    );
    assert!(!decision_queue::open(&mut f.state, &key));
    f.raw_key(press(KeyCode::Char('y'), KeyModifiers::NONE))
        .await;
    assert!(f.proposal(&origin).is_some());
    assert_eq!(host.counts(), HttpCounts { get: 0, post: 0 });
    assert!(!f.sender.rows().await.iter().any(|row| matches!(
        row.record,
        JournalRecord::Room(RoomEvent::RemoteEnvelopeDispatched { .. })
    )));
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
    decision_queue::prune_closed(
        &mut f.state,
        &rustain::adapters::tui::notification_log::LogFacts {
            active_conversation_id: Some(origin),
            ..Default::default()
        },
    );
    f.state.input_buffer.clear();
    f.state.cursor_position = 0;
    send_once(&mut f, &host).await;
    host.stop().await;
}

#[tokio::test]
async fn k03_composer_gate_preserves_draft_and_clearing_unblocks_decision() {
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut f = fixture(&host).await;
    open_proposal(&mut f);
    f.state.input_buffer = "draft".into();
    f.state.cursor_position = 2;
    let rows = f.paint(60, 16);
    assert!(
        rows.iter()
            .any(|row| row.contains("Finish or clear your draft"))
    );
    for key in ['y', 'r', 'n'] {
        assert!(matches!(
            f.raw_key(press(KeyCode::Char(key), KeyModifiers::NONE))
                .await,
            Some(InputAction::Consumed)
        ));
        assert_eq!(f.state.input_buffer, "draft");
        assert_eq!(f.state.cursor_position, 2);
        assert_not_sent(&f, &host).await;
    }
    // The frame does not edit the shared composer: defer it to clear the draft.
    f.raw_key(press(KeyCode::Esc, KeyModifiers::NONE)).await;
    presentation::reducer::set_base(&mut f.state, FocusState::Input);
    f.raw_key(press(KeyCode::Char('u'), KeyModifiers::CONTROL))
        .await;
    f.raw_key(press(KeyCode::Char('k'), KeyModifiers::CONTROL))
        .await;
    assert!(f.state.input_buffer.is_empty());
    send_once(&mut f, &host).await;
    host.stop().await;
}
