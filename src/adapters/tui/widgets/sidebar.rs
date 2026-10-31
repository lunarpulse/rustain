//! History sidebar widget for conversation list display.
//!
//! Renders a scrollable list of conversations with:
//! - Title (truncated)
//! - Relative timestamp (e.g., "2m ago", "3h ago", "Apr 9")
//! - Message count
//! - Visual indicators for open/active status

use ratatui::{
    prelude::*,
    style::{Modifier, Style},
    widgets::{Block, Borders, Clear, List, ListItem, ListState},
};
use unicode_width::UnicodeWidthStr;

use crate::adapters::tui::text_geometry as geom;
use crate::domain::models::session_meta::now_unix;
use crate::domain::services::session_index::SessionSummary;

/// Render the history sidebar panel.
///
/// # Arguments
/// * `area` - The rect to render in
/// * `buf` - The buffer to render to
/// * `entries` - The session summaries to display (sorted by updated_at desc)
/// * `selected_index` - The currently selected entry index
/// * `active_conversation_id` - The ID of the conversation in the active tab (for highlighting)
/// * `theme` - The current theme (for colors)
pub fn render_history_panel(
    area: Rect,
    buf: &mut Buffer,
    entries: &[SessionSummary],
    selected_index: usize,
    active_conversation_id: Option<&str>,
    theme: &crate::adapters::tui::theme::Theme,
) {
    // Clear the area first
    Clear.render(area, buf);

    // Create the block with title
    let block = Block::default()
        .title("History")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.colors.fg_secondary));

    let inner_area = block.inner(area);
    block.render(area, buf);

    if entries.is_empty() {
        // Empty state
        let empty_text = Text::from("No conversations").style(
            Style::default()
                .fg(theme.colors.fg_muted)
                .add_modifier(Modifier::ITALIC),
        );
        let empty_paragraph =
            ratatui::widgets::Paragraph::new(empty_text).alignment(Alignment::Center);
        empty_paragraph.render(inner_area, buf);
        return;
    }

    // Build list items — compute now once for consistent timestamps
    let now = now_unix();
    let list_items: Vec<ListItem> = entries
        .iter()
        .enumerate()
        .map(|(idx, entry)| {
            let is_selected = idx == selected_index;
            let is_active = active_conversation_id == Some(entry.conversation_id.as_str());

            // Format the line: [indicator] [fork?] Title (relative_time) [msg_count]
            let indicator = if entry.is_active {
                "● " // Active conversation indicator
            } else if entry.is_open {
                "○ " // Open but not active
            } else {
                "  "
            };
            // Story 4-3a.1 / DF-095: forked conversations get a prefix marker.
            let fork_marker = if entry.has_fork_source { "🔀 " } else { "" };
            let relative_time =
                crate::domain::services::relative_time::format_relative_time(entry.updated_at, now);
            let msg_count = format!("[{}]", entry.message_count);

            // Calculate available width for title using display width
            let time_width = relative_time.width() + 1; // +1 for space
            let count_width = msg_count.width() + 1; // +1 for space
            let indicator_width = 2;
            let fork_marker_width = fork_marker.width();
            let available_width = (inner_area.width as usize)
                .saturating_sub(time_width + count_width + indicator_width + fork_marker_width + 2); // -2 for padding

            let title = if entry.title.is_empty() {
                "(Untitled)"
            } else {
                &entry.title
            };
            let truncated_title = truncate_to_width(title, available_width);

            let line_text = format!(
                "{}{}{} {} {}",
                indicator, fork_marker, truncated_title, relative_time, msg_count
            );

            // Determine style based on state
            let style = if is_active {
                Style::default()
                    .fg(theme.colors.accent)
                    .add_modifier(Modifier::BOLD)
            } else if is_selected {
                Style::default()
                    .fg(theme.colors.fg_primary)
                    .bg(theme.colors.bg_secondary)
            } else {
                Style::default().fg(theme.colors.fg_primary)
            };

            ListItem::new(line_text).style(style)
        })
        .collect();

    // Create the list widget
    let mut list_state = ListState::default();
    list_state.select(Some(selected_index));

    let list = List::new(list_items)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("");

    // Render with state
    ratatui::widgets::StatefulWidget::render(list, inner_area, buf, &mut list_state);
}

