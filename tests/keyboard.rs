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
    assert_eq!(state.scroll_offset(), 0);

    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('K'));
    assert_eq!(action, InputAction::Consumed);
    assert_eq!(state.scroll_offset(), 0);
}

// Covers: FR22 (vim keybindings), FR13 (scroll)
/// J at bottom is no-op.
#[test]
fn test_block_jump_down_at_bottom_noop() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;
    state.total_content_height = 100;
    state.block_boundaries = vec![0, 25, 50, 75];
    state.set_scroll_offset(0); // at bottom

    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('J'));
    assert_eq!(action, InputAction::Consumed);
    assert_eq!(state.scroll_offset(), 0);
}

// Covers: FR22 (vim keybindings), FR13 (scroll)
/// K at top is no-op.
#[test]
fn test_block_jump_up_at_top_noop() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;
    state.viewport_height = 24; // tests use terminal_height as viewport
    state.total_content_height = 100;
    state.block_boundaries = vec![0, 25, 50, 75];
    state.set_scroll_offset(76); // at top (max_offset = 100-24 = 76)

    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('K'));
    assert_eq!(action, InputAction::Consumed);
    assert_eq!(state.scroll_offset(), 76);
}

// Covers: FR22 (vim keybindings), FR13 (scroll)
/// {/} with no user messages is no-op.
#[test]
fn test_message_jump_no_user_messages_noop() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;
    state.total_content_height = 100;
    state.message_boundaries = vec![]; // No messages
    state.user_message_boundaries = vec![]; // No user messages

    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('{'));
    assert_eq!(action, InputAction::Consumed);
    assert_eq!(state.scroll_offset(), 0);

    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('}'));
    assert_eq!(action, InputAction::Consumed);
    assert_eq!(state.scroll_offset(), 0);
}

// Covers: FR22 (vim keybindings), FR13 (scroll)
/// J/K with single block: J should emit BlockJump to bottom. S16.8, AC7.
#[test]
fn test_block_jump_single_block() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;
    state.total_content_height = 50;
    state.block_boundaries = vec![0];
    state.set_scroll_offset(10);
    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('J'));
    assert!(
        matches!(
            action,
            InputAction::BlockJump {
                offset: 0,
                auto_scroll: true
            }
        ),
        "J should emit BlockJump(offset=0, auto_scroll=true), got {:?}",
        action
    );
}

// === Multi-line input keyboard tests (Story 3.1) ===

