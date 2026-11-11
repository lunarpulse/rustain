//! The paint of the Mission Control frame (S-MC, Story 21.7): the title the compositor
//! supplies, the tab row, the tab body and the generated rail. Paint only: the frame rectangle
//! comes from the common compositor, the state is shared and read-only, and what the paint
//! measured (rows, viewport, the wide split) is recorded in the state's [`Fit`] so the keys
//! clamp against the very rows painted here (height == paint).
//!
//! A list tab paints the rows [`tabs`] builds — master list, and on a wide frame the detail
//! preview of the selected row; the Room tab paints `room_panel::render_body`, the same
//! read-only replay the sidebar showed.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear};

use super::room_panel;
use crate::adapters::tui::keymap::Context as KeyContext;
use crate::adapters::tui::keymap::render::{rail, rail_arrows, rail_pair};
use crate::adapters::tui::keymap::table_mission::MissionCmd;
use crate::adapters::tui::mission_control::{Fit, McPane, McTab, tabs};
use crate::adapters::tui::presentation::viewer::{self, ViewerCmd};
use crate::adapters::tui::state::TuiState;
use crate::adapters::tui::text_geometry;
use crate::domain::services::peer_text::sanitize_display_line;

pub fn render(frame: &mut Frame<'_>, rect: Rect, state: &TuiState, title: &str) {
    let theme = &state.theme;
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
    let mission = &state.mission_control;
    // Priority: the body keeps one row, then the tab row, then the rail.
    let tab_rows = u16::from(inner.height >= 2);
    let rail_rows = u16::from(inner.height >= 3);
    let body_rows = inner.height - tab_rows - rail_rows;
    let tab_area = Rect::new(inner.x, inner.y, inner.width, tab_rows);
    let body = Rect::new(inner.x, inner.y + tab_rows, inner.width, body_rows);
    let rail_area = Rect::new(
        inner.x,
        inner.y + tab_rows + body_rows,
        inner.width,
        rail_rows,
    );

    if tab_rows > 0 {
        frame.render_widget(tab_line(state), tab_area);
    }
    let fit = match mission.tab {
        McTab::Room => paint_room(frame, body, state),
        McTab::Probe => paint_list(frame, body, state),
    };
    mission.set_fit(fit);
    if rail_rows > 0 {
        let text = rail_text(state, fit, usize::from(rail_area.width));
        frame.render_widget(
            Line::styled(text, Style::default().fg(theme.colors.fg_secondary)),
            rail_area,
        );
    }
}

/// The tab row: the delivered tabs in fixed order, the current one bold and marked, the others
/// muted.
fn tab_line(state: &TuiState) -> Line<'static> {
    let theme = &state.theme;
    let mission = &state.mission_control;
    let mut spans = Vec::with_capacity(4);
    for (index, tab) in mission.delivered().as_slice().iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
        }
        if *tab == mission.tab {
            spans.push(Span::styled(
                format!("▸{}", tab.label()),
                Style::default()
                    .fg(theme.colors.accent)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled(
                format!(" {}", tab.label()),
                Style::default().fg(theme.colors.fg_muted),
            ));
        }
    }
    Line::from(spans)
}

fn paint_room(frame: &mut Frame<'_>, body: Rect, state: &TuiState) -> Fit {
    let paint = room_panel::render_body(
        body,
        frame.buffer_mut(),
        &state.room_panel,
        state.mission_control.room_cursor,
        &state.theme,
    );
    let fit = Fit {
        list_viewport: paint.viewport_rows,
        scrollable: state.room_panel.visible_len() > paint.viewport_rows,
        ..Fit::default()
    };
    state.mission_control.record_room_paint(paint);
    fit
}

