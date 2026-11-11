//! Story 21.3 conformance leg K4 (navigation contexts): History (the 21-7 frame),
//! SearchTyping, SearchNavigating and BookmarkList against the real local
//! dispatch, through `support::press` only.
//!
//! Positive: every row of each context, with that context current in a real
//! `TuiState`, yields its command's effect BY VALUE (selection index, query
//! text, sub-state, returned payload). Each `witness` match is exhaustive over
//! its command enum, so a new variant forces a new witness.
//!
//! Negative: every `Key::universe()` key that resolves to no row does nothing
//! (inert action, salient state unchanged). The two documented text exceptions:
//! a printable key in Search *typing* appends exactly itself to the query, and a
//! printable other than `n`/`N` in Search *navigating* returns to typing and
//! appends itself (the whole printable range is one arm, not a row).

use rustain::adapters::tui::app::InputAction;
use rustain::adapters::tui::history_window::{self, Fit};
use rustain::adapters::tui::keymap::table::GlobalCmd;
use rustain::adapters::tui::keymap::table_input::{FieldCmd, InputCmd};
use rustain::adapters::tui::keymap::table_nav::{BookmarkCmd, SearchCmd};
use rustain::adapters::tui::keymap::table_overlay::OverlayCmd;
use rustain::adapters::tui::keymap::{self, Binding, CommandId, Context, Host, Key, Resolved};
use rustain::adapters::tui::presentation::{Surface, is_open, reducer, text_field::TextField};
use rustain::adapters::tui::state::{SearchSubstate, TuiState};
use rustain::domain::models::{ConversationSummary, FocusState};
use rustain::domain::services::session_index::SessionIndex;

use super::support::{assert_global_effect, fresh, global_owned, is_inert, press};

// ── fixtures ──────────────────────────────────────────────────────────────

/// The History window open over five conversations (`h0` newest .. `h4`), every rail hint
/// painted whole (what a wide paint offers), `h1` selected.
fn history_state() -> TuiState {
    let summaries = (0..5)
        .map(|i| ConversationSummary {
            id: format!("h{i}"),
            title: format!("Conversation {i}"),
            created_at: 1_000 - i64::from(i) * 10,
            updated_at: 1_000 - i64::from(i) * 10,
            message_count: 3,
            has_fork_source: false,
        })
        .collect();
    let index = SessionIndex::build(summaries);
    let mut state = TuiState::new(130, 24);
    history_window::open(&mut state, &index, 2_000);
    state
        .history_window
        .as_ref()
        .expect("the window is open")
        .set_fit(Fit {
            rows: 5,
            viewport: 5,
            open: true,
            delete: true,
            search: true,
            ..Fit::default()
        });
    assert_eq!(press(&mut state, Key::Down), InputAction::Consumed);
    assert_eq!(selected(&state).as_deref(), Some("h1"));
    state
}

/// Within-conversation search opened through its real front door (`Ctrl+F` in
/// an empty composer) with the query `foo` typed; optionally committed to
/// navigating (what the event loop does on `SearchCommit` with matches).
fn search_state(navigating: bool) -> TuiState {
    let mut state = fresh();
    assert_eq!(press(&mut state, Key::Ctrl('f')), InputAction::OpenSearch);
    assert!(is_open(&state, Surface::Search));
    assert_eq!(state.focus, FocusState::Input);
    assert_eq!(state.search_state.substate, SearchSubstate::Typing);
    for c in "foo".chars() {
        assert_eq!(
            press(&mut state, Key::Char(c)),
            InputAction::SearchQueryChanged
        );
    }
    assert_eq!(state.search_state.query.as_str(), "foo");
    if navigating {
        state.search_state.substate = SearchSubstate::Navigating;
    }
    state
}

/// The bookmark list open with five bookmarks, the third selected.
fn bookmark_state() -> TuiState {
    let mut state = fresh();
    state.focus = FocusState::Chat;
    reducer::open(&mut state, Surface::BookmarkList);
    state.bookmark_list_count = 5;
    state.bookmark_list_selected = 2;
    state
}

