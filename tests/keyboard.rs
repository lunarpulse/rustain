use rustain::adapters::tui::app::handle_input;
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
    assert_eq!(state.focus, FocusState::Chat { scroll_offset: 0 });

    // Esc → Input
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Esc));
    assert_eq!(state.focus, FocusState::Input);
}

/// AC: 'i' in chat focus → focus Input.
#[test]
fn test_i_focuses_input_from_chat() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat { scroll_offset: 0 };

    handle_input(&mut state, &DomainInputEvent::KeyPress('i'));
    assert_eq!(state.focus, FocusState::Input);
}

/// AC: 'q' in chat focus → quit.
#[test]
fn test_q_quits_from_chat() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat { scroll_offset: 0 };

    handle_input(&mut state, &DomainInputEvent::KeyPress('q'));
    assert!(state.should_quit);
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

/// Enter clears input buffer (placeholder send).
#[test]
fn test_enter_clears_input() {
    let mut state = TuiState::new(80, 24);
    handle_input(&mut state, &DomainInputEvent::KeyPress('x'));
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Enter));

    assert_eq!(state.input_buffer, "");
    assert_eq!(state.cursor_position, 0);
}
