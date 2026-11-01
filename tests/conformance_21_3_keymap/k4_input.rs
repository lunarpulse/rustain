//! Story 21.3 conformance leg (k4_input): the KG-2 truth sweep of the Input
//! context (AC5, K4, local host). Every key goes through the real front door
//! (`support::press`: crossterm `KeyEvent` -> `convert_crossterm_event` ->
//! `handle_input`) with `FocusState::Input` current in a real `TuiState`.
//! The attached twin of each row lives next to `handle_attached_key`.

use rustain::adapters::tui::app::InputAction;
use rustain::adapters::tui::keymap::resolve::rows_of;
use rustain::adapters::tui::keymap::table_input::InputCmd;
use rustain::adapters::tui::keymap::{CommandId, Context, Host, Key, Resolved, resolve};
use rustain::adapters::tui::state::TuiState;
use rustain::domain::models::FocusState;
use rustain::domain::models::autocomplete::AutocompleteKind;
use rustain::domain::models::visual::OverlayType;

use super::support::{assert_global_effect, fresh, global_owned, is_inert, press, press_all};

/// A fresh Input-focus state with `text` in the composer and the cursor at `cursor`.
fn composer(text: &str, cursor: usize) -> TuiState {
    let mut state = fresh();
    state.input_buffer = text.to_string();
    state.cursor_position = cursor;
    state
}

/// The state a "no command fires" sweep must see unchanged.
#[derive(Debug, Clone, PartialEq)]
struct Salient {
    focus: FocusState,
    buffer: String,
    cursor: usize,
    multiline: bool,
    autocomplete: bool,
    reverse_search: bool,
    palette: bool,
    which_key: bool,
    search: bool,
    quit_confirm: bool,
    pending_images: usize,
    history_len: usize,
    history_navigating: bool,
    scroll: usize,
}

fn salient(state: &TuiState) -> Salient {
    Salient {
        focus: state.focus.clone(),
        buffer: state.input_buffer.clone(),
        cursor: state.cursor_position,
        multiline: state.multiline_mode,
        autocomplete: state.autocomplete.active,
        reverse_search: state.reverse_search.active,
        palette: state.command_palette.active,
        which_key: state.which_key.active,
        search: state.search_state.active,
        quit_confirm: state.quit_confirm.is_some(),
        pending_images: state.pending_images.len(),
        history_len: state.input_history.len(),
        history_navigating: state.input_history.is_navigating(),
        scroll: state.input_scroll_offset,
    }
}

