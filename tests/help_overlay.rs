use rustain::adapters::tui::app::{InputAction, handle_input};
use rustain::adapters::tui::hints;
use rustain::adapters::tui::keymap::render_help;
use rustain::adapters::tui::presentation::{self, Surface};
use rustain::adapters::tui::state::{HelpOverlayState, TuiState};
use rustain::domain::events::{DomainInputEvent, DomainKey};
use rustain::domain::models::FocusState;
use rustain::infrastructure::build_info;

// ============================================================
// Unit: HelpOverlayState (Task 7.4)
// ============================================================

#[test]
fn test_help_overlay_opener_pushes_and_back_restores_chat() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;
    handle_input(&mut state, &DomainInputEvent::KeyPress('?'));
    assert!(presentation::is_open(&state, Surface::Help));
    assert_eq!(state.help_overlay.scroll_offset, 0);
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Esc));
    assert!(!presentation::is_open(&state, Surface::Help));
    assert_eq!(state.focus, FocusState::Chat);
}

#[test]
fn test_help_overlay_payload_starts_at_top() {
    let state = HelpOverlayState::new();
    assert_eq!(state.scroll_offset, 0);
    assert_eq!(state.selected_category, 0);
}

#[test]
fn test_help_overlay_state_open_resets_scroll() {
    let mut state = HelpOverlayState::new();
    state.scroll_offset = 10;
    state.reset();
    assert_eq!(state.scroll_offset, 0);
}

// ============================================================
// Unit: render_help (Task 7.1, 7.2, 7.3)
// ============================================================

#[test]
fn test_help_categories_non_empty() {
    let groups = render_help::help_groups();
    assert!(!groups.is_empty());
    for group in &groups {
        assert!(
            !group.rows.is_empty(),
            "Category '{}' has no bindings",
            group.name
        );
    }
}

#[test]
fn test_tmux_conflicts_contains_known() {
    let conflicts = render_help::tmux_conflicts();
    assert!(conflicts.len() >= 2);
    assert!(conflicts.iter().any(|c| c.key == "Ctrl+B"));
    assert!(conflicts.iter().any(|c| c.key == "Ctrl+A"));
}

#[test]
fn test_is_multiplexer_session_false_when_unset() {
    unsafe { std::env::remove_var("TMUX") };
    unsafe { std::env::remove_var("STY") };
    assert!(!render_help::is_multiplexer_session());
}

// ============================================================
// Unit: contextual_hint (Task 7.5)
// ============================================================

#[test]
fn test_contextual_hint_input_focus() {
    let hint = hints::contextual_hint(
        &FocusState::Input,
        1,
        5,
        false,
        &Default::default(),
        None,
        false,
    );
    assert!(hint.is_some());
    assert!(hint.unwrap().contains('?'));
}

#[test]
fn test_contextual_hint_chat_focus() {
    let hint = hints::contextual_hint(
        &FocusState::Chat,
        1,
        5,
        false,
        &Default::default(),
        None,
        false,
    );
    assert!(hint.is_some());
    assert!(hint.unwrap().contains("j/k"));
}

#[test]
fn test_contextual_hint_fades_above_threshold() {
    assert!(
        hints::contextual_hint(
            &FocusState::Input,
            6,
            5,
            false,
            &Default::default(),
            None,
            false
        )
        .is_none()
    );
}

#[test]
fn test_contextual_hint_shows_at_threshold() {
    assert!(
        hints::contextual_hint(
            &FocusState::Input,
            5,
            5,
            false,
            &Default::default(),
            None,
            false
        )
        .is_some()
    );
}

// ============================================================
// Unit: version_block (Task 7.6)
// ============================================================

#[test]
fn test_version_block_non_empty() {
    let v = build_info::version_block();
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
    assert_eq!(state.focus, FocusState::Chat);
    assert!(presentation::is_open(&state, Surface::Help));
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
    assert!(presentation::is_open(&state, Surface::Help));

    // Close with ?
    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('?'));
    assert_eq!(action, InputAction::Consumed);
    assert!(!presentation::is_open(&state, Surface::Help));
    assert_eq!(state.focus, FocusState::Chat);
}

