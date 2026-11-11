//! Story 21.3 conformance leg K4 (Chat): the Chat rows of the keymap table
//! against the real local dispatch, through `support::press` only.
//!
//! * `every_row_resolves_to_its_command_by_value` drives every local Chat row's
//!   keys with Chat focus current and asserts the VALUE of the effect (scroll
//!   offset, returned payload, focus, pending flags). The `witness` match is
//!   exhaustive over `ChatCmd`, so a new variant forces a new witness.
//! * the negative sweeps press every `Key::universe()` key that resolves to no
//!   row (and every prefix followed by every non-completing key) and assert no
//!   command fires and nothing salient moves.
//!
//! Chat arms that exist only behind a pending card (plan `y e n`, delegation,
//! consolidation, forget, spawn gate, team proposal, recipient surface) are
//! Permission/3-C territory and are not exercised here.

use rustain::adapters::tui::app::InputAction;
use rustain::adapters::tui::keymap::table_chat::ChatCmd;
use rustain::adapters::tui::keymap::{self, Binding, CommandId, Context, Host, Key, Resolved};
use rustain::adapters::tui::presentation::{Surface, is_open};
use rustain::adapters::tui::state::{Direction, TuiState};
use rustain::domain::models::FocusState;

use super::support::{assert_global_effect, decode, fresh, global_owned, is_inert, press};

// ── fixtures ──────────────────────────────────────────────────────────────

fn chat_state() -> TuiState {
    let mut state = fresh();
    state.focus = FocusState::Chat;
    state
}

/// Chat with a rendered 100-line transcript in a 20-line viewport, scrolled to
/// offset-from-bottom 40 (viewport top at line 40). Block boundaries and user
/// message boundaries differ so each jump command has its own exact answer.
fn boundary_state() -> TuiState {
    let mut state = chat_state();
    state.total_content_height = 100;
    state.viewport_height = 20;
    state.block_boundaries = vec![0, 10, 30, 60, 90];
    state.user_message_boundaries = vec![0, 20, 50];
    state.set_scroll_offset(40);
    state
}

/// Chat while a plan is executing (what the retired drill-down hung off): the keys stay Chat's.
fn plan_state() -> TuiState {
    let mut state = chat_state();
    state.task_panel_state.last_executed_plan_id = Some("plan-1".to_owned());
    state
}

/// The state a "no command fires" sweep must find unchanged.
#[derive(Debug, PartialEq)]
struct Salient {
    focus: FocusState,
    input: String,
    cursor: usize,
    plan: Option<String>,
    reorder: Option<rustain::adapters::tui::state::Reorder>,
    scroll: usize,
    wave_selected: usize,
    diverge_open: bool,
    selected_tool: Option<String>,
}

fn salient(state: &TuiState) -> Salient {
    Salient {
        focus: state.focus.clone(),
        input: state.input_buffer.clone(),
        cursor: state.cursor_position,
        plan: state.task_panel_state.last_executed_plan_id.clone(),
        reorder: state.task_panel_state.reorder.clone(),
        scroll: state.scroll_offset(),
        wave_selected: state.wave_overlay_selected,
        diverge_open: state.wave_diverge_open,
        selected_tool: state.selected_item_id.clone(),
    }
}

fn no_chords(state: &TuiState) -> bool {
    !state.pending_z && state.pending_bracket.is_none() && !state.pending_g
}

fn one(state: &mut TuiState, key: Key) -> InputAction {
    press(state, key)
}

// ── positive: every row, by value ─────────────────────────────────────────

