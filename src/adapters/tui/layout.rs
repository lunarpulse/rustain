use ratatui::prelude::*;

use super::theme::Theme;

/// Layout regions for the TUI frame.
pub struct AppLayout {
    pub chat_pane: Rect,
    pub status_bar: Rect,
    pub input_area: Rect,
}

/// Compute the three-region layout based on terminal size.
/// - >=80x24: full layout
/// - 60x16 to 80x24: compact layout (same regions, minimal status)
/// - <60x16: returns None (terminal too small)
pub fn compute_layout(area: Rect, _theme: &Theme) -> Option<AppLayout> {
    if area.width < 60 || area.height < 16 {
        return None;
    }

    let chunks = Layout::vertical([
        Constraint::Min(1),    // chat pane (fills remaining)
        Constraint::Length(1), // status bar
        Constraint::Length(3), // input area
    ])
    .split(area);

    Some(AppLayout {
        chat_pane: chunks[0],
        status_bar: chunks[1],
        input_area: chunks[2],
    })
}
