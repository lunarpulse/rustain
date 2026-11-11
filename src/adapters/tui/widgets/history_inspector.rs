//! The paint of the History window (Story 21.7 A21): the saved conversations as one row each,
//! `<marker><title> · <age> · <n> msgs`, the selected one marked, a conversation that left the
//! list shown as a muted placeholder, and the generated rail. One function lays the rows out for
//! both the viewport and the paint, and records what it painted ([`Fit`]) so the keys clamp
//! against exactly these rows and a key whose hint was not painted is inert (height == paint).
//!
//! The row format is the one the sidebar panel painted: `●` the active conversation, `○` one
//! open in another tab, a fork marker, the title cut on whole graphemes so the age and the
//! message count always survive. Titles are untrusted text: they are sanitized here, at the
//! span builder.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear};

use crate::adapters::tui::history_window::{Fit, HistoryWindowState, UNTITLED};
use crate::adapters::tui::keymap::Context as KeyContext;
use crate::adapters::tui::keymap::render::{rail, rail_pair};
use crate::adapters::tui::keymap::table_mission::MissionCmd;
use crate::adapters::tui::mission_control::tabs;
use crate::adapters::tui::presentation::viewer::ViewerCmd;
use crate::adapters::tui::state::TuiState;
use crate::adapters::tui::text_geometry;
use crate::domain::services::peer_text::sanitize_display_line;
use crate::domain::services::relative_time::format_relative_time;
use crate::domain::services::session_index::SessionSummary;

/// What the list says when there is nothing to list.
pub const EMPTY: &str = "No conversations";

/// One list row at `width` cells (marker excluded): the indicator, the fork marker, the title
/// cut to what is left, and the age and message count that always survive.
#[must_use]
pub fn row_text(session: &SessionSummary, now: i64, width: usize) -> String {
    let indicator = if session.is_active {
        "● "
    } else if session.is_open {
        "○ "
    } else {
        "  "
    };
    let fork = if session.has_fork_source { "🔀 " } else { "" };
    let suffix = format!(
        " · {} · {} msgs",
        format_relative_time(session.updated_at, now),
        session.message_count
    );
    let title = sanitize_display_line(&session.title);
    let title = if title.is_empty() { UNTITLED } else { &title };
    let room = width.saturating_sub(
        text_geometry::cells(indicator)
            + text_geometry::cells(fork)
            + text_geometry::cells(&suffix),
    );
    format!(
        "{indicator}{fork}{}{suffix}",
        text_geometry::truncate_to_width(title, room)
    )
}

