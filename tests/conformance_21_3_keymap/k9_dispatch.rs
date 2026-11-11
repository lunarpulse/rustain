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

/// DF-256: Tasks-tab `Enter` opens the selected task's window (a `TaskRef`); with no task the
/// Tasks tab has nothing to open and `Enter` is consumed (never the composer or the Stage).
/// The sidebar's open-entry fallback is witnessed on History.
#[test]
fn tasks_enter_opens_the_task_window_and_history_enter_opens_the_entry() {
    let mut s = super::support::tasks_hub();
    assert_eq!(
        press(&mut s, Key::Enter),
        InputAction::OpenTaskWindow(super::support::task_ref(2))
    );

    let mut s = fresh();
    rustain::adapters::tui::presentation::reducer::open_mission_control(
        &mut s,
        rustain::adapters::tui::mission_control::McTab::Tasks,
    );
    rustain::adapters::tui::mission_control::tasks::sync(
        &mut s,
        &rustain::domain::models::Conversation::default(),
    );
    assert_eq!(press(&mut s, Key::Enter), InputAction::Consumed);

    let mut s = fresh();
    s.sidebar_visible = true;
    s.focus = FocusState::Sidebar {
        panel: PanelType::History,
        selected: 1,
    };
    assert_eq!(
        press(&mut s, Key::Enter),
        InputAction::OpenSidebarConversation
    );
}

/// DF-262: shrinking below the sidebar width drops the sidebar and the sidebar focus; a wide
/// resize keeps them. (The task drill-down this once also dropped is gone, 21-7 7-D.)
#[test]
fn narrow_resize_clears_the_sidebar() {
    let setup = || {
        let mut s = fresh();
        s.terminal_width = 130;
        s.sidebar_visible = true;
        s.sidebar_panel = Some(PanelType::History);
        s.focus = FocusState::Sidebar {
            panel: PanelType::History,
            selected: 0,
        };
        s
    };
    let mut s = setup();
    let resize = |s: &mut TuiState, width| handle_input(s, &DomainInputEvent::Resize(width, 30));
    assert_eq!(resize(&mut s, 100), InputAction::Consumed);
    assert!(!s.sidebar_visible);
    assert_eq!(s.sidebar_panel, None);
    assert_eq!(s.focus, FocusState::Chat);

    let mut s = setup();
    resize(&mut s, 140);
    assert!(s.sidebar_visible);
    assert_eq!(s.sidebar_panel, Some(PanelType::History));
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

/// An armed reorder owns `Esc` only inside Mission Control's Tasks tab (`MissionReorder`): the
/// reorder cancels there; Chat's own `Esc` no longer depends on a reorder state (DF-21-3-REORDER-WAVE-PRECEDENCE).
#[test]
fn esc_cancels_an_armed_reorder_only_in_the_tasks_tab() {
    let mut s = super::support::reorder_hub();
    assert_eq!(press(&mut s, Key::Esc), InputAction::TaskReorderCancel);

    let mut plain = chat();
    let expected = press(&mut plain, Key::Esc);
    let mut armed = chat();
    armed.task_panel_state.reorder = Some(rustain::adapters::tui::state::Reorder {
        conversation_id: "c".to_owned(),
        plan_id: "p".to_owned(),
        number: 2,
        original_order: vec![1, 2, 3],
    });
    assert_eq!(press(&mut armed, Key::Esc), expected);
    assert_ne!(expected, InputAction::TaskReorderCancel);
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
        let panel = PanelType::History;
        assert_eq!(effect(panel, key), expect, "{panel:?} {key:?}");
    }
}
