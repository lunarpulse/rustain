use ratatui::Terminal;
use ratatui::backend::TestBackend;

use rustain::adapters::palette_registry::PaletteRegistry;
use rustain::adapters::tui::app::{InputAction, handle_input};
use rustain::adapters::tui::state::{CommandPaletteState, Direction, TuiState, WhichKeyState};
use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::{command_palette, which_key_bar};
use rustain::domain::events::{DomainInputEvent, DomainKey};
use rustain::domain::models::FocusState;
use rustain::domain::models::palette::{PaletteAction, PaletteEntry, PaletteScope};
use rustain::domain::models::visual::OverlayType;

// ============================================================
// Unit: PaletteRegistry (Task 9.1)
// ============================================================

#[test]
fn test_palette_registry_register_and_query_by_scope() {
    let mut reg = PaletteRegistry::new();
    reg.register(PaletteEntry {
        name: "/new".to_string(),
        description: "Start new session".to_string(),
        shortcut: None,
        scope: PaletteScope::SlashCommand,
        action: PaletteAction::ExecuteCommand("new".to_string()),
    });
    reg.register(PaletteEntry {
        name: "gpt-4".to_string(),
        description: "OpenAI model".to_string(),
        shortcut: Some("Ctrl+X, M".to_string()),
        scope: PaletteScope::Model,
        action: PaletteAction::SwitchModel("gpt-4".to_string()),
    });

    assert_eq!(reg.all_entries().len(), 2);
    assert_eq!(reg.entries_for_scope(PaletteScope::SlashCommand).len(), 1);
    assert_eq!(reg.entries_for_scope(PaletteScope::Model).len(), 1);
    assert_eq!(reg.entries_for_scope(PaletteScope::Profile).len(), 0);

    let scopes = reg.populated_scopes();
    assert!(scopes.contains(&PaletteScope::SlashCommand));
    assert!(scopes.contains(&PaletteScope::Model));
    assert!(!scopes.contains(&PaletteScope::Profile));
}

// ============================================================
// Unit: Fuzzy filter (Task 9.2)
// ============================================================

#[test]
fn test_fuzzy_filter_exact_match_ranks_highest() {
    let mut reg = PaletteRegistry::new();
    reg.register(make_entry("/new", "Start new", PaletteScope::SlashCommand));
    reg.register(make_entry(
        "/renew",
        "Renew token",
        PaletteScope::SlashCommand,
    ));

    let results = reg.fuzzy_filter("new", None);
    // /new should rank first (exact prefix) over /renew (substring)
    assert_eq!(results[0].name, "/new");
}

#[test]
fn test_fuzzy_filter_case_insensitive() {
    let mut reg = PaletteRegistry::new();
    reg.register(make_entry(
        "/Deploy",
        "Deploy staging",
        PaletteScope::SlashCommand,
    ));

    let results = reg.fuzzy_filter("deploy", None);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].name, "/Deploy");
}

#[test]
fn test_fuzzy_filter_scoped_filtering() {
    let mut reg = PaletteRegistry::new();
    reg.register(make_entry("/new", "New", PaletteScope::SlashCommand));
    reg.register(make_entry("model-x", "Model", PaletteScope::Model));

    // Scoped to SlashCommand should only return slash commands
    let results = reg.fuzzy_filter("", Some(PaletteScope::SlashCommand));
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].name, "/new");

    // Unscoped returns all
    let results = reg.fuzzy_filter("", None);
    assert_eq!(results.len(), 2);
}

#[test]
fn test_fuzzy_filter_empty_query_returns_all() {
    let mut reg = PaletteRegistry::new();
    reg.register(make_entry("/a", "A", PaletteScope::SlashCommand));
    reg.register(make_entry("/b", "B", PaletteScope::SlashCommand));
    reg.register(make_entry("/c", "C", PaletteScope::SlashCommand));

    let results = reg.fuzzy_filter("", None);
    assert_eq!(results.len(), 3);
}

