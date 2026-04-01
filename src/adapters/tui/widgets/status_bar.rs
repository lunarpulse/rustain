use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

use crate::adapters::tui::theme::Theme;

/// Render the status bar with model name and current status.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    model: &str,
    status: &str,
    compact: bool,
    theme: &Theme,
) {
    // Both compact and standard modes currently show model + status only.
    // Compact branch will diverge when additional status info is added.
    let _ = compact;
    let content = format!(" {} | {}", model, status);

    let fg = if status.contains("Streaming") {
        theme.colors.status_streaming
    } else {
        theme.colors.status_fg
    };

    let widget = Paragraph::new(content).style(Style::default().fg(fg).bg(theme.colors.status_bg));
    frame.render_widget(widget, area);
}
