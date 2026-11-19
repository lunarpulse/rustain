//! K6 (slice 6-B): the S-QUEUE list. `Ctrl+X q` (the real leader row) opens the list
//! of every waiting decision; Enter opens the selected item as its own frame; a
//! deferred item is never reopened by surprise. Rows are read from the painted buffer.

use std::sync::Arc;
use std::time::Duration;

use crate::conformance_21_5_tool_inspector::support::{Fixture, prose};
use rustain::adapters::tui::app::{InputAction, handle_input};
use rustain::adapters::tui::decision_queue::{self, Arrival, DecisionDraft, DecisionKind};
use rustain::adapters::tui::handlers::transcript_target::durable_target;
use rustain::adapters::tui::keymap::Key;
use rustain::adapters::tui::presentation::{self, Surface};
use rustain::adapters::tui::state::TuiState;
use rustain::domain::clock::MockClock;
use rustain::domain::events::DomainKey;
use rustain::domain::models::tool_call::{ApprovalSource, RequestId};
use rustain::domain::models::{ToolRisk, Turn};
use rustain::domain::services::approval_runtime::{ApprovalRuntimeEvent, PendingReview};
use rustain::infrastructure::runtime::approval_bridge;

use super::support::{clocked, facts, key, paint, ready, route, row_text, screen, special};

fn permission(state: &mut TuiState, tool: &str) {
    let id = RequestId::new();
    let source = ApprovalSource::ForegroundTurn {
        conversation_id: "c1".into(),
    };
    let review = PendingReview {
        id: id.clone(),
        source: source.clone(),
        tool: tool.into(),
        risk: ToolRisk::Elevated,
        input: Arc::new(serde_json::json!({"command": tool})),
    };
    approval_bridge::apply(
        state,
        ApprovalRuntimeEvent::Requested {
            id,
            source,
            tool: tool.into(),
            risk: ToolRisk::Elevated,
            consent_card: None,
        },
        Some(review),
    );
}

/// Press `Ctrl+X q` through the real leader and run the effect the host runs.
fn ctrl_x_q(state: &mut TuiState) {
    let _ = handle_input(state, &special(DomainKey::CtrlX));
    assert_eq!(handle_input(state, &key('q')), InputAction::FocusDecisions);
    decision_queue::open_waiting(state, &facts("c1"));
}

/// Three waiting items, the oldest one deferred: (state, clock).
fn three() -> (TuiState, Arc<MockClock>) {
    let (mut state, clock) = clocked(80, 24);
    permission(&mut state, "First");
    paint(&mut state, &facts("c1"), 80, 24);
    ready(&mut state, &clock);
    permission(&mut state, "Second");
    permission(&mut state, "Third");
    // Defer the first (Esc): it stays queued and counted.
    let _ = route(&mut state, &special(DomainKey::Esc), &facts("c1"));
    (state, clock)
}

#[test]
fn ctrl_x_q_lists_and_never_reopens_a_deferred_item() {
    let (mut state, _clock) = three();
    assert_eq!(decision_queue::waiting(&state), 3);
    assert!(!presentation::is_open(&state, Surface::Decision));
    ctrl_x_q(&mut state);
    assert!(presentation::is_open(&state, Surface::DecisionQueue));
    let text = screen(&paint(&mut state, &facts("c1"), 80, 24));
    assert!(text.contains("Needs you"), "{text}");
    for tool in ["First", "Second", "Third"] {
        assert!(
            text.contains(&format!("Permission · {tool} · — ·")),
            "{text}"
        );
    }
    assert!(
        text.contains("Permission · First · — ·") && text.contains("deferred"),
        "{text}"
    );
    // The selection starts at the oldest item the user has NOT set aside.
    let selected = text
        .lines()
        .find(|row| row.contains("▸ "))
        .expect("a selected row");
    assert!(selected.contains("Second"), "{selected}");
    assert!(
        !presentation::is_open(&state, Surface::Decision),
        "opening the list never opens an item"
    );
    assert!(
        text.contains("[Enter] Open  [Esc] Back  [Up/Down] Select"),
        "{text}"
    );
}

