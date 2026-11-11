//! Shared fields are exercised through the real key boundary, never a helper-only editor.
use super::keymap_support::press;
use super::stage_support::{cache_keys, chat_pixels, paste, stage, view};
use rustain::adapters::tui::{
    app::{InputAction, handle_input},
    history_window,
    keymap::Key,
    presentation::{Surface, text_field::TextField},
    state::TuiState,
};
use rustain::domain::events::DomainInputEvent;
use rustain::domain::models::{FocusState, TabManager};
use rustain::domain::services::session_index::SessionIndex;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Palette,
    Search,
    Reverse,
    Cross,
    Log,
    Help,
}

const FIELDS: [Field; 6] = [
    Field::Palette,
    Field::Search,
    Field::Reverse,
    Field::Cross,
    Field::Log,
    Field::Help,
];

fn key(h: &mut crate::e2e_harness::TestHarness, tabs: &mut TabManager, key: Key) -> InputAction {
    let action = super::k8_stage::search_key(h, tabs, key);
    if action == InputAction::CloseCrossSearch {
        rustain::adapters::tui::handlers::export::close_cross_search(&mut h.state);
    }
    action
}

impl Field {
    fn prepare(self, state: &mut TuiState) {
        if self == Self::Cross {
            // CrossSearch is opened from the History frame; the Monitor column no longer
            // paints History (old -> new: the sidebar-focus fixture is gone).
            state.monitor = true;
        }
        state.focus = FocusState::Input;
    }

    fn begin_open(self, state: &mut TuiState) -> (u16, u16) {
        self.prepare(state);
        let size = (state.terminal_width, state.terminal_height);
        if self == Self::Cross {
            // The History window is opened and painted at 130 columns (its `/` hint must have
            // been painted); the resulting global frame then owns editing at all supported
            // sizes.
            handle_input(state, &DomainInputEvent::Resize(130, 30));
            history_window::open(state, &SessionIndex::new(), 0);
            super::k7_cross_search::paint(state);
        }
        size
    }

    fn finish_open(self, state: &mut TuiState, size: (u16, u16)) {
        if self == Self::Cross {
            handle_input(state, &DomainInputEvent::Resize(size.0, size.1));
        }
        match self {
            Self::Reverse => assert!(state.reverse_search.is_open()),
            _ => assert_eq!(
                state.modal_stack.top().unwrap().surface,
                match self {
                    Self::Palette => Surface::CommandPalette,
                    Self::Search => Surface::Search,
                    Self::Cross => Surface::CrossSearch,
                    Self::Log => Surface::NotificationLog,
                    Self::Help => Surface::Help,
                    Self::Reverse => unreachable!(),
                }
            ),
        }
        if self.inspector() {
            assert!(match self {
                Self::Log => state.notification_log.as_ref().unwrap().search.editing,
                Self::Help => state.help_overlay.search.editing,
                _ => unreachable!(),
            });
        }
    }

    fn open_keys(self) -> &'static [Key] {
        match self {
            Self::Palette => &[Key::Ctrl('p')],
            Self::Search => &[Key::Ctrl('f')],
            Self::Reverse => &[Key::Ctrl('r')],
            Self::Cross => &[Key::Char('/')],
            Self::Log => &[Key::Ctrl('x'), Key::Char('n'), Key::Char('/')],
            Self::Help => &[Key::Ctrl('x'), Key::Char('?'), Key::Char('/')],
        }
    }

    fn open(self, state: &mut TuiState) {
        let size = self.begin_open(state);
        for key in self.open_keys() {
            press(state, *key);
        }
        self.finish_open(state, size);
    }

    fn open_on_stage(self, h: &mut crate::e2e_harness::TestHarness, tabs: &mut TabManager) {
        let size = self.begin_open(&mut h.state);
        for input in self.open_keys() {
            key(h, tabs, *input);
        }
        self.finish_open(&mut h.state, size);
    }

    fn text(self, state: &TuiState) -> &str {
        self.value(state).as_str()
    }

    fn value(self, state: &TuiState) -> &TextField {
        match self {
            Self::Palette => &state.command_palette.filter_text,
            Self::Search => &state.search_state.query,
            Self::Reverse => &state.reverse_search.query,
            Self::Cross => &state.cross_search.query,
            Self::Log => &state.notification_log.as_ref().unwrap().search.query,
            Self::Help => &state.help_overlay.search.query,
        }
    }

    fn inspector(self) -> bool {
        matches!(self, Self::Log | Self::Help)
    }
}

#[test]
fn insert_and_kill_line_at_the_caret_grapheme_safe() {
    const TAIL: &str = "👩‍👩‍👧‍👦界e\u{301} z";
    for field in FIELDS {
        let mut state = TuiState::new(130, 30);
        field.open(&mut state);
        for ch in format!("a{TAIL}").chars() {
            press(&mut state, Key::Char(ch));
        }
        assert_eq!(
            field.text(&state),
            format!("a{TAIL}"),
            "{field:?}: typing control"
        );
        press(&mut state, Key::Ctrl('a'));
        press(&mut state, Key::Right);
        press(&mut state, Key::Char('X'));
        assert_eq!(
            field.text(&state),
            format!("aX{TAIL}"),
            "{field:?}: insert at caret, not tail"
        );
        press(&mut state, Key::Ctrl('k'));
        assert_eq!(
            field.text(&state),
            "aX",
            "{field:?}: kill the entire grapheme-safe tail"
        );
        press(&mut state, Key::Ctrl('a'));
        press(&mut state, Key::Ctrl('w'));
        assert_eq!(
            field.text(&state),
            "aX",
            "{field:?}: unchanged edit at start"
        );
        press(&mut state, Key::Ctrl('e'));
        press(&mut state, Key::Ctrl('h'));
        assert_eq!(field.text(&state), "a", "{field:?}: delete before caret");
        assert!(
            state.input_buffer.is_empty(),
            "{field:?}: field never edits composer"
        );
    }
}

