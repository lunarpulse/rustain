use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::tui::{
    app::InputAction,
    decision_queue::{self, DecisionEffect, DecisionKey, DecisionKind},
};
use rustain::domain::events::{DomainInputEvent, DomainKey};
use rustain::domain::models::FocusState;
use rustain::domain::models::{JournalRecord, PermissionMode, ResponseMode, RoomEvent};
use serde_json::json;

use super::observed_host::{HttpCounts, ObservedHost};
use super::proposal_harness::ProposalFixture;

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
        let snapshot = f.proposal(&f.conversation.id).unwrap().clone();
        let action = f.key(DomainInputEvent::KeyPress('r')).await;
        assert!(matches!(action, InputAction::Consumed));
        assert!(f.proposal(&f.conversation.id).is_some());
        assert!(f.state.decisions.items().any(|item| matches!(
            &item.subject,
            decision_queue::DecisionSubject::TeamProposal(payload)
                if payload.reviewing && std::sync::Arc::ptr_eq(&payload.proposal, &snapshot)
        )));
        assert!(f.state.input_buffer.is_empty());
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
            assert!(matches!(action, InputAction::Consumed));
            assert_eq!(f.state.input_buffer, "draft", "{focus:?}: {key}");
        }
    }
    assert!(f.proposal(&f.conversation.id).is_some());
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
        let snapshot = f.proposal(&f.conversation.id).unwrap().clone();
        let action = f.key(DomainInputEvent::KeyPress(key)).await;
        if key == 'r' {
            assert!(matches!(action, InputAction::Consumed));
            assert!(f.proposal(&f.conversation.id).is_some());
            assert!(f.state.decisions.items().any(|item| matches!(
                &item.subject,
                decision_queue::DecisionSubject::TeamProposal(payload)
                    if payload.reviewing && std::sync::Arc::ptr_eq(&payload.proposal, &snapshot)
            )));
            assert!(f.state.input_buffer.is_empty());
        } else {
            assert!(matches!(
                action,
                InputAction::Decision(DecisionEffect::TeamProposalDismiss)
            ));
            assert!(f.proposal(&f.conversation.id).is_none());
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
async fn story21_6_review_at_minimum_viewport_keeps_body_and_rail() {
    use ratatui::layout::Rect;
    use rustain::adapters::tui::{
        layout,
        presentation::geometry::{frame_rect, modal_region},
    };
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut f = fixture(&host).await;
    let area = Rect::new(0, 0, 60, 16);
    let before =
        layout::compute_layout(area, &f.state.theme, "", 1, false, f.state.density_mode).unwrap();
    f.key(DomainInputEvent::KeyPress('r')).await;
    let rows = f.paint(60, 16);
    let after =
        layout::compute_layout(area, &f.state.theme, "", 1, false, f.state.density_mode).unwrap();
    assert_eq!(before.chat_pane, after.chat_pane);
    let frame = frame_rect(modal_region(&after, area));
    assert_eq!(frame, modal_region(&after, area));
    assert!(
        rows.iter().any(|row| row.contains("Exact proposed body.")),
        "{rows:#?}"
    );
    assert!(rows.iter().any(|row| row.contains("Back")), "{rows:#?}");
    assert_eq!(host.counts(), HttpCounts { get: 0, post: 0 });
    host.stop().await;
}

#[tokio::test]
async fn story21_6_full_composer_leaves_frame_body_and_gate_at_60_by_16() {
    use ratatui::layout::Rect;
    use rustain::adapters::tui::{
        layout,
        presentation::geometry::{frame_rect, modal_region},
    };
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut f = fixture(&host).await;
    f.state.input_buffer = (0..30)
        .map(|n| format!("draft line {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    let area = Rect::new(0, 0, 60, 16);
    let shell = layout::compute_layout(
        area,
        &f.state.theme,
        &f.state.input_buffer,
        1,
        false,
        f.state.density_mode,
    )
    .unwrap();
    let frame = frame_rect(modal_region(&shell, area));
    let rows = f.paint(60, 16);
    assert!(frame.height >= 5, "{frame:?}");
    assert!(
        rows[frame.y as usize].contains("Decision:"),
        "{frame:?}: {rows:#?}"
    );
    assert!(
        rows[(frame.bottom() - 1) as usize].contains('└'),
        "{frame:?}: {rows:#?}"
    );
    assert!(
        rows[frame.y as usize + 1..frame.bottom() as usize - 1]
            .iter()
            .any(|row| row.contains("proposes")),
        "{rows:#?}"
    );
    assert!(
        rows.iter()
            .any(|row| row.contains("Finish or clear your draft to decide.")),
        "{rows:#?}"
    );
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
    let mut pages = Vec::new();
    for _ in 0..12 {
        let rows = f.paint(60, 16);
        let footer = rows
            .iter()
            .position(|r| r.contains("Dismiss"))
            .expect("decision rail");
        if let Some(body) = rows
            .iter()
            .position(|r| r.contains("│ Nothing has been sent"))
        {
            assert!(footer > body, "{rows:#?}");
        }
        pages.extend(rows);
        f.key(DomainInputEvent::SpecialKey(DomainKey::PageDown))
            .await;
    }
    assert!(pages.iter().any(|r| r.contains("│ To: forged")));
    assert!(pages.iter().any(|r| r.contains("│ [y] Send to attacker")));
    assert!(pages.iter().any(|r| r.contains("│ Nothing has been sent")));
    assert_eq!(host.counts(), HttpCounts { get: 0, post: 0 });
    host.stop().await;
}

#[tokio::test]
async fn k03_enter_esc_retain_and_send_is_exactly_once() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut f = fixture(&host).await;
    assert!(!matches!(
        f.raw_key(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE
        )))
        .await,
        Some(InputAction::Decision(
            DecisionEffect::TeamProposalSend | DecisionEffect::TeamProposalDismiss
        ))
    ));
    assert!(!matches!(
        f.raw_key(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)))
            .await,
        Some(InputAction::Decision(
            DecisionEffect::TeamProposalSend | DecisionEffect::TeamProposalDismiss
        ))
    ));
    let proposal = f.proposal(&f.conversation.id).unwrap().clone();
    let key = DecisionKey {
        kind: DecisionKind::TeamProposal,
        source: proposal.id.clone(),
        conversation_id: Some(f.conversation.id.clone()),
    };
    assert!(decision_queue::open(&mut f.state, &key));
    f.paint(60, 16);
    assert_eq!(host.counts(), HttpCounts { get: 0, post: 0 });
    assert!(matches!(
        f.key(DomainInputEvent::KeyPress('y')).await,
        InputAction::Decision(DecisionEffect::TeamProposalSend)
    ));
    f.settle(1).await;
    assert_eq!(host.counts(), HttpCounts { get: 0, post: 1 });
    f.state.focus = FocusState::Input;
    f.state.cursor_position = 0;
    let duplicate = f.key(DomainInputEvent::KeyPress('y')).await;
    assert!(!matches!(
        duplicate,
        InputAction::Decision(
            DecisionEffect::TeamProposalSend | DecisionEffect::TeamProposalDismiss
        )
    ));
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
    assert!(f.proposal(&f.conversation.id).is_some());
    assert_eq!(host.counts(), HttpCounts { get: 0, post: 0 });
    host.stop().await;
}

