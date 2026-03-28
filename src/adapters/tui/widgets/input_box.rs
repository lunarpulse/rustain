use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::domain::models::FocusState;

/// Render the text input area with cursor.
pub fn render(frame: &mut Frame, area: Rect, input: &str, cursor_pos: usize, focus: FocusState) {
    let is_focused = focus == FocusState::Input;
    let border_style = if is_focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let widget = Paragraph::new(input).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(border_style)
            .title(" Message "),
    );
    frame.render_widget(widget, area);

    if is_focused {
        let inner_width = area.width.saturating_sub(2);
        let clamped = (cursor_pos as u16).min(inner_width);
        frame.set_cursor_position((area.x.saturating_add(clamped).saturating_add(1), area.y + 1));
    }
}