#[test]
fn crlf_deletions_route_through_every_migrated_local_field() {
    for field in FIELDS {
        for (edit, position, expected, cursor) in [
            (Key::Ctrl('k'), 1, "a\r\nb", 1),
            (Key::Ctrl('k'), 0, "\r\nb", 0),
            (Key::Ctrl('k'), 3, "a\r\n", 3),
            (Key::Ctrl('k'), 4, "a\r\nb", 4),
            (Key::Ctrl('u'), 1, "\r\nb", 0),
            (Key::Ctrl('u'), 3, "a\r\nb", 3),
            (Key::Ctrl('u'), 4, "a\r\n", 3),
            (Key::Ctrl('w'), 1, "\r\nb", 0),
            (Key::Ctrl('w'), 3, "b", 0),
            (Key::Ctrl('w'), 4, "a\r\n", 3),
            (Key::Backspace, 3, "ab", 1),
            (Key::Backspace, 1, "\r\nb", 0),
            (Key::Ctrl('h'), 3, "ab", 1),
            (Key::Delete, 1, "ab", 1),
            (Key::Delete, 0, "\r\nb", 0),
            (Key::Delete, 4, "a\r\nb", 4),
        ] {
            let (mut h, mut tabs) = stage(80, 24);
            h.state.focus = FocusState::Input;
            h.state.input_history.push("history control".into());
            field.open_on_stage(&mut h, &mut tabs);
            h.state.input_buffer = "untouched composer".into();
            paste(&mut h, &mut tabs, "a\r\nb");
            assert_eq!(field.text(&h.state), "a\r\nb", "real paste control");
            for motion in [Key::Home, Key::Left, Key::Home] {
                key(&mut h, &mut tabs, motion);
            }
            for _ in 0..match position {
                0 => 0,
                1 => 1,
                3 => 2,
                4 => 3,
                _ => unreachable!(),
            } {
                key(&mut h, &mut tabs, Key::Right);
            }
            assert_eq!(field.value(&h.state).cursor(), position);
            let before = view(&tabs);
            let action = key(&mut h, &mut tabs, edit);
            assert_eq!(field.text(&h.state), expected, "{field:?}: {edit:?}");
            assert_eq!(field.value(&h.state).cursor(), cursor);
            assert_eq!(
                action,
                match (field, expected != "a\r\nb") {
                    (Field::Search, true) => InputAction::SearchQueryChanged,
                    (Field::Cross, true) => InputAction::CrossSearchQueryChanged,
                    _ => InputAction::Consumed,
                }
            );
            assert_eq!(view(&tabs), before);
            assert_eq!(h.state.input_buffer, "untouched composer");
        }
    }
}

#[test]
fn crlf_search_eol_kill_never_rescans_or_changes_the_rendered_owning_stage() {
    let (mut h, mut tabs) = stage(80, 24);
    h.state.focus = FocusState::Input;
    h.render_active_tab(&tabs);
    let original = view(&tabs);
    key(&mut h, &mut tabs, Key::Ctrl('f'));
    h.state.input_buffer = "untouched composer".into();
    paste(&mut h, &mut tabs, "stage-0-row-00");
    assert!(!h.state.search_state.matches.is_empty());
    assert_ne!(view(&tabs), original, "real calm-jump change control");
    paste(&mut h, &mut tabs, "\r\nb");
    key(&mut h, &mut tabs, Key::Home);
    key(&mut h, &mut tabs, Key::Left);
    h.render_active_tab(&tabs);
    let (before, pixels, cache) = (view(&tabs), chat_pixels(&h), cache_keys(&h, &tabs));
    let field = h.state.search_state.query.clone();
    let instant = h.state.search_state.last_search_instant;
    assert!(instant.is_some(), "the production paste actually scanned");
    assert_eq!(
        key(&mut h, &mut tabs, Key::Ctrl('k')),
        InputAction::Consumed
    );
    assert_eq!(h.state.search_state.query, field);
    assert_eq!(h.state.search_state.last_search_instant, instant);
    assert_eq!(view(&tabs), before);
    h.render_active_tab(&tabs);
    assert_eq!(chat_pixels(&h), pixels);
    assert_eq!(cache_keys(&h, &tabs), cache);
    key(&mut h, &mut tabs, Key::Home);
    assert_eq!(
        key(&mut h, &mut tabs, Key::Ctrl('k')),
        InputAction::SearchQueryChanged
    );
    assert_eq!(h.state.search_state.query.as_str(), "\r\nb");
    assert_ne!(
        h.state.search_state.last_search_instant, instant,
        "content-change control"
    );
    assert_eq!(h.state.input_buffer, "untouched composer");
    key(&mut h, &mut tabs, Key::Esc);
    assert_eq!(view(&tabs), original);
}

#[test]
fn reverse_query_owns_the_only_visible_caret() {
    let (mut h, tabs) = super::stage_support::stage(80, 24);
    h.state.input_history.push("history entry".into());
    h.state.input_buffer = "untouched composer".into();
    Field::Reverse.open(&mut h.state);
    assert!(
        h.state.reverse_search.is_open(),
        "the popup must really be offered"
    );
    h.render_active_tab(&tabs);
    assert!(
        !h.terminal.backend().cursor_visible(),
        "the owning query's in-widget caret must not leave a second composer cursor"
    );
    assert!(
        h.terminal
            .backend()
            .buffer()
            .content
            .iter()
            .any(|cell| cell.modifier.contains(ratatui::style::Modifier::REVERSED)),
        "the visible query caret is a positive paint control"
    );
}