fn rows(context: Context) -> Vec<&'static Binding> {
    keymap::resolve::rows_of(context, Host::Local).collect()
}

/// `row` is what its own keys resolve to from `from` (own rows, then parents).
fn assert_resolves(from: Context, row: &Binding) {
    match keymap::resolve(from, Host::Local, row.keys) {
        Resolved::Hit(hit) => {
            assert_eq!(
                hit.command, row.command,
                "{from:?}: keys {:?} resolve elsewhere",
                row.keys
            );
            assert_eq!(hit.context, row.context);
        }
        other => panic!("{from:?}: row {row:?} does not resolve to itself: {other:?}"),
    }
}

// ── History ───────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
struct HistorySalient {
    focus: FocusState,
    top: Option<Surface>,
    depth: usize,
    selected: Option<String>,
    input: String,
    leader: bool,
}

fn history_salient(state: &TuiState) -> HistorySalient {
    HistorySalient {
        focus: state.focus.clone(),
        top: state.modal_stack.top().map(|frame| frame.surface),
        depth: state.modal_stack.len(),
        selected: selected(state),
        input: state.input_buffer.clone(),
        leader: state.pending_leader.is_some(),
    }
}

/// The conversation id the open History window has selected.
fn selected(state: &TuiState) -> Option<String> {
    state
        .history_window
        .as_ref()
        .and_then(|window| window.selected_id().map(str::to_owned))
}

fn history_witness(command: CommandId, keys: &[Key]) {
    use rustain::adapters::tui::keymap::table_mission::MissionCmd;
    use rustain::adapters::tui::presentation::viewer::ViewerCmd;
    let key = keys[0];
    let mut state = history_state();
    let sel = |state: &TuiState| selected(state).expect("a selected conversation");
    match command {
        CommandId::Viewer(ViewerCmd::LineDown) => {
            assert_eq!(press(&mut state, key), InputAction::Consumed);
            assert_eq!(sel(&state), "h2");
            // The selection stops on the last entry.
            for _ in 0..6 {
                press(&mut state, key);
            }
            assert_eq!(sel(&state), "h4");
        }
        CommandId::Viewer(ViewerCmd::LineUp) => {
            assert_eq!(press(&mut state, key), InputAction::Consumed);
            assert_eq!(sel(&state), "h0");
            press(&mut state, key);
            assert_eq!(sel(&state), "h0", "the selection stops on the first entry");
        }
        CommandId::Viewer(ViewerCmd::HalfDown) => {
            assert_eq!(press(&mut state, key), InputAction::Consumed);
            assert_eq!(sel(&state), "h3", "half of the 5-row viewport is 2");
        }
        CommandId::Viewer(ViewerCmd::HalfUp) => {
            assert_eq!(press(&mut state, key), InputAction::Consumed);
            assert_eq!(sel(&state), "h0");
        }
        CommandId::Viewer(ViewerCmd::PageDown) => {
            assert_eq!(press(&mut state, key), InputAction::Consumed);
            assert_eq!(sel(&state), "h4");
        }
        CommandId::Viewer(ViewerCmd::PageUp) => {
            assert_eq!(press(&mut state, key), InputAction::Consumed);
            assert_eq!(sel(&state), "h0");
        }
        CommandId::Viewer(ViewerCmd::Top) => {
            assert_eq!(press(&mut state, key), InputAction::Consumed);
            assert_eq!(sel(&state), "h0");
        }
        CommandId::Viewer(ViewerCmd::Tail) => {
            assert_eq!(press(&mut state, key), InputAction::Consumed);
            assert_eq!(sel(&state), "h4");
        }
        CommandId::Mission(MissionCmd::HistoryOpen) => {
            assert_eq!(
                press(&mut state, key),
                InputAction::OpenHistoryEntry("h1".to_owned())
            );
        }
        CommandId::Mission(MissionCmd::HistoryDelete) => {
            assert_eq!(
                press(&mut state, key),
                InputAction::DeleteHistoryEntry("h1".to_owned())
            );
        }
        CommandId::Mission(MissionCmd::HistorySearch) => {
            assert_eq!(press(&mut state, key), InputAction::OpenCrossSearch);
            assert!(is_open(&state, Surface::CrossSearch));
            assert_eq!(
                state.modal_stack.len(),
                2,
                "CrossSearch opens above History"
            );
            let frame = state.modal_stack.top().unwrap();
            assert_eq!(frame.surface, Surface::CrossSearch);
            assert_eq!(frame.conversation_id, None);
            assert!(is_open(&state, Surface::History));
        }
        CommandId::Mission(MissionCmd::Back) => {
            assert_eq!(press(&mut state, key), InputAction::Consumed);
            assert!(!is_open(&state, Surface::History));
            assert_eq!(state.modal_stack.len(), 0);
        }
        CommandId::Global(GlobalCmd::OpenLeader) => {
            assert_eq!(press(&mut state, key), InputAction::Consumed);
            assert!(state.pending_leader.is_some(), "Ctrl+X opens the leader");
            assert!(is_open(&state, Surface::History), "History stays beneath");
        }
        CommandId::Input(InputCmd::OpenPalette) => {
            assert_eq!(press(&mut state, key), InputAction::Consumed);
            assert!(is_open(&state, Surface::CommandPalette));
            assert!(is_open(&state, Surface::History), "History stays beneath");
        }
        CommandId::Overlay(OverlayCmd::QuitRequest) => {
            assert_eq!(press(&mut state, key), InputAction::Quit);
            assert!(is_open(&state, Surface::History), "History stays open");
        }
        other => panic!("History row {keys:?} carries an unwitnessed command {other:?}"),
    }
}

