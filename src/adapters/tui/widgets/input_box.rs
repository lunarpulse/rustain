use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::adapters::tui::theme::Theme;
use crate::domain::models::FocusState;

/// Maximum number of visible input lines before scrolling.
pub const MAX_INPUT_LINES: usize = 8;

/// Compute the number of display rows needed for the input area (including borders).
/// Returns at least 3 (1 line + 2 border rows), up to MAX_INPUT_LINES + 2.
///
/// Uses Unicode display width for line wrapping so CJK/emoji input that occupies
/// more terminal columns than its scalar count computes the correct row count (DF-060).
// Covers: FR16, UX-DR76
pub fn input_area_height(input: &str, area_width: u16) -> u16 {
    let inner_width = (area_width as usize).saturating_sub(2).max(1);
    let visual_rows: usize = input
        .split('\n')
        .map(|line| {
            let w = UnicodeWidthStr::width(line);
            if w == 0 { 1 } else { w.div_ceil(inner_width) }
        })
        .sum();
    let visible = visual_rows.clamp(1, MAX_INPUT_LINES);
    (visible as u16) + 2
}

/// Estimate token count from text using character/word heuristic.
/// ~1 token per 4 characters, blended with ~1.3 tokens per word.
// Covers: UX-DR66
pub fn estimate_tokens(text: &str) -> usize {
    let chars = text.chars().count();
    let words = text.split_whitespace().count();
    std::cmp::max(chars / 4, (words as f64 * 1.3) as usize)
}

/// Render the text input area with multi-line support and cursor.
// Covers: FR16, UX-DR76, UX-DR66
pub fn render(
    frame: &mut Frame,
    area: Rect,
    input: &str,
    cursor_pos: usize,
    focus: FocusState,
    theme: &Theme,
    multiline_mode: bool,
    input_scroll_offset: usize,
    image_indicator: Option<&str>,
) {
    let is_focused = focus == FocusState::Input;
    let border_style = if is_focused {
        Style::default().fg(theme.colors.accent)
    } else {
        Style::default().fg(theme.colors.fg_muted)
    };

    // Build title with optional [ML] indicator
    let title = if multiline_mode {
        " Message [ML] "
    } else {
        " Message "
    };

    // Build token estimate as right-aligned title on bottom border
    let token_title = if input.chars().count() > 500 {
        let tokens = estimate_tokens(input);
        format!(" ~{} tokens ", tokens)
    } else {
        String::new()
    };

    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(title);

    // Build bottom border titles: image indicator (left) and token estimate (right)
    // Covers: FR112 (AC1)
    if let Some(indicator) = image_indicator {
        block = block.title_bottom(
            Line::from(format!(" {} ", indicator))
                .style(Style::default().fg(theme.colors.fg_muted)),
        );
    }

    if !token_title.is_empty() {
        block = block.title_bottom(
            Line::from(token_title).right_aligned().style(
                Style::default()
                    .fg(theme.colors.text_hint)
                    .add_modifier(Modifier::ITALIC),
            ),
        );
    }

    // Build multi-line content
    let lines: Vec<&str> = input.split('\n').collect();
    let total_lines = lines.len();

    // Apply scroll offset for input box
    let max_visible = (area.height.saturating_sub(2)) as usize; // subtract borders
    let scroll = if total_lines > max_visible {
        input_scroll_offset.min(total_lines.saturating_sub(max_visible))
    } else {
        0
    };

    let visible_lines: Vec<Line> = lines
        .iter()
        .skip(scroll)
        .take(max_visible.max(1))
        .map(|&line| Line::from(line.to_string()))
        .collect();

    let widget = Paragraph::new(visible_lines)
        .style(
            Style::default()
                .fg(theme.colors.fg_primary)
                .bg(theme.colors.bg_primary),
        )
        .block(block);
    frame.render_widget(widget, area);

    // Set cursor position
    if is_focused {
        // Calculate 2D cursor position from char index
        let (cursor_row, cursor_col) = cursor_to_row_col(input, cursor_pos);
        let visible_row = cursor_row.saturating_sub(scroll);

        let inner_width = area.width.saturating_sub(2);
        let clamped_col = (cursor_col as u16).min(inner_width);
        let clamped_row = (visible_row as u16).min(area.height.saturating_sub(3));

        frame.set_cursor_position((
            area.x.saturating_add(clamped_col).saturating_add(1),
            area.y + 1 + clamped_row,
        ));
    }
}

/// Convert a char-index cursor position to (row, col) within multi-line text.
///
/// `col` is the **display column** (Unicode display width), not the Unicode scalar
/// count. This makes cursor placement correct for CJK and emoji (DF-052).
pub fn cursor_to_row_col(text: &str, cursor_pos: usize) -> (usize, usize) {
    let mut row = 0;
    let mut col = 0;
    for (i, c) in text.chars().enumerate() {
        if i == cursor_pos {
            return (row, col);
        }
        if c == '\n' {
            row += 1;
            col = 0;
        } else {
            col += UnicodeWidthChar::width(c).unwrap_or(1);
        }
    }
    (row, col)
}

/// Convert a (row, display-col) position to a char-index within multi-line text.
///
/// `target_col` is a **display column** to match `cursor_to_row_col`. Wide chars
/// may cause `col` to jump past `target_col`; the function returns at `col >= target_col`.
pub fn row_col_to_cursor(text: &str, target_row: usize, target_col: usize) -> usize {
    let mut row = 0;
    let mut col = 0;
    for (i, c) in text.chars().enumerate() {
        if row == target_row && col >= target_col {
            return i;
        }
        if c == '\n' {
            if row == target_row {
                // target_col is beyond line end — clamp to end of line
                return i;
            }
            row += 1;
            col = 0;
        } else {
            col += UnicodeWidthChar::width(c).unwrap_or(1);
        }
    }
    // Past the end — return text length in chars
    text.chars().count()
}

/// Get the display-column length of a specific row in the text (excluding newline).
///
/// Returns Unicode display width — consistent with `cursor_to_row_col` column values.
pub fn line_len_at_row(text: &str, target_row: usize) -> usize {
    text.split('\n')
        .nth(target_row)
        .map(|line| UnicodeWidthStr::width(line))
        .unwrap_or(0)
}

/// Count the number of lines in the text.
pub fn line_count(text: &str) -> usize {
    text.split('\n').count()
}
