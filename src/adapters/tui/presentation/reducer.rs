//! The sole owner of Log/Quit frame and payload lifetimes and focus restoration.

use super::{Frame, Opened, Surface, is_open};
use crate::adapters::tui::notification_log::NotificationLogState;
use crate::adapters::tui::quit_confirm::{QuitConfirmState, QuitFacts, QuitRequest};
use crate::adapters::tui::state::TuiState;

/// Open once, leaving an existing frame and its payload untouched.
/// Quit's guarded request installs its facts before calling this function.
pub fn open(state: &mut TuiState, surface: Surface) -> Opened {
    if is_open(state, surface) {
        debug_assert!(super::consistent(state));
        return Opened::Unchanged;
    }
    match surface {
        Surface::NotificationLog => {
            state.notification_log = Some(NotificationLogState {
                selected_id: state
                    .notifications
                    .records()
                    .next_back()
                    .map(|record| record.id),
                selected: 0,
                detail: false,
                detail_scroll: 0,
                caller_focus: state.focus.clone(),
            });
            state.notifications.log_presented();
        }
        Surface::QuitConfirm => {
            assert!(
                state.quit_confirm.is_some(),
                "Quit must open through request_quit"
            );
        }
    }
    let stack = &mut state.modal_stack;
    stack.last_id = stack
        .last_id
        .checked_add(1)
        .expect("modal frame id overflow");
    stack.frames.push(Frame {
        id: stack.last_id,
        surface,
        conversation_id: None,
    });
    state.needs_redraw = true;
    state.current_hint = None;
    debug_assert!(super::consistent(state));
    Opened::Pushed
}

/// Preserve the existing guard facts and direct idle exit; repeated requests
/// neither replace the saved facts nor disturb any surface beneath Quit.
pub fn request_quit(state: &mut TuiState, facts: QuitFacts) -> QuitRequest {
    if is_open(state, Surface::QuitConfirm) {
        debug_assert!(super::consistent(state));
        return QuitRequest::Confirming;
    }
    if facts.is_idle() {
        debug_assert!(super::consistent(state));
        return QuitRequest::ExitNow;
    }
    state.quit_confirm = Some(QuitConfirmState {
        facts,
        caller_focus: state.focus.clone(),
    });
    open(state, Surface::QuitConfirm);
    debug_assert!(super::consistent(state));
    QuitRequest::Confirming
}

/// Back removes exactly the top frame, never an interrupted frame beneath it.
pub fn back(state: &mut TuiState) -> Option<Surface> {
    let Some(surface) = state.modal_stack.top().map(|frame| frame.surface) else {
        debug_assert!(super::consistent(state));
        return None;
    };
    close(state, surface);
    debug_assert!(super::consistent(state));
    Some(surface)
}

