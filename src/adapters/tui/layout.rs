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
    /// Search bar row, reserved at the top of `chat_pane` when the search
    /// overlay is active (Story 4-4 AC1). `None` when inactive.
    pub search_bar: Option<Rect>,
    /// Bookmark list panel, reserved at the bottom of `chat_pane` when the
    /// bookmark list overlay is active (Story 4-4 AC10). `None` when inactive.
    /// Populated by `reserve_bookmark_panel`, which is wired up in Task 5
    /// (bookmark list widget). The layout API is ready now so Task 5 doesn't
    /// need to re-touch `compute_layout`.
    #[allow(dead_code)]
    pub bookmark_panel: Option<Rect>,
    /// Dashboard-mode panel area (Story 8.4b AC-4). When in Dashboard mode,
    /// 70% of the content area is reserved for panel widgets; the bottom 30%
    /// is compact chat. `None` in Focus/Monitor modes.
    pub dashboard_panel: Option<Rect>,
}

impl AppLayout {
    /// Reserve a 1-row slot at the top of `chat_pane` for the search bar.
    ///
    /// Shrinks `chat_pane` downward by 1 row and populates `search_bar` with
    /// the reserved row. Idempotent-ish: safe to call multiple times but the
    /// caller should only call once per frame with the current `active` flag.
    ///
    /// No-op when `active == false` or `chat_pane.height < 2`.
    // Covers: Story 4-4 AC1 layout reservation
    pub fn reserve_search_bar(&mut self, active: bool) {
        if !active || self.chat_pane.height < 2 {
            return;
        }
        let bar = Rect {
            x: self.chat_pane.x,
            y: self.chat_pane.y,
            width: self.chat_pane.width,
            height: 1,
        };
        self.chat_pane = Rect {
            x: self.chat_pane.x,
            y: self.chat_pane.y + 1,
            width: self.chat_pane.width,
            height: self.chat_pane.height - 1,
        };
        self.search_bar = Some(bar);
    }

