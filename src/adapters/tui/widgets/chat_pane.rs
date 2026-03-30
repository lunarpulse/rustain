use ratatui::prelude::*;

use crate::adapters::tui::theme::Theme;

use super::empty_state;

/// Render the chat pane. When no messages exist, delegates to empty_state.
pub fn render(frame: &mut Frame, area: Rect, has_messages: bool, theme: &Theme) {
    if has_messages {
        // Future stories render message list here
    } else {
        empty_state::render(frame, area, theme);
    }
}
