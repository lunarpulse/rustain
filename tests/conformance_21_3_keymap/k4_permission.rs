//! Permission rows and isolation through the painted decision-frame front door.

use super::support::{is_inert, permission, permission_payload, press, press_all};
use rustain::adapters::tui::app::InputAction;
use rustain::adapters::tui::keymap::resolve::rows_of;
use rustain::adapters::tui::keymap::table_permission::PermissionCmd;
use rustain::adapters::tui::keymap::{CommandId, Context, Host, Key, Resolved, resolve};
use rustain::adapters::tui::presentation::{Surface, is_open};
use rustain::adapters::tui::state::TuiState;
use rustain::domain::models::FocusState;

fn card() -> TuiState {
    permission()
}

fn card_in_review() -> TuiState {
    let mut state = card();
    assert_eq!(press(&mut state, Key::Char('r')), InputAction::Consumed);
    assert!(permission_payload(&state).reviewing);
    state
}

#[derive(Debug, Clone, PartialEq)]
struct Salient {
    focus: FocusState,
    items: Vec<rustain::adapters::tui::decision_queue::DecisionItem>,
    frame: bool,
    buffer: String,
    cursor: usize,
    multiline: bool,
    palette: bool,
    which_key: bool,
    quit_confirm: bool,
    history_len: usize,
}

fn salient(state: &TuiState) -> Salient {
    Salient {
        focus: state.focus.clone(),
        items: state.decisions.items().cloned().collect(),
        frame: is_open(state, Surface::Decision),
        buffer: state.input_buffer.clone(),
        cursor: state.cursor_position,
        multiline: state.multiline_mode,
        palette: is_open(state, Surface::CommandPalette),
        which_key: state.pending_leader.is_some(),
        quit_confirm: state.quit_confirm.is_some(),
        history_len: state.input_history.len(),
    }
}

/// Exhaustive on purpose: every PermissionCmd has a behavioral witness.
fn witness(cmd: PermissionCmd, keys: &[Key]) {
    let mut state = card();
    let expected = match cmd {
        PermissionCmd::AllowOnce => InputAction::PermissionAllow,
        PermissionCmd::AllowSession => InputAction::PermissionSessionAllow,
        PermissionCmd::AllowAlways => InputAction::PermissionAlwaysAllow,
        PermissionCmd::Deny => InputAction::PermissionDeny,
        PermissionCmd::DenyWithFeedback | PermissionCmd::Review => InputAction::Consumed,
    };
    assert_eq!(press_all(&mut state, keys), vec![expected]);
    match cmd {
        PermissionCmd::DenyWithFeedback => assert!(permission_payload(&state).feedback.is_some()),
        PermissionCmd::Review => assert!(permission_payload(&state).reviewing),
        _ => {
            assert!(state.decisions.is_empty(), "the answered item is retired");
            assert!(!is_open(&state, Surface::Decision));
        }
    }
}

/// Table-driven positive leg over the local Permission rows.
#[test]
fn every_permission_row_resolves_to_its_command_by_value() {
    let mut witnessed = 0;
    for row in rows_of(Context::Permission, Host::Local) {
        let CommandId::Permission(cmd) = row.command else {
            panic!("Permission row {row:?} carries a command of another group");
        };
        match resolve(Context::Permission, Host::Local, row.keys) {
            Resolved::Hit(hit) => assert_eq!(
                hit.command, row.command,
                "{row:?} must resolve to its own command"
            ),
            other => panic!("{row:?} does not resolve: {other:?}"),
        }
        witness(cmd, row.keys);
        witnessed += 1;
    }
    assert_eq!(witnessed, 6, "y s a n f r are the permission rows");
}

/// Both hosts share the same six permission rows.
#[test]
fn attached_and_local_permission_rows_match() {
    let rows = |host| {
        rows_of(Context::Permission, host)
            .map(|row| (row.keys.to_vec(), row.command))
            .collect::<Vec<_>>()
    };
    assert_eq!(rows(Host::Local), rows(Host::Attached));
    assert_eq!(rows(Host::Attached).len(), 6);
}

#[test]
fn review_backs_out_before_the_permission_is_deferred() {
    for key in [Key::Esc, Key::Ctrl('c')] {
        let mut state = card_in_review();
        assert_eq!(press(&mut state, key), InputAction::Consumed);
        assert!(!permission_payload(&state).reviewing);
        assert!(is_open(&state, Surface::Decision));
        assert!(!state.decisions.items().next().unwrap().deferred);
        assert_eq!(press(&mut state, key), InputAction::Consumed);
        assert!(!is_open(&state, Surface::Decision));
        assert!(state.decisions.items().next().unwrap().deferred);
    }
}

/// Negative sweep over one starting state: every key with NO local Permission
/// row is swallowed (inert action, nothing typed, card and Review untouched).
fn sweep_without_rows(start: impl Fn() -> TuiState, label: &str) -> usize {
    let mut swept = 0;
    for key in Key::universe() {
        if !matches!(
            resolve(Context::Permission, Host::Local, &[key]),
            Resolved::Miss
        ) {
            continue;
        }
        let mut state = start();
        let before = salient(&state);
        let action = press(&mut state, key);
        assert!(
            is_inert(&action),
            "{label}: {key:?} has no Permission row but returned {action:?}"
        );
        assert_eq!(
            salient(&state),
            before,
            "{label}: {key:?} changed the card state"
        );
        swept += 1;
    }
    swept
}

#[test]
fn keys_without_a_permission_row_are_swallowed_with_review_closed() {
    let swept = sweep_without_rows(card, "review closed");
    assert!(swept > 130, "the sweep covered the universe ({swept} keys)");
    // Positive control: a key WITH a row visibly changes the compared state.
    let mut state = card();
    let before = salient(&state);
    press(&mut state, Key::Char('r'));
    assert_ne!(
        salient(&state),
        before,
        "r opens Review and the snapshot sees it"
    );
}

#[test]
fn keys_without_a_permission_row_are_swallowed_with_review_open() {
    let swept = sweep_without_rows(card_in_review, "review open");
    assert!(swept > 130, "the sweep covered the universe ({swept} keys)");
}
