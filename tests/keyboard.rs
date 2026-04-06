use rustain::adapters::tui::app::{InputAction, handle_input};
use rustain::adapters::tui::state::TuiState;
use rustain::domain::events::{DomainInputEvent, DomainKey};
use rustain::domain::models::FocusState;

// Covers: FR22 (vim keybindings)
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

// Covers: FR22 (vim keybindings)
/// AC: 'i' in chat focus → focus Input.
#[test]
fn test_i_focuses_input_from_chat() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;

    handle_input(&mut state, &DomainInputEvent::KeyPress('i'));
    assert_eq!(state.focus, FocusState::Input);
}

// Covers: FR22 (vim keybindings)
/// AC: 'q' in chat focus → Quit action.
#[test]
fn test_q_quits_from_chat() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;

    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('q'));
    assert_eq!(action, InputAction::Quit);
}

// Covers: FR16 (input controls)
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

// Covers: FR16 (input controls)
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

// Covers: FR16 (input controls)
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

// Covers: FR16 (input controls)
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

// Covers: FR16 (input controls)
/// Enter with empty buffer returns Consumed (not SubmitMessage).
#[test]
fn test_empty_enter_returns_consumed() {
    let mut state = TuiState::new(80, 24);
    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Enter));
    assert_eq!(action, InputAction::Consumed);
}

// Covers: FR22 (vim keybindings), FR13 (scroll)
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

// Covers: FR22 (vim keybindings), FR13 (scroll)
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

// Covers: FR22 (vim keybindings), FR13 (scroll)
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

// Covers: FR22 (vim keybindings), FR13 (scroll)
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

// Covers: FR22 (vim keybindings), FR13 (scroll)
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

// === Multi-line input keyboard tests (Story 3.1) ===

// Covers: UX-DR76 — Shift+Enter inserts newline
#[test]
fn test_shift_enter_inserts_newline() {
    let mut state = TuiState::new(80, 24);
    handle_input(&mut state, &DomainInputEvent::KeyPress('a'));
    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::ShiftEnter));
    assert_eq!(action, InputAction::Consumed);
    assert_eq!(state.input_buffer, "a\n");
    assert_eq!(state.cursor_position, 2);
}

// Covers: UX-DR76 — Ctrl+E toggles multiline mode
#[test]
fn test_ctrl_e_toggles_multiline_mode() {
    let mut state = TuiState::new(80, 24);
    assert!(!state.multiline_mode);

    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlE));
    assert_eq!(action, InputAction::Consumed);
    assert!(state.multiline_mode);

    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlE));
    assert_eq!(action, InputAction::Consumed);
    assert!(!state.multiline_mode);
}

// Covers: UX-DR76 — Enter inserts newline in multiline mode
#[test]
fn test_enter_inserts_newline_in_multiline_mode() {
    let mut state = TuiState::new(80, 24);
    state.multiline_mode = true;
    handle_input(&mut state, &DomainInputEvent::KeyPress('x'));
    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Enter));
    assert_eq!(action, InputAction::Consumed);
    assert_eq!(state.input_buffer, "x\n");
}

// Covers: UX-DR76 — Ctrl+Enter submits in multiline mode
#[test]
fn test_ctrl_enter_submits_in_multiline_mode() {
    let mut state = TuiState::new(80, 24);
    state.multiline_mode = true;
    handle_input(&mut state, &DomainInputEvent::KeyPress('h'));
    handle_input(&mut state, &DomainInputEvent::KeyPress('i'));
    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlEnter));
    assert_eq!(action, InputAction::SubmitMessage("hi".to_string()));
}

// Covers: UX-DR76 — Esc submits in multiline mode when content exists
#[test]
fn test_esc_submits_in_multiline_mode_with_content() {
    let mut state = TuiState::new(80, 24);
    state.multiline_mode = true;
    handle_input(&mut state, &DomainInputEvent::KeyPress('x'));
    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Esc));
    assert_eq!(action, InputAction::SubmitMessage("x".to_string()));
}

// Covers: UX-DR76 — Esc toggles focus in multiline mode when input is empty
#[test]
fn test_esc_toggles_focus_in_multiline_mode_empty() {
    let mut state = TuiState::new(80, 24);
    state.multiline_mode = true;
    // Empty input — should toggle to Chat
    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Esc));
    assert_eq!(action, InputAction::Consumed);
    assert_eq!(state.focus, FocusState::Chat);
}

// Covers: UX-DR76 — Enter submits normally (not multiline mode)
#[test]
fn test_enter_submits_normally_without_multiline_mode() {
    let mut state = TuiState::new(80, 24);
    assert!(!state.multiline_mode);
    handle_input(&mut state, &DomainInputEvent::KeyPress('h'));
    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Enter));
    assert_eq!(action, InputAction::SubmitMessage("h".to_string()));
}

