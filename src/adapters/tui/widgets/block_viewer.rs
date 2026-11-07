//! S-BLOCK paints only the frame rectangle supplied by the common compositor.
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear};

use crate::adapters::tui::{
    block_viewer::{self, BlockViewerState},
    keymap::{Context, render, table_block::BlockCmd, table_overlay::OverlayCmd},
    markdown::{BlockKind, BlockMode},
    presentation::viewer::{self, ViewerCmd},
    text_geometry as geom,
    theme::Theme,
};

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// `code 2/4 · rust · 45 lines · Rendered · col 1-76/122`; the column range only
/// appears when something is wider than the viewport.
fn status(state: &BlockViewerState) -> String {
    let Some(summary) = &state.summary else {
        return String::new();
    };
    let mut words = vec![match &summary.kind {
        BlockKind::Code { .. } => format!("code {}/{}", summary.index, summary.total),
        BlockKind::Table { .. } => format!("table {}/{}", summary.index, summary.total),
    }];
    match &summary.kind {
        BlockKind::Code { language, lines } => {
            words.extend(language.as_deref().map(block_viewer::safe_language));
            words.push(plural(*lines, "line", "lines"));
        }
        BlockKind::Table { cols, rows } => {
            words.push(format!(
                "{} × {}",
                plural(*cols, "col", "cols"),
                plural(*rows, "row", "rows")
            ));
        }
    }
    words.push(
        match state.mode {
            BlockMode::Rendered => "Rendered",
            BlockMode::Source => "Source",
        }
        .to_owned(),
    );
    if state.wide() {
        let first = state.viewer.h_off.min(state.widest - state.cols) + 1;
        let last = (first - 1 + state.cols).min(state.widest);
        words.push(format!("col {first}-{last}/{}", state.widest));
    }
    words.join(" · ")
}

fn rail_text(state: &BlockViewerState) -> String {
    let back = format!(
        "{} Back",
        render::rail(Context::BlockViewer, BlockCmd::Back)
    );
    if state.missing {
        return back;
    }
    let mut text = format!(
        "{back}  {} Copy  [{}] Scroll",
        render::rail(Context::BlockViewer, BlockCmd::Copy),
        render::rail_pair(
            Context::InspectorBody,
            ViewerCmd::LineDown,
            ViewerCmd::LineUp
        ),
    );
    if state.wide() {
        text.push_str(&format!(
            "  [{}] Columns",
            render::rail_pair(
                Context::MarkdownBody,
                ViewerCmd::ScrollLeft,
                ViewerCmd::ScrollRight
            )
        ));
    }
    text.push_str(&format!(
        "  {} Search",
        render::rail(Context::BlockViewer, OverlayCmd::InspectorSearch)
    ));
    text
}

pub fn render(
    frame: &mut Frame,
    rect: Rect,
    state: &BlockViewerState,
    theme: &Theme,
    title: &str,
    show_caret: bool,
) {
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
    let parts = block_viewer::parts(rect);
    let muted = Style::default().fg(theme.colors.fg_muted);
    if let Some(area) = parts.status {
        let editing = !state.missing && state.search.editor.editing;
        if editing {
            let line = state
                .search
                .editor
                .query_line(usize::from(area.width), theme, show_caret);
            frame
                .buffer_mut()
                .set_line(area.x, area.y, &line, area.width);
        } else if !state.missing {
            let mut label = status(state);
            if let Some(search) = state.search.status() {
                label = format!("{search} · {label}");
            }
            frame.buffer_mut().set_stringn(
                area.x,
                area.y,
                label,
                usize::from(area.width),
                Style::default().fg(theme.colors.fg_secondary),
            );
        }
    }
    let body = parts.body;
    if state.missing {
        if body.height > 0 {
            frame.buffer_mut().set_stringn(
                body.x,
                body.y,
                "This block is no longer in the conversation.",
                usize::from(body.width),
                Style::default().fg(theme.colors.fg_secondary),
            );
        }
    } else {
        let range = viewer::slice(&state.viewer, state.rows.len(), usize::from(body.height));
        let h_off = state
            .viewer
            .h_off
            .min(state.widest.saturating_sub(state.cols));
        for (offset, index) in range.enumerate() {
            let (line, left, right) = block_viewer::window(
                state.painted(index),
                state.row_widths[index],
                h_off,
                usize::from(body.width),
            );
            let y = body.y + offset as u16;
            frame
                .buffer_mut()
                .set_line(body.x + u16::from(left), y, &line, body.width);
            if left {
                frame.buffer_mut().set_stringn(body.x, y, "←", 1, muted);
            }
            if right {
                frame
                    .buffer_mut()
                    .set_stringn(body.right() - 1, y, "→", 1, muted);
            }
        }
    }
    if let Some(area) = parts.rail {
        frame.buffer_mut().set_stringn(
            area.x,
            area.y,
            geom::ellipsize(&rail_text(state), usize::from(area.width), "…"),
            usize::from(area.width),
            Style::default().fg(theme.colors.fg_secondary),
        );
    }
}