/// The expectation of one row's keys, by value. Exhaustive over `ChatCmd`.
fn witness(cmd: ChatCmd, keys: &[Key]) {
    match cmd {
        ChatCmd::FocusToggle => {
            let mut state = chat_state();
            assert_eq!(one(&mut state, Key::Esc), InputAction::Consumed);
            assert_eq!(state.focus, FocusState::Input);
            assert_eq!(one(&mut state, Key::Esc), InputAction::Consumed);
            assert_eq!(state.focus, FocusState::Chat);
        }
        ChatCmd::FocusInput => {
            let mut state = chat_state();
            assert_eq!(one(&mut state, Key::Char('i')), InputAction::Consumed);
            assert_eq!(state.focus, FocusState::Input);
            assert_eq!(state.input_buffer, "", "`i` is a command, never text");
        }
        ChatCmd::Quit => {
            let mut state = chat_state();
            assert_eq!(one(&mut state, Key::Char('q')), InputAction::Quit);
        }
        ChatCmd::OpenHelp => {
            let mut state = chat_state();
            assert_eq!(one(&mut state, Key::Char('?')), InputAction::Consumed);
            assert_eq!(state.focus, FocusState::Chat);
            assert!(is_open(&state, Surface::Help));
            assert_eq!(state.modal_stack.top().unwrap().surface, Surface::Help);
        }
        ChatCmd::OpenPalette => {
            let mut state = chat_state();
            assert_eq!(one(&mut state, Key::Ctrl('p')), InputAction::Consumed);
            assert_eq!(state.focus, FocusState::Chat);
            assert!(is_open(&state, Surface::CommandPalette));
        }
        ChatCmd::CancelOrQuit => {
            let mut state = chat_state();
            assert_eq!(one(&mut state, Key::Ctrl('c')), InputAction::CancelOrQuit);
        }
        ChatCmd::ScrollHalfPageDown => {
            let mut state = chat_state();
            assert_eq!(
                one(&mut state, Key::Ctrl('d')),
                InputAction::ScrollHalfPageDown
            );
        }
        ChatCmd::ScrollHalfPageUp => {
            let mut state = chat_state();
            assert_eq!(
                one(&mut state, Key::Ctrl('u')),
                InputAction::ScrollHalfPageUp
            );
        }
        ChatCmd::ScrollPageDown => {
            // Local route is Ctrl+F (PgDn is the attached host's route).
            assert_eq!(keys, &[Key::Ctrl('f')]);
            let mut state = chat_state();
            assert_eq!(
                one(&mut state, Key::Ctrl('f')),
                InputAction::ScrollFullPageDown
            );
        }
        ChatCmd::ScrollPageUp => {
            assert_eq!(keys, &[Key::Ctrl('b')]);
            let mut state = chat_state();
            assert_eq!(
                one(&mut state, Key::Ctrl('b')),
                InputAction::ScrollFullPageUp
            );
        }
        ChatCmd::ScrollLineDown => {
            let mut state = chat_state();
            assert_eq!(one(&mut state, Key::Char('j')), InputAction::ScrollLineDown);
            // A plan in flight no longer captures the key (the drill-down is gone, 21-7 7-D):
            // the task result scrolls in the task window, witnessed in k4_panels.
            let mut running = plan_state();
            assert_eq!(
                one(&mut running, Key::Char('j')),
                InputAction::ScrollLineDown
            );
        }
        ChatCmd::ScrollLineUp => {
            let mut state = chat_state();
            assert_eq!(one(&mut state, Key::Char('k')), InputAction::ScrollLineUp);
            let mut running = plan_state();
            assert_eq!(one(&mut running, Key::Char('k')), InputAction::ScrollLineUp);
        }
        ChatCmd::ScrollToTop => match keys {
            [Key::Char('g')] => {
                let mut state = chat_state();
                assert_eq!(one(&mut state, Key::Char('g')), InputAction::ScrollToTop);
                assert!(state.pending_g, "a first `g` arms the `gg` chord");
                assert_eq!(one(&mut state, Key::Char('g')), InputAction::Consumed);
                assert!(!state.pending_g, "the second `g` completes and clears it");
                let mut running = plan_state();
                assert_eq!(one(&mut running, Key::Char('g')), InputAction::ScrollToTop);
            }
            [Key::Char('z'), Key::Char('g')] => {
                let mut state = chat_state();
                let first = one(&mut state, Key::Char('z'));
                assert_eq!(first, InputAction::Consumed);
                assert!(state.pending_z);
                assert_eq!(one(&mut state, Key::Char('g')), InputAction::ScrollToTop);
                assert!(!state.pending_z);
            }
            other => panic!("unexpected ScrollToTop row keys {other:?}"),
        },
        ChatCmd::ScrollToBottom => {
            let mut state = chat_state();
            assert_eq!(one(&mut state, Key::Char('G')), InputAction::ScrollToBottom);
            let mut running = plan_state();
            assert_eq!(
                one(&mut running, Key::Char('G')),
                InputAction::ScrollToBottom
            );
        }
        ChatCmd::BlockJumpNext => {
            // Viewport top at line 40: the next block boundary below it is 60,
            // i.e. offset-from-bottom 80 - 60 = 20 (K would land on 30 -> 50).
            let mut state = boundary_state();
            assert_eq!(
                one(&mut state, Key::Char('J')),
                InputAction::BlockJump {
                    offset: 20,
                    auto_scroll: false
                }
            );
            // Past the last boundary the jump lands on the bottom and re-follows.
            let mut bottom = boundary_state();
            bottom.set_scroll_offset(5);
            assert_eq!(
                one(&mut bottom, Key::Char('J')),
                InputAction::BlockJump {
                    offset: 0,
                    auto_scroll: true
                }
            );
        }
        ChatCmd::BlockJumpPrev => {
            let mut state = boundary_state();
            assert_eq!(
                one(&mut state, Key::Char('K')),
                InputAction::BlockJump {
                    offset: 50,
                    auto_scroll: false
                }
            );
        }
        ChatCmd::UserMessagePrev => {
            // User-message boundaries [0, 20, 50]: previous before line 40 is 20.
            let mut state = boundary_state();
            assert_eq!(
                one(&mut state, Key::Char('{')),
                InputAction::BlockJump {
                    offset: 60,
                    auto_scroll: false
                }
            );
        }
        ChatCmd::UserMessageNext => {
            let mut state = boundary_state();
            assert_eq!(
                one(&mut state, Key::Char('}')),
                InputAction::BlockJump {
                    offset: 30,
                    auto_scroll: false
                }
            );
        }
        ChatCmd::CycleInvocation => {
            let mut state = chat_state();
            assert_eq!(
                one(&mut state, Key::Tab),
                InputAction::CycleInvocationInFocusedTurn
            );
        }
        ChatCmd::PrevTab => {
            let mut state = chat_state();
            assert_eq!(one(&mut state, Key::ShiftTab), InputAction::SwitchToPrevTab);
        }
        ChatCmd::NewTab => {
            let mut state = chat_state();
            assert_eq!(one(&mut state, Key::Ctrl('t')), InputAction::NewTab);
        }
        ChatCmd::SwitchToTab(n) => {
            let digit = Key::Char(char::from(b'0' + n));
            assert_eq!(keys, &[digit]);
            let mut state = chat_state();
            assert_eq!(
                one(&mut state, digit),
                InputAction::SwitchToTab(usize::from(n))
            );
        }
        ChatCmd::PasteClipboard => {
            let mut state = chat_state();
            assert_eq!(
                one(&mut state, Key::Alt('v')),
                InputAction::RequestClipboardPaste
            );
        }
        ChatCmd::Inspect => {
            let mut state = chat_state();
            state.focused_item_id = Some("tool-1".to_owned());
            state.selected_item_id = Some("previous-tool".to_owned());
            assert_eq!(one(&mut state, Key::Enter), InputAction::InspectTool);
            assert_eq!(
                state.selected_item_id.as_deref(),
                Some("previous-tool"),
                "the runtime resolves the actual target before changing selection"
            );
            assert!(
                state.modal_stack.is_empty(),
                "the pure mapper has no conversation"
            );
        }
        ChatCmd::Copy => {
            let mut state = chat_state();
            assert_eq!(
                one(&mut state, Key::Char('c')),
                InputAction::CopyToClipboard(String::new())
            );
            // A plan in flight no longer turns `c` into a task-result copy: the result is
            // copied from the task window (`Context::TaskWindow` `c`, k4_panels).
            let mut running = plan_state();
            assert_eq!(
                one(&mut running, Key::Char('c')),
                InputAction::CopyToClipboard(String::new())
            );
        }
        ChatCmd::ForkAtMessage => {
            let mut state = chat_state();
            assert_eq!(one(&mut state, Key::Char('f')), InputAction::ForkAtMessage);
        }
        ChatCmd::RewindAtMessage => {
            let mut state = chat_state();
            assert_eq!(
                one(&mut state, Key::Char('R')),
                InputAction::RewindAtMessage
            );
        }
        ChatCmd::ToggleBookmark => {
            let mut state = chat_state();
            assert_eq!(one(&mut state, Key::Char('m')), InputAction::ToggleBookmark);
        }
        ChatCmd::OpenBookmarkList => {
            let mut state = chat_state();
            assert_eq!(
                one(&mut state, Key::Char('\'')),
                InputAction::OpenBookmarkList
            );
        }
        ChatCmd::FoldToggle => sequence(keys, b'z', InputAction::FoldToggleAtFocus),
        ChatCmd::FoldCollapse => sequence(keys, b'z', InputAction::CollapseFocus),
        ChatCmd::FoldExpand => sequence(keys, b'z', InputAction::ExpandFocus),
        ChatCmd::CollapseAllTurns => sequence(keys, b'z', InputAction::CollapseAllTurns),
        ChatCmd::ExpandAllTurns => sequence(keys, b'z', InputAction::ExpandAllTurns),
        ChatCmd::ToggleSummaryTier => sequence(keys, b'z', InputAction::ToggleSummaryTier),
        ChatCmd::RecenterAnchor => sequence(keys, b'z', InputAction::RecenterAnchor),
        ChatCmd::NextProseAnchor => {
            sequence(keys, b']', InputAction::JumpProseAnchor(Direction::Down))
        }
        ChatCmd::PrevProseAnchor => {
            sequence(keys, b'[', InputAction::JumpProseAnchor(Direction::Up))
        }
        ChatCmd::LatestProseAnchor => sequence(keys, b']', InputAction::JumpToLatestProseAnchor),
    }
}

