use ratatui::prelude::*;

use super::theme::Theme;
use super::widgets::input_box;

/// Minimum input area height (1 content line + 2 border rows) for an empty input buffer.
/// Tests that need to locate the status bar row use this constant via:
///   `status_row = terminal_height - MIN_INPUT_HEIGHT - 1`
#[allow(dead_code)] // used from tests/e2e_harness.rs; not referenced in the binary
pub const MIN_INPUT_HEIGHT: u16 = 3;

/// Layout regions for the TUI frame.
pub struct AppLayout {
    pub chat_pane: Rect,
    pub status_bar: Rect,
    pub input_area: Rect,
}

/// Compute the three-region layout based on terminal size.
/// Input area height is dynamic based on content (multi-line support).
/// - >=80x24: full layout
/// - 60x16 to 80x24: compact layout (same regions, minimal status)
/// - <60x16: returns None (terminal too small)
// Covers: FR16, UX-DR76
pub fn compute_layout(area: Rect, _theme: &Theme, input: &str) -> Option<AppLayout> {
    if area.width < 60 || area.height < 16 {
        return None;
    }

    let input_height = input_box::input_area_height(input, area.width);

    let chunks = Layout::vertical([
        Constraint::Min(1),               // chat pane (fills remaining)
        Constraint::Length(1),            // status bar
        Constraint::Length(input_height), // input area (dynamic)
    ])
    .split(area);

    Some(AppLayout {
        chat_pane: chunks[0],
        status_bar: chunks[1],
        input_area: chunks[2],
    })
}