#[test]
fn routed_paste_and_every_edit_key_keep_whole_clusters_and_the_composer() {
    for field in FIELDS {
        let (mut h, mut tabs) = stage(80, 24);
        h.state.focus = FocusState::Input;
        h.state.input_history.push("history control".into());
        field.open_on_stage(&mut h, &mut tabs);
        h.state.input_buffer = "retained composer".into();
        h.state.cursor_position = 4;
        paste(&mut h, &mut tabs, "a👩‍👩‍👧‍👦界e\u{301} z");
        assert_eq!(
            field.text(&h.state),
            "a👩‍👩‍👧‍👦界e\u{301} z",
            "{field:?}: actual paste"
        );
        key(&mut h, &mut tabs, Key::Home);
        key(&mut h, &mut tabs, Key::Right);
        key(&mut h, &mut tabs, Key::Delete);
        assert_eq!(
            field.text(&h.state),
            "a界e\u{301} z",
            "{field:?}: forward-delete whole ZWJ"
        );
        key(&mut h, &mut tabs, Key::Right);
        key(&mut h, &mut tabs, Key::Backspace);
        assert_eq!(
            field.text(&h.state),
            "ae\u{301} z",
            "{field:?}: backspace whole CJK"
        );
        key(&mut h, &mut tabs, Key::End);
        key(&mut h, &mut tabs, Key::Left);
        key(&mut h, &mut tabs, Key::Left);
        key(&mut h, &mut tabs, Key::Ctrl('u'));
        assert_eq!(
            field.text(&h.state),
            " z",
            "{field:?}: kill only the prefix"
        );
        key(&mut h, &mut tabs, Key::Ctrl('k'));
        assert_eq!(field.text(&h.state), "");
        paste(&mut h, &mut tabs, "\nα界\nend");
        key(&mut h, &mut tabs, Key::Ctrl('a'));
        key(&mut h, &mut tabs, Key::Char('X'));
        key(&mut h, &mut tabs, Key::Ctrl('k'));
        assert_eq!(
            field.text(&h.state),
            "\nα界\nX",
            "{field:?}: logical line, raw paste preserved"
        );
        assert_eq!(field.value(&h.state).cursor(), 5);
        assert_eq!(
            (h.state.input_buffer.as_str(), h.state.cursor_position),
            ("retained composer", 4)
        );
    }
}

#[test]
fn every_ascii_printable_inserts_at_the_query_caret_without_commanding() {
    for field in FIELDS {
        for ch in (0x20u8..0x7f).map(char::from) {
            let mut state = TuiState::new(80, 24);
            field.open(&mut state);
            state.input_buffer = "draft".into();
            handle_input(&mut state, &DomainInputEvent::Paste("ab".into()));
            press(&mut state, Key::Home);
            press(&mut state, Key::Right);
            let frame = state.modal_stack.top().cloned();
            if field == Field::Cross {
                h_cross_results(&mut state);
            }
            let action = press(&mut state, Key::Char(ch));
            if field == Field::Cross && matches!(ch, 'j' | 'k') {
                assert_eq!(field.text(&state), "ab");
                assert_eq!(state.cross_search.selected, if ch == 'j' { 2 } else { 0 });
                assert_eq!(
                    action,
                    InputAction::Consumed,
                    "legacy j/k reserve navigation"
                );
            } else {
                assert_eq!(field.text(&state), format!("a{ch}b"), "{field:?}: {ch:?}");
                assert!(matches!(
                    action,
                    InputAction::Consumed
                        | InputAction::SearchQueryChanged
                        | InputAction::CrossSearchQueryChanged
                ));
            }
            assert_eq!(
                state.modal_stack.top(),
                frame.as_ref(),
                "{field:?}: {ch:?} is text"
            );
            assert_eq!(state.input_buffer, "draft");
        }
    }
}

#[test]
fn query_carets_are_fitted_owned_and_hidden_under_a_leader() {
    use ratatui::style::Modifier;
    for (width, height) in [(60, 16), (80, 24), (130, 30)] {
        for field in FIELDS {
            let (mut h, mut tabs) = stage(width, height);
            h.state.focus = FocusState::Input;
            h.state.input_history.push("history fixture".into());
            field.open_on_stage(&mut h, &mut tabs);
            // Preserve the empty-composer opener; fit a retained draft underneath it.
            h.state.input_buffer = ["full composer"; 8].join("\n");
            h.render_active_tab(&tabs);
            paste(&mut h, &mut tabs, "a界e\u{301}X");
            key(&mut h, &mut tabs, Key::Home);
            key(&mut h, &mut tabs, Key::Right);
            let field_before = field.value(&h.state).clone();
            h.render_active_tab(&tabs);
            let carets = |h: &crate::e2e_harness::TestHarness| {
                h.terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .filter(|cell| {
                        cell.symbol() == "界" && cell.modifier.contains(Modifier::REVERSED)
                    })
                    .count()
            };
            assert_eq!(
                carets(&h),
                1,
                "{field:?} {width}x{height}: actual wide caret"
            );
            assert!(!h.terminal.backend().cursor_visible());
            key(&mut h, &mut tabs, Key::Ctrl('x'));
            assert!(
                h.state.pending_leader.is_some(),
                "the covering leader must be reachable"
            );
            h.render_active_tab(&tabs);
            assert_eq!(
                carets(&h),
                0,
                "{field:?}: covered query never paints a caret"
            );
            assert_eq!(field.value(&h.state), &field_before);
            key(&mut h, &mut tabs, Key::Esc);
            h.render_active_tab(&tabs);
            if matches!(field, Field::Reverse) {
                assert!(
                    !h.state.reverse_search.is_open(),
                    "legacy leader opening cancels the popup"
                );
                assert_eq!(carets(&h), 0);
            } else {
                assert_eq!(
                    carets(&h),
                    1,
                    "{field:?}: cancelling cover restores the same caret"
                );
            }
        }
    }
}

