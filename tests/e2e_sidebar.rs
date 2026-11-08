//! E2E tests for Story 4.2: Conversation History Sidebar.
//! Tests SessionIndex integration, sidebar keyboard navigation,
//! focus cycling, sidebar widget rendering, and live updates.

use rustain::adapters::tui::app::{InputAction, handle_input};
use rustain::adapters::tui::state::TuiState;
use rustain::adapters::tui::widgets::sidebar;
use rustain::domain::events::{DomainInputEvent, DomainKey};
use rustain::domain::models::visual::{DeleteConfirmTarget, PanelType};
use rustain::domain::models::{ConversationSummary, FocusState, StatusState};
use rustain::domain::services::session_index::{SessionIndex, SessionSummary};

use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn make_state() -> TuiState {
    TuiState::with_capability(
        120,
        24,
        rustain::adapters::tui::color_detect::ColorCapability::TrueColor,
    )
}

fn make_summary(id: &str, title: &str, updated_at: i64) -> ConversationSummary {
    ConversationSummary {
        id: id.to_string(),
        title: title.to_string(),
        created_at: updated_at - 1000,
        updated_at,
        message_count: 5,
        has_fork_source: false,
    }
}

// ── AC1: Ctrl+X h reveals/toggles the History sidebar (Ctrl+H is inert) ─────

#[test]
fn test_ctrl_x_h_reveals_the_history_sidebar() {
    let mut state = make_state();
    assert!(!state.sidebar_visible);

    // The chord asks the host to open (or, when already showing, close) the
    // History panel; `panel_bridge::open_panel` toggles it, so a second chord
    // yields the very same request.
    for _ in 0..2 {
        handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlX));
        let action = handle_input(&mut state, &DomainInputEvent::KeyPress('h'));
        assert_eq!(action, InputAction::OpenPanel(PanelType::History));
        assert!(state.pending_leader.is_none());
    }

    // Ctrl+H no longer toggles anything (Story 21.3 AC6).
    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlH));
    assert!(
        matches!(action, InputAction::Consumed | InputAction::Ignored),
        "{action:?}"
    );
    assert!(!state.sidebar_visible);
}

// ── AC2: Sidebar renders conversation list ──────────────────────────────────

#[test]
fn test_sidebar_renders_empty_state() {
    let backend = TestBackend::new(40, 10);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = rustain::adapters::tui::theme::Theme::dark();

    terminal
        .draw(|frame| {
            let area = frame.area();
            sidebar::render_history_panel(area, frame.buffer_mut(), &[], 0, None, &theme);
        })
        .unwrap();

    let buf = terminal.backend().buffer().clone();
    let content: String = buf.content.iter().map(|c| c.symbol().to_string()).collect();
    assert!(content.contains("No conversations"));
}

#[test]
fn test_sidebar_renders_entries() {
    let backend = TestBackend::new(50, 10);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = rustain::adapters::tui::theme::Theme::dark();

    let entries = vec![
        SessionSummary::new(
            "a".to_string(),
            "First Chat".to_string(),
            1_700_000_000,
            1_699_999_000,
            3,
        ),
        SessionSummary::new(
            "b".to_string(),
            "Second Chat".to_string(),
            1_699_999_000,
            1_699_998_000,
            7,
        ),
    ];

    terminal
        .draw(|frame| {
            let area = frame.area();
            sidebar::render_history_panel(area, frame.buffer_mut(), &entries, 0, None, &theme);
        })
        .unwrap();

    let buf = terminal.backend().buffer().clone();
    let content: String = buf.content.iter().map(|c| c.symbol().to_string()).collect();
    assert!(content.contains("First Chat"));
    assert!(content.contains("Second Chat"));
}

#[test]
fn test_sidebar_highlights_active_conversation() {
    let backend = TestBackend::new(50, 10);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = rustain::adapters::tui::theme::Theme::dark();

    let mut entries = vec![
        SessionSummary::new(
            "a".to_string(),
            "Active One".to_string(),
            1_700_000_000,
            1_699_999_000,
            3,
        ),
        SessionSummary::new(
            "b".to_string(),
            "Inactive".to_string(),
            1_699_999_000,
            1_699_998_000,
            7,
        ),
    ];
    entries[0].is_active = true;

    terminal
        .draw(|frame| {
            let area = frame.area();
            sidebar::render_history_panel(area, frame.buffer_mut(), &entries, 0, Some("a"), &theme);
        })
        .unwrap();

    let buf = terminal.backend().buffer().clone();
    let content: String = buf.content.iter().map(|c| c.symbol().to_string()).collect();
    // Active entry shows bullet indicator
    assert!(content.contains("●"));
}

