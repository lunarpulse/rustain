use ratatui::Terminal;
use ratatui::backend::TestBackend;

use rustain::adapters::palette_registry::PaletteRegistry;
use rustain::adapters::tui::app::{InputAction, handle_input};
use rustain::adapters::tui::keymap::{AvailCtx, Context, Host, Key, Resolved, resolve};
use rustain::adapters::tui::presentation::{self, Surface};
use rustain::adapters::tui::state::{CommandPaletteState, Direction, TuiState};
use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::{command_palette, which_key_bar};
use rustain::domain::events::{DomainInputEvent, DomainKey};
use rustain::domain::models::FocusState;
use rustain::domain::models::palette::{PaletteAction, PaletteEntry, PaletteScope};
use rustain::domain::models::visual::{DensityMode, OverlayType};

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
        action: PaletteAction::ExecuteCommand("new".to_string(), None),
    });
    reg.register(PaletteEntry {
        name: "gpt-4".to_string(),
        description: "OpenAI model".to_string(),
        shortcut: Some("Ctrl+X, M".to_string()),
        scope: PaletteScope::Model,
        action: PaletteAction::SwitchModel {
            provider_id: "openai".to_string(),
            model_id: "gpt-4".to_string(),
        },
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
fn test_command_palette_frontdoor_open_dismiss() {
    let mut state = TuiState::new(80, 24);
    assert!(!presentation::is_open(&state, Surface::CommandPalette));
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlP));
    assert!(presentation::is_open(&state, Surface::CommandPalette));
    assert_eq!(state.focus, FocusState::Input);
    assert!(state.command_palette.filter_text.is_empty());
    assert_eq!(state.command_palette.selected_index, 0);
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Esc));
    assert!(!presentation::is_open(&state, Surface::CommandPalette));
    assert_eq!(state.focus, FocusState::Input);
    assert!(state.command_palette.filter_text.is_empty());
}

#[test]
fn test_command_palette_reset_and_clear_only_change_payload() {
    let mut state = CommandPaletteState::new();
    state.filter_text = "stale".to_owned();
    state.filtered_entries = vec![make_entry("/old", "Old", PaletteScope::SlashCommand)];
    state.selected_index = 3;
    state.scroll_offset = 2;
    state.current_scope = Some(PaletteScope::SlashCommand);
    let epoch = state.open_epoch;
    state.reset();
    assert_eq!(state.open_epoch, epoch.wrapping_add(1));
    assert!(state.filter_text.is_empty());
    assert!(state.filtered_entries.is_empty());
    assert_eq!(state.selected_index, 0);
    assert_eq!(state.scroll_offset, 0);
    assert_eq!(state.current_scope, None);
    state.filter_text.push('x');
    state.clear();
    assert!(state.filter_text.is_empty());
    assert_eq!(state.open_epoch, epoch.wrapping_add(1));
}

#[test]
fn test_command_palette_navigate_wraps() {
    let mut state = CommandPaletteState::new();
    state.reset();
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
    state.reset();
    state.filtered_entries = vec![make_entry("/new", "New", PaletteScope::SlashCommand)];

    let selected = state.selected();
    assert!(selected.is_some());
    assert_eq!(selected.unwrap().name, "/new");
}

#[test]
fn test_command_palette_execute_selected() {
    let mut state = CommandPaletteState::new();
    state.reset();
    state.filtered_entries = vec![PaletteEntry {
        name: "/new".to_string(),
        description: "New".to_string(),
        shortcut: None,
        scope: PaletteScope::SlashCommand,
        action: PaletteAction::ExecuteCommand("new".to_string(), None),
    }];

    let action = state.execute_selected();
    assert_eq!(
        action,
        Some(PaletteAction::ExecuteCommand("new".to_string(), None))
    );
}

#[test]
fn test_command_palette_filter_update_resets_selection() {
    let mut state = CommandPaletteState::new();
    state.reset();
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
    state.reset();
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
// Integration: Routed pending leader (Task 9.4)
// ============================================================

#[test]
fn test_which_key_open_dismiss() {
    let mut state = TuiState::new(80, 24);
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlX));
    assert_eq!(state.pending_leader, Some(Context::Input));
    assert_eq!(state.focus, FocusState::Input);
    assert!(state.modal_stack.is_empty());
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Esc));
    assert!(state.pending_leader.is_none());
    assert_eq!(state.focus, FocusState::Input);
}

#[test]
fn test_which_key_valid_chord_lookup() {
    // Every shipped local chord resolves in the registry's Leader context.
    // (`d` is retired locally; `h` is History — Story 21.3 AC2.)
    for c in ['p', 'm', 'a', 's', 'l', 't', 'u', 'w', 'h', '?'] {
        assert!(
            matches!(
                resolve(Context::Leader, Host::Local, &[Key::Char(c)]),
                Resolved::Hit(_)
            ),
            "Ctrl+X {c} must resolve locally"
        );
    }
}