#[test]
fn non_search_fields_do_not_navigate_the_owning_stage() {
    for field in [
        Field::Palette,
        Field::Reverse,
        Field::Cross,
        Field::Log,
        Field::Help,
    ] {
        for (width, height) in [(60, 16), (80, 24), (130, 30)] {
            let (mut h, mut tabs) = stage(width, height);
            field.prepare(&mut h.state);
            h.state.input_buffer = "composer 界\nsecond row".into();
            h.state.cursor_position = 3;
            h.render_active_tab(&tabs);
            let (before_view, before_keys, before_pixels) =
                (view(&tabs), cache_keys(&h, &tabs), chat_pixels(&h));
            assert!(!before_keys.0.is_empty() || !before_keys.1.is_empty());
            let layout = super::k3_compose::layout(&h.state);
            field.open_on_stage(&mut h, &mut tabs);
            paste(&mut h, &mut tabs, "a界e\u{301}");
            key(&mut h, &mut tabs, Key::Home);
            key(&mut h, &mut tabs, Key::Right);
            key(&mut h, &mut tabs, Key::Char('Z'));
            assert_eq!(
                field.text(&h.state),
                "aZ界e\u{301}",
                "observable edit control"
            );
            h.render_active_tab(&tabs);
            assert_eq!(view(&tabs), before_view);
            assert_eq!(cache_keys(&h, &tabs), before_keys);
            assert_eq!(
                super::k3_compose::layout(&h.state).chat_pane,
                layout.chat_pane
            );
            assert_eq!(
                super::k3_compose::layout(&h.state).input_area,
                layout.input_area
            );
            key(&mut h, &mut tabs, Key::Esc);
            if field.inspector() {
                assert!(
                    h.state.modal_stack.top().is_some(),
                    "first Esc clears only the child"
                );
                key(&mut h, &mut tabs, Key::Esc);
            }
            if field == Field::Cross {
                // The History frame the search was opened from is the top frame again.
                assert_eq!(h.state.modal_stack.top().unwrap().surface, Surface::History);
                key(&mut h, &mut tabs, Key::Esc);
                assert!(h.state.modal_stack.is_empty());
            }
            h.render_active_tab(&tabs);
            assert_eq!(
                chat_pixels(&h),
                before_pixels,
                "same manager's Stage restores"
            );
            assert_eq!(h.state.input_buffer, "composer 界\nsecond row");
            assert_eq!(h.state.cursor_position, 3);
            h.state.focus = FocusState::Chat;
            assert_eq!(
                key(&mut h, &mut tabs, Key::Char('k')),
                InputAction::ScrollLineUp
            );
            let conversation = tabs.active_tab().conversation.clone();
            rustain::adapters::tui::handlers::scroll::handle_apply_scroll_intent(
                &mut tabs,
                &mut h.state,
                &conversation,
                rustain::domain::models::ScrollDelta::LineUp,
            );
            h.render_active_tab(&tabs);
            assert_ne!(
                view(&tabs),
                before_view,
                "same manager's independent navigation control"
            );
            assert_ne!(
                chat_pixels(&h),
                before_pixels,
                "the rendered owner really moves"
            );
        }
    }
}

#[test]
fn search_caret_only_edits_never_rescan_but_content_keeps_both_calm_jumps() {
    let (mut h, mut tabs) = stage(80, 24);
    h.state.focus = FocusState::Input;
    h.render_active_tab(&tabs);
    let before_view = view(&tabs);
    let before_pixels = chat_pixels(&h);
    key(&mut h, &mut tabs, Key::Ctrl('f'));
    h.state.input_buffer = "untouched Search composer".into();
    paste(&mut h, &mut tabs, "-row-00");
    assert_eq!(h.state.search_state.matches.len(), 3);
    assert_ne!(
        view(&tabs),
        before_view,
        "0 -> matches navigates the owning view"
    );
    key(&mut h, &mut tabs, Key::Enter);
    key(&mut h, &mut tabs, Key::Char('n'));
    let middle = view(&tabs);
    assert_eq!(
        h.state.search_state.matches[h.state.search_state.focused_match_index].message_index,
        1
    );
    key(&mut h, &mut tabs, Key::Home);
    assert_eq!(
        view(&tabs),
        middle,
        "Navigating -> Typing caret motion does not navigate"
    );
    paste(&mut h, &mut tabs, "stage-0");
    assert_ne!(
        view(&tabs),
        middle,
        "loss of the focused match still makes its calm jump"
    );
    assert_eq!(
        h.state.search_state.matches[h.state.search_state.focused_match_index].message_index,
        0
    );
    let matched = view(&tabs);
    h.render_active_tab(&tabs);
    let matched_pixels = chat_pixels(&h);
    assert_ne!(
        matched_pixels, before_pixels,
        "actual rendered Stage changes, not a detached view"
    );
    let instant = h.state.search_state.last_search_instant;
    let keys = cache_keys(&h, &tabs);
    for edit in [
        Key::Home,
        Key::Ctrl('w'),
        Key::Ctrl('u'),
        Key::Ctrl('h'),
        Key::Left,
        Key::Right,
        Key::Left,
        Key::Ctrl('e'),
        Key::Right,
        Key::Delete,
        Key::Ctrl('k'),
    ] {
        assert_eq!(
            key(&mut h, &mut tabs, edit),
            InputAction::Consumed,
            "{edit:?}"
        );
        assert_eq!(h.state.search_state.query.as_str(), "stage-0-row-00");
        assert_eq!(
            h.state.search_state.last_search_instant, instant,
            "{edit:?}: no rescan"
        );
        assert_eq!(view(&tabs), matched, "{edit:?}: no Stage navigation");
        h.render_active_tab(&tabs);
        assert_eq!(chat_pixels(&h), matched_pixels);
        assert_eq!(cache_keys(&h, &tabs), keys);
    }
    key(&mut h, &mut tabs, Key::Ctrl('u'));
    assert!(h.state.search_state.query.is_empty());
    assert!(
        h.state.search_state.matches.is_empty(),
        "the actual effect clears obsolete highlights"
    );
    assert_eq!(view(&tabs), matched, "clearing alone does not jump");
    paste(&mut h, &mut tabs, "stage-2-row-00");
    assert_ne!(
        view(&tabs),
        matched,
        "0 -> matches still jumps after editing"
    );
    key(&mut h, &mut tabs, Key::Esc);
    assert_eq!(
        view(&tabs),
        before_view,
        "Esc restores the complete owning ViewState"
    );
    h.render_active_tab(&tabs);
    assert_eq!(chat_pixels(&h), before_pixels);
    assert_eq!(h.state.input_buffer, "untouched Search composer");
}