// Covers: UX-DR76 — cursor wrapping in multi-line content
#[test]
fn test_up_down_cursor_in_multiline_content() {
    let mut state = TuiState::new(80, 24);
    state.input_buffer = "abc\ndef".to_string();
    state.cursor_position = 5; // at 'd' on second line → (1, 1)

    // Up arrow moves to first line
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Up));
    assert_eq!(state.cursor_position, 1); // (0, 1) → char idx 1

    // Down arrow moves back to second line
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Down));
    assert_eq!(state.cursor_position, 5); // (1, 1) → char idx 5
}

// Covers: UX-DR76 — backspace at line boundary merges lines
#[test]
fn test_backspace_at_line_boundary() {
    let mut state = TuiState::new(80, 24);
    state.input_buffer = "abc\ndef".to_string();
    state.cursor_position = 4; // at start of second line (after '\n')

    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Backspace));
    assert_eq!(state.input_buffer, "abcdef");
    assert_eq!(state.cursor_position, 3);
}

// Covers: UX-DR76 — Home moves to start of current line
#[test]
fn test_home_key() {
    let mut state = TuiState::new(80, 24);
    state.input_buffer = "abc\ndef".to_string();
    state.cursor_position = 5; // mid-second line

    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Home));
    assert_eq!(state.cursor_position, 4); // start of second line
}

// Covers: UX-DR76 — End moves to end of current line
#[test]
fn test_end_key() {
    let mut state = TuiState::new(80, 24);
    state.input_buffer = "abc\ndef".to_string();
    state.cursor_position = 4; // start of second line

    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::End));
    assert_eq!(state.cursor_position, 7); // end of second line
}

// === Input History keyboard tests (Story 3.1) ===

// Covers: UX-DR74, AC3 — Up populates previous message when input empty
#[test]
fn test_up_arrow_history_when_empty() {
    let mut state = TuiState::new(80, 24);
    state.input_history.push("first".to_string());
    state.input_history.push("second".to_string());

    // Up when empty → populate with "second"
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Up));
    assert_eq!(state.input_buffer, "second");

    // Up again → "first"
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Up));
    assert_eq!(state.input_buffer, "first");
}

// Covers: UX-DR74, AC3 — Down cycles forward, empty at end restores draft
#[test]
fn test_down_arrow_history_cycle() {
    let mut state = TuiState::new(80, 24);
    state.input_history.push("msg1".to_string());
    state.input_history.push("msg2".to_string());

    // Navigate up
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Up));
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Up));
    assert_eq!(state.input_buffer, "msg1");

    // Navigate down
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Down));
    assert_eq!(state.input_buffer, "msg2");

    // Down past end → restores draft (empty)
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Down));
    assert_eq!(state.input_buffer, "");
}

// Covers: UX-DR74, AC6 — Submit adds to history
#[test]
fn test_submit_adds_to_history() {
    let mut state = TuiState::new(80, 24);
    handle_input(&mut state, &DomainInputEvent::KeyPress('h'));
    handle_input(&mut state, &DomainInputEvent::KeyPress('i'));
    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Enter));
    assert_eq!(action, InputAction::SubmitMessage("hi".to_string()));

    // Now navigate up — should find "hi"
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Up));
    assert_eq!(state.input_buffer, "hi");
}

// Covers: UX-DR74 — Ctrl+R activates reverse search
#[test]
fn test_ctrl_r_activates_reverse_search() {
    let mut state = TuiState::new(80, 24);
    state.input_history.push("test command".to_string());

    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlR));
    assert_eq!(action, InputAction::Consumed);
    assert!(state.reverse_search.active);
    assert_eq!(state.focus, FocusState::Overlay(rustain::domain::models::visual::OverlayType::ReverseSearch));
}

// Covers: UX-DR74 — Reverse search: typing filters, Enter selects, Esc cancels
#[test]
fn test_reverse_search_flow() {
    let mut state = TuiState::new(80, 24);
    state.input_history.push("add auth middleware".to_string());
    state.input_history.push("check auth token".to_string());
    state.input_history.push("run tests".to_string());

    // Activate reverse search
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlR));
    assert!(state.reverse_search.active);

    // Type "auth" to filter
    handle_input(&mut state, &DomainInputEvent::KeyPress('a'));
    handle_input(&mut state, &DomainInputEvent::KeyPress('u'));
    handle_input(&mut state, &DomainInputEvent::KeyPress('t'));
    handle_input(&mut state, &DomainInputEvent::KeyPress('h'));

    assert_eq!(state.reverse_search.query, "auth");
    assert_eq!(state.reverse_search.matches.len(), 2);

    // Enter selects the match
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Enter));
    assert!(!state.reverse_search.active);
    assert!(state.input_buffer.contains("auth"));
}

// Covers: UX-DR74 — Reverse search: Esc cancels
#[test]
fn test_reverse_search_cancel() {
    let mut state = TuiState::new(80, 24);
    state.input_history.push("hello".to_string());

    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlR));
    assert!(state.reverse_search.active);

    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Esc));
    assert!(!state.reverse_search.active);
    assert_eq!(state.focus, FocusState::Input);
}