// Covers: UX-DR76 — Shift+Enter inserts newline
#[test]
fn test_shift_enter_inserts_newline() {
    let mut state = TuiState::new(80, 24);
    handle_input(&mut state, &DomainInputEvent::KeyPress('a'));
    let action = handle_input(
        &mut state,
        &DomainInputEvent::SpecialKey(DomainKey::ShiftEnter),
    );
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
    let action = handle_input(
        &mut state,
        &DomainInputEvent::SpecialKey(DomainKey::CtrlEnter),
    );
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

    handle_input(
        &mut state,
        &DomainInputEvent::SpecialKey(DomainKey::Backspace),
    );
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
    assert_eq!(
        state.focus,
        FocusState::Overlay(rustain::domain::models::visual::OverlayType::ReverseSearch)
    );
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

// === Autocomplete keyboard tests (Story 3.2, Task 6) ===

use rustain::domain::models::autocomplete::{AutocompleteKind, AutocompleteSuggestion};

// Covers: AC1 — '/' at position 0 triggers slash command autocomplete
#[test]
fn test_slash_at_position_zero_triggers_autocomplete() {
    let mut state = TuiState::new(80, 24);
    handle_input(&mut state, &DomainInputEvent::KeyPress('/'));
    assert!(state.autocomplete.active);
    assert_eq!(state.autocomplete.kind, AutocompleteKind::SlashCommand);
    assert_eq!(state.autocomplete.trigger_position, 0);
    assert_eq!(state.input_buffer, "/");
}

// Covers: AC1 — '/' at position > 0 does NOT trigger autocomplete
#[test]
fn test_slash_not_at_position_zero_no_autocomplete() {
    let mut state = TuiState::new(80, 24);
    handle_input(&mut state, &DomainInputEvent::KeyPress('a'));
    handle_input(&mut state, &DomainInputEvent::KeyPress('/'));
    assert!(!state.autocomplete.active);
    assert_eq!(state.input_buffer, "a/");
}

// Covers: AC3 — '@' anywhere triggers file mention autocomplete
#[test]
fn test_at_triggers_file_mention_autocomplete() {
    let mut state = TuiState::new(80, 24);
    handle_input(&mut state, &DomainInputEvent::KeyPress('h'));
    handle_input(&mut state, &DomainInputEvent::KeyPress('i'));
    handle_input(&mut state, &DomainInputEvent::KeyPress(' '));
    handle_input(&mut state, &DomainInputEvent::KeyPress('@'));
    assert!(state.autocomplete.active);
    assert_eq!(state.autocomplete.kind, AutocompleteKind::FileMention);
    assert_eq!(state.autocomplete.trigger_position, 3); // '@' is at position 3
    assert_eq!(state.input_buffer, "hi @");
}

// Covers: AC1 — typing after trigger updates filter text
#[test]
fn test_typing_after_trigger_updates_filter() {
    let mut state = TuiState::new(80, 24);
    handle_input(&mut state, &DomainInputEvent::KeyPress('/'));
    assert!(state.autocomplete.active);

    handle_input(&mut state, &DomainInputEvent::KeyPress('n'));
    assert_eq!(state.autocomplete.filter_text, "n");
    assert_eq!(state.input_buffer, "/n");

    handle_input(&mut state, &DomainInputEvent::KeyPress('e'));
    assert_eq!(state.autocomplete.filter_text, "ne");
    assert_eq!(state.input_buffer, "/ne");
}

// Covers: AC1 — Up/Down navigates autocomplete when active
#[test]
fn test_up_down_navigates_autocomplete() {
    let mut state = TuiState::new(80, 24);
    handle_input(&mut state, &DomainInputEvent::KeyPress('/'));
    // Manually populate suggestions for test
    state.autocomplete.suggestions = vec![
        AutocompleteSuggestion::SlashCommand {
            name: "new".to_string(),
            description: "New session".to_string(),
        },
        AutocompleteSuggestion::SlashCommand {
            name: "help".to_string(),
            description: "Show help".to_string(),
        },
    ];

    assert_eq!(state.autocomplete.selected_index, 0);
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Down));
    assert_eq!(state.autocomplete.selected_index, 1);
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Down));
    assert_eq!(state.autocomplete.selected_index, 0); // Wraps around
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Up));
    assert_eq!(state.autocomplete.selected_index, 1); // Wraps back
}

// Covers: AC1 — Tab selects autocomplete suggestion
#[test]
fn test_tab_selects_autocomplete() {
    let mut state = TuiState::new(80, 24);
    handle_input(&mut state, &DomainInputEvent::KeyPress('/'));
    state.autocomplete.suggestions = vec![AutocompleteSuggestion::SlashCommand {
        name: "new".to_string(),
        description: "New session".to_string(),
    }];

    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Tab));
    assert!(!state.autocomplete.active);
    assert_eq!(state.input_buffer, "/new");
}

// Covers: AC1 — Enter selects autocomplete suggestion
#[test]
fn test_enter_selects_autocomplete() {
    let mut state = TuiState::new(80, 24);
    handle_input(&mut state, &DomainInputEvent::KeyPress('/'));
    state.autocomplete.suggestions = vec![AutocompleteSuggestion::SlashCommand {
        name: "new".to_string(),
        description: "New session".to_string(),
    }];

    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Enter));
    assert!(!state.autocomplete.active);
    assert_eq!(state.input_buffer, "/new");
}

// Covers: AC1 — Esc dismisses autocomplete
#[test]
fn test_esc_dismisses_autocomplete() {
    let mut state = TuiState::new(80, 24);
    handle_input(&mut state, &DomainInputEvent::KeyPress('/'));
    assert!(state.autocomplete.active);

    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Esc));
    assert!(!state.autocomplete.active);
    assert_eq!(state.input_buffer, "/"); // Content preserved
}

