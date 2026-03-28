use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

/// Render the status bar with model name and current status.
pub fn render(frame: &mut Frame, area: Rect, model: &str, status: &str, _compact: bool) {
    let content = format!(" {} | {}", model, status);

    let widget =
        Paragraph::new(content).style(Style::default().fg(Color::DarkGray).bg(Color::Black));
    frame.render_widget(widget, area);
}
