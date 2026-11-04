//! Shared fields are exercised through the real key boundary, never a helper-only editor.
use super::keymap_support::press;
use super::{
    k8_stage::search_key as key,
    stage_support::{cache_keys, chat_pixels, paste, stage, view},
};
use rustain::adapters::tui::{
    app::{InputAction, handle_input},
    keymap::Key,
    presentation::{Surface, text_field::TextField},
    state::TuiState,
};
use rustain::domain::events::DomainInputEvent;
use rustain::domain::models::FocusState;

#[derive(Clone, Copy, Debug)]
enum Field {
    Palette,
    Search,
    Reverse,
}

impl Field {
    fn open(self, state: &mut TuiState) {
        state.focus = FocusState::Input;
        press(
            state,
            Key::Ctrl(match self {
                Self::Palette => 'p',
                Self::Search => 'f',
                Self::Reverse => 'r',
            }),
        );
        match self {
            Self::Palette => assert_eq!(
                state.modal_stack.top().unwrap().surface,
                Surface::CommandPalette
            ),
            Self::Search => assert_eq!(state.modal_stack.top().unwrap().surface, Surface::Search),
            Self::Reverse => assert!(state.reverse_search.is_open()),
        }
    }

    fn text(self, state: &TuiState) -> &str {
        match self {
            Self::Palette => state.command_palette.filter_text.as_str(),
            Self::Search => state.search_state.query.as_str(),
            Self::Reverse => state.reverse_search.query.as_str(),
        }
    }

    fn value(self, state: &TuiState) -> &TextField {
        match self {
            Self::Palette => &state.command_palette.filter_text,
            Self::Search => &state.search_state.query,
            Self::Reverse => &state.reverse_search.query,
        }
    }

    fn open_key(self) -> Key {
        Key::Ctrl(match self {
            Self::Palette => 'p',
            Self::Search => 'f',
            Self::Reverse => 'r',
        })
    }
}

#[test]
fn insert_and_kill_line_at_the_caret_grapheme_safe() {
    const TAIL: &str = "👩‍👩‍👧‍👦界e\u{301} z";
    for field in [Field::Palette, Field::Search, Field::Reverse] {
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
    for field in [Field::Palette, Field::Search, Field::Reverse] {
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
            key(&mut h, &mut tabs, field.open_key());
            match field {
                Field::Palette => assert_eq!(
                    h.state.modal_stack.top().unwrap().surface,
                    Surface::CommandPalette
                ),
                Field::Search => {
                    assert_eq!(h.state.modal_stack.top().unwrap().surface, Surface::Search)
                }
                Field::Reverse => assert!(h.state.reverse_search.is_open()),
            }
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
                if matches!(field, Field::Search) && expected != "a\r\nb" {
                    InputAction::SearchQueryChanged
                } else {
                    InputAction::Consumed
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
    for field in [Field::Palette, Field::Search, Field::Reverse] {
        let (mut h, mut tabs) = stage(80, 24);
        h.state.focus = FocusState::Input;
        h.state.input_history.push("history control".into());
        key(&mut h, &mut tabs, field.open_key());
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
    for field in [Field::Palette, Field::Search, Field::Reverse] {
        for ch in (0x20u8..0x7f).map(char::from) {
            let mut state = TuiState::new(80, 24);
            field.open(&mut state);
            state.input_buffer = "draft".into();
            handle_input(&mut state, &DomainInputEvent::Paste("ab".into()));
            press(&mut state, Key::Home);
            press(&mut state, Key::Right);
            let frame = state.modal_stack.top().cloned();
            let action = press(&mut state, Key::Char(ch));
            assert_eq!(field.text(&state), format!("a{ch}b"), "{field:?}: {ch:?}");
            assert!(matches!(
                action,
                InputAction::Consumed | InputAction::SearchQueryChanged
            ));
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
        for field in [Field::Palette, Field::Search, Field::Reverse] {
            let (mut h, mut tabs) = stage(width, height);
            h.state.focus = FocusState::Input;
            h.state.input_history.push("history fixture".into());
            key(&mut h, &mut tabs, field.open_key());
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
fn palette_and_reverse_fields_do_not_navigate_the_owning_stage() {
    for field in [Field::Palette, Field::Reverse] {
        for (width, height) in [(60, 16), (80, 24), (130, 30)] {
            let (mut h, mut tabs) = stage(width, height);
            h.state.focus = FocusState::Input;
            h.state.input_buffer = "composer 界\nsecond row".into();
            h.state.cursor_position = 3;
            h.render_active_tab(&tabs);
            let (before_view, before_keys, before_pixels) =
                (view(&tabs), cache_keys(&h, &tabs), chat_pixels(&h));
            assert!(!before_keys.0.is_empty() || !before_keys.1.is_empty());
            let layout = super::k3_compose::layout(&h.state);
            key(&mut h, &mut tabs, field.open_key());
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
            h.render_active_tab(&tabs);
            assert_eq!(
                chat_pixels(&h),
                before_pixels,
                "same manager's Stage restores"
            );
            assert_eq!(h.state.input_buffer, "composer 界\nsecond row");
            assert_eq!(h.state.cursor_position, 3);
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
    for field in [Field::Palette, Field::Search, Field::Reverse] {
        let (mut h, mut tabs) = stage(80, 24);
        h.state.focus = FocusState::Input;
        h.state.input_history.push("history control".into());
        key(&mut h, &mut tabs, field.open_key());
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
        key(&mut h, &mut tabs, Key::Char('?'));
        assert_eq!(h.state.modal_stack.top().unwrap().surface, Surface::Help);
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
        assert_permission_painted(&mut h, &tabs, &id);
    }
}
