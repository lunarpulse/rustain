//! K3 A1: shared compositor order, covered frames underneath Quit, and shell invariance.
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

fn top_rect(_state: &TuiState, layout: &AppLayout, buffer: &Buffer) -> Rect {
    // old -> new: a decision was a bottom-anchored box over the chat pane, found by its
    // own `┌`; it is now an ordinary L3 frame in the shared Standard `frame_rect`.
    geometry::frame_rect(geometry::modal_region(layout, buffer.area))
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
                &mut presentation::occlusion::OpaqueRects::new(shell.chat_pane),
                |frame| {
                    // M4: observe BEFORE painting status, not just its final unoccluded row.
                    // Correct geometry alone must not let status-before-frames survive.
                    if let Some(title) = title {
                        let buffer = frame.buffer_mut();
                        let rect = top_rect(state, layout, buffer);
                        assert_eq!(buffer[(rect.x, rect.y)].symbol(), "┌");
                        assert_eq!(buffer[(rect.right() - 1, rect.bottom() - 1)].symbol(), "┘");
                        let header = row(buffer, rect, rect.y);
                        assert_eq!(
                            header
                                .strip_prefix('┌')
                                .unwrap()
                                .trim_end_matches(['─', '┐'])
                                .trim(),
                            rustain::adapters::tui::text_geometry::clip(
                                title,
                                usize::from(rect.width.saturating_sub(3))
                            )
                            .trim_end(),
                            "caller-owned breadcrumb/title prefix: {header:?}"
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
            for route in [
                "log",
                "quit",
                "log-quit",
                "help",
                "palette",
                "help-quit",
                "palette-help-log",
                "palette-help-log-quit",
                "pane",
                "help-pane",
            ] {
                let mut state = TuiState::new(width, height);
                if full {
                    state.input_buffer = ["draft"; 8].join("\n");
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
                    "help" => {
                        press_all(&mut state, &[Key::Ctrl('x'), Key::Char('?')]);
                        "Help — Rustain Keybindings"
                    }
                    "palette" => {
                        press(&mut state, Key::Ctrl('p'));
                        "Command Palette"
                    }
                    "help-quit" => {
                        press_all(&mut state, &[Key::Ctrl('x'), Key::Char('?')]);
                        open_quit(&mut state);
                        assert!(presentation::is_open(&state, Surface::Help));
                        "Help — Rustain Keybindings › System · Quit"
                    }
                    "palette-help-log" | "palette-help-log-quit" => {
                        press_all(
                            &mut state,
                            &[
                                Key::Ctrl('p'),
                                Key::Ctrl('x'),
                                Key::Char('?'),
                                Key::Ctrl('x'),
                                Key::Char('n'),
                            ],
                        );
                        if route.ends_with("-quit") {
                            open_quit(&mut state);
                            "Command Palette › Help — Rustain Keybindings › System · Notifications › System · Quit"
                        } else {
                            "Command Palette › Help — Rustain Keybindings › System · Notifications"
                        }
                    }
                    "pane" | "help-pane" => {
                        if route == "help-pane" {
                            press_all(&mut state, &[Key::Ctrl('x'), Key::Char('?')]);
                        }
                        rustain::adapters::tui::decision_queue::raise(
                            &mut state,
                            rustain::adapters::tui::decision_queue::DecisionDraft::new(
                                rustain::adapters::tui::decision_queue::DecisionKind::Budget,
                                "budget detail",
                            ),
                            rustain::adapters::tui::decision_queue::Arrival::Requested,
                        );
                        // old -> new: `System · Daily budget` -> the decision frame title.
                        if route == "pane" {
                            "Decision: Daily budget"
                        } else {
                            "Help — Rustain Keybindings › Decision: Daily budget"
                        }
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
                let top = top_rect(&state, &after, &painted);
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
fn quit_hides_search_cursors_and_restores_frame_and_popup_owners() {
    use rustain::adapters::tui::widgets::input_box;
    use rustain::domain::models::{FocusState, visual::PanelType};
    for surface in ["search", "reverse", "cross"] {
        let mut state = TuiState::new(130, 30);
        match surface {
            "search" => {
                press(&mut state, Key::Ctrl('f'));
                press_all(
                    &mut state,
                    &[Key::Char('f'), Key::Char('o'), Key::Char('x')],
                );
            }
            "reverse" => {
                state.input_buffer = "kept draft".into();
                state.cursor_position = 4;
                state.input_history.push("界 first".into());
                state.input_history.push("界 second".into());
                press_all(&mut state, &[Key::Ctrl('r'), Key::Char('界'), Key::Down]);
                assert_eq!(state.focus, FocusState::Input);
                assert!(state.modal_stack.is_empty());
                assert_eq!(state.reverse_search.selected_match, Some(1));
            }
            _ => {
                state.sidebar_visible = true;
                state.density_mode = DensityMode::Monitor;
                state.sidebar_panel = Some(PanelType::History);
                state.focus = FocusState::Sidebar {
                    panel: PanelType::History,
                    selected: 0,
                };
                let history_focus = state.focus.clone();
                assert_eq!(
                    press(&mut state, Key::Char('/')),
                    InputAction::OpenCrossSearch
                );
                assert_eq!(state.focus, history_focus);
                let owner = state
                    .modal_stack
                    .top()
                    .expect("History slash opens a frame");
                assert_eq!(owner.surface, Surface::CrossSearch);
                assert!(owner.conversation_id.is_none(), "Cross-Search is global");
                press_all(
                    &mut state,
                    &[Key::Char('f'), Key::Char('o'), Key::Char('x')],
                );
            }
        }
        let focus = state.focus.clone();
        let frames: Vec<_> = state.modal_stack.iter().cloned().collect();
        assert!(presentation::consistent(&state));
        let paint = |state: &TuiState| {
            let mut terminal = Terminal::new(TestBackend::new(130, 30)).unwrap();
            let show_cursor = state.modal_stack.is_empty() && !state.reverse_search.is_open();
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
                    if surface == "reverse" {
                        input_box::render(
                            frame,
                            shell.input_area,
                            &state.input_buffer,
                            state.cursor_position,
                            if show_cursor {
                                state.focus.clone()
                            } else {
                                FocusState::Chat
                            },
                            &state.theme,
                            state.multiline_mode,
                            state.input_scroll_offset,
                            None,
                        );
                    }
                    presentation::compose(
                        frame,
                        state,
                        &shell,
                        Host::Local,
                        &LogFacts::default(),
                        &mut presentation::occlusion::OpaqueRects::new(shell.chat_pane),
                        |_| {},
                    );
                })
                .unwrap();
            let cursor = terminal.get_cursor_position().unwrap();
            (
                terminal.backend().buffer().clone(),
                terminal.backend().cursor_visible(),
                cursor,
            )
        };
        let before = paint(&state);
        assert!(
            !before.1,
            "{surface}: the query uses its in-widget caret, never hardware"
        );
        if surface == "search" {
            let stage = layout(&state).chat_pane;
            assert!(
                row(&before.0, stage, stage.y).starts_with("Search: fox"),
                "the real Search frame must paint despite its hidden cursor"
            );
        }
        if surface == "cross" {
            let shell_layout = compute_layout(
                before.0.area,
                &state.theme,
                &state.input_buffer,
                1,
                state.sidebar_visible,
                state.density_mode,
            )
            .unwrap();
            let rect = geometry::frame_rect(geometry::modal_region(&shell_layout, before.0.area));
            assert_eq!(before.0[(rect.x, rect.y)].symbol(), "┌");
            assert!(row(&before.0, rect, rect.y).contains("Cross-Search"));
            assert!(
                row(&before.0, rect, rect.y + 1).contains("/ fox"),
                "the real Cross-Search frame paints the edited query"
            );
        }
        let reverse_payload = (
            state.reverse_search.query.clone(),
            state.reverse_search.matches.clone(),
            state.reverse_search.selected_match,
        );
        if surface == "reverse" {
            let chat = layout(&state).chat_pane;
            let title = |buffer: &Buffer| {
                (chat.y..chat.bottom()).any(|y| row(buffer, chat, y).contains("Reverse Search"))
            };
            assert!(title(&before.0), "compose owns the reverse popup paint");
            assert!(row(&before.0, chat, chat.bottom() - 5).contains("Reverse Search"));
            let caret_x = chat.x
                + 1
                + rustain::adapters::tui::text_geometry::cells("(reverse-search): 界") as u16;
            assert!(
                before.0[(caret_x, chat.bottom() - 4)]
                    .modifier
                    .contains(Modifier::REVERSED),
                "the popup owns its actual styled caret cell instead of the composer cursor"
            );
            state.focus = FocusState::Chat;
            let other_focus = paint(&state);
            assert!(
                !title(&other_focus.0),
                "non-Input base suppresses popup cells"
            );
            assert!(!other_focus.1, "non-Input base suppresses popup cursor");
            state.focus = focus.clone();
            state.reverse_search.dismiss();
            let closed = paint(&state);
            assert!(!title(&closed.0), "closed payload must not paint");
            assert!(closed.1, "closed popup returns the composer cursor");
            assert_ne!(closed.2, before.2);
            state.reverse_search.selected_match = Some(0);
            state.reverse_search.matches.clear();
            let empty = paint(&state);
            assert!(title(&empty.0), "Some(0) keeps empty results open");
            assert!(
                !empty.1,
                "empty results retain the in-widget caret, not hardware"
            );
            assert!(
                empty.0[(caret_x, chat.bottom() - 3)]
                    .modifier
                    .contains(Modifier::REVERSED),
                "empty-results popup has four fitted rows and its own caret"
            );
            state.reverse_search.matches = reverse_payload.1.clone();
            state.reverse_search.selected_match = reverse_payload.2;
        }
        open_quit(&mut state);
        assert_eq!(
            state.modal_stack.top().unwrap().surface,
            Surface::QuitConfirm
        );
        assert!(
            state
                .modal_stack
                .iter()
                .take(frames.len())
                .eq(frames.iter()),
            "{surface}: Quit covers rather than replaces the original frame owners"
        );
        assert!(presentation::consistent(&state));
        let covered = paint(&state);
        assert!(
            !covered.1,
            "{surface}: no hidden text field owns the terminal cursor"
        );
        if surface == "reverse" {
            let chat = layout(&state).chat_pane;
            assert!(
                !(chat.y..chat.bottom())
                    .any(|y| row(&covered.0, chat, y).contains("Reverse Search")),
                "a frame suppresses the routed popup cells, not just its cursor"
            );
        }
        press(&mut state, Key::Char('n'));
        assert_eq!(state.focus, focus);
        assert_eq!(
            state.modal_stack.iter().cloned().collect::<Vec<_>>(),
            frames
        );
        assert!(presentation::consistent(&state));
        let restored = paint(&state);
        assert!(
            !restored.1,
            "{surface}: cancel restores the in-widget caret policy"
        );
        assert_eq!(restored.2, before.2, "{surface}: exact cursor is restored");
        if surface == "search" {
            let stage = layout(&state).chat_pane;
            assert_eq!(
                row(&restored.0, stage, stage.y),
                row(&before.0, stage, stage.y),
                "cancel restores exact Search query and counter cells"
            );
        }
        if surface == "cross" {
            assert_eq!(state.cross_search.query.as_str(), "fox");
            assert_eq!(
                restored.0, before.0,
                "cancel restores exact Cross-Search frame cells"
            );
        }
        assert_eq!(
            (
                state.reverse_search.query,
                state.reverse_search.matches,
                state.reverse_search.selected_match,
            ),
            reverse_payload,
            "{surface}: Quit preserves the exact reverse payload"
        );
        if surface == "reverse" {
            assert_eq!(restored.0, before.0, "cancel restores exact popup cells");
            assert_eq!(
                (state.input_buffer.as_str(), state.cursor_position),
                ("kept draft", 4)
            );
        }
    }
}

#[test]
fn a2a_smoke_help_predicates_match_fitted_frames() {
    for (width, height) in [(60, 16), (80, 24), (130, 30)] {
        let mut harness = crate::e2e_harness::TestHarness::with_size(width, height);
        harness.state.input_buffer = "A1-kept-draft".into();
        harness.state.cursor_position = harness.state.input_buffer.len();
        for (stage, keys) in [
            ("help-initial", vec![Key::Ctrl('x'), Key::Char('?')]),
            ("help-scrolled", vec![Key::Char('j'); 3]),
            ("help-log", vec![Key::Ctrl('x'), Key::Char('n')]),
            ("help-restored-log", vec![Key::Esc]),
            ("help-quit", vec![]),
            ("help-restored-quit", vec![Key::Char('n')]),
            ("composer", vec![Key::Esc]),
        ] {
            if stage == "help-quit" {
                open_quit(&mut harness.state);
            }
            for key in keys {
                press(&mut harness.state, key);
            }
            harness.render();
            let buffer = harness.terminal.backend().buffer();
            let screen = (0..height)
                .map(|y| row(buffer, buffer.area, y))
                .collect::<Vec<_>>()
                .join("\n");
            assert_eq!(screen.lines().count(), usize::from(height));
            assert!(screen.contains("A1-kept-draft"));
            if stage.starts_with("help") {
                assert!(screen.contains("Help — Rustain Keybindings"));
            }
            println!(
                "HELP_DESKCHECK_FRAME={}",
                serde_json::json!([width, height, stage, screen])
            );
        }
    }
}

#[test]
fn nested_help_keeps_its_closing_border_scroll_rail() {
    for (width, height) in [(60, 16), (80, 24), (130, 30)] {
        for nested in [false, true] {
            let mut state = TuiState::new(width, height);
            if nested {
                press(&mut state, Key::Ctrl('p'));
            }
            press(&mut state, Key::Ctrl('x'));
            press(&mut state, Key::Char('?'));
            let layout = layout(&state);
            let rect = geometry::frame_rect(geometry::modal_region(
                &layout,
                Rect::new(0, 0, width, height),
            ));
            let buffer = draw(&state, &layout, None);
            assert!(
                row(&buffer, rect, rect.bottom() - 1).contains("j/k scroll  ? close"),
                "{width}x{height} nested={nested}: the breadcrumb cannot erase the Help rail"
            );
        }
    }
}

#[test]
fn pending_menu_yields_to_top_quit_and_returns_exactly_on_cancel() {
    for (width, height) in [(60, 16), (80, 24), (130, 30)] {
        let mut harness = crate::e2e_harness::TestHarness::with_size(width, height);
        harness.state.input_buffer = "menu caller draft".into();
        harness.state.cursor_position = 4;
        for key in [Key::Ctrl('x'), Key::Char('?'), Key::Ctrl('x')] {
            press(&mut harness.state, key);
        }
        let origin = harness.state.pending_leader;
        let frames: Vec<_> = harness.state.modal_stack.iter().cloned().collect();
        harness.render();
        let menu = harness.terminal.backend().buffer().clone();
        assert!((0..height).any(|y| row(&menu, menu.area, y).contains("┌ Ctrl+X ")));
        open_quit(&mut harness.state);
        harness.render();
        let quit = harness.terminal.backend().buffer();
        let screen = (0..height)
            .map(|y| row(quit, quit.area, y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            screen.contains("[n/Esc/Enter] Stay"),
            "{width}x{height}: Quit rail must stay readable"
        );
        assert!(
            !screen.contains("┌ Ctrl+X "),
            "{width}x{height}: retained menu yields to Quit"
        );
        assert_eq!(harness.state.pending_leader, origin);
        press(&mut harness.state, Key::Char('n'));
        harness.render();
        assert_eq!(
            harness.terminal.backend().buffer(),
            &menu,
            "{width}x{height}: exact menu returns"
        );
        assert_eq!(
            harness
                .state
                .modal_stack
                .iter()
                .cloned()
                .collect::<Vec<_>>(),
            frames
        );
        assert_eq!(harness.state.pending_leader, origin);
    }
}

#[test]
fn frame_origin_leader_feedback_is_visible_and_retains_the_menu() {
    use rustain::adapters::tui::keymap::{
        Context, resolve::no_binding_text, table::CLOSE_WINDOW_FIRST,
    };
    for (width, height) in [(60, 16), (80, 24), (130, 30)] {
        for key in [Key::Char('t'), Key::Char('~')] {
            let mut state = TuiState::new(width, height);
            state.input_buffer = "kept draft".into();
            state.cursor_position = 4;
            press_all(
                &mut state,
                &[Key::Ctrl('x'), Key::Char('?'), Key::Ctrl('x')],
            );
            let frames: Vec<_> = state.modal_stack.iter().cloned().collect();
            let focus = state.focus.clone();
            let layout = layout(&state);
            let count = state.notifications.records().len();
            let expected = if key == Key::Char('t') {
                CLOSE_WINDOW_FIRST.to_owned()
            } else {
                no_binding_text(key, Context::Help.name())
            };
            assert_eq!(press(&mut state, key), InputAction::Consumed);
            assert_eq!(state.pending_leader, Some(Context::Help));
            assert_eq!(state.notifications.records().len(), count + 1);
            assert_eq!(
                state.notifications.records().next_back().unwrap().message,
                expected
            );
            let menu = draw(&state, &layout, None);
            let region = geometry::modal_region(&layout, menu.area);
            let top = (region.y..region.bottom())
                .find(|&y| row(&menu, region, y).contains("┌ Ctrl+X "))
                .expect("menu retained");
            assert!(
                (top..region.bottom()).any(|y| row(&menu, region, y).contains(&expected)),
                "{width}x{height} {key:?}: feedback must paint IN WhichKey, not under Help"
            );
            if key == Key::Char('t') {
                let (x, y) = (top..region.bottom())
                    .find_map(|y| {
                        let text = row(&menu, region, y);
                        text.find("Tasks (n/a)")
                            .map(|x| (text[..x].chars().count() as u16, y))
                    })
                    .expect("unavailable Tasks row remains visible");
                let cell = &menu[(region.x + x, y)];
                assert!(
                    cell.modifier
                        .intersects(Modifier::BOLD | Modifier::REVERSED)
                        || cell.fg != state.theme.colors.fg_muted,
                    "pressed unavailable row is emphasized"
                );
            }
            open_quit(&mut state);
            assert_eq!(state.pending_leader, Some(Context::Help));
            let quit = draw(&state, &layout, None);
            assert!((0..height).any(|y| row(&quit, quit.area, y).contains("[n/Esc/Enter] Stay")));
            assert!(!(0..height).any(|y| row(&quit, quit.area, y).contains("┌ Ctrl+X ")));
            assert_eq!(press(&mut state, Key::Char('n')), InputAction::Consumed);
            assert_eq!(
                draw(&state, &layout, None),
                menu,
                "exact menu and feedback return"
            );
            assert_eq!(state.pending_leader, Some(Context::Help));
            assert_eq!(
                state.modal_stack.iter().cloned().collect::<Vec<_>>(),
                frames
            );
            assert_eq!(
                (state.focus, state.input_buffer, state.cursor_position),
                (focus, "kept draft".into(), 4)
            );
        }
    }
}

#[test]
fn nested_palette_refusal_is_visible_without_dispatch() {
    use rustain::adapters::tui::keymap::table::CLOSE_WINDOW_FIRST;
    use rustain::domain::models::{
        palette::{PaletteAction, PaletteEntry, PaletteScope},
        visual::PanelType,
    };
    for (width, height) in [(60, 16), (80, 24), (130, 30)] {
        let mut state = TuiState::new(width, height);
        state.input_buffer = "palette caller draft".into();
        state.cursor_position = 5;
        press_all(
            &mut state,
            &[
                Key::Ctrl('x'),
                Key::Char('?'),
                Key::Ctrl('p'),
                Key::Char('m'),
            ],
        );
        state.command_palette.filtered_entries = ["model first", "model selected"]
            .map(|name| PaletteEntry {
                name: name.into(),
                description: String::new(),
                shortcut: None,
                scope: PaletteScope::All,
                action: PaletteAction::OpenPanel(PanelType::Tasks),
            })
            .to_vec();
        assert_eq!(press(&mut state, Key::Down), InputAction::Consumed);
        assert_eq!(state.command_palette.selected_index, 1);
        let frames: Vec<_> = state.modal_stack.iter().cloned().collect();
        let focus = state.focus.clone();
        let count = state.notifications.records().len();
        assert_eq!(press(&mut state, Key::Enter), InputAction::Consumed);
        assert_eq!(state.notifications.records().len(), count + 1);
        assert_eq!(
            state.notifications.records().next_back().unwrap().message,
            CLOSE_WINDOW_FIRST
        );
        assert_eq!(
            (
                state.command_palette.filter_text.as_str(),
                state.command_palette.selected_index
            ),
            ("m", 1)
        );
        assert_eq!(
            state.modal_stack.iter().cloned().collect::<Vec<_>>(),
            frames
        );
        let layout = layout(&state);
        let rect = geometry::frame_rect(geometry::modal_region(
            &layout,
            Rect::new(0, 0, width, height),
        ));
        let painted = draw(&state, &layout, None);
        assert!(
            (rect.y + 4..rect.bottom())
                .any(|y| row(&painted, rect, y).contains(CLOSE_WINDOW_FIRST)),
            "{width}x{height}: refusal must paint in the palette footer, below query and results"
        );
        let query_y = (rect.y..rect.bottom())
            .find(|&y| row(&painted, rect, y).contains("> m"))
            .expect("the exact edited palette query remains visible");
        assert!(
            painted[(rect.x + 4, query_y)]
                .modifier
                .contains(Modifier::REVERSED),
            "the visible query retains its EOL caret as well as its refusal"
        );
        assert_eq!(press(&mut state, Key::Down), InputAction::Consumed);
        let cleared = draw(&state, &layout, None);
        assert!(
            !(rect.y..rect.bottom()).any(|y| row(&cleared, rect, y).contains(CLOSE_WINDOW_FIRST))
        );
        assert_eq!(state.command_palette.filter_text.as_str(), "m");
        assert_eq!(state.command_palette.selected_index, 0);
        assert_eq!(state.command_palette.filtered_entries.len(), 2);
        assert_eq!(
            state.modal_stack.iter().cloned().collect::<Vec<_>>(),
            frames
        );
        assert_eq!(
            (state.focus, state.input_buffer, state.cursor_position),
            (focus, "palette caller draft".into(), 5)
        );
        assert_eq!(
            state.notifications.records().len(),
            count + 1,
            "next key only clears footer feedback"
        );
    }
}

#[test]
fn a2b_smoke_predicates_have_real_rust_frames() {
    use rustain::adapters::tui::{
        decision_queue::{self, Arrival, DecisionDraft, DecisionKind},
        keymap::{CommandId, Context, table::LeaderCmd},
        notifications::NotificationStore,
        state::LeaderFeedback,
    };
    use rustain::domain::{
        clock::MockClock,
        models::{
            NotificationsConfig,
            autocomplete::{AutocompleteKind, AutocompleteSuggestion},
        },
    };
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };

    fn emit(h: &mut crate::e2e_harness::TestHarness, host: Host, stage: &str) -> String {
        h.render_with_host(host);
        let buffer = h.terminal.backend().buffer();
        let screen = (0..h.state.terminal_height)
            .map(|y| row(buffer, buffer.area, y))
            .collect::<Vec<_>>()
            .join("\n");
        println!(
            "A2B_DESKCHECK_FRAME={}",
            serde_json::json!([
                h.state.terminal_width,
                h.state.terminal_height,
                stage,
                screen,
            ])
        );
        screen
    }
    for (width, height) in [(60, 16), (80, 24), (130, 30)] {
        let clock = Arc::new(MockClock::new(Instant::now()));
        let mut h = crate::e2e_harness::TestHarness::with_size(width, height);
        h.state.notifications =
            NotificationStore::from_config(&NotificationsConfig::default(), clock.clone());
        press_all(
            &mut h.state,
            &[
                Key::Ctrl('x'),
                Key::Char('?'),
                Key::Char('j'),
                Key::Char('j'),
                Key::Char('j'),
            ],
        );
        clock.advance(Duration::from_secs(2));
        let frame = h.state.modal_stack.top().unwrap().clone();
        let scroll = h.state.help_overlay.scroll_offset;
        emit(&mut h, Host::Local, "help-before-arrival");
        assert_eq!(
            decision_queue::raise(
                &mut h.state,
                DecisionDraft::new(DecisionKind::Budget, "Daily budget ($0.00) reached."),
                Arrival::Automatic,
            ),
            decision_queue::Raised::Focused
        );
        emit(&mut h, Host::Local, "help-budget");
        decision_queue::note_painted(&mut h.state);
        clock.advance(decision_queue::INERT_AFTER_PAINT);
        press(&mut h.state, Key::Esc);
        assert_eq!(h.state.modal_stack.top(), Some(&frame));
        assert_eq!(h.state.help_overlay.scroll_offset, scroll);
        emit(&mut h, Host::Local, "help-deferred");
        press_all(&mut h.state, &[Key::Ctrl('x'), Key::Char('q')]);
        decision_queue::open_waiting(&mut h.state, &LogFacts::default());
        emit(&mut h, Host::Local, "help-reopened-budget");
        decision_queue::note_painted(&mut h.state);
        clock.advance(decision_queue::INERT_AFTER_PAINT);
        assert_eq!(
            press(&mut h.state, Key::Char('y')),
            InputAction::Decision(decision_queue::DecisionEffect::BudgetContinue,)
        );
        assert_eq!(h.state.modal_stack.top(), Some(&frame));
        assert_eq!(h.state.help_overlay.scroll_offset, scroll);
        emit(&mut h, Host::Local, "help-resolved");

        // These are real shared Rust paints with attached availability; socket
        // ownership/rollback is separately covered by the attached front door.
        let mut attached = crate::e2e_harness::TestHarness::with_size(width, height);
        press_all(
            &mut attached.state,
            &[
                Key::Char('/'),
                Key::Char('d'),
                Key::Char('e'),
                Key::Char('t'),
            ],
        );
        assert_eq!(
            attached.state.autocomplete.kind,
            Some(AutocompleteKind::SlashCommand)
        );
        attached.state.autocomplete.suggestions = vec![AutocompleteSuggestion::SlashCommand {
            name: "detach".into(),
            description: "Detach this client".into(),
        }];
        emit(&mut attached, Host::Attached, "attached-popup");
        press(&mut attached.state, Key::Esc);
        assert!(attached.state.input_buffer.is_empty());
        emit(&mut attached, Host::Attached, "attached-popup-rollback");
        press_all(
            &mut attached.state,
            &[
                Key::Ctrl('x'),
                Key::Char('?'),
                Key::Char('j'),
                Key::Char('j'),
                Key::Char('j'),
                Key::Ctrl('x'),
            ],
        );
        attached.state.leader_feedback = Some(LeaderFeedback {
            command: Some(CommandId::Leader(LeaderCmd::Decisions)),
            message: "No decisions waiting in this client".into(),
        });
        assert_eq!(attached.state.pending_leader, Some(Context::Help));
        emit(&mut attached, Host::Attached, "attached-help-no-decisions");
    }
}

#[test]
fn a_covered_pane_does_not_start_its_inert_window() {
    use rustain::adapters::tui::{
        decision_queue::{self, Arrival, DecisionDraft, DecisionKind},
        notifications::NotificationStore,
    };
    use rustain::domain::{clock::MockClock, models::NotificationsConfig};
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };
    let clock = Arc::new(MockClock::new(Instant::now()));
    let mut state = TuiState::new(80, 24);
    state.notifications =
        NotificationStore::from_config(&NotificationsConfig::default(), clock.clone());
    decision_queue::raise(
        &mut state,
        DecisionDraft::new(DecisionKind::Budget, "covered"),
        Arrival::Requested,
    );
    press_all(&mut state, &[Key::Ctrl('x'), Key::Char('?')]);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let mut paint = |state: &TuiState| {
        let layout = layout(state);
        let mut painted = false;
        terminal
            .draw(|frame| {
                painted = presentation::compose(
                    frame,
                    state,
                    &Shell::new(frame.area(), &layout),
                    Host::Local,
                    &LogFacts::default(),
                    &mut presentation::occlusion::OpaqueRects::new(layout.chat_pane),
                    |_| {},
                );
            })
            .unwrap();
        painted
    };
    assert!(!paint(&state));
    clock.advance(Duration::from_secs(2));
    press(&mut state, Key::Esc);
    assert_eq!(press(&mut state, Key::Char('y')), InputAction::Consumed);
    assert!(state.decisions.holds(DecisionKind::Budget));
    assert!(paint(&state));
    decision_queue::note_painted(&mut state);
    clock.advance(decision_queue::INERT_AFTER_PAINT);
    assert_eq!(
        press(&mut state, Key::Char('y')),
        InputAction::Decision(decision_queue::DecisionEffect::BudgetContinue)
    );
}

#[test]
fn autocomplete_is_painted_only_without_a_frame_and_returns_after_quit() {
    use rustain::domain::models::autocomplete::AutocompleteSuggestion;
    for (width, height) in [(60, 16), (80, 24), (130, 30)] {
        let mut state = TuiState::new(width, height);
        press(&mut state, Key::Char('/'));
        state.autocomplete.suggestions = vec![AutocompleteSuggestion::SlashCommand {
            name: "detach".into(),
            description: "routed-popup-canary".into(),
        }];
        let before = layout(&state);
        let popup = draw(&state, &before, None);
        let text = |buffer: &Buffer| {
            buffer
                .content()
                .iter()
                .map(|c| c.symbol())
                .collect::<String>()
        };
        assert!(text(&popup).contains("routed-popup-canary"));
        assert!(state.modal_stack.is_empty());
        open_quit(&mut state);
        let covered = draw(&state, &before, Some("System · Quit"));
        assert!(!text(&covered).contains("routed-popup-canary"));
        press(&mut state, Key::Char('n'));
        assert!(state.modal_stack.is_empty() && state.autocomplete.is_open());
        assert_eq!(draw(&state, &before, None), popup);
        press(&mut state, Key::Esc);
        assert!(state.input_buffer.is_empty() && !state.autocomplete.is_open());
    }
}

#[test]
fn pane_model_effect_paints_its_destination_and_restores_the_caller() {
    use rustain::adapters::tui::{
        decision_queue::{self, Arrival, DecisionDraft, DecisionKind},
        notifications::NotificationStore,
    };
    use rustain::domain::{clock::MockClock, models::NotificationsConfig};
    use std::{sync::Arc, time::Instant};
    for (width, height) in [(60, 16), (80, 24), (130, 30)] {
        let clock = Arc::new(MockClock::new(Instant::now()));
        let mut state = TuiState::new(width, height);
        state.notifications =
            NotificationStore::from_config(&NotificationsConfig::default(), clock.clone());
        press_all(&mut state, &[Key::Ctrl('x'), Key::Char('?')]);
        let caller = state.modal_stack.iter().cloned().collect::<Vec<_>>();
        decision_queue::raise(
            &mut state,
            DecisionDraft::new(DecisionKind::Budget, "budget detail"),
            Arrival::Requested,
        );
        let layout = layout(&state);
        draw(&state, &layout, None);
        decision_queue::note_painted(&mut state);
        clock.advance(decision_queue::INERT_AFTER_PAINT);
        assert_eq!(
            press(&mut state, Key::Char('s')),
            InputAction::Decision(
                rustain::adapters::tui::decision_queue::DecisionEffect::BudgetSwitchCheaper
            )
        );
        rustain::adapters::tui::handlers::model_switch::open_selector(
            &mut state,
            &super::k7_surfaces::Info,
            &rustain::domain::models::AppConfig::default(),
            None,
        );
        let painted = draw(&state, &layout, None);
        let rect = top_rect(&state, &layout, &painted);
        assert!((rect.y..rect.bottom()).any(|y| row(&painted, rect, y).contains("Select Model")));
        assert!(
            (rect.y + 1..rect.bottom() - 1).any(|y| row(&painted, rect, y).contains("Model one"))
        );
        assert!(
            !(rect.y..rect.bottom())
                .any(|y| row(&painted, rect, y).contains("Please close the window first"))
        );
        assert!(!state.decisions.holds(DecisionKind::Budget));
        assert_eq!(press(&mut state, Key::Esc), InputAction::Consumed);
        assert_eq!(
            state.modal_stack.iter().cloned().collect::<Vec<_>>(),
            caller
        );
    }
}

#[test]
fn frame_origin_decisions_refusal_is_visible_and_retains_the_menu() {
    use rustain::adapters::tui::{
        decision_queue::{self, Arrival, DecisionDraft, DecisionKind},
        keymap::Context,
    };
    for (width, height) in [(60, 16), (80, 24), (130, 30)] {
        for waiting in ["empty", "other", "actionable"] {
            let mut state = TuiState::new(width, height);
            state.input_buffer = "kept draft".into();
            state.cursor_position = 4;
            if waiting != "empty" {
                let mut draft = DecisionDraft::new(DecisionKind::Budget, "waiting");
                if waiting == "other" {
                    draft = draft.conversation("other");
                    // The last draw saw another tab: the arrival queues, never opens (A5).
                    decision_queue::prune_closed(
                        &mut state,
                        &LogFacts {
                            active_conversation_id: Some("here".into()),
                            ..LogFacts::default()
                        },
                    );
                }
                decision_queue::raise(&mut state, draft, Arrival::Automatic);
            }
            press_all(&mut state, &[Key::Ctrl('x'), Key::Char('?')]);
            let frames: Vec<_> = state.modal_stack.iter().cloned().collect();
            press(&mut state, Key::Ctrl('x'));
            assert_eq!(
                press(&mut state, Key::Char('q')),
                InputAction::FocusDecisions
            );
            decision_queue::open_waiting(&mut state, &LogFacts::default());
            if waiting == "actionable" {
                assert!(state.pending_leader.is_none());
                assert_eq!(state.modal_stack.top().unwrap().surface, Surface::Decision);
                assert_eq!(state.modal_stack.iter().next(), frames.first());
            } else {
                let expected = if waiting == "empty" {
                    "No decisions waiting"
                } else {
                    "A decision is waiting in another tab — open that tab to answer it"
                };
                assert_eq!(state.pending_leader, Some(Context::Help));
                assert_eq!(state.leader_feedback.as_ref().unwrap().message, expected);
                assert_eq!(
                    state.modal_stack.iter().cloned().collect::<Vec<_>>(),
                    frames
                );
                let layout = layout(&state);
                let painted = draw(&state, &layout, None);
                let region = geometry::modal_region(&layout, painted.area);
                let top = (region.y..region.bottom())
                    .find(|&y| row(&painted, region, y).contains("┌ Ctrl+X "))
                    .expect("owning menu retained");
                let prefix = if waiting == "empty" {
                    expected
                } else {
                    "A decision is waiting in another tab"
                };
                assert!(
                    (top + 1..region.bottom()).any(|y| row(&painted, region, y).contains(prefix))
                );
            }
            assert_eq!(
                (&state.input_buffer, state.cursor_position),
                (&"kept draft".to_owned(), 4)
            );
        }
    }
}

#[test]
fn reverse_search_matches_paint_above_the_unchanged_composer() {
    let mut h = crate::e2e_harness::TestHarness::new();
    h.state.input_buffer = "retained-composer".into();
    h.state.cursor_position = 4;
    h.state.input_history.push("history-first".into());
    h.state.input_history.push("history-second".into());
    press(&mut h.state, Key::Ctrl('r'));
    press(&mut h.state, Key::Char('h'));
    h.render();
    let screen = h.screen_text();
    assert!(
        screen.contains("history-first"),
        "history results must not be clipped to composer height"
    );
    assert!(screen.contains("history-second"));
    assert!(
        screen.contains("retained-composer"),
        "popup never paints over the composer"
    );
}