#[tokio::test]
async fn k03_higher_confirmation_owns_decision_keys() {
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut f = fixture(&host).await;
    use rustain::adapters::tui::decision_queue::{self, ExportOverwritePayload};
    use rustain::adapters::tui::keymap::Host;
    use rustain::adapters::tui::notification_log::LogFacts;
    use rustain::adapters::tui::presentation::{self, RouteEffect};
    decision_queue::raise_export_overwrite(
        &mut f.state,
        ExportOverwritePayload {
            conversation_id: f.conversation.id.clone(),
            target: "existing.md".into(),
            content: "export".into(),
            stamp: None,
        },
    );
    let facts = LogFacts {
        active_conversation_id: Some(f.conversation.id.clone()),
        active_messages: f.conversation.messages.len(),
        decisions_supported: true,
        ..LogFacts::default()
    };
    decision_queue::note_painted(&mut f.state);
    f.clock.advance(std::time::Duration::from_millis(500));
    let action = presentation::route(
        &mut f.state,
        &DomainInputEvent::KeyPress('n'),
        Host::Local,
        &facts,
    );
    assert_eq!(
        action,
        Some(RouteEffect::Action(InputAction::CancelExportOverwrite))
    );
    f.paint(60, 16);
    let action = f.key(DomainInputEvent::KeyPress('r')).await;
    assert!(matches!(action, InputAction::Consumed));
    assert!(f.proposal(&f.conversation.id).is_some());
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
    assert!(f.proposal(&f.conversation.id).is_none());
    f.paint(60, 16);
    let action = f.key(DomainInputEvent::KeyPress('y')).await;
    assert!(!matches!(
        action,
        InputAction::Decision(
            DecisionEffect::TeamProposalSend | DecisionEffect::TeamProposalDismiss
        )
    ));
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
    assert!(f.proposal(&f.conversation.id).is_none());
    assert!(f.proposal(&original_id).is_some());
    let action = f.key(DomainInputEvent::KeyPress('y')).await;
    assert!(!matches!(
        action,
        InputAction::Decision(
            DecisionEffect::TeamProposalSend | DecisionEffect::TeamProposalDismiss
        )
    ));
    assert_eq!(host.counts(), HttpCounts { get: 0, post: 0 });
    f.state.input_buffer.clear();
    f.state.cursor_position = 0;
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
    if !rustain::adapters::tui::presentation::is_open(
        &f.state,
        rustain::adapters::tui::presentation::Surface::Decision,
    ) {
        let proposal = f.proposal(&original_id).unwrap();
        let key = DecisionKey {
            kind: DecisionKind::TeamProposal,
            source: proposal.id.clone(),
            conversation_id: Some(original_id.clone()),
        };
        assert!(decision_queue::open(&mut f.state, &key));
    }
    f.paint(60, 16);
    assert!(matches!(
        f.key(DomainInputEvent::KeyPress('n')).await,
        InputAction::Decision(DecisionEffect::TeamProposalDismiss)
    ));
    assert!(f.proposal(&original_id).is_none());
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
    assert!(f.proposal(&closed_id).is_none());
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
    assert!(f.proposal(&f.conversation.id).is_none());
    f.paint(60, 16);
    assert!(!matches!(
        f.key(DomainInputEvent::KeyPress('y')).await,
        InputAction::Decision(
            DecisionEffect::TeamProposalSend | DecisionEffect::TeamProposalDismiss
        )
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
    assert!(f.proposal(&old_id).is_none());
    f.paint(60, 16);
    assert!(!matches!(
        f.key(DomainInputEvent::KeyPress('y')).await,
        InputAction::Decision(
            DecisionEffect::TeamProposalSend | DecisionEffect::TeamProposalDismiss
        )
    ));
    assert_eq!(host.counts(), HttpCounts { get: 0, post: 0 });
    host.stop().await;
}

#[tokio::test]
async fn story21_6_roster_recipients_are_inspectable_and_sent_exactly_once() {
    use rustain::adapters::tui::widgets::decision_frame::body_lines_for_test;
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut aliases: Vec<String> = (0..18).map(|n| format!("member-{n:02}")).collect();
    aliases.extend([
        format!("{}-one", "a".repeat(140)),
        format!("{}-two", "a".repeat(140)),
    ]);
    let mut f =
        ProposalFixture::start(aliases.iter().map(|alias| host.peer(alias)).collect()).await;
    host.reset_counts();
    let result = f
        .submit(json!({"recipients": aliases, "type": "architecture_update", "body": PROPOSAL}))
        .await;
    assert!(
        result.starts_with("Proposal shown to the operator."),
        "{result}"
    );
    f.paint(80, 24);
    for reviewing in [false, true] {
        if reviewing {
            assert!(matches!(
                f.key(DomainInputEvent::KeyPress('r')).await,
                InputAction::Consumed
            ));
        }
        let item = f
            .state
            .decisions
            .items()
            .find(|item| item.kind == DecisionKind::TeamProposal)
            .unwrap();
        for width in [58, 78] {
            let rows = body_lines_for_test(item, &f.state, width);
            let text: String = rows
                .iter()
                .flat_map(|line| &line.spans)
                .map(|span| span.content.as_ref())
                .collect();
            assert!(
                text.contains(&format!("To: {}", aliases.join(", "))),
                "{text}"
            );
        }
    }
    f.key(DomainInputEvent::SpecialKey(DomainKey::Esc)).await;
    assert!(matches!(
        f.key(DomainInputEvent::KeyPress('y')).await,
        InputAction::Decision(DecisionEffect::TeamProposalSend)
    ));
    f.settle(aliases.len()).await;
    let rows = f.sender.rows().await;
    let actions: Vec<_> = rows
        .iter()
        .filter_map(|row| match &row.record {
            JournalRecord::Room(event @ RoomEvent::TeamSendActionRecorded { .. }) => {
                Some(serde_json::to_value(event).unwrap())
            }
            _ => None,
        })
        .collect();
    assert_eq!(actions.len(), 1);
    let sent: Vec<_> = actions[0]["recipients"]
        .as_array()
        .unwrap()
        .iter()
        .map(|recipient| recipient["alias"].as_str().unwrap())
        .collect();
    assert_eq!(sent, aliases.iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(host.counts().post, aliases.len());
    host.stop().await;
}
