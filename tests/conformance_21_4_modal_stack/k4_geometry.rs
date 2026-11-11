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
use rustain::domain::models::{PermissionMode, StatusState};

#[test]
fn region_ends_above_the_status_row() {
    // Every layout branch, including the optional tab/monitor-column combinations.
    // Offset origins catch confusing Stage height with its absolute bottom.
    let witnesses = [
        ("off", 80, 1, false),
        ("tabs", 80, 2, false),
        ("sidebar", 130, 1, true),
        ("sidebar-tabs", 130, 2, true),
        // The preference is kept below 120 columns; the column is simply not painted (A16).
        ("monitor-narrow", 80, 1, true),
        ("monitor-narrow-119", 119, 1, true),
    ];
    for (name, width, tabs, monitor) in witnesses {
        for full in [false, true] {
            let area = Rect::new(3, 2, width, 30);
            let input = if full {
                ["draft"; 8].join("\n")
            } else {
                String::new()
            };
            let layout = compute_layout(area, &Theme::dark(), &input, tabs, monitor).unwrap();
            assert_eq!(layout.tab_bar.is_some(), tabs > 1, "{name}");
            assert_eq!(
                layout.sidebar.is_some(),
                name.starts_with("sidebar"),
                "{name}"
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
                "Detail",
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
        if !quit {
            assert!(row(&buffer, region, region.y + 3).contains("[c] Copy"));
            let sheet: String = (region.y..region.bottom())
                .map(|y| row(&buffer, region, y))
                .collect();
            assert!(
                sheet.contains("[/] Search") && sheet.contains("[Esc] Close"),
                "{sheet}"
            );
        }
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
                presentation::compose(
                    frame,
                    &state,
                    &shell,
                    Host::Local,
                    &facts,
                    &mut presentation::occlusion::OpaqueRects::new(shell.chat_pane),
                    |_| {},
                );
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
                row(buffer, rect, rect.bottom() - 2).contains("[Enter] Detail")
                    && row(buffer, rect, rect.bottom() - 2).contains("[c] Copy"),
                "{all}"
            );
        }
        for action in ["Compact", "Start fresh", "[/] Search", "[Esc] Close"] {
            assert!(all.contains(action), "{all}");
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

#[test]
fn dense_which_key_keeps_every_registry_key_with_a_full_composer() {
    use super::keymap_support::{press, press_all};
    use ratatui::{style::Modifier, widgets::Paragraph};
    use rustain::adapters::tui::{
        keymap::{AvailCtx, Host, Key, render::leader_menu},
        notification_log::LogFacts,
        presentation,
    };

    // Attached is a host-availability paint twin; ingress uses the real local
    // Ctrl+X handler, never a fabricated pending-leader slot.
    for host in [Host::Local, Host::Attached] {
        for (width, height) in [(60, 16), (80, 24), (130, 30)] {
            for frame_origin in [false, true] {
                let mut state = TuiState::new(width, height);
                state.input_buffer = ["draft"; 8].join("\n");
                let before = layout(&state);
                if frame_origin {
                    press_all(&mut state, &[Key::Ctrl('x'), Key::Char('?')]);
                }
                press(&mut state, Key::Ctrl('x'));
                if frame_origin {
                    // `w` (Monitor density) is the chord a frame origin refuses; `t` is a Mission
                    // Control tab since 21-7 7-D and `h` a window since 7-E: both open from any
                    // origin.
                    press(&mut state, Key::Char('w'));
                }
                assert!(state.pending_leader.is_some());
                let mut avail = match host {
                    Host::Local => AvailCtx::local(width),
                    Host::Attached => AvailCtx::attached(width, false),
                };
                avail.frame_origin = frame_origin;
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal
                    .draw(|frame| {
                        let shell = Shell::new(frame.area(), &before);
                        assert_eq!(shell.input_area.height, 10);
                        if width == 60 {
                            assert_eq!(shell.region.height, 5);
                        }
                        presentation::compose(
                            frame,
                            &state,
                            &shell,
                            host,
                            &LogFacts::default(),
                            &mut presentation::occlusion::OpaqueRects::new(shell.chat_pane),
                            |frame| {
                                frame.render_widget(
                                    Paragraph::new("STATUS untouched"),
                                    shell.status_bar,
                                )
                            },
                        );
                    })
                    .unwrap();
                let buffer = terminal.backend().buffer();
                let region = geometry::modal_region(&before, buffer.area);
                let text = (region.y..region.bottom())
                    .map(|y| row(buffer, region, y))
                    .collect::<Vec<_>>()
                    .join("\n");
                for item in leader_menu(&avail) {
                    let key = format!("[{}]", item.key);
                    let (x, y) = (region.y..region.bottom())
                        .find_map(|y| {
                            let line = row(buffer, region, y);
                            line.find(&key).map(|byte| {
                                (
                                    rustain::adapters::tui::text_geometry::cells(&line[..byte])
                                        as u16,
                                    y,
                                )
                            })
                        })
                        .unwrap_or_else(|| {
                            panic!("{host:?} {width}x{height}: {key} missing in\n{text}")
                        });
                    let cell = &buffer[(region.x + x, y)];
                    let pressed = state
                        .leader_feedback
                        .as_ref()
                        .is_some_and(|feedback| feedback.command == Some(item.command));
                    assert_eq!(
                        cell.fg,
                        if item.available || pressed {
                            state.theme.colors.accent
                        } else {
                            state.theme.colors.fg_muted
                        }
                    );
                    if pressed {
                        assert!(cell.modifier.contains(Modifier::BOLD));
                    }
                    if width == 130 {
                        assert!(text.contains(&item.text()), "full label lost: {text}");
                    }
                }
                if let Some(feedback) = &state.leader_feedback {
                    assert!(
                        row(buffer, region, region.bottom() - 1)
                            .contains(feedback.message.as_ref()),
                        "feedback must own the bottom border: {text}"
                    );
                }
                assert!(
                    row(buffer, before.status_bar, before.status_bar.y)
                        .contains("STATUS untouched")
                );
                let after = layout(&state);
                assert_eq!(
                    (after.chat_pane, after.input_area, after.status_bar),
                    (before.chat_pane, before.input_area, before.status_bar)
                );
                assert_eq!(state.input_buffer, ["draft"; 8].join("\n"));
            }
        }
    }
}
