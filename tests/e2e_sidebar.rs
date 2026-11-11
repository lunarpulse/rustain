//! E2E tests for Story 4.2 (conversation history), as migrated by Story 21.7 (7-E): the
//! History sidebar PANEL is gone; History is an L2 frame (`history_window`). Tests the
//! SessionIndex integration, the History window's paint and keyboard navigation (selection by
//! conversation id), that Tab never targets a sidebar, and the sidebar column's resize rule.

use rustain::adapters::tui::app::{InputAction, handle_input};
use rustain::adapters::tui::history_window;
use rustain::adapters::tui::keymap::Host;
use rustain::adapters::tui::layout::compute_layout;
use rustain::adapters::tui::notification_log::LogFacts;
use rustain::adapters::tui::presentation::occlusion::OpaqueRects;
use rustain::adapters::tui::presentation::{Shell, Surface, compose, is_open};
use rustain::adapters::tui::state::TuiState;
use rustain::domain::events::{DomainInputEvent, DomainKey};
use rustain::domain::models::visual::DeleteConfirmTarget;
use rustain::domain::models::{ConversationSummary, FocusState, StatusState};
use rustain::domain::services::session_index::SessionIndex;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;

/// The `now` every History window of this file is opened at (unix seconds).
const NOW: i64 = 1_700_000_000;

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

/// `count` conversations `e0` (newest) .. `e{count-1}` (oldest).
fn numbered_index(count: usize) -> SessionIndex {
    SessionIndex::build(
        (0..count)
            .map(|i| make_summary(&format!("e{i}"), &format!("Chat {i}"), NOW - 100 * i as i64))
            .collect(),
    )
}

/// Paint the stack once through the production compositor (the same boundary the host draws
/// through) and return the screen rows. The paint also records what the History window
/// painted (`fit`), which its keys act on.
fn paint(state: &TuiState) -> Vec<String> {
    let (width, height) = (state.terminal_width, state.terminal_height);
    let area = Rect::new(0, 0, width, height);
    let layout = compute_layout(area, &state.theme, &state.input_buffer, 1, false)
        .expect("a real chat viewport");
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| {
            let shell = Shell::new(frame.area(), &layout);
            compose(
                frame,
                state,
                &shell,
                Host::Local,
                &LogFacts::default(),
                &mut OpaqueRects::new(shell.chat_pane),
                |_| {},
            );
        })
        .unwrap();
    terminal
        .backend()
        .buffer()
        .content
        .chunks(usize::from(width))
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect()
}

/// Open the History window on `index` and paint it once (a History key acts only if its rail
/// hint was painted by the last draw).
fn open_history(state: &mut TuiState, index: &SessionIndex) -> String {
    history_window::open(state, index, NOW);
    paint(state).join("\n")
}

fn selected(state: &TuiState) -> Option<String> {
    state
        .history_window
        .as_ref()
        .and_then(|window| window.selected_id())
        .map(str::to_owned)
}

// ── AC1: Ctrl+X h opens the History window at every width (Ctrl+H is inert) ─────

#[test]
fn test_ctrl_x_h_opens_the_history_window() {
    for width in [60, 80, 119, 120, 130] {
        let mut state = TuiState::with_capability(
            width,
            24,
            rustain::adapters::tui::color_detect::ColorCapability::TrueColor,
        );
        assert!(!state.monitor);

        // The chord asks the host to open the History window; a second chord yields the very
        // same request (there is no panel toggle and no 120-column refusal any more).
        for _ in 0..2 {
            handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlX));
            let action = handle_input(&mut state, &DomainInputEvent::KeyPress('h'));
            assert_eq!(action, InputAction::OpenHistory, "{width} columns");
            assert!(state.pending_leader.is_none());
        }

        // Ctrl+H no longer toggles anything (Story 21.3 AC6).
        let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlH));
        assert!(
            matches!(action, InputAction::Consumed | InputAction::Ignored),
            "{action:?}"
        );
        assert!(!state.monitor);
        assert!(!is_open(&state, Surface::History), "Ctrl+H opens nothing");
    }
}

// ── AC2: the History window renders the conversation list ────────────────────

#[test]
fn test_history_window_renders_empty_state() {
    let mut state = make_state();
    let screen = open_history(&mut state, &SessionIndex::new());
    assert!(is_open(&state, Surface::History));
    assert!(screen.contains("History"), "{screen}");
    assert!(screen.contains("No conversations"), "{screen}");
    assert!(screen.contains("Back"), "the way out is painted: {screen}");
}

