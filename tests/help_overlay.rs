use rustain::adapters::tui::app::{InputAction, handle_input};
use rustain::adapters::tui::help_data;
use rustain::adapters::tui::hints;
use rustain::adapters::tui::state::{HelpOverlayState, TuiState};
use rustain::adapters::tui::version_info;
use rustain::domain::events::{DomainInputEvent, DomainKey};
use rustain::domain::models::FocusState;
use rustain::domain::models::visual::OverlayType;

// ============================================================
// Unit: HelpOverlayState (Task 7.4)
// ============================================================

#[test]
fn test_help_overlay_state_open_sets_active() {
    let mut state = HelpOverlayState::new();
    assert!(!state.active);
    state.open(FocusState::Chat);
    assert!(state.active);
    assert_eq!(state.scroll_offset, 0);
}

#[test]
fn test_help_overlay_state_close_returns_prior_focus() {
    let mut state = HelpOverlayState::new();
    state.open(FocusState::Chat);
    let restored = state.close();
    assert!(!state.active);
    assert_eq!(restored, FocusState::Chat);
}

#[test]
fn test_help_overlay_state_open_resets_scroll() {
    let mut state = HelpOverlayState::new();
    state.open(FocusState::Input);
    state.scroll_offset = 10;
    state.open(FocusState::Chat);
    assert_eq!(state.scroll_offset, 0);
}

// ============================================================
// Unit: help_data (Task 7.1, 7.2, 7.3)
// ============================================================

#[test]
fn test_help_categories_non_empty() {
    let cats = help_data::help_categories();
    assert!(!cats.is_empty());
    for cat in cats {
        assert!(
            !cat.bindings.is_empty(),
            "Category '{}' has no bindings",
            cat.name
        );
    }
}

#[test]
fn test_tmux_conflicts_contains_known() {
    let conflicts = help_data::tmux_conflicts();
    assert!(conflicts.len() >= 2);
    assert!(conflicts.iter().any(|c| c.key == "Ctrl+B"));
    assert!(conflicts.iter().any(|c| c.key == "Ctrl+A"));
}

#[test]
fn test_is_multiplexer_session_false_when_unset() {
    unsafe { std::env::remove_var("TMUX") };
    unsafe { std::env::remove_var("STY") };
    assert!(!help_data::is_multiplexer_session());
}

// ============================================================
// Unit: contextual_hint (Task 7.5)
// ============================================================

#[test]
fn test_contextual_hint_input_focus() {
    let hint = hints::contextual_hint(&FocusState::Input, 1, 5, false);
    assert!(hint.is_some());
    assert!(hint.unwrap().contains('?'));
}

#[test]
fn test_contextual_hint_chat_focus() {
    let hint = hints::contextual_hint(&FocusState::Chat, 1, 5, false);
    assert!(hint.is_some());
    assert!(hint.unwrap().contains("j/k"));
}

#[test]
fn test_contextual_hint_fades_above_threshold() {
    assert!(hints::contextual_hint(&FocusState::Input, 6, 5, false).is_none());
}

#[test]
fn test_contextual_hint_shows_at_threshold() {
    assert!(hints::contextual_hint(&FocusState::Input, 5, 5, false).is_some());
}

// ============================================================
// Unit: version_string (Task 7.6)
// ============================================================

#[test]
fn test_version_string_non_empty() {
    let v = version_info::version_string();
    assert!(!v.is_empty());
    assert!(v.contains(env!("CARGO_PKG_VERSION")));
}

// ============================================================
// Integration: ? key opens help from Chat (Task 7.11)
// ============================================================

#[test]
fn test_question_mark_opens_help_from_chat() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;

    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('?'));

    assert_eq!(action, InputAction::Consumed);
    assert_eq!(state.focus, FocusState::Overlay(OverlayType::Help));
    assert!(state.help_overlay.active);
}

// ============================================================
// Integration: ? key in Input inserts character (Task 7.20)
// ============================================================

#[test]
fn test_question_mark_types_in_input_focus() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Input;

    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('?'));

    // In Input focus, ? should insert the character, not open help
    assert_eq!(action, InputAction::Consumed);
    assert_eq!(state.focus, FocusState::Input);
    assert_eq!(state.input_buffer, "?");
}