#[test]
fn down_then_enter_opens_the_chosen_item_and_back_returns_to_the_callers_frame() {
    let (mut state, clock) = three();
    ctrl_x_q(&mut state);
    let _ = route(&mut state, &special(DomainKey::Down), &facts("c1"));
    let _ = route(&mut state, &special(DomainKey::Enter), &facts("c1"));
    assert!(
        !presentation::is_open(&state, Surface::DecisionQueue),
        "the list is replaced"
    );
    assert!(presentation::is_open(&state, Surface::Decision));
    let text = screen(&paint(&mut state, &facts("c1"), 80, 24));
    assert!(
        text.contains("Needs you › Permission Third | 3 pending"),
        "{text}"
    );
    ready(&mut state, &clock);
    let _ = route(&mut state, &special(DomainKey::Esc), &facts("c1"));
    assert!(
        state.modal_stack.is_empty(),
        "Back returns to the list's caller, not the list"
    );
    assert!(!presentation::is_open(&state, Surface::DecisionQueue));
    // Nothing was reselected automatically: all three still wait.
    assert_eq!(decision_queue::waiting(&state), 3);
}

#[test]
fn enter_reopens_the_same_deferred_fork_from_needs_you() {
    const PREVIEW: &str = "Fork queue target";
    let mut turn = Turn::new("fixture-model".into(), 0);
    prose(&mut turn, PREVIEW);
    let mut fixture = Fixture::new(80, 24, turn);
    assert_eq!(fixture.press(Key::Char('f')), InputAction::ForkAtMessage);

    // Run the host's Fork producer for the Stage's only (durable) message.
    let conversation = &fixture.h.conversation;
    assert_eq!(conversation.messages.len(), 1);
    assert_eq!(durable_target(conversation, 0).unwrap(), 0);
    let target = &conversation.messages[0];
    let mut f = facts(&conversation.id);
    f.active_messages = conversation.messages.len();
    decision_queue::raise_fork(
        &mut fixture.h.state,
        &conversation.id,
        0,
        &target.content,
        decision_queue::message_target_fingerprint(target),
    );

    let fork_title = "┌ Decision: Fork ";
    let preview = format!("Message 1: \"{PREVIEW}\"");
    fixture.paint();
    let original = screen(fixture.buffer());
    assert!(
        original.lines().any(|row| row.starts_with(fork_title)),
        "{original}"
    );
    assert!(original.contains(&preview), "{original}");
    let decision = fixture
        .h
        .state
        .modal_stack
        .top()
        .and_then(|frame| frame.decision.clone())
        .expect("the original Fork decision");

    ready(&mut fixture.h.state, &fixture.clock);
    route(&mut fixture.h.state, &special(DomainKey::Esc), &f);
    fixture.paint();
    let deferred = screen(fixture.buffer());
    let status = row_text(fixture.buffer(), fixture.layout().status_bar.y);
    assert!(status.contains("Needs you (1) ^X q"), "{status}");
    assert!(
        !deferred.contains("Decision:") && !deferred.contains("┌ Needs you "),
        "{deferred}"
    );
    assert!(!deferred.contains("Forked from"), "{deferred}");

    assert_eq!(fixture.press(Key::Ctrl('x')), InputAction::Consumed);
    assert_eq!(fixture.press(Key::Char('q')), InputAction::FocusDecisions);
    decision_queue::open_waiting(&mut fixture.h.state, &f);
    fixture.paint();
    let listed = screen(fixture.buffer());
    assert!(
        listed.lines().any(|row| row.starts_with("┌ Needs you ")),
        "{listed}"
    );
    assert!(
        listed
            .lines()
            .any(|row| row.starts_with("│▸ Fork · message 1") && row.contains(" · deferred")),
        "{listed}"
    );
    assert!(
        listed.contains("[Enter] Open  [Esc] Back  [Up/Down] Select"),
        "{listed}"
    );
    assert!(!presentation::is_open(&fixture.h.state, Surface::Decision));

    route(&mut fixture.h.state, &special(DomainKey::Enter), &f);
    fixture.paint();
    let reopened = screen(fixture.buffer());
    assert!(
        reopened.lines().any(|row| row.starts_with(fork_title)),
        "{reopened}"
    );
    assert!(reopened.contains(&preview), "{reopened}");
    assert!(!presentation::is_open(
        &fixture.h.state,
        Surface::DecisionQueue
    ));
    assert_eq!(
        fixture.h.state.modal_stack.top().unwrap().decision.as_ref(),
        Some(&decision),
        "Enter must reopen the original Fork target"
    );
}