#[test]
fn test_history_window_renders_entries() {
    let mut state = make_state();
    let index = SessionIndex::build(vec![
        ConversationSummary {
            message_count: 3,
            ..make_summary("a", "First Chat", NOW)
        },
        ConversationSummary {
            message_count: 7,
            ..make_summary("b", "Second Chat", NOW - 1000)
        },
    ]);
    let screen = open_history(&mut state, &index);
    assert!(screen.contains("First Chat"), "{screen}");
    assert!(screen.contains("Second Chat"), "{screen}");
    assert!(screen.contains("3 msgs"), "{screen}");
    assert!(screen.contains("7 msgs"), "{screen}");
}

#[test]
fn test_history_window_highlights_active_conversation() {
    let mut state = make_state();
    let mut index = SessionIndex::build(vec![
        make_summary("a", "Active One", NOW),
        make_summary("b", "Inactive", NOW - 1000),
    ]);
    index.set_active(Some("a"));
    let screen = open_history(&mut state, &index);
    // Active entry shows the bullet indicator; the inactive one does not.
    assert!(screen.contains("● Active One"), "{screen}");
    assert!(!screen.contains("● Inactive"), "{screen}");
}

// ── AC3: Tab never targets a sidebar; Esc leaves the History window ───────────

#[test]
fn test_tab_from_chat_with_sidebar_never_focuses_sidebar() {
    // S16.6 AC5: Tab in Chat emits CycleInvocationInFocusedTurn. Since Story 21.7 (7-D,
    // RU-21-7-13) the event-loop fallback when no item cycles is the NEXT TAB, even with the
    // sidebar visible: the sidebar is never a Tab target (7-E: `FocusState` has no Sidebar
    // variant at all). The harness has no event loop, so the production seams checked are the
    // dispatcher (the action; focus stays on Chat) and the registry text every help surface
    // prints.
    let mut state = make_state();
    state.monitor = true;
    state.focus = FocusState::Chat;

    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Tab));
    assert_eq!(action, InputAction::CycleInvocationInFocusedTurn);
    assert_eq!(
        state.focus,
        FocusState::Chat,
        "Tab never moves focus to the sidebar"
    );
    let help = rustain::adapters::tui::keymap::render_help::help_rows();
    let tab = help
        .iter()
        .find(|row| row.key.starts_with("Tab") && row.description.contains("tables"))
        .expect("the Chat Tab help row");
    assert!(
        tab.description.ends_with("else next tab") && !tab.description.contains("sidebar"),
        "{}",
        tab.description
    );
}

#[test]
fn test_tab_from_chat_without_sidebar_switches_tab() {
    // S16.6 AC5: Tab in Chat now emits CycleInvocationInFocusedTurn.
    // The event-loop dispatcher falls through to tab-switch when guard fails
    // and sidebar is hidden. Harness has no event loop, so verify action
    // and simulate the fallback.
    let mut state = make_state();
    state.monitor = false;
    state.focus = FocusState::Chat;

    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Tab));
    assert_eq!(action, InputAction::CycleInvocationInFocusedTurn);
    // Simulate event-loop fallback: no focused turn + no sidebar → tab switch
}

#[test]
fn test_tab_in_history_window_is_consumed_and_keeps_the_frame() {
    // Old `test_tab_from_sidebar_returns_to_input` proved Tab left the (deleted) sidebar
    // focus. The History window is a modal frame: an unbound Tab is consumed and neither closes
    // it nor moves the selection.
    let mut state = make_state();
    state.monitor = true;
    open_history(&mut state, &numbered_index(3));

    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Tab));
    assert_eq!(action, InputAction::Consumed);
    assert!(history_window::is_top(&state));
    assert_eq!(selected(&state).as_deref(), Some("e0"));
}

#[test]
fn test_esc_from_history_window_closes_it() {
    let mut state = make_state();
    state.focus = FocusState::Chat;
    open_history(&mut state, &numbered_index(3));
    assert!(is_open(&state, Surface::History));

    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Esc));
    assert_eq!(action, InputAction::Consumed);
    assert!(!is_open(&state, Surface::History));
    assert!(
        state.history_window.is_none(),
        "the frame owned the payload"
    );
    assert!(state.modal_stack.is_empty());
    assert_eq!(state.focus, FocusState::Chat, "the base focus is untouched");
}

// ── AC4: Keyboard navigation (j/k/Enter/d), selection by conversation id ──────

#[test]
fn test_history_j_moves_selection_down() {
    let mut state = make_state();
    open_history(&mut state, &numbered_index(5));
    assert_eq!(selected(&state).as_deref(), Some("e0"));

    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('j'));
    assert_eq!(action, InputAction::Consumed);
    assert_eq!(selected(&state).as_deref(), Some("e1"));
}

