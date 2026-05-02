//! Story 16.6 AC9 — No keymap conflicts.
//!
//! Exhaustive table assertion: every S16.6 binding is dispatched correctly
//! when `state.focus == FocusState::Chat`, AND no overlapping legacy binding fires.

use rustain::adapters::tui::app::{InputAction, handle_input};
use rustain::adapters::tui::state::TuiState;
use rustain::domain::events::{DomainInputEvent, DomainKey};
use rustain::domain::models::FocusState;

fn make_state() -> TuiState {
    TuiState::new(80, 24)
}

fn key(c: char) -> DomainInputEvent {
    DomainInputEvent::KeyPress(c)
}

fn sp(key: DomainKey) -> DomainInputEvent {
    DomainInputEvent::SpecialKey(key)
}

#[test]
fn legacy_chat_keys_are_unchanged() {
    let legacy_keys: &[(DomainInputEvent, fn() -> InputAction)] = &[
        (key('j'), || InputAction::Consumed),
        (key('k'), || InputAction::Consumed),
        (key('J'), || InputAction::Consumed),
        (key('K'), || InputAction::Consumed),
        (key('{'), || InputAction::Consumed),
        (key('}'), || InputAction::Consumed),
        (key('g'), || InputAction::Consumed),
        (key('c'), || InputAction::CopyToClipboard(String::new())),
        (key('p'), || InputAction::Consumed),
        (key('i'), || InputAction::Consumed),
        (key('q'), || InputAction::Quit),
        (key('f'), || InputAction::ForkAtMessage),
        (key('R'), || InputAction::RewindAtMessage),
        (key('m'), || InputAction::ToggleBookmark),
        (key('\''), || InputAction::OpenBookmarkList),
        (key('?'), || InputAction::Consumed),
        (key('y'), || InputAction::Ignored), // no pending_large_image in test
        (key('n'), || InputAction::Ignored), // no pending_large_image in test
        (key('1'), || InputAction::SwitchToTab(1)),
    ];

    for (event, expected_fn) in legacy_keys {
        let mut state = make_state();
        state.focus = FocusState::Chat;
        let action = handle_input(&mut state, event);
        let expected = expected_fn();
        assert_eq!(
            action, expected,
            "Legacy key produced unexpected action"
        );
        assert!(!state.pending_z && state.pending_bracket.is_none(),
            "Legacy key should not set chord state");
    }
}

#[test]
fn vim_z_prefix_in_chat_only() {
    let mut state = make_state();
    state.focus = FocusState::Chat;
    handle_input(&mut state, &key('z'));
    assert!(state.pending_z, "z should set pending_z in Chat focus");

    let mut state2 = make_state();
    state2.focus = FocusState::Input;
    handle_input(&mut state2, &key('z'));
    assert!(!state2.pending_z, "z should NOT set pending_z in Input focus");
}

#[test]
fn g_capital_is_overridden() {
    let mut state = make_state();
    state.focus = FocusState::Chat;
    let action = handle_input(&mut state, &key('G'));
    assert_eq!(action, InputAction::JumpToLatestProseAnchor);
}

#[test]
fn tab_is_narrowly_overridden() {
    let mut state = make_state();
    state.focus = FocusState::Chat;
    let action = handle_input(&mut state, &sp(DomainKey::Tab));
    assert_eq!(action, InputAction::CycleInvocationInFocusedTurn);
}

#[test]
fn non_key_event_resets_vim_chord_state() {
    let mut state = make_state();
    state.focus = FocusState::Chat;
    handle_input(&mut state, &key('z'));
    assert!(state.pending_z);

    handle_input(&mut state, &DomainInputEvent::Resize(100, 30));
    assert!(!state.pending_z);
    assert!(state.pending_bracket.is_none());
}

#[test]
fn esc_resets_vim_chord_state() {
    let mut state = make_state();
    state.focus = FocusState::Chat;
    handle_input(&mut state, &key('z'));
    assert!(state.pending_z);

    handle_input(&mut state, &sp(DomainKey::Esc));
    assert!(!state.pending_z);
    assert!(state.pending_bracket.is_none());

    // Also verify that ESC resets bracket prefix
    let mut state2 = make_state();
    state2.focus = FocusState::Chat;
    handle_input(&mut state2, &key(']'));
    assert_eq!(state2.pending_bracket, Some(']'));

    handle_input(&mut state2, &sp(DomainKey::Esc));
    assert!(state2.pending_bracket.is_none());
    assert!(!state2.pending_z);
}

#[test]
fn chord_cancellation_absorbs_legacy_keys() {
    // Unfinished z-prefix followed by a legacy key should consume the legacy key
    let mut state = make_state();
    state.focus = FocusState::Chat;
    handle_input(&mut state, &key('z'));
    assert!(state.pending_z);
    let action = handle_input(&mut state, &key('1'));
    assert_eq!(action, InputAction::Consumed, "z1 should be consumed as cancelled chord");
    assert!(!state.pending_z);

    // Unfinished bracket-prefix followed by a legacy key
    let mut state2 = make_state();
    state2.focus = FocusState::Chat;
    handle_input(&mut state2, &key(']'));
    assert_eq!(state2.pending_bracket, Some(']'));
    let action2 = handle_input(&mut state2, &key('j'));
    assert_eq!(action2, InputAction::Consumed, "]j should be consumed as cancelled chord");
    assert!(state2.pending_bracket.is_none());
}

#[test]
fn vim_keys_not_in_which_key_chord_map() {
    let state = make_state();
    assert!(
        state.which_key.lookup_chord('z').is_none(),
        "z must not be a which-key chord"
    );
    assert!(
        state.which_key.lookup_chord(']').is_none(),
        "] must not be a which-key chord"
    );
    assert!(
        state.which_key.lookup_chord('[').is_none(),
        "[ must not be a which-key chord"
    );
    // Tab is a SpecialKey, not a char, so it cannot be in the chord_map by design.
}

#[test]
fn chord_state_defaults_false() {
    let state = make_state();
    assert!(!state.pending_z);
    assert!(state.pending_bracket.is_none());
}