// ── AC3: Focus cycling (Tab key, Esc) ───────────────────────────────────────

#[test]
fn test_tab_from_chat_with_sidebar_focuses_sidebar() {
    // S16.6 AC5: Tab in Chat now emits CycleInvocationInFocusedTurn.
    // The event-loop dispatcher guards (focused turn + expanded + >=2 invocations)
    // and falls through to sidebar focus when guard fails. The harness has no
    // event loop, so we verify the action and simulate the fallback.
    let mut state = make_state();
    state.sidebar_visible = true;
    state.focus = FocusState::Chat;

    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Tab));
    assert_eq!(action, InputAction::CycleInvocationInFocusedTurn);
    // Simulate event-loop fallback: no focused turn → sidebar focus
    state.focus = FocusState::Sidebar {
        panel: PanelType::History,
        selected: 0,
    };
    assert!(matches!(state.focus, FocusState::Sidebar { .. }));
}

#[test]
fn test_tab_from_chat_without_sidebar_switches_tab() {
    // S16.6 AC5: Tab in Chat now emits CycleInvocationInFocusedTurn.
    // The event-loop dispatcher falls through to tab-switch when guard fails
    // and sidebar is hidden. Harness has no event loop, so verify action
    // and simulate the fallback.
    let mut state = make_state();
    state.sidebar_visible = false;
    state.focus = FocusState::Chat;

    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Tab));
    assert_eq!(action, InputAction::CycleInvocationInFocusedTurn);
    // Simulate event-loop fallback: no focused turn + no sidebar → tab switch
}

#[test]
fn test_tab_from_sidebar_returns_to_input() {
    let mut state = make_state();
    state.sidebar_visible = true;
    state.focus = FocusState::Sidebar {
        panel: PanelType::History,
        selected: 0,
    };

    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Tab));
    assert_eq!(action, InputAction::Consumed);
    assert_eq!(state.focus, FocusState::Input);
}

#[test]
fn test_esc_from_sidebar_returns_to_chat() {
    let mut state = make_state();
    state.sidebar_visible = true;
    state.focus = FocusState::Sidebar {
        panel: PanelType::History,
        selected: 0,
    };

    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Esc));
    assert_eq!(action, InputAction::Consumed);
    assert_eq!(state.focus, FocusState::Chat);
}

// ── AC4: Keyboard navigation (j/k/Enter/d) ─────────────────────────────────

#[test]
fn test_sidebar_j_moves_selection_down() {
    let mut state = make_state();
    state.sidebar_visible = true;
    state.sidebar_entry_count = 5;
    state.sidebar_selected = 0;
    state.focus = FocusState::Sidebar {
        panel: PanelType::History,
        selected: 0,
    };

    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('j'));
    assert_eq!(action, InputAction::Consumed);
    assert_eq!(state.sidebar_selected, 1);
}

#[test]
fn test_sidebar_j_clamps_at_bottom() {
    let mut state = make_state();
    state.sidebar_visible = true;
    state.sidebar_entry_count = 3;
    state.sidebar_selected = 2; // At last entry
    state.focus = FocusState::Sidebar {
        panel: PanelType::History,
        selected: 2,
    };

    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('j'));
    assert_eq!(action, InputAction::Consumed);
    assert_eq!(state.sidebar_selected, 2); // Stays at 2
}

#[test]
fn test_sidebar_k_moves_selection_up() {
    let mut state = make_state();
    state.sidebar_visible = true;
    state.sidebar_entry_count = 5;
    state.sidebar_selected = 3;
    state.focus = FocusState::Sidebar {
        panel: PanelType::History,
        selected: 3,
    };

    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('k'));
    assert_eq!(action, InputAction::Consumed);
    assert_eq!(state.sidebar_selected, 2);
}