/// `prefix` then the second key: the prefix is consumed and pends, the second
/// key completes with `expected` and clears the pending flag.
fn sequence(keys: &[Key], prefix: u8, expected: InputAction) {
    assert_eq!(keys.len(), 2, "a sequence row has two keys");
    assert_eq!(keys[0], Key::Char(char::from(prefix)));
    let mut state = chat_state();
    assert_eq!(one(&mut state, keys[0]), InputAction::Consumed);
    match prefix {
        b'z' => assert!(state.pending_z, "`z` pends"),
        b']' | b'[' => assert_eq!(state.pending_bracket, Some(char::from(prefix))),
        other => panic!("unknown prefix {other}"),
    }
    assert_eq!(one(&mut state, keys[1]), expected);
    assert!(
        no_chords(&state),
        "the completed sequence leaves nothing pending"
    );
}

#[test]
fn every_row_resolves_to_its_command_by_value() {
    let rows: Vec<&'static Binding> =
        keymap::resolve::rows_of(Context::Chat, Host::Local).collect();
    assert!(
        rows.len() > 45,
        "the Chat table is complete, got {}",
        rows.len()
    );
    for row in rows {
        let CommandId::Chat(cmd) = row.command else {
            panic!("Chat row {row:?} carries a non-Chat command");
        };
        // The row is what its own keys resolve to (no shadowing by another row).
        match keymap::resolve(Context::Chat, Host::Local, row.keys) {
            Resolved::Hit(hit) => {
                assert_eq!(
                    hit.command, row.command,
                    "keys {:?} resolve elsewhere",
                    row.keys
                );
                assert_eq!(hit.keys, row.keys);
            }
            other => panic!("row {row:?} does not resolve to itself: {other:?}"),
        }
        witness(cmd, row.keys);
    }
}

