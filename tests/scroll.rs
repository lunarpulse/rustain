use rustain::adapters::tui::app::{InputAction, handle_input};
use rustain::adapters::tui::state::TuiState;
use rustain::domain::events::DomainInputEvent;
use rustain::domain::models::FocusState;

/// Helper: create a state with enough content to scroll.
fn scrollable_state() -> TuiState {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;
    // Simulate 100 lines of content so there's room to scroll
    state.total_content_height = 100;
    state
}

/// AC3: Scroll up (k) disables auto-scroll.
// Covers: FR13 (auto-scroll), FR22 (vim keybindings)
#[test]
fn test_scroll_up_disables_auto_scroll() {
    let mut state = scrollable_state();
    assert!(state.auto_scroll);

    handle_input(&mut state, &DomainInputEvent::KeyPress('k'));

    assert!(!state.auto_scroll);
    assert_eq!(state.scroll_offset, 1);
}

/// AC3: Jump to bottom (G) enables auto-scroll.
// Covers: FR13 (auto-scroll), FR22 (vim keybindings)
#[test]
fn test_jump_to_bottom_enables_auto_scroll() {
    let mut state = scrollable_state();

    // First scroll up
    handle_input(&mut state, &DomainInputEvent::KeyPress('k'));
    handle_input(&mut state, &DomainInputEvent::KeyPress('k'));
    assert!(!state.auto_scroll);
    assert_eq!(state.scroll_offset, 2);

    // Then jump to bottom
    handle_input(&mut state, &DomainInputEvent::KeyPress('G'));
    assert!(state.auto_scroll);
    assert_eq!(state.scroll_offset, 0);
}

/// AC3: Scroll down (j) decrements offset from bottom.
// Covers: FR13 (auto-scroll), FR22 (vim keybindings)
#[test]
fn test_scroll_down_decrements_offset() {
    let mut state = scrollable_state();

    // Scroll up first
    handle_input(&mut state, &DomainInputEvent::KeyPress('k'));
    handle_input(&mut state, &DomainInputEvent::KeyPress('k'));
    handle_input(&mut state, &DomainInputEvent::KeyPress('k'));
    assert_eq!(state.scroll_offset, 3);

    // Scroll down
    handle_input(&mut state, &DomainInputEvent::KeyPress('j'));
    assert_eq!(state.scroll_offset, 2);
    assert!(!state.auto_scroll);
}

/// AC3: Scroll down to offset 0 re-enables auto-scroll.
// Covers: FR13 (auto-scroll)
#[test]
fn test_scroll_to_bottom_auto_enables_auto_scroll() {
    let mut state = scrollable_state();

    // Scroll up 1
    handle_input(&mut state, &DomainInputEvent::KeyPress('k'));
    assert_eq!(state.scroll_offset, 1);
    assert!(!state.auto_scroll);

    // Scroll back down to 0
    handle_input(&mut state, &DomainInputEvent::KeyPress('j'));
    assert_eq!(state.scroll_offset, 0);
    assert!(state.auto_scroll);
}

/// Scroll down at offset 0 doesn't go negative (clamp).
// Covers: FR13 (auto-scroll)
#[test]
fn test_scroll_down_clamped_at_zero() {
    let mut state = scrollable_state();

    handle_input(&mut state, &DomainInputEvent::KeyPress('j'));
    assert_eq!(state.scroll_offset, 0);
    assert!(state.auto_scroll);
}

/// Scroll up clamped at max scrollable range.
// Covers: FR13 (auto-scroll)
#[test]
fn test_scroll_up_clamped_at_max() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;
    // Content fits in viewport — no scrolling possible
    state.total_content_height = 10;

    handle_input(&mut state, &DomainInputEvent::KeyPress('k'));
    // Should not scroll since content (10) < viewport (24)
    assert_eq!(state.scroll_offset, 0);
    assert!(state.auto_scroll);
}

/// AC4: 'q' in Chat focus returns Quit action.
// Covers: FR22 (vim keybindings)
#[test]
fn test_q_in_chat_returns_quit() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;

    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('q'));
    assert_eq!(action, InputAction::Quit);
}

/// AC7: Resize preserves approximate scroll position.
// Covers: FR13 (auto-scroll)
#[test]
fn test_resize_preserves_scroll_position() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;
    state.total_content_height = 100;
    state.scroll_offset = 30;
    state.auto_scroll = false;
    state.message_boundaries = vec![0, 20, 40, 60, 80];

    // Resize from 80x24 to 120x40
    handle_input(&mut state, &DomainInputEvent::Resize(120, 40));

    assert_eq!(state.terminal_width, 120);
    assert_eq!(state.terminal_height, 40);
    // Scroll offset should be approximately preserved (not 0, not at max)
    assert!(
        state.scroll_offset > 0,
        "Expected scroll position preserved after resize, got offset={}",
        state.scroll_offset
    );
}

/// AC7: Resize at bottom (auto_scroll=true) stays at bottom.
// Covers: FR13 (auto-scroll)
#[test]
fn test_resize_at_bottom_stays_at_bottom() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;
    state.total_content_height = 100;
    state.scroll_offset = 0;
    state.auto_scroll = true;

    handle_input(&mut state, &DomainInputEvent::Resize(120, 40));

    assert_eq!(state.scroll_offset, 0);
    assert!(state.auto_scroll);
}