// ============================================================
// Unit: CommandPaletteState (Task 9.3)
// ============================================================

#[test]
fn test_command_palette_state_open_dismiss() {
    let mut state = CommandPaletteState::new();
    assert!(!state.active);

    state.open(FocusState::Input);
    assert!(state.active);
    assert_eq!(state.previous_focus, Some(FocusState::Input));
    assert!(state.filter_text.is_empty());
    assert_eq!(state.selected_index, 0);

    let prev = state.dismiss();
    assert!(!state.active);
    assert_eq!(prev, Some(FocusState::Input));
    assert!(state.filter_text.is_empty());
}

#[test]
fn test_command_palette_navigate_wraps() {
    let mut state = CommandPaletteState::new();
    state.open(FocusState::Input);
    state.filtered_entries = vec![
        make_entry("/a", "A", PaletteScope::SlashCommand),
        make_entry("/b", "B", PaletteScope::SlashCommand),
        make_entry("/c", "C", PaletteScope::SlashCommand),
    ];

    assert_eq!(state.selected_index, 0);
    state.navigate(Direction::Down);
    assert_eq!(state.selected_index, 1);
    state.navigate(Direction::Down);
    assert_eq!(state.selected_index, 2);
    state.navigate(Direction::Down); // Wrap to 0
    assert_eq!(state.selected_index, 0);
    state.navigate(Direction::Up); // Wrap to 2
    assert_eq!(state.selected_index, 2);
}

#[test]
fn test_command_palette_selected_entry() {
    let mut state = CommandPaletteState::new();
    state.open(FocusState::Input);
    state.filtered_entries = vec![make_entry("/new", "New", PaletteScope::SlashCommand)];

    let selected = state.selected();
    assert!(selected.is_some());
    assert_eq!(selected.unwrap().name, "/new");
}

#[test]
fn test_command_palette_execute_selected() {
    let mut state = CommandPaletteState::new();
    state.open(FocusState::Input);
    state.filtered_entries = vec![PaletteEntry {
        name: "/new".to_string(),
        description: "New".to_string(),
        shortcut: None,
        scope: PaletteScope::SlashCommand,
        action: PaletteAction::ExecuteCommand("new".to_string()),
    }];

    let action = state.execute_selected();
    assert_eq!(
        action,
        Some(PaletteAction::ExecuteCommand("new".to_string()))
    );
}

#[test]
fn test_command_palette_filter_update_resets_selection() {
    let mut state = CommandPaletteState::new();
    state.open(FocusState::Input);
    state.selected_index = 5;

    state.update_filter(
        "test".to_string(),
        vec![make_entry("/test", "Test", PaletteScope::SlashCommand)],
    );

    assert_eq!(state.filter_text, "test");
    assert_eq!(state.selected_index, 0);
    assert_eq!(state.scroll_offset, 0);
}

#[test]
fn test_command_palette_scroll_behavior() {
    let mut state = CommandPaletteState::new();
    state.open(FocusState::Input);
    // Create more entries than MAX_VISIBLE (12)
    state.filtered_entries = (0..20)
        .map(|i| {
            make_entry(
                &format!("/cmd{}", i),
                &format!("Cmd {}", i),
                PaletteScope::SlashCommand,
            )
        })
        .collect();

    // Navigate past visible window
    for _ in 0..15 {
        state.navigate(Direction::Down);
    }
    assert!(state.scroll_offset > 0);
}

// ============================================================
// Unit: WhichKeyState (Task 9.4)
// ============================================================

#[test]
fn test_which_key_open_dismiss() {
    let mut wk = WhichKeyState::new();
    assert!(!wk.active);

    wk.open(FocusState::Input);
    assert!(wk.active);
    assert!(wk.started_at.is_some());
    assert_eq!(wk.previous_focus, Some(FocusState::Input));

    let prev = wk.dismiss();
    assert!(!wk.active);
    assert!(wk.started_at.is_none());
    assert_eq!(prev, Some(FocusState::Input));
}