/// The effect of `keys` (the row's key sequence) in a state that makes the
/// command observable, asserted by VALUE. The match is exhaustive on purpose:
/// a new `InputCmd` variant does not compile until it has a witness.
fn witness(cmd: InputCmd, keys: &[Key]) {
    match cmd {
        InputCmd::ReturnToChat => {
            let mut state = composer("draft", 5);
            assert_eq!(press_all(&mut state, keys), vec![InputAction::Consumed]);
            assert_eq!(state.focus, FocusState::Chat);
            assert_eq!(state.input_buffer, "draft", "Esc never sends or drops text");
        }
        InputCmd::SubmitOrNewline => {
            let mut state = composer("hello", 5);
            assert_eq!(
                press_all(&mut state, keys),
                vec![InputAction::SubmitMessage("hello".to_string())]
            );
            assert_eq!(state.input_buffer, "");
            assert_eq!(state.input_history.len(), 1);
            // Multi-line mode: the same key is a newline, never a send.
            let mut state = composer("a", 1);
            state.multiline_mode = true;
            assert_eq!(press_all(&mut state, keys), vec![InputAction::Consumed]);
            assert_eq!(state.input_buffer, "a\n");
            assert_eq!(state.cursor_position, 2);
        }
        InputCmd::Send => {
            let mut state = composer("hi", 2);
            state.multiline_mode = true;
            assert_eq!(
                press_all(&mut state, keys),
                vec![InputAction::SubmitMessage("hi".to_string())]
            );
            assert_eq!(state.input_buffer, "");
            // An empty composer sends nothing.
            let mut state = fresh();
            assert_eq!(press_all(&mut state, keys), vec![InputAction::Consumed]);
            assert_eq!(state.input_history.len(), 0);
        }
        InputCmd::InsertNewline => {
            let mut state = composer("ab", 1);
            assert_eq!(press_all(&mut state, keys), vec![InputAction::Consumed]);
            assert_eq!(state.input_buffer, "a\nb");
            assert_eq!(state.cursor_position, 2);
        }
        InputCmd::ToggleMultiline => {
            let mut state = fresh();
            assert!(!state.multiline_mode);
            assert_eq!(press_all(&mut state, keys), vec![InputAction::Consumed]);
            assert!(state.multiline_mode);
            assert_eq!(press_all(&mut state, keys), vec![InputAction::Consumed]);
            assert!(!state.multiline_mode);
        }
        InputCmd::PasteClipboard => {
            let mut state = fresh();
            assert_eq!(
                press_all(&mut state, keys),
                vec![InputAction::RequestClipboardPaste]
            );
        }
        InputCmd::OpenPalette => {
            let mut state = fresh();
            assert_eq!(press_all(&mut state, keys), vec![InputAction::Consumed]);
            assert!(state.command_palette.active);
            assert_eq!(
                state.focus,
                FocusState::Overlay(OverlayType::CommandPalette)
            );
            assert_eq!(
                state.command_palette.previous_focus,
                Some(FocusState::Input)
            );
        }
        InputCmd::ReverseSearch => {
            let mut state = fresh();
            assert_eq!(press_all(&mut state, keys), vec![InputAction::Consumed]);
            assert!(state.reverse_search.active);
            assert_eq!(state.focus, FocusState::Overlay(OverlayType::ReverseSearch));
        }
        InputCmd::OpenSearch => {
            let mut state = fresh();
            assert_eq!(press_all(&mut state, keys), vec![InputAction::OpenSearch]);
            assert!(state.search_state.active);
            assert_eq!(state.focus, FocusState::Overlay(OverlayType::Search));
        }
        InputCmd::NewTab => {
            let mut state = fresh();
            assert_eq!(press_all(&mut state, keys), vec![InputAction::NewTab]);
        }
        InputCmd::CancelOrQuit => {
            let mut state = fresh();
            assert_eq!(press_all(&mut state, keys), vec![InputAction::CancelOrQuit]);
        }
        InputCmd::LineEnd => {
            let mut state = composer("ab\ncd", 0);
            assert_eq!(press_all(&mut state, keys), vec![InputAction::Consumed]);
            assert_eq!(state.cursor_position, 2, "end of the FIRST logical line");
            assert_eq!(state.input_buffer, "ab\ncd");
            assert!(
                !state.multiline_mode,
                "Ctrl+E is never the multiline toggle"
            );
        }
        InputCmd::DeleteBack => {
            let mut state = composer("abc", 3);
            assert_eq!(press_all(&mut state, keys), vec![InputAction::Consumed]);
            assert_eq!(state.input_buffer, "ab");
            assert_eq!(state.cursor_position, 2);
        }
        InputCmd::DeleteForward => {
            let mut state = composer("abc", 1);
            assert_eq!(press_all(&mut state, keys), vec![InputAction::Consumed]);
            assert_eq!(state.input_buffer, "ac");
            assert_eq!(state.cursor_position, 1);
        }
        InputCmd::CursorLeft => {
            let mut state = composer("abc", 3);
            assert_eq!(press_all(&mut state, keys), vec![InputAction::Consumed]);
            assert_eq!(state.cursor_position, 2);
            assert_eq!(state.input_buffer, "abc");
        }
        InputCmd::CursorRight => {
            let mut state = composer("abc", 1);
            assert_eq!(press_all(&mut state, keys), vec![InputAction::Consumed]);
            assert_eq!(state.cursor_position, 2);
            assert_eq!(state.input_buffer, "abc");
        }
        InputCmd::CursorHome => {
            let mut state = composer("abc", 2);
            assert_eq!(press_all(&mut state, keys), vec![InputAction::Consumed]);
            assert_eq!(state.cursor_position, 0);
            assert_eq!(state.input_buffer, "abc");
        }
        InputCmd::CursorEnd => {
            let mut state = composer("abc", 0);
            assert_eq!(press_all(&mut state, keys), vec![InputAction::Consumed]);
            assert_eq!(state.cursor_position, 3);
            assert_eq!(state.input_buffer, "abc");
        }
        InputCmd::UpOrHistoryPrev => {
            let mut state = fresh();
            state.input_history.push("first".to_string());
            state.input_history.push("second".to_string());
            assert_eq!(press_all(&mut state, keys), vec![InputAction::Consumed]);
            assert_eq!(state.input_buffer, "second", "newest entry first");
            assert_eq!(state.cursor_position, 6);
            assert_eq!(press_all(&mut state, keys), vec![InputAction::Consumed]);
            assert_eq!(state.input_buffer, "first");
        }
        InputCmd::DownOrHistoryNext => {
            let mut state = fresh();
            state.input_history.push("first".to_string());
            state.input_history.push("second".to_string());
            press_all(&mut state, &[Key::Up, Key::Up]);
            assert_eq!(state.input_buffer, "first");
            assert_eq!(press_all(&mut state, keys), vec![InputAction::Consumed]);
            assert_eq!(
                state.input_buffer, "second",
                "Down walks toward newer entries"
            );
            assert_eq!(state.cursor_position, 6);
        }
        InputCmd::CycleMode => {
            let mut state = fresh();
            assert_eq!(press_all(&mut state, keys), vec![InputAction::CycleMode]);
        }
        InputCmd::OpenSlashPopup => {
            let mut state = fresh();
            assert_eq!(press_all(&mut state, keys), vec![InputAction::Consumed]);
            assert_eq!(state.input_buffer, "/");
            assert_eq!(state.cursor_position, 1);
            assert!(state.autocomplete.active);
            assert_eq!(state.autocomplete.kind, AutocompleteKind::SlashCommand);
            assert_eq!(state.autocomplete.trigger_position, 0);
        }
        InputCmd::OpenMentionPopup => {
            let mut state = composer("hi ", 3);
            assert_eq!(press_all(&mut state, keys), vec![InputAction::Consumed]);
            assert_eq!(state.input_buffer, "hi @");
            assert_eq!(state.cursor_position, 4);
            assert!(state.autocomplete.active);
            assert_eq!(state.autocomplete.kind, AutocompleteKind::FileMention);
            assert_eq!(state.autocomplete.trigger_position, 3);
        }
        InputCmd::TranscriptPageUp | InputCmd::TranscriptPageDown => panic!(
            "{cmd:?} is an attached-only command: its witness lives next to \
             `handle_attached_key` (story21_3_attached_*), a local row must not exist"
        ),
    }
}

