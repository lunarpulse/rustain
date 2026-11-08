use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::tui::app::InputAction;
use rustain::adapters::tui::decision_queue::{
    DecisionKind, PaneOutcome, handle_domain_event, note_painted, raise_large_image,
};
use rustain::adapters::tui::notification_log::LogFacts;
use rustain::adapters::tui::notifications::NotificationStore;
use rustain::adapters::tui::state::TuiState;
use rustain::domain::clock::MockClock;
use rustain::domain::events::DomainInputEvent;
use rustain::domain::models::NotificationsConfig;
use rustain::domain::models::{
    FocusState, ImageAttachment, JournalRecord, PermissionMode, ResponseMode, RoomEvent,
    TeamProposalDecision,
};
use serde_json::json;

use super::observed_host::{HttpCounts, ObservedHost};
use super::proposal_harness::ProposalFixture;

#[allow(clippy::duplicate_mod)]
#[path = "../common/render_helpers.rs"]
mod stage;

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

/// The event loop's pane dispatch, as the loop calls it: `Some` when the pane owned the key.
fn pane_key(state: &mut TuiState, key: char) -> Option<InputAction> {
    match handle_domain_event(
        state,
        &DomainInputEvent::KeyPress(key),
        &LogFacts::default(),
    )? {
        PaneOutcome::Effect(effect) => Some(effect.into_action()),
        _ => Some(InputAction::Consumed),
    }
}

#[tokio::test]
async fn k03_large_image_confirmation_owns_y_and_n_before_proposal_send() {
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut fixture = fixture(&host).await;

    // Old (19.21b): `handle_input` mapped `y`/`n` to ImageConfirmAttach/Cancel. New
    // (21.1 S2, AC9/AC12): the confirmation is a decision-pane item and the event
    // loop's pane dispatch owns its keys before any proposal action, inert for 500 ms.
    // Old -> new (21.6 6-A): the TuiState large-image slot is gone; the queued image
    // item IS the confirmation (and the gate that hides the proposal while it waits).
    let clock = Arc::new(MockClock::new(Instant::now()));
    fixture.state.notifications =
        NotificationStore::from_config(&NotificationsConfig::default(), clock.clone());
    let ask = |state: &mut TuiState| {
        raise_large_image(
            state,
            ImageAttachment {
                media_type: "image/png".into(),
                data: "large-base64-image".into(),
            },
            "Large image. Attach it?",
        );
        note_painted(state);
    };
    ask(&mut fixture.state);
    // Even a proposal visibility marker from the preceding paint cannot own y; the
    // inert window swallows it first, so nothing is attached or sent.
    assert!(matches!(
        pane_key(&mut fixture.state, 'y'),
        Some(InputAction::Consumed)
    ));
    // Old -> new: the slot outlived the answer, so the hidden-proposal check ran after
    // `y`; an answered item now leaves the queue, so it runs while the item still waits.
    let rows = fixture.paint(60, 16);
    assert!(fixture.state.team_proposal_visible.is_none());
    assert!(!rows.iter().any(|row| row.contains("[y] Send")));
    clock.advance(Duration::from_millis(501));
    assert!(matches!(
        pane_key(&mut fixture.state, 'y'),
        Some(InputAction::ImageConfirmAttach)
    ));
    assert!(!fixture.state.decisions.holds(DecisionKind::LargeImage));
    ask(&mut fixture.state);
    clock.advance(Duration::from_millis(501));
    assert!(matches!(
        pane_key(&mut fixture.state, 'n'),
        Some(InputAction::ImageConfirmCancel)
    ));
    assert_not_sent(&fixture, &host).await;

    // The fixture dispatches proposal actions only; each answer already removed its
    // image confirmation (as the real attach/cancel dispatcher consumes it), so the
    // same proposal can now be authorized.
    assert!(!fixture.state.decisions.holds(DecisionKind::LargeImage));
    send_once(&mut fixture, &host).await;
    host.stop().await;
}