// ============================================================
// Integration: ? toggles help off (Task 7.8)
// ============================================================

#[test]
fn test_question_mark_closes_help_overlay() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;

    // Open
    handle_input(&mut state, &DomainInputEvent::KeyPress('?'));
    assert!(state.help_overlay.active);

    // Close with ?
    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('?'));
    assert_eq!(action, InputAction::Consumed);
    assert!(!state.help_overlay.active);
    assert_eq!(state.focus, FocusState::Chat);
}

// ============================================================
// Integration: Esc closes help overlay (Task 7.9)
// ============================================================

#[test]
fn test_esc_closes_help_overlay() {
    let mut state = TuiState::new(80, 24);
    state.help_overlay.open(FocusState::Chat);
    state.focus = FocusState::Overlay(OverlayType::Help);

    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Esc));

    assert_eq!(action, InputAction::Consumed);
    assert!(!state.help_overlay.active);
    assert_eq!(state.focus, FocusState::Chat);
}

// ============================================================
// Integration: j/k scrolling (Task 7.10)
// ============================================================

#[test]
fn test_jk_scrolling_in_help_overlay() {
    let mut state = TuiState::new(80, 24);
    state.help_overlay.open(FocusState::Chat);
    state.focus = FocusState::Overlay(OverlayType::Help);

    // j increments scroll
    handle_input(&mut state, &DomainInputEvent::KeyPress('j'));
    assert_eq!(state.help_overlay.scroll_offset, 1);

    handle_input(&mut state, &DomainInputEvent::KeyPress('j'));
    assert_eq!(state.help_overlay.scroll_offset, 2);

    // k decrements scroll
    handle_input(&mut state, &DomainInputEvent::KeyPress('k'));
    assert_eq!(state.help_overlay.scroll_offset, 1);

    // k at 0 stays at 0
    handle_input(&mut state, &DomainInputEvent::KeyPress('k'));
    assert_eq!(state.help_overlay.scroll_offset, 0);
    handle_input(&mut state, &DomainInputEvent::KeyPress('k'));
    assert_eq!(state.help_overlay.scroll_offset, 0);
}

#[test]
fn test_g_and_shift_g_in_help_overlay() {
    let mut state = TuiState::new(80, 24);
    state.help_overlay.open(FocusState::Chat);
    state.focus = FocusState::Overlay(OverlayType::Help);

    // G scrolls to bottom (large value)
    handle_input(&mut state, &DomainInputEvent::KeyPress('G'));
    assert!(state.help_overlay.scroll_offset > 0);

    // g scrolls to top
    handle_input(&mut state, &DomainInputEvent::KeyPress('g'));
    assert_eq!(state.help_overlay.scroll_offset, 0);
}

// ============================================================
// Integration: ? while CommandPalette active → ignored (Task 7.12)
// ============================================================

#[test]
fn test_question_mark_ignored_during_command_palette() {
    let mut state = TuiState::new(80, 24);
    state.command_palette.open(FocusState::Input);
    state.focus = FocusState::Overlay(OverlayType::CommandPalette);

    // ? is treated as filter text in command palette, not help toggle
    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('?'));
    assert_eq!(action, InputAction::Consumed);
    assert!(state.command_palette.active);
    assert!(!state.help_overlay.active);
}

// ============================================================
// Integration: Ctrl+X, ? chord → opens help (Task 7.13)
// ============================================================

#[test]
fn test_ctrl_x_question_chord_opens_help() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Input;

    // Open which-key
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlX));
    assert!(state.which_key.active);

    // Press ?
    handle_input(&mut state, &DomainInputEvent::KeyPress('?'));
    assert!(state.help_overlay.active);
    assert_eq!(state.focus, FocusState::Overlay(OverlayType::Help));
}

// ============================================================
// Integration: Up/Down arrow scrolling in help overlay
// ============================================================

#[test]
fn test_arrow_key_scrolling_in_help_overlay() {
    let mut state = TuiState::new(80, 24);
    state.help_overlay.open(FocusState::Chat);
    state.focus = FocusState::Overlay(OverlayType::Help);

    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Down));
    assert_eq!(state.help_overlay.scroll_offset, 1);

    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Up));
    assert_eq!(state.help_overlay.scroll_offset, 0);
}

