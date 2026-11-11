//! K9 (AC10, AC22): the resolver-driven local dispatch behaves as the hand-written arms did.
//! The per-row equivalence record is the K4/K10 witness suites (green on the hand-written arms
//! before 3-B2, green on the resolver after); this leg adds what they cannot see: Chat sequences
//! and the pending tail, the guarded `Enter` (DF-256), the narrow resize (DF-262) and a command
//! that declines to its parent row.

use rustain::adapters::tui::app::{InputAction, handle_input, status_tail};
use rustain::adapters::tui::keymap::Key;
use rustain::adapters::tui::keymap::render::{PendingPrefix, StatusTail};
use rustain::adapters::tui::state::{Direction, TuiState};
use rustain::domain::events::DomainInputEvent;
use rustain::domain::models::FocusState;
use rustain::domain::models::visual::PanelType;

use super::support::{StubWave, fresh, press};

fn chat() -> TuiState {
    let mut state = fresh();
    state.focus = FocusState::Chat;
    state
}

/// A `z` / `[` / `]` prefix shows in the status tail, a special key or an unknown second key
/// clears it, and `gg` jumps once.
#[test]
fn chat_sequences_set_the_pending_tail_and_clear_it() {
    let mut s = chat();
    assert_eq!(press(&mut s, Key::Char('z')), InputAction::Consumed);
    assert_eq!(status_tail(&s), StatusTail::Pending(PendingPrefix::Z));
    assert_eq!(
        press(&mut s, Key::Char('a')),
        InputAction::FoldToggleAtFocus
    );
    assert_eq!(status_tail(&s), StatusTail::Menu);

    press(&mut s, Key::Char('['));
    assert_eq!(
        status_tail(&s),
        StatusTail::Pending(PendingPrefix::BracketOpen)
    );
    press(&mut s, Key::Up);
    assert_eq!(status_tail(&s), StatusTail::Menu);

    press(&mut s, Key::Char(']'));
    assert_eq!(
        press(&mut s, Key::Char(']')),
        InputAction::JumpProseAnchor(Direction::Down)
    );
    press(&mut s, Key::Char('z'));
    assert_eq!(press(&mut s, Key::Char('x')), InputAction::Consumed);
    assert_eq!(status_tail(&s), StatusTail::Menu);

    assert_eq!(press(&mut s, Key::Char('g')), InputAction::ScrollToTop);
    assert_eq!(press(&mut s, Key::Char('g')), InputAction::Consumed);
    assert_eq!(press(&mut s, Key::Char('g')), InputAction::ScrollToTop);
}

/// DF-256: Tasks `Enter` drills into the selected task, and with no task it is the sidebar's
/// open-entry key (the Tasks row declines to its parent).
#[test]
fn tasks_enter_drills_with_tasks_and_opens_the_entry_without() {
    let tasks = |count: usize| {
        let mut s = fresh();
        s.sidebar_visible = true;
        s.focus = FocusState::Sidebar {
            panel: PanelType::Tasks,
            selected: 1,
        };
        s.task_panel_state.task_count = count;
        s.task_panel_state.selected_index = 1;
        s
    };
    let mut s = tasks(3);
    assert_eq!(press(&mut s, Key::Enter), InputAction::Consumed);
    assert_eq!(s.task_panel_state.drill_down_task, Some(2));
    assert_eq!(s.focus, FocusState::Chat);

    let mut s = tasks(0);
    assert_eq!(
        press(&mut s, Key::Enter),
        InputAction::OpenSidebarConversation
    );
    assert_eq!(s.task_panel_state.drill_down_task, None);
}

/// DF-262: shrinking below the sidebar width drops the sidebar, the drill-down and the
/// sidebar focus; a wide resize keeps them.
#[test]
fn narrow_resize_clears_the_drill_down_and_the_sidebar() {
    let setup = || {
        let mut s = fresh();
        s.terminal_width = 130;
        s.sidebar_visible = true;
        s.sidebar_panel = Some(PanelType::Tasks);
        s.task_panel_state.drill_down_task = Some(2);
        s.focus = FocusState::Sidebar {
            panel: PanelType::Tasks,
            selected: 0,
        };
        s
    };
    let mut s = setup();
    let resize = |s: &mut TuiState, width| handle_input(s, &DomainInputEvent::Resize(width, 30));
    assert_eq!(resize(&mut s, 100), InputAction::Consumed);
    assert!(!s.sidebar_visible);
    assert_eq!(s.sidebar_panel, None);
    assert_eq!(s.task_panel_state.drill_down_task, None);
    assert_eq!(s.focus, FocusState::Chat);

    let mut s = setup();
    resize(&mut s, 140);
    assert!(s.sidebar_visible);
    assert_eq!(s.task_panel_state.drill_down_task, Some(2));
}