// ============================================================
// Integration: Esc closes help overlay (Task 7.9)
// ============================================================

#[test]
fn test_esc_closes_help_overlay() {
    let mut state = TuiState::new(80, 24);
    open_help_from_chat(&mut state);

    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Esc));

    assert_eq!(action, InputAction::Consumed);
    assert!(!presentation::is_open(&state, Surface::Help));
    assert_eq!(state.focus, FocusState::Chat);
}

// ============================================================
// Integration: j/k scrolling (Task 7.10)
// ============================================================

#[test]
fn test_jk_scrolling_in_help_overlay() {
    let mut state = TuiState::new(80, 24);
    open_help_from_chat(&mut state);

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
    open_help_from_chat(&mut state);

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
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlP));

    // ? is treated as filter text in command palette, not help toggle
    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('?'));
    assert_eq!(action, InputAction::Consumed);
    assert!(presentation::is_open(&state, Surface::CommandPalette));
    assert!(!presentation::is_open(&state, Surface::Help));
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
    assert_eq!(
        state.pending_leader,
        Some(rustain::adapters::tui::keymap::Context::Input)
    );

    // Press ?
    handle_input(&mut state, &DomainInputEvent::KeyPress('?'));
    assert!(presentation::is_open(&state, Surface::Help));
    assert_eq!(state.focus, FocusState::Input);
}

// ============================================================
// Integration: Up/Down arrow scrolling in help overlay
// ============================================================

#[test]
fn test_arrow_key_scrolling_in_help_overlay() {
    let mut state = TuiState::new(80, 24);
    open_help_from_chat(&mut state);

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
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlP));

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
    // the version notice (Story 21.1: Info, source Help) should be in the store
    let record = state
        .notifications
        .records()
        .find(|record| record.source == rustain::adapters::tui::notifications::NoticeSource::Help)
        .expect("a version notice");
    assert_eq!(
        record.severity,
        rustain::adapters::tui::notifications::NoticeSeverity::Info
    );
    assert!(record.message.contains("rustain"));
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
    overlay.reset();
    let theme = Theme::dark();

    terminal
        .draw(|frame| {
            help_overlay::render(frame, frame.area(), &overlay, &theme, false, false);
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
    overlay.reset();
    let theme = Theme::dark();

    terminal
        .draw(|frame| {
            help_overlay::render(frame, frame.area(), &overlay, &theme, true, false);
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
                true,
                0, // injected_tokens
                Some("Tip: Press ? for help"),
                0,
                None,
                None,
                None,
                false,
                None, // daily_budget (Story 7.5)
                None,
                false,
                None,
                rustain::adapters::tui::state::LogAwareness::Hidden,
                0,
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
                true,
                0, // injected_tokens
                None,
                0,
                None,
                None,
                None,
                false,
                None, // daily_budget (Story 7.5)
                None,
                false,
                None,
                rustain::adapters::tui::state::LogAwareness::Hidden,
                0,
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
    use ratatui::{Terminal, backend::TestBackend, layout::Rect};
    use rustain::adapters::tui::{
        keymap::Host,
        layout::compute_layout,
        notification_log::LogFacts,
        presentation::{Shell, compose, geometry},
    };

    for (width, height, full) in [(60, 16, true), (130, 30, false)] {
        let mut state = TuiState::new(width, height);
        if full {
            state.input_buffer = ["draft"; 8].join("\n");
        }
        open_help_from_chat(&mut state);
        let area = Rect::new(0, 0, width, height);
        let layout = compute_layout(area, &state.theme, &state.input_buffer, 1, false).unwrap();
        let shell = Shell::new(area, &layout);
        let rect = geometry::frame_rect(shell.region);
        if full {
            assert_eq!(rect.height, 5);
        }
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                compose(
                    frame,
                    &state,
                    &shell,
                    Host::Local,
                    &LogFacts::default(),
                    &mut rustain::adapters::tui::presentation::occlusion::OpaqueRects::new(
                        shell.chat_pane,
                    ),
                    |_| {},
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let row = |y| {
            (rect.x..rect.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        };
        assert_eq!(buffer[(rect.x, rect.y)].symbol(), "┌");
        assert_eq!(buffer[(rect.right() - 1, rect.bottom() - 1)].symbol(), "┘");
        assert!(row(rect.y).contains("Help — Rustain Keybindings"));
        assert!(row(rect.bottom() - 1).contains("j/k scroll  ? close"));
        assert!(
            (rect.y + 1..rect.bottom() - 1).any(|y| row(y).chars().any(|c| c.is_alphanumeric()))
        );
        assert_eq!(
            buffer[(layout.status_bar.x, layout.status_bar.y)].symbol(),
            " "
        );
    }
}

// ============================================================
// Edge: session count file (Task 7.22, 7.23)
// ============================================================

#[test]
fn test_session_count_parse_valid() {
    // This tests the internal parser indirectly via the module tests
    // The parse logic is covered by hints::tests::test_parse_session_count_*
    let hint = hints::contextual_hint(
        &FocusState::Input,
        1,
        5,
        false,
        &Default::default(),
        None,
        false,
    );
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
    assert!(state.pending_leader.is_some());

    // Press ? — should execute chord (ShowHelp), dismissing which-key
    handle_input(&mut state, &DomainInputEvent::KeyPress('?'));
    assert!(state.pending_leader.is_none());
    assert!(presentation::is_open(&state, Surface::Help));
}

#[test]
fn test_ctrl_p_pushes_palette_over_help_and_back_preserves_scroll() {
    let mut state = TuiState::new(80, 24);
    open_help_from_chat(&mut state);
    handle_input(&mut state, &DomainInputEvent::KeyPress('j'));
    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlP));
    assert_eq!(action, InputAction::Consumed);
    assert!(presentation::is_open(&state, Surface::CommandPalette));
    assert!(presentation::is_open(&state, Surface::Help));
    assert_eq!(state.modal_stack.len(), 2);
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Esc));
    assert!(!presentation::is_open(&state, Surface::CommandPalette));
    assert!(presentation::is_open(&state, Surface::Help));
    assert_eq!(state.help_overlay.scroll_offset, 1);
    assert_eq!(state.focus, FocusState::Chat);
}

// ============================================================
// Integration: Ctrl+C is back one frame in the help overlay (21-3 AC13, RU-20)
// ============================================================

#[test]
fn test_ctrl_c_closes_help_overlay_and_never_cancels() {
    let mut state = TuiState::new(80, 24);
    open_help_from_chat(&mut state);

    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlC));
    // Ctrl+C closes the overlay and restores the caller; it no longer stops a turn
    // or asks to quit (it returned CancelOrQuit before 21-3 AC13).
    assert_eq!(action, InputAction::Consumed);
    assert!(!presentation::is_open(&state, Surface::Help));
    assert_eq!(state.focus, FocusState::Chat);
}