#[test]
fn test_which_key_valid_chord_lookup() {
    let wk = WhichKeyState::new();

    // All 10 chords should be registered
    assert!(wk.lookup_chord('p').is_some());
    assert!(wk.lookup_chord('m').is_some());
    assert!(wk.lookup_chord('a').is_some());
    assert!(wk.lookup_chord('s').is_some());
    assert!(wk.lookup_chord('l').is_some());
    assert!(wk.lookup_chord('t').is_some());
    assert!(wk.lookup_chord('u').is_some());
    assert!(wk.lookup_chord('w').is_some());
    assert!(wk.lookup_chord('d').is_some());
    assert!(wk.lookup_chord('?').is_some());
}

#[test]
fn test_which_key_invalid_chord_lookup() {
    let wk = WhichKeyState::new();
    assert!(wk.lookup_chord('z').is_none());
    assert!(wk.lookup_chord('q').is_none());
    assert!(wk.lookup_chord('1').is_none());
}

#[test]
fn test_which_key_timeout_check() {
    let mut wk = WhichKeyState::new();
    wk.open(FocusState::Input);

    // Just opened — should NOT be timed out with generous timeout
    assert!(!wk.is_timed_out(2000));

    // timeout_ms == 0 means "no timeout" — should never fire (P12 fix)
    assert!(!wk.is_timed_out(0));
}

#[test]
fn test_which_key_case_insensitive_lookup() {
    let wk = WhichKeyState::new();
    // Uppercase should work too
    assert!(wk.lookup_chord('P').is_some());
    assert!(wk.lookup_chord('M').is_some());
}

// ============================================================
// Integration: Ctrl+P opens palette (Task 9.5)
// ============================================================

#[test]
fn test_ctrl_p_opens_palette_from_input() {
    let mut state = TuiState::new(80, 24);
    assert_eq!(state.focus, FocusState::Input);

    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlP));
    assert_eq!(action, InputAction::Consumed);
    assert_eq!(
        state.focus,
        FocusState::Overlay(OverlayType::CommandPalette)
    );
    assert!(state.command_palette.active);
}

#[test]
fn test_ctrl_p_opens_palette_from_chat() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;

    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlP));
    assert_eq!(action, InputAction::Consumed);
    assert_eq!(
        state.focus,
        FocusState::Overlay(OverlayType::CommandPalette)
    );
    assert!(state.command_palette.active);
    assert_eq!(state.command_palette.previous_focus, Some(FocusState::Chat));
}

#[test]
fn test_ctrl_p_not_from_existing_overlay() {
    let mut state = TuiState::new(80, 24);
    state.help_overlay.open(FocusState::Chat);
    state.focus = FocusState::Overlay(OverlayType::Help);

    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlP));
    // Help overlay consumes all input — Ctrl+P is swallowed, palette not opened
    assert_eq!(action, InputAction::Consumed);
    assert!(!state.command_palette.active);
}

// ============================================================
// Integration: Ctrl+P opens palette from Sidebar (Task 9.5)
// ============================================================

#[test]
fn test_ctrl_p_opens_palette_from_sidebar() {
    use rustain::domain::models::visual::PanelType;
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Sidebar {
        panel: PanelType::History,
        selected: 0,
    };

    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlP));
    assert!(state.command_palette.active);
    assert_eq!(
        state.focus,
        FocusState::Overlay(OverlayType::CommandPalette)
    );
}

// ============================================================
// Integration: Ctrl+X opens which-key (Task 9.6)
// ============================================================

#[test]
fn test_ctrl_x_opens_which_key_from_input() {
    let mut state = TuiState::new(80, 24);
    assert_eq!(state.focus, FocusState::Input);

    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlX));
    assert_eq!(action, InputAction::Consumed);
    assert_eq!(state.focus, FocusState::Overlay(OverlayType::WhichKey));
    assert!(state.which_key.active);
}

