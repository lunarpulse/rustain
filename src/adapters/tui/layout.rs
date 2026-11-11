use ratatui::prelude::*;

use super::theme::Theme;
use super::widgets::input_box;

/// Minimum input area height (1 content line + 2 border rows) for an empty input buffer.
/// Tests that need to locate the status bar row use this constant via:
///   `status_row = terminal_height - MIN_INPUT_HEIGHT - 1`
#[allow(dead_code)] // used from tests/e2e_harness.rs; not referenced in the binary
pub const MIN_INPUT_HEIGHT: u16 = 3;

/// Rows a decision frame, sheet or toast needs from the Stage to paint (the 21-4 / 21-6
/// five-row frame floor: header, one body row, rail, two borders).
pub const MIN_FRAME_ROWS: u16 = 5;

/// Layout regions for the TUI frame.
pub struct AppLayout {
    pub chat_pane: Rect,
    /// The mission strip row directly above the status row (Story 21.7 A22): `None` when
    /// reserving it would leave the Stage fewer than [`MIN_FRAME_ROWS`] rows.
    pub mission_strip: Option<Rect>,
    pub status_bar: Rect,
    pub input_area: Rect,
    /// Tab bar row (shown when tab_count > 1 AND width >= 80).
    pub tab_bar: Option<Rect>,
    /// The read-only monitor column (shown when the monitor is on AND terminal_width >= 120).
    pub sidebar: Option<Rect>,
}

/// Minimum terminal width to show the monitor column.
pub const SIDEBAR_MIN_WIDTH: u16 = 120;

/// Width of the monitor column for a terminal `area_width` cells wide.
fn sidebar_column_width(area_width: u16) -> u16 {
    ((area_width as f32) * 0.3).clamp(30.0, 50.0) as u16
}

/// Width of the composer rectangle `compute_layout` allocates: the whole
/// terminal width, except that a painted monitor column (the monitor is on and the
/// terminal is wide enough) narrows the main column that holds the composer.
/// Composer height, wrapping and caret math must use this width, not the
/// terminal width (Story 21.11).
pub fn composer_width(area_width: u16, monitor: bool) -> u16 {
    if monitor && area_width >= SIDEBAR_MIN_WIDTH {
        area_width.saturating_sub(sidebar_column_width(area_width))
    } else {
        area_width
    }
}

/// Compute the layout based on terminal size and tab count.
/// Input area height is dynamic based on content (multi-line support).
/// - >=120x24: full layout with the optional monitor column (`monitor`)
/// - >=80x24: full layout (with optional tab bar when tab_count > 1)
/// - 60x16 to 80x24: compact layout (same regions, minimal status, no tab bar)
/// - <60x16: returns None (terminal too small)
///
/// The main column is, top to bottom: the tab bar (when shown), the Stage, the mission strip
/// (one row, whenever the Stage keeps at least [`MIN_FRAME_ROWS`] rows after giving it up), the
/// status row and the composer.
// Covers: FR16, UX-DR76, UX-DR41
pub fn compute_layout(
    area: Rect,
    _theme: &Theme,
    input: &str,
    tab_count: usize,
    monitor: bool,
) -> Option<AppLayout> {
    if area.width < 60 || area.height < 16 {
        return None;
    }

    let input_height = input_box::input_area_height(input, composer_width(area.width, monitor));

    // Show tab bar when multiple tabs are open and terminal is wide enough
    let show_tab_bar = tab_count > 1 && area.width >= 80;

    // The column is painted iff the monitor is on and the terminal is wide enough; below the
    // minimum width the preference is kept but nothing is painted (A16).
    let sidebar_width = if monitor && area.width >= SIDEBAR_MIN_WIDTH {
        sidebar_column_width(area.width)
    } else {
        0
    };
    let (main_area, sidebar) = if sidebar_width > 0 {
        let columns = Layout::horizontal([
            Constraint::Min(1),                // main content area
            Constraint::Length(sidebar_width), // monitor column
        ])
        .split(area);
        (columns[0], Some(columns[1]))
    } else {
        (area, None)
    };

    let tab_rows = u16::from(show_tab_bar);
    let stage_rows = main_area.height.saturating_sub(tab_rows + 1 + input_height);
    let strip_rows = u16::from(stage_rows > MIN_FRAME_ROWS);
    let rows = Layout::vertical([
        Constraint::Length(tab_rows),     // tab bar
        Constraint::Min(1),               // chat pane (fills remaining)
        Constraint::Length(strip_rows),   // mission strip
        Constraint::Length(1),            // status bar
        Constraint::Length(input_height), // input area (dynamic)
    ])
    .split(main_area);

    Some(AppLayout {
        tab_bar: show_tab_bar.then_some(rows[0]),
        chat_pane: rows[1],
        mission_strip: (strip_rows > 0).then_some(rows[2]),
        status_bar: rows[3],
        input_area: rows[4],
        sidebar,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_theme() -> Theme {
        Theme::for_capability(crate::adapters::tui::color_detect::ColorCapability::TrueColor)
    }

    #[test]
    fn story21_7_the_column_is_painted_iff_the_monitor_is_on_and_the_terminal_is_wide_enough() {
        let theme = test_theme();
        let wide = Rect::new(0, 0, 120, 30);
        let on = compute_layout(wide, &theme, "", 1, true).unwrap();
        let column = on
            .sidebar
            .expect("monitor on at 120 columns paints the column");
        assert_eq!(column.width, 36, "30% of 120");
        assert_eq!(on.chat_pane.width + column.width, 120);
        assert!(
            compute_layout(wide, &theme, "", 1, false)
                .unwrap()
                .sidebar
                .is_none(),
            "monitor off: no column at any width"
        );
        // The preference is kept below the floor, the column is simply not painted (A16).
        let narrow = Rect::new(0, 0, 119, 30);
        let layout = compute_layout(narrow, &theme, "", 1, true).unwrap();
        assert!(layout.sidebar.is_none(), "119 columns: nothing painted");
        assert_eq!(layout.chat_pane.width, 119);
    }

    #[test]
    fn story21_7_composer_width_excludes_exactly_the_painted_column() {
        let theme = test_theme();
        for width in [60u16, 80, 119, 120, 130, 200] {
            for monitor in [false, true] {
                let layout =
                    compute_layout(Rect::new(0, 0, width, 30), &theme, "", 1, monitor).unwrap();
                assert_eq!(
                    composer_width(width, monitor),
                    layout.input_area.width,
                    "{width} monitor={monitor}: one rule for the composer and the layout"
                );
            }
        }
    }
}
