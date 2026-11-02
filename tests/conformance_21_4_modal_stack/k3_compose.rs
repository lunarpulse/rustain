//! K3 A1: shared compositor order, legacy Help underneath Quit, and shell invariance.
use super::keymap_support::{press, press_all};
use ratatui::style::Modifier;
use ratatui::widgets::Paragraph;
use ratatui::{Frame, Terminal, backend::TestBackend, buffer::Buffer, layout::Rect};
use rustain::adapters::tui::{
    app::InputAction,
    keymap::{Host, Key},
    layout::{AppLayout, compute_layout},
    notification_log::{self, LogFacts},
    notifications::{self, NoticeSeverity, NoticeSource},
    presentation::{self, Shell, Surface, geometry},
    quit_confirm::{self, QuitFacts, QuitHost, QuitRequest},
    state::TuiState,
    widgets::help_overlay,
};
use rustain::domain::models::visual::DensityMode;

pub(super) fn layout(state: &TuiState) -> AppLayout {
    compute_layout(
        Rect::new(0, 0, state.terminal_width, state.terminal_height),
        &state.theme,
        &state.input_buffer,
        1,
        false,
        DensityMode::Focus,
    )
    .unwrap()
}

pub(super) fn row(buffer: &Buffer, rect: Rect, y: u16) -> String {
    (rect.x..rect.right())
        .map(|x| buffer[(x, y)].symbol())
        .collect()
}

pub(super) fn open_quit(state: &mut TuiState) {
    assert_eq!(press(state, Key::Ctrl('q')), InputAction::Quit);
    let facts = QuitFacts {
        streams: 1,
        composer_draft: !state.input_buffer.is_empty(),
        ..QuitFacts::idle(QuitHost::Local)
    };
    assert_eq!(quit_confirm::request(state, facts), QuitRequest::Confirming);
    assert!(presentation::is_open(state, Surface::QuitConfirm));
    assert!(presentation::consistent(state));
}

pub(super) fn draw(state: &TuiState, layout: &AppLayout, title: Option<&str>) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(
        state.terminal_width,
        state.terminal_height,
    ))
    .unwrap();
    terminal
        .draw(|frame| {
            let shell = Shell::new(frame.area(), layout);
            assert_eq!(
                (
                    shell.area,
                    shell.chat_pane,
                    shell.input_area,
                    shell.status_bar
                ),
                (
                    frame.area(),
                    layout.chat_pane,
                    layout.input_area,
                    layout.status_bar
                )
            );
            assert_eq!(shell.region, geometry::modal_region(layout, frame.area()));
            for y in 0..frame.area().height {
                frame.render_widget(
                    Paragraph::new("s".repeat(usize::from(frame.area().width))),
                    Rect::new(0, y, frame.area().width, 1),
                );
            }
            presentation::compose(
                frame,
                state,
                &shell,
                Host::Local,
                &LogFacts::default(),
                |frame| {
                    help_overlay::render(
                        frame,
                        shell.area,
                        &state.help_overlay,
                        &state.theme,
                        false,
                    )
                },
                |frame| {
                    // M4: observe BEFORE painting status, not just its final unoccluded row.
                    // Correct geometry alone must not let status-before-frames survive.
                    if let Some(title) = title {
                        let rect = geometry::frame_rect(shell.region);
                        let buffer = frame.buffer_mut();
                        assert_eq!(buffer[(rect.x, rect.y)].symbol(), "┌");
                        assert_eq!(buffer[(rect.right() - 1, rect.bottom() - 1)].symbol(), "┘");
                        let header = row(buffer, rect, rect.y);
                        assert_eq!(
                            header
                                .strip_prefix('┌')
                                .unwrap()
                                .trim_end_matches(['─', '┐'])
                                .trim(),
                            title,
                            "exact caller-owned frame title: {header:?}"
                        );
                    }
                    frame.render_widget(Paragraph::new("status-row-sentinel"), shell.status_bar);
                },
            );
        })
        .unwrap();
    terminal.backend().buffer().clone()
}