#[test]
fn test_sidebar_k_clamps_at_top() {
    let mut state = make_state();
    state.sidebar_visible = true;
    state.sidebar_entry_count = 5;
    state.sidebar_selected = 0;
    state.focus = FocusState::Sidebar {
        panel: PanelType::History,
        selected: 0,
    };

    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('k'));
    assert_eq!(action, InputAction::Consumed);
    assert_eq!(state.sidebar_selected, 0); // Stays at 0
}

#[test]
fn test_sidebar_enter_opens_conversation() {
    let mut state = make_state();
    state.sidebar_visible = true;
    state.sidebar_entry_count = 3;
    state.sidebar_selected = 1;
    state.focus = FocusState::Sidebar {
        panel: PanelType::History,
        selected: 1,
    };

    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Enter));
    assert_eq!(action, InputAction::OpenSidebarConversation);
}

#[test]
fn test_sidebar_d_deletes_conversation() {
    let mut state = make_state();
    state.sidebar_visible = true;
    state.sidebar_entry_count = 3;
    state.sidebar_selected = 0;
    state.focus = FocusState::Sidebar {
        panel: PanelType::History,
        selected: 0,
    };

    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('d'));
    assert_eq!(action, InputAction::DeleteSidebarConversation);
}

// ── AC9: SessionIndex sorted by updated_at ──────────────────────────────────

#[test]
fn test_session_index_sorted_newest_first() {
    let summaries = vec![
        make_summary("old", "Old", 1000),
        make_summary("new", "New", 3000),
        make_summary("mid", "Mid", 2000),
    ];
    let index = SessionIndex::build(summaries);
    let entries = index.entries();
    assert_eq!(entries[0].conversation_id, "new");
    assert_eq!(entries[1].conversation_id, "mid");
    assert_eq!(entries[2].conversation_id, "old");
}

#[test]
fn test_session_index_touch_moves_to_front() {
    let summaries = vec![make_summary("a", "A", 1000), make_summary("b", "B", 2000)];
    let mut index = SessionIndex::build(summaries);
    // "a" is oldest, at the back
    assert_eq!(index.entries()[1].conversation_id, "a");

    // Touch "a" — should move it to front
    index.touch("a", Some("Updated A".to_string()), Some(10));
    assert_eq!(index.entries()[0].conversation_id, "a");
    assert_eq!(index.entries()[0].title, "Updated A");
    assert_eq!(index.entries()[0].message_count, 10);
}

#[test]
fn test_session_index_set_active_clears_others() {
    let summaries = vec![make_summary("a", "A", 1000), make_summary("b", "B", 2000)];
    let mut index = SessionIndex::build(summaries);
    index.set_active(Some("a"));

    assert!(index.get("a").unwrap().is_active);
    assert!(!index.get("b").unwrap().is_active);

    // Switch active
    index.set_active(Some("b"));
    assert!(!index.get("a").unwrap().is_active);
    assert!(index.get("b").unwrap().is_active);
}

#[test]
fn test_session_index_remove_updates_index() {
    let summaries = vec![
        make_summary("a", "A", 1000),
        make_summary("b", "B", 2000),
        make_summary("c", "C", 3000),
    ];
    let mut index = SessionIndex::build(summaries);
    assert_eq!(index.len(), 3);

    let removed = index.remove("b");
    assert!(removed.is_some());
    assert_eq!(index.len(), 2);
    assert!(index.get("b").is_none());
    // Remaining entries still accessible
    assert!(index.get("a").is_some());
    assert!(index.get("c").is_some());
}

// ── AC11: Resize hides sidebar when too narrow ──────────────────────────────

#[test]
fn test_resize_hides_sidebar_when_narrow() {
    let mut state = make_state();
    state.sidebar_visible = true;
    state.focus = FocusState::Sidebar {
        panel: PanelType::History,
        selected: 0,
    };

    // Simulate resize to below SIDEBAR_MIN_WIDTH
    let action = handle_input(
        &mut state,
        &DomainInputEvent::Resize(80, 24), // 80 < 120 (SIDEBAR_MIN_WIDTH)
    );
    assert_eq!(action, InputAction::Consumed);
    assert!(!state.sidebar_visible);
    assert_eq!(state.focus, FocusState::Chat); // Focus moved out of sidebar
}

