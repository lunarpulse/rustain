//! Front-door witnesses: only the top owner receives ingress; Search owns Stage navigation.
use super::keymap_support::press;
use rustain::adapters::tui::app::{InputAction, handle_input};
use rustain::adapters::tui::keymap::Key;
use rustain::adapters::tui::presentation::{self, Surface};
use rustain::adapters::tui::quit_confirm::{QuitFacts, QuitHost};
use rustain::adapters::tui::state::{SearchSubstate, TuiState};
use rustain::adapters::tui::{notification_log, quit_confirm};
use rustain::domain::events::{DomainInputEvent, DomainKey};
use rustain::domain::models::{RequestId, ToolRisk, tool_call::ApprovalSource};
use rustain::domain::models::{ScrollDelta, TabManager};
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

pub(super) fn request_permission(state: &mut TuiState) -> RequestId {
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
            false,
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
    assert_eq!(state.command_palette.filter_text.as_str(), "mo");
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

fn search_press(state: &mut TuiState, tabs: &mut TabManager, key: Key) -> InputAction {
    let action = press(state, key);
    let conversation = tabs.active_tab().conversation.clone();
    rustain::adapters::tui::handlers::search::handle_search_action(
        tabs,
        state,
        &conversation,
        &action,
    );
    action
}

fn search_top(navigating: bool) -> (crate::e2e_harness::TestHarness, TabManager) {
    let mut h = crate::e2e_harness::TestHarness::new();
    h.send_message("needle first\nneedle second");
    let mut tabs = TabManager::new(tokio_util::sync::CancellationToken::new());
    tabs.active_tab_mut().conversation = std::mem::take(&mut h.conversation);
    h.state.active_tab_id = tabs.active_tab().id;
    h.render_active_tab(&tabs);
    assert_eq!(
        search_press(&mut h.state, &mut tabs, Key::Ctrl('f')),
        InputAction::OpenSearch
    );
    for c in "needle".chars() {
        assert_eq!(
            search_press(&mut h.state, &mut tabs, Key::Char(c)),
            InputAction::SearchQueryChanged
        );
    }
    assert_eq!(h.state.search_state.matches.len(), 2);
    if navigating {
        assert_eq!(
            search_press(&mut h.state, &mut tabs, Key::Enter),
            InputAction::SearchCommit
        );
    }
    assert_eq!(h.state.modal_stack.top().unwrap().surface, Surface::Search);
    assert_eq!(
        h.state.search_state.substate,
        if navigating {
            SearchSubstate::Navigating
        } else {
            SearchSubstate::Typing
        }
    );
    h.state.input_buffer = "untouched Search draft".into();
    h.state.cursor_position = 5;
    h.state.help_overlay.scroll_offset = 7;
    (h, tabs)
}

pub(super) fn assert_permission_deferred(state: &TuiState, id: &RequestId) {
    assert!(
        state.pending_permission.is_none(),
        "{id:?} must stay queued"
    );
    assert_eq!(state.permission_queue.len(), 1);
}

pub(super) fn assert_permission_painted(
    h: &mut crate::e2e_harness::TestHarness,
    tabs: &TabManager,
    id: &RequestId,
) {
    // Assert promotion BEFORE painting: neither a render nor another key may repair it.
    assert_eq!(h.state.pending_permission.as_ref().map(|p| &p.id), Some(id));
    assert!(h.state.permission_queue.is_empty());
    h.render_active_tab(tabs);
    let state = &mut h.state;
    h.terminal
        .draw(|frame| {
            let layout = rustain::adapters::tui::layout::compute_layout(
                frame.area(),
                &state.theme,
                &state.input_buffer,
                1,
                false,
                rustain::domain::models::visual::DensityMode::Focus,
            )
            .unwrap();
            // The active-tab fixture omits legacy cards; use the runtime's real
            // permission painter with the SAME state, reader and chat viewport.
            let _ = rustain::adapters::tui::widgets::permission_prompt::render_permission_surface(
                frame,
                layout.chat_pane,
                state.pending_permission.as_ref(),
                state.pending_feedback_input.as_ref(),
                state.permission_queue.len(),
                &mut state.permission_reader,
                &state.theme,
            );
        })
        .unwrap();
    h.assert_screen_contains(
        "real permission admission",
        "the queued request paints on release",
    );
    assert_eq!(h.state.pending_permission.as_ref().map(|p| &p.id), Some(id));
    assert_eq!(
        press(&mut h.state, Key::Char('y')),
        InputAction::PermissionAllow
    );
}

fn search_release_promotes_permission(key: Key, navigating: bool) {
    let (mut h, mut tabs) = search_top(navigating);
    let saved_view = format!(
        "{:?}",
        h.state.search_state.pre_search_view.as_ref().unwrap()
    );
    if navigating {
        assert_eq!(
            search_press(&mut h.state, &mut tabs, Key::Char('n')),
            InputAction::SearchNext
        );
        assert_eq!(h.state.search_state.focused_match_index, 1);
        assert_eq!(
            h.state.search_state.query.as_str(),
            "needle",
            "n navigates, not types"
        );
    }
    let current_view = format!("{:?}", tabs.active_tab().view_state);
    let id = request_permission(&mut h.state);
    assert_permission_deferred(&h.state, &id);
    let action = press(&mut h.state, key);
    assert_eq!(
        action,
        if key == Key::Enter {
            InputAction::SearchCommit
        } else {
            InputAction::CloseSearch
        }
    );
    assert_permission_deferred(&h.state, &id);
    assert!(presentation::is_open(&h.state, Surface::Search));
    let conversation = tabs.active_tab().conversation.clone();
    rustain::adapters::tui::handlers::search::handle_search_action(
        &mut tabs,
        &mut h.state,
        &conversation,
        &action,
    );
    assert!(h.state.modal_stack.is_empty());
    assert!(h.state.search_state.pre_search_view.is_none());
    assert_eq!(
        format!("{:?}", tabs.active_tab().view_state),
        if key == Key::Enter {
            current_view
        } else {
            saved_view
        }
    );
    assert_permission_painted(&mut h, &tabs, &id);
}

#[test]
fn search_escape_release_promotes_and_paints_the_queued_request() {
    for navigating in [false, true] {
        search_release_promotes_permission(Key::Esc, navigating);
    }
}

#[test]
fn search_ctrl_c_release_promotes_and_paints_the_queued_request() {
    for navigating in [false, true] {
        search_release_promotes_permission(Key::Ctrl('c'), navigating);
    }
}

#[test]
fn search_navigating_enter_release_promotes_and_paints_the_queued_request() {
    search_release_promotes_permission(Key::Enter, true);
}

#[test]
fn search_top_routes_every_key_without_falling_through_to_the_composer_or_cards() {
    for navigating in [false, true] {
        for key in Key::universe() {
            let (mut h, mut tabs) = search_top(navigating);
            let draft = (h.state.input_buffer.clone(), h.state.cursor_position);
            let action = search_press(&mut h.state, &mut tabs, key);
            assert!(
                matches!(
                    action,
                    InputAction::Consumed
                        | InputAction::Ignored
                        | InputAction::Quit
                        | InputAction::CloseSearch
                        | InputAction::SearchCommit
                        | InputAction::SearchQueryChanged
                        | InputAction::SearchReturnToTyping
                        | InputAction::SearchNext
                        | InputAction::SearchPrev
                ),
                "navigating={navigating} key={key:?}: {action:?}"
            );
            assert_eq!(
                (h.state.input_buffer.clone(), h.state.cursor_position),
                draft,
                "navigating={navigating} key={key:?}: {action:?}"
            );
            assert_eq!(h.state.help_overlay.scroll_offset, 7);
            assert!(h.state.pending_images.is_empty());
        }
        for event in [
            DomainInputEvent::Paste("query paste, not composer input".into()),
            DomainInputEvent::ImagePaste(vec![1, 2, 3]),
        ] {
            let (mut h, mut tabs) = search_top(navigating);
            let frame = h.state.modal_stack.top().cloned();
            let before = (
                h.state.input_buffer.clone(),
                h.state.cursor_position,
                h.state.search_state.query.clone(),
                h.state.search_state.focused_match_index,
                format!("{:?}", tabs.active_tab().view_state),
            );
            let action = handle_input(&mut h.state, &event);
            let conversation = tabs.active_tab().conversation.clone();
            rustain::adapters::tui::handlers::search::handle_search_action(
                &mut tabs,
                &mut h.state,
                &conversation,
                &action,
            );
            if let DomainInputEvent::Paste(text) = &event {
                assert_eq!(
                    action,
                    if navigating {
                        InputAction::SearchReturnToTyping
                    } else {
                        InputAction::SearchQueryChanged
                    }
                );
                assert_eq!(
                    (h.state.input_buffer.clone(), h.state.cursor_position),
                    (before.0, before.1)
                );
                assert_eq!(
                    h.state.search_state.query.as_str(),
                    format!("{}{text}", before.2.as_str())
                );
                assert_eq!(
                    h.state.search_state.query.cursor(),
                    before.2.cursor() + text.chars().count()
                );
                assert!(h.state.search_state.matches.is_empty());
                assert_eq!(h.state.search_state.focused_match_index, 0);
                assert_eq!(format!("{:?}", tabs.active_tab().view_state), before.4);
            } else {
                assert_eq!(action, InputAction::Consumed);
                assert_eq!(
                    (
                        h.state.input_buffer.clone(),
                        h.state.cursor_position,
                        h.state.search_state.query.clone(),
                        h.state.search_state.focused_match_index,
                        format!("{:?}", tabs.active_tab().view_state)
                    ),
                    before
                );
            }
            assert_eq!(h.state.modal_stack.top(), frame.as_ref());
            assert!(h.state.pending_images.is_empty());
        }
    }
    let mut state = TuiState::new(80, 24);
    assert_eq!(
        handle_input(&mut state, &DomainInputEvent::Paste("base paste".into())),
        InputAction::Consumed
    );
    assert_eq!(
        state.input_buffer, "base paste",
        "base paste remains available"
    );
}

#[test]
fn search_context_switch_and_permission_controls_use_actual_dispatch() {
    let (mut h, mut tabs) = search_top(false);
    let id = request_permission(&mut h.state);
    assert!(h.state.pending_permission.is_none());
    assert_eq!(h.state.permission_queue.len(), 1);
    assert_eq!(
        search_press(&mut h.state, &mut tabs, Key::Char('y')),
        InputAction::SearchQueryChanged
    );
    assert_eq!(h.state.search_state.query.as_str(), "needley");
    assert!(
        h.state.pending_permission.is_none(),
        "{id:?} must stay queued"
    );
    assert_eq!(h.state.permission_queue.len(), 1);
    assert_eq!(h.state.input_buffer, "untouched Search draft");

    // Close this exact fixture, not a fresh Search without the queued request.
    assert_eq!(
        search_press(&mut h.state, &mut tabs, Key::Ctrl('c')),
        InputAction::CloseSearch
    );
    assert!(h.state.modal_stack.is_empty());
    assert_permission_painted(&mut h, &tabs, &id);
    approval_bridge::apply(
        &mut h.state,
        ApprovalRuntimeEvent::Cancelled {
            id,
            reason: rustain::domain::services::approval_runtime::CancelReason::SourceAborted,
        },
        None,
    );
    assert_eq!(
        press(&mut h.state, Key::Ctrl('c')),
        InputAction::CancelOrQuit
    );
}

#[test]
fn quit_cancel_restores_the_identical_search_frame_and_query_in_both_contexts() {
    for navigating in [false, true] {
        for cancel in [Key::Char('n'), Key::Esc, Key::Ctrl('c')] {
            let (mut h, mut tabs) = search_top(navigating);
            let frame = h.state.modal_stack.top().cloned();
            let before = (
                h.state.search_state.query.clone(),
                h.state.search_state.matches.clone(),
                h.state.search_state.focused_match_index,
                h.state.search_state.substate,
                h.state.search_state.owning_tab,
                format!("{:?}", h.state.search_state.pre_search_view),
                format!("{:?}", tabs.active_tab().view_state),
                h.state.input_buffer.clone(),
                h.state.cursor_position,
            );
            let id = request_permission(&mut h.state);
            assert_permission_deferred(&h.state, &id);
            assert_eq!(
                search_press(&mut h.state, &mut tabs, Key::Ctrl('q')),
                InputAction::Quit
            );
            quit_confirm::request(
                &mut h.state,
                QuitFacts {
                    streams: 1,
                    ..QuitFacts::idle(QuitHost::Local)
                },
            );
            assert_eq!(
                h.state.modal_stack.top().unwrap().surface,
                Surface::QuitConfirm
            );
            assert_eq!(
                search_press(&mut h.state, &mut tabs, cancel),
                InputAction::Consumed
            );
            assert_eq!(h.state.modal_stack.top(), frame.as_ref());
            assert_eq!(
                (
                    h.state.search_state.query.clone(),
                    h.state.search_state.matches.clone(),
                    h.state.search_state.focused_match_index,
                    h.state.search_state.substate,
                    h.state.search_state.owning_tab,
                    format!("{:?}", h.state.search_state.pre_search_view),
                    format!("{:?}", tabs.active_tab().view_state),
                    h.state.input_buffer.clone(),
                    h.state.cursor_position,
                ),
                before
            );
            assert_permission_deferred(&h.state, &id);
            search_press(&mut h.state, &mut tabs, Key::Esc);
            assert!(h.state.modal_stack.is_empty());
            assert_permission_painted(&mut h, &tabs, &id);
        }
    }
}

#[test]
fn reverse_search_accept_and_cancel_promote_and_paint_the_exact_queued_request() {
    for key in [Key::Enter, Key::Esc, Key::Ctrl('c')] {
        let mut h = crate::e2e_harness::TestHarness::new();
        let tabs = TabManager::new(tokio_util::sync::CancellationToken::new());
        h.state.active_tab_id = tabs.active_tab_id();
        h.state.input_history.push("selected history entry".into());
        h.state.input_buffer = "retained draft".into();
        h.state.cursor_position = 4;
        assert_eq!(press(&mut h.state, Key::Ctrl('r')), InputAction::Consumed);
        assert_eq!(press(&mut h.state, Key::Char('s')), InputAction::Consumed);
        assert!(h.state.reverse_search.is_open());
        assert_eq!(h.state.reverse_search.matches.len(), 1);
        let id = request_permission(&mut h.state);
        assert_permission_deferred(&h.state, &id);
        assert_eq!(press(&mut h.state, key), InputAction::Consumed);
        assert!(!h.state.reverse_search.is_open());
        assert!(h.state.modal_stack.is_empty());
        assert_eq!(
            h.state.input_buffer,
            if key == Key::Enter {
                "selected history entry"
            } else {
                "retained draft"
            }
        );
        assert_eq!(
            h.state.cursor_position,
            if key == Key::Enter {
                "selected history entry".len()
            } else {
                4
            }
        );
        assert_permission_painted(&mut h, &tabs, &id);
    }
}

#[tokio::test]
async fn every_bookmark_close_effect_promotes_and_paints_the_queued_request() {
    use rustain::adapters::{filesystem::FileSystemStorage, tui::handlers::bookmark};
    for path in [
        "esc",
        "ctrl-c",
        "jump",
        "empty-jump",
        "invalid-jump",
        "last-delete",
        "close",
        "empty-open",
        "wrong-stage",
    ] {
        let (mut h, mut tabs) = super::stage_support::stage(80, 24);
        assert_eq!(
            press(&mut h.state, Key::Char('\'')),
            InputAction::OpenBookmarkList
        );
        bookmark::handle_apply_open_bookmark_list(
            &tabs,
            &mut h.state,
            &tabs.active_tab().conversation,
        );
        assert!(
            presentation::is_open(&h.state, Surface::BookmarkList),
            "{path}: reachable owner"
        );
        let id = request_permission(&mut h.state);
        assert_permission_deferred(&h.state, &id);
        let mut conversation = tabs.active_tab().conversation.clone();
        match path {
            "esc" | "ctrl-c" => {
                assert_eq!(
                    press(
                        &mut h.state,
                        if path == "esc" {
                            Key::Esc
                        } else {
                            Key::Ctrl('c')
                        }
                    ),
                    InputAction::CloseBookmarkList
                );
            }
            "jump" | "empty-jump" | "invalid-jump" | "wrong-stage" => {
                if path == "empty-jump" {
                    tabs.active_tab_mut().session_meta.bookmarks.clear();
                }
                if path == "invalid-jump" {
                    tabs.active_tab_mut().session_meta.bookmarks = vec![999];
                }
                if path == "wrong-stage" {
                    conversation.id = "not-the-owned-stage".into();
                }
                assert_eq!(press(&mut h.state, Key::Enter), InputAction::JumpToBookmark);
                bookmark::handle_apply_jump_bookmark(&mut tabs, &mut h.state, &conversation);
            }
            "last-delete" => {
                tabs.active_tab_mut().session_meta.bookmarks = vec![0];
                let dir = tempfile::tempdir().unwrap();
                let storage = FileSystemStorage::new(dir.path().join("sessions"));
                assert_eq!(
                    press(&mut h.state, Key::Char('d')),
                    InputAction::DeleteBookmark
                );
                bookmark::handle_apply_delete_bookmark(
                    &mut tabs,
                    &mut h.state,
                    &conversation,
                    &storage,
                )
                .await;
                assert!(tabs.active_tab().session_meta.bookmarks.is_empty());
            }
            "empty-open" => {
                tabs.active_tab_mut().session_meta.bookmarks.clear();
                bookmark::handle_apply_open_bookmark_list(
                    &tabs,
                    &mut h.state,
                    &tabs.active_tab().conversation,
                );
            }
            "close" => bookmark::close(&mut h.state),
            _ => unreachable!(),
        }
        assert!(
            !presentation::is_open(&h.state, Surface::BookmarkList),
            "{path}"
        );
        assert_permission_painted(&mut h, &tabs, &id);
        // Runtime's repeated close must not overwrite the newly promoted card.
        bookmark::close(&mut h.state);
        assert_eq!(
            h.state.pending_permission.as_ref().map(|p| &p.id),
            Some(&id)
        );
    }
}

#[test]
fn covered_bookmark_effects_do_not_promote_until_the_last_owner_releases() {
    use rustain::adapters::tui::handlers::bookmark;
    for close in [false, true] {
        let (mut h, mut tabs) = super::stage_support::stage(80, 24);
        bookmark::handle_apply_open_bookmark_list(
            &tabs,
            &mut h.state,
            &tabs.active_tab().conversation,
        );
        press(&mut h.state, Key::Ctrl('x'));
        press(&mut h.state, Key::Char('?'));
        let help = h.state.modal_stack.top().cloned().unwrap();
        assert_eq!(help.surface, Surface::Help);
        let id = request_permission(&mut h.state);
        if close {
            bookmark::close(&mut h.state);
        } else {
            let conversation = tabs.active_tab().conversation.clone();
            let view = super::stage_support::view(&tabs);
            bookmark::handle_apply_jump_bookmark(&mut tabs, &mut h.state, &conversation);
            assert_eq!(super::stage_support::view(&tabs), view);
            assert!(presentation::is_open(&h.state, Surface::BookmarkList));
        }
        assert_eq!(h.state.modal_stack.top(), Some(&help));
        assert_permission_deferred(&h.state, &id);
        press(&mut h.state, Key::Esc);
        if !close {
            assert_permission_deferred(&h.state, &id);
            press(&mut h.state, Key::Esc);
        }
        assert_permission_painted(&mut h, &tabs, &id);
    }
}

#[test]
fn every_wave_close_effect_promotes_and_paints_without_cancelling_the_run() {
    use rustain::adapters::tui::handlers::wave_overlay;
    use std::sync::Arc;
    for path in [
        "esc",
        "ctrl-c",
        "direct-div-esc",
        "direct-div-ctrl-c",
        "list-div-esc",
        "list-div-s",
        "close",
        "dismiss",
        "covered-close",
    ] {
        let (mut h, tabs) = super::stage_support::stage(80, 24);
        let run: Arc<dyn rustain::domain::ports::wave_handle::WaveHandle> =
            Arc::new(super::keymap_support::StubWave(3));
        h.state.wave_run = Some(run.clone());
        let conversation_id = &tabs.active_tab().conversation.id;
        let open = press(
            &mut h.state,
            if path.starts_with("direct-div") {
                Key::Char('d')
            } else {
                Key::Char('r')
            },
        );
        wave_overlay::apply(&mut h.state, conversation_id, &open);
        if path.starts_with("list-div") {
            let action = press(&mut h.state, Key::Char('d'));
            wave_overlay::apply(&mut h.state, conversation_id, &action);
        }
        assert!(
            presentation::is_open(&h.state, Surface::WaveOverlay),
            "{path}: actual opener"
        );
        let cover = if path == "covered-close" {
            press(&mut h.state, Key::Ctrl('x'));
            press(&mut h.state, Key::Char('?'));
            Some(h.state.modal_stack.top().cloned().unwrap())
        } else {
            None
        };
        let id = request_permission(&mut h.state);
        assert_permission_deferred(&h.state, &id);
        let action = match path {
            "close" | "covered-close" => InputAction::CloseWaveOverlay,
            "dismiss" => InputAction::DismissWave,
            "ctrl-c" | "direct-div-ctrl-c" => press(&mut h.state, Key::Ctrl('c')),
            "list-div-s" => press(&mut h.state, Key::Char('s')),
            _ => press(&mut h.state, Key::Esc),
        };
        wave_overlay::apply(&mut h.state, conversation_id, &action);
        if let Some(cover) = cover {
            assert!(
                presentation::is_open(&h.state, Surface::WaveOverlay),
                "a covered foreground action is ignored"
            );
            assert_eq!(h.state.modal_stack.top(), Some(&cover));
            assert_permission_deferred(&h.state, &id);
            press(&mut h.state, Key::Esc);
            assert_permission_deferred(&h.state, &id);
            wave_overlay::apply(
                &mut h.state,
                conversation_id,
                &InputAction::CloseWaveOverlay,
            );
        }
        assert!(
            !presentation::is_open(&h.state, Surface::WaveOverlay),
            "{path}"
        );
        assert_permission_painted(&mut h, &tabs, &id);
        if path == "dismiss" {
            assert!(h.state.wave_run.is_none());
        } else {
            assert!(Arc::ptr_eq(h.state.wave_run.as_ref().unwrap(), &run));
        }
    }
}

#[test]
fn stage_reconcile_releases_the_last_owner_without_clobbering_its_promoted_card() {
    use rustain::adapters::tui::handlers::{bookmark, wave_overlay};
    use rustain::infrastructure::runtime::event_loop::replace_stage_conversation;
    for wave in [false, true] {
        for covered in [false, true] {
            let (mut h, mut tabs) = super::stage_support::stage(80, 24);
            if wave {
                h.state.wave_run = Some(std::sync::Arc::new(super::keymap_support::StubWave(3)));
                let action = press(&mut h.state, Key::Char('r'));
                wave_overlay::apply(&mut h.state, &tabs.active_tab().conversation.id, &action);
            } else {
                bookmark::handle_apply_open_bookmark_list(
                    &tabs,
                    &mut h.state,
                    &tabs.active_tab().conversation,
                );
            }
            if covered {
                press(&mut h.state, Key::Ctrl('x'));
                press(&mut h.state, Key::Char('?'));
            }
            let global = covered.then(|| h.state.modal_stack.top().cloned().unwrap());
            let id = request_permission(&mut h.state);
            assert_permission_deferred(&h.state, &id);
            let replacement = rustain::domain::models::Conversation::default();
            replace_stage_conversation(
                &mut tabs.active_tab_mut().conversation,
                &mut h.state,
                replacement,
            );
            // This is the normal post-load/reset base target in the production arms.
            presentation::reducer::settle_stage_base(
                &mut h.state,
                rustain::domain::models::FocusState::Input,
            );
            if let Some(global) = global {
                assert_eq!(h.state.modal_stack.top(), Some(&global));
                assert_permission_deferred(&h.state, &id);
                press(&mut h.state, Key::Esc);
            }
            assert_permission_painted(&mut h, &tabs, &id);
        }
    }
}

pub(super) fn assert_runtime_permission_painted(
    h: &mut crate::e2e_harness::TestHarness,
    tabs: &mut TabManager,
    core: &rustain::infrastructure::runtime::agent_core::AgentCore,
    id: &RequestId,
) {
    assert_eq!(h.state.pending_permission.as_ref().map(|p| &p.id), Some(id));
    assert!(
        h.state.permission_queue.is_empty(),
        "promotion precedes paint or another key"
    );
    let screen = crate::conformance_tui_orchestration::render_runtime(h, tabs, core);
    assert!(screen.contains("real permission admission"), "{screen}");
    assert_eq!(h.state.pending_permission.as_ref().map(|p| &p.id), Some(id));
    assert_eq!(
        press(&mut h.state, Key::Char('y')),
        InputAction::PermissionAllow
    );
}

#[test]
fn cross_search_closing_paths_settle_real_queued_permissions_and_covering_does_not() {
    use rustain::adapters::tui::{app, handlers::export};
    use rustain::infrastructure::runtime::agent_core::AgentCore;
    let core = AgentCore::test_noop();
    for path in ["Esc", "Ctrl+C", "back", "close", "replacement", "covered"] {
        let mut h = crate::e2e_harness::TestHarness::with_size(130, 30);
        let mut tabs = TabManager::new(tokio_util::sync::CancellationToken::new());
        tabs.active_tab_mut().conversation = h.conversation.clone();
        h.state = super::k7_cross_search::history();
        h.state.active_tab_id = tabs.active_tab_id();
        super::k7_cross_search::open(&mut h.state);
        if path == "replacement" {
            export::close_cross_search(&mut h.state);
            // Reducer-unit depth-cap fixture, not an unavailable History chord
            // claimed reachable inside another frame.
            for surface in [
                Surface::Help,
                Surface::ModelSelector,
                Surface::ProfileSwitcher,
                Surface::CrossSearch,
            ] {
                presentation::reducer::open(&mut h.state, surface);
            }
            assert_eq!(h.state.modal_stack.len(), 4);
        }
        for ch in "ab".chars() {
            assert_eq!(
                press(&mut h.state, Key::Char(ch)),
                InputAction::CrossSearchQueryChanged
            );
        }
        let pending = export::apply_cross_search_query_change(&mut h.state);
        let owner = h.state.modal_stack.top().unwrap().id;
        let id = request_permission(&mut h.state);
        assert_permission_deferred(&h.state, &id);
        match path {
            "Esc" | "Ctrl+C" => {
                let key = if path == "Esc" {
                    Key::Esc
                } else {
                    Key::Ctrl('c')
                };
                assert_eq!(press(&mut h.state, key), InputAction::CloseCrossSearch);
                assert_permission_deferred(&h.state, &id);
                export::close_cross_search(&mut h.state);
            }
            "back" => {
                app::with_card_focus_reconcile(&mut h.state, |s| {
                    presentation::reducer::back(s);
                });
            }
            "close" => {
                app::with_card_focus_reconcile(&mut h.state, |s| {
                    presentation::reducer::close(s, Surface::CrossSearch)
                });
            }
            "replacement" => {
                press(&mut h.state, Key::Ctrl('p'));
                assert_eq!(
                    h.state.modal_stack.top().unwrap().surface,
                    Surface::CommandPalette
                );
                assert!(!presentation::is_open(&h.state, Surface::CrossSearch));
                assert!(h.state.cross_search.query.is_empty());
                assert_permission_deferred(&h.state, &id);
                while !h.state.modal_stack.is_empty() {
                    app::with_card_focus_reconcile(&mut h.state, |s| {
                        presentation::reducer::back(s);
                    });
                }
            }
            "covered" => {
                press(&mut h.state, Key::Ctrl('p'));
                assert_eq!(
                    h.state.modal_stack.top().unwrap().surface,
                    Surface::CommandPalette
                );
                export::close_cross_search(&mut h.state);
                assert!(h.state.modal_stack.iter().any(|f| f.id == owner));
                assert_eq!(h.state.cross_search.query.as_str(), "ab");
                assert!(h.state.cross_search.running);
                assert_permission_deferred(&h.state, &id);
                press(&mut h.state, Key::Esc);
                assert_eq!(h.state.modal_stack.top().unwrap().id, owner);
                assert_eq!(press(&mut h.state, Key::Esc), InputAction::CloseCrossSearch);
                export::close_cross_search(&mut h.state);
            }
            _ => unreachable!(),
        }
        assert!(
            !presentation::is_open(&h.state, Surface::CrossSearch),
            "{path}"
        );
        let export::CrossSearchScanAction::Spawn {
            query,
            frame_id,
            revision,
        } = pending
        else {
            panic!("real pending request");
        };
        assert_eq!(
            export::apply_cross_search_results(
                &mut h.state,
                frame_id,
                revision,
                query,
                vec![],
                false,
                false
            ),
            export::CrossSearchResultsOutcome::DiscardedStale,
            "{path}"
        );
        assert_runtime_permission_painted(&mut h, &mut tabs, &core, &id);
    }
}