/// Table-driven positive leg: every Input row of the local host resolves to its
/// command and produces that command's effect by value.
#[test]
fn every_input_row_resolves_to_its_command_by_value() {
    let mut witnessed = 0;
    for row in rows_of(Context::Input, Host::Local) {
        let CommandId::Input(cmd) = row.command else {
            panic!("Input row {row:?} carries a command of another group");
        };
        match resolve(Context::Input, Host::Local, row.keys) {
            Resolved::Hit(hit) => assert_eq!(
                hit.command, row.command,
                "{row:?} must resolve to its own command"
            ),
            other => panic!("{row:?} does not resolve: {other:?}"),
        }
        witness(cmd, row.keys);
        witnessed += 1;
    }
    assert!(
        witnessed >= 20,
        "the Input table is not empty ({witnessed} rows)"
    );
}

/// A text field inherits only the leader and the quit request from Global (A4).
#[test]
fn input_inherits_only_leader_and_quit_from_global() {
    for key in [Key::Ctrl('x'), Key::Ctrl('q')] {
        match resolve(Context::Input, Host::Local, &[key]) {
            Resolved::Hit(row) => assert_eq!(row.context, Context::Global, "{key:?}"),
            other => panic!("{key:?} must resolve to its Global row from Input, got {other:?}"),
        }
    }
    for key in Key::universe() {
        if let Resolved::Hit(row) = resolve(Context::Input, Host::Local, &[key]) {
            assert!(
                row.context == Context::Input || matches!(key, Key::Ctrl('x') | Key::Ctrl('q')),
                "{key:?} leaked a {:?} row into Input",
                row.context
            );
        }
    }
}

/// Negative sweep: every key of the universe with NO Input row is inert in a
/// fresh Input state (a printable ASCII key is typing: it inserts exactly itself
/// and nothing else changes); the two Global keys have their Global effect.
#[test]
fn keys_without_an_input_row_are_inert_or_plain_typing() {
    let mut swept = 0;
    for key in Key::universe() {
        if global_owned(Context::Input, key) {
            assert_global_effect(&mut fresh(), key);
            continue;
        }
        if !matches!(resolve(Context::Input, Host::Local, &[key]), Resolved::Miss) {
            continue;
        }
        let mut state = fresh();
        let before = salient(&state);
        let action = press(&mut state, key);
        assert!(
            is_inert(&action),
            "{key:?} has no Input row but returned {action:?}"
        );
        let expected = match key {
            Key::Char(c) => Salient {
                buffer: c.to_string(),
                cursor: 1,
                ..before
            },
            _ => before,
        };
        assert_eq!(salient(&state), expected, "{key:?} changed salient state");
        swept += 1;
    }
    assert!(swept > 100, "the sweep covered the universe ({swept} keys)");

    // Positive control: a key WITH a row visibly changes the compared state.
    let mut state = composer("abc", 3);
    let before = salient(&state);
    press(&mut state, Key::Left);
    assert_ne!(
        salient(&state),
        before,
        "Left has a row and moves the cursor"
    );
}

/// AC12: unbound Ctrl/Alt letters are consumed no-ops in Input, never the bare
/// letter and never a command (Ctrl+A/W/K/U/H, Ctrl+J, Ctrl+Y, Alt+C/L/U).
#[test]
fn unbound_ctrl_and_alt_letters_are_inert_in_input() {
    let keys = [
        Key::Ctrl('a'),
        Key::Ctrl('w'),
        Key::Ctrl('k'),
        Key::Ctrl('u'),
        Key::Ctrl('h'),
        Key::Ctrl('j'),
        Key::Ctrl('y'),
        Key::Alt('c'),
        Key::Alt('l'),
        Key::Alt('u'),
        Key::Alt('x'),
    ];
    for key in keys {
        assert!(
            matches!(resolve(Context::Input, Host::Local, &[key]), Resolved::Miss),
            "{key:?} must have no Input row"
        );
        let mut state = composer("abc", 3);
        let before = salient(&state);
        let action = press(&mut state, key);
        assert!(is_inert(&action), "{key:?} returned {action:?}");
        assert_eq!(salient(&state), before, "{key:?} must not type or act");
    }
}
