//! Story 21.3 conformance leg (k4_permission): the KG-2 truth sweep of the
//! Permission context (AC5, K4, local host): focus `Confirmation(Permission)`
//! with a pending tool call, Review closed and Review open. Every key goes
//! through the real front door (`support::press`). The attached twin of each row
//! (`AttachedApprovals::key`, consent / consolidation / retract arms) lives next
//! to `handle_attached_key` / `attach_loop`.
//!
//! `Esc` and `Ctrl+C` never deny the card (3-C, A8): with Review closed they post one cue,
//! with Review open they go back.

use std::sync::Arc;

use rustain::adapters::tui::app::InputAction;
use rustain::adapters::tui::keymap::resolve::rows_of;
use rustain::adapters::tui::keymap::table_permission::PermissionCmd;
use rustain::adapters::tui::keymap::{
    CommandId, Context, Host, Hosts, Key, Resolved, resolve, table,
};
use rustain::adapters::tui::presentation::{Surface, is_open};
use rustain::adapters::tui::state::{PendingPermission, TuiState};
use rustain::domain::models::FocusState;
use rustain::domain::models::ToolRisk;
use rustain::domain::models::tool_call::{ApprovalSource, RequestId};
use rustain::domain::models::visual::{ConfirmationType, OverlayType};

use super::support::{fresh, is_inert, press, press_all};

fn card_focus() -> FocusState {
    FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::Permission))
}

/// A fresh state with one pending tool-call decision owning the keyboard.
fn card() -> TuiState {
    let mut state = fresh();
    state.pending_permission = Some(PendingPermission {
        id: RequestId::new(),
        source: ApprovalSource::ForegroundTurn {
            conversation_id: "c1".into(),
        },
        tool_name: "Bash".to_string(),
        input: Arc::new(serde_json::json!({"command": "ls"})),
        risk: ToolRisk::Elevated,
    });
    state.focus = card_focus();
    state
}

/// The same card with the Review reader open (`r` pressed through the front door).
fn card_in_review() -> TuiState {
    let mut state = card();
    assert_eq!(press(&mut state, Key::Char('r')), InputAction::Consumed);
    assert!(state.permission_reader.review.is_some(), "Review opened");
    state
}

fn pending_id(state: &TuiState) -> RequestId {
    state
        .pending_permission
        .as_ref()
        .expect("the card is pending")
        .id
        .clone()
}

/// The state a "no command fires" sweep must see unchanged.
#[derive(Debug, Clone, PartialEq)]
struct Salient {
    focus: FocusState,
    pending: Option<RequestId>,
    review: Option<RequestId>,
    reader_scroll: usize,
    queued: usize,
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
        pending: state.pending_permission.as_ref().map(|p| p.id.clone()),
        review: state.permission_reader.review.clone(),
        reader_scroll: state.permission_reader.scroll,
        queued: state.permission_queue.len(),
        buffer: state.input_buffer.clone(),
        cursor: state.cursor_position,
        multiline: state.multiline_mode,
        palette: is_open(state, Surface::CommandPalette),
        which_key: state.pending_leader.is_some(),
        quit_confirm: state.quit_confirm.is_some(),
        history_len: state.input_history.len(),
    }
}

