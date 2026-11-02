//! K4 A1: Stage-edge region, caller-owned frame geometry, five-row fit and Ready priority.
use super::k3_compose::{draw, layout, open_quit, row};
use ratatui::{Frame, Terminal, backend::TestBackend, layout::Rect};
use rustain::adapters::tui::{
    layout::compute_layout,
    notification_log,
    notifications::{self, NoticeSeverity, NoticeSource},
    presentation::{Shell, geometry},
    state::{LogAwareness, TuiState},
    theme::Theme,
    widgets::status_bar,
};
use rustain::domain::models::{PermissionMode, StatusState, visual::DensityMode};

#[test]
fn region_ends_above_the_status_row() {
    // Every layout branch, including the optional tab/sidebar combinations.
    // Offset origins catch confusing Stage height with its absolute bottom.
    let witnesses = [
        ("focus", 80, 1, false, DensityMode::Focus),
        ("tabs", 80, 2, false, DensityMode::Focus),
        ("sidebar", 130, 1, true, DensityMode::Monitor),
        ("sidebar-tabs", 130, 2, true, DensityMode::Monitor),
        ("monitor-narrow", 80, 1, true, DensityMode::Monitor),
        ("dashboard", 130, 1, true, DensityMode::Dashboard),
        ("dashboard-tabs", 130, 2, true, DensityMode::Dashboard),
    ];
    for (name, width, tabs, sidebar, density) in witnesses {
        for full in [false, true] {
            let area = Rect::new(3, 2, width, 30);
            let input = if full {
                ["draft"; 8].join("\n")
            } else {
                String::new()
            };
            let layout =
                compute_layout(area, &Theme::dark(), &input, tabs, sidebar, density).unwrap();
            assert_eq!(layout.tab_bar.is_some(), tabs > 1, "{name}");
            assert_eq!(
                layout.sidebar.is_some(),
                name.starts_with("sidebar"),
                "{name}"
            );
            assert_eq!(
                layout.dashboard_panel.is_some(),
                density == DensityMode::Dashboard
            );
            assert_eq!(layout.input_area.height, if full { 10 } else { 3 });
            let region = geometry::modal_region(&layout, area);
            assert_eq!(
                region,
                Rect::new(
                    area.x,
                    area.y,
                    area.width,
                    layout.chat_pane.bottom() - area.y
                ),
                "{name}, full={full}"
            );
            assert_eq!(
                region.bottom(),
                layout.status_bar.y,
                "M5: status must be excluded"
            );
            assert_eq!(region.intersection(layout.status_bar).height, 0);
            // Assert the public caller's geometry too: an unused correct helper is not enough.
            let shell = Shell::new(area, &layout);
            assert_eq!(shell.region, region, "{name}: Shell must use modal_region");
            assert_eq!(
                (shell.chat_pane, shell.input_area, shell.status_bar),
                (layout.chat_pane, layout.input_area, layout.status_bar)
            );
        }
    }
}

#[test]
fn standard_frames_are_full_sheets_below_100_and_ninety_by_eighty_five_at_100() {
    // Literal independent rectangles pin both sides of the breakpoint and centering.
    for (region, expected) in [
        (Rect::new(3, 2, 60, 5), Rect::new(3, 2, 60, 5)),
        (Rect::new(3, 2, 99, 20), Rect::new(3, 2, 99, 20)),
        (Rect::new(3, 2, 100, 20), Rect::new(8, 3, 90, 17)),
        (Rect::new(3, 2, 130, 26), Rect::new(9, 4, 117, 22)),
    ] {
        assert_eq!(geometry::frame_rect(region), expected);
    }
}