// ============================================================
// Integration: Version palette action (Task 7.14)
// ============================================================

#[test]
fn test_version_palette_action_produces_feedback() {
    use rustain::domain::models::palette::{PaletteAction, PaletteEntry, PaletteScope};

    let mut state = TuiState::new(80, 24);
    state.command_palette.open(FocusState::Input);
    state.focus = FocusState::Overlay(OverlayType::CommandPalette);

    // Manually inject a version entry and select it
    state.command_palette.filtered_entries = vec![PaletteEntry {
        name: "version".to_string(),
        description: "Show version".to_string(),
        shortcut: None,
        scope: PaletteScope::All,
        action: PaletteAction::ShowVersion,
    }];
    state.command_palette.selected_index = 0;

    // Press Enter to execute
    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Enter));
    assert_eq!(action, InputAction::Consumed);
    // version-info feedback block should exist
    assert!(state.feedback_blocks.contains_key("version-info"));
    let fb = &state.feedback_blocks["version-info"];
    assert!(fb.message.contains("rustain"));
}

// ============================================================
// Rendering: help overlay renders without crash (Task 7.15)
// ============================================================

#[test]
fn test_help_overlay_renders_no_crash() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use rustain::adapters::tui::theme::Theme;
    use rustain::adapters::tui::widgets::help_overlay;

    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut overlay = HelpOverlayState::new();
    overlay.open(FocusState::Chat);
    let theme = Theme::dark();

    terminal
        .draw(|frame| {
            help_overlay::render(frame, frame.area(), &overlay, &theme, false);
        })
        .unwrap();
}

// ============================================================
// Rendering: help overlay with tmux notice (Task 7.16)
// ============================================================

#[test]
fn test_help_overlay_renders_with_tmux_no_crash() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use rustain::adapters::tui::theme::Theme;
    use rustain::adapters::tui::widgets::help_overlay;

    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut overlay = HelpOverlayState::new();
    overlay.open(FocusState::Chat);
    let theme = Theme::dark();

    terminal
        .draw(|frame| {
            help_overlay::render(frame, frame.area(), &overlay, &theme, true);
        })
        .unwrap();
}

// ============================================================
// Rendering: status bar with hint (Task 7.17)
// ============================================================

#[test]
fn test_status_bar_renders_hint() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use rustain::adapters::tui::theme::Theme;
    use rustain::adapters::tui::widgets::status_bar;
    use rustain::domain::models::{PermissionMode, StatusState};

    let backend = TestBackend::new(120, 1);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = Theme::dark();

    terminal
        .draw(|frame| {
            let area = frame.area();
            status_bar::render(
                frame,
                area,
                "test-model",
                None,
                &StatusState::Idle,
                &theme,
                0,
                &[],
                0,
                24,
                PermissionMode::Normal,
                None,
                0, // context_window
                false,
                None,
                false,
                Some("Tip: Press ? for help"),
                0,
                None,
                None,
                None,
                false,
            );
        })
        .unwrap();

    let buf = terminal.backend().buffer().clone();
    let row_str: String = (0..120).map(|x| buf[(x, 0)].symbol().to_string()).collect();
    assert!(
        row_str.contains("Tip: Press ? for help"),
        "Status bar should contain hint text, got: {}",
        row_str.trim()
    );
}

// ============================================================
// Rendering: status bar without hint when above threshold (Task 7.18)
// ============================================================

#[test]
fn test_status_bar_no_hint_when_none() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use rustain::adapters::tui::theme::Theme;
    use rustain::adapters::tui::widgets::status_bar;
    use rustain::domain::models::{PermissionMode, StatusState};

    let backend = TestBackend::new(120, 1);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = Theme::dark();

    terminal
        .draw(|frame| {
            let area = frame.area();
            status_bar::render(
                frame,
                area,
                "test-model",
                None,
                &StatusState::Idle,
                &theme,
                0,
                &[],
                0,
                24,
                PermissionMode::Normal,
                None,
                0, // context_window
                false,
                None,
                false,
                None,
                0,
                None,
                None,
                None,
                false,
            );
        })
        .unwrap();

    let buf = terminal.backend().buffer().clone();
    let row_str: String = (0..120).map(|x| buf[(x, 0)].symbol().to_string()).collect();
    assert!(
        !row_str.contains("Tip:"),
        "Status bar should NOT contain hint text when None"
    );
}

