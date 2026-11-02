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
use crate::adapters::tui::text_geometry as geom;
use crate::adapters::tui::theme::Theme;

/// Prefix of the query row.
const QUERY_PREFIX: &str = "/ ";
/// Indent of the excerpt row (its text starts after these cells).
const EXCERPT_INDENT: &str = "    ";

/// Render the cross-search overlay into `area` (expected to be the sidebar
/// column). No-op when `state.active == false`.
/// `show_cursor` controls only hardware cursor placement, not painted cells.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    state: &CrossSearchState,
    theme: &Theme,
    show_cursor: bool,
) {
    if !state.active || area.height < 4 {
        return;
    }

    frame.render_widget(Clear, area);

    let inner_width = area.width.saturating_sub(2) as usize;
    let mut lines: Vec<Line> = Vec::new();

    // Row 0: query input with cursor. The query scrolls so its tail (and the
    // caret after it) stays inside the border: `/ ` prefix, one caret cell.
    let query_budget = inner_width.saturating_sub(geom::cells(QUERY_PREFIX) + 1);
    let shown_query = geom::clip_tail(&state.query, query_budget);
    let query_display = format!("{QUERY_PREFIX}{shown_query}");
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
            let title_text = shorten_to_cells(
                &r.title,
                inner_width.saturating_sub(geom::cells(title_prefix)),
            );
            lines.push(Line::from(vec![
                Span::styled(title_prefix.to_string(), title_style),
                Span::styled(title_text, title_style),
            ]));

            // Row 1: excerpt with matched substring bolded (AC5).
            let excerpt_text: String = shorten_to_cells(
                &r.excerpt,
                inner_width.saturating_sub(geom::cells(EXCERPT_INDENT)),
            );
            let excerpt_spans = build_excerpt_spans(&excerpt_text, &state.query, theme);
            let mut row_spans: Vec<Span<'static>> = vec![Span::raw(EXCERPT_INDENT)];
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
            format!(
                " [{}] open  [{}] close ",
                crate::adapters::tui::keymap::render::key_text(
                    crate::adapters::tui::keymap::table_overlay::OverlayCmd::CrossOpen
                ),
                crate::adapters::tui::keymap::render::key_text(
                    crate::adapters::tui::keymap::table_overlay::OverlayCmd::CrossClose
                )
            ),
            Style::default().fg(theme.colors.fg_muted),
        ));

    let widget = Paragraph::new(lines).block(block).style(
        Style::default()
            .fg(theme.colors.fg_primary)
            .bg(theme.colors.bg_secondary),
    );
    frame.render_widget(widget, area);

    // Cursor at end of the (possibly scrolled) query on row 0.
    if show_cursor {
        let query_cells = geom::cells(QUERY_PREFIX) + geom::cells(shown_query);
        let cursor_col = area.x + 1 + query_cells as u16; // +1 for left border
        let cursor_col = cursor_col.min(area.x + area.width.saturating_sub(2));
        frame.set_cursor_position((cursor_col, area.y + 1));
    }
}

/// Split an excerpt into spans so case-insensitive matches against `query`
/// are rendered with a bold emphasis style (AC5).
///
/// Returns owned spans so the caller can drop the temporary `excerpt` string.
/// Walks `excerpt.to_lowercase()` and `query.to_lowercase()` by byte offset
/// (the logical match ranges are unchanged), then expands each match to whole
/// extended-grapheme bounds before painting — searching `👨` inside a family
/// sequence or `e` before a combining accent paints the complete cluster, not
/// a fragment — and merges painted ranges that overlap so no source byte is
/// emitted twice. Segment spans copy the original excerpt bytes into owned
/// `String`s.
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
    // Logical match ranges (byte offsets into `excerpt`), unchanged matching.
    let mut matches: Vec<(usize, usize)> = Vec::new();
    let mut cursor = 0usize;
    let needle_byte_len = needle_lower.len();
    while let Some(pos) = haystack_lower[cursor..].find(&needle_lower) {
        let abs = cursor + pos;
        if excerpt.is_char_boundary(abs) && excerpt.is_char_boundary(abs + needle_byte_len) {
            matches.push((abs, abs + needle_byte_len));
            cursor = abs + needle_byte_len;
        } else {
            cursor = abs + 1;
        }
        if cursor >= haystack_lower.len() {
            break;
        }
    }
    // Painted ranges: whole graphemes, overlapping ranges merged.
    let mut painted: Vec<(usize, usize)> = Vec::with_capacity(matches.len());
    for (start, end) in matches {
        let (start, end) = geom::expand_to_graphemes(excerpt, start, end);
        match painted.last_mut() {
            Some(last) if start < last.1 => last.1 = last.1.max(end),
            _ => painted.push((start, end)),
        }
    }
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut at = 0usize;
    for (start, end) in painted {
        if start > at {
            spans.push(Span::styled(excerpt[at..start].to_string(), base_style));
        }
        spans.push(Span::styled(excerpt[start..end].to_string(), match_style));
        at = end;
    }
    if at < excerpt.len() {
        spans.push(Span::styled(excerpt[at..].to_string(), base_style));
    }
    if spans.is_empty() {
        spans.push(Span::styled(excerpt.to_string(), base_style));
    }
    spans
}

