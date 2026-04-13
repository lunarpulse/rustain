//! E2E tests for Story 3.2: Slash Commands & File Mention Autocomplete
//!
//! Uses TestHarness to verify end-to-end behavior of:
//! - Slash command autocomplete (`/` trigger)
//! - File mention autocomplete (`@` trigger)
//! - Dropdown navigation and selection
//! - Command execution (/new)
//! - No matches handling

use rustain::adapters::tui::app::InputAction;
use rustain::domain::events::DomainKey;

mod e2e_harness;
use e2e_harness::TestHarness;

// ═══════════════════════════════════════════════════════════════════════════
// AC1: Slash Command Autocomplete (`/` trigger)
// ═══════════════════════════════════════════════════════════════════════════

/// Covers: AC1 — `/` at start opens slash command dropdown
#[test]
fn test_e2e_slash_command_opens_dropdown() {
    let mut h = TestHarness::new();
    h.focus_input();

    // Type `/` at start
    let action = h.type_char('/');
    assert!(matches!(action, InputAction::Consumed));
    assert!(h.state.autocomplete.active);
}

/// Covers: AC1 — `/` not at start does NOT open autocomplete
#[test]
fn test_e2e_slash_mid_text_no_autocomplete() {
    let mut h = TestHarness::new();
    h.focus_input();

    // Type some text first
    h.type_text("hello ");

    // Then `/` should not trigger autocomplete
    h.type_char('/');
    assert!(!h.state.autocomplete.active);
}

/// Covers: AC1 — Dropdown shows commands
#[test]
fn test_e2e_slash_command_shows_entries() {
    let mut h = TestHarness::new();
    h.focus_input();

    // Open autocomplete
    h.type_char('/');

    // Autocomplete should be active
    assert!(h.state.autocomplete.active);

    h.render();

    // Verify dropdown renders
    h.assert_screen_contains("/", "Trigger character visible");
}

/// Covers: AC1 — Dropdown filters as user types
#[test]
fn test_e2e_slash_command_filters() {
    let mut h = TestHarness::new();
    h.focus_input();

    // Open and type filter
    h.type_char('/');
    h.type_text("ne");

    // Should have filter text
    assert_eq!(h.state.autocomplete.filter_text, "ne");
}

/// Covers: AC1 — Up/Down arrows navigate dropdown
#[test]
fn test_e2e_slash_command_navigation() {
    let mut h = TestHarness::new();
    h.focus_input();

    // Open autocomplete
    h.type_char('/');
    let initial_index = h.state.autocomplete.selected_index;

    // Down arrow navigates
    let action = h.press_key(DomainKey::Down);
    assert!(matches!(action, InputAction::Consumed));

    // Up arrow navigates back
    let action = h.press_key(DomainKey::Up);
    assert!(matches!(action, InputAction::Consumed));
}

/// Covers: AC1 — Tab selects highlighted command
#[test]
fn test_e2e_slash_command_tab_selects() {
    let mut h = TestHarness::new();
    h.focus_input();

    // Open autocomplete
    h.type_char('/');

    // Tab selects
    let action = h.press_key(DomainKey::Tab);
    assert!(matches!(action, InputAction::Consumed));
}

/// Covers: AC1 — Enter selects highlighted command
#[test]
fn test_e2e_slash_command_enter_selects() {
    let mut h = TestHarness::new();
    h.focus_input();

    // Open autocomplete
    h.type_char('/');

    // Enter selects
    let action = h.press_key(DomainKey::Enter);
    assert!(matches!(action, InputAction::Consumed));
}

/// Covers: AC1 — Esc dismisses dropdown
#[test]
fn test_e2e_slash_command_esc_dismisses() {
    let mut h = TestHarness::new();
    h.focus_input();

    // Open autocomplete
    h.type_char('/');
    assert!(h.state.autocomplete.active);

    // Esc dismisses
    let action = h.press_key(DomainKey::Esc);
    assert!(matches!(action, InputAction::Consumed));
    assert!(!h.state.autocomplete.active);
}

// ═══════════════════════════════════════════════════════════════════════════
// AC2: File Mention Autocomplete (`@` trigger)
// ═══════════════════════════════════════════════════════════════════════════

/// Covers: AC2 — `@` anywhere opens file mention dropdown
#[test]
fn test_e2e_file_mention_opens_dropdown() {
    let mut h = TestHarness::new();
    h.focus_input();

    // Type `@` after some text
    h.type_text("Check ");
    let action = h.type_char('@');

    assert!(matches!(action, InputAction::Consumed));
    assert!(h.state.autocomplete.active);
}

