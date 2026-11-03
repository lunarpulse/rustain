//! Front-door witnesses: a Log/Quit key must never reach the composer or cards below.
use super::keymap_support::press;
use rustain::adapters::tui::app::{InputAction, handle_input};
use rustain::adapters::tui::keymap::Key;
use rustain::adapters::tui::presentation::{self, Surface};
use rustain::adapters::tui::quit_confirm::{QuitFacts, QuitHost};
use rustain::adapters::tui::state::TuiState;
use rustain::adapters::tui::{notification_log, quit_confirm};
use rustain::domain::events::{DomainInputEvent, DomainKey};
use rustain::domain::models::ScrollDelta;
use rustain::domain::models::{RequestId, ToolRisk, tool_call::ApprovalSource};
use rustain::domain::services::approval_runtime::ApprovalRuntimeEvent;
use rustain::infrastructure::runtime::approval_bridge;

fn log() -> TuiState {
    let mut state = TuiState::new(80, 24);
    state.input_buffer = "untouched draft".into();
    state.cursor_position = 5;
    press(&mut state, Key::Ctrl('x'));
    press(&mut state, Key::Char('n'));
    state
}

fn request_permission(state: &mut TuiState) -> RequestId {
    let id = RequestId::new();
    approval_bridge::apply(
        state,
        ApprovalRuntimeEvent::Requested {
            id: id.clone(),
            source: ApprovalSource::ForegroundTurn {
                conversation_id: "active".into(),
            },
            tool: "Bash".into(),
            risk: ToolRisk::Elevated,
            consent_card: Some("real permission admission".into()),
        },
        None,
    );
    id
}

#[tokio::test]
async fn only_the_top_frame_receives_keys() {
    for surface in [
        Surface::NotificationLog,
        Surface::Help,
        Surface::CommandPalette,
        Surface::DecisionPane,
        Surface::QuitConfirm,
        Surface::ModelSelector,
        Surface::ProfileSwitcher,
        Surface::UsagePanel,
        Surface::Login,
    ] {
        for key in Key::universe() {
            let mut state = log();
            if matches!(surface, Surface::Help | Surface::CommandPalette) {
                press(&mut state, Key::Ctrl('x'));
                press(&mut state, Key::Char('?'));
                state.help_overlay.scroll_offset = 7;
            }
            if surface == Surface::CommandPalette {
                press(&mut state, Key::Ctrl('p'));
            }
            if surface == Surface::DecisionPane {
                rustain::adapters::tui::decision_pane::raise(
                    &mut state,
                    rustain::adapters::tui::decision_pane::DecisionDraft::new(
                        rustain::adapters::tui::decision_pane::DecisionKind::Budget,
                        "pane owns the key",
                    ),
                    rustain::adapters::tui::decision_pane::Arrival::Requested,
                );
            }
            if surface == Surface::QuitConfirm {
                quit_confirm::request(
                    &mut state,
                    QuitFacts {
                        streams: 1,
                        ..QuitFacts::idle(QuitHost::Local)
                    },
                );
            }
            if matches!(
                surface,
                Surface::ModelSelector
                    | Surface::ProfileSwitcher
                    | Surface::UsagePanel
                    | Surface::Login
            ) {
                tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    super::k7_surfaces::open(&mut state, surface),
                )
                .await
                .expect("bounded real opener");
            }
            assert_eq!(state.modal_stack.top().unwrap().surface, surface);
            let before = (
                state.input_buffer.clone(),
                state.cursor_position,
                state.scroll_offset(),
                (surface != Surface::Help).then_some(state.help_overlay.scroll_offset),
            );
            let action = press(&mut state, key);
            assert!(
                !matches!(
                    action,
                    InputAction::CancelOrQuit
                        | InputAction::PermissionAllow
                        | InputAction::PermissionDeny
                        | InputAction::MouseScroll(_)
                ),
                "surface={surface:?} key={key:?}: {action:?}"
            );
            assert_eq!(
                (
                    state.input_buffer.clone(),
                    state.cursor_position,
                    state.scroll_offset(),
                    (surface != Surface::Help).then_some(state.help_overlay.scroll_offset)
                ),
                before,
                "surface={surface:?} key={key:?}: {action:?}"
            );
        }
        for event in [
            DomainInputEvent::Paste("not composer text".into()),
            DomainInputEvent::ImagePaste(vec![1, 2, 3]),
            DomainInputEvent::MouseScroll(ScrollDelta::WheelUp(2)),
        ] {
            let mut state = log();
            if matches!(surface, Surface::Help | Surface::CommandPalette) {
                press(&mut state, Key::Ctrl('x'));
                press(&mut state, Key::Char('?'));
            }
            if surface == Surface::CommandPalette {
                press(&mut state, Key::Ctrl('p'));
            }
            if surface == Surface::DecisionPane {
                rustain::adapters::tui::decision_pane::raise(
                    &mut state,
                    rustain::adapters::tui::decision_pane::DecisionDraft::new(
                        rustain::adapters::tui::decision_pane::DecisionKind::Budget,
                        "pane owns ingress",
                    ),
                    rustain::adapters::tui::decision_pane::Arrival::Requested,
                );
            }
            if surface == Surface::QuitConfirm {
                quit_confirm::request(
                    &mut state,
                    QuitFacts {
                        streams: 1,
                        ..QuitFacts::idle(QuitHost::Local)
                    },
                );
            }
            if matches!(
                surface,
                Surface::ModelSelector
                    | Surface::ProfileSwitcher
                    | Surface::UsagePanel
                    | Surface::Login
            ) {
                tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    super::k7_surfaces::open(&mut state, surface),
                )
                .await
                .expect("bounded real opener");
            }
            assert_eq!(state.modal_stack.top().unwrap().surface, surface);
            let draft = state.input_buffer.clone();
            let action = handle_input(&mut state, &event);
            assert_eq!(action, InputAction::Consumed);
            assert_eq!(state.input_buffer, draft);
            assert!(state.pending_images.is_empty());
        }
    }
    let mut state = TuiState::new(80, 24);
    press(&mut state, Key::Char('y'));
    assert_eq!(
        state.input_buffer, "y",
        "positive control: base Input still types"
    );
    assert_eq!(press(&mut state, Key::Ctrl('c')), InputAction::CancelOrQuit);
}