#[test]
fn every_history_row_resolves_to_its_command_by_value() {
    let own = rows(Context::History);
    let inherited = rows(Context::InspectorBody);
    assert_eq!(
        own.len(),
        8,
        "Enter d / Esc Ctrl+C Ctrl+X Ctrl+P Ctrl+Q, got {}",
        own.len()
    );
    assert!(
        inherited.len() >= 10,
        "the RU-25 scroll rows, got {}",
        inherited.len()
    );
    for row in own.into_iter().chain(inherited) {
        // The History frame inherits the RU-25 rows through its parent.
        assert_resolves(Context::History, row);
        history_witness(row.command, row.keys);
    }
}

#[test]
fn keys_without_a_history_row_do_nothing_in_the_history_frame() {
    let mut swept = 0;
    for key in Key::universe() {
        if global_owned(Context::History, key) {
            assert_global_effect(&mut history_state(), key);
            continue;
        }
        if !matches!(
            keymap::resolve(Context::History, Host::Local, &[key]),
            Resolved::Miss
        ) {
            continue;
        }
        let mut state = history_state();
        let before = history_salient(&state);
        let action = press(&mut state, key);
        assert!(
            is_inert(&action),
            "{key:?} without a History row returned {action:?}"
        );
        assert_eq!(
            history_salient(&state),
            before,
            "{key:?} moved History state"
        );
        swept += 1;
    }
    assert!(
        swept > 100,
        "the sweep must cover the whole universe, swept {swept}"
    );

    // Positive control: a key WITH a row visibly changes state.
    let mut state = history_state();
    let before = history_salient(&state);
    press(&mut state, Key::Char('j'));
    assert_ne!(history_salient(&state), before);
}

/// A History key acts only where its rail hint was painted: with nothing painted `Enter`, `d`
/// and `/` are inert and move nothing.
#[test]
fn history_actions_are_inert_where_their_hint_was_not_painted() {
    for key in [Key::Enter, Key::Char('d'), Key::Char('/')] {
        let mut state = history_state();
        state
            .history_window
            .as_ref()
            .expect("open")
            .set_fit(Fit::default());
        let before = history_salient(&state);
        let action = press(&mut state, key);
        assert!(is_inert(&action), "{key:?} unpainted returned {action:?}");
        assert_eq!(
            history_salient(&state),
            before,
            "{key:?} unpainted moved state"
        );
    }
}

// ── SearchTyping / SearchNavigating ───────────────────────────────────────