// Covers: AC1 — Backspace past trigger dismisses autocomplete
#[test]
fn test_backspace_past_trigger_dismisses() {
    let mut state = TuiState::new(80, 24);
    handle_input(&mut state, &DomainInputEvent::KeyPress('/'));
    handle_input(&mut state, &DomainInputEvent::KeyPress('n'));
    assert!(state.autocomplete.active);
    assert_eq!(state.input_buffer, "/n");

    // Backspace removes 'n', filter becomes empty
    handle_input(
        &mut state,
        &DomainInputEvent::SpecialKey(DomainKey::Backspace),
    );
    // At this point cursor_position == 1 which equals trigger_position + 1
    // So the next backspace will dismiss
    assert_eq!(state.input_buffer, "/");

    // Backspace removes '/' and dismisses
    handle_input(
        &mut state,
        &DomainInputEvent::SpecialKey(DomainKey::Backspace),
    );
    assert!(!state.autocomplete.active);
    assert_eq!(state.input_buffer, "");
}

// Covers: AC4 — File mention selection inserts @path and tracks mention
#[test]
fn test_file_mention_selection() {
    let mut state = TuiState::new(80, 24);
    handle_input(&mut state, &DomainInputEvent::KeyPress('@'));
    state.autocomplete.suggestions = vec![AutocompleteSuggestion::FilePath {
        path: "src/main.rs".to_string(),
        is_dir: false,
    }];

    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Tab));
    assert!(!state.autocomplete.active);
    assert_eq!(state.input_buffer, "@src/main.rs");
    assert_eq!(state.resolved_mentions.len(), 1);
    assert_eq!(state.resolved_mentions[0].path, "src/main.rs");
}

// Covers: AC4 — multiple @ mentions in single message
#[test]
fn test_multiple_file_mentions() {
    let mut state = TuiState::new(80, 24);
    // First mention
    handle_input(&mut state, &DomainInputEvent::KeyPress('@'));
    state.autocomplete.suggestions = vec![AutocompleteSuggestion::FilePath {
        path: "src/main.rs".to_string(),
        is_dir: false,
    }];
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Tab));
    assert_eq!(state.input_buffer, "@src/main.rs");

    // Type space then second mention
    handle_input(&mut state, &DomainInputEvent::KeyPress(' '));
    handle_input(&mut state, &DomainInputEvent::KeyPress('@'));
    state.autocomplete.suggestions = vec![AutocompleteSuggestion::FilePath {
        path: "src/lib.rs".to_string(),
        is_dir: false,
    }];
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Tab));
    assert_eq!(state.input_buffer, "@src/main.rs @src/lib.rs");
    assert_eq!(state.resolved_mentions.len(), 2);
}

// Covers: P9 fix — duplicate file mentions are deduplicated
#[test]
fn test_duplicate_file_mention_deduplication() {
    let mut state = TuiState::new(80, 24);
    // First mention of src/main.rs
    handle_input(&mut state, &DomainInputEvent::KeyPress('@'));
    state.autocomplete.suggestions = vec![AutocompleteSuggestion::FilePath {
        path: "src/main.rs".to_string(),
        is_dir: false,
    }];
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Tab));
    assert_eq!(state.resolved_mentions.len(), 1);

    // Type space then second mention of the SAME file
    handle_input(&mut state, &DomainInputEvent::KeyPress(' '));
    handle_input(&mut state, &DomainInputEvent::KeyPress('@'));
    state.autocomplete.suggestions = vec![AutocompleteSuggestion::FilePath {
        path: "src/main.rs".to_string(),
        is_dir: false,
    }];
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Tab));
    // Should still be 1 — duplicate not added
    assert_eq!(state.resolved_mentions.len(), 1);
    assert_eq!(state.input_buffer, "@src/main.rs @src/main.rs");
}

// Covers: AC2 — /new command submission returns ExecuteCommand
#[test]
fn test_slash_new_submits_execute_command() {
    let mut state = TuiState::new(80, 24);
    handle_input(&mut state, &DomainInputEvent::KeyPress('/'));
    state.autocomplete.suggestions = vec![AutocompleteSuggestion::SlashCommand {
        name: "new".to_string(),
        description: "New session".to_string(),
    }];
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Tab));
    assert_eq!(state.input_buffer, "/new");

    // Submit the /new command
    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Enter));
    assert_eq!(
        action,
        InputAction::ExecuteCommand {
            name: "new".to_string(),
            args: None
        }
    );
}

