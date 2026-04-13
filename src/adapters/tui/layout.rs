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
    /// Tab bar row (shown when tab_count > 1 AND width >= 80).
    pub tab_bar: Option<Rect>,
    /// Sidebar column (shown when sidebar_visible AND terminal_width >= 120).
    pub sidebar: Option<Rect>,
}

/// Minimum terminal width to show sidebar.
pub const SIDEBAR_MIN_WIDTH: u16 = 120;

/// Compute the layout based on terminal size and tab count.
/// Input area height is dynamic based on content (multi-line support).
/// - >=120x24: full layout with optional sidebar
/// - >=80x24: full layout (with optional tab bar when tab_count > 1)
/// - 60x16 to 80x24: compact layout (same regions, minimal status, no tab bar)
/// - <60x16: returns None (terminal too small)
// Covers: FR16, UX-DR76, UX-DR41
pub fn compute_layout(
    area: Rect,
    _theme: &Theme,
    input: &str,
    tab_count: usize,
    sidebar_visible: bool,
) -> Option<AppLayout> {
    if area.width < 60 || area.height < 16 {
        return None;
    }

    let input_height = input_box::input_area_height(input, area.width);

    // Show tab bar when multiple tabs are open and terminal is wide enough
    let show_tab_bar = tab_count > 1 && area.width >= 80;

    // Calculate sidebar width: min(50, terminal_width * 0.3) with minimum of 30
    let show_sidebar = sidebar_visible && area.width >= SIDEBAR_MIN_WIDTH;
    let sidebar_width = if show_sidebar {
        ((area.width as f32) * 0.3).clamp(30.0, 50.0) as u16
    } else {
        0
    };

    if sidebar_width > 0 {
        // Split horizontally: main content | sidebar
        let main_chunks = Layout::horizontal([
            Constraint::Min(1),                // main content area
            Constraint::Length(sidebar_width), // sidebar
        ])
        .split(area);

        let main_area = main_chunks[0];
        let sidebar_rect = main_chunks[1];

        // Now split the main area vertically
        if show_tab_bar {
            let vertical_chunks = Layout::vertical([
                Constraint::Length(1),            // tab bar
                Constraint::Min(1),               // chat pane
                Constraint::Length(1),            // status bar
                Constraint::Length(input_height), // input area
            ])
            .split(main_area);

            Some(AppLayout {
                tab_bar: Some(vertical_chunks[0]),
                chat_pane: vertical_chunks[1],
                status_bar: vertical_chunks[2],
                input_area: vertical_chunks[3],
                sidebar: Some(sidebar_rect),
            })
        } else {
            let vertical_chunks = Layout::vertical([
                Constraint::Min(1),               // chat pane
                Constraint::Length(1),            // status bar
                Constraint::Length(input_height), // input area
            ])
            .split(main_area);

            Some(AppLayout {
                tab_bar: None,
                chat_pane: vertical_chunks[0],
                status_bar: vertical_chunks[1],
                input_area: vertical_chunks[2],
                sidebar: Some(sidebar_rect),
            })
        }
    } else if show_tab_bar {
        let chunks = Layout::vertical([
            Constraint::Length(1),            // tab bar
            Constraint::Min(1),               // chat pane (fills remaining)
            Constraint::Length(1),            // status bar
            Constraint::Length(input_height), // input area (dynamic)
        ])
        .split(area);

        Some(AppLayout {
            tab_bar: Some(chunks[0]),
            chat_pane: chunks[1],
            status_bar: chunks[2],
            input_area: chunks[3],
            sidebar: None,
        })
    } else {
        let chunks = Layout::vertical([
            Constraint::Min(1),               // chat pane (fills remaining)
            Constraint::Length(1),            // status bar
            Constraint::Length(input_height), // input area (dynamic)
        ])
        .split(area);

        Some(AppLayout {
            tab_bar: None,
            chat_pane: chunks[0],
            status_bar: chunks[1],
            input_area: chunks[2],
            sidebar: None,
        })
    }
}