#[derive(Debug, PartialEq)]
struct SearchSalient {
    focus: FocusState,
    query: TextField,
    substate: SearchSubstate,
    active: bool,
    input: String,
}

fn search_salient(state: &TuiState) -> SearchSalient {
    SearchSalient {
        focus: state.focus.clone(),
        query: state.search_state.query.clone(),
        substate: state.search_state.substate,
        active: is_open(state, Surface::Search),
        input: state.input_buffer.clone(),
    }
}

fn search_witness(context: Context, cmd: SearchCmd, keys: &[Key]) {
    let navigating = context == Context::SearchNavigating;
    let key = keys[0];
    let mut state = search_state(navigating);
    let sub = if navigating {
        SearchSubstate::Navigating
    } else {
        SearchSubstate::Typing
    };
    match cmd {
        SearchCmd::Close => {
            assert_eq!(press(&mut state, key), InputAction::CloseSearch);
            assert_eq!(
                state.search_state.query.as_str(),
                "foo",
                "the production effect seam applies close/restore after dispatch"
            );
        }
        SearchCmd::Commit => {
            assert_eq!(press(&mut state, key), InputAction::SearchCommit);
            assert_eq!(state.search_state.query.as_str(), "foo");
        }
        SearchCmd::NextMatch => {
            assert!(navigating, "n navigates only after the query is committed");
            assert_eq!(press(&mut state, key), InputAction::SearchNext);
            assert_eq!(
                state.search_state.query.as_str(),
                "foo",
                "n is a command, never text"
            );
            assert_eq!(state.search_state.substate, sub);
        }
        SearchCmd::PrevMatch => {
            assert!(navigating);
            assert_eq!(press(&mut state, key), InputAction::SearchPrev);
            assert_eq!(state.search_state.query.as_str(), "foo");
            assert_eq!(state.search_state.substate, sub);
        }
    }
}

#[test]
fn every_search_row_resolves_to_its_command_by_value() {
    for context in [Context::SearchTyping, Context::SearchNavigating] {
        let own = rows(context);
        let expected = if context == Context::SearchTyping {
            4
        } else {
            5
        };
        assert!(own.len() >= expected, "{context:?} rows, got {}", own.len());
        for row in own {
            assert_resolves(context, row);
            match row.command {
                CommandId::Search(cmd) => search_witness(context, cmd, row.keys),
                CommandId::Field(FieldCmd::Clear) => {
                    let navigating = context == Context::SearchNavigating;
                    let mut state = search_state(navigating);
                    assert_eq!(
                        press(&mut state, row.keys[0]),
                        if navigating {
                            InputAction::SearchReturnToTyping
                        } else {
                            InputAction::SearchQueryChanged
                        }
                    );
                    assert!(state.search_state.query.is_empty());
                    assert_eq!(state.search_state.substate, SearchSubstate::Typing);
                }
                CommandId::Overlay(OverlayCmd::QuitRequest) => {
                    let mut state = search_state(context == Context::SearchNavigating);
                    let before = search_salient(&state);
                    assert_eq!(press(&mut state, row.keys[0]), InputAction::Quit);
                    assert_eq!(
                        search_salient(&state),
                        before,
                        "S-QUIT keeps the exact search"
                    );
                }
                CommandId::Global(keymap::table::GlobalCmd::OpenLeader) => {
                    let mut state = search_state(context == Context::SearchNavigating);
                    let before = search_salient(&state);
                    let frame = state.modal_stack.top().unwrap().clone();
                    assert_eq!(press(&mut state, row.keys[0]), InputAction::Consumed);
                    assert_eq!(state.pending_leader, Some(context));
                    assert_eq!(state.modal_stack.top(), Some(&frame));
                    press(&mut state, Key::Esc);
                    assert_eq!(search_salient(&state), before);
                    assert_eq!(state.modal_stack.top(), Some(&frame));
                }
                CommandId::Input(keymap::table_input::InputCmd::OpenPalette) => {
                    let mut state = search_state(context == Context::SearchNavigating);
                    let before = search_salient(&state);
                    let frame = state.modal_stack.top().unwrap().clone();
                    assert_eq!(press(&mut state, row.keys[0]), InputAction::Consumed);
                    assert_eq!(
                        state.modal_stack.top().unwrap().surface,
                        Surface::CommandPalette
                    );
                    assert!(state.modal_stack.iter().any(|owned| owned == &frame));
                    press(&mut state, Key::Esc);
                    assert_eq!(search_salient(&state), before);
                    assert_eq!(state.modal_stack.top(), Some(&frame));
                }
                _ => panic!("unexpected Search row {row:?}"),
            }
        }
    }
}