#[test]
fn tiny_terminal_consumes_every_user_ingress_but_resize_recovers() {
    for (w, h) in [(59, 24), (80, 15)] {
        let mut state = TuiState::new(w, h);
        for event in [
            DomainInputEvent::KeyPress('y'),
            DomainInputEvent::SpecialKey(DomainKey::CtrlQ),
            DomainInputEvent::SpecialKey(DomainKey::CtrlC),
            DomainInputEvent::Paste("text".into()),
            DomainInputEvent::ImagePaste(vec![1]),
            DomainInputEvent::MouseScroll(ScrollDelta::WheelUp(2)),
        ] {
            assert_eq!(handle_input(&mut state, &event), InputAction::Consumed);
            assert!(state.input_buffer.is_empty());
            assert!(state.pending_images.is_empty());
        }
        handle_input(&mut state, &DomainInputEvent::Resize(80, 24));
        press(&mut state, Key::Char('y'));
        assert_eq!(state.input_buffer, "y");
    }
}

#[test]
fn quit_cancel_returns_to_the_same_log_and_help() {
    let mut state = log();
    let saved = state.notification_log.clone();
    assert_eq!(press(&mut state, Key::Ctrl('q')), InputAction::Quit);
    quit_confirm::request(
        &mut state,
        QuitFacts {
            streams: 1,
            ..QuitFacts::idle(QuitHost::Local)
        },
    );
    assert_eq!(press(&mut state, Key::Char('n')), InputAction::Consumed);
    assert_eq!(state.notification_log, saved);
    notification_log::close(&mut state);
    press(&mut state, Key::Ctrl('x'));
    press(&mut state, Key::Char('?'));
    state.help_overlay.scroll_offset = 7;
    let focus = state.focus.clone();
    assert_eq!(press(&mut state, Key::Ctrl('q')), InputAction::Quit);
    quit_confirm::request(
        &mut state,
        QuitFacts {
            streams: 1,
            ..QuitFacts::idle(QuitHost::Local)
        },
    );
    press(&mut state, Key::Char('n'));
    assert!(presentation::is_open(&state, Surface::Help));
    assert_eq!(state.help_overlay.scroll_offset, 7);
    assert_eq!(state.focus, focus);
}