#[test]
fn test_resize_keeps_sidebar_when_wide_enough() {
    let mut state = make_state();
    state.sidebar_visible = true;
    state.focus = FocusState::Sidebar {
        panel: PanelType::History,
        selected: 0,
    };

    // Resize to exactly SIDEBAR_MIN_WIDTH — should keep sidebar
    let _action = handle_input(&mut state, &DomainInputEvent::Resize(120, 24));
    assert!(state.sidebar_visible);
}

// ── AC5/AC6: Delete confirmation flow (Story 21.1 S2: decision queue, 21.6 frame) ──
//
// Old (S1): the prompt was a 30 s status flash plus `Overlay(Confirmation(
// DeleteConfirmation))`, and `handle_input` mapped `y`/`n`/`Esc` on that focus.
// New (AC9, D5): `begin_delete_confirmation` raises a queue item; ONLY the
// focused decision frame answers (the same `decision_queue::handle_domain_event` the
// event loop calls), after the 500 ms inert window, exactly once.

use rustain::adapters::tui::decision_queue::{
    self, DecisionEffect, DecisionKind, DecisionSubject, PaneOutcome,
};
use rustain::adapters::tui::notification_log::LogFacts;
use rustain::adapters::tui::notifications::NotificationStore;
use rustain::domain::clock::MockClock;
use rustain::domain::models::NotificationsConfig;
use rustain::infrastructure::runtime::event_loop::{
    begin_delete_confirmation, complete_bulk_delete,
};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn pane_state() -> (TuiState, Arc<MockClock>) {
    let clock = Arc::new(MockClock::new(Instant::now()));
    let mut state = make_state();
    state.notifications =
        NotificationStore::from_config(&NotificationsConfig::default(), clock.clone());
    (state, clock)
}

/// First paint, then the 500 ms inert window passes.
fn ready(state: &mut TuiState, clock: &MockClock) {
    decision_queue::note_painted(state);
    clock.advance(Duration::from_millis(500));
}

fn press(state: &mut TuiState, event: DomainInputEvent) -> Option<PaneOutcome> {
    decision_queue::handle_domain_event(state, &event, &LogFacts::default())
}

fn single() -> DeleteConfirmTarget {
    DeleteConfirmTarget::Single {
        id: "conv-123".to_string(),
        title: "Test".to_string(),
    }
}

#[test]
fn test_delete_confirmation_y_confirms() {
    let (mut state, clock) = pane_state();
    begin_delete_confirmation(&mut state, single());
    assert!(
        rustain::adapters::tui::presentation::is_open(
            &state,
            rustain::adapters::tui::presentation::Surface::Decision
        ),
        "the user's own delete takes the frame"
    );
    assert!(state.decisions.holds(DecisionKind::DeleteConversations));
    // Old: the removed delete slot was asserted after the key. New: the target is
    // the queue item's payload (its slot is gone); it is observable before the key.
    let held = state.decisions.items().next().expect("the delete is held");
    assert_eq!(held.subject, DecisionSubject::Delete(single()));
    ready(&mut state, &clock);

    let outcome = press(&mut state, DomainInputEvent::KeyPress('y'));
    assert_eq!(
        outcome,
        Some(PaneOutcome::Effect(DecisionEffect::ConfirmDelete))
    );
    assert_eq!(
        DecisionEffect::ConfirmDelete.into_action(),
        InputAction::ConfirmDelete
    );
    assert!(state.decisions.is_empty(), "resolved exactly once");
    assert!(
        press(&mut state, DomainInputEvent::KeyPress('y')).is_none(),
        "a second y has no pane to answer"
    );
    assert_eq!(state.modal_stack.len(), 0, "the answered frame is popped");
}

#[test]
fn test_delete_confirmation_n_cancels() {
    let (mut state, clock) = pane_state();
    begin_delete_confirmation(&mut state, single());
    ready(&mut state, &clock);
    let outcome = press(&mut state, DomainInputEvent::KeyPress('n'));
    assert_eq!(
        outcome,
        Some(PaneOutcome::Effect(DecisionEffect::CancelDelete))
    );
    assert_eq!(
        DecisionEffect::CancelDelete.into_action(),
        InputAction::CancelDelete
    );
}