#[test]
fn edited_fields_defer_the_exact_request_and_release_it_on_actual_close() {
    use super::k2_boundary::{
        assert_permission_deferred, assert_permission_painted, request_permission,
    };
    for field in FIELDS {
        let (mut h, mut tabs) = stage(80, 24);
        h.state.focus = FocusState::Input;
        h.state.input_history.push("history control".into());
        field.open_on_stage(&mut h, &mut tabs);
        paste(&mut h, &mut tabs, "ab");
        key(&mut h, &mut tabs, Key::Home);
        key(&mut h, &mut tabs, Key::Right);
        let saved = field.value(&h.state).clone();
        let id = request_permission(&mut h.state);
        assert_permission_deferred(&h.state, &id);
        key(&mut h, &mut tabs, Key::Ctrl('x'));
        assert!(h.state.pending_leader.is_some());
        assert_permission_deferred(&h.state, &id);
        key(&mut h, &mut tabs, Key::Esc);
        assert_eq!(field.value(&h.state), &saved);
        if matches!(field, Field::Reverse) {
            assert!(
                !h.state.reverse_search.is_open(),
                "legacy leader opening dismisses ReverseSearch"
            );
            assert_permission_painted(&mut h, &tabs, &id);
            continue;
        }
        let frame = h.state.modal_stack.top().unwrap().clone();
        key(&mut h, &mut tabs, Key::Ctrl('x'));
        let cover = if field == Field::Help {
            Surface::NotificationLog
        } else {
            Surface::Help
        };
        key(
            &mut h,
            &mut tabs,
            Key::Char(if field == Field::Help { 'n' } else { '?' }),
        );
        assert_eq!(h.state.modal_stack.top().unwrap().surface, cover);
        assert_permission_deferred(&h.state, &id);
        key(&mut h, &mut tabs, Key::Esc);
        assert_eq!(h.state.modal_stack.top(), Some(&frame));
        super::k3_compose::open_quit(&mut h.state);
        assert_permission_deferred(&h.state, &id);
        key(&mut h, &mut tabs, Key::Char('n'));
        assert_eq!(h.state.modal_stack.top(), Some(&frame));
        assert_eq!(
            field.value(&h.state),
            &saved,
            "covered caret survives exactly"
        );
        assert_permission_deferred(&h.state, &id);
        key(&mut h, &mut tabs, Key::Esc);
        if field.inspector() {
            assert_eq!(h.state.modal_stack.top(), Some(&frame));
            assert_permission_deferred(&h.state, &id);
            key(&mut h, &mut tabs, Key::Esc);
        }
        assert_permission_painted(&mut h, &tabs, &id);
    }
}

fn h_cross_results(state: &mut TuiState) {
    state.cross_search.results = (0..3)
        .map(
            |n| rustain::domain::services::cross_search::CrossSearchResult {
                conversation_id: format!("cross-{n}"),
                title: format!("result {n}"),
                excerpt: "ab".into(),
                timestamp: 0,
                first_match_message_index: 0,
            },
        )
        .collect();
    state.cross_search.selected = 1;
}