#[test]
fn test_which_key_invalid_chord_lookup() {
    let hit = |c: char| {
        matches!(
            resolve(Context::Leader, Host::Local, &[Key::Char(c)]),
            Resolved::Hit(_)
        )
    };
    assert!(!hit('z'));
    assert!(!hit('j'));
    // Story 21.1 slice S2: `Ctrl+X q` brings the oldest decision back, so `q`
    // is a bound chord now (it was the invalid-key probe before).
    assert!(hit('q'));
    assert!(!hit('1'));
}

#[test]
fn test_which_key_case_insensitive_lookup() {
    // Uppercase should work too
    for c in ['P', 'M'] {
        assert!(matches!(
            resolve(Context::Leader, Host::Local, &[Key::Char(c)]),
            Resolved::Hit(_)
        ));
    }
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
    assert_eq!(state.focus, FocusState::Input);
    assert!(presentation::is_open(&state, Surface::CommandPalette));
}

#[test]
fn test_ctrl_p_opens_palette_from_chat() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;

    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlP));
    assert_eq!(action, InputAction::Consumed);
    assert_eq!(state.focus, FocusState::Chat);
    assert!(presentation::is_open(&state, Surface::CommandPalette));
}

#[test]
fn test_ctrl_p_pushes_over_existing_help_frame() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;
    handle_input(&mut state, &DomainInputEvent::KeyPress('?'));
    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlP));
    assert_eq!(action, InputAction::Consumed);
    assert!(presentation::is_open(&state, Surface::CommandPalette));
    assert_eq!(state.modal_stack.len(), 2);
    assert_eq!(
        state.modal_stack.top().unwrap().surface,
        Surface::CommandPalette
    );
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Esc));
    assert_eq!(state.modal_stack.top().unwrap().surface, Surface::Help);
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
    assert!(presentation::is_open(&state, Surface::CommandPalette));
    assert_eq!(
        state.focus,
        FocusState::Sidebar {
            panel: PanelType::History,
            selected: 0,
        }
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
    assert_eq!(state.focus, FocusState::Input);
    assert_eq!(state.pending_leader, Some(Context::Input));
}

#[test]
fn test_ctrl_x_opens_which_key_from_chat() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;

    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlX));
    assert_eq!(state.pending_leader, Some(Context::Chat));
    assert_eq!(state.focus, FocusState::Chat);
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
    assert!(state.pending_leader.is_some());
    assert_eq!(
        state.focus,
        FocusState::Sidebar {
            panel: PanelType::History,
            selected: 0,
        }
    );
}

#[test]
fn test_ctrl_x_second_key_dispatches_chord() {
    let mut state = TuiState::new(80, 24);
    // Open which-key
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlX));
    assert!(state.pending_leader.is_some());

    // Press 'm' — wired to OpenModelSelector (Story 7.2)
    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('m'));
    assert_eq!(action, InputAction::OpenModelSelector);
    assert!(state.pending_leader.is_none());
    assert_eq!(state.focus, FocusState::Input);
}

#[test]
fn test_ctrl_x_from_palette_routes_help_and_preserves_query_on_back() {
    let mut state = TuiState::new(80, 24);
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlP));
    handle_input(&mut state, &DomainInputEvent::KeyPress('m'));
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlX));
    assert_eq!(state.pending_leader, Some(Context::CommandPalette));
    assert_eq!(
        state.modal_stack.top().unwrap().surface,
        Surface::CommandPalette
    );
    handle_input(&mut state, &DomainInputEvent::KeyPress('?'));
    assert!(state.pending_leader.is_none());
    assert_eq!(state.modal_stack.top().unwrap().surface, Surface::Help);
    assert_eq!(state.command_palette.filter_text, "m");
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Esc));
    assert_eq!(
        state.modal_stack.top().unwrap().surface,
        Surface::CommandPalette
    );
    assert_eq!(state.command_palette.filter_text, "m");
}

#[test]
fn test_ctrl_x_invalid_key_dismisses() {
    let mut state = TuiState::new(80, 24);
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlX));
    assert!(state.pending_leader.is_some());

    // Press an unbound key: consumed, dismissed, and the miss is announced once
    // (Story 21.3 AC2: no silent swallow).
    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('z'));
    assert_eq!(action, InputAction::Consumed);
    assert!(state.pending_leader.is_none()); // Dismissed
    assert_eq!(state.focus, FocusState::Input);
    let notices: Vec<_> = state
        .notifications
        .records()
        .filter(|record| {
            record.source == rustain::adapters::tui::notifications::NoticeSource::Chord
        })
        .map(|record| record.message.clone())
        .collect();
    assert_eq!(notices, ["No binding for Ctrl+X z in Input"]);
}

// ============================================================
// Integration: Palette scoped prefix filtering (Task 9.7)
// ============================================================

