//! The paint of the artifact window (Story 21.7 A15): the facts and the body as rows laid out
//! once per (width, mode, generation) by the window state, sliced at the viewer's top, the
//! filter field when it is open or kept, and the generated rail. What the paint measured (rows,
//! viewport, which keyed hints the rail painted whole) is recorded in the state, so the keys
//! clamp against the painted rows and a key whose hint is not painted is inert.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear};

use crate::adapters::tui::artifact_window::Live;
use crate::adapters::tui::keymap::Context as KeyContext;
use crate::adapters::tui::keymap::render::{rail, rail_pair};
use crate::adapters::tui::keymap::table_block::BodyCmd;
use crate::adapters::tui::keymap::table_mission::MissionCmd;
use crate::adapters::tui::keymap::table_overlay::OverlayCmd;
use crate::adapters::tui::presentation::markdown_body::BodyMode;
use crate::adapters::tui::presentation::viewer::{self, ViewerCmd};
use crate::adapters::tui::state::TuiState;
use crate::adapters::tui::text_geometry;

pub fn render(frame: &mut Frame<'_>, rect: Rect, state: &TuiState, title: &str, show_caret: bool) {
    let theme = &state.theme;
    let Some(window) = state.artifact_window.as_ref() else {
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
    let editing = window.search.editing;
    let kept = !window.search.query.is_empty();
    // The body keeps one row; the open field and the rail (its own keys) come next, a kept
    // filter's echo last.
    let mut left = inner.height.saturating_sub(1);
    let mut take = |wanted: bool| {
        let granted = u16::from(wanted && left > 0);
        left -= granted;
        granted
    };
    let (field, rail_rows);
    if editing {
        field = take(true);
        rail_rows = take(true);
    } else {
        rail_rows = take(true);
        field = take(kept);
    }
    let body_rows = inner.height - field - rail_rows;
    let body = Rect::new(inner.x, inner.y, inner.width, body_rows);
    let field_area = Rect::new(inner.x, inner.y + body_rows, inner.width, field);
    let rail_area = Rect::new(inner.x, inner.y + body_rows + field, inner.width, rail_rows);

    let lines = window.rows(theme, inner.width);
    let rows = lines.len();
    let viewport = usize::from(body_rows);
    window.set_fit(rows, viewport);
    let mut seen = window.viewer;
    if seen.follow {
        seen.top = usize::MAX;
    }
    let range = viewer::slice(&seen, rows, viewport);
    let (wide, widest) = window.wide_rows();
    let cols = usize::from(body.width);
    window.set_wide_fit(if wide.is_empty() { 0 } else { widest }, cols);
    let h_off = window.viewer.h_off.min(widest.saturating_sub(cols));
    for (offset, line) in lines[range.clone()].iter().enumerate() {
        let y = body.y + offset as u16;
        if crate::adapters::tui::presentation::markdown_body::is_wide_row(
            &wide,
            range.start + offset,
        ) {
            // A wide block scrolls sideways on its own, with edge markers.
            let (cut, left, right) =
                crate::adapters::tui::block_viewer::window(line, line.width(), h_off, cols);
            frame
                .buffer_mut()
                .set_line(body.x + u16::from(left), y, &cut, body.width);
            let muted = Style::default().fg(theme.colors.fg_muted);
            if left {
                frame.buffer_mut().set_stringn(body.x, y, "←", 1, muted);
            }
            if right {
                let last = body.x + body.width - 1;
                frame.buffer_mut().set_stringn(last, y, "→", 1, muted);
            }
        } else {
            frame.render_widget(line, Rect::new(body.x, y, body.width, 1));
        }
    }
    if field > 0 {
        let line = window
            .search
            .query_line(usize::from(field_area.width), theme, show_caret);
        frame.render_widget(
            Line::from(
                line.spans
                    .into_iter()
                    .map(|span| Span::styled(span.content.into_owned(), span.style))
                    .collect::<Vec<_>>(),
            ),
            field_area,
        );
    }
    let mut live = Live::default();
    if rail_rows > 0 {
        let (text, painted) = rail_text(window, rows > viewport, usize::from(rail_area.width));
        live = painted;
        frame.render_widget(
            Line::styled(text, Style::default().fg(theme.colors.fg_secondary)),
            rail_area,
        );
    }
    window.set_live(live);
}

/// The rail from the registry rows. A keyed hint that does not fit is dropped from the right
/// and its key is inert; the way out always stays.
fn rail_text(
    window: &crate::adapters::tui::artifact_window::ArtifactWindowState,
    scrollable: bool,
    width: usize,
) -> (String, Live) {
    if window.search.editing {
        return (
            format!(
                "{} Keep filter  {} Clear",
                rail(KeyContext::InspectorSearch, OverlayCmd::InspectorAccept),
                rail(KeyContext::InspectorSearch, OverlayCmd::InspectorBack),
            ),
            Live::default(),
        );
    }
    let shown = window.has_source();
    let mut items: Vec<(String, Option<&'static str>)> = Vec::with_capacity(4);
    if scrollable {
        items.push((
            format!(
                "[{}] Scroll",
                rail_pair(
                    KeyContext::InspectorBody,
                    ViewerCmd::LineDown,
                    ViewerCmd::LineUp
                )
            ),
            None,
        ));
    }
    let (widest, cols) = window.wide_fit();
    if widest > cols {
        items.push((
            format!(
                "[{}] Columns",
                rail_pair(
                    KeyContext::MarkdownBody,
                    ViewerCmd::ScrollLeft,
                    ViewerCmd::ScrollRight
                )
            ),
            None,
        ));
    }
    if shown {
        items.push((
            format!(
                "{} Search",
                rail(KeyContext::ArtifactWindow, OverlayCmd::InspectorSearch)
            ),
            Some("search"),
        ));
    }
    if window.has_source() {
        items.push((
            format!(
                "{} Copy",
                rail(KeyContext::ArtifactWindow, MissionCmd::CopySource)
            ),
            Some("copy"),
        ));
    }
    if shown && window.search.query.is_empty() {
        items.push((
            format!(
                "{} {}",
                rail(KeyContext::MarkdownBody, BodyCmd::ToggleMode),
                if window.mode == BodyMode::Rendered {
                    "Source"
                } else {
                    "Rendered"
                }
            ),
            Some("view"),
        ));
    }
    let back = format!(
        "{} Back",
        rail(KeyContext::ArtifactWindow, MissionCmd::Back)
    );
    loop {
        let mut text = items
            .iter()
            .map(|(text, _)| text.as_str())
            .collect::<Vec<_>>()
            .join("  ");
        if !text.is_empty() {
            text.push_str("  ");
        }
        text.push_str(&back);
        if items.is_empty() || text_geometry::cells(&text) <= width {
            let mut live = Live::default();
            for (_, key) in &items {
                match key {
                    Some("search") => live.search = true,
                    Some("copy") => live.copy = true,
                    Some("view") => live.view = true,
                    _ => {}
                }
            }
            return (text, live);
        }
        items.pop();
    }
}
