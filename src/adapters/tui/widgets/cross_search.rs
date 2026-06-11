//! Cross-conversation search overlay widget (Story 4-4 AC5).
//!
//! Renders **inside the sidebar column** as a 3-row-per-result vertical stack
//! (title / excerpt / timestamp). The horizontal single-line layout was
//! rejected in party-mode review — a ~30-column sidebar cannot fit a title +
//! 60-char excerpt + timestamp, so the vertical stack is the only layout
//! that honors the "within sidebar column" constraint.
//!
//! Pure render — no state mutation. Input dispatch lives in `app.rs`.
// Covers: Story 4-4 AC5 (UX-DR87), amendments 3

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::adapters::tui::state::CrossSearchState;
use crate::adapters::tui::theme::Theme;
use crate::domain::models::shorten_text;

/// Render the cross-search overlay into `area` (expected to be the sidebar
/// column). No-op when `state.active == false`.
pub fn render(frame: &mut Frame, area: Rect, state: &CrossSearchState, theme: &Theme) {
    if !state.active || area.height < 4 {
        return;
    }

    frame.render_widget(Clear, area);

    let inner_width = area.width.saturating_sub(2) as usize;
    let mut lines: Vec<Line> = Vec::new();

    // Row 0: query input with cursor.
    let query_display = format!("/ {}", state.query);
    lines.push(Line::from(Span::styled(
        query_display,
        Style::default()
            .fg(theme.colors.fg_primary)
            .add_modifier(Modifier::BOLD),
    )));

    // Separator row.
    lines.push(Line::from(Span::styled(
        "─".repeat(inner_width),
        Style::default().fg(theme.colors.fg_muted),
    )));

    // Loading state (reviewer Fix — no silent delay).
    if state.running {
        lines.push(Line::from(Span::styled(
            "Searching…".to_string(),
            Style::default()
                .fg(theme.colors.fg_muted)
                .add_modifier(Modifier::ITALIC),
        )));
    } else if state.query.len() < 2 {
        lines.push(Line::from(Span::styled(
            "Type at least 2 characters".to_string(),
            Style::default()
                .fg(theme.colors.fg_muted)
                .add_modifier(Modifier::ITALIC),
        )));
    } else if state.results.is_empty() {
        // Story 4-4 AC7 — spec-mandated wording.
        lines.push(Line::from(Span::styled(
            "No matches found".to_string(),
            Style::default().fg(theme.colors.fg_muted),
        )));
    } else {
        // Each result occupies 3 rows + 1 separator row.
        let selected = state.selected.min(state.results.len() - 1);
        for (i, r) in state.results.iter().enumerate() {
            let is_sel = i == selected;
            let title_prefix = if is_sel { "▸ " } else { "  " };
            let title_style = if is_sel {
                Style::default()
                    .fg(theme.colors.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(theme.colors.fg_primary)
                    .add_modifier(Modifier::BOLD)
            };
            let title_text = shorten_text(&r.title, inner_width.saturating_sub(4));
            lines.push(Line::from(vec![
                Span::styled(title_prefix.to_string(), title_style),
                Span::styled(title_text.to_string(), title_style),
            ]));

            // Row 1: excerpt with matched substring bolded (AC5).
            let excerpt_text: String =
                shorten_text(&r.excerpt, inner_width.saturating_sub(4)).to_string();
            let excerpt_spans = build_excerpt_spans(&excerpt_text, &state.query, theme);
            let mut row_spans: Vec<Span<'static>> = vec![Span::raw("    ")];
            row_spans.extend(excerpt_spans);
            lines.push(Line::from(row_spans));

            // Row 2: timestamp (muted, relative)
            lines.push(Line::from(Span::styled(
                format!("    {}", relative_time(r.timestamp)),
                Style::default().fg(theme.colors.fg_muted),
            )));
        }

        // Truncation hint — spec-mandated wording (AC5).
        // When both flags are set the count hint wins because it's more actionable.
        if state.truncated_by_count {
            lines.push(Line::from(Span::styled(
                "Showing 20 most recent matches — refine query for more".to_string(),
                Style::default()
                    .fg(theme.colors.warning)
                    .add_modifier(Modifier::ITALIC),
            )));
        } else if state.truncated_by_time {
            lines.push(Line::from(Span::styled(
                "Scan stopped after 200 ms — refine query for full coverage".to_string(),
                Style::default()
                    .fg(theme.colors.warning)
                    .add_modifier(Modifier::ITALIC),
            )));
        }
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.colors.accent))
        .title(Span::styled(
            " Cross-Search ",
            Style::default()
                .fg(theme.colors.accent)
                .add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Span::styled(
            " [Enter] open  [Esc] close ",
            Style::default().fg(theme.colors.fg_muted),
        ));

    let widget = Paragraph::new(lines).block(block).style(
        Style::default()
            .fg(theme.colors.fg_primary)
            .bg(theme.colors.bg_secondary),
    );
    frame.render_widget(widget, area);

    // Cursor at end of query on row 0.
    let cursor_col = area.x + 3 + state.query.chars().count() as u16;
    let cursor_col = cursor_col.min(area.x + area.width.saturating_sub(2));
    frame.set_cursor_position((cursor_col, area.y + 1));
}

/// Split an excerpt into spans so case-insensitive matches against `query`
/// are rendered with a bold emphasis style (AC5).
///
/// Returns owned spans so the caller can drop the temporary `excerpt` string.
/// Pure ASCII-safe: walks `excerpt.to_lowercase()` and `query.to_lowercase()`
/// byte-by-byte, rebuilding segment spans by copying the original excerpt
/// bytes into owned `String`s.
fn build_excerpt_spans(excerpt: &str, query: &str, theme: &Theme) -> Vec<Span<'static>> {
    // AC5 row 2: excerpt rendered in `fg_muted` with matched substring bolded.
    // Second-audit Fix 3: was `fg_secondary`, corrected to spec color.
    let base_style = Style::default().fg(theme.colors.fg_muted);
    let match_style = base_style.add_modifier(Modifier::BOLD);
    if query.is_empty() || excerpt.is_empty() {
        return vec![Span::styled(excerpt.to_string(), base_style)];
    }
    let haystack_lower = excerpt.to_lowercase();
    let needle_lower = query.to_lowercase();
    if needle_lower.is_empty() {
        return vec![Span::styled(excerpt.to_string(), base_style)];
    }
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut cursor = 0usize;
    let needle_byte_len = needle_lower.len();
    while let Some(pos) = haystack_lower[cursor..].find(&needle_lower) {
        let abs = cursor + pos;
        if excerpt.is_char_boundary(abs) && excerpt.is_char_boundary(abs + needle_byte_len) {
            if abs > cursor {
                spans.push(Span::styled(excerpt[cursor..abs].to_string(), base_style));
            }
            spans.push(Span::styled(
                excerpt[abs..abs + needle_byte_len].to_string(),
                match_style,
            ));
            cursor = abs + needle_byte_len;
        } else {
            cursor = abs + 1;
        }
        if cursor >= haystack_lower.len() {
            break;
        }
    }
    if cursor < excerpt.len() {
        spans.push(Span::styled(excerpt[cursor..].to_string(), base_style));
    }
    if spans.is_empty() {
        spans.push(Span::styled(excerpt.to_string(), base_style));
    }
    spans
}

/// Lightweight relative time formatter — "just now", "Nm ago", "Nh ago", "Nd ago".
fn relative_time(timestamp: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let delta = (now - timestamp).max(0);
    if delta < 60 {
        "just now".to_string()
    } else if delta < 3600 {
        format!("{}m ago", delta / 60)
    } else if delta < 86400 {
        format!("{}h ago", delta / 3600)
    } else {
        format!("{}d ago", delta / 86400)
    }
}
