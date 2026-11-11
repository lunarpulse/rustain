//! The paint of the monitor column (Story 21.7 A16, AC21): the **current Mission Control tab's
//! body, read-only**, under a muted title `Monitor · <Tab>`. The column is a display, never a
//! focus or a key target: it paints no selection, no rail, no tab row and no filter, and no key is
//! routed to it (the compositor dims it under frames like the rest of the base).
//!
//! It reads what the hub reads — the list a tab holds, the Room fold, the held Messages page —
//! and changes none of it. Two facts reach the host through the draw's completion hook like the
//! hub's own: a Room body painted here counts as *presented* (the Room poll runs while it is on
//! screen, A14), and a Messages body painted here never commits a log visit (A13: the visit
//! commits only when the hub shows Messages on top).

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear};

use super::room_panel;
use crate::adapters::tui::keymap::render::leader_follow_up_long;
use crate::adapters::tui::keymap::table::LeaderCmd;
use crate::adapters::tui::mission_control::McTab;
use crate::adapters::tui::state::TuiState;
use crate::adapters::tui::text_geometry;
use crate::domain::services::peer_text::sanitize_display_line;

/// The frame title of the column.
#[must_use]
pub fn title(tab: McTab) -> String {
    format!("Monitor · {}", tab.label())
}

pub fn render(frame: &mut Frame<'_>, rect: Rect, state: &TuiState) {
    let theme = &state.theme;
    let mission = &state.mission_control;
    let tab = mission.monitor_tab();
    let muted = Style::default().fg(theme.colors.fg_muted);
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(title(tab), muted))
            .border_style(muted),
        rect,
    );
    let body = rect.inner(Margin::new(1, 1));
    if body.width == 0 || body.height == 0 {
        return;
    }
    match tab {
        McTab::Room => {
            let paint = room_panel::render_body(
                body,
                frame.buffer_mut(),
                &state.room_panel,
                mission.room_cursor,
                theme,
            );
            mission.record_monitor_room_paint(paint);
        }
        McTab::Messages => paint_messages(frame, body, state),
        McTab::Agents | McTab::Tasks | McTab::Artifacts | McTab::Probe => {
            paint_rows(frame, body, state, tab);
        }
    }
}

/// One muted italic line per wrapped row of `words`.
fn words(frame: &mut Frame<'_>, body: Rect, state: &TuiState, text: &str) {
    let style = Style::default()
        .fg(state.theme.colors.fg_muted)
        .add_modifier(Modifier::ITALIC);
    let safe = sanitize_display_line(text);
    for (offset, row) in text_geometry::wrap_hard(&safe, usize::from(body.width).max(1))
        .into_iter()
        .take(usize::from(body.height))
        .enumerate()
    {
        frame.render_widget(
            Line::styled(row.to_owned(), style),
            Rect::new(body.x, body.y + offset as u16, body.width, 1),
        );
    }
}

/// The newest rows of the held A2A log page (the column shows the tail: the closing claim last).
/// Nothing is marked painted here: only the hub on top commits a visit.
fn paint_messages(frame: &mut Frame<'_>, body: Rect, state: &TuiState) {
    let Some(page) = state.mission_control.messages.as_ref() else {
        words(
            frame,
            body,
            state,
            &format!(
                "{} opens the A2A log.",
                leader_follow_up_long(LeaderCmd::A2aLog)
            ),
        );
        return;
    };
    let lines = page.monitor_body(&state.theme, body.width);
    let start = lines.len().saturating_sub(usize::from(body.height));
    for (offset, line) in lines[start..].iter().enumerate() {
        frame.render_widget(
            line,
            Rect::new(body.x, body.y + offset as u16, body.width, 1),
        );
    }
}

/// The rows of a list tab, unselected and unfiltered (the hub's filter and selection are the
/// hub's own), one per line, from the top.
fn paint_rows(frame: &mut Frame<'_>, body: Rect, state: &TuiState, tab: McTab) {
    let theme = &state.theme;
    let list = state.mission_control.list_for(tab);
    let rows = list.map_or(&[][..], |list| list.all());
    if rows.is_empty() {
        let text = list
            .and_then(|list| list.empty.as_deref())
            .unwrap_or("Nothing here yet.");
        words(frame, body, state, text);
        return;
    }
    let mut area = body;
    if let Some(note) = list.and_then(|list| list.note.as_deref())
        && area.height >= 2
    {
        frame.render_widget(
            Line::styled(
                text_geometry::clip(&sanitize_display_line(note), usize::from(area.width))
                    .to_owned(),
                Style::default()
                    .fg(theme.colors.fg_muted)
                    .add_modifier(Modifier::ITALIC),
            ),
            Rect::new(area.x, area.y, area.width, 1),
        );
        area = Rect::new(area.x, area.y + 1, area.width, area.height - 1);
    }
    for (offset, row) in rows.iter().take(usize::from(area.height)).enumerate() {
        let text = sanitize_display_line(&row.text);
        frame.render_widget(
            Line::styled(
                text_geometry::clip(&text, usize::from(area.width)).to_owned(),
                Style::default().fg(theme.colors.fg_primary),
            ),
            Rect::new(area.x, area.y + offset as u16, area.width, 1),
        );
    }
}