fn paint_list(frame: &mut Frame<'_>, body: Rect, state: &TuiState) -> Fit {
    let theme = &state.theme;
    let mission = &state.mission_control;
    let rows = mission.rows();
    let selection = mission.selection();
    let resolved = mission.resolved();
    let total = tabs::view_len(rows.len(), resolved);
    let split = tabs::split(body.width);
    let master_width = split.map_or(body.width, |(master, _)| master);
    let master = Rect::new(body.x, body.y, master_width, body.height);
    let viewport = usize::from(body.height);
    let top = tabs::keep_visible(
        mission.fit().list_top,
        tabs::view_pos(resolved),
        viewport,
        total,
    );
    if total == 0 {
        frame.render_widget(
            Line::styled(
                "Nothing here yet.",
                Style::default()
                    .fg(theme.colors.fg_muted)
                    .add_modifier(Modifier::ITALIC),
            ),
            master,
        );
    }
    for offset in 0..viewport.min(total.saturating_sub(top)) {
        let Some(row) = tabs::view_row(rows, selection, resolved, top + offset) else {
            break;
        };
        // Untrusted text is sanitized at the span builder, never left to the backend.
        let text = sanitize_display_line(&row.text);
        let (marker, style) = if row.gone {
            (
                "▸ ",
                Style::default()
                    .fg(theme.colors.fg_muted)
                    .add_modifier(Modifier::ITALIC),
            )
        } else if row.selected {
            (
                "▸ ",
                Style::default()
                    .fg(theme.colors.accent)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            ("  ", Style::default().fg(theme.colors.fg_primary))
        };
        let line = Line::from(vec![Span::styled(format!("{marker}{text}"), style)]);
        frame.render_widget(
            line,
            Rect::new(master.x, master.y + offset as u16, master.width, 1),
        );
    }
    let mut fit = Fit {
        list_rows: total,
        list_viewport: viewport,
        list_top: top,
        wide: split.is_some(),
        ..Fit::default()
    };
    if let Some((_, detail_width)) = split {
        let detail_area = Rect::new(body.x + master_width + 1, body.y, detail_width, body.height);
        let muted = Style::default().fg(theme.colors.fg_muted);
        for y in body.y..body.bottom() {
            frame.render_widget(
                Line::styled("│", muted),
                Rect::new(body.x + master_width, y, 1, 1),
            );
        }
        let lines = mission.detail_rows(usize::from(detail_width));
        let slice = viewer::slice(
            &mission.detail,
            lines.len(),
            usize::from(detail_area.height),
        );
        for (offset, text) in lines[slice].iter().enumerate() {
            frame.render_widget(
                Line::styled(text.clone(), Style::default().fg(theme.colors.fg_primary)),
                Rect::new(
                    detail_area.x,
                    detail_area.y + offset as u16,
                    detail_area.width,
                    1,
                ),
            );
        }
        fit.detail_rows = lines.len();
        fit.detail_viewport = usize::from(detail_area.height);
    }
    fit.scrollable = match (mission.pane, fit.wide) {
        (McPane::Detail, true) => fit.detail_rows > fit.detail_viewport,
        _ => total > 1,
    };
    fit
}

/// The rail, generated from the registry rows: a key is painted only where it acts (a second
/// tab, rows to move through, a wide frame); what does not fit is dropped from the right, the
/// way out always stays.
fn rail_text(state: &TuiState, fit: Fit, width: usize) -> String {
    let mission = &state.mission_control;
    let mut items: Vec<String> = Vec::with_capacity(4);
    if mission.delivered().as_slice().len() >= 2 {
        items.push(format!(
            "{} Tab",
            rail_arrows(
                KeyContext::MissionControl,
                MissionCmd::PrevTab,
                MissionCmd::NextTab
            )
        ));
    }
    if fit.scrollable {
        items.push(format!(
            "[{}] {}",
            rail_pair(
                KeyContext::InspectorBody,
                ViewerCmd::LineDown,
                ViewerCmd::LineUp
            ),
            if mission.tab.is_list() && !(mission.pane == McPane::Detail && fit.wide) {
                "Move"
            } else {
                "Scroll"
            }
        ));
    }
    if fit.wide && mission.tab.is_list() {
        items.push(format!(
            "{} {}",
            rail(KeyContext::MissionControl, MissionCmd::Focus),
            if mission.pane == McPane::Master {
                "Detail"
            } else {
                "List"
            }
        ));
    }
    let back = format!(
        "{} Back",
        rail(KeyContext::MissionControl, MissionCmd::Back)
    );
    loop {
        let mut text = items.join("  ");
        if !text.is_empty() {
            text.push_str("  ");
        }
        text.push_str(&back);
        if items.is_empty() || text_geometry::cells(&text) <= width {
            return text;
        }
        items.pop();
    }
}
