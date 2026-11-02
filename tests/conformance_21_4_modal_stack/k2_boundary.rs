//! Front-door witnesses: a Log/Quit key must never reach the composer or cards below.
use super::keymap_support::press;
use rustain::adapters::tui::app::{InputAction, handle_input};
use rustain::adapters::tui::keymap::Key;
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

#[test]
fn only_the_top_frame_receives_keys() {
    for quit in [false, true] {
        for key in Key::universe() {
            let mut state = log();
            if quit {
                quit_confirm::request(
                    &mut state,
                    QuitFacts {
                        streams: 1,
                        ..QuitFacts::idle(QuitHost::Local)
                    },
                );
            }
            let before = (
                state.input_buffer.clone(),
                state.cursor_position,
                state.scroll_offset(),
                state.help_overlay.scroll_offset,
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
                "quit={quit} key={key:?}: {action:?}"
            );
            assert_eq!(
                (
                    state.input_buffer.clone(),
                    state.cursor_position,
                    state.scroll_offset(),
                    state.help_overlay.scroll_offset
                ),
                before,
                "quit={quit} key={key:?}: {action:?}"
            );
        }
        for event in [
            DomainInputEvent::Paste("not composer text".into()),
            DomainInputEvent::ImagePaste(vec![1, 2, 3]),
            DomainInputEvent::MouseScroll(ScrollDelta::WheelUp(2)),
        ] {
            let mut state = log();
            if quit {
                quit_confirm::request(
                    &mut state,
                    QuitFacts {
                        streams: 1,
                        ..QuitFacts::idle(QuitHost::Local)
                    },
                );
            }
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
fn quit_cancel_returns_to_the_same_log_and_legacy_help() {
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
    assert!(state.help_overlay.active);
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
        assert!(before_hint.is_some());
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
            rustain::adapters::tui::keymap::render::StatusTail::Hidden
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
