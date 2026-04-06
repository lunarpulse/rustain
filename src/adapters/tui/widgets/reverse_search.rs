use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::adapters::tui::state::ReverseSearchState;
use crate::adapters::tui::theme::Theme;

/// Maximum visible matches in the reverse search overlay.
const MAX_VISIBLE_MATCHES: usize = 5;

/// Render the reverse search overlay above the input box.
// Covers: UX-DR74 (reverse search overlay)
pub fn render(frame: &mut Frame, area: Rect, state: &ReverseSearchState, theme: &Theme) {
    if !state.active {
        return;
    }

    let mut lines: Vec<Line> = Vec::new();

    // Query line
    lines.push(Line::from(vec![
        Span::styled(
            "(reverse-search): ",
            Style::default()
                .fg(theme.colors.fg_muted)
                .add_modifier(Modifier::ITALIC),
        ),
        Span::styled(
            state.query.clone(),
            Style::default()
                .fg(theme.colors.fg_primary)
                .add_modifier(Modifier::BOLD),
        ),
    ]));

    // Match lines
    if state.matches.is_empty() && !state.query.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no matches",
            Style::default().fg(theme.colors.fg_muted),
        )));
    } else {
        let scroll = state.selected_match.saturating_sub(MAX_VISIBLE_MATCHES - 1);
        for (i, (_idx, entry)) in state.matches.iter().skip(scroll).take(MAX_VISIBLE_MATCHES).enumerate() {
            let display_idx = i + scroll;
            let prefix = if display_idx == state.selected_match {
                " ▸ "
            } else {
                "   "
            };
            let style = if display_idx == state.selected_match {
                Style::default().fg(theme.colors.accent)
            } else {
                Style::default().fg(theme.colors.fg_secondary)
            };
            // Truncate entry to fit using char-boundary-safe slicing
            let max_len = area.width.saturating_sub(6) as usize;
            let display: String = if entry.chars().count() > max_len {
                let truncated: String = entry.chars().take(max_len.saturating_sub(1)).collect();
                format!("{}…", truncated)
            } else {
                entry.clone()
            };
            lines.push(Line::from(vec![
                Span::styled(prefix.to_string(), style),
                Span::styled(format!("\"{}\"", display), style),
            ]));
        }
    }

    let height = lines.len() as u16 + 2; // +2 for borders
    let overlay_height = height.min(area.height);
    let overlay_area = Rect {
        x: area.x,
        y: area.y + area.height - overlay_height,
        width: area.width,
        height: overlay_height,
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.colors.accent))
        .title(" Reverse Search ");

    let widget = Paragraph::new(lines).block(block).style(
        Style::default()
            .fg(theme.colors.fg_primary)
            .bg(theme.colors.bg_secondary),
    );

    frame.render_widget(Clear, overlay_area);
    frame.render_widget(widget, overlay_area);

    // Show cursor at end of query on the query line (row 1 inside border)
    // Covers: UX-DR74 (AC4 filter input cursor)
    let prefix_len = "(reverse-search): ".chars().count();
    let query_len = state.query.chars().count();
    let cursor_col = (prefix_len + query_len) as u16 + 1; // +1 for left border
    let cursor_x = (overlay_area.x + cursor_col).min(overlay_area.x + overlay_area.width.saturating_sub(2));
    let cursor_y = overlay_area.y + 1; // +1 for top border
    frame.set_cursor_position((cursor_x, cursor_y));
}
