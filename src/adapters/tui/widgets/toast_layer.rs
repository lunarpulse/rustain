//! Toast layer (Story 21.1 AC3, A5, RU-16).
//!
//! Toasts paint inside the chat pane's bottom edge (directly above the status
//! row), newest at the bottom, right-aligned. They reserve no rows and change
//! no layout, scroll or height-cache key: the caller paints them AFTER the
//! Stage, status row and composer and BEFORE every overlay, so a modal always
//! covers a toast. Widths and clipping use `text_geometry` only.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::adapters::tui::notification_log::LogFacts;
use crate::adapters::tui::notifications::{
    MAX_VISIBLE_TOASTS, NoticeRecord, NoticeSeverity, NotificationStore, headline,
};
use crate::adapters::tui::text_geometry as geom;
use crate::adapters::tui::theme::Theme;

/// Below this pane width a toast is one borderless row; at or above it a
/// bordered box no wider than half the pane.
pub const WIDE_PANE_COLUMNS: u16 = 100;
/// The bordered form needs the three rows per toast it paints.
const WIDE_PANE_MIN_ROWS: u16 = 8;

/// Visible-toast cap for a pane of this height: `min(3, max(1, height / 4))`.
#[must_use]
pub fn capacity_for(pane_height: u16) -> usize {
    MAX_VISIBLE_TOASTS.min(usize::from((pane_height / 4).max(1)))
}

fn severity_color(theme: &Theme, severity: NoticeSeverity) -> Color {
    match severity {
        NoticeSeverity::Info => theme.colors.info,
        NoticeSeverity::Success => theme.colors.success,
        NoticeSeverity::Warning => theme.colors.warning,
        NoticeSeverity::Error => theme.colors.error,
    }
}

/// The painted text of one toast: severity word, `[conversation] ` when the
/// notice belongs to a background conversation, first line, ` ×N`.
fn toast_text(record: &NoticeRecord, facts: &LogFacts) -> (String, String) {
    let head = format!("{} ", record.severity.word());
    let mut body = String::new();
    if let Some(conversation_id) = &record.conversation_id
        && facts.active_conversation_id.as_deref() != Some(conversation_id.as_str())
    {
        body.push_str(&format!("[{}] ", facts.title_of(conversation_id)));
    }
    body.push_str(&headline(&record.message));
    if record.count > 1 {
        body.push_str(&format!(" ×{}", record.count));
    }
    (head, body)
}

/// Paint the store's visible toasts into `pane`. Paints nothing when there is
/// nothing visible or the pane cannot hold a row. `facts` names the active
/// conversation: a toast of any other conversation is attributed.
pub fn render(
    frame: &mut Frame,
    pane: Rect,
    store: &NotificationStore,
    theme: &Theme,
    facts: &LogFacts,
) {
    if pane.width < 4 || pane.height == 0 {
        return;
    }
    let wide = pane.width >= WIDE_PANE_COLUMNS && pane.height >= WIDE_PANE_MIN_ROWS;
    let toasts: Vec<&NoticeRecord> = store.visible().collect();
    let take = capacity_for(pane.height).min(toasts.len());
    let shown = &toasts[toasts.len() - take..];
    for (offset, record) in shown.iter().rev().enumerate() {
        let (head, body) = toast_text(record, facts);
        let color = severity_color(theme, record.severity);
        let head_style = Style::default().fg(color).add_modifier(Modifier::BOLD);
        let body_style = Style::default().fg(theme.colors.fg_primary);
        let surface = Style::default().bg(theme.colors.bg_surface);
        if wide {
            let max_width = usize::from(pane.width / 2).max(8);
            let wanted = geom::cells(&head) + geom::cells(&body) + 2;
            let width = wanted.min(max_width);
            let inner = width.saturating_sub(2);
            let budget = inner.saturating_sub(geom::cells(&head));
            let body = geom::ellipsize(&body, budget, "…").into_owned();
            let rows = u16::try_from(offset + 1)
                .unwrap_or(u16::MAX)
                .saturating_mul(3);
            let Some(y) = pane.bottom().checked_sub(rows) else {
                break;
            };
            let width = u16::try_from(width).unwrap_or(pane.width);
            let area = Rect::new(pane.right() - width, y, width, 3);
            frame.render_widget(Clear, area);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(color))
                .style(surface);
            let line = Line::from(vec![
                Span::styled(head, head_style),
                Span::styled(body, body_style),
            ]);
            frame.render_widget(Paragraph::new(line).block(block), area);
        } else {
            let width = usize::from(pane.width);
            let head_cells = geom::cells(&head);
            let body = geom::ellipsize(&body, width.saturating_sub(head_cells), "…").into_owned();
            let used = (head_cells + geom::cells(&body)).min(width);
            let Some(y) = pane
                .bottom()
                .checked_sub(u16::try_from(offset + 1).unwrap_or(u16::MAX))
            else {
                break;
            };
            let used = u16::try_from(used).unwrap_or(pane.width);
            let area = Rect::new(pane.right() - used, y, used, 1);
            frame.render_widget(Clear, area);
            let line = Line::from(vec![
                Span::styled(head, head_style),
                Span::styled(body, body_style),
            ]);
            frame.render_widget(Paragraph::new(line).style(surface), area);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn story21_1_capacity_follows_pane_height() {
        assert_eq!(capacity_for(0), 1);
        assert_eq!(capacity_for(3), 1);
        assert_eq!(capacity_for(8), 2);
        assert_eq!(capacity_for(12), 3);
        assert_eq!(capacity_for(40), 3);
    }

    #[test]
    fn story21_1_toast_body_is_the_headline_of_the_message() {
        let now = std::time::Instant::now();
        let record = NoticeRecord {
            id: 1,
            conversation_id: None,
            source: crate::adapters::tui::notifications::NoticeSource::System,
            severity: NoticeSeverity::Info,
            message: "\n\nreal text".to_owned(),
            first_at: now,
            last_at: now,
            first_wall_ms: 0,
            last_wall_ms: 0,
            count: 1,
            deadline: now,
            presentation: crate::adapters::tui::notifications::Presentation::Visible,
            action: None,
        };
        let (head, body) = toast_text(&record, &LogFacts::default());
        assert_eq!(head, "Info ");
        assert_eq!(body, "real text");
    }
}
