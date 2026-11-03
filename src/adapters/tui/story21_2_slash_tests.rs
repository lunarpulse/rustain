//! Story 21-2 AC4 (K4): slash-popup edit ownership through the real key ingress.

use crate::adapters::tui::app::{InputAction, handle_input};
use crate::adapters::tui::state::TuiState;
use crate::domain::events::{DomainInputEvent, DomainKey};
use crate::domain::models::FocusState;
use crate::domain::models::autocomplete::{AutocompleteKind, AutocompleteSuggestion};

fn key(state: &mut TuiState, c: char) -> InputAction {
    handle_input(state, &DomainInputEvent::KeyPress(c))
}

fn special(state: &mut TuiState, k: DomainKey) -> InputAction {
    handle_input(state, &DomainInputEvent::SpecialKey(k))
}

fn type_text(state: &mut TuiState, text: &str) {
    for c in text.chars() {
        key(state, c);
    }
}

fn help_suggestion() -> AutocompleteSuggestion {
    AutocompleteSuggestion::SlashCommand {
        name: "help".to_owned(),
        description: "Show help".to_owned(),
    }
}

#[test]
fn story21_2_lone_slash_then_esc_leaves_no_residue_and_only_closes_popup() {
    let mut state = TuiState::new(80, 24);
    state.input_scroll_offset = 3;
    key(&mut state, '/');
    assert!(state.autocomplete.is_open());
    assert!(state.autocomplete.owns_slash_edit());
    assert_eq!(state.input_buffer, "/");

    let action = special(&mut state, DomainKey::Esc);

    // Esc closes only the popup: no submit/quit/cancel, focus unchanged.
    assert_eq!(action, InputAction::Consumed);
    assert_eq!(state.focus, FocusState::Input);
    assert!(!state.autocomplete.is_open());
    assert!(!state.autocomplete.owns_slash_edit());
    assert_eq!(state.input_buffer, "");
    assert_eq!(state.cursor_position, 0);
    assert_eq!(state.input_scroll_offset, 3);
}

#[test]
fn story21_2_slash_with_typed_filter_then_esc_restores_saved_composer() {
    let mut state = TuiState::new(80, 24);
    key(&mut state, '/');
    type_text(&mut state, "hel");
    assert_eq!(state.input_buffer, "/hel");
    assert_eq!(state.autocomplete.filter_text, "hel");

    // Backspace inside the filter keeps ownership (still provisional).
    special(&mut state, DomainKey::Backspace);
    assert_eq!(state.input_buffer, "/he");
    assert!(state.autocomplete.owns_slash_edit());

    assert_eq!(special(&mut state, DomainKey::Esc), InputAction::Consumed);
    assert_eq!(state.input_buffer, "");
    assert_eq!(state.cursor_position, 0);
    assert!(!state.autocomplete.is_open());
}

#[test]
fn story21_2_esc_restores_mid_buffer_unicode_prefix_and_suffix_exactly() {
    // The production trigger opens only on an empty composer, so the generic
    // snapshot is exercised with a hand-built popup-owned provisional edit:
    // saved "é日本" (cursor 1, scroll 2) -> live "é/he日本" (cursor 4).
    let mut state = TuiState::new(80, 24);
    state
        .autocomplete
        .open(AutocompleteKind::SlashCommand, 1, Vec::new());
    state.autocomplete.own_slash_edit("é日本", 1, 2);
    state.autocomplete.filter_text = "he".to_owned();
    state.input_buffer = "é/he日本".to_owned();
    state.cursor_position = 4;
    state.input_scroll_offset = 0;

    assert_eq!(special(&mut state, DomainKey::Esc), InputAction::Consumed);

    assert_eq!(state.input_buffer, "é日本");
    assert_eq!(state.cursor_position, 1);
    assert_eq!(state.input_scroll_offset, 2);
    assert!(!state.autocomplete.is_open());
}

#[test]
fn story21_2_stale_snapshot_is_never_restored_after_unrelated_edit() {
    // Pasted text while the popup is open.
    let mut state = TuiState::new(80, 24);
    key(&mut state, '/');
    handle_input(&mut state, &DomainInputEvent::Paste("pasted".to_owned()));
    assert!(!state.autocomplete.owns_slash_edit());
    special(&mut state, DomainKey::Esc);
    assert_eq!(state.input_buffer, "/pasted");

    // Cursor relocation that bypasses the key path (e.g. pointer): the live
    // composer no longer matches the provisional edit.
    let mut state = TuiState::new(80, 24);
    key(&mut state, '/');
    type_text(&mut state, "he");
    state.input_buffer = "/hexyz".to_owned();
    state.cursor_position = 6;
    special(&mut state, DomainKey::Esc);
    assert_eq!(state.input_buffer, "/hexyz");
    assert!(!state.autocomplete.is_open());

    // Cursor moved with a navigation key: popup dismissed, text is authored.
    let mut state = TuiState::new(80, 24);
    key(&mut state, '/');
    type_text(&mut state, "h");
    special(&mut state, DomainKey::Left);
    assert!(!state.autocomplete.is_open());
    assert!(!state.autocomplete.owns_slash_edit());
    special(&mut state, DomainKey::Esc);
    assert_eq!(state.input_buffer, "/h");

    // Space ends the command name: arguments follow, text is authored.
    let mut state = TuiState::new(80, 24);
    key(&mut state, '/');
    type_text(&mut state, "export ");
    assert!(!state.autocomplete.owns_slash_edit());
    special(&mut state, DomainKey::Esc);
    assert_eq!(state.input_buffer, "/export ");
}