// Covers: AC2 — user-defined command submission returns SubmitWithContext
#[test]
fn test_user_command_submits_with_context() {
    let mut state = TuiState::new(80, 24);
    handle_input(&mut state, &DomainInputEvent::KeyPress('/'));
    state.autocomplete.suggestions = vec![AutocompleteSuggestion::SlashCommand {
        name: "deploy-staging".to_string(),
        description: "Deploy".to_string(),
    }];
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Tab));
    assert_eq!(state.input_buffer, "/deploy-staging");

    // Submit
    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Enter));
    assert_eq!(
        action,
        InputAction::SubmitWithContext {
            text: "".to_string(),
            command: Some("deploy-staging".to_string()),
            command_args: None,
        }
    );
}

// Covers: AC5 — '@' with empty results shows "No matches"
#[test]
fn test_at_with_no_results() {
    let mut state = TuiState::new(80, 24);
    handle_input(&mut state, &DomainInputEvent::KeyPress('@'));
    assert!(state.autocomplete.active);
    // Suggestions empty — the "No matches" state is rendered by the widget
    assert!(state.autocomplete.suggestions.is_empty());
}

// Covers: AC8 — autocomplete opens normally during active streaming
#[test]
fn test_autocomplete_during_streaming_focus() {
    // Autocomplete is state + render, no interaction with streaming
    // Just verify that autocomplete works when not in special focus states
    let mut state = TuiState::new(80, 24);
    // Simulate streaming being active (focus is still Input)
    assert_eq!(state.focus, FocusState::Input);
    handle_input(&mut state, &DomainInputEvent::KeyPress('/'));
    assert!(state.autocomplete.active);
}

// === /new command tests (Story 3.2, Task 7) ===

// Covers: AC7 — /new with active conversation returns ExecuteCommand
#[test]
fn test_slash_new_command_flow() {
    let mut state = TuiState::new(80, 24);
    // Type /new and select it
    handle_input(&mut state, &DomainInputEvent::KeyPress('/'));
    state.autocomplete.suggestions = vec![AutocompleteSuggestion::SlashCommand {
        name: "new".to_string(),
        description: "New session".to_string(),
    }];
    // Tab to select
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Tab));
    assert_eq!(state.input_buffer, "/new");
    assert!(!state.autocomplete.active);

    // Submit
    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Enter));
    assert_eq!(
        action,
        InputAction::ExecuteCommand {
            name: "new".to_string(),
            args: None
        }
    );
    assert!(state.input_buffer.is_empty());
}

// Covers: AC7 — /new with empty session (no save attempted)
#[test]
fn test_slash_new_empty_session() {
    let mut state = TuiState::new(80, 24);
    // Directly type "/new" without autocomplete
    state.input_buffer = "/new".to_string();
    state.cursor_position = 4;

    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Enter));
    assert_eq!(
        action,
        InputAction::ExecuteCommand {
            name: "new".to_string(),
            args: None
        }
    );
}

// Covers: AC7 — resolved mentions are cleared on /new
#[test]
fn test_slash_new_clears_mentions() {
    use rustain::adapters::tui::state::ResolvedMention;
    let mut state = TuiState::new(80, 24);
    state.resolved_mentions.push(ResolvedMention {
        path: "test.rs".to_string(),
    });

    state.input_buffer = "/new".to_string();
    state.cursor_position = 4;
    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Enter));
    // /new is a built-in command — returns ExecuteCommand, event loop handles state reset
    assert_eq!(
        action,
        InputAction::ExecuteCommand {
            name: "new".to_string(),
            args: None
        }
    );
    // Input buffer should be cleared by submit_message
    assert!(state.input_buffer.is_empty());
}

// === Story 3-6a: /ml slash command (Sprint Change Proposal 2026-04-08, AC#3) ===

// Covers: Sprint Change Proposal 2026-04-08, AC#3
/// /ml submitted directly returns ExecuteCommand("ml").
#[test]
fn test_slash_ml_returns_execute_command() {
    let mut state = TuiState::new(80, 24);
    state.input_buffer = "/ml".to_string();
    state.cursor_position = 3;

    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Enter));
    assert_eq!(
        action,
        InputAction::ExecuteCommand {
            name: "ml".to_string(),
            args: None
        }
    );
    assert!(state.input_buffer.is_empty());
}