#[tokio::test]
async fn model_query_control_keeps_the_real_owning_stage_and_composer() {
    use rustain::adapters::tui::handlers::scroll;
    use rustain::domain::models::ScrollDelta;
    for (width, height) in [(60, 16), (80, 24), (130, 30)] {
        let (mut h, mut tabs) = stage(width, height);
        h.state.focus = FocusState::Input;
        h.state.input_buffer = "model query must not edit 界\nsecond row".into();
        h.state.cursor_position = 3;
        h.render_active_tab(&tabs);
        let before = (view(&tabs), cache_keys(&h, &tabs), chat_pixels(&h));
        let layout = super::k3_compose::layout(&h.state);
        super::k7_surfaces::open(&mut h.state, Surface::ModelSelector).await;
        let owner = h.state.modal_stack.top().unwrap().clone();
        assert_eq!(h.state.model_selector.columns[0].models.len(), 2);
        key(&mut h, &mut tabs, Key::Char('/'));
        assert!(
            h.state.model_selector.search_active,
            "legacy search is reachable"
        );
        for c in "one".chars() {
            key(&mut h, &mut tabs, Key::Char(c));
        }
        assert_eq!(h.state.model_selector.search_query, "one");
        assert_eq!(
            h.state.model_selector.filtered_indices,
            vec![0],
            "observable filtering control, not an inert input path"
        );
        for caret_only in [Key::Home, Key::Ctrl('a'), Key::Ctrl('e'), Key::Ctrl('k')] {
            key(&mut h, &mut tabs, caret_only);
            assert_eq!(
                h.state.model_selector.search_query, "one",
                "ModelSelector retains its legacy no-caret field"
            );
        }
        let provider = h.state.model_selector.selected_provider;
        key(&mut h, &mut tabs, Key::Right);
        assert_ne!(
            h.state.model_selector.selected_provider, provider,
            "legacy Right changes provider, not a query caret"
        );
        assert!(!h.state.model_selector.search_active);
        assert!(h.state.model_selector.search_query.is_empty());
        h.render_active_tab(&tabs);
        assert_eq!(view(&tabs), before.0);
        assert_eq!(cache_keys(&h, &tabs), before.1);
        key(&mut h, &mut tabs, Key::Left);
        assert_eq!(h.state.model_selector.selected_provider, provider);
        key(&mut h, &mut tabs, Key::Char('/'));
        for c in "one".chars() {
            key(&mut h, &mut tabs, Key::Char(c));
        }
        assert_eq!(h.state.model_selector.search_query, "one");
        h.render_active_tab(&tabs);
        assert_eq!(view(&tabs), before.0);
        assert_eq!(cache_keys(&h, &tabs), before.1);
        assert_eq!(
            super::k3_compose::layout(&h.state).chat_pane,
            layout.chat_pane
        );
        assert_eq!(
            super::k3_compose::layout(&h.state).input_area,
            layout.input_area
        );
        assert_eq!(h.state.modal_stack.top(), Some(&owner));
        assert_eq!(
            h.state.input_buffer,
            "model query must not edit 界\nsecond row"
        );
        assert_eq!(h.state.cursor_position, 3);
        key(&mut h, &mut tabs, Key::Esc);
        assert_eq!(
            h.state.modal_stack.top(),
            Some(&owner),
            "legacy search back first"
        );
        key(&mut h, &mut tabs, Key::Esc);
        assert!(h.state.modal_stack.is_empty());
        h.render_active_tab(&tabs);
        assert_eq!(view(&tabs), before.0);
        assert_eq!(cache_keys(&h, &tabs), before.1);
        assert_eq!(chat_pixels(&h), before.2);

        key(&mut h, &mut tabs, Key::Esc);
        assert_eq!(h.state.focus, FocusState::Chat);
        assert_eq!(
            key(&mut h, &mut tabs, Key::Char('k')),
            InputAction::ScrollLineUp
        );
        let conversation = tabs.active_tab().conversation.clone();
        scroll::handle_apply_scroll_intent(
            &mut tabs,
            &mut h.state,
            &conversation,
            ScrollDelta::LineUp,
        );
        h.render_active_tab(&tabs);
        assert_ne!(
            view(&tabs),
            before.0,
            "same manager really navigates when asked"
        );
        assert_ne!(
            chat_pixels(&h),
            before.2,
            "production render observes that move"
        );
    }
}

#[test]
fn empty_inspector_editor_protects_typing_even_without_recent_printable_or_draft() {
    use rustain::adapters::tui::presentation;
    for field in [Field::Log, Field::Help] {
        let (mut h, mut tabs) = stage(80, 24);
        h.state.focus = FocusState::Chat;
        key(&mut h, &mut tabs, Key::Ctrl('x'));
        key(
            &mut h,
            &mut tabs,
            Key::Char(if field == Field::Log { 'n' } else { '?' }),
        );
        h.state.last_printable_at = None;
        assert!(h.state.input_buffer.is_empty());
        assert!(
            presentation::arrival_may_preempt(&h.state, h.state.notifications.now()),
            "{field:?}: idle list is the independent arrival control"
        );
        key(&mut h, &mut tabs, Key::Char('/'));
        h.state.last_printable_at = None;
        assert!(
            field.value(&h.state).is_empty(),
            "empty field is genuinely active"
        );
        assert!(
            !presentation::arrival_may_preempt(&h.state, h.state.notifications.now()),
            "{field:?}: the actual child context protects input, not a draft/timer"
        );
        key(&mut h, &mut tabs, Key::Enter);
        h.state.last_printable_at = None;
        assert!(
            presentation::arrival_may_preempt(&h.state, h.state.notifications.now()),
            "{field:?}: accepting returns to the non-typing list"
        );
    }
}

#[test]
fn accepted_help_filter_scrolls_its_own_list_and_restores_the_original_pixels() {
    let (mut h, mut tabs) = stage(60, 16);
    h.state.input_buffer = ["retained draft"; 8].join("\n");
    key(&mut h, &mut tabs, Key::Ctrl('x'));
    key(&mut h, &mut tabs, Key::Char('?'));
    for _ in 0..3 {
        key(&mut h, &mut tabs, Key::Char('j'));
    }
    h.state.help_overlay.selected_category = 2;
    h.render_active_tab(&tabs);
    let original = h.terminal.backend().buffer().clone();
    let owning_view = view(&tabs);
    key(&mut h, &mut tabs, Key::Char('/'));
    paste(&mut h, &mut tabs, "scroll");
    key(&mut h, &mut tabs, Key::Enter);
    assert!(!h.state.help_overlay.search.editing);
    h.render_active_tab(&tabs);
    let filtered = h.terminal.backend().buffer().clone();
    key(&mut h, &mut tabs, Key::Char('j'));
    h.render_active_tab(&tabs);
    assert_eq!(h.state.help_overlay.filtered_scroll_offset, 1);
    assert_ne!(
        h.terminal.backend().buffer(),
        &filtered,
        "actual matching rows move in the fitted sheet"
    );
    for up in [Key::Char('k'), Key::Up] {
        key(&mut h, &mut tabs, Key::Char('G'));
        h.render_active_tab(&tabs);
        let bottom = h.terminal.backend().buffer().clone();
        let end = h.state.help_overlay.filtered_scroll_offset;
        assert_ne!(&bottom, &filtered, "end jump actually changes visible rows");
        key(&mut h, &mut tabs, up);
        h.render_active_tab(&tabs);
        assert_ne!(
            h.terminal.backend().buffer(),
            &bottom,
            "{up:?} moves upward immediately after the filtered end jump"
        );
        assert_eq!(h.state.help_overlay.filtered_scroll_offset, end - 1);
        key(&mut h, &mut tabs, Key::Char('G'));
        for _ in 0..8 {
            key(&mut h, &mut tabs, Key::Char('j'));
        }
        h.render_active_tab(&tabs);
        assert_eq!(h.state.help_overlay.filtered_scroll_offset, end);
        assert_eq!(
            h.terminal.backend().buffer(),
            &bottom,
            "downward overscroll is bounded"
        );
    }
    key(&mut h, &mut tabs, Key::Char('g'));
    key(&mut h, &mut tabs, Key::Char('j'));
    assert_eq!(h.state.help_overlay.scroll_offset, 3);
    assert_eq!(h.state.help_overlay.selected_category, 2);
    key(&mut h, &mut tabs, Key::Char('/'));
    key(&mut h, &mut tabs, Key::Home);
    key(&mut h, &mut tabs, Key::Right);
    assert_eq!(
        h.state.help_overlay.filtered_scroll_offset, 1,
        "caret-only edit preserves list position"
    );
    key(&mut h, &mut tabs, Key::Char('x'));
    assert_eq!(
        h.state.help_overlay.filtered_scroll_offset, 0,
        "content change resets only filtered scrolling"
    );
    key(&mut h, &mut tabs, Key::Esc);
    h.render_active_tab(&tabs);
    assert_eq!(
        h.terminal.backend().buffer(),
        &original,
        "same original Help pixels, not merely an unchanged counter"
    );
    assert_eq!(view(&tabs), owning_view);
}