/// Remove one surface and its payload. Closing a covered surface does not
/// change the focus of the still-open top frame.
pub fn close(state: &mut TuiState, surface: Surface) {
    let Some(index) = state
        .modal_stack
        .frames
        .iter()
        .position(|frame| frame.surface == surface)
    else {
        debug_assert!(super::consistent(state));
        return;
    };
    let was_top = index + 1 == state.modal_stack.len();
    state.modal_stack.frames.remove(index);
    let caller_focus = match surface {
        Surface::NotificationLog => {
            let caller = state.notification_log.take().map(|log| log.caller_focus);
            state.notifications.log_closed();
            caller
        }
        Surface::QuitConfirm => state.quit_confirm.take().map(|quit| quit.caller_focus),
    };
    if was_top && let Some(focus) = caller_focus {
        state.focus = focus;
    }
    state.needs_redraw = true;
    debug_assert!(super::consistent(state));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tui::keymap::{Host, Key};
    use crate::adapters::tui::notification_log::{self, LogFacts, LogOutcome};
    use crate::adapters::tui::notifications::{self, NoticeSeverity, NoticeSource};
    use crate::adapters::tui::presentation::consistent;
    use crate::adapters::tui::quit_confirm::{self, QuitHost, QuitKeyOutcome};
    use crate::domain::models::{FocusState, OverlayType};

    #[test]
    fn story21_4_front_doors_preserve_identity_payload_and_legacy_base() {
        for (host, quit_host) in [
            (Host::Local, QuitHost::Local),
            (Host::Attached, QuitHost::Attached),
        ] {
            let mut state = TuiState::new(80, 24);
            state.focus = FocusState::Overlay(OverlayType::Help);
            state.help_overlay.active = true;
            state.help_overlay.scroll_offset = 7;
            state.input_buffer = "keep draft".into();
            state.cursor_position = 3;
            let base = state.focus.clone();
            notifications::notify(
                &mut state,
                NoticeSource::System,
                NoticeSeverity::Info,
                "older record",
            );
            notifications::notify(
                &mut state,
                NoticeSource::System,
                NoticeSeverity::Warning,
                "newest record",
            );
            let newest = state.notifications.records().next_back().unwrap().id;
            notification_log::open(&mut state);
            let log_frame = state.modal_stack.top().unwrap().clone();
            assert_eq!(log_frame.surface, Surface::NotificationLog);
            assert_eq!(log_frame.conversation_id, None);
            assert_eq!(
                state.notification_log.as_ref().unwrap().selected_id,
                Some(newest)
            );
            let facts = LogFacts::default();
            for key in [Key::Char('j'), Key::Enter, Key::Char('j')] {
                assert_eq!(
                    notification_log::handle_key(&mut state, host, key, &facts),
                    LogOutcome::Consumed
                );
            }
            let log = state.notification_log.clone();
            assert!(log.as_ref().unwrap().detail);
            assert_eq!(log.as_ref().unwrap().detail_scroll, 1);
            state.needs_redraw = false;
            notification_log::open(&mut state);
            assert!(!state.needs_redraw, "duplicate open is a no-op");
            assert_eq!(state.modal_stack.len(), 1);
            assert_eq!(state.modal_stack.top(), Some(&log_frame));
            assert_eq!(state.notification_log, log);
            assert_eq!(state.focus, base);
            assert!(consistent(&state));

            // A18: Ctrl+Q asks the host to guard quit without closing Log.
            assert_eq!(
                notification_log::handle_key(&mut state, host, Key::Ctrl('q'), &facts),
                LogOutcome::Quit
            );
            assert_eq!(state.notification_log, log);
            let busy = QuitFacts {
                streams: 1,
                ..QuitFacts::idle(quit_host)
            };
            assert_eq!(
                quit_confirm::request(&mut state, busy),
                QuitRequest::Confirming
            );
            let quit_frame = state.modal_stack.top().unwrap().clone();
            let quit = state.quit_confirm.clone();
            assert_eq!(quit_frame.surface, Surface::QuitConfirm);
            assert!(quit_frame.id > log_frame.id);
            state.needs_redraw = false;
            assert_eq!(
                quit_confirm::request(&mut state, QuitFacts::idle(quit_host)),
                QuitRequest::Confirming
            );
            assert!(!state.needs_redraw);
            assert_eq!(state.modal_stack.len(), 2);
            assert_eq!(state.modal_stack.top(), Some(&quit_frame));
            assert_eq!(state.quit_confirm, quit);
            assert_eq!(state.notification_log, log);
            assert_eq!(state.focus, base);
            assert!(consistent(&state));

            assert_eq!(
                quit_confirm::execute(&mut state, host, Key::Esc),
                QuitKeyOutcome::Cancelled
            );
            assert_eq!(state.modal_stack.top(), Some(&log_frame));
            assert_eq!(state.notification_log, log);
            assert!(state.quit_confirm.is_none());
            assert!(consistent(&state));
            assert_eq!(
                notification_log::handle_key(&mut state, host, Key::Esc, &facts),
                LogOutcome::Closed
            );
            assert_eq!(back(&mut state), None);
            assert!(state.modal_stack.is_empty());
            assert!(state.notification_log.is_none());
            assert!(consistent(&state));
            assert_eq!(state.focus, base);
            assert!(state.help_overlay.active);
            assert_eq!(state.help_overlay.scroll_offset, 7);
            assert_eq!(state.input_buffer, "keep draft");
            assert_eq!(state.cursor_position, 3);

            notification_log::open(&mut state);
            let reopened = state.modal_stack.top().unwrap();
            assert!(reopened.id > quit_frame.id, "ids never recycle after back");
            assert_eq!(reopened.surface, Surface::NotificationLog);
            assert!(consistent(&state));
        }
    }

    #[test]
    fn story21_4_idle_and_confirm_remove_no_underlying_frame() {
        let mut state = TuiState::new(80, 24);
        notification_log::open(&mut state);
        let frame = state.modal_stack.top().unwrap().clone();
        assert_eq!(
            quit_confirm::request(&mut state, QuitFacts::idle(QuitHost::Local)),
            QuitRequest::ExitNow
        );
        assert_eq!(state.modal_stack.top(), Some(&frame));
        assert!(state.quit_confirm.is_none());
        assert!(consistent(&state));
        quit_confirm::request(
            &mut state,
            QuitFacts {
                composer_draft: true,
                ..QuitFacts::idle(QuitHost::Local)
            },
        );
        assert_eq!(
            quit_confirm::execute(&mut state, Host::Local, Key::Char('y')),
            QuitKeyOutcome::Confirmed
        );
        assert_eq!(state.modal_stack.len(), 1);
        assert_eq!(state.modal_stack.top(), Some(&frame));
        assert!(state.notification_log.is_some());
        assert!(consistent(&state));
        assert_eq!(
            quit_confirm::execute(&mut state, Host::Local, Key::Char('y')),
            QuitKeyOutcome::Consumed
        );
        assert_eq!(state.modal_stack.top(), Some(&frame));
    }
}
