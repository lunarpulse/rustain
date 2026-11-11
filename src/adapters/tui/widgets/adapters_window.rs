//! The paint of the Adapters window (Story 21.7 A21): the snapshot's lines laid out at the
//! frame's inner width by `adapter_status_panel::lines`, sliced at the viewer's top, and the
//! generated rail. The painted line count and the viewport are recorded in the state, so the
//! keys clamp against exactly the rows painted (height == paint).

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear};

use super::adapter_status_panel::lines;
use crate::adapters::tui::keymap::Context as KeyContext;
use crate::adapters::tui::keymap::render::{rail, rail_pair};
use crate::adapters::tui::keymap::table_mission::MissionCmd;
use crate::adapters::tui::presentation::viewer::{self, ViewerCmd};
use crate::adapters::tui::state::TuiState;
use crate::adapters::tui::text_geometry;

pub fn render(frame: &mut Frame<'_>, rect: Rect, state: &TuiState, title: &str) {
    let theme = &state.theme;
    let Some(window) = state.adapters_window.as_ref() else {
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
    let rows = lines(&window.rows, body.width, theme);
    let viewport = usize::from(body.height);
    window.set_fit(rows.len(), viewport);
    let range = viewer::slice(&window.viewer, rows.len(), viewport);
    for (offset, line) in rows[range].iter().enumerate() {
        frame.render_widget(
            line,
            Rect::new(body.x, body.y + offset as u16, body.width, 1),
        );
    }
    if rail_rows > 0 {
        let mut items = Vec::with_capacity(2);
        if rows.len() > viewport {
            items.push(format!(
                "[{}] Scroll",
                rail_pair(
                    KeyContext::InspectorBody,
                    ViewerCmd::LineDown,
                    ViewerCmd::LineUp
                )
            ));
        }
        let back = format!("{} Back", rail(KeyContext::Adapters, MissionCmd::Back));
        let text = loop {
            let mut text = items.join("  ");
            if !text.is_empty() {
                text.push_str("  ");
            }
            text.push_str(&back);
            if items.is_empty() || text_geometry::cells(&text) <= usize::from(inner.width) {
                break text;
            }
            items.pop();
        };
        frame.render_widget(
            Line::styled(text, Style::default().fg(theme.colors.fg_secondary)),
            Rect::new(inner.x, inner.y + body.height, inner.width, 1),
        );
    }
}