/// Sweep every rowless key of a Search sub-state: a printable inserts itself (and,
/// in navigating, returns to typing: the documented refine-without-Esc arm); every
/// other key is inert. Returns `(printable count, inert count)`.
fn sweep_search(context: Context, navigating: bool) -> (usize, usize) {
    let (mut text, mut inert) = (0, 0);
    for key in Key::universe() {
        if !matches!(
            keymap::resolve(context, Host::Local, &[key]),
            Resolved::Miss
        ) {
            continue;
        }
        let mut state = search_state(navigating);
        let before = search_salient(&state);
        let action = press(&mut state, key);
        if let Key::Char(c) = key {
            let expected = if navigating {
                InputAction::SearchReturnToTyping
            } else {
                InputAction::SearchQueryChanged
            };
            assert_eq!(action, expected, "{context:?} {key:?}");
            assert_eq!(state.search_state.substate, SearchSubstate::Typing);
            assert_eq!(
                state.search_state.query.as_str(),
                format!("foo{c}"),
                "{key:?} inserts itself"
            );
            assert_eq!(state.focus, before.focus);
            assert_eq!(
                state.input_buffer, before.input,
                "the composer is not the field"
            );
            text += 1;
        } else {
            assert!(
                is_inert(&action),
                "{context:?} {key:?} without a row returned {action:?}"
            );
            assert_eq!(
                search_salient(&state),
                before,
                "{context:?} {key:?} moved Search state"
            );
            inert += 1;
        }
    }
    (text, inert)
}

#[test]
fn search_typing_appends_exactly_the_printable_and_every_other_rowless_key_is_inert() {
    let (printable, inert) = sweep_search(Context::SearchTyping, false);
    assert_eq!(printable, 95, "every printable ASCII key types");
    assert!(
        inert > 50,
        "the non-printable universe is inert, swept {inert}"
    );
}

#[test]
fn search_navigating_n_rows_navigate_and_every_other_rowless_key_is_inert_or_returns_to_typing() {
    let (returned, inert) = sweep_search(Context::SearchNavigating, true);
    assert_eq!(
        returned, 93,
        "every printable except n and N returns to typing"
    );
    assert!(
        inert > 40,
        "the non-printable universe is inert, swept {inert}"
    );
    // Positive control: a key WITH a row (`n`) moves match navigation, and
    // leaves the query and sub-state alone.
    let mut state = search_state(true);
    let before = search_salient(&state);
    assert_eq!(press(&mut state, Key::Char('n')), InputAction::SearchNext);
    assert_eq!(search_salient(&state), before);
}

// ── BookmarkList ──────────────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
struct BookmarkSalient {
    focus: FocusState,
    selected: usize,
    count: usize,
    input: String,
    frames: Vec<Surface>,
}

fn bookmark_salient(state: &TuiState) -> BookmarkSalient {
    BookmarkSalient {
        focus: state.focus.clone(),
        selected: state.bookmark_list_selected,
        count: state.bookmark_list_count,
        input: state.input_buffer.clone(),
        frames: state
            .modal_stack
            .iter()
            .map(|frame| frame.surface)
            .collect(),
    }
}