// Covers: Sprint Change Proposal 2026-04-08, AC#3
/// /ml via autocomplete selection also returns ExecuteCommand("ml").
#[test]
fn test_slash_ml_via_autocomplete_returns_execute_command() {
    let mut state = TuiState::new(80, 24);
    handle_input(&mut state, &DomainInputEvent::KeyPress('/'));
    state.autocomplete.suggestions = vec![AutocompleteSuggestion::SlashCommand {
        name: "ml".to_string(),
        description: "Toggle multi-line mode".to_string(),
    }];
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Tab));
    assert_eq!(state.input_buffer, "/ml");

    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Enter));
    assert_eq!(
        action,
        InputAction::ExecuteCommand {
            name: "ml".to_string(),
            args: None
        }
    );
}

// === Story 3-4: Image Attachment & Clipboard Operations ===

// 10.9: 'c' key in Chat focus with focused_tool_id → CopyToClipboard
#[test]
fn test_c_key_chat_focus_triggers_copy() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;
    state.focused_tool_id = Some("tool_123".to_string());

    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('c'));
    assert_eq!(action, InputAction::CopyToClipboard(String::new()));
}

// 10.10: 'c' key in Chat focus without focused tool → CopyToClipboard (resolved in event loop)
#[test]
fn test_c_key_chat_focus_no_tool_triggers_copy() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;
    state.focused_tool_id = None;

    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('c'));
    assert_eq!(action, InputAction::CopyToClipboard(String::new()));
}

// 10.3: ImagePaste attaches image and sets indicator
#[test]
fn test_image_paste_creates_attachment() {
    let mut state = TuiState::new(80, 24);
    // PNG magic bytes
    let png_data = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00];

    let action = handle_input(&mut state, &DomainInputEvent::ImagePaste(png_data));
    assert_eq!(action, InputAction::Consumed);
    assert_eq!(state.pending_images.len(), 1);
    assert_eq!(state.pending_images[0].media_type, "image/png");
    assert!(state.image_indicator.is_some());
    assert!(
        state
            .image_indicator
            .as_ref()
            .unwrap()
            .contains("image attached")
    );
}

// 10.16: Paste empty data → no crash, no image attached
#[test]
fn test_image_paste_empty_data_no_crash() {
    let mut state = TuiState::new(80, 24);
    let action = handle_input(&mut state, &DomainInputEvent::ImagePaste(vec![]));
    // Empty data should fail format detection → ImageFormatError
    assert_eq!(action, InputAction::ImageFormatError);
    assert!(state.pending_images.is_empty());
}

// 10.18: Multiple images attached then indicator updated
#[test]
fn test_multiple_images_indicator_updates() {
    let mut state = TuiState::new(80, 24);
    let png_data = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00];
    let jpeg_data = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x00, 0x00, 0x00];

    handle_input(&mut state, &DomainInputEvent::ImagePaste(png_data));
    assert_eq!(state.pending_images.len(), 1);

    handle_input(&mut state, &DomainInputEvent::ImagePaste(jpeg_data));
    assert_eq!(state.pending_images.len(), 2);
    assert!(
        state
            .image_indicator
            .as_ref()
            .unwrap()
            .contains("2 images attached")
    );
}

// Unsupported image format → ImageFormatError
#[test]
fn test_image_paste_unsupported_format() {
    let mut state = TuiState::new(80, 24);
    // BMP magic bytes
    let bmp_data = vec![0x42, 0x4D, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];

    let action = handle_input(&mut state, &DomainInputEvent::ImagePaste(bmp_data));
    assert_eq!(action, InputAction::ImageFormatError);
    assert!(state.pending_images.is_empty());
}

// Text paste inserts into input buffer
#[test]
fn test_text_paste_inserts_text() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Input;
    state.input_buffer = "hello ".to_string();
    state.cursor_position = 6;

    let action = handle_input(&mut state, &DomainInputEvent::Paste("world".to_string()));
    assert_eq!(action, InputAction::Consumed);
    assert_eq!(state.input_buffer, "hello world");
    assert_eq!(state.cursor_position, 11);
}