#[test]
fn permission_requested_under_log_or_quit_waits_until_the_stack_empties() {
    for surface in ["log", "quit", "log-quit"] {
        let mut state = TuiState::new(80, 24);
        if surface != "quit" {
            press(&mut state, Key::Ctrl('x'));
            press(&mut state, Key::Char('n'));
        }
        if surface != "log" {
            quit_confirm::request(
                &mut state,
                QuitFacts {
                    streams: 1,
                    ..QuitFacts::idle(QuitHost::Local)
                },
            );
        }
        let base = state.focus.clone();
        let id = request_permission(&mut state);
        assert!(
            state.pending_permission.is_none(),
            "{surface}: permission must defer"
        );
        assert_eq!(state.permission_queue.len(), 1);
        assert_eq!(state.focus, base);
        assert_eq!(press(&mut state, Key::Char('a')), InputAction::Consumed);
        press(&mut state, Key::Esc);
        if surface == "log-quit" {
            assert!(state.pending_permission.is_none(), "Log still owns the top");
            press(&mut state, Key::Esc);
        }
        assert_eq!(state.pending_permission.as_ref().map(|p| &p.id), Some(&id));
        assert!(state.permission_queue.is_empty());
        assert_eq!(
            press(&mut state, Key::Char('y')),
            InputAction::PermissionAllow
        );
    }
}

#[test]
fn tiny_permission_is_not_answered_and_the_visible_control_still_answers() {
    let mut state = TuiState::new(80, 24);
    let id = request_permission(&mut state);
    handle_input(&mut state, &DomainInputEvent::Resize(59, 16));
    assert_eq!(press(&mut state, Key::Char('y')), InputAction::Consumed);
    assert_eq!(state.pending_permission.as_ref().map(|p| &p.id), Some(&id));
    handle_input(&mut state, &DomainInputEvent::Resize(80, 24));
    assert_eq!(
        press(&mut state, Key::Char('y')),
        InputAction::PermissionAllow
    );
}

#[test]
fn frame_owned_hints_hide_and_restore_the_underlying_context() {
    fn hint(state: &TuiState) -> Option<String> {
        rustain::adapters::tui::hints::contextual_hint(
            &state.focus,
            state.session_count,
            state.theme.timing.status_hint_fade_sessions,
            state.is_vscode,
            &state.modal_stack,
            state.pending_leader,
        )
    }
    for origin in ["log", "base", "leader", "palette"] {
        let mut state = TuiState::new(130, 30);
        state.input_buffer = "draft".into();
        if origin == "leader" {
            press(&mut state, Key::Ctrl('x'));
        } else if origin == "palette" {
            press(&mut state, Key::Ctrl('p'));
        }
        let before_hint = hint(&state);
        state.current_hint = before_hint.clone(); // The runtime's normal refresh before this key.
        let before_tail = rustain::adapters::tui::app::status_tail(&state);
        assert_eq!(before_hint.is_some(), origin != "palette");
        if origin == "log" {
            press(&mut state, Key::Ctrl('x'));
            press(&mut state, Key::Char('n'));
        } else {
            assert_eq!(press(&mut state, Key::Ctrl('q')), InputAction::Quit);
            quit_confirm::request(
                &mut state,
                QuitFacts {
                    streams: 1,
                    ..QuitFacts::idle(QuitHost::Local)
                },
            );
        }
        assert!(
            hint(&state).is_none() && state.current_hint.is_none(),
            "{origin}: the production painter's cached hint must hide immediately"
        );
        assert_eq!(
            rustain::adapters::tui::app::status_tail(&state),
            if origin == "log" {
                rustain::adapters::tui::keymap::render::StatusTail::Menu
            } else {
                rustain::adapters::tui::keymap::render::StatusTail::Hidden
            }
        );
        if origin == "log" {
            assert_eq!(press(&mut state, Key::Ctrl('q')), InputAction::Quit);
            quit_confirm::request(
                &mut state,
                QuitFacts {
                    streams: 1,
                    ..QuitFacts::idle(QuitHost::Local)
                },
            );
            press(&mut state, Key::Char('n'));
            assert!(
                hint(&state).is_none() && state.current_hint.is_none(),
                "Log still owns the keys after Quit cancel"
            );
            let event = super::keymap_support::decode(Key::Esc).unwrap();
            assert!(
                rustain::adapters::tui::app::try_with_card_focus_reconcile(&mut state, |state| {
                    rustain::adapters::tui::presentation::route(
                        state,
                        &event,
                        rustain::adapters::tui::keymap::Host::Local,
                        &Default::default(),
                    )
                })
                .is_some()
            );
        } else {
            press(&mut state, Key::Char('n'));
        }
        assert_eq!(
            hint(&state),
            before_hint,
            "{origin}: exact base hint returns"
        );
        assert_eq!(
            state.current_hint, before_hint,
            "{origin}: cached hint returns without another key"
        );
        assert_eq!(
            rustain::adapters::tui::app::status_tail(&state),
            before_tail
        );
    }
}

