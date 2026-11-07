//! S-TOOL paints only the frame rectangle supplied by the common compositor.
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear};
use unicode_segmentation::UnicodeSegmentation;

use crate::adapters::tui::{
    block_viewer,
    keymap::{
        Context, render, table_block::BodyCmd, table_overlay::OverlayCmd, table_tool::ToolCmd,
    },
    presentation::{
        markdown_body::{self, BodyMode},
        viewer::{self, ViewerCmd},
    },
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
    show_caret: bool,
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
        let text = if state.missing {
            format!("{} · {:?}", state.target.tool_call_id, state.status)
        } else {
            format!(
                "{} Wrap  {} Copy · {} · {:?}",
                render::rail(Context::ToolInspector, ToolCmd::Wrap),
                render::rail(Context::ToolInspector, ToolCmd::Copy),
                state.target.tool_call_id,
                state.status,
            )
        };
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
    let editing = !state.missing && state.search.editor.editing;
    let search_status = if state.missing {
        None
    } else {
        state.search.status()
    };
    let wrap_status = if !state.wrap_applies() {
        "Rendered"
    } else if state.wrap {
        "Wrap on"
    } else {
        "Wrap off"
    };
    let compact_status = if parts.status.is_none() {
        search_status.as_deref()
    } else {
        None
    };
    if parts.tabs.height > 0 {
        let field_width = if editing || compact_status.is_some() {
            parts.tabs.width.saturating_sub(
                tabs.width()
                    .min(usize::from(parts.tabs.width.saturating_sub(12))) as u16
                    + 2,
            )
        } else {
            0
        };
        let tabs_width = if field_width > 0 {
            parts.tabs.width.saturating_sub(field_width + 2)
        } else {
            parts.tabs.width
        };
        frame
            .buffer_mut()
            .set_line(parts.tabs.x, parts.tabs.y, &tabs, tabs_width);
        if field_width > 0 {
            let query = if editing {
                state
                    .search
                    .editor
                    .query_line(usize::from(field_width), theme, show_caret)
            } else {
                Line::styled(
                    compact_status.unwrap_or(""),
                    Style::default().fg(theme.colors.fg_muted),
                )
            };
            frame.buffer_mut().set_line(
                parts.tabs.right() - field_width,
                parts.tabs.y,
                &query,
                field_width,
            );
        }
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
        let index = state.tab.index();
        let h_off = if state.scrollable() {
            state.viewers[index]
                .h_off
                .min(state.widest.saturating_sub(state.cols))
        } else {
            0
        };
        // A rendered body clips its wide rows with `→` whatever 21-5's wrap says.
        let clip_wrap = state.wrap && state.wrap_applies();
        for (offset, line) in rows[range.clone()].iter().enumerate() {
            let row_index = range.start + offset;
            let y = parts.body.y + offset as u16;
            let row_width = state.row_widths[index][row_index];
            // (first row cell shown, cells the row moved right, cells painted)
            let (from, shift, painted) =
                if h_off > 0 && markdown_body::is_wide_row(&state.wide, row_index) {
                    let (cut, left, right) =
                        block_viewer::window(line, row_width, h_off, usize::from(parts.body.width));
                    frame.buffer_mut().set_line(
                        parts.body.x + u16::from(left),
                        y,
                        &cut,
                        parts.body.width,
                    );
                    let muted = Style::default().fg(theme.colors.fg_muted);
                    if left {
                        frame
                            .buffer_mut()
                            .set_stringn(parts.body.x, y, "←", 1, muted);
                    }
                    if right {
                        frame
                            .buffer_mut()
                            .set_stringn(parts.body.right() - 1, y, "→", 1, muted);
                    }
                    (h_off + usize::from(left), usize::from(left), cut.width())
                } else {
                    let (paint_width, marker) =
                        viewer::slice_line(row_width, parts.body.width, clip_wrap);
                    frame
                        .buffer_mut()
                        .set_line(parts.body.x, y, line, paint_width);
                    if let Some(marker) = marker {
                        frame.buffer_mut().set_stringn(
                            parts.body.right() - 1,
                            y,
                            marker,
                            1,
                            Style::default().fg(theme.colors.fg_muted),
                        );
                    }
                    (0, 0, usize::from(paint_width))
                };
            if state.search.query().is_empty() {
                continue;
            }
            if let Some(origin) = state.origins[index].get(row_index).and_then(Option::as_ref) {
                let mut bytes = origin.bytes.start;
                let mut x = 0;
                'spans: for span in &line.spans {
                    for (at, grapheme) in span.content.grapheme_indices(true) {
                        let cells = geom::cells(grapheme);
                        if x >= from && x + cells > from + painted {
                            break 'spans;
                        }
                        if x >= from {
                            if let Some(focused) = state.search.highlight(
                                origin.logical_line,
                                bytes + at..bytes + at + grapheme.len(),
                            ) {
                                if cells > 0 {
                                    frame.buffer_mut().set_style(
                                        Rect::new(
                                            parts.body.x + (shift + x - from) as u16,
                                            y,
                                            cells as u16,
                                            1,
                                        ),
                                        if focused {
                                            theme.search_highlight_focused
                                        } else {
                                            theme.search_highlight
                                        },
                                    );
                                }
                            }
                        }
                        x += cells;
                    }
                    bytes += span.content.len();
                }
            }
        }
    }
    if let Some(status) = parts.status {
        let first = if rows.is_empty() { 0 } else { range.start + 1 };
        let mut label = format!(
            "{wrap_status} · Lines {first}–{} of {}",
            range.end,
            rows.len()
        );
        if state.scrollable() {
            let from = state.viewers[state.tab.index()]
                .h_off
                .min(state.widest.saturating_sub(state.cols));
            label.push_str(&format!(
                " · col {}-{}/{}",
                from + 1,
                (from + state.cols).min(state.widest),
                state.widest
            ));
        }
        if state.tab == crate::adapters::tui::tool_inspector::ToolTab::Output
            && matches!(
                state.status,
                crate::domain::models::InvocationStatus::Pending
                    | crate::domain::models::InvocationStatus::Running
            )
        {
            label.push_str(if state.viewers[state.tab.index()].follow {
                " · Following"
            } else {
                " · Paused"
            });
        }
        if let Some(search) = search_status {
            label = format!("{search} · {label}");
        }
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
            let search = render::rail(Context::ToolInspector, OverlayCmd::InspectorSearch);
            let wrap = render::rail(Context::ToolInspector, ToolCmd::Wrap);
            let copy = render::rail(Context::ToolInspector, ToolCmd::Copy);
            let mut text = back;
            if parts.status.is_none() {
                if state.wrap_applies() {
                    text.push_str(&format!("  {wrap} {wrap_status}"));
                }
                text.push_str(&format!("  {copy} Copy"));
            }
            text.push_str(&format!("  [{tabs}] Tab"));
            if state.is_body() {
                text.push_str(&format!(
                    "  {} {}",
                    render::rail(Context::MarkdownBody, BodyCmd::ToggleMode),
                    match state.mode {
                        BodyMode::Source => "Rendered",
                        BodyMode::Rendered => "Source",
                    }
                ));
            }
            text.push_str(&format!("  [{scroll}] Scroll"));
            if state.scrollable() {
                text.push_str(&format!(
                    "  [{}] Columns",
                    render::rail_pair(
                        Context::MarkdownBody,
                        ViewerCmd::ScrollLeft,
                        ViewerCmd::ScrollRight
                    )
                ));
            }
            text.push_str(&format!("  {search} Search"));
            for (available, command, label) in [
                (state.previous_call, ToolCmd::PrevCall, "Previous"),
                (state.next_call, ToolCmd::NextCall, "Next"),
            ] {
                if available {
                    text.push_str(&format!(
                        "  {} {label}",
                        render::rail(Context::ToolInspector, command)
                    ));
                }
            }
            if parts.status.is_some() && parts.identity.is_none() {
                if state.wrap_applies() {
                    text.push_str(&format!("  {wrap} Wrap"));
                }
                text.push_str(&format!("  {copy} Copy"));
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tui::{
        markdown, presentation::viewer, state::MessageHeightKey, tool_inspector::ToolTarget,
    };
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn review_r2_12_literal_wide_substitution_then_later_match_pixels() {
        let source = "界x";
        let mut theme = Theme::dark();
        theme.search_highlight_focused = Style::default().fg(Color::Yellow).bg(Color::Blue);
        let rows = markdown::render(source, 1, &theme, &markdown::RenderOptions::literal()).lines;
        assert_eq!(rows[0].spans[0].content, " ");
        assert_eq!(rows[1].spans[0].content, "x");
        let origins = viewer::row_origins(source, &rows);
        let mut state = ToolInspectorState::new(
            ToolTarget {
                conversation_id: "conversation".into(),
                tool_call_id: "call".into(),
            },
            3,
        );
        state.search.commit("x".into());
        state.search.project(source, &origins, 0);
        state.origins[0] = origins;
        state.row_widths[0] = rows.iter().map(Line::width).collect();
        state.bodies[0] = Some((
            MessageHeightKey {
                msg_id: "call".into(),
                terminal_width: 1,
                content_hash: 0,
            },
            rows,
        ));
        let mut terminal = Terminal::new(TestBackend::new(3, 9)).unwrap();
        terminal
            .draw(|frame| render(frame, Rect::new(0, 0, 3, 9), &state, &theme, "", false))
            .unwrap();
        let body = layout::parts(Rect::new(0, 0, 3, 9)).body;
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(body.x, body.y)].symbol(), " ");
        let matched = &buffer[(body.x, body.y + 1)];
        assert_eq!(matched.symbol(), "x");
        assert_eq!(matched.fg, Color::Yellow);
        assert_eq!(matched.bg, Color::Blue);
    }
}