#[test]
fn test_ctrl_x_opens_which_key_from_chat() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;

    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlX));
    assert!(state.which_key.active);
    assert_eq!(state.focus, FocusState::Overlay(OverlayType::WhichKey));
}

#[test]
fn test_ctrl_x_opens_which_key_from_sidebar() {
    use rustain::domain::models::visual::PanelType;
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Sidebar {
        panel: PanelType::History,
        selected: 0,
    };

    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlX));
    assert!(state.which_key.active);
    assert_eq!(state.focus, FocusState::Overlay(OverlayType::WhichKey));
}

#[test]
fn test_ctrl_x_second_key_dispatches_chord() {
    let mut state = TuiState::new(80, 24);
    // Open which-key
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlX));
    assert!(state.which_key.active);

    // Press a valid chord key
    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('m'));
    assert_eq!(action, InputAction::Consumed);
    assert!(!state.which_key.active); // Dismissed
    // Focus restored to Input
    assert_eq!(state.focus, FocusState::Input);
    // Feedback block created for Noop chord
    assert!(state.feedback_blocks.contains_key("chord-m"));
}

#[test]
fn test_ctrl_x_invalid_key_dismisses() {
    let mut state = TuiState::new(80, 24);
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlX));
    assert!(state.which_key.active);

    // Press an invalid key
    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('z'));
    assert_eq!(action, InputAction::Consumed);
    assert!(!state.which_key.active); // Dismissed
    // No feedback block for invalid keys
    assert!(!state.feedback_blocks.contains_key("chord-z"));
}

// ============================================================
// Integration: Palette scoped prefix filtering (Task 9.7)
// ============================================================

#[test]
fn test_palette_scope_prefix_detection() {
    let mut state = TuiState::new(80, 24);
    // Open palette
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlP));
    assert!(state.command_palette.active);

    // Type '/' prefix
    handle_input(&mut state, &DomainInputEvent::KeyPress('/'));
    assert_eq!(
        state.command_palette.current_scope,
        Some(PaletteScope::SlashCommand)
    );
    assert_eq!(state.command_palette.filter_text, "/");

    // Type more characters
    handle_input(&mut state, &DomainInputEvent::KeyPress('n'));
    assert_eq!(state.command_palette.filter_text, "/n");
}

#[test]
fn test_palette_scope_prefix_at() {
    let mut state = TuiState::new(80, 24);
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlP));
    handle_input(&mut state, &DomainInputEvent::KeyPress('@'));
    assert_eq!(
        state.command_palette.current_scope,
        Some(PaletteScope::FileMention)
    );
}

#[test]
fn test_palette_scope_prefix_colon() {
    let mut state = TuiState::new(80, 24);
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlP));
    handle_input(&mut state, &DomainInputEvent::KeyPress(':'));
    assert_eq!(
        state.command_palette.current_scope,
        Some(PaletteScope::Model)
    );
}

// ============================================================
// Integration: Stream-safe overlay (Task 9.8)
// ============================================================

#[test]
fn test_palette_opens_without_interrupting_stream() {
    let mut state = TuiState::new(80, 24);
    // Simulate streaming state
    state.status = rustain::domain::models::StatusState::Streaming;

    // Ctrl+P should still open palette
    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlP));
    assert_eq!(action, InputAction::Consumed);
    assert!(state.command_palette.active);
    // Streaming status should NOT be affected (it's the event loop's concern, not app.rs)
    assert_eq!(
        state.status,
        rustain::domain::models::StatusState::Streaming
    );
}

// ============================================================
// Rendering: Command palette overlay (Task 9.9)
// ============================================================

#[test]
fn test_command_palette_renders_centered() {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = Theme::dark();

    let mut state = CommandPaletteState::new();
    state.open(FocusState::Input);
    state.filter_text = "test".to_string();

    terminal
        .draw(|frame| {
            let area = frame.area();
            command_palette::render(frame, area, &state, &theme);
        })
        .unwrap();

    let buffer = terminal.backend().buffer().clone();
    assert!(buffer_contains_text(&buffer, "Command Palette"));
}