#[test]
fn story21_2_accepted_completion_is_intentional_text_esc_does_not_erase_it() {
    for accept in [DomainKey::Tab, DomainKey::Enter] {
        let mut state = TuiState::new(80, 24);
        key(&mut state, '/');
        type_text(&mut state, "he");
        state.autocomplete.suggestions = vec![help_suggestion()];

        let action = special(&mut state, accept);
        assert_eq!(action, InputAction::Consumed, "accept via {accept:?}");
        // Positive control: completion replaced trigger..cursor with /name.
        assert_eq!(state.input_buffer, "/help");
        assert_eq!(state.cursor_position, 5);
        assert!(!state.autocomplete.is_open());
        assert!(!state.autocomplete.owns_slash_edit());

        special(&mut state, DomainKey::Esc);
        assert_eq!(
            state.input_buffer, "/help",
            "Esc after {accept:?} must keep text"
        );
        assert_eq!(state.cursor_position, 5);
    }
}

#[test]
fn story21_2_mention_popup_never_acquires_rollback_ownership() {
    let mut state = TuiState::new(80, 24);
    type_text(&mut state, "see @");
    assert!(state.autocomplete.is_open());
    assert_eq!(state.autocomplete.kind, Some(AutocompleteKind::FileMention));
    assert!(!state.autocomplete.owns_slash_edit());
    type_text(&mut state, "sr");
    assert!(!state.autocomplete.owns_slash_edit());

    assert_eq!(special(&mut state, DomainKey::Esc), InputAction::Consumed);
    assert!(!state.autocomplete.is_open());
    assert_eq!(state.input_buffer, "see @sr", "mention text stays authored");
    assert_eq!(state.cursor_position, 7);

    // A mention opened right after a slash popup was cancelled starts clean.
    let mut state = TuiState::new(80, 24);
    key(&mut state, '/');
    special(&mut state, DomainKey::Esc);
    key(&mut state, '@');
    assert!(!state.autocomplete.owns_slash_edit());
    special(&mut state, DomainKey::Esc);
    assert_eq!(state.input_buffer, "@");
}

#[test]
fn story21_2_pasted_or_literal_slash_text_has_no_rollback_ownership() {
    let mut state = TuiState::new(80, 24);
    handle_input(&mut state, &DomainInputEvent::Paste("/help me".to_owned()));
    assert!(!state.autocomplete.is_open());
    assert!(!state.autocomplete.owns_slash_edit());
    special(&mut state, DomainKey::Esc);
    assert_eq!(state.input_buffer, "/help me");

    // A slash typed into existing text is literal: no popup, no ownership.
    let mut state = TuiState::new(80, 24);
    type_text(&mut state, "a/b");
    assert!(!state.autocomplete.is_open());
    assert!(!state.autocomplete.owns_slash_edit());
    assert_eq!(state.input_buffer, "a/b");
}

#[test]
fn story21_2_backspace_of_trigger_keeps_explicit_removal_behaviour() {
    let mut state = TuiState::new(80, 24);
    key(&mut state, '/');
    assert_eq!(
        special(&mut state, DomainKey::Backspace),
        InputAction::Consumed
    );
    assert_eq!(state.input_buffer, "");
    assert_eq!(state.cursor_position, 0);
    assert!(!state.autocomplete.is_open());
    assert!(!state.autocomplete.owns_slash_edit());
}

#[test]
fn story21_2_submit_and_dismissal_clear_ownership() {
    let mut state = TuiState::new(80, 24);
    key(&mut state, '/');
    state.autocomplete.dismiss();
    assert!(!state.autocomplete.owns_slash_edit());

    let mut state = TuiState::new(80, 24);
    key(&mut state, '/');
    type_text(&mut state, "new ");
    assert!(!state.autocomplete.owns_slash_edit());
    // Submit goes through the normal slash dispatch (not a rollback).
    assert_eq!(
        special(&mut state, DomainKey::Enter),
        InputAction::ExecuteCommand {
            name: "new".to_owned(),
            args: None
        }
    );
    assert!(!state.autocomplete.owns_slash_edit());
}