#[test]
fn clearing_log_filter_keeps_the_retained_selected_record_in_the_fitted_window() {
    use rustain::adapters::tui::notifications::{self, NoticeSource};
    let (mut h, mut tabs) = stage(60, 16);
    h.state.input_buffer = ["retained draft"; 8].join("\n");
    let owning_view = view(&tabs);
    notifications::error(&mut h.state, NoticeSource::System, "retained failure");
    let retained = h.state.notifications.records().next_back().unwrap().id;
    notifications::info(&mut h.state, NoticeSource::System, "ordinary arrival");
    key(&mut h, &mut tabs, Key::Ctrl('x'));
    key(&mut h, &mut tabs, Key::Char('n'));
    key(&mut h, &mut tabs, Key::Char('j'));
    let owner = h.state.modal_stack.top().unwrap().clone();
    assert_eq!(
        h.state.notification_log.as_ref().unwrap().selected_id,
        Some(retained)
    );
    key(&mut h, &mut tabs, Key::Char('/'));
    paste(&mut h, &mut tabs, "eRr");
    h.render_active_tab(&tabs);
    assert!(
        h.screen_text().contains("retained failure"),
        "real severity-filter control"
    );
    paste(&mut h, &mut tabs, "no-match");
    h.render_active_tab(&tabs);
    assert!(h.screen_text().contains("No matching notifications."));
    key(&mut h, &mut tabs, Key::Esc);
    assert_eq!(h.state.modal_stack.top(), Some(&owner));
    assert_eq!(
        h.state.notification_log.as_ref().unwrap().selected_id,
        Some(retained)
    );
    h.render_active_tab(&tabs);
    let screen = h.screen_text();
    assert!(
        screen.contains("retained failure"),
        "the retained id's actual row must remain visible: {screen}"
    );
    assert!(
        !screen.contains("ordinary arrival"),
        "only the selected row fits this sheet"
    );
    assert_eq!(view(&tabs), owning_view);
    assert_eq!(h.state.input_buffer, ["retained draft"; 8].join("\n"));
}

#[test]
fn review_bb2b_sigma_log_filter_uses_real_dispatch_and_paint() {
    use rustain::adapters::tui::notifications::{self, NoticeSource};
    for (message, query) in [("ΟΣ", "ος"), ("ος", "ΟΣ"), ("σ", "ς"), ("ς", "σ")] {
        let (mut h, mut tabs) = stage(60, 16);
        h.state.input_buffer = ["retained draft"; 8].join("\n");
        let owning_view = view(&tabs);
        notifications::info(
            &mut h.state,
            NoticeSource::System,
            format!("Greek {message} record"),
        );
        let retained = h.state.notifications.records().next_back().unwrap().id;
        notifications::info(&mut h.state, NoticeSource::System, "independent decoy");
        key(&mut h, &mut tabs, Key::Ctrl('x'));
        key(&mut h, &mut tabs, Key::Char('n'));
        h.render_active_tab(&tabs);
        let unfiltered = h.terminal.backend().buffer().clone();
        key(&mut h, &mut tabs, Key::Char('/'));
        paste(&mut h, &mut tabs, query);
        h.render_active_tab(&tabs);
        let painted: String = h
            .terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            painted.contains(&format!("Greek {message} record")),
            "{message:?}/{query:?}: {painted}"
        );
        assert!(!painted.contains("independent decoy"));
        assert!(!painted.contains("No matching notifications."));
        assert_ne!(
            h.terminal.backend().buffer(),
            &unfiltered,
            "filter changes actual rows"
        );
        assert_eq!(
            h.state.notification_log.as_ref().unwrap().selected_id,
            Some(retained)
        );
        key(&mut h, &mut tabs, Key::Home);
        key(&mut h, &mut tabs, Key::Ctrl('k'));
        paste(&mut h, &mut tabs, "not-a-match");
        h.render_active_tab(&tabs);
        let absent: String = h
            .terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(absent.contains("No matching notifications."));
        assert!(
            !absent.contains("Greek "),
            "independent nonmatching production control"
        );
        assert_eq!(view(&tabs), owning_view);
    }
}

