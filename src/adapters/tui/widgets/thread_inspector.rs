//! S-THREAD paints only the frame rectangle the common compositor supplies. The pinned
//! identity rows, the scrolling body and the generated rail are laid out once per
//! (width, content generation, rail facts) and reused by every repaint; the row list the
//! viewer clamps against is the row list painted here (height == paint, AD-2113).

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear};

use crate::adapters::tui::keymap::Context as KeyContext;
use crate::adapters::tui::keymap::render::{rail, rail_key, rail_pair};
use crate::adapters::tui::keymap::table_thread::ThreadCmd;
use crate::adapters::tui::presentation::markdown_body::{self, BodyMode};
use crate::adapters::tui::presentation::viewer::{self, ViewerCmd};
use crate::adapters::tui::state::{ThreadAccess, ThreadInspectorState, TuiState};
use crate::adapters::tui::thread_inspector::{
    self, ThreadAction, identity_text, retro_overlay, retro_overlay_bits, retro_preview_lines,
};
use crate::domain::models::Conversation;

/// Everything the rail depends on besides the width: the offered actions, the access, the
/// paging facts and the context/redaction wording.
fn rail_facts(
    inspector: &ThreadInspectorState,
    actions: &[Option<ThreadAction>; 6],
    redacted: bool,
) -> u32 {
    actions.iter().flatten().fold(
        (u32::from(inspector.access == ThreadAccess::ReadOnly) << 31)
            | (u32::from(inspector.access == ThreadAccess::LocalOnly) << 30)
            | (u32::from(inspector.omitted_before > 0) << 29)
            | (u32::from(inspector.omitted_after > 0) << 28)
            | (u32::from(inspector.context_selected) << 27)
            | (u32::from(redacted) << 22),
        |facts, action| facts | (1 << (*action as u8)),
    )
}

fn footer_text(
    inspector: &ThreadInspectorState,
    actions: &[Option<ThreadAction>; 6],
    redacted: bool,
) -> (String, String) {
    let mut footer = String::new();
    match inspector.access {
        ThreadAccess::LocalOnly => footer.push_str(
            "local view only — replying, acknowledgement and item context require a daemon-attached writer\n",
        ),
        ThreadAccess::ReadOnly => {
            footer.push_str("read-only — view only; item actions require the attached writer\n");
        }
        ThreadAccess::Writer => {
            for action in actions.iter().flatten() {
                if !footer.is_empty() {
                    footer.push_str("  ");
                }
                if *action == ThreadAction::RetroLeaveOut {
                    footer.push_str(&format!(
                        "[{}-{}] {}",
                        rail_key(KeyContext::ThreadInspector, ThreadCmd::LeaveOut(0)),
                        rail_key(KeyContext::ThreadInspector, ThreadCmd::LeaveOut(2)),
                        action.label(false, redacted),
                    ));
                } else {
                    footer.push_str(&format!(
                        "{} {}",
                        rail(KeyContext::ThreadInspector, action.command()),
                        action.label(inspector.context_selected, redacted),
                    ));
                }
            }
            if !footer.is_empty() {
                footer.push('\n');
            }
        }
    }
    // The writer rail (or the passive sentence) is the first section, the reader rail the second.
    let info = std::mem::take(&mut footer);
    if inspector.omitted_before > 0 {
        footer.push_str(&format!(
            "{} Earlier  ",
            rail(KeyContext::ThreadInspector, ThreadCmd::Earlier)
        ));
    }
    if inspector.omitted_after > 0 {
        footer.push_str(&format!(
            "{} Newer  ",
            rail(KeyContext::ThreadInspector, ThreadCmd::Newer)
        ));
    }
    footer.push_str(&format!(
        "[{}] Scroll  {} Close",
        rail_pair(
            KeyContext::InspectorBody,
            ViewerCmd::PageUp,
            ViewerCmd::PageDown
        ),
        rail(KeyContext::ThreadInspector, ThreadCmd::Back)
    ));
    (info, footer)
}

