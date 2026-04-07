// Covers: FR16 (multi-line input), AC1 (input box widget tests)
//! Dedicated rendering tests for the input box widget.
//! Tests rendering output via TestBackend, complementing keyboard.rs (state tests).

use ratatui::Terminal;
use ratatui::backend::TestBackend;

use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::input_box;
use rustain::domain::models::FocusState;

mod common;

fn render_input_box(
    width: u16,
    height: u16,
    input: &str,
    cursor_pos: usize,
    focus: FocusState,
) -> Terminal<TestBackend> {
    render_input_box_ml(width, height, input, cursor_pos, focus, false, 0)
}

fn render_input_box_ml(
    width: u16,
    height: u16,
    input: &str,
    cursor_pos: usize,
    focus: FocusState,
    multiline_mode: bool,
    input_scroll_offset: usize,
) -> Terminal<TestBackend> {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = Theme::dark();

    terminal
        .draw(|frame| {
            let area = frame.area();
            input_box::render(
                frame,
                area,
                input,
                cursor_pos,
                focus,
                &theme,
                multiline_mode,
                input_scroll_offset,
            );
        })
        .unwrap();

    terminal
}

// Covers: FR16 (input controls), AC1 — empty input renders cursor area
#[test]
fn test_input_box_empty_focused() {
    let terminal = render_input_box(40, 3, "", 0, FocusState::Input);
    let text = common::buffer_text(&terminal);

    // Should render the " Message " border title
    assert!(
        text.contains("Message"),
        "Input box should show 'Message' border title"
    );
}

// Covers: FR16, AC1 — typed text appears in buffer
#[test]
fn test_input_box_typed_text_visible() {
    let terminal = render_input_box(40, 3, "Hello world", 11, FocusState::Input);
    let text = common::buffer_text(&terminal);

    assert!(
        text.contains("Hello world"),
        "Typed text should appear in rendered buffer, got: {:?}",
        text.trim()
    );
}

// Covers: FR16, AC1 — cursor position matches text length
#[test]
fn test_input_box_cursor_position() {
    let mut terminal = render_input_box(40, 3, "abc", 3, FocusState::Input);

    // The cursor should be set at (area.x + cursor_pos + 1, area.y + 1) due to border
    // With area starting at (0,0) and border offset of 1, cursor at col 4, row 1
    let cursor = terminal.get_cursor_position().unwrap();
    assert_eq!(
        cursor.x, 4,
        "Cursor X should be at border_offset + cursor_pos (1 + 3)"
    );
    assert_eq!(cursor.y, 1, "Cursor Y should be at row 1 (inside border)");
}

// Covers: FR16, AC1 — backspace removes last character (state-level, verified in render)
#[test]
fn test_input_box_after_backspace() {
    // Simulate state after backspace: "ab" with cursor at 2
    let terminal = render_input_box(40, 3, "ab", 2, FocusState::Input);
    let text = common::buffer_text(&terminal);

    assert!(
        text.contains("ab"),
        "Buffer should show remaining text after backspace"
    );
    assert!(!text.contains("abc"), "Deleted char should not appear");
}

// Covers: FR16, AC1 — multi-byte character renders without panic
#[test]
fn test_input_box_multibyte_emoji() {
    let terminal = render_input_box(40, 3, "Hello 🦀", 7, FocusState::Input);
    let text = common::buffer_text(&terminal);

    assert!(
        text.contains("Hello"),
        "ASCII portion should render, got: {:?}",
        text.trim()
    );
    // Emoji may render as replacement or wide char depending on backend;
    // the key assertion is no panic occurred.
}

// Covers: FR16, AC1 — multi-byte CJK renders without panic
#[test]
fn test_input_box_multibyte_cjk() {
    let terminal = render_input_box(40, 3, "你好世界", 4, FocusState::Input);
    let text = common::buffer_text(&terminal);

    // CJK characters are double-width in terminal; they should appear
    assert!(
        text.contains("你") || text.contains("好"),
        "CJK characters should render without panic"
    );
}

// Covers: FR16, AC1 — long text handling (text wider than input box)
#[test]
fn test_input_box_long_text_no_panic() {
    let long_text = "a".repeat(100); // 100 chars in a 40-wide box
    let mut terminal = render_input_box(40, 3, &long_text, 100, FocusState::Input);
    let text = common::buffer_text(&terminal);

    // Should render some portion without panicking
    assert!(text.contains("aaa"), "Some text should be visible");

    // Cursor should be clamped to inner width
    let cursor = terminal.get_cursor_position().unwrap();
    let inner_width = 40 - 2; // border takes 2 cols
    assert!(
        cursor.x <= inner_width + 1,
        "Cursor should be clamped to inner width"
    );
}

// Covers: FR16, AC1 — focused vs unfocused style difference
#[test]
fn test_input_box_focused_vs_unfocused_style() {
    let theme = Theme::dark();
    let accent = theme.colors.accent;
    let muted = theme.colors.fg_muted;

    // Focused: border should use accent color
    let focused_terminal = render_input_box(40, 3, "", 0, FocusState::Input);
    let focused_buf = focused_terminal.backend().buffer().clone();
    // Check top-left border character (row 0, col 0 — border corner)
    let focused_border_style = focused_buf
        .cell(ratatui::prelude::Position::new(0, 0))
        .map(|c| c.fg)
        .unwrap();
    assert_eq!(
        focused_border_style, accent,
        "Focused input box border should use accent color"
    );

    // Unfocused: border should use muted color
    let unfocused_terminal = render_input_box(40, 3, "", 0, FocusState::Chat);
    let unfocused_buf = unfocused_terminal.backend().buffer().clone();
    let unfocused_border_style = unfocused_buf
        .cell(ratatui::prelude::Position::new(0, 0))
        .map(|c| c.fg)
        .unwrap();
    assert_eq!(
        unfocused_border_style, muted,
        "Unfocused input box border should use fg_muted color"
    );
}