fn review_bb2b_help_payload_fixture(
    lower: &[Surface],
) -> (crate::e2e_harness::TestHarness, TabManager) {
    use rustain::adapters::tui::presentation::reducer;
    let (mut h, mut tabs) = stage(60, 16);
    h.state.focus = FocusState::Input;
    h.state.input_buffer = ["retained draft"; 8].join("\n");
    for &surface in lower {
        reducer::open(&mut h.state, surface);
    }
    reducer::open(&mut h.state, Surface::Help);
    h.state.help_overlay.scroll_offset = 3;
    h.state.help_overlay.selected_category = 2;
    key(&mut h, &mut tabs, Key::Char('/'));
    paste(&mut h, &mut tabs, "scroll");
    key(&mut h, &mut tabs, Key::Enter);
    key(&mut h, &mut tabs, Key::Char('j'));
    assert_eq!(h.state.help_overlay.filtered_scroll_offset, 1);
    key(&mut h, &mut tabs, Key::Char('/'));
    key(&mut h, &mut tabs, Key::Home);
    key(&mut h, &mut tabs, Key::Right);
    assert!(h.state.help_overlay.search.editing);
    assert_eq!(h.state.help_overlay.search.query.as_str(), "scroll");
    (h, tabs)
}

#[test]
fn review_bb2b_help_payload_is_released_after_accepted_filter_question_close() {
    let (mut h, mut tabs) = review_bb2b_help_payload_fixture(&[]);
    let owning_view = view(&tabs);
    key(&mut h, &mut tabs, Key::Enter);
    assert!(!h.state.help_overlay.search.editing);
    assert!(!h.state.help_overlay.search.query.is_empty());
    key(&mut h, &mut tabs, Key::Char('?'));
    assert!(!rustain::adapters::tui::presentation::is_open(
        &h.state,
        Surface::Help
    ));
    assert_eq!(h.state.help_overlay.search, Default::default());
    assert_eq!(h.state.help_overlay.filtered_scroll_offset, 0);
    assert_eq!(
        (
            h.state.help_overlay.scroll_offset,
            h.state.help_overlay.selected_category
        ),
        (3, 2)
    );
    assert_eq!(view(&tabs), owning_view);
}

#[test]
fn review_bb2b_help_payload_is_released_on_truncation_replacement_and_continuation_discard() {
    use rustain::adapters::tui::presentation::{self, reducer};
    let full = [
        Surface::CommandPalette,
        Surface::ModelSelector,
        Surface::NotificationLog,
    ];
    let mut reached = 0;
    for mode in 0..3 {
        let lower: &[Surface] = if mode == 0 { &full[..1] } else { &full };
        let (mut h, mut tabs) = review_bb2b_help_payload_fixture(lower);
        let owning_view = view(&tabs);
        if mode == 1 {
            reducer::open(&mut h.state, Surface::ProfileSwitcher);
        } else {
            if mode == 2 {
                review_bb2b_request_guarded_quit(&mut h, &mut tabs);
                assert!(presentation::is_open(&h.state, Surface::Help));
                assert!(
                    !h.state
                        .modal_stack
                        .iter()
                        .any(|f| f.surface == Surface::Help)
                );
            }
            reducer::open(&mut h.state, Surface::CommandPalette);
        }
        assert!(
            !presentation::is_open(&h.state, Surface::Help),
            "mode={mode}"
        );
        assert_eq!(
            h.state.help_overlay.search,
            Default::default(),
            "mode={mode}"
        );
        assert_eq!(
            h.state.help_overlay.filtered_scroll_offset, 0,
            "mode={mode}"
        );
        assert_eq!(
            (
                h.state.help_overlay.scroll_offset,
                h.state.help_overlay.selected_category
            ),
            (3, 2)
        );
        assert_eq!(view(&tabs), owning_view);
        reached += 1;
    }
    assert_eq!(reached, 3, "all release paths are actually reached");
}

#[test]
fn review_bb2b_help_payload_is_retained_exactly_under_quit_cover_and_continuation() {
    let full = [
        Surface::CommandPalette,
        Surface::ModelSelector,
        Surface::NotificationLog,
    ];
    let mut reached = 0;
    for lower in [&[][..], &full[..]] {
        let (mut h, mut tabs) = review_bb2b_help_payload_fixture(lower);
        let owner = h.state.modal_stack.top().unwrap().clone();
        let search = h.state.help_overlay.search.clone();
        let owning_view = view(&tabs);
        h.render_active_tab(&tabs);
        let original = h.terminal.backend().buffer().clone();
        review_bb2b_request_guarded_quit(&mut h, &mut tabs);
        assert_eq!(
            h.state.modal_stack.top().unwrap().surface,
            Surface::QuitConfirm
        );
        assert_eq!(h.state.help_overlay.search, search);
        assert_eq!(h.state.help_overlay.filtered_scroll_offset, 1);
        assert_eq!(
            h.state
                .modal_stack
                .iter()
                .any(|f| f.surface == Surface::Help),
            lower.is_empty()
        );
        key(&mut h, &mut tabs, Key::Char('n'));
        assert_eq!(h.state.modal_stack.top(), Some(&owner));
        assert_eq!(h.state.help_overlay.search, search);
        assert_eq!(h.state.help_overlay.filtered_scroll_offset, 1);
        h.render_active_tab(&tabs);
        assert_eq!(h.terminal.backend().buffer(), &original);
        assert_eq!(view(&tabs), owning_view);
        reached += 1;
    }
    assert_eq!(reached, 2, "both cover and actual continuation reached");
}

fn review_bb2b_request_guarded_quit(
    h: &mut crate::e2e_harness::TestHarness,
    tabs: &mut TabManager,
) {
    use rustain::adapters::tui::quit_confirm::{self, QuitFacts, QuitHost};
    assert_eq!(key(h, tabs, Key::Ctrl('q')), InputAction::Quit);
    let facts = QuitFacts {
        composer_draft: !h.state.input_buffer.is_empty(),
        ..QuitFacts::idle(QuitHost::Local)
    };
    assert!(
        facts.composer_draft,
        "actual retained composer arms the guard"
    );
    quit_confirm::request(&mut h.state, facts);
}