#[test]
fn test_command_palette_renders_with_entries() {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = Theme::dark();

    let mut state = CommandPaletteState::new();
    state.open(FocusState::Input);
    state.filtered_entries = vec![make_entry(
        "/new",
        "Start new session",
        PaletteScope::SlashCommand,
    )];

    terminal
        .draw(|frame| {
            let area = frame.area();
            command_palette::render(frame, area, &state, &theme);
        })
        .unwrap();

    let buffer = terminal.backend().buffer().clone();
    assert!(buffer_contains_text(&buffer, "/new"));
    assert!(buffer_contains_text(&buffer, "Start new session"));
}

#[test]
fn test_command_palette_renders_no_matches() {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = Theme::dark();

    let mut state = CommandPaletteState::new();
    state.open(FocusState::Input);
    state.filter_text = "xyz".to_string();
    // No entries

    terminal
        .draw(|frame| {
            let area = frame.area();
            command_palette::render(frame, area, &state, &theme);
        })
        .unwrap();

    let buffer = terminal.backend().buffer().clone();
    assert!(buffer_contains_text(&buffer, "No matches"));
}

// ============================================================
// Rendering: Which-key bar (Task 9.10)
// ============================================================

#[test]
fn test_which_key_bar_renders_chord_labels() {
    let backend = TestBackend::new(120, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = Theme::dark();

    let mut state = WhichKeyState::new();
    state.open(FocusState::Input);

    terminal
        .draw(|frame| {
            let area = frame.area();
            which_key_bar::render(frame, area, &state, &theme);
        })
        .unwrap();

    let buffer = terminal.backend().buffer().clone();
    assert!(buffer_contains_text(&buffer, "rofile"));
    assert!(buffer_contains_text(&buffer, "odel"));
    assert!(buffer_contains_text(&buffer, "dapter"));
}

// ============================================================
// Regression: Story 3-2 autocomplete not broken (Task 9.11)
// ============================================================

#[test]
fn test_adding_palette_state_doesnt_break_autocomplete() {
    let mut state = TuiState::new(80, 24);

    // Autocomplete should still work normally
    assert!(!state.autocomplete.active);
    assert!(!state.command_palette.active);
    assert!(!state.which_key.active);

    // Type '/' to trigger autocomplete (simulating what event_loop does)
    handle_input(&mut state, &DomainInputEvent::KeyPress('/'));
    // The autocomplete activation happens in event_loop, not app.rs
    // But the state machine should be intact
    assert_eq!(state.input_buffer, "/");
}

// ============================================================
// Regression: Only one overlay at a time (Task 9.12)
// ============================================================

#[test]
fn test_ctrl_p_dismisses_autocomplete_before_opening() {
    let mut state = TuiState::new(80, 24);

    // Simulate autocomplete being active
    state.autocomplete.active = true;

    // Ctrl+P should dismiss autocomplete and open palette
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlP));
    assert!(!state.autocomplete.active);
    assert!(state.command_palette.active);
}

#[test]
fn test_ctrl_x_dismisses_autocomplete_before_opening() {
    let mut state = TuiState::new(80, 24);
    state.autocomplete.active = true;

    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlX));
    assert!(!state.autocomplete.active);
    assert!(state.which_key.active);
}

#[test]
fn test_ctrl_p_dismisses_reverse_search() {
    let mut state = TuiState::new(80, 24);
    state.reverse_search.active = true;
    state.focus = FocusState::Overlay(OverlayType::ReverseSearch);

    // P16: Ctrl+P from ReverseSearch (Tier-2 overlay) should dismiss reverse_search
    // and open command palette.
    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlP));
    assert_eq!(action, InputAction::Consumed);
    assert!(state.command_palette.active);
    assert!(!state.reverse_search.active);
}

