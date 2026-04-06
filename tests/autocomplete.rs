use ratatui::prelude::*;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use rustain::adapters::tui::state::{AutocompleteState, Direction};
use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::autocomplete_popup;
use rustain::domain::models::autocomplete::{AutocompleteKind, AutocompleteSuggestion};

// === AutocompleteState unit tests (Story 3.2, Task 1) ===

// Covers: AC1, AC3 — open populates suggestions and sets active
#[test]
fn test_autocomplete_open() {
    let mut ac = AutocompleteState::new();
    assert!(!ac.active);

    let suggestions = vec![
        AutocompleteSuggestion::SlashCommand {
            name: "new".to_string(),
            description: "Start a new session".to_string(),
        },
        AutocompleteSuggestion::SlashCommand {
            name: "help".to_string(),
            description: "Show help".to_string(),
        },
    ];
    ac.open(AutocompleteKind::SlashCommand, 0, suggestions);

    assert!(ac.active);
    assert_eq!(ac.kind, AutocompleteKind::SlashCommand);
    assert_eq!(ac.trigger_position, 0);
    assert_eq!(ac.suggestions.len(), 2);
    assert_eq!(ac.selected_index, 0);
    assert_eq!(ac.filter_text, "");
}

// Covers: AC1 — filter updates suggestions and resets selection
#[test]
fn test_autocomplete_update_filter() {
    let mut ac = AutocompleteState::new();
    let initial = vec![
        AutocompleteSuggestion::SlashCommand {
            name: "new".to_string(),
            description: "New session".to_string(),
        },
        AutocompleteSuggestion::SlashCommand {
            name: "help".to_string(),
            description: "Help".to_string(),
        },
    ];
    ac.open(AutocompleteKind::SlashCommand, 0, initial);
    ac.selected_index = 1;

    // Filter to only "new"
    let filtered = vec![AutocompleteSuggestion::SlashCommand {
        name: "new".to_string(),
        description: "New session".to_string(),
    }];
    ac.update_filter("ne".to_string(), filtered);

    assert_eq!(ac.filter_text, "ne");
    assert_eq!(ac.suggestions.len(), 1);
    assert_eq!(ac.selected_index, 0); // Reset on filter
}

// Covers: AC1 — navigate wraps around (Down at end → first, Up at first → last)
#[test]
fn test_autocomplete_navigate_wrap_around() {
    let mut ac = AutocompleteState::new();
    let suggestions = vec![
        AutocompleteSuggestion::SlashCommand {
            name: "a".to_string(),
            description: "".to_string(),
        },
        AutocompleteSuggestion::SlashCommand {
            name: "b".to_string(),
            description: "".to_string(),
        },
        AutocompleteSuggestion::SlashCommand {
            name: "c".to_string(),
            description: "".to_string(),
        },
    ];
    ac.open(AutocompleteKind::SlashCommand, 0, suggestions);

    // Navigate down to last
    ac.navigate(Direction::Down); // → 1
    ac.navigate(Direction::Down); // → 2
    assert_eq!(ac.selected_index, 2);

    // Wrap around to first
    ac.navigate(Direction::Down); // → 0
    assert_eq!(ac.selected_index, 0);

    // Wrap around up from first to last
    ac.navigate(Direction::Up); // → 2
    assert_eq!(ac.selected_index, 2);
}

// Covers: AC1, AC3 — navigate with empty suggestions is no-op
#[test]
fn test_autocomplete_navigate_empty() {
    let mut ac = AutocompleteState::new();
    ac.open(AutocompleteKind::SlashCommand, 0, vec![]);
    ac.navigate(Direction::Down);
    assert_eq!(ac.selected_index, 0);
    ac.navigate(Direction::Up);
    assert_eq!(ac.selected_index, 0);
}