#[test]
fn every_chat_command_has_a_local_witness_row() {
    // PgUp/PgDn are the attached host's pagers; their command is also routed
    // locally by Ctrl+B/Ctrl+F, so every command still has a local witness row.
    let local: Vec<ChatCmd> = keymap::resolve::rows_of(Context::Chat, Host::Local)
        .map(|row| match row.command {
            CommandId::Chat(cmd) => cmd,
            other => panic!("non-Chat command {other:?}"),
        })
        .collect();
    for row in keymap::table()
        .iter()
        .filter(|r| r.context == Context::Chat)
    {
        let CommandId::Chat(cmd) = row.command else {
            unreachable!()
        };
        assert!(
            local.contains(&cmd),
            "{cmd:?} has no local row (keys {:?})",
            row.keys
        );
    }
}

#[test]
fn attached_only_rows_do_not_act_locally() {
    for key in [Key::PageUp, Key::PageDown] {
        assert!(matches!(
            keymap::resolve(Context::Chat, Host::Attached, &[key]),
            Resolved::Hit(_)
        ));
        assert!(matches!(
            keymap::resolve(Context::Chat, Host::Local, &[key]),
            Resolved::Miss
        ));
        let mut state = chat_state();
        let before = salient(&state);
        assert!(
            is_inert(&press(&mut state, key)),
            "{key:?} is inert locally"
        );
        assert_eq!(salient(&state), before);
    }
}

// ── negative: nothing without a row acts ──────────────────────────────────