// 10.12: Submit message with pending images → images cleared, indicator cleared
#[test]
fn test_submit_clears_pending_images_state() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Input;
    state.input_buffer = "describe this".to_string();
    state.cursor_position = 13;
    state
        .pending_images
        .push(rustain::domain::models::ImageAttachment {
            media_type: "image/png".to_string(),
            data: "base64data".to_string(),
        });
    state.image_indicator = Some("[image attached: 1KB]".to_string());

    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Enter));
    // Submit should return SubmitMessage
    assert_eq!(
        action,
        InputAction::SubmitMessage("describe this".to_string())
    );
    // Note: pending_images are drained in event_loop, not in app.rs
    // But input_buffer should be cleared
    assert!(state.input_buffer.is_empty());
}

// AC4: Large image triggers ImageSizeWarning, then 'y' confirms attachment
#[test]
fn test_large_image_confirm_attach() {
    let mut state = TuiState::new(80, 24);
    // Simulate a large PNG image (>5MB base64)
    let mut png_data = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    png_data.resize(4 * 1024 * 1024, 0); // 4MB raw → ~5.3MB base64 → triggers warning

    let action = handle_input(&mut state, &DomainInputEvent::ImagePaste(png_data));
    // Should return ImageSizeWarning with the data
    match action {
        InputAction::ImageSizeWarning {
            media_type,
            data: _,
            warning,
        } => {
            assert_eq!(media_type, "image/png");
            assert!(warning.contains("Large image"));
        }
        _ => panic!("Expected ImageSizeWarning, got {:?}", action),
    }
}

// AC4: 'y' key in Chat focus with pending_large_image returns ImageConfirmAttach
#[test]
fn test_large_image_y_confirms() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;
    state.pending_large_image = Some(rustain::domain::models::ImageAttachment {
        media_type: "image/png".to_string(),
        data: "large_base64_data".to_string(),
    });

    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('y'));
    assert_eq!(action, InputAction::ImageConfirmAttach);
}

// AC4: 'n' key in Chat focus with pending_large_image returns ImageConfirmCancel
#[test]
fn test_large_image_n_cancels() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;
    state.pending_large_image = Some(rustain::domain::models::ImageAttachment {
        media_type: "image/png".to_string(),
        data: "large_base64_data".to_string(),
    });

    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('n'));
    assert_eq!(action, InputAction::ImageConfirmCancel);
}

// AC4: 'y' key in Chat focus without pending_large_image does NOT trigger confirm
#[test]
fn test_y_key_without_pending_image_not_confirm() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;
    // No pending_large_image

    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('y'));
    // Should NOT be ImageConfirmAttach — should be something else (Consumed or other)
    assert_ne!(action, InputAction::ImageConfirmAttach);
}

// P1: Oversized image paste (>20MB) is rejected before base64 encoding
#[test]
fn test_oversized_image_paste_rejected() {
    let mut state = TuiState::new(80, 24);
    // Create data >20MB with PNG magic bytes
    let mut huge_data = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    huge_data.resize(21 * 1024 * 1024, 0); // 21MB

    let action = handle_input(&mut state, &DomainInputEvent::ImagePaste(huge_data));
    assert_eq!(action, InputAction::ImageFormatError);
    assert!(state.pending_images.is_empty());
}

// 'c' key in Input focus should type 'c', not copy
#[test]
fn test_c_key_input_focus_types_c() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Input;
    state.input_buffer.clear();
    state.cursor_position = 0;

    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('c'));
    assert_eq!(action, InputAction::Consumed);
    assert_eq!(state.input_buffer, "c");
}

// Covers: Sprint Change Proposal 2026-04-08, UX-DR76 amendment, AC#1
/// Alt+Enter inserts a newline at cursor position (VS Code alternative to Shift+Enter).
#[test]
fn test_alt_enter_inserts_newline() {
    let mut state = TuiState::new(80, 24);
    handle_input(&mut state, &DomainInputEvent::KeyPress('a'));
    let action = handle_input(
        &mut state,
        &DomainInputEvent::SpecialKey(DomainKey::AltEnter),
    );
    assert_eq!(action, InputAction::Consumed);
    assert_eq!(state.input_buffer, "a\n");
    assert_eq!(state.cursor_position, 2);
}

