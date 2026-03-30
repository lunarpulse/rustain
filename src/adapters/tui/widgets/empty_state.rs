use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

use crate::adapters::tui::theme::Theme;

/// Render the welcome/empty state message in the chat pane.
pub fn render(frame: &mut Frame, area: Rect, theme: &Theme) {
    let text = vec![
        Line::from(""),
        Line::from(Span::styled(
            "Welcome to Rustain.",
            theme.typography.heading.fg(theme.colors.fg_primary),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Type a message to start.",
            theme.typography.meta.fg(theme.colors.fg_muted),
        )),
    ];

    let widget = Paragraph::new(text).alignment(Alignment::Center);
    frame.render_widget(widget, area);
}
