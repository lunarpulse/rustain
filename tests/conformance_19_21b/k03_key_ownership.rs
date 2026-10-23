use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::tui::app::InputAction;
use rustain::domain::models::{
    FocusState, ImageAttachment, JournalRecord, PermissionMode, ResponseMode, RoomEvent,
    TeamProposalDecision,
};
use serde_json::json;

use super::observed_host::{HttpCounts, ObservedHost};
use super::proposal_harness::ProposalFixture;

async fn fixture(host: &ObservedHost) -> ProposalFixture {
    let mut fixture = ProposalFixture::start(vec![host.peer("lena-po")]).await;
    fixture.set_mode(PermissionMode::Yolo);
    host.reset_counts();
    let result = fixture
        .submit(json!({
            "recipients": ["lena-po"],
            "type": "architecture_update",
            "body": "Exact proposed body."
        }))
        .await;
    assert!(result.starts_with("Proposal shown to the operator."));
    fixture.state.focus = FocusState::Chat;
    fixture.paint(60, 16);
    assert!(fixture.state.team_proposal_visible.is_some());
    fixture
}

fn press(code: KeyCode, modifiers: KeyModifiers) -> Event {
    Event::Key(KeyEvent::new(code, modifiers))
}

async fn assert_not_sent(fixture: &ProposalFixture, host: &ObservedHost) {
    assert!(
        fixture
            .state
            .pending_team_proposals
            .contains_key(&fixture.conversation.id)
    );
    assert!(fixture.state.team_send_blocks.is_empty());
    assert_eq!(host.counts(), HttpCounts { get: 0, post: 0 });
    assert!(!fixture.sender.rows().await.iter().any(|row| matches!(
        row.record,
        JournalRecord::Room(RoomEvent::RemoteEnvelopeDispatched { .. })
    )));
}

async fn send_once(fixture: &mut ProposalFixture, host: &ObservedHost) {
    fixture.paint(60, 16);
    assert!(matches!(
        fixture
            .raw_key(press(KeyCode::Char('y'), KeyModifiers::NONE))
            .await,
        Some(InputAction::TeamProposal {
            decision: Some(TeamProposalDecision::Send),
            ..
        })
    ));
    fixture.settle(1).await;
    assert_eq!(host.counts(), HttpCounts { get: 0, post: 1 });
    let dispatches = fixture
        .sender
        .rows()
        .await
        .iter()
        .filter(|row| {
            matches!(
                row.record,
                JournalRecord::Room(RoomEvent::RemoteEnvelopeDispatched { .. })
            )
        })
        .count();
    assert_eq!(dispatches, 1);
}

#[tokio::test]
async fn k03_large_image_confirmation_owns_y_and_n_before_proposal_send() {
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut fixture = fixture(&host).await;
    fixture.state.pending_large_image = Some(ImageAttachment {
        media_type: "image/png".into(),
        data: "large-base64-image".into(),
    });

    // Even a proposal visibility marker from the preceding paint cannot own y.
    assert!(matches!(
        fixture
            .raw_key(press(KeyCode::Char('y'), KeyModifiers::NONE))
            .await,
        Some(InputAction::ImageConfirmAttach)
    ));
    let rows = fixture.paint(60, 16);
    assert!(fixture.state.team_proposal_visible.is_none());
    assert!(!rows.iter().any(|row| row.contains("[y] Send")));
    assert!(matches!(
        fixture
            .raw_key(press(KeyCode::Char('n'), KeyModifiers::NONE))
            .await,
        Some(InputAction::ImageConfirmCancel)
    ));
    assert_not_sent(&fixture, &host).await;

    // The fixture dispatches proposal actions only; clear the image confirmation
    // as its real attach/cancel dispatcher does, then authorize this same proposal.
    fixture.state.pending_large_image = None;
    send_once(&mut fixture, &host).await;
    host.stop().await;
}