#[test]
fn status_row_is_painted_last_and_never_covered() {
    // Named viewport/composer witnesses, not a size sweep.
    for (width, height) in [(60, 16), (80, 24), (130, 30)] {
        for full in [false, true] {
            for route in ["log", "quit", "log-quit", "legacy-help-quit"] {
                let mut state = TuiState::new(width, height);
                if full {
                    state.input_buffer = ["draft"; 8].join("\n");
                }
                if route == "legacy-help-quit" {
                    press_all(&mut state, &[Key::Ctrl('x'), Key::Char('?')]);
                    assert!(state.help_overlay.active);
                }
                let before = layout(&state);
                assert_eq!(before.input_area.height, if full { 10 } else { 3 });
                let bare = draw(&state, &before, None);
                let title = match route {
                    "log" => {
                        notification_log::open(&mut state);
                        "System · Notifications"
                    }
                    "log-quit" => {
                        notification_log::open(&mut state);
                        open_quit(&mut state);
                        "System · Notifications › System · Quit"
                    }
                    "legacy-help-quit" => {
                        open_quit(&mut state);
                        assert!(state.help_overlay.active, "Quit must not close legacy Help");
                        "System · Quit"
                    }
                    _ => {
                        open_quit(&mut state);
                        "System · Quit"
                    }
                };
                assert!(presentation::consistent(&state));
                let after = layout(&state);
                assert_eq!(
                    (after.chat_pane, after.status_bar, after.input_area),
                    (before.chat_pane, before.status_bar, before.input_area),
                    "{route}"
                );
                let painted = draw(&state, &after, Some(title));
                for x in after.status_bar.x..after.status_bar.right() {
                    assert_eq!(
                        painted[(x, after.status_bar.y)],
                        bare[(x, before.status_bar.y)],
                        "{width}x{height}, full={full}, {route}: status cell {x}"
                    );
                }
                let region = geometry::modal_region(&after, painted.area);
                let top = geometry::frame_rect(region);
                for y in region.y..region.bottom() {
                    for x in region.x..region.right() {
                        if !top.contains((x, y).into()) {
                            assert!(
                                painted[(x, y)].modifier.contains(Modifier::DIM),
                                "{route}: undimmed region cell ({x},{y})"
                            );
                        }
                    }
                }
                for y in after.input_area.y..after.input_area.bottom() {
                    for x in after.input_area.x..after.input_area.right() {
                        assert_eq!(painted[(x, y)].symbol(), bare[(x, y)].symbol());
                        assert!(
                            painted[(x, y)].modifier.contains(Modifier::DIM),
                            "{route}: composer must dim without moving"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn toast_arrival_cannot_overpaint_the_top_quit_frame() {
    let mut state = TuiState::new(80, 24);
    open_quit(&mut state);
    let layout = layout(&state);
    let before = draw(&state, &layout, Some("System · Quit"));
    notifications::notify(
        &mut state,
        NoticeSource::System,
        NoticeSeverity::Error,
        "toast-witness",
    );
    let after = draw(&state, &layout, Some("System · Quit"));
    let rect = geometry::frame_rect(geometry::modal_region(&layout, after.area));
    for y in rect.y..rect.bottom() {
        for x in rect.x..rect.right() {
            assert_eq!(
                after[(x, y)],
                before[(x, y)],
                "toast overwrote frame at ({x},{y})"
            );
        }
    }
    press(&mut state, Key::Char('n'));
    assert!(!presentation::is_open(&state, Surface::QuitConfirm));
    let without_frame = draw(&state, &layout, None);
    assert!(
        (layout.chat_pane.y..layout.chat_pane.bottom()).any(|y| row(
            &without_frame,
            layout.chat_pane,
            y
        )
        .contains("toast-witness")),
        "positive control: an unobstructed toast really paints"
    );
}

#[test]
fn smoke_notice_prefix_survives_named_viewports() {
    for (severity, command, prefix, message) in [
        (
            NoticeSeverity::Warning,
            "/a1-second-invalid",
            "Warning Unknown command: /a1-second-invalid ",
            "Unknown command: /a1-second-invalid — not a built-in or a custom command.",
        ),
        (
            NoticeSeverity::Info,
            "/config",
            "Info /config is not available ",
            "/config is not available in an attached session. Detach (Ctrl+X d) and use it from a standalone `rustain` session.",
        ),
    ] {
        for (width, height) in [(60, 16), (80, 24), (130, 30)] {
            let mut harness = crate::e2e_harness::TestHarness::with_size(width, height);
            notifications::notify(&mut harness.state, NoticeSource::System, severity, message);
            harness.render();
            let buffer = harness.terminal.backend().buffer();
            let screen = (0..height)
                .map(|y| row(buffer, buffer.area, y))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(screen.contains(prefix), "{width}x{height}: {screen}");
            assert!(screen.contains('…'), "{width}x{height}: {screen}");
            println!(
                "TOAST_DESKCHECK_FRAME={}",
                serde_json::json!([width, height, command, screen])
            );
        }
    }
}

#[test]
fn tiny_terminal_painter_keeps_the_shared_minimum_size_message() {
    for (width, height) in [(59, 16), (60, 15)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame: &mut Frame| {
                presentation::paint_too_small(frame, frame.area());
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let text = (0..height)
            .map(|y| row(buffer, buffer.area, y))
            .collect::<String>();
        assert_eq!(text.trim(), "Terminal too small (min 60x16)");
    }
}

#[test]
fn wide_sidebar_cells_below_the_frame_are_dimmed_without_reflow() {
    let mut state = TuiState::new(130, 30);
    let area = Rect::new(0, 0, 130, 30);
    let before = compute_layout(area, &state.theme, "", 2, true, DensityMode::Monitor).unwrap();
    open_quit(&mut state);
    let after = compute_layout(area, &state.theme, "", 2, true, DensityMode::Monitor).unwrap();
    assert_eq!(
        (after.sidebar, after.chat_pane, after.input_area),
        (before.sidebar, before.chat_pane, before.input_area)
    );
    let buffer = draw(&state, &after, Some("System · Quit"));
    let top = geometry::frame_rect(geometry::modal_region(&after, area));
    let sidebar = after.sidebar.expect("named sidebar witness");
    for y in sidebar.y..sidebar.bottom() {
        for x in sidebar.x..sidebar.right() {
            if !top.contains((x, y).into()) {
                assert_eq!(buffer[(x, y)].symbol(), "s", "sidebar moved at ({x},{y})");
                assert!(
                    buffer[(x, y)].modifier.contains(Modifier::DIM),
                    "sidebar escaped the frame dim at ({x},{y})"
                );
            }
        }
    }
}

#[test]
fn quit_hides_legacy_search_cursors_and_cancel_restores_them() {
    use rustain::adapters::tui::widgets::{cross_search, reverse_search, search_bar};
    use rustain::domain::models::{FocusState, visual::PanelType};
    for surface in ["search", "reverse", "cross"] {
        let mut state = TuiState::new(130, 30);
        match surface {
            "search" => {
                press(&mut state, Key::Ctrl('f'));
            }
            "reverse" => {
                press(&mut state, Key::Ctrl('r'));
            }
            _ => {
                state.sidebar_visible = true;
                state.density_mode = DensityMode::Monitor;
                state.sidebar_panel = Some(PanelType::History);
                state.focus = FocusState::Sidebar {
                    panel: PanelType::History,
                    selected: 0,
                };
                press(&mut state, Key::Char('/'));
            }
        }
        let focus = state.focus.clone();
        let paint = |state: &TuiState| {
            let mut terminal = Terminal::new(TestBackend::new(130, 30)).unwrap();
            let show_cursor = state.modal_stack.is_empty();
            terminal
                .draw(|frame| {
                    let layout = compute_layout(
                        frame.area(),
                        &state.theme,
                        &state.input_buffer,
                        1,
                        state.sidebar_visible,
                        state.density_mode,
                    )
                    .unwrap();
                    let shell = Shell::new(frame.area(), &layout);
                    presentation::compose(
                        frame,
                        state,
                        &shell,
                        Host::Local,
                        &LogFacts::default(),
                        |frame| match surface {
                            "search" => search_bar::render(
                                frame,
                                Rect::new(
                                    shell.chat_pane.x,
                                    shell.chat_pane.y,
                                    shell.chat_pane.width,
                                    1,
                                ),
                                &state.search_state,
                                &state.theme,
                                show_cursor,
                            ),
                            "reverse" => reverse_search::render(
                                frame,
                                shell.input_area,
                                &state.reverse_search,
                                &state.theme,
                                show_cursor,
                            ),
                            _ => cross_search::render(
                                frame,
                                layout.sidebar.unwrap(),
                                &state.cross_search,
                                &state.theme,
                                show_cursor,
                            ),
                        },
                        |_| {},
                    );
                })
                .unwrap();
            terminal.backend().cursor_visible()
        };
        assert!(
            paint(&state),
            "{surface}: the real legacy cursor is the positive control"
        );
        open_quit(&mut state);
        assert!(
            !paint(&state),
            "{surface}: no hidden text field owns the terminal cursor"
        );
        press(&mut state, Key::Char('n'));
        assert_eq!(state.focus, focus);
        assert!(paint(&state), "{surface}: cancel returns the legacy cursor");
    }
}