fn bookmark_witness(cmd: BookmarkCmd, keys: &[Key]) {
    let key = keys[0];
    let mut state = bookmark_state();
    match cmd {
        BookmarkCmd::SelectNext => {
            assert_eq!(press(&mut state, key), InputAction::Consumed);
            assert_eq!(state.bookmark_list_selected, 3);
            state.bookmark_list_selected = 4;
            press(&mut state, key);
            assert_eq!(
                state.bookmark_list_selected, 4,
                "clamped to the last bookmark"
            );
        }
        BookmarkCmd::SelectPrev => {
            assert_eq!(press(&mut state, key), InputAction::Consumed);
            assert_eq!(state.bookmark_list_selected, 1);
            state.bookmark_list_selected = 0;
            press(&mut state, key);
            assert_eq!(
                state.bookmark_list_selected, 0,
                "clamped to the first bookmark"
            );
        }
        BookmarkCmd::Delete => {
            assert_eq!(press(&mut state, key), InputAction::DeleteBookmark);
            assert_eq!(
                state.bookmark_list_selected, 2,
                "the loop removes it, not the key"
            );
        }
        BookmarkCmd::Undo => assert_eq!(press(&mut state, key), InputAction::UndoBookmarkDelete),
        BookmarkCmd::Jump => assert_eq!(press(&mut state, key), InputAction::JumpToBookmark),
        BookmarkCmd::Close => assert_eq!(press(&mut state, key), InputAction::CloseBookmarkList),
    }
    assert_eq!(
        is_open(&state, Surface::BookmarkList),
        cmd != BookmarkCmd::Close
    );
}

#[test]
fn every_bookmark_row_resolves_to_its_command_by_value() {
    let own = rows(Context::BookmarkList);
    assert!(own.len() >= 10, "BookmarkList rows, got {}", own.len());
    for row in own {
        assert_resolves(Context::BookmarkList, row);
        match row.command {
            CommandId::Bookmark(cmd) => bookmark_witness(cmd, row.keys),
            CommandId::Overlay(OverlayCmd::QuitRequest) => {
                let mut state = bookmark_state();
                let before = bookmark_salient(&state);
                assert_eq!(press(&mut state, row.keys[0]), InputAction::Quit);
                assert_eq!(
                    bookmark_salient(&state),
                    before,
                    "S-QUIT keeps the exact bookmark list"
                );
            }
            CommandId::Global(GlobalCmd::OpenLeader) | CommandId::Input(InputCmd::OpenPalette) => {
                let mut state = bookmark_state();
                let caller = state.modal_stack.top().unwrap().id;
                let before = bookmark_salient(&state);
                assert!(is_inert(&press(&mut state, row.keys[0])));
                assert!(state.modal_stack.iter().any(|frame| frame.id == caller));
                assert_eq!(state.focus, before.focus);
                assert_eq!(state.bookmark_list_selected, before.selected);
                assert_eq!(state.bookmark_list_count, before.count);
                assert_eq!(state.input_buffer, before.input);
                if row.command == CommandId::Global(GlobalCmd::OpenLeader) {
                    assert!(state.pending_leader.is_some());
                    assert_eq!(state.modal_stack.top().unwrap().id, caller);
                } else {
                    assert_eq!(
                        state.modal_stack.top().unwrap().surface,
                        Surface::CommandPalette
                    );
                }
            }
            _ => panic!("unexpected BookmarkList row {row:?}"),
        }
    }
}

#[test]
fn keys_without_a_bookmark_row_do_nothing() {
    let mut swept = 0;
    for key in Key::universe() {
        if !matches!(
            keymap::resolve(Context::BookmarkList, Host::Local, &[key]),
            Resolved::Miss
        ) {
            continue;
        }
        let mut state = bookmark_state();
        let before = bookmark_salient(&state);
        let action = press(&mut state, key);
        assert!(
            is_inert(&action),
            "{key:?} without a BookmarkList row returned {action:?}"
        );
        assert_eq!(
            bookmark_salient(&state),
            before,
            "{key:?} moved BookmarkList state"
        );
        swept += 1;
    }
    assert!(
        swept > 130,
        "the sweep must cover the whole universe, swept {swept}"
    );

    // Positive control: a key WITH a row visibly changes state.
    let mut state = bookmark_state();
    let before = bookmark_salient(&state);
    press(&mut state, Key::Char('j'));
    assert_ne!(bookmark_salient(&state), before);
}
