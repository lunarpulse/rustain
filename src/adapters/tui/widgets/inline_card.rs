//! Shared bottom-anchored inline card renderer (Story 11.4a refactor).
//!
//! The memory-consolidation card (11.2a) and the `/memory forget` confirm card
//! (11.4a) render identically — a bordered `Paragraph` anchored to the bottom of
//! the chat pane ("helpful review, never a modal interruption" — UX). This factor
//! holds that one rendering so each card site only builds its `Vec<Line>`.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

/// Render `lines` as a bordered card anchored to the bottom of `chat_pane`,
/// clamped to the pane height. `accent` colours the border.
pub fn render_bottom_anchored_card(
    buf: &mut Buffer,
    lines: Vec<Line<'_>>,
    accent: Color,
    chat_pane: Rect,
) {
    let card_height = (lines.len() as u16 + 2).min(chat_pane.height);
    let card_area = Rect {
        x: chat_pane.x,
        y: chat_pane.y + chat_pane.height.saturating_sub(card_height),
        width: chat_pane.width,
        height: card_height,
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(accent));
    let para = Paragraph::new(lines).block(block);
    Widget::render(para, card_area, buf);
}
