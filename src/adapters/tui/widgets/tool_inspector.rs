//! S-TOOL paints only the frame rectangle supplied by the common compositor.
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear};

use crate::adapters::tui::{
    keymap::{Context, render, table_tool::ToolCmd},
    presentation::viewer::{self, ViewerCmd},
    text_geometry as geom,
    theme::Theme,
    tool_inspector::{ToolInspectorState, layout},
};

pub fn render(
    frame: &mut Frame,
    rect: Rect,
    state: &ToolInspectorState,
    theme: &Theme,
    title: &str,
) {
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(theme.colors.info_border))
        .style(
            Style::default()
                .fg(theme.colors.fg_primary)
                .bg(theme.colors.bg_surface),
        );
    frame.render_widget(block, rect);
    let parts = layout::parts(rect);
    if let Some(identity) = parts.identity {
        let text = format!("{} · {:?}", state.target.tool_call_id, state.status);
        frame.buffer_mut().set_stringn(
            identity.x,
            identity.y,
            text,
            usize::from(identity.width),
            Style::default().fg(theme.colors.fg_muted),
        );
    }
    let mut spans = Vec::with_capacity(state.tabs.len() * 2 + 2);
    for (index, tab) in state.tabs.iter().enumerate() {
        let active = *tab == state.tab;
        let style = if active {
            Style::default()
                .fg(theme.colors.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.colors.fg_secondary)
        };
        if index != 0 {
            spans.push(Span::raw("  "));
        }
        if active {
            spans.push(Span::styled("[", style));
        }
        spans.push(Span::styled(tab.label(), style));
        if active {
            spans.push(Span::styled("]", style));
        }
    }
    let tabs = Line::from(spans);
    if parts.tabs.height > 0 {
        frame
            .buffer_mut()
            .set_line(parts.tabs.x, parts.tabs.y, &tabs, parts.tabs.width);
    }
    let rows = state.rows();
    let range = viewer::slice(
        &state.viewers[state.tab.index()],
        rows.len(),
        usize::from(parts.body.height),
    );
    if state.missing {
        if parts.body.height > 0 {
            frame.buffer_mut().set_stringn(
                parts.body.x,
                parts.body.y,
                "This tool call is no longer in the conversation.",
                usize::from(parts.body.width),
                Style::default().fg(theme.colors.fg_secondary),
            );
        }
    } else {
        for (offset, line) in rows[range.clone()].iter().enumerate() {
            frame.buffer_mut().set_line(
                parts.body.x,
                parts.body.y + offset as u16,
                line,
                parts.body.width,
            );
        }
    }
    if let Some(status) = parts.status {
        let first = if rows.is_empty() { 0 } else { range.start + 1 };
        let label = format!("Lines {first}–{} of {}", range.end, rows.len());
        frame.buffer_mut().set_stringn(
            status.x,
            status.y,
            label,
            usize::from(status.width),
            Style::default().fg(theme.colors.fg_muted),
        );
    }
    if parts.rail.height > 0 {
        let back = format!(
            "{} Back",
            render::rail(Context::ToolInspector, ToolCmd::Back)
        );
        let label = if state.missing {
            back
        } else {
            let tabs =
                render::rail_pair(Context::ToolInspector, ToolCmd::PrevTab, ToolCmd::NextTab);
            let scroll = render::rail_pair(
                Context::InspectorBody,
                ViewerCmd::LineDown,
                ViewerCmd::LineUp,
            );
            let text = format!("{back}  [{tabs}] Tab  [{scroll}] Scroll");
            geom::ellipsize(&text, usize::from(parts.rail.width), "…").into_owned()
        };
        frame.buffer_mut().set_stringn(
            parts.rail.x,
            parts.rail.y,
            label,
            usize::from(parts.rail.width),
            Style::default().fg(theme.colors.fg_secondary),
        );
    }
}