#[tokio::test]
async fn k03_tool_peek_dismisses_y_without_sending_with_stale_or_fresh_paint() {
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut fixture = fixture(&host).await;
    // The real scripted turn emitted this tool call; p creates its collapsed state.
    fixture.state.focused_tool_id = Some("proposal-0".into());
    for repaint in [false, true] {
        assert!(matches!(
            fixture
                .raw_key(press(KeyCode::Char('p'), KeyModifiers::NONE))
                .await,
            Some(InputAction::Consumed)
        ));
        assert!(fixture.state.tool_block_states["proposal-0"].peek_active);
        if repaint {
            let rows = fixture.paint(60, 16);
            assert!(fixture.state.team_proposal_visible.is_none());
            assert!(!rows.iter().any(|row| row.contains("[y] Send")));
        }
        assert!(matches!(
            fixture
                .raw_key(press(KeyCode::Char('y'), KeyModifiers::NONE))
                .await,
            Some(InputAction::Consumed)
        ));
        assert!(!fixture.state.tool_block_states["proposal-0"].peek_active);
        assert_not_sent(&fixture, &host).await;
        fixture.paint(60, 16);
    }
    send_once(&mut fixture, &host).await;
    host.stop().await;
}

#[tokio::test]
async fn k03_ctrl_r_keeps_reverse_search_but_modified_and_repeat_decisions_do_not_authorize() {
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut fixture = fixture(&host).await;
    fixture.state.focus = FocusState::Input;
    assert!(matches!(
        fixture
            .raw_key(press(KeyCode::Char('r'), KeyModifiers::CONTROL))
            .await,
        Some(InputAction::Consumed)
    ));
    assert!(fixture.state.reverse_search.active);
    assert_not_sent(&fixture, &host).await;
    assert!(matches!(
        fixture
            .raw_key(press(KeyCode::Esc, KeyModifiers::NONE))
            .await,
        Some(InputAction::Consumed)
    ));
    assert!(!fixture.state.reverse_search.active);
    assert_eq!(fixture.state.focus, FocusState::Input);
    fixture.paint(60, 16);

    for key in ['y', 'r', 'n'] {
        for modifiers in [
            KeyModifiers::CONTROL,
            KeyModifiers::ALT,
            KeyModifiers::SHIFT,
        ] {
            if key == 'r' && modifiers == KeyModifiers::CONTROL {
                continue; // This exact press is the dedicated reverse-search shortcut.
            }
            assert!(
                fixture
                    .raw_key(press(KeyCode::Char(key), modifiers))
                    .await
                    .is_none(),
                "modified proposal character must not authorize: {key} {modifiers:?}"
            );
        }
        for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
            assert!(
                fixture
                    .raw_key(Event::Key(KeyEvent::new_with_kind(
                        KeyCode::Char(key),
                        KeyModifiers::NONE,
                        kind,
                    )))
                    .await
                    .is_none(),
                "non-press proposal character must not authorize: {key} {kind:?}"
            );
        }
    }
    assert!(
        fixture
            .raw_key(Event::Key(KeyEvent::new_with_kind(
                KeyCode::Char('r'),
                KeyModifiers::CONTROL,
                KeyEventKind::Repeat,
            )))
            .await
            .is_none()
    );
    assert!(!fixture.state.reverse_search.active);
    assert_not_sent(&fixture, &host).await;
    send_once(&mut fixture, &host).await;
    host.stop().await;
}

#[tokio::test]
async fn k03_clearing_a_draft_requires_a_fresh_decision_footer_paint() {
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut f = fixture(&host).await;
    f.state.focus = FocusState::Input;
    f.state.input_buffer = "d".into();
    f.state.cursor_position = 1;
    let rows = f.paint(60, 16);
    assert!(
        rows.iter()
            .any(|row| row.contains("Finish or clear your draft"))
    );
    f.raw_key(press(KeyCode::Backspace, KeyModifiers::NONE))
        .await;
    assert!(f.state.input_buffer.is_empty());
    assert!(matches!(
        f.raw_key(press(KeyCode::Char('y'), KeyModifiers::NONE))
            .await,
        Some(InputAction::Consumed)
    ));
    assert_not_sent(&f, &host).await;
    send_once(&mut f, &host).await;
    host.stop().await;
}