#[test]
fn esc_closes_the_list_and_restores_what_it_covered() {
    let (mut state, _clock) = three();
    presentation::reducer::open(&mut state, Surface::Help);
    ctrl_x_q(&mut state);
    assert_eq!(
        state.modal_stack.top().unwrap().surface,
        Surface::DecisionQueue
    );
    let _ = route(&mut state, &special(DomainKey::Esc), &facts("c1"));
    assert_eq!(state.modal_stack.top().unwrap().surface, Surface::Help);
}

#[test]
fn an_empty_queue_says_so_and_opens_nothing() {
    let (mut state, _clock) = clocked(80, 24);
    let _ = handle_input(&mut state, &special(DomainKey::CtrlX));
    let _ = handle_input(&mut state, &key('q'));
    decision_queue::open_waiting(&mut state, &facts("c1"));
    assert!(!presentation::is_open(&state, Surface::DecisionQueue));
    let toast = state
        .notifications
        .records()
        .map(|record| record.message.clone())
        .collect::<Vec<_>>();
    assert!(
        toast
            .iter()
            .any(|message| message.contains("No decisions waiting")),
        "{toast:?}"
    );
}

#[test]
fn every_kind_lists_with_its_source_and_the_list_fits_sixty_columns() {
    let (mut state, _clock) = clocked(60, 16);
    decision_queue::raise(
        &mut state,
        DecisionDraft::new(DecisionKind::Budget, "Daily budget ($10.00) reached."),
        Arrival::Automatic,
    );
    permission(&mut state, "Bash");
    ctrl_x_q(&mut state);
    let text = screen(&paint(&mut state, &facts("c1"), 60, 16));
    assert!(text.contains("Daily budget"), "{text}");
    assert!(text.contains("Permission · Bash"), "{text}");
    for row in text.lines() {
        assert!(unicode_width::UnicodeWidthStr::width(row) <= 60, "{row}");
    }
}

#[test]
fn long_rows_keep_age_and_deferred_stale_and_other_tab_markers() {
    for width in [60, 80] {
        for status in ["deferred", "stale", "passive"] {
            let (mut state, clock) = clocked(width, 24);
            let mut f = facts("c1");
            f.titles = vec![
                (
                    "c1".into(),
                    "A very long active conversation title for testing".into(),
                ),
                (
                    "c2".into(),
                    "A very long other conversation title for testing".into(),
                ),
            ];
            // Record the active tab before raising the other-tab item.
            decision_queue::prune_closed(&mut state, &f);
            let conversation = if status == "passive" { "c2" } else { "c1" };
            decision_queue::raise(
                &mut state,
                DecisionDraft::new(DecisionKind::Budget, "budget detail")
                    .conversation(conversation)
                    .summary("A long summary that must not hide the essential state marker"),
                Arrival::Automatic,
            );
            if status != "passive" {
                paint(&mut state, &f, width, 24);
                ready(&mut state, &clock);
                if status == "deferred" {
                    let _ = route(&mut state, &special(DomainKey::Esc), &f);
                } else {
                    decision_queue::withdraw_for(&mut state, DecisionKind::Budget, "c1");
                }
            }
            clock.advance(Duration::from_secs(12));
            decision_queue::open_waiting(&mut state, &f);
            let text = screen(&paint(&mut state, &f, width, 24));
            let row = text
                .lines()
                .find(|row| row.contains("Daily budget ·") || row.contains("Daily budget…"))
                .expect(&text);
            assert!(row.contains(" · 12s"), "{status} {width}: {row}");
            match status {
                "deferred" => assert!(row.contains(" · deferred"), "{row}"),
                "stale" => assert!(row.contains(" · stale"), "{row}"),
                _ => assert!(
                    row.contains(" · Open ") && row.contains(" to decide"),
                    "{row}"
                ),
            }
            assert!(row.contains('…'), "{row}");
        }
    }
}