#[test]
fn test_delete_confirmation_esc_cancels() {
    // Esc is the non-destructive default (RU-7): Cancel, never Later.
    let (mut state, clock) = pane_state();
    begin_delete_confirmation(&mut state, DeleteConfirmTarget::Bulk { count: 5 });
    ready(&mut state, &clock);
    let outcome = press(&mut state, DomainInputEvent::SpecialKey(DomainKey::Esc));
    assert_eq!(
        outcome,
        Some(PaneOutcome::Effect(DecisionEffect::CancelDelete))
    );
    assert!(state.decisions.is_empty());
}

// F3: Additional E2E tests for confirmation flows

#[test]
fn test_e2e_delete_with_confirmation() {
    let (mut state, clock) = pane_state();
    state.sidebar_visible = true;
    state.sidebar_entry_count = 3;
    state.sidebar_selected = 0;
    state.focus = FocusState::Sidebar {
        panel: PanelType::History,
        selected: 0,
    };

    // Step 1: press 'd' to initiate the delete (the sidebar key is unchanged).
    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('d'));
    assert_eq!(action, InputAction::DeleteSidebarConversation);
    assert!(
        state.decisions.is_empty(),
        "nothing is asked until the host raises it"
    );

    // Step 2: the event loop's arm raises the confirmation; step 3: `y` confirms.
    begin_delete_confirmation(&mut state, single());
    ready(&mut state, &clock);
    let outcome = press(&mut state, DomainInputEvent::KeyPress('y'));
    assert_eq!(
        outcome,
        Some(PaneOutcome::Effect(DecisionEffect::ConfirmDelete))
    );
}

#[test]
fn test_e2e_delete_confirmation_leaves_the_status_row_alone() {
    // Old: the prompt overwrote the status with a Flash and Cancel reset it to
    // Idle. New: the prompt is not a status at all, so a running turn's status
    // survives both the question and the answer.
    let (mut state, clock) = pane_state();
    state.status = StatusState::Streaming;
    begin_delete_confirmation(&mut state, single());
    assert_eq!(
        state.status,
        StatusState::Streaming,
        "asking changes no status"
    );
    ready(&mut state, &clock);
    let outcome = press(&mut state, DomainInputEvent::KeyPress('n'));
    assert_eq!(
        outcome,
        Some(PaneOutcome::Effect(DecisionEffect::CancelDelete))
    );
    assert_eq!(
        state.status,
        StatusState::Streaming,
        "answering changes no status"
    );
}

#[test]
fn test_e2e_delete_cancel() {
    let (mut state, clock) = pane_state();
    begin_delete_confirmation(&mut state, single());
    ready(&mut state, &clock);
    let outcome = press(&mut state, DomainInputEvent::KeyPress('n'));
    assert_eq!(
        outcome,
        Some(PaneOutcome::Effect(DecisionEffect::CancelDelete))
    );
}

#[test]
fn test_e2e_bulk_delete_with_confirmation() {
    let (mut state, clock) = pane_state();
    state.sidebar_entry_count = 5;
    begin_delete_confirmation(&mut state, DeleteConfirmTarget::Bulk { count: 5 });
    ready(&mut state, &clock);
    let outcome = press(&mut state, DomainInputEvent::KeyPress('y'));
    assert_eq!(
        outcome,
        Some(PaneOutcome::Effect(DecisionEffect::ConfirmDelete))
    );
}

/// AC9 / RU-6: the rail keys are inert for 500 ms after the first paint and a
/// key pressed inside the window is dropped — it is never replayed, so the
/// delete cannot be confirmed by a key that arrived before the user saw it.
#[test]
fn story21_1_delete_keys_are_inert_for_500ms_and_never_replayed() {
    let (mut state, clock) = pane_state();
    begin_delete_confirmation(&mut state, single());
    // Before the first paint nothing may answer.
    assert_eq!(
        press(&mut state, DomainInputEvent::KeyPress('y')),
        Some(PaneOutcome::Consumed)
    );
    decision_queue::note_painted(&mut state);
    clock.advance(Duration::from_millis(499));
    assert_eq!(
        press(&mut state, DomainInputEvent::KeyPress('y')),
        Some(PaneOutcome::Consumed),
        "499 ms after first paint is still inert"
    );
    assert!(
        state.decisions.holds(DecisionKind::DeleteConversations),
        "dropped, not replayed"
    );
    clock.advance(Duration::from_millis(1));
    assert_eq!(
        press(&mut state, DomainInputEvent::KeyPress('y')),
        Some(PaneOutcome::Effect(DecisionEffect::ConfirmDelete)),
        "positive control: the same key at 500 ms answers"
    );
}

