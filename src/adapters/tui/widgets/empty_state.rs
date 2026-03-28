use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

/// Render the welcome/empty state message in the chat pane.
pub fn render(frame: &mut Frame, area: Rect) {
    let text = vec![
        Line::from(""),
        Line::from(Span::styled(
            "Welcome to Rustain.",
            Style::default().fg(Color::White).bold(),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Type a message to start.",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let widget = Paragraph::new(text).alignment(Alignment::Center);
    frame.render_widget(widget, area);
}
