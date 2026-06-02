//! E2E tests for Story 3.5: Help Overlay, Version Info & Discoverability
//!
//! Uses TestHarness to verify end-to-end behavior of:
//! - Help overlay toggle (? key, Esc, Ctrl+X ? chord)
//! - Help overlay scrolling (j/k, arrow keys, g/G)
//! - tmux detection and conflict warnings
//! - Version info display via command palette
//! - Contextual status bar hints (first 5 sessions)

use rustain::adapters::tui::app::InputAction;
use rustain::adapters::tui::widgets::help_overlay;
use rustain::domain::events::DomainKey;
use rustain::domain::models::FocusState;
use rustain::domain::models::visual::OverlayType;
use serial_test::serial;

mod e2e_harness;
use e2e_harness::TestHarness;

// ═══════════════════════════════════════════════════════════════════════════
// AC1: Help Overlay Toggle
// ═══════════════════════════════════════════════════════════════════════════

/// Covers: AC1 — ? key opens help overlay from Chat focus
#[test]
fn test_e2e_help_overlay_opens_from_chat() {
    let mut h = TestHarness::new();

    // Start in Chat focus
    h.press_key(DomainKey::Esc);
    assert!(matches!(h.state.focus, FocusState::Chat));

    // Press ? to open help
    let action = h.type_char('?');
    assert!(matches!(action, InputAction::Consumed));
    assert!(matches!(
        h.state.focus,
        FocusState::Overlay(OverlayType::Help)
    ));
    assert!(h.state.help_overlay.active);

    // Render and verify help overlay appears (render help overlay directly)
    h.terminal
        .draw(|frame| {
            help_overlay::render(frame, frame.area(), &h.state.help_overlay, &h.theme, false);
        })
        .unwrap();

    h.assert_screen_contains("Help — Rustain Keybindings", "Help overlay title visible");
    h.assert_screen_contains(
        "Rustain is a composable AI agent platform",
        "Prose introduction visible",
    );
}

/// Covers: AC1 — ? key types character in Input focus (doesn't open help)
#[test]
fn test_e2e_help_overlay_does_not_open_in_input() {
    let mut h = TestHarness::new();

    // Ensure Input focus
    h.focus_input();
    assert!(matches!(h.state.focus, FocusState::Input));

    // Type ? — should appear in input, not open help
    let action = h.type_char('?');
    assert!(matches!(action, InputAction::Consumed));
    assert!(matches!(h.state.focus, FocusState::Input));
    assert!(!h.state.help_overlay.active);
    assert_eq!(h.state.input_buffer, "?");
}

/// Covers: AC1 — ? toggles help overlay off when active
#[test]
fn test_e2e_help_overlay_toggles_off_with_question() {
    let mut h = TestHarness::new();

    // Open help from Chat
    h.press_key(DomainKey::Esc);
    h.type_char('?');
    assert!(h.state.help_overlay.active);

    // Press ? again to close
    let action = h.type_char('?');
    assert!(matches!(action, InputAction::Consumed));
    assert!(!h.state.help_overlay.active);
    assert!(matches!(h.state.focus, FocusState::Chat));
}

/// Covers: AC1 — Esc closes help overlay
#[test]
fn test_e2e_help_overlay_closes_with_esc() {
    let mut h = TestHarness::new();

    // Open help
    h.press_key(DomainKey::Esc);
    h.type_char('?');
    assert!(h.state.help_overlay.active);

    // Press Esc to close
    let action = h.press_key(DomainKey::Esc);
    assert!(matches!(action, InputAction::Consumed));
    assert!(!h.state.help_overlay.active);
    assert!(matches!(h.state.focus, FocusState::Chat));
}

