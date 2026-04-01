use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

use crate::adapters::tui::theme::Theme;
use crate::adapters::tui::widgets::chat_pane::virtual_scroll::offset_to_message_index;

/// Render the status bar with model name, current status, and scroll position.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    model: &str,
    status: &str,
    compact: bool,
    theme: &Theme,
    scroll_offset: usize,
    message_boundaries: &[usize],
    total_content_height: usize,
    viewport_height: u16,
) {
    let _ = compact;
    let mut content = format!(" {} | {}", model, status);

    // AC4: Show scroll position indicator when scrolled
    if scroll_offset > 0 && !message_boundaries.is_empty() {
        let (current, total) = offset_to_message_index(
            scroll_offset,
            viewport_height,
            message_boundaries,
            total_content_height,
        );
        content.push_str(&format!(" | msg {}/{}", current, total));
    }

    let fg = if status.contains("Streaming") {
        theme.colors.status_streaming
    } else {
        theme.colors.status_fg
    };

    let widget = Paragraph::new(content).style(Style::default().fg(fg).bg(theme.colors.status_bg));
    frame.render_widget(widget, area);
}