/// The paint of one thread frame. `conversation` is the attached writer's own conversation
/// (the eligibility facts); the local host passes its Stage's.
pub fn render(
    frame: &mut Frame<'_>,
    rect: Rect,
    state: &TuiState,
    conversation: Option<&Conversation>,
    title: &str,
) {
    let theme = &state.theme;
    let Some(inspector) = state.thread_inspector.as_ref() else {
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
    let width = usize::from(inner.width);
    let actions = thread_inspector::actions(state, conversation);
    let retro = conversation.and_then(|c| retro_overlay(c, inspector, &state.retro_left_out));
    let redacted = retro
        .and_then(|overlay| overlay.left_out)
        .is_some_and(|set| !set.is_empty());
    let mut cache = inspector.layout.borrow_mut();
    let body_key = (
        inner.width,
        inspector.generation,
        retro_overlay_bits(retro.as_ref()),
    );
    if cache.body_key != Some(body_key) {
        let paint = |text: &str, indent: usize| {
            wrap(
                text,
                width,
                indent,
                if text.starts_with("      ") {
                    theme.colors.fg_primary
                } else {
                    theme.colors.fg_secondary
                },
            )
        };
        let mut body =
            markdown_body::layout(&inspector.content, width, theme, BodyMode::Source, true).lines;
        if let Some(overlay) = retro {
            for text in retro_preview_lines(overlay.preview, overlay.left_out) {
                body.extend(wrap(&text, width, 2, theme.colors.fg_primary));
            }
        }
        body.extend(
            inspector
                .results
                .iter()
                .flat_map(|result| result.split('\n'))
                .flat_map(|line| paint(line, 0)),
        );
        for notice in crate::adapters::tui::handlers::recipient_actions::surface_action_notices(
            &state.recipient_actions,
            &inspector.target,
            inspector.node_id.as_deref(),
        ) {
            body.extend(wrap(&notice, width, 2, theme.colors.fg_primary));
        }
        cache.header = identity_text(inspector)
            .split('\n')
            .flat_map(|line| wrap(line, width, 2, theme.colors.fg_secondary))
            .collect();
        cache.body = body;
        cache.body_key = Some(body_key);
        inspector
            .layouts_built
            .set(inspector.layouts_built.get() + 1);
    }
    let footer_key = (inner.width, rail_facts(inspector, &actions, redacted));
    if cache.footer_key != Some(footer_key) {
        let (info, reader) = footer_text(inspector, &actions, redacted);
        let rows = |text: &str| -> Vec<Line<'static>> {
            text.split('\n')
                .flat_map(|line| wrap(line, width, 2, theme.colors.fg_secondary))
                .collect()
        };
        cache.footer_info = if info.is_empty() {
            Vec::new()
        } else {
            rows(info.trim_end_matches('\n'))
        };
        cache.footer_reader = rows(&reader);
        cache.footer_key = Some(footer_key);
    }
    // One shared fitting plan for paint and dispatch (rail truth): keep one body row, then give
    // the rail priority over the identity rows. When the writer rail does not fit whole it is not
    // painted, so its keys are inert; when the reader rail is cut, its page keys are inert.
    let room = inner.height;
    let body_min = u16::from(room > 0);
    let rail_room = room.saturating_sub(body_min);
    let rows_of = |lines: &[Line<'static>]| u16::try_from(lines.len()).unwrap_or(u16::MAX);
    let (info_rows, reader_rows) = (rows_of(&cache.footer_info), rows_of(&cache.footer_reader));
    let show_info = info_rows.saturating_add(reader_rows) <= rail_room;
    let reader_height = if show_info {
        reader_rows
    } else {
        reader_rows.min(rail_room)
    };
    let info_height = if show_info { info_rows } else { 0 };
    inspector
        .rail_live
        .set((show_info || info_rows == 0, reader_height == reader_rows));
    let footer_height = info_height + reader_height;
    let header_height = rows_of(&cache.header).min(room.saturating_sub(body_min + footer_height));
    let body_height = room - header_height - footer_height;
    let rows = cache.body.len();
    inspector.fit.set((rows, usize::from(body_height)));
    let top = Rect::new(inner.x, inner.y, inner.width, header_height);
    let body = Rect::new(inner.x, inner.y + header_height, inner.width, body_height);
    let info_area = Rect::new(
        inner.x,
        inner.y + room - footer_height,
        inner.width,
        info_height,
    );
    let reader_area = Rect::new(
        inner.x,
        inner.y + room - reader_height,
        inner.width,
        reader_height,
    );
    paint_lines(frame, top, &cache.header);
    let range = viewer::slice(&inspector.viewer, rows, usize::from(body_height));
    paint_lines(frame, body, &cache.body[range]);
    paint_lines(frame, info_area, &cache.footer_info);
    let reader_start = cache
        .footer_reader
        .len()
        .saturating_sub(usize::from(reader_height));
    paint_lines(frame, reader_area, &cache.footer_reader[reader_start..]);
}

fn wrap(
    text: &str,
    width: usize,
    indent: usize,
    color: ratatui::style::Color,
) -> Vec<Line<'static>> {
    use unicode_width::UnicodeWidthStr as _;
    let indent = indent.min(width.saturating_sub(2));
    let prefix = &"      "[..indent];
    let mut result = Vec::new();
    for paragraph in text.split('\n') {
        let hanging = (indent + paragraph.bytes().take_while(|byte| *byte == b' ').count())
            .min(6)
            .min(width.saturating_sub(2));
        let span = Span::raw(paragraph);
        let mut line = String::with_capacity(paragraph.len().min(width.saturating_mul(4)) + indent);
        line.push_str(prefix);
        let mut used = indent;
        for grapheme in span.styled_graphemes(Style::default()) {
            let cells = grapheme.symbol.width();
            if used + cells > width && used > indent {
                result.push(Line::styled(
                    std::mem::take(&mut line),
                    Style::default().fg(color),
                ));
                line.push_str(&"      "[..hanging]);
                used = hanging;
            }
            line.push_str(grapheme.symbol);
            used += cells;
        }
        result.push(Line::styled(line, Style::default().fg(color)));
    }
    result
}