// Covers: AC1 — selected returns correct suggestion
#[test]
fn test_autocomplete_selected() {
    let mut ac = AutocompleteState::new();
    let suggestions = vec![
        AutocompleteSuggestion::SlashCommand {
            name: "new".to_string(),
            description: "New session".to_string(),
        },
        AutocompleteSuggestion::SlashCommand {
            name: "help".to_string(),
            description: "Help".to_string(),
        },
    ];
    ac.open(AutocompleteKind::SlashCommand, 0, suggestions);

    let selected = ac.selected().unwrap();
    assert_eq!(
        *selected,
        AutocompleteSuggestion::SlashCommand {
            name: "new".to_string(),
            description: "New session".to_string(),
        }
    );

    ac.navigate(Direction::Down);
    let selected = ac.selected().unwrap();
    assert_eq!(
        *selected,
        AutocompleteSuggestion::SlashCommand {
            name: "help".to_string(),
            description: "Help".to_string(),
        }
    );
}

// Covers: AC1 — selected returns None when empty
#[test]
fn test_autocomplete_selected_empty() {
    let ac = AutocompleteState::new();
    assert!(ac.selected().is_none());
}

// Covers: AC1 — dismiss resets all state
#[test]
fn test_autocomplete_dismiss() {
    let mut ac = AutocompleteState::new();
    let suggestions = vec![AutocompleteSuggestion::SlashCommand {
        name: "new".to_string(),
        description: "New session".to_string(),
    }];
    ac.open(AutocompleteKind::SlashCommand, 0, suggestions);
    ac.selected_index = 0;
    ac.filter_text = "ne".to_string();

    ac.dismiss();

    assert!(!ac.active);
    assert!(ac.filter_text.is_empty());
    assert!(ac.suggestions.is_empty());
    assert_eq!(ac.selected_index, 0);
    assert_eq!(ac.scroll_offset, 0);
}

// Covers: AC5 — empty state after filtering to no matches
#[test]
fn test_autocomplete_no_matches_state() {
    let mut ac = AutocompleteState::new();
    let suggestions = vec![AutocompleteSuggestion::SlashCommand {
        name: "new".to_string(),
        description: "New session".to_string(),
    }];
    ac.open(AutocompleteKind::SlashCommand, 0, suggestions);

    // Filter to something with no matches
    ac.update_filter("xyz".to_string(), vec![]);
    assert!(ac.suggestions.is_empty());
    assert!(ac.selected().is_none());
}

// Covers: AC3 — file mention kind
#[test]
fn test_autocomplete_file_mention() {
    let mut ac = AutocompleteState::new();
    let suggestions = vec![
        AutocompleteSuggestion::FilePath {
            path: "src/main.rs".to_string(),
            is_dir: false,
        },
        AutocompleteSuggestion::FilePath {
            path: "src/lib.rs".to_string(),
            is_dir: false,
        },
    ];
    ac.open(AutocompleteKind::FileMention, 5, suggestions);

    assert!(ac.active);
    assert_eq!(ac.kind, AutocompleteKind::FileMention);
    assert_eq!(ac.trigger_position, 5);
    assert_eq!(ac.suggestions.len(), 2);
}

// Covers: AC1 — scroll offset adjustment during navigation
#[test]
fn test_autocomplete_scroll_offset() {
    let mut ac = AutocompleteState::new();
    let suggestions: Vec<AutocompleteSuggestion> = (0..15)
        .map(|i| AutocompleteSuggestion::SlashCommand {
            name: format!("cmd{}", i),
            description: "".to_string(),
        })
        .collect();
    ac.open(AutocompleteKind::SlashCommand, 0, suggestions);

    // Navigate past visible window (max 8 visible)
    for _ in 0..9 {
        ac.navigate(Direction::Down);
    }
    // selected_index = 9, scroll_offset should be >= 2 to keep it visible
    assert!(ac.scroll_offset > 0);
    assert!(ac.selected_index >= ac.scroll_offset);
    assert!(ac.selected_index < ac.scroll_offset + 8);
}

// === Popup rendering tests (Story 3.2, Task 5) ===

fn make_theme() -> Theme {
    Theme::dark()
}

