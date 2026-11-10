//! S-THREAD paints only the frame rectangle the common compositor supplies. The pinned
//! identity rows, the scrolling body and the generated rail are laid out once per
//! (width, content generation, rail facts) and reused by every repaint; the row list the
//! viewer clamps against is the row list painted here (height == paint, AD-2113).
//!
//! The body is the page's segments (`thread_body::build`): local facts literal, peer text
//! rendered as markdown behind the six-cell gutter (or the literal source, `v`).

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear};

use crate::adapters::tui::block_viewer;
use crate::adapters::tui::keymap::Context as KeyContext;
use crate::adapters::tui::keymap::render::{rail, rail_key, rail_pair};
use crate::adapters::tui::keymap::table_block::BodyCmd;
use crate::adapters::tui::keymap::table_overlay::OverlayCmd;
use crate::adapters::tui::keymap::table_thread::ThreadCmd;
use crate::adapters::tui::presentation::markdown_body::{self, BodyMode};
use crate::adapters::tui::presentation::viewer::{self, ViewerCmd};
use crate::adapters::tui::state::{ThreadAccess, ThreadInspectorState, ThreadTarget, TuiState};
use crate::adapters::tui::thread_body::{self, GUTTER};
use crate::adapters::tui::thread_inspector::{
    self, ThreadAction, identity_text, retro_overlay, retro_overlay_bits, retro_preview_lines,
};
use crate::domain::models::Conversation;

/// What the page rail offers besides the writer actions, by facts alone.
#[derive(Clone)]
struct Offers {
    toggle: bool,
    open: bool,
    viewport: bool,
    /// `x`: the words of the retract entry (`Retract`, `Retract auto-sent reply HH:MM`).
    retract: Option<String>,
}

/// Everything the rail depends on besides the width: the offered actions, the access, the
/// paging facts, the page offers, the field state and the context/redaction wording.
fn rail_facts(
    inspector: &ThreadInspectorState,
    actions: &[Option<ThreadAction>; 6],
    redacted: bool,
    offers: &Offers,
) -> u64 {
    let base = actions.iter().flatten().fold(
        (u32::from(inspector.access == ThreadAccess::ReadOnly) << 31)
            | (u32::from(inspector.access == ThreadAccess::LocalOnly) << 30)
            | (u32::from(inspector.omitted_before > 0) << 29)
            | (u32::from(inspector.omitted_after > 0) << 28)
            | (u32::from(inspector.context_selected) << 27)
            | (u32::from(redacted) << 22),
        |facts, action| facts | (1 << (*action as u8)),
    );
    let extra = u64::from(inspector.mode == BodyMode::Source)
        | (u64::from(offers.toggle) << 1)
        | (u64::from(offers.open) << 2)
        | (u64::from(offers.viewport) << 3)
        | (retract_facts(offers) << 9)
        | (u64::from(matches!(inspector.target, ThreadTarget::Node(_))) << 4)
        | (u64::from(inspector.reply.is_some()) << 5)
        | (u64::from(inspector.search.editing) << 6)
        | (u64::from(inspector.viewport) << 7)
        | (u64::from(!inspector.search.query.is_empty()) << 8);
    u64::from(base) | (extra << 32)
}