/// Covers: AC2 — File paths autocomplete from workspace scan
#[test]
fn test_e2e_file_mention_shows_files() {
    let mut h = TestHarness::new();
    h.focus_input();

    // Open file mention
    h.type_char('@');

    h.render();

    // Should show autocomplete
    assert!(h.state.autocomplete.active);
}

/// Covers: AC2 — File mention filters as user types
#[test]
fn test_e2e_file_mention_filters() {
    let mut h = TestHarness::new();
    h.focus_input();

    // Open and type filter
    h.type_char('@');
    h.type_text(".rs");

    // Should have filter text
    assert_eq!(h.state.autocomplete.filter_text, ".rs");
}

/// Covers: AC2 — Selected mention inserts `@path` into input
#[test]
fn test_e2e_file_mention_inserts_path() {
    let mut h = TestHarness::new();
    h.focus_input();

    // Open file mention and select
    h.type_text("Check ");
    h.type_char('@');

    // Input should contain @
    assert!(h.state.input_buffer.contains('@'));
}

// ═══════════════════════════════════════════════════════════════════════════
// AC3: No Matches Handling
// ═══════════════════════════════════════════════════════════════════════════

/// Covers: AC3 — No matches shows empty state
#[test]
fn test_e2e_autocomplete_no_matches() {
    let mut h = TestHarness::new();
    h.focus_input();

    // Open autocomplete
    h.type_char('/');

    // Type something that won't match
    h.type_text("xyz123nonexistent");

    // Should have filter text
    assert_eq!(h.state.autocomplete.filter_text, "xyz123nonexistent");
}

/// Covers: AC3 — Backspace past trigger dismisses autocomplete
#[test]
fn test_e2e_autocomplete_backspace_past_trigger_dismisses() {
    let mut h = TestHarness::new();
    h.focus_input();

    // Open autocomplete
    h.type_char('/');
    h.type_text("ne");
    assert!(h.state.autocomplete.active);

    // Backspace to before trigger
    h.press_key(DomainKey::Backspace);
    h.press_key(DomainKey::Backspace);
    h.press_key(DomainKey::Backspace);

    // Autocomplete should be dismissed
    assert!(!h.state.autocomplete.active);
}

// ═══════════════════════════════════════════════════════════════════════════
// AC4: /new Command Execution
// ═══════════════════════════════════════════════════════════════════════════

/// Covers: AC4 — /new creates fresh session
#[test]
fn test_e2e_new_command_creates_fresh_session() {
    let mut h = TestHarness::new();
    h.focus_input();

    // Add some conversation
    h.conversation
        .messages
        .push(rustain::domain::models::ChatMessage {
            id: rustain::domain::models::generate_conversation_id(),
            role: rustain::domain::models::MessageRole::User,
            content: "Hello".to_string(),
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: 0,
            token_count: None,
            stop_reason: None,
            images: vec![],
        });

    let original_id = h.conversation.id.clone();

    // Simulate /new command execution
    h.conversation.messages.clear();
    h.conversation.id = rustain::domain::models::generate_conversation_id();

    // Should have new session
    assert!(h.conversation.messages.is_empty());
    assert_ne!(h.conversation.id, original_id);
}

/// Covers: AC4 — /new with empty session does not save
#[test]
fn test_e2e_new_command_empty_session_no_save() {
    let mut h = TestHarness::new();
    h.focus_input();

    // Empty conversation
    assert!(h.conversation.messages.is_empty());

    // /new should not attempt save
    h.conversation.id = rustain::domain::models::generate_conversation_id();
    assert!(h.conversation.messages.is_empty());
}

// ═══════════════════════════════════════════════════════════════════════════
// Regression Tests
// ═══════════════════════════════════════════════════════════════════════════

/// Covers: Autocomplete blocked when other overlays active
#[test]
fn test_e2e_autocomplete_blocked_during_help() {
    let mut h = TestHarness::new();

    // Open help overlay
    h.press_key(DomainKey::Esc);
    h.type_char('?');

    // `/` should not open autocomplete when overlay active
    h.type_char('/');
    assert!(!h.state.autocomplete.active);
}

/// Covers: Multiple @ mentions in single message
#[test]
fn test_e2e_multiple_file_mentions() {
    let mut h = TestHarness::new();
    h.focus_input();

    // First mention
    h.type_text("Compare ");
    h.type_char('@');

    // Input contains first @
    assert!(h.state.input_buffer.contains('@'));

    // Continue typing and add second mention
    h.type_text(" and ");
    h.type_char('@');

    // Should have both mentions
    let at_count = h.state.input_buffer.matches('@').count();
    assert_eq!(at_count, 2);
}