fn paint_owned_frame(fixture: &mut ProposalFixture) -> Vec<String> {
    fixture.tabs.active_tab_mut().conversation = fixture.conversation.clone();
    fixture.state.terminal_width = 60;
    fixture.state.terminal_height = 16;
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 16)).unwrap();
    let core = rustain::infrastructure::runtime::agent_core::AgentCore::test_noop();
    let count = fixture.tabs.tab_count();
    rustain::infrastructure::runtime::event_loop::render(
        &mut terminal,
        &mut fixture.state,
        &fixture.conversation,
        &Default::default(),
        "fixture-model",
        "fixture-provider",
        &*fixture.security,
        count,
        0,
        Some(&mut fixture.tabs),
        &rustain::domain::services::session_index::SessionIndex::new(),
        128_000,
        &core,
    )
    .unwrap();
    terminal
        .backend()
        .buffer()
        .content
        .chunks(60)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect())
        .collect()
}

fn assert_tool_covers_proposal(fixture: &ProposalFixture, rows: &[String]) {
    // Shared frames keep the underlay metadata, but it cannot authorize a key.
    assert!(fixture.state.team_proposal_visible.is_some());
    assert!(rows.iter().any(|row| row.contains("Tool · Read #1")));
    assert!(!rows.iter().any(|row| row.contains("[y] Send")));
}

#[tokio::test]
async fn k03_tool_inspector_owns_y_without_sending_with_stale_or_fresh_paint() {
    use rustain::adapters::tui::presentation::Surface;
    use rustain::domain::models::{InvocationStatus, MessageRole, StopReason};

    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut fixture = fixture(&host).await;
    // The ownership witness uses a real, renderable invocation, not the old
    // wire-id-only peek marker. The pending proposal remains the send target.
    fixture
        .conversation
        .messages
        .push(stage::make_msg("inspected-tool", MessageRole::Assistant));
    fixture.conversation.turns.push(stage::make_turn(
        "inspected-tool",
        vec![stage::tool("Read", InvocationStatus::Success)],
        Some(StopReason::EndTurn),
    ));
    for repaint in [false, true] {
        stage::inspect_stage_tool(&mut fixture.state, &fixture.conversation, None);
        let owner = fixture.state.modal_stack.top().unwrap().clone();
        assert!(matches!(owner.surface, Surface::ToolInspector(_)));
        if repaint {
            let rows = paint_owned_frame(&mut fixture);
            assert_tool_covers_proposal(&fixture, &rows);
        }
        assert!(matches!(
            fixture
                .raw_key(press(KeyCode::Char('y'), KeyModifiers::NONE))
                .await,
            Some(InputAction::Consumed)
        ));
        assert_not_sent(&fixture, &host).await;
        if !repaint {
            let rows = paint_owned_frame(&mut fixture);
            assert_tool_covers_proposal(&fixture, &rows);
        }
        assert_eq!(fixture.state.modal_stack.top(), Some(&owner));
        assert!(matches!(
            fixture
                .raw_key(press(KeyCode::Esc, KeyModifiers::NONE))
                .await,
            Some(InputAction::Consumed)
        ));
        assert!(fixture.state.modal_stack.is_empty());
        assert!(fixture.state.tool_inspectors.is_empty());
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
    assert!(fixture.state.reverse_search.is_open());
    assert_eq!(fixture.state.focus, FocusState::Input);
    assert!(fixture.state.modal_stack.is_empty());
    assert_not_sent(&fixture, &host).await;
    assert!(matches!(
        fixture
            .raw_key(press(KeyCode::Esc, KeyModifiers::NONE))
            .await,
        Some(InputAction::Consumed)
    ));
    assert!(!fixture.state.reverse_search.is_open());
    assert_eq!(fixture.state.focus, FocusState::Input);
    assert!(fixture.state.modal_stack.is_empty());
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
    assert!(!fixture.state.reverse_search.is_open());
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