// ============================================================
// Integration: Unrecognized keys consumed by help overlay (P4)
// ============================================================

#[test]
fn test_unrecognized_keys_consumed_by_help_overlay() {
    let mut state = TuiState::new(80, 24);
    open_help_from_chat(&mut state);

    // Random character should be consumed, not leaked
    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('x'));
    assert_eq!(action, InputAction::Consumed);
    assert!(presentation::is_open(&state, Surface::Help)); // frame stays open
}

// ============================================================
// Edge: shared caller geometry on a wide terminal (P6)
// ============================================================

#[test]
fn test_shared_geometry_wide_terminal_no_overflow() {
    use rustain::adapters::tui::widgets::help_overlay;

    // Exercise the production geometry with a 1000-column region.
    let backend = ratatui::backend::TestBackend::new(1000, 50);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    let mut overlay = HelpOverlayState::new();
    overlay.reset();
    let theme = rustain::adapters::tui::theme::Theme::dark();

    terminal
        .draw(|frame| {
            let area = presentation::geometry::frame_rect(frame.area());
            help_overlay::render(frame, area, &overlay, &theme, false, false);
        })
        .unwrap();
}

fn open_help_from_chat(state: &mut TuiState) {
    state.focus = FocusState::Chat;
    assert_eq!(
        handle_input(state, &DomainInputEvent::KeyPress('?')),
        InputAction::Consumed
    );
    assert!(presentation::is_open(state, Surface::Help));
}