/// Covers: AC1 — Help overlay shows keybinding categories
#[test]
fn test_e2e_help_overlay_shows_categories() {
    // Use larger terminal to fit all categories without scrolling.
    // Height was increased from 50→65 in Story 3-6a (INPUT section expansion),
    // from 65→85 in Story 4-4 (SEARCH & BOOKMARKS category: 11 new bindings),
    // and from 85→110 for S16.6 (VIM FOLD & MOTION: 11 bindings) + PERMISSIONS (6 bindings).
    let mut h = TestHarness::with_size(100, 110);

    h.press_key(DomainKey::Esc);
    h.type_char('?');

    // Render help overlay directly
    h.terminal
        .draw(|frame| {
            help_overlay::render(frame, frame.area(), &h.state.help_overlay, &h.theme, false);
        })
        .unwrap();

    // Verify all categories are visible
    h.assert_screen_contains("NAVIGATION", "Navigation category visible");
    h.assert_screen_contains("INPUT", "Input category visible");
    h.assert_screen_contains("COMMANDS", "Commands category visible");
    h.assert_screen_contains("CHORDS", "Chords category visible");
    h.assert_screen_contains("CLIPBOARD", "Clipboard category visible");
    h.assert_screen_contains("GENERAL", "General category visible");

    // Verify some specific bindings
    h.assert_screen_contains("j / k", "Scroll binding visible");
    h.assert_screen_contains("Ctrl+P", "Command palette binding visible");
    h.assert_screen_contains("Ctrl+X", "Chord prefix visible");
}

// ═══════════════════════════════════════════════════════════════════════════
// AC3: tmux/screen Compatibility Notice
// ═══════════════════════════════════════════════════════════════════════════

/// Covers: AC3 — tmux warning shows when TMUX env var is set
#[test]
#[serial] // env var mutation — serialize to prevent cross-test contamination
fn test_e2e_help_overlay_shows_tmux_warning() {
    // Note: This test sets TMUX env var, which affects the global state
    // In a real E2E test with process isolation, this would be cleaner
    unsafe {
        std::env::set_var("TMUX", "/tmp/tmux-1000/default,1234,0");
    }

    // Use larger terminal to fit tmux warning without scrolling.
    // Height bumped 60→85 (S4.4), 85→100 (S6.0d), 100→120 (S16.8 SCROLL & MOUSE category).
    let mut h = TestHarness::with_size(100, 120);
    h.press_key(DomainKey::Esc);
    h.type_char('?');

    // Render help overlay with tmux detected
    h.terminal
        .draw(|frame| {
            help_overlay::render(
                frame,
                frame.area(),
                &h.state.help_overlay,
                &h.theme,
                true, // tmux detected
            );
        })
        .unwrap();

    // Verify tmux warning appears
    h.assert_screen_contains("tmux detected", "tmux warning visible");
    h.assert_screen_contains("Ctrl+B", "Ctrl+B conflict mentioned");
    h.assert_screen_contains("Ctrl+A", "Ctrl+A conflict mentioned");

    // Cleanup
    unsafe {
        std::env::remove_var("TMUX");
    }
}