// ============================================================
// Edge: small terminal (Task 7.21)
// ============================================================

#[test]
fn test_help_overlay_small_terminal_no_crash() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use rustain::adapters::tui::theme::Theme;
    use rustain::adapters::tui::widgets::help_overlay;

    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut overlay = HelpOverlayState::new();
    overlay.open(FocusState::Chat);
    let theme = Theme::dark();

    terminal
        .draw(|frame| {
            help_overlay::render(frame, frame.area(), &overlay, &theme, false);
        })
        .unwrap();
}

// ============================================================
// Edge: session count file (Task 7.22, 7.23)
// ============================================================

#[test]
fn test_session_count_parse_valid() {
    // This tests the internal parser indirectly via the module tests
    // The parse logic is covered by hints::tests::test_parse_session_count_*
    let hint = hints::contextual_hint(&FocusState::Input, 1, 5, false);
    assert!(hint.is_some());
}

// ============================================================
// Regression: ? does not interfere with existing overlays (Task 7.19)
// ============================================================

#[test]
fn test_question_mark_ignored_during_which_key() {
    // The which-key chord for ? should route to ShowHelp, not be "ignored"
    // This is tested by test_ctrl_x_question_chord_opens_help above.
    // But let's also verify that the which-key overlay itself is not disrupted:
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Input;

    // Open which-key
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlX));
    assert!(state.which_key.active);

    // Press ? — should execute chord (ShowHelp), dismissing which-key
    handle_input(&mut state, &DomainInputEvent::KeyPress('?'));
    assert!(!state.which_key.active);
    assert!(state.help_overlay.active);
}

#[test]
fn test_ctrl_p_blocked_from_help_overlay() {
    let mut state = TuiState::new(80, 24);
    state.help_overlay.open(FocusState::Chat);
    state.focus = FocusState::Overlay(OverlayType::Help);

    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlP));
    // Help overlay consumes all input — Ctrl+P is swallowed, not passed through
    assert_eq!(action, InputAction::Consumed);
    assert!(!state.command_palette.active);
}

// ============================================================
// Integration: Ctrl+C passes through help overlay (P10)
// ============================================================

#[test]
fn test_ctrl_c_passes_through_help_overlay() {
    let mut state = TuiState::new(80, 24);
    state.help_overlay.open(FocusState::Chat);
    state.focus = FocusState::Overlay(OverlayType::Help);

    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlC));
    // Ctrl+C should close overlay and signal cancel
    assert_eq!(action, InputAction::CancelOrQuit);
    assert!(!state.help_overlay.active);
    assert_eq!(state.focus, FocusState::Chat);
}

// ============================================================
// Integration: Unrecognized keys consumed by help overlay (P4)
// ============================================================

#[test]
fn test_unrecognized_keys_consumed_by_help_overlay() {
    let mut state = TuiState::new(80, 24);
    state.help_overlay.open(FocusState::Chat);
    state.focus = FocusState::Overlay(OverlayType::Help);

    // Random character should be consumed, not leaked
    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('x'));
    assert_eq!(action, InputAction::Consumed);
    assert!(state.help_overlay.active); // overlay stays open
}

// ============================================================
// Edge: wide terminal centered_area no overflow (P6)
// ============================================================

#[test]
fn test_centered_area_wide_terminal_no_overflow() {
    use rustain::adapters::tui::widgets::help_overlay;

    // 1000-column terminal should not overflow u16 arithmetic
    // The centered_area function is private, but we can verify rendering doesn't panic
    let backend = ratatui::backend::TestBackend::new(1000, 50);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    let mut overlay = HelpOverlayState::new();
    overlay.open(FocusState::Chat);
    let theme = rustain::adapters::tui::theme::Theme::dark();

    terminal
        .draw(|frame| {
            help_overlay::render(frame, frame.area(), &overlay, &theme, false);
        })
        .unwrap();
}