#[test]
fn keys_without_a_chat_row_do_nothing() {
    let mut swept = 0;
    for key in Key::universe() {
        if global_owned(Context::Chat, key) {
            assert_global_effect(&mut chat_state(), key);
            continue;
        }
        if !matches!(
            keymap::resolve(Context::Chat, Host::Local, &[key]),
            Resolved::Miss
        ) {
            continue; // a row (or a pending prefix, swept below) owns it
        }
        let mut state = chat_state();
        let before = salient(&state);
        let action = press(&mut state, key);
        assert!(
            is_inert(&action),
            "{key:?} without a Chat row returned {action:?}"
        );
        assert_eq!(salient(&state), before, "{key:?} moved Chat state");
        assert!(no_chords(&state), "{key:?} armed a chord");
        swept += 1;
    }
    assert!(
        swept > 100,
        "the sweep must cover the whole universe, swept {swept}"
    );

    // Positive control: a key WITH a row visibly changes state.
    let mut state = chat_state();
    let before = salient(&state);
    press(&mut state, Key::Char('i'));
    assert_ne!(salient(&state), before);
}

#[test]
fn ctrl_y_and_ctrl_j_have_no_row_and_never_act() {
    for key in [Key::Ctrl('y'), Key::Ctrl('j')] {
        assert!(matches!(
            keymap::resolve(Context::Chat, Host::Local, &[key]),
            Resolved::Miss
        ));
        // Ctrl+J used to fold to `j` (a scroll) and Ctrl+Y to `y` (plan approve).
        let mut state = boundary_state();
        let before = salient(&state);
        let action = press(&mut state, key);
        assert!(is_inert(&action), "{key:?} returned {action:?}");
        assert_eq!(salient(&state), before);
    }
}

/// Every first key of a Chat sequence row.
fn prefixes() -> Vec<Key> {
    let mut out: Vec<Key> = Vec::new();
    for row in keymap::resolve::rows_of(Context::Chat, Host::Local) {
        if row.keys.len() == 2 && !out.contains(&row.keys[0]) {
            out.push(row.keys[0]);
        }
    }
    out
}

#[test]
fn a_prefix_followed_by_any_non_completing_key_has_no_effect_and_clears_the_chord() {
    let prefixes = prefixes();
    for expected in ['z', '[', ']'] {
        assert!(
            prefixes.contains(&Key::Char(expected)),
            "{expected:?} is a sequence prefix"
        );
    }
    let mut swept = 0;
    for prefix in prefixes {
        for key in Key::universe() {
            if matches!(
                keymap::resolve(Context::Chat, Host::Local, &[prefix, key]),
                Resolved::Hit(_)
            ) {
                continue; // completes a sequence
            }
            let mut state = chat_state();
            assert!(is_inert(&press(&mut state, prefix)));
            let armed = state.pending_z || state.pending_bracket.is_some() || state.pending_g;
            assert!(armed, "{prefix:?} must arm a chord");
            let before = salient(&state);

            let action = press(&mut state, key);
            assert!(!state.pending_g, "{prefix:?} then {key:?} left `g` pending");

            let is_char = matches!(
                decode(key),
                Some(rustain::domain::events::DomainInputEvent::KeyPress(_))
            );
            if !is_char {
                // Any non-character event cancels the chord and is handled as if
                // fresh. So the outcome (action and the chord the key itself may
                // arm, e.g. `z`) equals pressing the key in a fresh Chat state.
                let mut reference = chat_state();
                let expected = press(&mut reference, key);
                assert_eq!(action, expected, "{prefix:?} then {key:?}");
                assert_eq!(
                    (state.pending_z, state.pending_bracket),
                    (reference.pending_z, reference.pending_bracket),
                    "{prefix:?} then {key:?} chord flags"
                );
            } else {
                // z / [ / ] discard a non-completing character: consumed, no
                // effect, nothing left pending.
                assert_eq!(action, InputAction::Consumed, "{prefix:?} then {key:?}");
                assert!(
                    no_chords(&state),
                    "{prefix:?} then {key:?} left a chord pending"
                );
                assert_eq!(
                    salient(&state),
                    before,
                    "{prefix:?} then {key:?} moved state"
                );
            }
            swept += 1;
        }
    }
    assert!(
        swept > 300,
        "three prefixes over the whole universe, swept {swept}"
    );
}