pub fn render(frame: &mut Frame<'_>, rect: Rect, state: &TuiState, title: &str) {
    let theme = &state.theme;
    let Some(window) = state.history_window.as_ref() else {
        return;
    };
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(Style::default().fg(theme.colors.info_border))
            .style(
                Style::default()
                    .fg(theme.colors.fg_primary)
                    .bg(theme.colors.bg_surface),
            ),
        rect,
    );
    let inner = rect.inner(Margin::new(1, 1));
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    // The body keeps one row; the rail (its own keys) takes the last.
    let rail_rows = u16::from(inner.height > 1);
    let body = Rect::new(inner.x, inner.y, inner.width, inner.height - rail_rows);
    let muted = Style::default()
        .fg(theme.colors.fg_muted)
        .add_modifier(Modifier::ITALIC);
    let resolved = window.resolved();
    let total = tabs::view_len(window.rows().len(), resolved);
    let viewport = usize::from(body.height);
    let top = tabs::keep_visible(window.fit().top, tabs::view_pos(resolved), viewport, total);
    if total == 0 {
        frame.render_widget(Line::styled(EMPTY, muted), body);
    }
    for offset in 0..viewport.min(total.saturating_sub(top)) {
        let index = top + offset;
        let placeholder = matches!(resolved, tabs::Resolved::Gone(at) if at == index);
        let (marker, text, style) = if placeholder {
            (
                "▸ ",
                format!(
                    "{} is no longer available",
                    sanitize_display_line(&window.selection().label)
                ),
                muted,
            )
        } else {
            let session_index = match resolved {
                tabs::Resolved::Gone(at) if index > at => index - 1,
                _ => index,
            };
            let Some(session) = window.sessions().get(session_index) else {
                break;
            };
            let selected = matches!(resolved, tabs::Resolved::Found(at) if at == index);
            let width = usize::from(body.width).saturating_sub(2);
            let style = if selected {
                Style::default()
                    .fg(theme.colors.accent)
                    .add_modifier(Modifier::BOLD)
            } else if session.is_active {
                Style::default().fg(theme.colors.accent)
            } else {
                Style::default().fg(theme.colors.fg_primary)
            };
            (
                if selected { "▸ " } else { "  " },
                row_text(session, window.now(), width),
                style,
            )
        };
        let line = format!("{marker}{text}");
        frame.render_widget(
            Line::styled(
                text_geometry::clip(&line, usize::from(body.width)).to_owned(),
                style,
            ),
            Rect::new(body.x, body.y + offset as u16, body.width, 1),
        );
    }
    let mut fit = Fit {
        rows: total,
        viewport,
        top,
        ..Fit::default()
    };
    if rail_rows > 0 {
        let selected = window.selected_id().is_some();
        let mut items: Vec<(String, Option<usize>)> = Vec::with_capacity(5);
        if total > 1 {
            items.push((
                format!(
                    "[{}] Move",
                    rail_pair(
                        KeyContext::InspectorBody,
                        ViewerCmd::LineDown,
                        ViewerCmd::LineUp
                    )
                ),
                None,
            ));
        }
        if selected {
            items.push((
                format!(
                    "{} Open",
                    rail(KeyContext::History, MissionCmd::HistoryOpen)
                ),
                Some(0),
            ));
            items.push((
                format!(
                    "{} Delete",
                    rail(KeyContext::History, MissionCmd::HistoryDelete)
                ),
                Some(1),
            ));
        }
        items.push((
            format!(
                "{} Search",
                rail(KeyContext::History, MissionCmd::HistorySearch)
            ),
            Some(2),
        ));
        let back = format!("{} Back", rail(KeyContext::History, MissionCmd::Back));
        // What does not fit is dropped from the right; the way out always stays.
        let text = loop {
            let mut text = items
                .iter()
                .map(|(text, _)| text.as_str())
                .collect::<Vec<_>>()
                .join("  ");
            if !text.is_empty() {
                text.push_str("  ");
            }
            text.push_str(&back);
            if items.is_empty() || text_geometry::cells(&text) <= usize::from(inner.width) {
                for (_, mark) in &items {
                    match mark {
                        Some(0) => fit.open = true,
                        Some(1) => fit.delete = true,
                        Some(2) => fit.search = true,
                        _ => {}
                    }
                }
                break text;
            }
            items.pop();
        };
        frame.render_widget(
            Line::styled(text, Style::default().fg(theme.colors.fg_secondary)),
            Rect::new(inner.x, inner.y + body.height, inner.width, 1),
        );
    }
    window.set_fit(fit);
}

#[cfg(test)]
mod tests {
    use super::*;

    // Story 21.11 AC4/K4 (ported from the deleted `widgets/sidebar.rs`, Story 21.7 7-E): the
    // row is a 2-cell indicator + title + ` · just now` + ` · 3 msgs` (20 cells), so the title
    // budget is the row width minus 22; each case picks its title budget by choosing the width.
    const C: &str = "界";
    const G: &str = "👨\u{200d}👩\u{200d}👧";
    const A: &str = "e\u{301}";
    const SUFFIX: &str = " · just now · 3 msgs";

    fn row(title: &str, budget: usize) -> String {
        let now = 1_700_000_000;
        let entry = SessionSummary::new("c1".into(), title.into(), now, now, 3);
        row_text(&entry, now, budget + 22)
    }

    #[test]
    fn story21_7_history_row_title_cuts_on_whole_graphemes() {
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
                row(&title, budget),
                format!("  {shown}{SUFFIX}"),
                "title {title:?} budget {budget}"
            );
        }
    }
}