#[test]
fn story21_4_a2a_help_palette_leader_preserve_the_frame_caller() {
    let mut state = TuiState::new(130, 30);
    state.input_buffer = "retained".into();
    state.cursor_position = 3;
    press(&mut state, Key::Ctrl('p'));
    state.command_palette.filter_text = "mo".into();
    press(&mut state, Key::Ctrl('x'));
    press(&mut state, Key::Char('?'));
    press(&mut state, Key::Char('j'));
    press(&mut state, Key::Ctrl('x'));
    press(&mut state, Key::Char('n'));
    assert_eq!(state.modal_stack.len(), 3, "palette / Help / Log");
    let frames: Vec<_> = state.modal_stack.iter().cloned().collect();
    super::k3_compose::open_quit(&mut state);
    assert_eq!(state.modal_stack.len(), 4, "Quit is the fourth live frame");
    press(&mut state, Key::Char('n'));
    assert_eq!(
        state.modal_stack.iter().cloned().collect::<Vec<_>>(),
        frames
    );
    assert!(frames.windows(2).all(|w| w[0].id < w[1].id));
    press(&mut state, Key::Esc);
    assert_eq!(state.modal_stack.len(), 2);
    assert_eq!(state.help_overlay.scroll_offset, 1);
    press(&mut state, Key::Esc);
    assert_eq!(state.modal_stack.len(), 1);
    assert_eq!(state.command_palette.filter_text, "mo");
    press(&mut state, Key::Esc);
    assert!(state.modal_stack.is_empty());
    assert_eq!(state.input_buffer, "retained");
    assert_eq!(state.cursor_position, 3);
    assert_eq!(state.focus, rustain::domain::models::FocusState::Input);
}

#[test]
fn reopening_help_truncates_without_resetting_its_payload() {
    let mut state = TuiState::new(130, 30);
    for key in [
        Key::Ctrl('p'),
        Key::Ctrl('x'),
        Key::Char('?'),
        Key::Char('j'),
    ] {
        press(&mut state, key);
    }
    let help = state.modal_stack.top().cloned();
    for key in [
        Key::Ctrl('x'),
        Key::Char('n'),
        Key::Ctrl('x'),
        Key::Char('?'),
    ] {
        press(&mut state, key);
    }
    assert_eq!(state.modal_stack.len(), 2);
    assert_eq!(state.modal_stack.top(), help.as_ref());
    assert_eq!(state.help_overlay.scroll_offset, 1);
    assert!(rustain::adapters::tui::presentation::consistent(&state));
}