// Covers: AC1, AC5 — popup renders with suggestions
#[test]
fn test_popup_renders_with_items() {
    let mut ac = AutocompleteState::new();
    ac.open(
        AutocompleteKind::SlashCommand,
        0,
        vec![
            AutocompleteSuggestion::SlashCommand {
                name: "new".to_string(),
                description: "New session".to_string(),
            },
            AutocompleteSuggestion::SlashCommand {
                name: "help".to_string(),
                description: "Show help".to_string(),
            },
        ],
    );

    let theme = make_theme();
    let backend = TestBackend::new(60, 20);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal
        .draw(|frame| {
            let input_area = Rect::new(0, 16, 60, 4);
            autocomplete_popup::render(frame, input_area, &ac, &theme);
        })
        .unwrap();

    // Verify popup rendered something (non-empty buffer area above input)
    let buf = terminal.backend().buffer();
    // Check that the popup area contains command names
    let buf_str = buffer_to_string(buf);
    assert!(buf_str.contains("/new"), "Popup should show /new command");
    assert!(buf_str.contains("/help"), "Popup should show /help command");
}

// Covers: AC5 — "No matches" state renders correctly
#[test]
fn test_popup_renders_no_matches() {
    let mut ac = AutocompleteState::new();
    ac.open(AutocompleteKind::SlashCommand, 0, vec![]);

    let theme = make_theme();
    let backend = TestBackend::new(60, 20);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal
        .draw(|frame| {
            let input_area = Rect::new(0, 16, 60, 4);
            autocomplete_popup::render(frame, input_area, &ac, &theme);
        })
        .unwrap();

    let buf = terminal.backend().buffer();
    let buf_str = buffer_to_string(buf);
    assert!(
        buf_str.contains("No matches"),
        "Popup should show 'No matches'"
    );
}

// Covers: AC3 — file mention popup renders file paths
#[test]
fn test_popup_renders_file_paths() {
    let mut ac = AutocompleteState::new();
    ac.open(
        AutocompleteKind::FileMention,
        5,
        vec![
            AutocompleteSuggestion::FilePath {
                path: "src/main.rs".to_string(),
                is_dir: false,
            },
            AutocompleteSuggestion::FilePath {
                path: "src/".to_string(),
                is_dir: true,
            },
        ],
    );

    let theme = make_theme();
    let backend = TestBackend::new(60, 20);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal
        .draw(|frame| {
            let input_area = Rect::new(0, 16, 60, 4);
            autocomplete_popup::render(frame, input_area, &ac, &theme);
        })
        .unwrap();

    let buf = terminal.backend().buffer();
    let buf_str = buffer_to_string(buf);
    assert!(buf_str.contains("src/main.rs"), "Should show file path");
}

// Covers: AC1 — popup does not render when inactive
#[test]
fn test_popup_does_not_render_when_inactive() {
    let ac = AutocompleteState::new();
    assert!(!ac.active);

    let theme = make_theme();
    let backend = TestBackend::new(60, 20);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal
        .draw(|frame| {
            let input_area = Rect::new(0, 16, 60, 4);
            autocomplete_popup::render(frame, input_area, &ac, &theme);
        })
        .unwrap();

    // Buffer should be empty (all spaces)
    let buf = terminal.backend().buffer();
    let buf_str = buffer_to_string(buf);
    let non_space = buf_str.chars().filter(|c| !c.is_whitespace()).count();
    assert_eq!(non_space, 0, "Inactive popup should not render anything");
}

// Covers: AC1, AC5 — long list shows scroll indicator in title
#[test]
fn test_popup_long_list_scroll_indicator() {
    let mut ac = AutocompleteState::new();
    let suggestions: Vec<AutocompleteSuggestion> = (0..15)
        .map(|i| AutocompleteSuggestion::SlashCommand {
            name: format!("cmd{}", i),
            description: "".to_string(),
        })
        .collect();
    ac.open(AutocompleteKind::SlashCommand, 0, suggestions);

    let theme = make_theme();
    let backend = TestBackend::new(60, 20);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal
        .draw(|frame| {
            let input_area = Rect::new(0, 16, 60, 4);
            autocomplete_popup::render(frame, input_area, &ac, &theme);
        })
        .unwrap();

    let buf = terminal.backend().buffer();
    let buf_str = buffer_to_string(buf);
    // Should show [1/15] indicator
    assert!(
        buf_str.contains("1/15"),
        "Long list should show scroll indicator"
    );
}

/// Helper: convert test backend buffer to string.
fn buffer_to_string(buf: &ratatui::buffer::Buffer) -> String {
    let mut s = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            s.push_str(buf.cell((x, y)).map_or(" ", |c| c.symbol()));
        }
        s.push('\n');
    }
    s
}