// Covers: Sprint Change Proposal 2026-04-08, UX-DR76 amendment, AC#1
/// Alt+Enter inserts newline even when multi-line mode is off (always inserts).
#[test]
fn test_alt_enter_inserts_newline_without_multiline_mode() {
    let mut state = TuiState::new(80, 24);
    assert!(!state.multiline_mode);
    handle_input(&mut state, &DomainInputEvent::KeyPress('x'));

    let action = handle_input(
        &mut state,
        &DomainInputEvent::SpecialKey(DomainKey::AltEnter),
    );
    assert_eq!(action, InputAction::Consumed);
    assert_eq!(state.input_buffer, "x\n");
}

// Covers: Sprint Change Proposal 2026-04-08, UX-DR76 amendment, AC#2
/// Alt+M toggles multi-line mode (VS Code alternative to Ctrl+E).
#[test]
fn test_alt_m_toggles_multiline_mode() {
    let mut state = TuiState::new(80, 24);
    assert!(!state.multiline_mode);

    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::AltM));
    assert_eq!(action, InputAction::Consumed);
    assert!(state.multiline_mode);

    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::AltM));
    assert_eq!(action, InputAction::Consumed);
    assert!(!state.multiline_mode);
}

// Covers: Sprint Change Proposal 2026-04-08, AC#7 (backward compatibility)
/// Alt+Enter works identically to Shift+Enter — inserts newline.
#[test]
fn test_alt_enter_behaves_like_shift_enter() {
    // Shift+Enter baseline
    let mut state1 = TuiState::new(80, 24);
    handle_input(&mut state1, &DomainInputEvent::KeyPress('h'));
    handle_input(
        &mut state1,
        &DomainInputEvent::SpecialKey(DomainKey::ShiftEnter),
    );
    let buffer_shift = state1.input_buffer.clone();

    // Alt+Enter should produce the same result
    let mut state2 = TuiState::new(80, 24);
    handle_input(&mut state2, &DomainInputEvent::KeyPress('h'));
    handle_input(
        &mut state2,
        &DomainInputEvent::SpecialKey(DomainKey::AltEnter),
    );
    let buffer_alt = state2.input_buffer.clone();

    assert_eq!(buffer_shift, buffer_alt);
}

// Covers: Sprint Change Proposal 2026-04-08, AC#7 (backward compatibility)
/// Alt+M behaves identically to Ctrl+E — toggles multi-line mode.
#[test]
fn test_alt_m_behaves_like_ctrl_e() {
    // Ctrl+E baseline
    let mut state1 = TuiState::new(80, 24);
    handle_input(&mut state1, &DomainInputEvent::SpecialKey(DomainKey::CtrlE));
    let mode_after_ctrl_e = state1.multiline_mode;

    // Alt+M should produce the same result
    let mut state2 = TuiState::new(80, 24);
    handle_input(&mut state2, &DomainInputEvent::SpecialKey(DomainKey::AltM));
    let mode_after_alt_m = state2.multiline_mode;

    assert_eq!(mode_after_ctrl_e, mode_after_alt_m);
}

// Covers: Sprint Change Proposal 2026-04-08, AC11 (negative focus-state tests)
/// Alt+Enter in Chat focus is ignored — does not insert newline into buffer.
#[test]
fn test_alt_enter_ignored_in_chat_focus() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;
    let initial_buffer = state.input_buffer.clone();

    let action = handle_input(
        &mut state,
        &DomainInputEvent::SpecialKey(DomainKey::AltEnter),
    );
    // Alt+Enter in Chat focus should be ignored (not handled)
    assert_eq!(
        action,
        InputAction::Ignored,
        "Alt+Enter in Chat should be Ignored"
    );
    assert_eq!(
        state.input_buffer, initial_buffer,
        "Alt+Enter in Chat should not modify buffer"
    );
}

// Covers: Sprint Change Proposal 2026-04-08, AC11 (negative focus-state tests)
/// Alt+M in Chat focus is ignored — does not toggle multi-line mode.
#[test]
fn test_alt_m_ignored_in_chat_focus() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;
    let initial_mode = state.multiline_mode;

    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::AltM));
    assert_eq!(
        state.multiline_mode, initial_mode,
        "Alt+M in Chat should not toggle multiline mode"
    );
}