/// The effect of `keys` in a card state that makes the command observable, by
/// VALUE. Exhaustive on purpose: a new `PermissionCmd` does not compile until it
/// has a witness.
fn witness(cmd: PermissionCmd, keys: &[Key]) {
    match cmd {
        PermissionCmd::AllowOnce => {
            let mut state = card();
            assert_eq!(
                press_all(&mut state, keys),
                vec![InputAction::PermissionAllow]
            );
        }
        PermissionCmd::AllowSession => {
            let mut state = card();
            assert_eq!(
                press_all(&mut state, keys),
                vec![InputAction::PermissionSessionAllow]
            );
        }
        PermissionCmd::AllowAlways => {
            let mut state = card();
            assert_eq!(
                press_all(&mut state, keys),
                vec![InputAction::PermissionAlwaysAllow]
            );
        }
        PermissionCmd::Deny => {
            let mut state = card();
            assert_eq!(
                press_all(&mut state, keys),
                vec![InputAction::PermissionDeny]
            );
        }
        PermissionCmd::DenyWithFeedback => {
            let mut state = card();
            assert_eq!(
                press_all(&mut state, keys),
                vec![InputAction::PermissionDenyFeedback]
            );
        }
        PermissionCmd::Review => {
            let mut state = card();
            let id = pending_id(&state);
            assert_eq!(press_all(&mut state, keys), vec![InputAction::Consumed]);
            assert_eq!(
                state.permission_reader.review,
                Some(id),
                "Review pins the request"
            );
            assert_eq!(state.permission_reader.scroll, 0);
            assert_eq!(state.focus, card_focus(), "Review stays inside the card");
        }
        PermissionCmd::BackOrCue => {
            // 3-C (A8): with Review closed the key decides NOTHING: one inert answer, one cue,
            // the card stays pending (K11 asserts the cue text and every other card).
            let mut state = card();
            let id = pending_id(&state);
            assert_eq!(press_all(&mut state, keys), vec![InputAction::Consumed]);
            assert_eq!(pending_id(&state), id, "the key never decides the card");
            assert_eq!(state.notifications.records().count(), 1, "one cue");
            // With Review open the same key only goes back, the card stays pending.
            let mut state = card_in_review();
            let id = pending_id(&state);
            assert_eq!(press_all(&mut state, keys), vec![InputAction::Consumed]);
            assert_eq!(state.permission_reader.review, None, "Review closed");
            assert_eq!(pending_id(&state), id, "the card is still pending");
            assert_eq!(state.focus, card_focus());
        }
        PermissionCmd::ReviewPageDown => {
            let mut state = card_in_review();
            state.permission_reader.scroll_max = 100;
            state.permission_reader.viewport_rows = 11;
            assert_eq!(press_all(&mut state, keys), vec![InputAction::Consumed]);
            assert_eq!(
                state.permission_reader.scroll, 10,
                "one page is viewport - 1"
            );
            assert_eq!(press_all(&mut state, keys), vec![InputAction::Consumed]);
            assert_eq!(state.permission_reader.scroll, 20);
        }
        PermissionCmd::ReviewPageUp => {
            let mut state = card_in_review();
            state.permission_reader.scroll_max = 100;
            state.permission_reader.viewport_rows = 11;
            state.permission_reader.scroll = 25;
            assert_eq!(press_all(&mut state, keys), vec![InputAction::Consumed]);
            assert_eq!(
                state.permission_reader.scroll, 15,
                "one page is viewport - 1"
            );
            assert_eq!(press_all(&mut state, keys), vec![InputAction::Consumed]);
            assert_eq!(state.permission_reader.scroll, 5);
        }
        PermissionCmd::ScrollLineDown => {
            let mut state = card();
            assert_eq!(
                press_all(&mut state, keys),
                vec![InputAction::ScrollLineDown]
            );
        }
        PermissionCmd::ScrollLineUp => {
            let mut state = card();
            assert_eq!(press_all(&mut state, keys), vec![InputAction::ScrollLineUp]);
        }
        PermissionCmd::ScrollToBottom => {
            let mut state = card();
            assert_eq!(
                press_all(&mut state, keys),
                vec![InputAction::ScrollToBottom]
            );
        }
        PermissionCmd::ScrollToTop => {
            let mut state = card();
            assert_eq!(press_all(&mut state, keys), vec![InputAction::ScrollToTop]);
        }
        PermissionCmd::QuitRequest => panic!(
            "{cmd:?} is an attached-only command: the local card swallows Ctrl+Q today; \
             its witness lives next to `handle_attached_key` (story21_3_attached_*)"
        ),
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
    assert_eq!(
        witnessed, 14,
        "y s a n f r Esc Ctrl+C PgUp PgDn j k G g are the local card keys"
    );
}

/// The rows of one host only are exactly the ones this leg knows about.
#[test]
fn the_only_attached_permission_row_is_ctrl_q() {
    let attached_only: Vec<_> = table()
        .iter()
        .filter(|row| row.context == Context::Permission && row.hosts == Hosts::Attached)
        .map(|row| (row.keys.to_vec(), row.command))
        .collect();
    assert_eq!(
        attached_only,
        vec![(
            vec![Key::Ctrl('q')],
            CommandId::Permission(PermissionCmd::QuitRequest)
        )]
    );
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
