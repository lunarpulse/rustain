//! Story 16.8 AC11 — No-regression scroll contract.
//! S16.8 migrated j/k/g/G/J/K/{/} from direct mutation to InputAction emission.
//! P15: Behavioral tests updated — InputAction emission verification.
//! Note: Full scroll-math regression tests require `compute_scroll` which is
//! #[cfg(test)] at the crate level (not accessible from integration tests).
//! Those tests live in the event_loop.rs unit test module.

use rustain::adapters::tui::app::{InputAction, handle_input};
use rustain::adapters::tui::state::TuiState;
use rustain::domain::events::DomainInputEvent;
use rustain::domain::models::FocusState;

fn scrollable_state() -> TuiState {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;
    state.total_content_height = 100;
    state
}

#[test]
fn test_k_emits_scroll_line_up() {
    let mut state = scrollable_state();
    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('k'));
    assert_eq!(action, InputAction::ScrollLineUp);
}

#[test]
fn test_j_emits_scroll_line_down() {
    let mut state = scrollable_state();
    state.set_scroll_offset(3);
    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('j'));
    assert_eq!(action, InputAction::ScrollLineDown);
}

#[test]
fn test_g_emits_scroll_to_bottom() {
    let mut state = scrollable_state();
    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('G'));
    assert_eq!(action, InputAction::ScrollToBottom);
}

#[test]
fn test_g_emits_scroll_to_top_and_sets_pending_g() {
    let mut state = scrollable_state();
    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('g'));
    assert_eq!(action, InputAction::ScrollToTop);
    assert!(state.pending_g);
}

#[test]
fn test_gg_chord_second_g_idempotent() {
    // P6: Second g in gg chord is idempotent — returns Consumed.
    let mut state = scrollable_state();
    handle_input(&mut state, &DomainInputEvent::KeyPress('g'));
    assert!(state.pending_g);
    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('g'));
    assert_eq!(action, InputAction::Consumed);
    assert!(!state.pending_g);
}

#[test]
fn test_j_at_zero_emits_scroll_line_down() {
    let mut state = scrollable_state();
    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('j'));
    assert_eq!(action, InputAction::ScrollLineDown);
}

#[test]
fn test_k_when_content_fits_emits_scroll_line_up() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;
    state.total_content_height = 10;
    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('k'));
    assert_eq!(action, InputAction::ScrollLineUp);
}

#[test]
fn test_q_in_chat_returns_quit() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;
    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('q'));
    assert_eq!(action, InputAction::Quit);
}

#[test]
fn test_resize_preserves_scroll_position() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;
    state.total_content_height = 100;
    state.set_scroll_offset(30);
    state.set_auto_scroll(false);
    state.message_boundaries = vec![0, 20, 40, 60, 80];
    handle_input(&mut state, &DomainInputEvent::Resize(120, 40));
    assert_eq!(state.terminal_width, 120);
    assert!(state.scroll_offset() > 0);
}

#[test]
fn test_resize_at_bottom_stays_at_bottom() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;
    state.total_content_height = 100;
    state.set_scroll_offset(0);
    state.set_auto_scroll(true);
    handle_input(&mut state, &DomainInputEvent::Resize(120, 40));
    assert_eq!(state.scroll_offset(), 0);
    assert!(state.auto_scroll());
}