/// Truncate text to fit within a given display width.
/// Cuts on whole extended-grapheme boundaries (never inside a ZWJ sequence or
/// before a combining mark), measuring display cells with the shared policy.
/// Budgets below the three-cell `...` marker return the whole-grapheme prefix
/// that fits, with no marker; from three cells up the marker is inside the
/// budget.
pub fn truncate_to_width(text: &str, max_width: usize) -> String {
    const MARKER: &str = "...";
    if geom::cells(text) <= max_width {
        return text.to_string();
    }

    // F18: If max_width can't even fit the ellipsis, return what fits.
    if max_width < MARKER.len() {
        return geom::clip(text, max_width).to_string();
    }

    let mut result = String::with_capacity(max_width);
    result.push_str(geom::clip(text, max_width - MARKER.len()));
    result.push_str(MARKER);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_to_width() {
        assert_eq!(truncate_to_width("Hello", 10), "Hello");
        assert_eq!(truncate_to_width("Hello World", 8), "Hello...");
        assert_eq!(truncate_to_width("Test", 3), "...");
    }

    #[test]
    fn test_truncate_to_width_multibyte() {
        // CJK characters are 2 columns wide each
        let text = "こんにちは世界"; // 7 chars, 14 columns
        let result = truncate_to_width(text, 8);
        // max_width=8, minus 3 for "..." leaves 5 columns for CJK
        // 2 CJK chars = 4 columns (next would exceed 5), so "こん..."
        assert!(result.ends_with("..."));
        assert_eq!(result, "こん...");
    }

    // ── Story 21.11 AC4/K4: display-width correctness ──────────────────────
    // The sidebar title budget is `inner width - 17` for a row with the
    // `just now` timestamp and a one-digit `[3]` count, so each case picks its
    // title budget by choosing the panel width.

    const C: &str = "界";
    const G: &str = "👨\u{200d}👩\u{200d}👧";
    const A: &str = "e\u{301}";

    /// Paint one real history entry in a panel whose title budget is
    /// `budget` cells and return the first list row as painted glyphs.
    fn painted_row(title: &str, budget: usize) -> String {
        let width = u16::try_from(budget + 17 + 2).unwrap();
        let area = Rect::new(0, 0, width, 3);
        let mut buf = Buffer::empty(area);
        let now = now_unix();
        let entry = SessionSummary::new("c1".into(), title.into(), now, now, 3);
        let theme = crate::adapters::tui::theme::Theme::dark();
        render_history_panel(area, &mut buf, &[entry], 0, None, &theme);
        let mut row = String::new();
        let mut x = 1u16;
        while x < width - 1 {
            let symbol = buf.cell((x, 1)).unwrap().symbol();
            row.push_str(symbol);
            x += u16::try_from(geom::cells(symbol).max(1)).unwrap();
        }
        row.trim_end().to_string()
    }

    #[test]
    fn display_width_sidebar_title_cuts_on_whole_graphemes() {
        let cases: Vec<(String, usize, String)> = vec![
            // ASCII positive control: exact fit, fit-minus-one, and longer.
            ("abcdefgh".into(), 8, "abcdefgh".into()),
            ("abcdefghi".into(), 8, "abcde...".into()),
            ("abcdefghijkl".into(), 8, "abcde...".into()),
            // C: 10 cells exact; one cell short keeps the marker inside.
            (C.repeat(5), 10, C.repeat(5)),
            (C.repeat(5), 9, format!("{}...", C.repeat(3))),
            (C.repeat(5), 8, format!("{}...", C.repeat(2))),
            // G: never `👨‍...`.
            (G.repeat(5), 10, G.repeat(5)),
            (G.repeat(5), 9, format!("{}...", G.repeat(3))),
            (G.repeat(5), 8, format!("{}...", G.repeat(2))),
            // A: one cell each, the accent stays with its base.
            (A.repeat(10), 10, A.repeat(10)),
            (A.repeat(10), 9, format!("{}...", A.repeat(6))),
            (A.repeat(10), 8, format!("{}...", A.repeat(5))),
            // Budgets below the marker: whole-grapheme prefix, no marker.
            (format!("{G}xx"), 4, format!("{G}xx")),
            (format!("{G}xx"), 3, "...".into()),
            (format!("{G}xx"), 2, G.into()),
            (format!("{G}xx"), 1, String::new()),
            (format!("{G}xx"), 0, String::new()),
            (format!("{C}x"), 1, String::new()),
            (format!("{A}xx"), 2, format!("{A}x")),
        ];
        for (title, budget, shown) in cases {
            assert_eq!(
                painted_row(&title, budget),
                format!("  {shown} just now [3]").trim_end(),
                "title {title:?} budget {budget}"
            );
        }
    }

    #[test]
    fn display_width_truncate_to_width_never_leaves_a_zwj_fragment() {
        for budget in 0..14 {
            let out = truncate_to_width(&G.repeat(6), budget);
            assert!(
                geom::cells(&out) <= budget.max(3),
                "budget {budget}: {out:?}"
            );
            assert!(!out.ends_with('\u{200d}'), "budget {budget}: {out:?}");
            let body = out.trim_end_matches('.');
            assert_eq!(body.len() % G.len(), 0, "whole families only: {out:?}");
        }
    }
}