#[test]
fn test_palette_scope_prefix_detection() {
    let mut state = TuiState::new(80, 24);
    // Open palette
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlP));
    assert!(presentation::is_open(&state, Surface::CommandPalette));

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
    assert!(presentation::is_open(&state, Surface::CommandPalette));
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
fn test_command_palette_renders_caller_geometry() {
    use rustain::adapters::tui::{
        layout::compute_layout,
        notification_log::LogFacts,
        presentation::{Shell, compose, geometry},
    };

    for (width, height, full) in [(60, 16, true), (130, 30, false)] {
        let mut state = TuiState::new(width, height);
        if full {
            state.input_buffer = ["draft"; 8].join("\n");
        }
        handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlP));
        state.command_palette.filter_text = "test".to_owned();
        state.command_palette.filtered_entries = vec![make_entry(
            "/test",
            "body-witness",
            PaletteScope::SlashCommand,
        )];
        let area = ratatui::layout::Rect::new(0, 0, width, height);
        let layout = compute_layout(
            area,
            &state.theme,
            &state.input_buffer,
            1,
            false,
            DensityMode::Focus,
        )
        .unwrap();
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
                    |_| {},
                    |_| {},
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(rect.x, rect.y)].symbol(), "┌");
        assert_eq!(buffer[(rect.right() - 1, rect.bottom() - 1)].symbol(), "┘");
        assert!(buffer_contains_text(buffer, "Command Palette"));
        assert!(buffer_contains_text(buffer, "> test_"));
        assert!(buffer_contains_text(buffer, "body-witness"));
        assert!(buffer_contains_text(buffer, "close"));
        assert_eq!(
            buffer[(layout.status_bar.x, layout.status_bar.y)].symbol(),
            " "
        );
    }
}

#[test]
fn test_command_palette_renders_with_entries() {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = Theme::dark();

    let mut state = CommandPaletteState::new();
    state.reset();
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
    state.reset();
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

    terminal
        .draw(|frame| {
            let area = frame.area();
            which_key_bar::render(
                frame,
                area,
                &theme,
                &AvailCtx::local(120, DensityMode::Focus),
                None,
            );
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
    assert!(!presentation::is_open(&state, Surface::CommandPalette));
    assert!(state.pending_leader.is_none());

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
    assert!(presentation::is_open(&state, Surface::CommandPalette));
}

#[test]
fn test_ctrl_x_dismisses_autocomplete_before_opening() {
    let mut state = TuiState::new(80, 24);
    state.autocomplete.active = true;

    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlX));
    assert!(!state.autocomplete.active);
    assert!(state.pending_leader.is_some());
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
    assert!(presentation::is_open(&state, Surface::CommandPalette));
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
    assert!(state.pending_leader.is_some());
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
    assert!(presentation::is_open(&state, Surface::CommandPalette));

    // Dismiss via Esc
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Esc));
    assert!(!presentation::is_open(&state, Surface::CommandPalette));
    assert_eq!(state.focus, FocusState::Input);

    // Re-open immediately
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlP));
    assert!(presentation::is_open(&state, Surface::CommandPalette));
    assert_eq!(state.focus, FocusState::Input);
}

// ============================================================
// Additional: Palette Esc dismiss restores focus
// ============================================================

#[test]
fn test_palette_esc_restores_chat_focus() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;

    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlP));
    assert!(presentation::is_open(&state, Surface::CommandPalette));
    assert_eq!(state.focus, FocusState::Chat);

    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Esc));
    assert_eq!(state.focus, FocusState::Chat);
    assert!(!presentation::is_open(&state, Surface::CommandPalette));
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
        action: PaletteAction::ExecuteCommand("new".to_string(), None),
    }];

    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Enter));
    assert_eq!(
        action,
        InputAction::ExecuteCommand {
            name: "new".to_string(),
            args: None
        }
    );
    assert!(!presentation::is_open(&state, Surface::CommandPalette));
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

// Story 21.1 AC5: `/notifications` is offered by the palette in both hosts.
#[test]
fn story21_1_palette_offers_notifications_in_both_hosts() {
    use rustain::adapters::builtin_commands::CommandHost;
    use rustain::adapters::command_registry::CommandRegistry;

    let commands = CommandRegistry::new();
    for host in [CommandHost::Local, CommandHost::Attached] {
        let mut reg = PaletteRegistry::for_host(host);
        reg.populate_from_command_registry(&commands);
        let entry = reg
            .all_entries()
            .into_iter()
            .find(|entry| entry.name == "/notifications")
            .unwrap_or_else(|| panic!("{host:?} palette hides /notifications"));
        assert_eq!(entry.scope, PaletteScope::SlashCommand, "{host:?}");
        assert_eq!(
            entry.action,
            PaletteAction::ExecuteCommand("notifications".to_string(), None),
            "{host:?}"
        );
    }
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