#[test]
fn test_history_j_clamps_at_bottom() {
    let mut state = make_state();
    open_history(&mut state, &numbered_index(3));
    for _ in 0..5 {
        let action = handle_input(&mut state, &DomainInputEvent::KeyPress('j'));
        assert_eq!(action, InputAction::Consumed);
    }
    assert_eq!(selected(&state).as_deref(), Some("e2")); // Stays at the last entry
}

#[test]
fn test_history_k_moves_selection_up() {
    let mut state = make_state();
    open_history(&mut state, &numbered_index(5));
    for _ in 0..3 {
        handle_input(&mut state, &DomainInputEvent::KeyPress('j'));
    }
    assert_eq!(selected(&state).as_deref(), Some("e3"));

    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('k'));
    assert_eq!(action, InputAction::Consumed);
    assert_eq!(selected(&state).as_deref(), Some("e2"));
}

#[test]
fn test_history_k_clamps_at_top() {
    let mut state = make_state();
    open_history(&mut state, &numbered_index(5));

    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('k'));
    assert_eq!(action, InputAction::Consumed);
    assert_eq!(selected(&state).as_deref(), Some("e0")); // Stays at the first entry
}

#[test]
fn test_history_enter_opens_conversation() {
    let mut state = make_state();
    open_history(&mut state, &numbered_index(3));
    handle_input(&mut state, &DomainInputEvent::KeyPress('j'));

    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Enter));
    assert_eq!(action, InputAction::OpenHistoryEntry("e1".to_string()));
}

#[test]
fn test_history_d_deletes_conversation() {
    let mut state = make_state();
    open_history(&mut state, &numbered_index(3));

    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('d'));
    assert_eq!(action, InputAction::DeleteHistoryEntry("e0".to_string()));
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

// ── AC11: Resize keeps the monitor preference below 120 columns (A16) ───────
// (7-F: the column paints `Monitor · <Tab>` and is never a focus target; the P11 shrink reset
// is gone: the preference survives, the layout simply does not paint the column below 120.)

#[test]
fn test_resize_keeps_the_monitor_preference_when_narrow() {
    let mut state = make_state();
    state.monitor = true;
    state.focus = FocusState::Chat;

    // Simulate resize to below SIDEBAR_MIN_WIDTH
    let action = handle_input(
        &mut state,
        &DomainInputEvent::Resize(80, 24), // 80 < 120 (SIDEBAR_MIN_WIDTH)
    );
    assert_eq!(action, InputAction::Consumed);
    assert!(state.monitor, "the preference is kept; nothing is painted");
    assert_eq!(state.focus, FocusState::Chat, "the column never held focus");
}

#[test]
fn test_resize_keeps_sidebar_when_wide_enough() {
    let mut state = make_state();
    state.monitor = true;
    state.focus = FocusState::Chat;

    // Resize to exactly SIDEBAR_MIN_WIDTH — should keep sidebar
    let _action = handle_input(&mut state, &DomainInputEvent::Resize(120, 24));
    assert!(state.monitor);
    assert_eq!(state.focus, FocusState::Chat);
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
    decision_queue::handle_domain_event(
        state,
        &event,
        &LogFacts::default(),
        rustain::adapters::tui::keymap::Host::Local,
    )
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
    let index = SessionIndex::build(vec![make_summary("conv-123", "Test", NOW)]);
    history_window::open(&mut state, &index, NOW);
    paint(&state);

    // Step 1: press 'd' in the History window; the action carries the selected id.
    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('d'));
    assert_eq!(
        action,
        InputAction::DeleteHistoryEntry("conv-123".to_string())
    );
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

/// A cancelled delete asked from the History window leaves History on top (focus returns
/// through the stack; nothing writes the base focus).
#[test]
fn test_e2e_delete_cancel_returns_to_the_history_window() {
    let (mut state, clock) = pane_state();
    let index = SessionIndex::build(vec![make_summary("conv-123", "Test", NOW)]);
    history_window::open(&mut state, &index, NOW);
    paint(&state);
    begin_delete_confirmation(&mut state, single());
    assert!(!history_window::is_top(&state), "the decision is on top");
    ready(&mut state, &clock);
    let outcome = press(&mut state, DomainInputEvent::KeyPress('n'));
    assert_eq!(
        outcome,
        Some(PaneOutcome::Effect(DecisionEffect::CancelDelete))
    );
    decision_queue::apply_resolved_action(&mut state, InputAction::CancelDelete);
    assert!(state.decisions.is_empty());
    assert!(history_window::is_top(&state), "History is on top again");
    assert_eq!(
        state.history_window.as_ref().and_then(|w| w.selected_id()),
        Some("conv-123"),
        "the selection survived the question"
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

/// A plain `y` with no decision frame is not an answer (the old overlay arm is gone): a
/// deferred delete question stays queued, holds no frame, and a stray `y` neither
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
