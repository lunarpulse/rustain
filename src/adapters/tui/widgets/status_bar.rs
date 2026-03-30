use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

use crate::adapters::tui::theme::Theme;

/// Render the status bar with model name and current status.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    model: &str,
    status: &str,
    _compact: bool,
    theme: &Theme,
) {
    let content = format!(" {} | {}", model, status);

    let widget = Paragraph::new(content).style(
        Style::default()
            .fg(theme.colors.status_fg)
            .bg(theme.colors.status_bg),
    );
    frame.render_widget(widget, area);
}