fn footer_text(
    inspector: &ThreadInspectorState,
    actions: &[Option<ThreadAction>; 6],
    redacted: bool,
    offers: &Offers,
) -> (String, String) {
    // A text field or the block viewport replaces the page rail by its own keys.
    if inspector.reply.is_some() {
        return (
            String::new(),
            format!(
                "{} Send  {} Clear",
                rail(KeyContext::ThreadReply, ThreadCmd::SendReply),
                rail(KeyContext::ThreadReply, ThreadCmd::ReplyBack),
            ),
        );
    }
    if inspector.search.editing {
        return (
            String::new(),
            format!(
                "{} Keep filter  {} Clear",
                rail(KeyContext::InspectorSearch, OverlayCmd::InspectorAccept),
                rail(KeyContext::InspectorSearch, OverlayCmd::InspectorBack),
            ),
        );
    }
    if inspector.viewport {
        return (
            String::new(),
            format!(
                "[{}] Columns  {} Leave",
                rail_pair(
                    KeyContext::MarkdownBody,
                    ViewerCmd::ScrollLeft,
                    ViewerCmd::ScrollRight
                ),
                rail(KeyContext::BodyViewport, ThreadCmd::LeaveViewport),
            ),
        );
    }
    let mut footer = String::new();
    let item_target = matches!(
        inspector.target,
        ThreadTarget::Item(_) | ThreadTarget::Node(_)
    );
    match inspector.access {
        // The passive sentences speak of item actions: the sent thread and the log have none.
        ThreadAccess::LocalOnly if item_target => footer.push_str(
            "local view only — replying, acknowledgement and item context require a daemon-attached writer\n",
        ),
        ThreadAccess::ReadOnly if item_target => {
            footer.push_str("read-only — view only; item actions require the attached writer\n");
        }
        ThreadAccess::LocalOnly | ThreadAccess::ReadOnly => {}
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
            if let Some(label) = &offers.retract {
                if !footer.is_empty() {
                    footer.push_str("  ");
                }
                footer.push_str(&format!(
                    "{} {label}",
                    rail(KeyContext::ThreadInspector, ThreadCmd::Retract)
                ));
            }
            if !footer.is_empty() {
                footer.push('\n');
            }
        }
    }
    // The writer rail (or the passive sentence) is the first section, the page rail the second.
    let info = std::mem::take(&mut footer);
    let mut push = |text: String| {
        footer.push_str(&text);
        footer.push_str("  ");
    };
    if inspector.omitted_before > 0 {
        push(format!(
            "{} Earlier",
            rail(KeyContext::ThreadInspector, ThreadCmd::Earlier)
        ));
    }
    if inspector.omitted_after > 0 {
        push(format!(
            "{} Newer",
            rail(KeyContext::ThreadInspector, ThreadCmd::Newer)
        ));
    }
    if offers.toggle {
        push(format!(
            "{} {}",
            rail(KeyContext::ThreadInspector, ThreadCmd::Toggle),
            if matches!(inspector.target, ThreadTarget::Node(_)) {
                "Item"
            } else {
                "Response"
            }
        ));
    }
    if offers.open {
        push(format!(
            "{} Open decision",
            rail(KeyContext::ThreadInspector, ThreadCmd::OpenDecision)
        ));
    }
    let log = matches!(inspector.target, ThreadTarget::Log { .. });
    if !log {
        push(format!(
            "{} Copy",
            rail(KeyContext::ThreadInspector, ThreadCmd::Copy)
        ));
    }
    push(format!(
        "{} Search",
        rail(KeyContext::ThreadInspector, OverlayCmd::InspectorSearch)
    ));
    if !log {
        push(format!(
            "{} {}",
            rail(KeyContext::MarkdownBody, BodyCmd::ToggleMode),
            if inspector.mode == BodyMode::Rendered {
                "Source"
            } else {
                "Rendered"
            }
        ));
    }
    if offers.viewport {
        push(format!(
            "{} Block viewport",
            rail(KeyContext::ThreadInspector, ThreadCmd::Viewport)
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

/// The retract entry's share of the rail key: present, plus a hash of its words (a newer
/// auto-sent reply changes the time in them).
fn retract_facts(offers: &Offers) -> u64 {
    offers.retract.as_ref().map_or(0, |label| {
        1 | (label.bytes().fold(0u64, |hash, byte| {
            hash.wrapping_mul(31).wrapping_add(u64::from(byte))
        }) & 0x3f_ffff)
            << 1
    })
}

/// The one row of the open text field: the prompt of the command it will run (reply) or the
/// filter query, with the caret only where the compositor grants it.
fn field_line(
    inspector: &ThreadInspectorState,
    width: usize,
    theme: &crate::adapters::tui::theme::Theme,
    show_caret: bool,
) -> Option<Line<'static>> {
    if let Some(reply) = &inspector.reply {
        let ThreadTarget::Item(address) = &inspector.target else {
            return None;
        };
        let prompt = reply.prompt(address.item().as_str());
        let used = crate::adapters::tui::text_geometry::cells(&prompt);
        let view = reply.field.view(width.saturating_sub(used).max(1));
        let normal = Style::default().fg(theme.colors.fg_primary);
        return Some(Line::from(vec![
            Span::styled(prompt, Style::default().fg(theme.colors.accent)),
            Span::styled(view.before.to_owned(), normal),
            Span::styled(
                view.caret.to_owned(),
                if show_caret {
                    normal.add_modifier(Modifier::REVERSED)
                } else {
                    normal
                },
            ),
            Span::styled(view.after.to_owned(), normal),
        ]));
    }
    if inspector.search.editing || !inspector.search.query.is_empty() {
        let line = inspector.search.query_line(width, theme, show_caret);
        return Some(Line::from(
            line.spans
                .into_iter()
                .map(|span| Span::styled(span.content.into_owned(), span.style))
                .collect::<Vec<_>>(),
        ));
    }
    None
}

/// The paint of one thread frame. `conversation` is the attached writer's own conversation
/// (the eligibility facts); the local host passes its Stage's.
pub fn render(
    frame: &mut Frame<'_>,
    rect: Rect,
    state: &TuiState,
    conversation: Option<&Conversation>,
    title: &str,
    show_caret: bool,
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
    let offers = Offers {
        toggle: thread_inspector::offered(state, conversation, ThreadCmd::Toggle),
        open: thread_inspector::offered(state, conversation, ThreadCmd::OpenDecision),
        viewport: false,
        retract: thread_inspector::retract_plan(state, conversation).map(|plan| plan.label()),
    };
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
        let laid = thread_body::build(inspector, theme, width);
        let mut body = laid.lines;
        if let Some(overlay) = retro {
            for text in retro_preview_lines(overlay.preview, overlay.left_out) {
                body.extend(wrap(&text, width, 2, theme.colors.fg_primary));
            }
        }
        body.extend(
            state
                .thread_outcomes
                .lines(&inspector.outcome_key())
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
        cache.header = Some(identity_text(inspector))
            .filter(|text| !text.is_empty())
            .iter()
            .flat_map(|text| text.split('\n'))
            .flat_map(|line| wrap(line, width, 2, theme.colors.fg_secondary))
            .collect();
        cache.body = body;
        cache.wide = laid.wide;
        cache.widest = laid.widest;
        cache.body_key = Some(body_key);
        inspector
            .layouts_built
            .set(inspector.layouts_built.get() + 1);
    }
    // A wide block exists only where some block is wider than the columns it scrolls in.
    let cols = width.saturating_sub(GUTTER).max(1);
    inspector.wide_fit.set((
        if cache.wide.is_empty() {
            0
        } else {
            cache.widest
        },
        cols,
    ));
    // The viewport row is offered only where a wide block exists in this very layout.
    let offers = Offers {
        viewport: !cache.wide.is_empty(),
        ..offers
    };
    let footer_key = (
        inner.width,
        rail_facts(inspector, &actions, redacted, &offers),
    );
    if cache.footer_key != Some(footer_key) {
        let (info, reader) = footer_text(inspector, &actions, redacted, &offers);
        let rows = |text: &str| -> Vec<Line<'static>> {
            text.split('\n')
                .flat_map(|line| wrap(line.trim_end(), width, 2, theme.colors.fg_secondary))
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
    // One shared fitting plan for paint and dispatch (rail truth): keep one body row, then the
    // open field's row, then give the rail priority over the identity rows. When the writer
    // rail does not fit whole it is not painted, so its keys are inert; when the reader rail
    // is cut, the page keys are inert.
    let room = inner.height;
    let body_min = u16::from(room > 0);
    let field = field_line(inspector, width, theme, show_caret);
    let field_height = u16::from(field.is_some() && room > body_min);
    let rail_room = room.saturating_sub(body_min + field_height);
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
    let header_height =
        rows_of(&cache.header).min(room.saturating_sub(body_min + field_height + footer_height));
    let body_height = room - header_height - field_height - footer_height;
    let rows = cache.body.len();
    if matches!(inspector.target, ThreadTarget::Log { .. }) && body_height >= 1 && rows > 0 {
        inspector.log_painted.set(true);
    }
    inspector.fit.set((rows, usize::from(body_height)));
    let top = Rect::new(inner.x, inner.y, inner.width, header_height);
    let body = Rect::new(inner.x, inner.y + header_height, inner.width, body_height);
    let field_area = Rect::new(
        inner.x,
        inner.y + header_height + body_height,
        inner.width,
        field_height,
    );
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
    // `follow` is a pending jump-to-end (`G`): resolved here against the live fit, so a key
    // that followed a field/filter transition before this paint cannot leave a stale position
    // (8B-02). Any other scroll clears it.
    let mut seen = inspector.viewer;
    if seen.follow {
        seen.top = usize::MAX;
    }
    let range = viewer::slice(&seen, rows, usize::from(body_height));
    let h_off = inspector
        .viewer
        .h_off
        .min(cache.widest.saturating_sub(cols));
    for (offset, line) in cache.body[range.clone()].iter().enumerate() {
        let y = body.y + offset as u16;
        if markdown_body::is_wide_row(&cache.wide, range.start + offset) && line.spans.len() > 1 {
            // The block row scrolls sideways behind its fixed gutter, with edge markers.
            let block = Line::from(line.spans[1..].to_vec());
            let (cut, left, right) = block_viewer::window(&block, block.width(), h_off, cols);
            let x = body.x + GUTTER as u16;
            frame.buffer_mut().set_line(
                x + u16::from(left),
                y,
                &cut,
                u16::try_from(cols).unwrap_or(u16::MAX),
            );
            let muted = Style::default().fg(theme.colors.fg_muted);
            if left {
                frame.buffer_mut().set_stringn(x, y, "←", 1, muted);
            }
            if right {
                let last = x + u16::try_from(cols).unwrap_or(u16::MAX) - 1;
                frame.buffer_mut().set_stringn(last, y, "→", 1, muted);
            }
        } else {
            frame.render_widget(line, Rect::new(body.x, y, body.width, 1));
        }
    }
    if let Some(field) = &field {
        paint_lines(frame, field_area, std::slice::from_ref(field));
    }
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
        inspector.segments = vec![crate::adapters::tui::thread_body::ThreadSegment::Text(
            content.to_owned(),
        )];
        reducer::open_thread(&mut state, inspector, "Thread · consultation".to_owned());
        state
    }

    /// The frame's inner rows (borders excluded), trailing blanks trimmed, painted by the
    /// real widget into a `TestBackend` of `outer` columns.
    fn paint(state: &TuiState, outer: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(outer, HEIGHT)).unwrap();
        terminal
            .draw(|frame| {
                render(
                    frame,
                    frame.area(),
                    state,
                    None,
                    "Thread · consultation",
                    false,
                )
            })
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