fn paint_lines(frame: &mut Frame<'_>, area: Rect, lines: &[Line<'static>]) {
    for (index, line) in lines.iter().take(usize::from(area.height)).enumerate() {
        frame.render_widget(
            line,
            Rect::new(area.x, area.y + index as u16, area.width, 1),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tui::presentation::reducer;
    use crate::adapters::tui::state::ThreadTarget;
    use crate::domain::models::{ItemAddress, ItemId, PeerId, SemanticMessageType};
    use ratatui::{Terminal, backend::TestBackend};

    const HEIGHT: u16 = 26;

    fn open(content: &str) -> TuiState {
        let mut state = TuiState::new(80, HEIGHT);
        let address = ItemAddress::from_a2a_ingress(
            PeerId::from_public_key(&[5; 32]).unwrap(),
            ItemId::from_replay("item-1"),
        );
        let mut inspector = ThreadInspectorState::new(
            ThreadTarget::Item(address),
            ThreadAccess::LocalOnly,
            SemanticMessageType::Consultation,
        );
        inspector.content = content.to_owned();
        reducer::open_thread(&mut state, inspector, "Thread · consultation".to_owned());
        state
    }

    /// The frame's inner rows (borders excluded), trailing blanks trimmed, painted by the
    /// real widget into a `TestBackend` of `outer` columns.
    fn paint(state: &TuiState, outer: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(outer, HEIGHT)).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), state, None, "Thread · consultation"))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (1..HEIGHT - 1)
            .map(|y| {
                (1..outer - 1)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    fn line_text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
            .trim_end()
            .to_owned()
    }

    fn content() -> String {
        let words = (0..100)
            .map(|n| format!("word{n:03}"))
            .collect::<Vec<_>>()
            .join(" ");
        format!("{words} {}", "L".repeat(45))
    }

    /// Paints at `inner` columns (frame width `inner + 2`), scrolled to the top or the end,
    /// and checks the clamp's row count is the painted row count. Returns `(rows, viewport)`.
    fn parity(inner: u16, to_end: bool) -> (usize, usize) {
        let mut state = open(&content());
        if to_end {
            state.thread_inspector.as_mut().unwrap().scroll_to_end();
        }
        let painted = paint(&state, inner + 2);
        let inspector = state.thread_inspector.as_ref().unwrap();
        let (rows, viewport) = inspector.fit.get();
        let cache = inspector.layout.borrow();
        let label = format!("inner width {inner} end={to_end}");
        assert_eq!(rows, cache.body.len(), "{label}: fit rows == laid-out rows");
        assert!(
            rows > viewport && viewport >= 1,
            "{label}: positive control: the body is clipped ({rows} rows, viewport {viewport})"
        );
        let header = cache.header.len();
        let footer = cache.footer_info.len() + cache.footer_reader.len();
        assert_eq!(
            viewport,
            usize::from(HEIGHT - 2) - header - footer,
            "{label}: the viewport is what the plan left"
        );
        let top = if to_end { rows - viewport } else { 0 };
        let shown = &painted[header..header + viewport];
        let expected: Vec<String> = cache.body[top..top + viewport]
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(
            shown,
            expected.as_slice(),
            "{label}: painted == clamped slice"
        );
        // The literal body layout ends with one empty row; every other painted row has text.
        assert!(
            shown.iter().filter(|row| !row.is_empty()).count() + 1 >= viewport,
            "{label}: no blank frame: at most the layout's own trailing row is empty"
        );
        (rows, viewport)
    }

    #[test]
    fn story21_8_the_clamped_row_count_is_the_painted_row_count_at_widths_20_21_and_33() {
        let mut rows_at = Vec::new();
        for inner in [20u16, 21, 33] {
            let (top_rows, _) = parity(inner, false);
            let (end_rows, _) = parity(inner, true);
            assert_eq!(top_rows, end_rows, "one layout per width");
            rows_at.push(top_rows);
        }
        assert!(
            rows_at[0] > rows_at[2],
            "positive control: the width changes the wrap ({rows_at:?})"
        );
    }

    #[test]
    fn story21_8_a_width_change_in_one_frame_relays_out_and_clamps_the_retained_viewer() {
        let mut state = open(&content());
        state.thread_inspector.as_mut().unwrap().scroll_to_end();
        let narrow = paint(&state, 22);
        let inspector = state.thread_inspector.as_ref().unwrap();
        let (narrow_rows, viewport) = inspector.fit.get();
        assert_eq!(inspector.layouts_built.get(), 1);
        assert!(narrow.iter().any(|row| !row.is_empty()));
        let wide = paint(&state, 35);
        let (wide_rows, wide_viewport) = inspector.fit.get();
        assert_eq!(
            inspector.layouts_built.get(),
            2,
            "a new width is a new layout key"
        );
        assert!(
            wide_rows < narrow_rows,
            "positive control: the wider frame wraps less"
        );
        let cache = inspector.layout.borrow();
        let header = cache.header.len();
        assert!(
            wide_viewport >= 1 && wide_rows > wide_viewport,
            "{wide_rows} {wide_viewport} {viewport}"
        );
        let expected: Vec<String> = cache.body[wide_rows - wide_viewport..]
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(
            wide[header..header + wide_viewport],
            expected[..],
            "after widening the retained end position paints the last rows, not a blank frame"
        );
    }
}