// === Multi-line rendering tests ===

// Covers: FR16, UX-DR76 — multi-line text renders multiple lines
#[test]
fn test_input_box_multiline_two_lines() {
    let terminal = render_input_box_ml(40, 5, "hello\nworld", 11, FocusState::Input, false, 0);
    let text = common::buffer_text(&terminal);
    assert!(text.contains("hello"), "First line should render");
    assert!(text.contains("world"), "Second line should render");
}

// Covers: FR16, UX-DR76 — five lines render correctly
#[test]
fn test_input_box_multiline_five_lines() {
    let input = "line1\nline2\nline3\nline4\nline5";
    let terminal = render_input_box_ml(40, 9, input, 29, FocusState::Input, false, 0);
    let text = common::buffer_text(&terminal);
    assert!(text.contains("line1"), "Line 1 visible");
    assert!(text.contains("line5"), "Line 5 visible");
}

// Covers: FR16, UX-DR76 — max height with scroll (8+ lines)
#[test]
fn test_input_box_multiline_max_height_scroll() {
    let input = (0..10)
        .map(|i| format!("line{}", i))
        .collect::<Vec<_>>()
        .join("\n");
    // Area height of 10 = 8 visible lines + 2 borders
    let terminal = render_input_box_ml(40, 10, &input, 0, FocusState::Input, false, 0);
    let text = common::buffer_text(&terminal);
    // First lines should be visible
    assert!(text.contains("line0"), "First line visible without scroll");
}

// Covers: UX-DR76 — ML indicator shown when multiline_mode is active
#[test]
fn test_input_box_ml_indicator_shown() {
    let terminal = render_input_box_ml(40, 3, "", 0, FocusState::Input, true, 0);
    let text = common::buffer_text(&terminal);
    assert!(
        text.contains("[ML]"),
        "ML indicator should show when multiline_mode is active"
    );
}

// Covers: UX-DR76 — ML indicator NOT shown when multiline_mode is off
#[test]
fn test_input_box_ml_indicator_hidden() {
    let terminal = render_input_box_ml(40, 3, "", 0, FocusState::Input, false, 0);
    let text = common::buffer_text(&terminal);
    assert!(
        !text.contains("[ML]"),
        "ML indicator should not show when multiline_mode is off"
    );
}

// Covers: UX-DR66 — token estimate shown for long input
#[test]
fn test_input_box_token_estimate_shown() {
    let long_text = "a".repeat(600); // > 500 chars
    let terminal = render_input_box_ml(80, 3, &long_text, 600, FocusState::Input, false, 0);
    let text = common::buffer_text(&terminal);
    assert!(
        text.contains("tokens"),
        "Token estimate should display for input > 500 chars"
    );
}

// Covers: UX-DR66 — token estimate NOT shown for short input
#[test]
fn test_input_box_token_estimate_hidden_short() {
    let terminal = render_input_box_ml(40, 3, "hello", 5, FocusState::Input, false, 0);
    let text = common::buffer_text(&terminal);
    assert!(
        !text.contains("tokens"),
        "Token estimate should not display for short input"
    );
}

// Covers: UX-DR66 — token estimate function
#[test]
fn test_estimate_tokens_500_chars() {
    let text = "a".repeat(500);
    let tokens = input_box::estimate_tokens(&text);
    assert!(
        tokens >= 100 && tokens <= 200,
        "500 chars should estimate ~125 tokens, got {}",
        tokens
    );
}

#[test]
fn test_estimate_tokens_1000_chars() {
    let text = "a".repeat(1000);
    let tokens = input_box::estimate_tokens(&text);
    assert!(
        tokens >= 200 && tokens <= 400,
        "1000 chars should estimate ~250 tokens, got {}",
        tokens
    );
}

#[test]
fn test_estimate_tokens_empty() {
    assert_eq!(input_box::estimate_tokens(""), 0);
}

// Covers: FR16 — cursor_to_row_col helper
#[test]
fn test_cursor_to_row_col_single_line() {
    assert_eq!(input_box::cursor_to_row_col("hello", 0), (0, 0));
    assert_eq!(input_box::cursor_to_row_col("hello", 3), (0, 3));
    assert_eq!(input_box::cursor_to_row_col("hello", 5), (0, 5));
}

#[test]
fn test_cursor_to_row_col_multi_line() {
    assert_eq!(input_box::cursor_to_row_col("ab\ncd\nef", 0), (0, 0));
    assert_eq!(input_box::cursor_to_row_col("ab\ncd\nef", 2), (0, 2)); // at '\n'
    assert_eq!(input_box::cursor_to_row_col("ab\ncd\nef", 3), (1, 0)); // start of line 2
    assert_eq!(input_box::cursor_to_row_col("ab\ncd\nef", 5), (1, 2)); // at second '\n'
    assert_eq!(input_box::cursor_to_row_col("ab\ncd\nef", 6), (2, 0)); // start of line 3
}

// Covers: FR16 — input_area_height function
#[test]
fn test_input_area_height_single_line() {
    assert_eq!(input_box::input_area_height("hello", 80), 3);
}

#[test]
fn test_input_area_height_multi_line() {
    assert_eq!(input_box::input_area_height("a\nb\nc", 80), 5); // 3 lines + 2 borders
}

#[test]
fn test_input_area_height_capped() {
    let input = (0..20)
        .map(|i| format!("line{}", i))
        .collect::<Vec<_>>()
        .join("\n");
    let height = input_box::input_area_height(&input, 80);
    assert_eq!(
        height, 10,
        "Height should be capped at MAX_INPUT_LINES(8) + 2 borders"
    );
}