/// Messages `Esc` with no kept filter is not swallowed by the tab: the one Mission Control frame
/// closes (the A2A log was a sidebar panel whose `Esc` declined to the sidebar's back-to-Chat).
#[test]
fn messages_esc_with_nothing_to_clear_closes_the_frame() {
    let mut s = fresh();
    rustain::adapters::tui::mission_control::open_messages(
        &mut s,
        false,
        &["row one".to_owned(), "— closing claim".to_owned()],
        None,
    );
    assert!(rustain::adapters::tui::presentation::is_open(
        &s,
        rustain::adapters::tui::presentation::Surface::MissionControl
    ));
    assert_eq!(press(&mut s, Key::Esc), InputAction::Consumed);
    assert!(s.modal_stack.is_empty(), "the frame closed");
    assert!(
        s.mission_control.messages.is_none(),
        "a closed visit drops its page"
    );
}

/// An armed reorder wins `Esc` over an open drill-down (the old arm order): the reorder
/// cancels and the drill stays open.
#[test]
fn esc_cancels_an_armed_reorder_before_the_drill_down_closes() {
    let mut s = chat();
    s.task_panel_state.drill_down_task = Some(2);
    s.task_panel_state.reorder_mode_for = Some(2);
    assert_eq!(press(&mut s, Key::Esc), InputAction::TaskReorderCancel);
    assert_eq!(s.task_panel_state.drill_down_task, Some(2));
}

/// With a wave retained, a `z` prefix swallows the wave key after it (`z d` does not open the
/// diverge view and clears the tail), while plain `d` still opens it and `z s` still toggles
/// the summary tier.
#[test]
fn a_z_prefix_swallows_the_wave_key_after_it() {
    let wave = || {
        let mut s = chat();
        s.wave_run = Some(std::sync::Arc::new(StubWave(3)));
        s
    };
    let mut s = wave();
    assert_eq!(press(&mut s, Key::Char('z')), InputAction::Consumed);
    assert_eq!(status_tail(&s), StatusTail::Pending(PendingPrefix::Z));
    assert_eq!(press(&mut s, Key::Char('d')), InputAction::Consumed);
    assert_eq!(status_tail(&s), StatusTail::Menu);
    assert_eq!(press(&mut s, Key::Char('d')), InputAction::OpenDivergeView);

    let mut s = wave();
    press(&mut s, Key::Char('z'));
    assert_eq!(
        press(&mut s, Key::Char('s')),
        InputAction::ToggleSummaryTier
    );
}

/// The Sidebar-family children declare no row for these keys, so each takes the Sidebar
/// context's effect through its parent link (a changed `parents()` or a deleted parent row
/// shows here).
#[test]
fn sidebar_family_children_take_the_sidebar_effect_for_inherited_keys() {
    let effect = |panel: PanelType, key: Key| {
        let mut s = fresh();
        s.sidebar_visible = true;
        s.focus = FocusState::Sidebar { panel, selected: 0 };
        let action = press(&mut s, key);
        let focus = match s.focus {
            FocusState::Sidebar { .. } => "Sidebar".to_string(),
            other => format!("{other:?}"),
        };
        format!("{action:?} | {focus}")
    };
    let keys = [
        (Key::Esc, "Consumed | Chat"),
        (Key::Char('q'), "Quit | Sidebar"),
        (Key::Tab, "Consumed | Input"),
        (Key::Ctrl('c'), "CancelOrQuit | Sidebar"),
        (Key::Ctrl('t'), "NewTab | Sidebar"),
        (Key::Alt('v'), "RequestClipboardPaste | Sidebar"),
        (Key::Enter, "OpenSidebarConversation | Sidebar"),
    ];
    for (key, expect) in keys {
        for panel in [PanelType::Tasks, PanelType::History] {
            assert_eq!(effect(panel, key), expect, "{panel:?} {key:?}");
        }
    }
}
