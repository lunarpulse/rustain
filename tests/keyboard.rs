use rustain::adapters::tui::app::{InputAction, handle_input};
use rustain::adapters::tui::state::TuiState;
use rustain::domain::events::{DomainInputEvent, DomainKey};
use rustain::domain::models::FocusState;

/// AC: Esc toggles focus between Input and Chat.
#[test]
fn test_esc_toggles_focus() {
    let mut state = TuiState::new(80, 24);
    assert_eq!(state.focus, FocusState::Input);

    // Esc → Chat
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Esc));
    assert_eq!(state.focus, FocusState::Chat);

    // Esc → Input
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Esc));
    assert_eq!(state.focus, FocusState::Input);
}

/// AC: 'i' in chat focus → focus Input.
#[test]
fn test_i_focuses_input_from_chat() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;

    handle_input(&mut state, &DomainInputEvent::KeyPress('i'));
    assert_eq!(state.focus, FocusState::Input);
}

/// AC: 'q' in chat focus → Quit action.
#[test]
fn test_q_quits_from_chat() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;

    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('q'));
    assert_eq!(action, InputAction::Quit);
}

/// Typing in input mode adds to buffer.
#[test]
fn test_typing_in_input() {
    let mut state = TuiState::new(80, 24);
    assert_eq!(state.focus, FocusState::Input);

    handle_input(&mut state, &DomainInputEvent::KeyPress('h'));
    handle_input(&mut state, &DomainInputEvent::KeyPress('i'));

    assert_eq!(state.input_buffer, "hi");
    assert_eq!(state.cursor_position, 2);
}

/// Backspace removes characters.
#[test]
fn test_backspace_in_input() {
    let mut state = TuiState::new(80, 24);
    handle_input(&mut state, &DomainInputEvent::KeyPress('a'));
    handle_input(&mut state, &DomainInputEvent::KeyPress('b'));
    handle_input(
        &mut state,
        &DomainInputEvent::SpecialKey(DomainKey::Backspace),
    );

    assert_eq!(state.input_buffer, "a");
    assert_eq!(state.cursor_position, 1);
}

/// Multi-byte characters are handled correctly (no char-boundary panic).
#[test]
fn test_multibyte_chars_in_input() {
    let mut state = TuiState::new(80, 24);

    // Type multi-byte chars
    handle_input(&mut state, &DomainInputEvent::KeyPress('é'));
    handle_input(&mut state, &DomainInputEvent::KeyPress('€'));
    handle_input(&mut state, &DomainInputEvent::KeyPress('日'));

    assert_eq!(state.input_buffer, "é€日");
    assert_eq!(state.cursor_position, 3);

    // Backspace should remove last multi-byte char
    handle_input(
        &mut state,
        &DomainInputEvent::SpecialKey(DomainKey::Backspace),
    );
    assert_eq!(state.input_buffer, "é€");
    assert_eq!(state.cursor_position, 2);

    // Left then insert in the middle
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Left));
    handle_input(&mut state, &DomainInputEvent::KeyPress('x'));
    assert_eq!(state.input_buffer, "éx€");
    assert_eq!(state.cursor_position, 2);
}

/// Enter with text returns SubmitMessage and clears buffer.
#[test]
fn test_enter_returns_submit_message() {
    let mut state = TuiState::new(80, 24);
    handle_input(&mut state, &DomainInputEvent::KeyPress('x'));
    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Enter));

    assert_eq!(action, InputAction::SubmitMessage("x".to_string()));
    assert_eq!(state.input_buffer, "");
    assert_eq!(state.cursor_position, 0);
}

/// Enter with empty buffer returns Consumed (not SubmitMessage).
#[test]
fn test_empty_enter_returns_consumed() {
    let mut state = TuiState::new(80, 24);
    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Enter));
    assert_eq!(action, InputAction::Consumed);
}

/// J/K at conversation start (no-op when no content or at boundary).
#[test]
fn test_block_jump_no_content_is_noop() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;
    // No content, no boundaries
    state.total_content_height = 0;
    state.block_boundaries = vec![];

    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('J'));
    assert_eq!(action, InputAction::Consumed);
    assert_eq!(state.scroll_offset, 0);

    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('K'));
    assert_eq!(action, InputAction::Consumed);
    assert_eq!(state.scroll_offset, 0);
}

/// J at bottom is no-op.
#[test]
fn test_block_jump_down_at_bottom_noop() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;
    state.total_content_height = 100;
    state.block_boundaries = vec![0, 25, 50, 75];
    state.scroll_offset = 0; // at bottom

    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('J'));
    assert_eq!(action, InputAction::Consumed);
    assert_eq!(state.scroll_offset, 0);
}

/// K at top is no-op.
#[test]
fn test_block_jump_up_at_top_noop() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;
    state.total_content_height = 100;
    state.block_boundaries = vec![0, 25, 50, 75];
    state.scroll_offset = 76; // at top (max_offset = 100-24 = 76)

    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('K'));
    assert_eq!(action, InputAction::Consumed);
    assert_eq!(state.scroll_offset, 76);
}

/// {/} with no user messages is no-op.
#[test]
fn test_message_jump_no_user_messages_noop() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;
    state.total_content_height = 100;
    state.message_boundaries = vec![]; // No user messages

    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('{'));
    assert_eq!(action, InputAction::Consumed);
    assert_eq!(state.scroll_offset, 0);

    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('}'));
    assert_eq!(action, InputAction::Consumed);
    assert_eq!(state.scroll_offset, 0);
}

/// J/K with single block: J from scrolled position should jump to bottom.
#[test]
fn test_block_jump_single_block() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;
    state.total_content_height = 50;
    state.block_boundaries = vec![0];
    state.scroll_offset = 10;

    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('J'));
    assert_eq!(action, InputAction::Consumed);
    // Should jump to bottom (offset 0)
    assert_eq!(state.scroll_offset, 0);
}
