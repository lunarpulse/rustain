//! K4 extension, AC20: the four 3-A review items through the local front door
//! (attached legs: `cargo test --lib story21_3_attached_` in `attach_keys.rs`).

use rustain::adapters::tui::app::InputAction;
use rustain::adapters::tui::keymap::Key;
use rustain::adapters::tui::state::TuiState;
use rustain::domain::models::FocusState;

use super::support::{fresh, is_inert, press};

fn messages(state: &TuiState) -> Vec<String> {
    state
        .notifications
        .records()
        .map(|r| r.message.clone())
        .collect()
}

/// DF-21-3-LEADER-CTRLQ-HOST-ASYMMETRY: the local host swallowed Ctrl+Q as an
/// unknown follow-up (a toast, no quit) while the attached host quit.
#[test]
fn ctrl_q_with_the_leader_open_closes_it_and_requests_the_guarded_quit() {
    let mut state = fresh();
    state.input_buffer = "draft".into();
    state.cursor_position = 5;
    press(&mut state, Key::Ctrl('x'));
    assert!(state.which_key.active);
    assert_eq!(press(&mut state, Key::Ctrl('q')), InputAction::Quit);
    assert!(!state.which_key.active, "the leader is closed");
    assert_eq!(state.focus, FocusState::Input, "the origin is restored");
    assert!(messages(&state).is_empty(), "never a no-binding toast");
    assert_eq!(state.input_buffer, "draft");
    // Positive control: an ordinary follow-up still runs its row.
    press(&mut state, Key::Ctrl('x'));
    assert_eq!(press(&mut state, Key::Char('?')), InputAction::Consumed);
    assert!(state.help_overlay.active);
}

/// DF-21-3-UNBOUND-KEY-ALIASING: a decoded key with an extra modifier resolved to
/// the plain Ctrl/Alt key (`Key::Ctrl('c')`), so a resolver-driven dispatch would
/// have interrupted on Ctrl+Shift+C and opened the leader on Ctrl+Shift+X.
#[test]
fn extra_modifier_keys_never_act_as_the_plain_key() {
    let (ctrl_shift, alt_shift) = (5, 6);
    let mut state = fresh();
    assert!(is_inert(&press(&mut state, Key::Combo('c', ctrl_shift))));
    assert!(is_inert(&press(&mut state, Key::Combo('q', ctrl_shift))));
    assert!(is_inert(&press(&mut state, Key::Combo('x', ctrl_shift))));
    assert!(!state.which_key.active, "Ctrl+Shift+X opens no leader");
    assert!(is_inert(&press(&mut state, Key::Combo('m', alt_shift))));
    assert!(!state.multiline_mode, "Alt+Shift+M toggles nothing");
    // Positive controls: the plain keys still act.
    assert_eq!(press(&mut state, Key::Ctrl('c')), InputAction::CancelOrQuit);
    assert_eq!(press(&mut state, Key::Ctrl('q')), InputAction::Quit);
    press(&mut state, Key::Alt('m'));
    assert!(state.multiline_mode, "Alt+M toggles multi-line");
    press(&mut state, Key::Ctrl('x'));
    assert!(state.which_key.active, "Ctrl+X opens the leader");
    // In the open leader the combined key is a named unknown follow-up, not Ctrl+C
    // (which would cancel silently).
    assert_eq!(
        press(&mut state, Key::Combo('c', ctrl_shift)),
        InputAction::Consumed
    );
    assert_eq!(
        messages(&state),
        vec!["No binding for Ctrl+X Ctrl+Shift+C in Input".to_string()]
    );
}