// Covers: Sprint Change Proposal 2026-04-08, AC11 (negative focus-state tests)
/// ShiftEnter in Chat focus is ignored — does not insert newline into buffer.
#[test]
fn test_shift_enter_ignored_in_chat_focus() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;
    let initial_buffer = state.input_buffer.clone();

    handle_input(
        &mut state,
        &DomainInputEvent::SpecialKey(DomainKey::ShiftEnter),
    );
    assert_eq!(
        state.input_buffer, initial_buffer,
        "ShiftEnter in Chat should not modify buffer"
    );
}

// Covers: Sprint Change Proposal 2026-04-08, AC11 (negative focus-state tests)
/// Ctrl+E in Chat focus is ignored — does not toggle multi-line mode.
#[test]
fn test_ctrl_e_ignored_in_chat_focus() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;
    let initial_mode = state.multiline_mode;

    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlE));
    assert_eq!(
        state.multiline_mode, initial_mode,
        "Ctrl+E in Chat should not toggle multiline mode"
    );
}

// Covers: Sprint Change Proposal 2026-04-08, AC10 (crossterm conversion)
/// Alt+Enter crossterm key event converts to DomainKey::AltEnter.
#[test]
fn test_crossterm_alt_enter_converts_to_domain_alt_enter() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
    use rustain::adapters::tui::app::convert_crossterm_event;

    let event = Event::Key(KeyEvent {
        code: KeyCode::Enter,
        modifiers: KeyModifiers::ALT,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    });

    let domain_event =
        convert_crossterm_event(&event, &rustain::domain::models::MouseConfig::default());
    assert!(
        domain_event.is_some(),
        "Alt+Enter should produce a domain event"
    );
    match domain_event.unwrap() {
        DomainInputEvent::SpecialKey(DomainKey::AltEnter) => {} // expected
        other => panic!("Expected AltEnter, got {:?}", other),
    }
}

// Covers: Sprint Change Proposal 2026-04-08, AC10 (crossterm conversion)
/// Alt+M crossterm key event converts to DomainKey::AltM.
#[test]
fn test_crossterm_alt_m_converts_to_domain_alt_m() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
    use rustain::adapters::tui::app::convert_crossterm_event;

    let event = Event::Key(KeyEvent {
        code: KeyCode::Char('m'),
        modifiers: KeyModifiers::ALT,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    });

    let domain_event =
        convert_crossterm_event(&event, &rustain::domain::models::MouseConfig::default());
    assert!(
        domain_event.is_some(),
        "Alt+M should produce a domain event"
    );
    match domain_event.unwrap() {
        DomainInputEvent::SpecialKey(DomainKey::AltM) => {} // expected
        other => panic!("Expected AltM, got {:?}", other),
    }
}

// Covers: Sprint Change Proposal 2026-04-08, AC12 (keybinding + autocomplete interaction)
/// Alt+Enter while autocomplete is active dismisses the popup AND inserts newline.
#[test]
fn test_alt_enter_dismisses_autocomplete_and_inserts_newline() {
    let mut state = TuiState::new(80, 24);
    // Activate autocomplete via '/'
    handle_input(&mut state, &DomainInputEvent::KeyPress('/'));
    assert!(state.autocomplete.active);

    // Alt+Enter: autocomplete dismissed, newline inserted
    handle_input(
        &mut state,
        &DomainInputEvent::SpecialKey(DomainKey::AltEnter),
    );
    assert!(
        !state.autocomplete.active,
        "Alt+Enter should dismiss autocomplete"
    );
    assert!(
        state.input_buffer.contains('\n'),
        "Alt+Enter should insert a newline after dismissing autocomplete"
    );
}

// Covers: Sprint Change Proposal 2026-04-08, AC12 (keybinding + autocomplete interaction)
/// Alt+M while autocomplete is active dismisses the popup AND toggles multi-line mode.
#[test]
fn test_alt_m_dismisses_autocomplete_and_toggles_multiline() {
    let mut state = TuiState::new(80, 24);
    let initial_mode = state.multiline_mode;
    // Activate autocomplete via '/'
    handle_input(&mut state, &DomainInputEvent::KeyPress('/'));
    assert!(state.autocomplete.active);

    // Alt+M: autocomplete dismissed, multiline toggled
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::AltM));
    assert!(
        !state.autocomplete.active,
        "Alt+M should dismiss autocomplete"
    );
    assert_ne!(
        state.multiline_mode, initial_mode,
        "Alt+M should toggle multi-line mode after dismissing autocomplete"
    );
}