#[test]
fn log_and_quit_keep_header_body_and_footer_in_a_minimum_height_frame() {
    for (width, quit, expected) in [
        (60, false, Rect::new(0, 0, 60, 5)),
        (60, true, Rect::new(0, 0, 60, 5)),
        (100, false, Rect::new(5, 0, 90, 5)),
        (100, true, Rect::new(5, 0, 90, 5)),
    ] {
        let mut state = TuiState::new(width, 16);
        state.input_buffer = ["draft"; 8].join("\n");
        let layout = layout(&state);
        let region =
            geometry::frame_rect(geometry::modal_region(&layout, Rect::new(0, 0, width, 16)));
        assert_eq!(region, expected);
        let (title, header, body, footer) = if quit {
            open_quit(&mut state);
            ("System · Quit", "Quit rustain?", "running turn", "Stay")
        } else {
            notifications::notify(
                &mut state,
                NoticeSource::System,
                NoticeSeverity::Info,
                "body-witness",
            );
            notification_log::open(&mut state);
            (
                "System · Notifications",
                "Notifications (1)",
                "body-witness",
                "Close",
            )
        };
        let buffer = draw(&state, &layout, Some(title));
        assert!(
            row(&buffer, region, region.y + 1).contains(header),
            "{title}: header"
        );
        assert!(
            row(&buffer, region, region.y + 2).contains(body),
            "{title}: body"
        );
        assert!(
            row(&buffer, region, region.y + 3).contains(footer),
            "{title}: footer"
        );
        assert_eq!(buffer[(region.x, region.bottom() - 1)].symbol(), "└");
        assert_eq!(buffer[(region.right() - 1, region.y)].symbol(), "┐");
    }
}

#[test]
fn eligible_log_actions_cannot_starve_the_five_row_body() {
    use rustain::adapters::tui::{
        keymap::Host,
        notification_log::LogFacts,
        notifications::{NoticeAction, NoticeDraft},
        presentation,
    };
    for height in [16, 24] {
        let mut state = TuiState::new(60, height);
        state.input_buffer = ["draft"; 8].join("\n");
        notifications::notify_draft(
            &mut state,
            NoticeDraft::new(
                NoticeSource::Context,
                NoticeSeverity::Warning,
                "context-body",
            )
            .conversation("conv-a")
            .action(NoticeAction::ContextDecision),
        );
        notification_log::open(&mut state);
        let layout = layout(&state);
        let facts = LogFacts {
            active_conversation_id: Some("conv-a".into()),
            decisions_supported: true,
            context_warning: true,
            ..Default::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(60, height)).unwrap();
        terminal
            .draw(|frame| {
                let shell = Shell::new(frame.area(), &layout);
                presentation::compose(frame, &state, &shell, Host::Local, &facts, |_| {}, |_| {});
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rect = geometry::frame_rect(geometry::modal_region(&layout, buffer.area));
        assert!(row(buffer, rect, rect.y + 1).contains("Notifications (1)"));
        assert!(row(buffer, rect, rect.y + 2).contains("context-body"));
        let all: String = (rect.y..rect.bottom())
            .map(|y| row(buffer, rect, y))
            .collect();
        assert!(all.contains("Close"), "{height}: {all}");
        if height == 16 {
            assert!(
                row(buffer, rect, rect.bottom() - 2).contains("Close"),
                "{all}"
            );
        }
        if height == 24 {
            assert!(
                all.contains("Compact") && all.contains("Start fresh"),
                "{all}"
            );
        }
    }
}

fn paint_busy_status(frame: &mut Frame, model: &str, theme: &Theme) {
    // The existing renderer API, once: the long hint tail may not displace Ready.
    status_bar::render(
        frame,
        frame.area(),
        model,
        None,
        &StatusState::Idle,
        theme,
        0,
        &[],
        0,
        20,
        PermissionMode::Normal,
        None,
        0,
        false,
        None,
        false,
        true,
        0,
        Some("a deliberately long whole key-hint tail"),
        0,
        None,
        None,
        None,
        false,
        None,
        None,
        DensityMode::Focus,
        false,
        None,
        LogAwareness::Unseen(7),
        0,
    );
}

#[test]
fn forty_cell_model_cannot_clip_ready_or_the_log_fact() {
    let theme = Theme::dark();
    let model = "m".repeat(40);
    for width in [60, 80, 130] {
        let mut terminal = Terminal::new(TestBackend::new(width, 1)).unwrap();
        terminal
            .draw(|frame| paint_busy_status(frame, &model, &theme))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let text = row(buffer, buffer.area, 0);
        assert!(
            text.contains("Ready"),
            "{width}: state outranks model/hints: {text:?}"
        );
        assert!(
            text.contains("log: 7"),
            "{width}: log count outranks hints: {text:?}"
        );
    }
}
