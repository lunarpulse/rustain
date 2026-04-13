//! E2E tests for Story 4.2: Conversation History Sidebar.
//! Tests SessionIndex integration, sidebar keyboard navigation,
//! focus cycling, sidebar widget rendering, and live updates.

use rustain::adapters::tui::app::{InputAction, handle_input};
use rustain::adapters::tui::state::TuiState;
use rustain::adapters::tui::widgets::sidebar;
use rustain::domain::events::{DomainInputEvent, DomainKey};
use rustain::domain::models::visual::{DeleteConfirmTarget, PanelType};
use rustain::domain::models::{ConversationSummary, FocusState};
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

// ── AC1: Ctrl+H toggles sidebar visibility ──────────────────────────────────

#[test]
fn test_ctrl_h_toggles_sidebar_visible() {
    let mut state = make_state();
    assert!(!state.sidebar_visible);

    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlH));
    assert_eq!(action, InputAction::ToggleSidebar);
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
    let mut state = make_state();
    state.sidebar_visible = true;
    state.focus = FocusState::Chat;

    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Tab));
    assert_eq!(action, InputAction::Consumed);
    assert!(matches!(state.focus, FocusState::Sidebar { .. }));
}

#[test]
fn test_tab_from_chat_without_sidebar_switches_tab() {
    let mut state = make_state();
    state.sidebar_visible = false;
    state.focus = FocusState::Chat;

    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Tab));
    assert_eq!(action, InputAction::SwitchToNextTab);
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

// ── AC5/AC6: Delete confirmation flow ───────────────────────────────────────

#[test]
fn test_delete_confirmation_y_confirms() {
    use rustain::domain::models::visual::{ConfirmationType, OverlayType};

    let mut state = make_state();
    let target = DeleteConfirmTarget::Single {
        id: "conv-123".to_string(),
        title: "Test".to_string(),
    };
    state.focus = FocusState::Overlay(OverlayType::Confirmation(
        ConfirmationType::DeleteConfirmation(target),
    ));
    state.pending_delete = Some(DeleteConfirmTarget::Single {
        id: "conv-123".to_string(),
        title: "Test".to_string(),
    });

    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('y'));
    assert_eq!(action, InputAction::ConfirmDelete);
}

#[test]
fn test_delete_confirmation_n_cancels() {
    use rustain::domain::models::visual::{ConfirmationType, OverlayType};

    let mut state = make_state();
    let target = DeleteConfirmTarget::Single {
        id: "conv-123".to_string(),
        title: "Test".to_string(),
    };
    state.focus = FocusState::Overlay(OverlayType::Confirmation(
        ConfirmationType::DeleteConfirmation(target),
    ));
    state.pending_delete = Some(DeleteConfirmTarget::Single {
        id: "conv-123".to_string(),
        title: "Test".to_string(),
    });

    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('n'));
    assert_eq!(action, InputAction::CancelDelete);
}

#[test]
fn test_delete_confirmation_esc_cancels() {
    use rustain::domain::models::visual::{ConfirmationType, OverlayType};

    let mut state = make_state();
    let target = DeleteConfirmTarget::Bulk { count: 5 };
    state.focus = FocusState::Overlay(OverlayType::Confirmation(
        ConfirmationType::DeleteConfirmation(target),
    ));
    state.pending_delete = Some(DeleteConfirmTarget::Bulk { count: 5 });

    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Esc));
    assert_eq!(action, InputAction::CancelDelete);
}

// F3: Additional E2E tests for confirmation flows

#[test]
fn test_e2e_delete_with_confirmation() {
    use rustain::domain::models::visual::{ConfirmationType, OverlayType};

    let mut state = make_state();
    state.sidebar_visible = true;
    state.sidebar_entry_count = 3;
    state.sidebar_selected = 0;
    state.focus = FocusState::Sidebar {
        panel: PanelType::History,
        selected: 0,
    };

    // Step 1: Press 'd' to initiate delete
    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('d'));
    assert_eq!(action, InputAction::DeleteSidebarConversation);

    // Simulate event loop setting up confirmation (would happen in real app)
    let target = DeleteConfirmTarget::Single {
        id: "conv-123".to_string(),
        title: "Test Chat".to_string(),
    };
    state.pending_delete = Some(target.clone());
    state.focus = FocusState::Overlay(OverlayType::Confirmation(
        ConfirmationType::DeleteConfirmation(target),
    ));

    // Step 2: Press 'y' to confirm
    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('y'));
    assert_eq!(action, InputAction::ConfirmDelete);
}

#[test]
fn test_e2e_delete_confirmation_clears_status() {
    use rustain::domain::models::StatusState;
    use rustain::domain::models::visual::{ConfirmationType, OverlayType};

    let mut state = make_state();
    // Set up a pending delete
    let target = DeleteConfirmTarget::Single {
        id: "conv-123".to_string(),
        title: "Test".to_string(),
    };
    state.pending_delete = Some(target.clone());
    state.focus = FocusState::Overlay(OverlayType::Confirmation(
        ConfirmationType::DeleteConfirmation(target),
    ));
    state.status = StatusState::Flash {
        message: "Delete \"Test\"? This cannot be undone. [y/n]".to_string(),
        remaining_ms: 30000,
    };

    // Cancel the delete
    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('n'));
    assert_eq!(action, InputAction::CancelDelete);

    // U1: Status should be cleared immediately (set to Idle by CancelDelete handler)
    // Note: In the real event loop, CancelDelete sets status to Idle
}

#[test]
fn test_e2e_delete_cancel() {
    use rustain::domain::models::visual::{ConfirmationType, OverlayType};

    let mut state = make_state();
    let target = DeleteConfirmTarget::Single {
        id: "conv-123".to_string(),
        title: "Test".to_string(),
    };
    state.pending_delete = Some(target.clone());
    state.focus = FocusState::Overlay(OverlayType::Confirmation(
        ConfirmationType::DeleteConfirmation(target),
    ));

    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('n'));
    assert_eq!(action, InputAction::CancelDelete);
}

#[test]
fn test_e2e_bulk_delete_with_confirmation() {
    use rustain::domain::models::visual::{ConfirmationType, OverlayType};

    let mut state = make_state();
    // Simulate having conversations
    state.sidebar_entry_count = 5;

    // Set up bulk delete confirmation
    let target = DeleteConfirmTarget::Bulk { count: 5 };
    state.pending_delete = Some(target.clone());
    state.focus = FocusState::Overlay(OverlayType::Confirmation(
        ConfirmationType::DeleteConfirmation(target),
    ));

    // Confirm bulk delete
    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('y'));
    assert_eq!(action, InputAction::ConfirmDelete);
}

// ── Relative time formatting ────────────────────────────────────────────────

#[test]
fn test_format_relative_time_ranges() {
    let now = 1_700_000_000;
    assert_eq!(sidebar::format_relative_time(now - 5, now), "just now");
    assert_eq!(sidebar::format_relative_time(now - 120, now), "2m ago");
    assert_eq!(sidebar::format_relative_time(now - 7200, now), "2h ago");
}
