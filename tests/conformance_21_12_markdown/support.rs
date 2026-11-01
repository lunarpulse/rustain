//! Shared fixtures and observers for the 21-12 markdown conformance module.
//!
//! Everything here drives the real chat-pane front doors
//! (`chat_pane::render_with_search` for the local host, `chat_pane::render_attached`
//! for the attached host) into a `TestBackend` buffer and reads the painted
//! cells back. No expected row is produced by the formula under test.
#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap};

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use rustain::adapters::tui::state::TabRenderState;
use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::chat_pane;
use rustain::domain::clock::MockClock;
use rustain::domain::models::{
    ChatMessage, Conversation, MessageRole, StopReason, StreamingState, Turn, ViewState,
};
use unicode_width::UnicodeWidthStr;

use crate::render_helpers::{make_conversation, make_msg, make_turn, prose};

/// Content columns of the Stage gutter (`│ `).
pub const GUTTER: usize = 2;

/// One painted frame plus the height the pane reported for it.
pub struct Pane {
    pub buffer: Buffer,
    pub total_height: usize,
}

/// Painted rows of a pane: every buffer row, continuation cells dropped,
/// trailing blanks trimmed.
pub fn rows(buffer: &Buffer) -> Vec<String> {
    (buffer.area.top()..buffer.area.bottom())
        .map(|y| row_text(buffer, y))
        .collect()
}

pub fn row_text(buffer: &Buffer, y: u16) -> String {
    let mut out = String::new();
    let mut x = buffer.area.left();
    while x < buffer.area.right() {
        let Some(cell) = buffer.cell(Position::new(x, y)) else {
            break;
        };
        let symbol = cell.symbol();
        out.push_str(symbol);
        x = x.saturating_add(u16::try_from(symbol.width().max(1)).unwrap_or(1));
    }
    out.trim_end().to_owned()
}

/// Cells occupied by the glyphs of one painted row, trailing blanks included.
pub fn row_cells(buffer: &Buffer, y: u16) -> usize {
    let mut cells = 0usize;
    let mut x = buffer.area.left();
    while x < buffer.area.right() {
        let Some(cell) = buffer.cell(Position::new(x, y)) else {
            break;
        };
        let w = cell.symbol().width().max(1);
        cells += w;
        x = x.saturating_add(u16::try_from(w).unwrap_or(1));
    }
    cells
}

/// Style of the first cell whose glyph starts `needle` on any row.
pub fn style_of(buffer: &Buffer, needle: &str) -> Option<Style> {
    for y in buffer.area.top()..buffer.area.bottom() {
        let text = row_text(buffer, y);
        if let Some(byte) = text.find(needle) {
            let col = UnicodeWidthStr::width(&text[..byte]);
            let cell = buffer.cell(Position::new(u16::try_from(col).ok()?, y))?;
            return Some(cell.style());
        }
    }
    None
}

pub fn is_bold(buffer: &Buffer, needle: &str) -> bool {
    style_of(buffer, needle).is_some_and(|s| s.add_modifier.contains(Modifier::BOLD))
}

fn clock() -> MockClock {
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    clock.set_frame(3);
    clock
}

#[allow(clippy::too_many_arguments)]
pub fn paint_local(
    conversation: &Conversation,
    open_turn: Option<&Turn>,
    streaming: &StreamingState,
    open_prose: Option<&str>,
    theme: &Theme,
    width: u16,
    height: u16,
) -> Pane {
    let clock = clock();
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let mut total_height = 0usize;
    terminal
        .draw(|frame| {
            let mut tab = TabRenderState::default();
            let result = chat_pane::render_with_search(
                frame,
                Rect::new(0, 0, width, height),
                conversation,
                open_turn,
                streaming,
                &ViewState::default(),
                &clock,
                0,
                true,
                theme,
                &mut tab,
                &HashMap::new(),
                &BTreeMap::new(),
                None,
                None,
                &[],
                &[],
                None,
                None,
                open_prose,
                None,
            );
            total_height = result.total_content_height;
        })
        .unwrap();
    Pane {
        buffer: terminal.backend().buffer().clone(),
        total_height,
    }
}

pub fn paint_attached(
    conversation: &Conversation,
    streaming: &StreamingState,
    theme: &Theme,
    width: u16,
    height: u16,
) -> Pane {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let mut total_height = 0usize;
    terminal
        .draw(|frame| {
            let mut tab = TabRenderState::default();
            let result = chat_pane::render_attached(
                frame,
                Rect::new(0, 0, width, height),
                conversation,
                streaming,
                0,
                true,
                theme,
                &mut tab,
                &HashMap::new(),
                &BTreeMap::new(),
                &HashMap::new(),
            );
            total_height = result.total_content_height;
        })
        .unwrap();
    Pane {
        buffer: terminal.backend().buffer().clone(),
        total_height,
    }
}

/// A conversation with one committed assistant turn holding `markdown` prose.
pub fn turn_conversation(markdown: &str) -> Conversation {
    let turn = make_turn("k", vec![prose(markdown)], Some(StopReason::EndTurn));
    make_conversation(vec![make_msg("k", MessageRole::Assistant)], vec![turn])
}

/// Paint `markdown` as a committed assistant turn whose content is `inner`
/// columns wide (the pane adds the two-cell gutter). Returns the content rows
/// with the gutter removed, the final gutter half-line and trailing blank rows
/// dropped, so the rows are exactly what the markdown pipeline produced
/// (including its trailing blank line, which is kept as one empty row).
pub fn turn_rows(markdown: &str, inner: usize) -> Vec<String> {
    let pane = paint_local(
        &turn_conversation(markdown),
        None,
        &StreamingState::default(),
        None,
        &Theme::dark(),
        u16::try_from(inner + GUTTER).unwrap(),
        120,
    );
    content_rows(&pane)
}

/// Gutter-stripped rows of the committed-turn layout: `total_height` rows from
/// the top, minus the trailing half-line (gutter only).
pub fn content_rows(pane: &Pane) -> Vec<String> {
    let all = rows(&pane.buffer);
    let mut out: Vec<String> = all
        .into_iter()
        .take(pane.total_height)
        .map(|row| {
            row.strip_prefix("│ ")
                .map(str::to_owned)
                .unwrap_or_else(|| row.strip_prefix('│').unwrap_or(&row).to_owned())
        })
        .collect();
    // The turn layout ends with a gutter half-line after the markdown's own
    // trailing blank row.
    if out.last().is_some_and(String::is_empty) {
        out.pop();
    }
    out
}

/// A user message with the given stored content.
pub fn user_message(content: &str) -> ChatMessage {
    let mut message = make_msg("u1", MessageRole::User);
    message.content = content.to_owned();
    message
}

/// A completed legacy assistant message (no matching turn) with `content`.
pub fn legacy_assistant_message(content: &str) -> ChatMessage {
    let mut message = make_msg("a1", MessageRole::Assistant);
    message.content = content.to_owned();
    message
}

pub fn streaming_buffer(text: &str) -> StreamingState {
    StreamingState {
        is_streaming: true,
        current_text_buffer: text.to_owned(),
        ..StreamingState::default()
    }
}

/// Trim leading and trailing empty rows.
pub fn trimmed(rows: Vec<String>) -> Vec<String> {
    let start = rows
        .iter()
        .position(|r| !r.is_empty())
        .unwrap_or(rows.len());
    let end = rows
        .iter()
        .rposition(|r| !r.is_empty())
        .map_or(start, |i| i + 1);
    rows[start..end].to_vec()
}