/// Covers: AC3 — No tmux warning when not in tmux
#[test]
#[serial] // env var mutation — serialize to prevent cross-test contamination
fn test_e2e_help_overlay_no_tmux_warning_without_env() {
    unsafe {
        std::env::remove_var("TMUX");
    }

    let mut h = TestHarness::new();
    h.press_key(DomainKey::Esc);
    h.type_char('?');

    // Render help overlay without tmux
    h.terminal
        .draw(|frame| {
            help_overlay::render(
                frame,
                frame.area(),
                &h.state.help_overlay,
                &h.theme,
                false, // no tmux
            );
        })
        .unwrap();

    // Verify no tmux warning
    let screen = h.screen_text();
    assert!(
        !screen.contains("tmux detected"),
        "No tmux warning when TMUX unset"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// Help Overlay Scrolling
// ═══════════════════════════════════════════════════════════════════════════

/// Covers: AC1 — j/k scrolling in help overlay
#[test]
fn test_e2e_help_overlay_jk_scrolling() {
    let mut h = TestHarness::new();

    h.press_key(DomainKey::Esc);
    h.type_char('?');
    assert_eq!(h.state.help_overlay.scroll_offset, 0);

    // j increments scroll
    let action = h.type_char('j');
    assert!(matches!(action, InputAction::Consumed));
    assert_eq!(h.state.help_overlay.scroll_offset, 1);

    // j again
    h.type_char('j');
    assert_eq!(h.state.help_overlay.scroll_offset, 2);

    // k decrements scroll
    h.type_char('k');
    assert_eq!(h.state.help_overlay.scroll_offset, 1);

    // k at 0 stays at 0
    h.type_char('k');
    h.type_char('k');
    assert_eq!(h.state.help_overlay.scroll_offset, 0);
}

/// Covers: AC1 — Arrow key scrolling in help overlay
#[test]
fn test_e2e_help_overlay_arrow_scrolling() {
    let mut h = TestHarness::new();

    h.press_key(DomainKey::Esc);
    h.type_char('?');
    assert_eq!(h.state.help_overlay.scroll_offset, 0);

    // Down arrow increments scroll
    let action = h.press_key(DomainKey::Down);
    assert!(matches!(action, InputAction::Consumed));
    assert_eq!(h.state.help_overlay.scroll_offset, 1);

    // Up arrow decrements scroll
    h.press_key(DomainKey::Up);
    assert_eq!(h.state.help_overlay.scroll_offset, 0);
}

/// Covers: AC1 — g/G jump to top/bottom in help overlay
#[test]
fn test_e2e_help_overlay_gg_scrolling() {
    let mut h = TestHarness::new();

    h.press_key(DomainKey::Esc);
    h.type_char('?');

    // G scrolls to bottom (large value)
    h.type_char('G');
    assert!(h.state.help_overlay.scroll_offset > 0);

    // g scrolls to top
    h.type_char('g');
    assert_eq!(h.state.help_overlay.scroll_offset, 0);
}

// ═══════════════════════════════════════════════════════════════════════════
// AC4: Version Information
// ═══════════════════════════════════════════════════════════════════════════

/// Covers: AC4 — Version info displayed via command palette
#[test]
fn test_e2e_version_command_via_palette() {
    use rustain::domain::models::palette::{PaletteAction, PaletteEntry, PaletteScope};

    let mut h = TestHarness::new();

    // Open command palette
    h.press_key(DomainKey::CtrlP);
    assert!(h.state.command_palette.active);

    // Inject version entry and select it
    h.state.command_palette.filtered_entries = vec![PaletteEntry {
        name: "version".to_string(),
        description: "Show version".to_string(),
        shortcut: None,
        scope: PaletteScope::All,
        action: PaletteAction::ShowVersion,
    }];
    h.state.command_palette.selected_index = 0;

    // Press Enter to execute
    let action = h.press_key(DomainKey::Enter);
    assert!(matches!(action, InputAction::Consumed));

    // Verify version feedback block exists
    assert!(h.state.feedback_blocks.contains_key("version-info"));
    let fb = &h.state.feedback_blocks["version-info"];
    assert!(fb.message.contains("rustain"));
    assert!(fb.message.contains(env!("CARGO_PKG_VERSION")));
}

// ═══════════════════════════════════════════════════════════════════════════
// AC5: Contextual Status Bar Hints
// ═══════════════════════════════════════════════════════════════════════════

/// Covers: AC5 — Hint visible in status bar for new sessions
#[test]
fn test_e2e_status_bar_hint_for_new_session() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use rustain::adapters::tui::theme::Theme;
    use rustain::adapters::tui::widgets::status_bar;
    use rustain::domain::models::visual::DensityMode;
    use rustain::domain::models::{PermissionMode, StatusState};

    // Render status bar directly with hint
    let backend = TestBackend::new(120, 1);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = Theme::dark();

    terminal
        .draw(|frame| {
            status_bar::render(
                frame,
                frame.area(),
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
                Some("Tip: j/k to scroll, i to type, ? for help"),
                0,
                None,
                None,
                None,
                false,
                None, // daily_budget (Story 7.5)
                None,
                DensityMode::Focus,
                false,
            );
        })
        .unwrap();

    // Verify hint appears in status bar
    let buf = terminal.backend().buffer().clone();
    let row_str: String = (0..120).map(|x| buf[(x, 0)].symbol().to_string()).collect();
    assert!(
        row_str.contains("Tip:"),
        "Status bar should contain hint, got: {}",
        row_str.trim()
    );
    assert!(
        row_str.contains("? for help"),
        "Status bar should contain hint text, got: {}",
        row_str.trim()
    );
}

/// Covers: AC5 — Hint hidden after fade threshold
#[test]
fn test_e2e_status_bar_no_hint_after_fade() {
    let mut h = TestHarness::new();

    // Simulate session 6 (above default fade threshold of 5)
    h.state.session_count = 6;
    h.state.current_hint = None; // Would be set to None by hints::contextual_hint()

    h.render();

    // Verify no hint in status bar
    let buf = h.terminal.backend().buffer().clone();
    let status_text: String = buf
        .content()
        .iter()
        .map(|cell| cell.symbol().chars().next().unwrap_or(' '))
        .collect();
    assert!(
        !status_text.contains("Tip:"),
        "No hint after fade threshold"
    );
}

/// Covers: AC5 — Hint changes based on focus state
#[test]
fn test_e2e_contextual_hint_per_focus() {
    use rustain::adapters::tui::hints::contextual_hint;

    // Test Input focus hint
    let hint = contextual_hint(&FocusState::Input, 1, 5, false);
    assert!(hint.is_some());
    assert!(hint.unwrap().contains("? for help"));

    // Test Chat focus hint
    let hint = contextual_hint(&FocusState::Chat, 1, 5, false);
    assert!(hint.is_some());
    assert!(hint.unwrap().contains("j/k"));

    // Test above threshold
    let hint = contextual_hint(&FocusState::Input, 6, 5, false);
    assert!(hint.is_none());
}

// ═══════════════════════════════════════════════════════════════════════════
// Regression Tests
// ═══════════════════════════════════════════════════════════════════════════

/// Covers: ? key blocked when other Tier-1 overlays are active
#[test]
fn test_e2e_help_blocked_during_command_palette() {
    let mut h = TestHarness::new();

    // Open command palette
    h.press_key(DomainKey::CtrlP);
    assert!(h.state.command_palette.active);
    assert!(matches!(
        h.state.focus,
        FocusState::Overlay(OverlayType::CommandPalette)
    ));

    // ? should be treated as filter text, not open help
    let action = h.type_char('?');
    assert!(matches!(action, InputAction::Consumed));
    assert!(h.state.command_palette.active);
    assert!(!h.state.help_overlay.active);

    // ? should appear in filter text
    assert!(h.state.command_palette.filter_text.contains('?'));
}

/// Covers: Ctrl+X, ? chord opens help
#[test]
fn test_e2e_help_overlay_ctrl_x_chord() {
    let mut h = TestHarness::new();

    // Start in Input
    h.focus_input();

    // Open which-key
    h.press_key(DomainKey::CtrlX);
    assert!(h.state.which_key.active);

    // Press ? — should execute chord and open help
    h.type_char('?');
    assert!(!h.state.which_key.active);
    assert!(h.state.help_overlay.active);
    assert!(matches!(
        h.state.focus,
        FocusState::Overlay(OverlayType::Help)
    ));
}

/// Covers: Help overlay at minimum terminal size
#[test]
fn test_e2e_help_overlay_minimum_terminal_size() {
    let mut h = TestHarness::with_size(80, 24); // Minimum supported

    h.press_key(DomainKey::Esc);
    h.type_char('?');

    // Render help overlay directly - should not panic
    h.terminal
        .draw(|frame| {
            help_overlay::render(frame, frame.area(), &h.state.help_overlay, &h.theme, false);
        })
        .unwrap();

    h.assert_screen_contains("Help", "Help renders at minimum size");
}

/// Covers: Ctrl+P blocked when help overlay is active
#[test]
fn test_e2e_ctrl_p_blocked_in_help_overlay() {
    let mut h = TestHarness::new();

    // Open help
    h.press_key(DomainKey::Esc);
    h.type_char('?');
    assert!(h.state.help_overlay.active);

    // Ctrl+P should be ignored (consumed by help handler or blocked by Tier-1 guard)
    let _action = h.press_key(DomainKey::CtrlP);
    assert!(!h.state.command_palette.active);
    assert!(h.state.help_overlay.active); // Help still active
}