#[test]
fn test_ctrl_x_while_reverse_search_opens_which_key() {
    let mut state = TuiState::new(80, 24);
    state.reverse_search.active = true;
    state.focus = FocusState::Overlay(OverlayType::ReverseSearch);

    // P16: Ctrl+X from ReverseSearch (Tier-2 overlay) should dismiss reverse_search
    // and open which-key.
    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlX));
    assert_eq!(action, InputAction::Consumed);
    assert!(state.which_key.active);
    assert!(!state.reverse_search.active);
}

// ============================================================
// Edge: Rapid dismiss + re-open (Task 9.13)
// ============================================================

#[test]
fn test_palette_rapid_dismiss_reopen() {
    let mut state = TuiState::new(80, 24);

    // Open
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlP));
    assert!(state.command_palette.active);

    // Dismiss via Esc
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Esc));
    assert!(!state.command_palette.active);
    assert_eq!(state.focus, FocusState::Input);

    // Re-open immediately
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlP));
    assert!(state.command_palette.active);
    assert_eq!(
        state.focus,
        FocusState::Overlay(OverlayType::CommandPalette)
    );
}

// ============================================================
// Edge: Which-key timeout at boundary (Task 9.14)
// ============================================================

#[test]
fn test_which_key_timeout_at_zero() {
    let mut wk = WhichKeyState::new();
    wk.open(FocusState::Input);

    // P12: timeout_ms == 0 means "no timeout" — should NOT immediately fire
    assert!(!wk.is_timed_out(0));
}

#[test]
fn test_which_key_not_timed_out_when_inactive() {
    let wk = WhichKeyState::new();
    // Not opened — should not be timed out
    assert!(!wk.is_timed_out(0));
}

// ============================================================
// Additional: Palette Esc dismiss restores focus
// ============================================================

#[test]
fn test_palette_esc_restores_chat_focus() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;

    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlP));
    assert_eq!(
        state.focus,
        FocusState::Overlay(OverlayType::CommandPalette)
    );

    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Esc));
    assert_eq!(state.focus, FocusState::Chat);
    assert!(!state.command_palette.active);
}

#[test]
fn test_palette_enter_executes_command() {
    let mut state = TuiState::new(80, 24);
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlP));

    // Set up a command entry
    state.command_palette.filtered_entries = vec![PaletteEntry {
        name: "/new".to_string(),
        description: "New".to_string(),
        shortcut: None,
        scope: PaletteScope::SlashCommand,
        action: PaletteAction::ExecuteCommand("new".to_string()),
    }];

    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Enter));
    assert_eq!(
        action,
        InputAction::ExecuteCommand {
            name: "new".to_string(),
            args: None
        }
    );
    assert!(!state.command_palette.active);
}

#[test]
fn test_palette_backspace_clears_scope_when_prefix_deleted() {
    let mut state = TuiState::new(80, 24);
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlP));

    // Type scope prefix
    handle_input(&mut state, &DomainInputEvent::KeyPress('/'));
    assert_eq!(
        state.command_palette.current_scope,
        Some(PaletteScope::SlashCommand)
    );

    // Backspace removes prefix
    handle_input(
        &mut state,
        &DomainInputEvent::SpecialKey(DomainKey::Backspace),
    );
    assert!(state.command_palette.filter_text.is_empty());
    assert_eq!(state.command_palette.current_scope, None);
}

// ============================================================
// Helpers
// ============================================================

fn make_entry(name: &str, desc: &str, scope: PaletteScope) -> PaletteEntry {
    PaletteEntry {
        name: name.to_string(),
        description: desc.to_string(),
        shortcut: None,
        scope,
        action: PaletteAction::Noop,
    }
}

fn buffer_contains_text(buffer: &ratatui::buffer::Buffer, text: &str) -> bool {
    let content: String = buffer
        .content()
        .iter()
        .map(|cell| cell.symbol().chars().next().unwrap_or(' '))
        .collect();
    content.contains(text)
}
