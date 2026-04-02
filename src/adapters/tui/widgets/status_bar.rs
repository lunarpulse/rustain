use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

use crate::adapters::tui::theme::Theme;
use crate::adapters::tui::widgets::chat_pane::virtual_scroll::offset_to_message_index;
use crate::domain::models::PermissionMode;

/// Render the status bar with model name, current status, scroll position, and permission mode.
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
    permission_mode: PermissionMode,
) {
    let _ = compact;

    // Build left side: model + status
    let mut left_spans = vec![
        Span::styled(
            format!(" {} | {}", model, status),
            Style::default().fg(if status.contains("Streaming") {
                theme.colors.status_streaming
            } else {
                theme.colors.status_fg
            }),
        ),
    ];

    // AC4: Show scroll position indicator when scrolled
    if scroll_offset > 0 && !message_boundaries.is_empty() {
        let (current, total) = offset_to_message_index(
            scroll_offset,
            viewport_height,
            message_boundaries,
            total_content_height,
        );
        left_spans.push(Span::styled(
            format!(" | msg {}/{}", current, total),
            Style::default().fg(theme.colors.status_fg),
        ));
    }

    // Permission mode indicator on the right side
    let mode_span = match permission_mode {
        PermissionMode::Normal => Span::styled(
            " Normal ",
            Style::default()
                .fg(theme.colors.status_fg)
                .bg(theme.colors.status_bg),
        ),
        PermissionMode::Yolo => Span::styled(
            " YOLO ",
            Style::default()
                .fg(Color::White)
                .bg(theme.colors.status_yolo_warning)
                .add_modifier(Modifier::BOLD),
        ),
    };

    // Calculate available width for spacing
    let left_width: usize = left_spans.iter().map(|s| s.content.len()).sum();
    let right_width = mode_span.content.len();
    let padding = (area.width as usize)
        .saturating_sub(left_width)
        .saturating_sub(right_width);

    left_spans.push(Span::styled(
        " ".repeat(padding),
        Style::default().bg(theme.colors.status_bg),
    ));
    left_spans.push(mode_span);

    let line = Line::from(left_spans);
    let widget = Paragraph::new(line).style(Style::default().bg(theme.colors.status_bg));
    frame.render_widget(widget, area);
}