#[test]
fn every_nested_palette_legacy_target_is_unavailable_and_base_routes_are_unchanged() {
    use rustain::adapters::tui::{
        keymap::table::CLOSE_WINDOW_FIRST,
        presentation::{self, Surface},
    };
    use rustain::domain::models::{
        palette::{PaletteAction, PaletteEntry, PaletteScope},
        visual::PanelType,
    };
    let mut targets = vec![
        (
            PaletteAction::OpenPanel(PanelType::History),
            InputAction::OpenPanel(PanelType::History),
        ),
        (
            PaletteAction::DeleteAllConversations,
            InputAction::DeleteAllConversations,
        ),
        (
            PaletteAction::PasteImageFromClipboard,
            InputAction::RequestClipboardPaste,
        ),
    ];
    for panel in [
        PanelType::Tasks,
        PanelType::Agents,
        PanelType::Adapters,
        PanelType::TransparencyLog,
        PanelType::Room,
        PanelType::Artifacts,
    ] {
        targets.push((
            PaletteAction::OpenPanel(panel),
            InputAction::OpenPanel(panel),
        ));
    }
    for name in ["room", "artifacts"] {
        targets.push((
            PaletteAction::ExecuteCommand(name.into(), None),
            InputAction::ExecuteCommand {
                name: name.into(),
                args: None,
            },
        ));
    }
    for (name, args) in [
        ("retract-auto", "peer-response-id"),
        ("team", "view peer"),
        ("team", "retract peer"),
        ("artifact", "apply id"),
        ("artifact", "resolve id"),
        ("peer", "add peer"),
        ("peer", "reach refresh peer"),
        ("fanout", "task"),
        ("memory", "forget id"),
        ("extension-without-policy", "anything"),
        ("mode", "unclassified-argument"),
    ] {
        targets.push((
            PaletteAction::ExecuteCommand(name.into(), Some(args.into())),
            InputAction::ExecuteCommand {
                name: name.into(),
                args: Some(args.into()),
            },
        ));
    }
    targets.extend([
        (
            PaletteAction::ExecuteCommand("cancel-plan".into(), None),
            InputAction::TaskCancelPlan,
        ),
        (
            PaletteAction::ExecuteCommand("reorder-task".into(), None),
            InputAction::TaskReorderEnter(1),
        ),
    ]);
    for (action, expected) in targets {
        for nested in [false, true] {
            let mut state = TuiState::new(130, 30);
            if nested {
                press(&mut state, Key::Ctrl('x'));
                press(&mut state, Key::Char('?'));
            }
            press(&mut state, Key::Ctrl('p'));
            state.command_palette.filtered_entries.push(PaletteEntry {
                name: "selected fixture".into(),
                description: String::new(),
                shortcut: None,
                scope: PaletteScope::All,
                action: action.clone(),
            });
            let frames: Vec<_> = state.modal_stack.iter().cloned().collect();
            let count = state.notifications.records().len();
            let result = press(&mut state, Key::Enter);
            if nested {
                assert_eq!(result, InputAction::Consumed, "{action:?}");
                assert_eq!(
                    state.modal_stack.iter().cloned().collect::<Vec<_>>(),
                    frames
                );
                assert_eq!(state.command_palette.filtered_entries.len(), 1);
                assert_eq!(state.notifications.records().len(), count + 1);
                assert_eq!(
                    state.notifications.records().next_back().unwrap().message,
                    CLOSE_WINDOW_FIRST
                );
            } else {
                assert_eq!(result, expected, "{action:?}");
                assert!(!presentation::is_open(&state, Surface::CommandPalette));
            }
        }
    }
    for action in [
        PaletteAction::ShowVersion,
        PaletteAction::ExecuteCommand("notifications".into(), None),
        PaletteAction::ExecuteCommand("mode".into(), Some("plan".into())),
        PaletteAction::ExecuteCommand("room".into(), Some("list".into())),
        PaletteAction::ExecuteCommand("artifact".into(), Some("list".into())),
        PaletteAction::ExecuteCommand("team".into(), Some("status".into())),
        PaletteAction::ExecuteCommand("peer".into(), Some("list".into())),
        PaletteAction::ExecuteCommand("resume-all-tasks".into(), None),
    ] {
        let mut state = TuiState::new(130, 30);
        press(&mut state, Key::Ctrl('x'));
        press(&mut state, Key::Char('?'));
        press(&mut state, Key::Ctrl('p'));
        state.command_palette.filtered_entries.push(PaletteEntry {
            name: "allowed fixture".into(),
            description: String::new(),
            shortcut: None,
            scope: PaletteScope::All,
            action,
        });
        press(&mut state, Key::Enter);
        assert!(!presentation::is_open(&state, Surface::CommandPalette));
        assert!(presentation::is_open(&state, Surface::Help));
        assert!(
            state
                .notifications
                .records()
                .all(|record| record.message != CLOSE_WINDOW_FIRST)
        );
    }
}