    /// Reserve a bottom panel slot of up to `requested_height` rows in
    /// `chat_pane` for the bookmark list.
    ///
    /// Shrinks `chat_pane` upward, capping at half of `chat_pane.height` so
    /// the conversation is never completely hidden. No-op when
    /// `requested_height == 0` or `chat_pane.height < 4` (not enough room).
    ///
    /// Wired up by Task 5 (bookmark list widget). Exercised by `tests` below
    /// so the method body is compiled and verified even while unused from the
    /// production code path.
    // Covers: Story 4-4 AC10 bottom panel layout
    #[allow(dead_code)]
    pub fn reserve_bookmark_panel(&mut self, requested_height: u16) {
        if requested_height == 0 || self.chat_pane.height < 4 {
            return;
        }
        let max_allowed = self.chat_pane.height / 2;
        let panel_height = requested_height.min(max_allowed).max(1);
        let panel = Rect {
            x: self.chat_pane.x,
            y: self.chat_pane.y + self.chat_pane.height - panel_height,
            width: self.chat_pane.width,
            height: panel_height,
        };
        self.chat_pane = Rect {
            x: self.chat_pane.x,
            y: self.chat_pane.y,
            width: self.chat_pane.width,
            height: self.chat_pane.height - panel_height,
        };
        self.bookmark_panel = Some(panel);
    }
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
    density_mode: crate::domain::models::visual::DensityMode,
) -> Option<AppLayout> {
    if area.width < 60 || area.height < 16 {
        return None;
    }

    let input_height = input_box::input_area_height(input, area.width);

    // Show tab bar when multiple tabs are open and terminal is wide enough
    let show_tab_bar = tab_count > 1 && area.width >= 80;

    // Dashboard mode: full-width panel (70%) + compact chat (30%), no sidebar
    if density_mode == crate::domain::models::visual::DensityMode::Dashboard {
        let tab_bar_height: u16 = if show_tab_bar { 1 } else { 0 };
        let content_height = area
            .height
            .saturating_sub(input_height + 1 + tab_bar_height); // 1 status bar
        let panel_height = content_height * 7 / 10;
        let chat_height = content_height - panel_height;

        let mut y = area.y;
        let tab_bar = if show_tab_bar {
            let r = Rect::new(area.x, y, area.width, 1);
            y += 1;
            Some(r)
        } else {
            None
        };
        let dashboard_panel = Some(Rect::new(area.x, y, area.width, panel_height));
        y += panel_height;
        let chat_pane = Rect::new(area.x, y, area.width, chat_height);
        y += chat_height;
        let status_bar = Rect::new(area.x, y, area.width, 1);
        y += 1;
        let input_area = Rect::new(area.x, y, area.width, input_height);

        return Some(AppLayout {
            tab_bar,
            chat_pane,
            status_bar,
            input_area,
            sidebar: None,
            search_bar: None,
            bookmark_panel: None,
            dashboard_panel,
        });
    }

    // Focus mode forces sidebar hidden regardless of the sidebar_visible argument
    let effective_sidebar_visible =
        if density_mode == crate::domain::models::visual::DensityMode::Focus {
            false
        } else if density_mode == crate::domain::models::visual::DensityMode::Monitor {
            sidebar_visible
        } else {
            sidebar_visible
        };

    // Calculate sidebar width: min(50, terminal_width * 0.3) with minimum of 30
    let show_sidebar = effective_sidebar_visible && area.width >= SIDEBAR_MIN_WIDTH;
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
                search_bar: None,
                bookmark_panel: None,
                dashboard_panel: None,
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
                search_bar: None,
                bookmark_panel: None,
                dashboard_panel: None,
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
            search_bar: None,
            bookmark_panel: None,
            dashboard_panel: None,
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
            search_bar: None,
            bookmark_panel: None,
            dashboard_panel: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_layout() -> AppLayout {
        AppLayout {
            chat_pane: Rect {
                x: 0,
                y: 0,
                width: 80,
                height: 20,
            },
            status_bar: Rect {
                x: 0,
                y: 20,
                width: 80,
                height: 1,
            },
            input_area: Rect {
                x: 0,
                y: 21,
                width: 80,
                height: 3,
            },
            tab_bar: None,
            sidebar: None,
            search_bar: None,
            bookmark_panel: None,
            dashboard_panel: None,
        }
    }

    #[test]
    fn reserve_search_bar_shrinks_chat_pane_by_one_row() {
        let mut l = test_layout();
        let original_height = l.chat_pane.height;
        l.reserve_search_bar(true);
        assert_eq!(l.chat_pane.height, original_height - 1);
        assert_eq!(l.chat_pane.y, 1);
        assert!(l.search_bar.is_some());
        let bar = l.search_bar.unwrap();
        assert_eq!(bar.y, 0);
        assert_eq!(bar.height, 1);
    }

    #[test]
    fn reserve_search_bar_noop_when_inactive() {
        let mut l = test_layout();
        let before = l.chat_pane;
        l.reserve_search_bar(false);
        assert_eq!(l.chat_pane, before);
        assert!(l.search_bar.is_none());
    }

    #[test]
    fn reserve_search_bar_noop_when_chat_pane_too_small() {
        let mut l = test_layout();
        l.chat_pane.height = 1;
        l.reserve_search_bar(true);
        assert!(l.search_bar.is_none());
    }

    #[test]
    fn reserve_bookmark_panel_shrinks_chat_pane_upward() {
        let mut l = test_layout();
        l.reserve_bookmark_panel(6);
        assert_eq!(l.chat_pane.height, 14);
        assert!(l.bookmark_panel.is_some());
        let panel = l.bookmark_panel.unwrap();
        assert_eq!(panel.height, 6);
        assert_eq!(panel.y, 14);
    }

    #[test]
    fn reserve_bookmark_panel_caps_at_half_chat_pane() {
        let mut l = test_layout();
        // 20-row chat pane; requesting 15 rows clamps to 10 (half).
        l.reserve_bookmark_panel(15);
        assert_eq!(l.bookmark_panel.unwrap().height, 10);
        assert_eq!(l.chat_pane.height, 10);
    }

    #[test]
    fn reserve_bookmark_panel_noop_when_zero_height() {
        let mut l = test_layout();
        let before = l.chat_pane;
        l.reserve_bookmark_panel(0);
        assert_eq!(l.chat_pane, before);
        assert!(l.bookmark_panel.is_none());
    }

    #[test]
    fn both_reservations_combine() {
        let mut l = test_layout();
        l.reserve_search_bar(true);
        l.reserve_bookmark_panel(4);
        assert!(l.search_bar.is_some());
        assert!(l.bookmark_panel.is_some());
        // Top row = search bar, bottom 4 rows = bookmark panel, middle = chat
        assert_eq!(l.chat_pane.y, 1);
        assert_eq!(l.chat_pane.height, 20 - 1 - 4);
    }

    // Story 8.4b layout tests

    fn test_theme() -> Theme {
        Theme::for_capability(crate::adapters::tui::color_detect::ColorCapability::TrueColor)
    }

    #[test]
    fn dashboard_layout_splits_70_30() {
        let area = Rect::new(0, 0, 120, 30);
        let theme = test_theme();
        let layout = compute_layout(
            area,
            &theme,
            "",
            1,
            false,
            crate::domain::models::visual::DensityMode::Dashboard,
        )
        .unwrap();
        assert!(
            layout.dashboard_panel.is_some(),
            "Dashboard mode must populate dashboard_panel"
        );
        let panel = layout.dashboard_panel.unwrap();
        let chat = layout.chat_pane;
        // Content area = 30 - 1 status - 3 input = 26
        // Panel = 26 * 7/10 = 18, Chat = 8
        assert!(layout.sidebar.is_none(), "Dashboard: no sidebar");
        assert_eq!(panel.height, 18);
        assert_eq!(chat.height, 8);
        assert_eq!(panel.y + panel.height, chat.y);
    }

    #[test]
    fn focus_forces_sidebar_hidden() {
        let area = Rect::new(0, 0, 120, 30);
        let theme = test_theme();
        let layout = compute_layout(
            area,
            &theme,
            "",
            1,
            true, // sidebar_visible=true
            crate::domain::models::visual::DensityMode::Focus,
        )
        .unwrap();
        assert!(
            layout.sidebar.is_none(),
            "Focus mode must hide sidebar regardless of flag"
        );
    }

    #[test]
    fn monitor_preserves_sidebar_behavior() {
        let area = Rect::new(0, 0, 120, 30);
        let theme = test_theme();
        let with_sidebar = compute_layout(
            area,
            &theme,
            "",
            1,
            true,
            crate::domain::models::visual::DensityMode::Monitor,
        )
        .unwrap();
        assert!(
            with_sidebar.sidebar.is_some(),
            "Monitor with visible flag: sidebar shown"
        );

        let without_sidebar = compute_layout(
            area,
            &theme,
            "",
            1,
            false,
            crate::domain::models::visual::DensityMode::Monitor,
        )
        .unwrap();
        assert!(
            without_sidebar.sidebar.is_none(),
            "Monitor with hidden flag: sidebar hidden"
        );
    }
}
