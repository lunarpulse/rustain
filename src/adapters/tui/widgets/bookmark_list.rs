//! Bookmark list panel widget (Story 4-4 AC10).
//!
//! Renders a bottom-aligned list panel inside the chat pane region showing
//! the active conversation's bookmarks. NOT a centered modal — the panel
//! slides up from the bottom (like the status bar) so the conversation
//! stays visible above it, letting the user confirm "yes, that's the
//! message I want" before jumping.
//!
//! Wired to layout via `AppLayout::reserve_bookmark_panel()` (Task 2.3).
//!
//! Pure render — no state mutation. Input dispatch lives in `app.rs` and
//! the handler functions in `event_loop.rs`.
// Covers: Story 4-4 AC10 (UX-DR91)

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::adapters::tui::text_geometry as geom;
use crate::adapters::tui::theme::Theme;
use crate::adapters::tui::widgets::cross_search::shorten_to_cells;
use crate::domain::models::conversation::Conversation;

/// Render the bookmark list panel into `area`. No-op when `bookmarks` is
/// empty — the event loop should flash "No bookmarks" instead of opening
/// the panel in that case (see `apply_open_bookmark_list`).
pub fn render(
    frame: &mut Frame,
    area: Rect,
    conversation: &Conversation,
    bookmarks: &[usize],
    selected: usize,
    theme: &Theme,
) {
    if bookmarks.is_empty() || area.height < 3 {
        return;
    }

    frame.render_widget(Clear, area);

    // Compute how many entries fit: reserve 1 row for header, 1 for footer,
    // and account for the top/bottom border.
    let border_overhead: usize = 2; // top + bottom border
    let header_rows: usize = 1;
    let footer_rows: usize = 1;
    let visible_rows = (area.height as usize)
        .saturating_sub(border_overhead)
        .saturating_sub(header_rows)
        .saturating_sub(footer_rows);
    let visible_entries = visible_rows.min(bookmarks.len());
    let selected_clamped = selected.min(bookmarks.len().saturating_sub(1));

    // Scroll the visible window so the selected entry stays in view.
    let scroll_offset = if visible_entries == 0 {
        0
    } else if selected_clamped >= visible_entries {
        selected_clamped + 1 - visible_entries
    } else {
        0
    };

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        format!("Bookmarks ({})", bookmarks.len()),
        Style::default()
            .fg(theme.colors.bookmark_accent)
            .add_modifier(Modifier::BOLD),
    )));

    // AC10: shortened to the display cells left in the row after the borders,
    // the `▸ ` prefix and the `msg K: ` label (derived per row below). The
    // domain `shorten_text` is a scalar source cap and is not used here.
    let inner_width = (area.width as usize).saturating_sub(2);
    for i in 0..visible_entries {
        let entry_idx = scroll_offset + i;
        if entry_idx >= bookmarks.len() {
            break;
        }
        let msg_idx = bookmarks[entry_idx];
        let first_line = conversation
            .live_message_index(msg_idx)
            .and_then(|index| conversation.messages.get(index))
            .map(|m| m.content.lines().next().unwrap_or("").to_string())
            .unwrap_or_else(|| "(missing message)".to_string());
        let prefix = if entry_idx == selected_clamped {
            "▸ "
        } else {
            "  "
        };
        // Real content budget: inside the borders, after the row prefix and
        // the `msg K: ` label (whose width follows the index digits).
        let label = format!("msg {}: ", msg_idx);
        let prefix_cells = geom::cells(prefix) + geom::cells(&label);
        let content_budget = inner_width.saturating_sub(prefix_cells);
        let shortened = shorten_to_cells(&first_line, content_budget);
        let row_style = if entry_idx == selected_clamped {
            Style::default()
                .fg(theme.colors.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.colors.fg_primary)
        };
        // Display the durable zero-based index stored in SessionMeta.
        // Third-audit Fix R1: reverted from `msg_idx + 1` to honor the
        // original AC10 spec. A future UX discussion about 1-based display
        // for end users is tracked as DF-126 (party-mode third-audit).
        lines.push(Line::from(vec![
            Span::styled(prefix.to_string(), row_style),
            Span::styled(format!("{label}{shortened}"), row_style),
        ]));
    }

    lines.push(Line::from(Span::styled(
        "[Enter] Jump  [d/Del] Delete  [u] Undo  [Esc] Close".to_string(),
        Style::default().fg(theme.colors.fg_muted),
    )));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.colors.bookmark_accent))
        .title(Span::styled(
            " Bookmarks ",
            Style::default()
                .fg(theme.colors.bookmark_accent)
                .add_modifier(Modifier::BOLD),
        ));

    let paragraph = Paragraph::new(lines).block(block).style(
        Style::default()
            .fg(theme.colors.fg_primary)
            .bg(theme.colors.bg_secondary),
    );
    frame.render_widget(paragraph, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::MessageRole;
    use crate::domain::models::conversation::{ChatMessage, Conversation};

    fn msg(content: &str) -> ChatMessage {
        ChatMessage {
            id: "m".to_string(),
            role: MessageRole::User,
            content: content.to_string(),
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: 1_700_000_000,
            token_count: None,
            stop_reason: None,
            synthetic: false,
            images: vec![],
            origin: crate::domain::models::ChannelKind::Terminal,
            authorship: crate::domain::models::Authorship::HumanWritten,
            send_authorization: crate::domain::models::SendAuthorization::Unknown,
            retracted_at_ms: None,
            history_source: Default::default(),
        }
    }

    fn conv(messages: Vec<ChatMessage>) -> Conversation {
        Conversation {
            id: "c".to_string(),
            title: "Test".to_string(),
            messages,
            turns: Vec::new(),
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
            last_response_at: None,
            session_id: None,
            usage: None,
            plans: std::collections::HashMap::new(),
            fork_source: None,
            compaction: None,
            peer_row_controls: Default::default(),
            peer_row_revision: 0,
        }
    }

    #[test]
    fn render_noop_on_empty_bookmarks() {
        // Purely testing the function doesn't panic on empty input.
        let theme = Theme::dark();
        let c = conv(vec![msg("hello")]);
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(50, 10)).unwrap();
        term.draw(|frame| {
            render(frame, frame.area(), &c, &[], 0, &theme);
        })
        .unwrap();
    }

    #[test]
    fn render_noop_on_tiny_area() {
        let theme = Theme::dark();
        let c = conv(vec![msg("hello")]);
        // 2-row area is below the border_overhead + content minimum.
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(50, 2)).unwrap();
        term.draw(|frame| {
            render(frame, frame.area(), &c, &[0], 0, &theme);
        })
        .unwrap();
    }

    #[test]
    fn render_selected_entry_has_marker() {
        let theme = Theme::dark();
        let c = conv(vec![msg("first"), msg("second"), msg("third")]);
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(50, 10)).unwrap();
        term.draw(|frame| {
            render(frame, frame.area(), &c, &[0, 2], 1, &theme);
        })
        .unwrap();
        let buf = term.backend().buffer();
        let mut screen = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                screen.push_str(buf[(x, y)].symbol());
            }
            screen.push('\n');
        }
        // Header + both entries + footer visible.
        // Third-audit Fix R1: indices are 0-based per AC10, so bookmarks
        // [0, 2] render as "msg 0" and "msg 2".
        assert!(screen.contains("Bookmarks (2)"));
        assert!(screen.contains("msg 0"));
        assert!(screen.contains("msg 2"));
        // Selection marker on the second entry (idx=1, which is msg 2).
        assert!(screen.contains("▸ msg 2"));
    }

    // ── Story 21.11 AC4/K4: display-width correctness ──────────────────────

    /// Paint the real panel (`width` x 5: border, header, one entry, footer,
    /// border) and return the glyph rows plus the buffer.
    fn draw(
        width: u16,
        c: &Conversation,
        bookmark: usize,
    ) -> (Vec<String>, ratatui::buffer::Buffer) {
        let theme = Theme::dark();
        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 5)).unwrap();
        term.draw(|frame| {
            render(frame, frame.area(), c, &[bookmark], 0, &theme);
        })
        .unwrap();
        let buffer = term.backend().buffer().clone();
        let rows = (0..5u16)
            .map(|y| {
                let mut row = String::new();
                let mut x = 0u16;
                while x < width {
                    let symbol = buffer.cell((x, y)).unwrap().symbol();
                    row.push_str(symbol);
                    x += u16::try_from(geom::cells(symbol).max(1)).unwrap();
                }
                row
            })
            .collect();
        (rows, buffer)
    }

    #[test]
    fn display_width_bookmark_budget_follows_the_label_width() {
        // `msg 10: ` is one cell wider than `msg 0: `, so the same text gets
        // 28 cells instead of 29: the old fixed `-8` could not tell.
        let mut messages: Vec<ChatMessage> = (0..10).map(|_| msg("filler")).collect();
        messages.push(msg(&"y".repeat(29)));
        let c = conv(messages);
        let (rows, buffer) = draw(40, &c, 10);
        assert!(
            rows[2].starts_with(&format!("│▸ msg 10: {}...", "y".repeat(25))),
            "{:?}",
            rows[2]
        );
        assert_eq!(buffer.cell((39, 2)).unwrap().symbol(), "│");
        // The ASCII 29-cell text still fits whole beside the one-digit label.
        let c = conv(vec![msg(&"y".repeat(29))]);
        let (rows, _) = draw(40, &c, 0);
        assert!(
            rows[2].starts_with(&format!("│▸ msg 0: {}│", "y".repeat(29))),
            "{:?}",
            rows[2]
        );
    }
}
