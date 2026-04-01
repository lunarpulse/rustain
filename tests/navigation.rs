use rustain::adapters::tui::app::{InputAction, handle_input};
use rustain::adapters::tui::state::TuiState;
use rustain::domain::events::DomainInputEvent;
use rustain::domain::models::FocusState;

/// Integration test: full navigation flow.
/// Scroll up with k, block-jump with J/K, message-jump with {/}, verify scroll offsets.
#[test]
fn test_full_navigation_flow() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;
    state.total_content_height = 200;
    // Simulate 5 messages at lines 0, 40, 80, 120, 160
    // User messages at 0, 80, 160
    state.block_boundaries = vec![0, 40, 80, 120, 160];
    state.message_boundaries = vec![0, 80, 160];

    // Start at bottom (offset=0, auto_scroll=true)
    assert_eq!(state.scroll_offset, 0);
    assert!(state.auto_scroll);

    // Step 1: Scroll up with k (line-by-line)
    handle_input(&mut state, &DomainInputEvent::KeyPress('k'));
    assert_eq!(state.scroll_offset, 1);
    assert!(!state.auto_scroll);

    handle_input(&mut state, &DomainInputEvent::KeyPress('k'));
    assert_eq!(state.scroll_offset, 2);

    // Step 2: Block jump up with K
    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('K'));
    assert_eq!(action, InputAction::Consumed);
    // Should jump to a block boundary further up
    assert!(
        state.scroll_offset > 2,
        "K should jump further up from offset 2, got {}",
        state.scroll_offset
    );

    // Step 3: Jump to bottom with G
    handle_input(&mut state, &DomainInputEvent::KeyPress('G'));
    assert_eq!(state.scroll_offset, 0);
    assert!(state.auto_scroll);

    // Step 4: Scroll up then use { to jump to previous user message
    for _ in 0..5 {
        handle_input(&mut state, &DomainInputEvent::KeyPress('k'));
    }
    assert_eq!(state.scroll_offset, 5);

    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('{'));
    assert_eq!(action, InputAction::Consumed);
    // Should jump to a user message boundary
    assert!(
        state.scroll_offset > 5,
        "{{ should jump further up, got {}",
        state.scroll_offset
    );
}

/// Integration test: scroll down with j returns to bottom.
#[test]
fn test_scroll_down_returns_to_bottom() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;
    state.total_content_height = 100;

    // Scroll up 3 lines
    for _ in 0..3 {
        handle_input(&mut state, &DomainInputEvent::KeyPress('k'));
    }
    assert_eq!(state.scroll_offset, 3);

    // Scroll down 3 lines
    for _ in 0..3 {
        handle_input(&mut state, &DomainInputEvent::KeyPress('j'));
    }
    assert_eq!(state.scroll_offset, 0);
    assert!(state.auto_scroll);
}

/// Integration test: } jump with multiple user messages.
#[test]
fn test_message_jump_down_across_messages() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;
    state.total_content_height = 200;
    state.message_boundaries = vec![0, 50, 100, 150];
    // Start scrolled to top
    state.scroll_offset = 176; // max_offset = 200 - 24 = 176
    state.auto_scroll = false;

    // Jump down with }
    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('}'));
    assert_eq!(action, InputAction::Consumed);
    // Should move toward bottom
    assert!(
        state.scroll_offset < 176,
        "}} should move down from top, got {}",
        state.scroll_offset
    );
}