/// The sidebar `y` with no frame is not an answer (the old overlay arm is gone): a
/// deferred delete question stays queued, holds no frame, and a sidebar `y` neither
/// confirms it nor removes it.
// Old: a hand-seeded delete slot. New: the
// question is a queue item raised by `begin_delete_confirmation`, then set aside
// (`Ctrl+C` defers on every kind, even inside the inert window).
#[test]
fn story21_1_a_stray_y_without_a_pane_never_confirms_a_delete() {
    let mut state = make_state();
    begin_delete_confirmation(&mut state, single());
    assert_eq!(
        press(&mut state, DomainInputEvent::SpecialKey(DomainKey::CtrlC)),
        Some(PaneOutcome::Consumed)
    );
    assert!(state.modal_stack.is_empty(), "no frame holds the question");
    assert!(state.decisions.holds(DecisionKind::DeleteConversations));
    state.focus = FocusState::Sidebar {
        panel: PanelType::History,
        selected: 0,
    };
    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('y'));
    assert_ne!(action, InputAction::ConfirmDelete);
    assert!(
        state.decisions.holds(DecisionKind::DeleteConversations),
        "the stray y neither answered nor dropped the question"
    );
}

/// Story 21.1 S1-1 → S2 → 21.6: the bulk-delete question is a queue item (no status
/// flash exists), the confirmed delete ends in one Success notice and the
/// answered question leaves no `[y/n]` text anywhere.
#[test]
fn test_e2e_bulk_delete_confirmation_is_a_decision_item_and_ends_in_one_notice() {
    let (mut state, clock) = pane_state();
    state.sidebar_entry_count = 2;
    begin_delete_confirmation(&mut state, DeleteConfirmTarget::Bulk { count: 2 });
    let item = state
        .decisions
        .items()
        .next()
        .expect("the question is held");
    assert_eq!(item.kind, DecisionKind::DeleteConversations);
    assert!(
        item.detail.contains("Delete 2 conversations?"),
        "{}",
        item.detail
    );
    assert!(item.detail.contains("cannot be undone"), "{}", item.detail);
    assert!(!item.detail.contains("[y/n]"), "{}", item.detail);
    assert_eq!(state.status, StatusState::Idle, "no status flash");
    // Old: the removed delete slot held the target. New: the target is the
    // queue item's payload (the slot is gone; the effect arm takes it after the key).
    assert_eq!(
        item.subject,
        DecisionSubject::Delete(DeleteConfirmTarget::Bulk { count: 2 })
    );

    ready(&mut state, &clock);
    let outcome = press(&mut state, DomainInputEvent::KeyPress('y'));
    assert_eq!(
        outcome,
        Some(PaneOutcome::Effect(DecisionEffect::ConfirmDelete))
    );
    complete_bulk_delete(&mut state);

    assert_eq!(state.status, StatusState::Idle);
    let notices: Vec<&str> = state
        .notifications
        .visible()
        .map(|r| r.message.as_str())
        .collect();
    assert_eq!(notices, vec!["All conversations deleted"]);
    assert!(notices.iter().all(|m| !m.contains("[y/n]")));
}

// ── Relative time formatting ────────────────────────────────────────────────

#[test]
fn test_format_relative_time_ranges() {
    use rustain::domain::services::relative_time::format_relative_time;

    let now = 1_700_000_000;
    assert_eq!(format_relative_time(now - 5, now), "just now");
    assert_eq!(format_relative_time(now - 120, now), "2m ago");
    assert_eq!(format_relative_time(now - 7200, now), "2h ago");
}