/// Shorten a UI row to at most `budget` display cells, ending in `...` (inside
/// the budget) when cut.
///
/// This is the widget-side cell budget for text whose source cap lives
/// elsewhere (`session_meta::shorten_text` counts scalars and stays the
/// domain's source helper). It keeps that helper's presentation policy: cut on
/// a whitespace boundary found in the last ten scalars of the kept head when
/// there is one. The cut is always a whole-grapheme boundary; budgets no wider
/// than the marker return the plain whole-grapheme prefix.
pub(crate) fn shorten_to_cells(text: &str, budget: usize) -> String {
    const MARKER: &str = "...";
    if geom::cells(text) <= budget {
        return text.to_string();
    }
    if budget <= MARKER.len() {
        return geom::clip(text, budget).to_string();
    }
    let head = geom::clip(text, budget - MARKER.len());
    let window_start = head.char_indices().rev().nth(9).map_or(0, |(byte, _)| byte);
    let cut = head[window_start..]
        .rfind(char::is_whitespace)
        .map_or(head.len(), |pos| window_start + pos);
    let cut = geom::expand_to_graphemes(head, cut, cut).0;
    format!("{}{MARKER}", &head[..cut])
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

#[cfg(test)]
mod tests {
    use super::*;

    // ── Story 21.11 AC4/K4: display-width correctness ──────────────────────
    // Render-level coverage (title/excerpt/query rows, caret, painted
    // highlights) lives in `conformance_21_11_display_width::k4_shell`; the
    // tests below pin the private primitives it builds on.

    const C: &str = "界";
    const G: &str = "👨\u{200d}👩\u{200d}👧";
    const A: &str = "e\u{301}";

    #[test]
    fn display_width_cross_search_overlapping_highlights_merge_without_duplicates() {
        let theme = Theme::dark();
        // Both ZWJ joiners match, and both sit inside the same family
        // cluster: one painted range, every source byte emitted exactly once.
        let excerpt = format!("x{G}y{G}");
        let spans = build_excerpt_spans(&excerpt, "\u{200d}", &theme);
        let rebuilt: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(rebuilt, excerpt, "no byte duplicated or dropped");
        let bold: Vec<&str> = spans
            .iter()
            .filter(|s| s.style.add_modifier.contains(Modifier::BOLD))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(bold, vec![G, G], "each cluster painted once, whole");
        // Adjacent logical matches stay separate spans with whole clusters.
        let spans = build_excerpt_spans(&A.repeat(2), "e", &theme);
        let rebuilt: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(rebuilt, A.repeat(2));
        assert!(spans.iter().all(|s| s.content.as_ref() == A));
        assert!(
            spans
                .iter()
                .all(|s| s.style.add_modifier.contains(Modifier::BOLD))
        );
    }

    #[test]
    fn display_width_shorten_to_cells_keeps_word_preference_and_whole_graphemes() {
        // Whitespace within the last ten scalars of the kept head wins.
        assert_eq!(
            shorten_to_cells("hello world foo bar baz", 15),
            "hello world..."
        );
        // No whitespace: plain cell cut with the marker inside the budget.
        assert_eq!(shorten_to_cells("abcdefghijklmnop", 10), "abcdefg...");
        assert_eq!(shorten_to_cells("abcdefghij", 10), "abcdefghij");
        // Wide and joined clusters never split.
        assert_eq!(
            shorten_to_cells(&C.repeat(10), 10),
            format!("{}...", C.repeat(3))
        );
        assert_eq!(
            shorten_to_cells(&G.repeat(10), 8),
            format!("{}...", G.repeat(2))
        );
        assert_eq!(shorten_to_cells(&G.repeat(10), 3), G);
        // Budgets no wider than the marker: plain whole-grapheme prefix.
        assert_eq!(shorten_to_cells(&format!("{G}xx"), 2), G);
        assert_eq!(shorten_to_cells(&format!("{G}xx"), 1), "");
        assert_eq!(shorten_to_cells(&format!("{G}xx"), 0), "");
    }
}
