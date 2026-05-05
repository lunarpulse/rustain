//! Navigation integration tests updated for S16.8 InputAction-based scroll.
//! Scroll mutations now happen in the event loop (dispatch_view_scroll), not
//! in handle_input. Tests verify the correct InputAction emissions.

use rustain::adapters::tui::app::{InputAction, handle_input};
use rustain::adapters::tui::state::TuiState;
use rustain::domain::events::DomainInputEvent;
use rustain::domain::models::FocusState;

#[test]
fn test_full_navigation_flow() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;
    state.total_content_height = 200;
    state.block_boundaries = vec![0, 40, 80, 120, 160];
    state.message_boundaries = vec![0, 40, 80, 120, 160];
    state.user_message_boundaries = vec![0, 80, 160];
    assert_eq!(state.scroll_offset(), 0);
    assert!(state.auto_scroll());

    // Step 1: k now emits ScrollLineUp (S16.8, AC7)
    assert_eq!(
        handle_input(&mut state, &DomainInputEvent::KeyPress('k')),
        InputAction::ScrollLineUp
    );
    assert_eq!(
        handle_input(&mut state, &DomainInputEvent::KeyPress('k')),
        InputAction::ScrollLineUp
    );

    // Step 2: K emits BlockJump (S16.8, AC7)
    // Set a known scroll position so the boundary search has a starting point.
    state.set_scroll_offset(10);
    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('K'));
    assert!(
        matches!(action, InputAction::BlockJump { .. }),
        "K should emit BlockJump up, got {:?}",
        action
    );
    if let InputAction::BlockJump {
        offset,
        auto_scroll,
    } = action
    {
        assert!(offset > 10, "K should jump up (offset {} > 10)", offset);
        assert!(!auto_scroll);
    }

    // Step 3: G emits ScrollToBottom (S16.8, AC3)
    assert_eq!(
        handle_input(&mut state, &DomainInputEvent::KeyPress('G')),
        InputAction::ScrollToBottom
    );

    // Step 4: { emits BlockJump (S16.8, AC7)
    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('{'));
    assert!(
        matches!(action, InputAction::BlockJump { .. }),
        "{{ should emit BlockJump, got {:?}",
        action
    );
}

#[test]
fn test_k_and_j_emit_correct_actions() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;
    state.total_content_height = 100;

    // k → ScrollLineUp
    assert_eq!(
        handle_input(&mut state, &DomainInputEvent::KeyPress('k')),
        InputAction::ScrollLineUp
    );
    // j → ScrollLineDown
    assert_eq!(
        handle_input(&mut state, &DomainInputEvent::KeyPress('j')),
        InputAction::ScrollLineDown
    );
}

#[test]
fn test_block_jump_actions() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;
    state.total_content_height = 200;
    state.message_boundaries = vec![0, 50, 100, 150];
    state.user_message_boundaries = vec![0, 50, 100, 150];

    // Start near top
    state.set_scroll_offset(176);

    // } emits BlockJump toward bottom
    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('}'));
    assert!(
        matches!(action, InputAction::BlockJump { .. }),
        "}} should emit BlockJump, got {:?}",
        action
    );
    // Verify the jump goes toward bottom (smaller offset)
    if let InputAction::BlockJump { offset, .. } = action {
        assert!(
            offset < 176,
            "}} should jump toward bottom (offset {} < 176)",
            offset
        );
    }
}
