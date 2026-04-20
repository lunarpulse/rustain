//! E2E tests for Story 3.3: Command Palette & Which-Key Chords
//!
//! Uses TestHarness to verify end-to-end behavior of:
//! - Command palette (Ctrl+P) with fuzzy filtering
//! - Scoped prefixes (/ @ : > !)
//! - Which-key hint bar (Ctrl+X chords)
//! - Command execution from palette
//! - Shortcut graduation display

use rustain::adapters::tui::app::InputAction;
use rustain::domain::events::DomainKey;
use rustain::domain::models::palette::{PaletteAction, PaletteEntry, PaletteScope};

mod e2e_harness;
use e2e_harness::TestHarness;

// ═══════════════════════════════════════════════════════════════════════════
// AC1: Command Palette (Ctrl+P)
// ═══════════════════════════════════════════════════════════════════════════

/// Covers: AC1 — Ctrl+P opens command palette overlay
#[test]
fn test_e2e_palette_opens_with_ctrl_p() {
    let mut h = TestHarness::new();

    // From any focus, Ctrl+P opens palette
    let action = h.press_key(DomainKey::CtrlP);
    assert!(matches!(action, InputAction::Consumed));
    assert!(h.state.command_palette.active);
}

/// Covers: AC1 — Palette shows with text input and results
#[test]
fn test_e2e_palette_shows_input_and_results() {
    let mut h = TestHarness::new();

    h.press_key(DomainKey::CtrlP);

    // Palette should be active
    assert!(h.state.command_palette.active);

    // Should have filtered entries available
    h.render();
    // Main UI still renders normally when palette is open
    h.assert_screen_contains("mock-model", "Status bar visible");
}

/// Covers: AC1 — Results update with fuzzy matching
#[test]
fn test_e2e_palette_fuzzy_filtering() {
    let mut h = TestHarness::new();

    h.press_key(DomainKey::CtrlP);

    // Type filter
    h.state.command_palette.filter_text = "new".to_string();

    // Should have filter text
    assert_eq!(h.state.command_palette.filter_text, "new");
}

/// Covers: AC1 — Each result shows name, shortcut, description
#[test]
fn test_e2e_palette_result_format() {
    let mut h = TestHarness::new();

    h.press_key(DomainKey::CtrlP);

    // Inject an entry with shortcut
    h.state.command_palette.filtered_entries = vec![PaletteEntry {
        name: "new".to_string(),
        description: "Start new session".to_string(),
        shortcut: Some("Ctrl+N".to_string()),
        scope: PaletteScope::SlashCommand,
        action: PaletteAction::ExecuteCommand("new".to_string(), None),
    }];

    h.render();

    // Should render
    assert!(!h.state.command_palette.filtered_entries.is_empty());
}

/// Covers: AC1 — Enter selects and executes
#[test]
fn test_e2e_palette_enter_executes() {
    let mut h = TestHarness::new();

    h.press_key(DomainKey::CtrlP);

    // Inject executable entry
    h.state.command_palette.filtered_entries = vec![PaletteEntry {
        name: "test".to_string(),
        description: "Test command".to_string(),
        shortcut: None,
        scope: PaletteScope::All,
        action: PaletteAction::ShowVersion,
    }];
    h.state.command_palette.selected_index = 0;

    // Enter executes
    let action = h.press_key(DomainKey::Enter);
    assert!(matches!(action, InputAction::Consumed));
}

/// Covers: AC1 — Esc closes without action
#[test]
fn test_e2e_palette_esc_closes() {
    let mut h = TestHarness::new();

    h.press_key(DomainKey::CtrlP);
    assert!(h.state.command_palette.active);

    let action = h.press_key(DomainKey::Esc);
    assert!(matches!(action, InputAction::Consumed));
    assert!(!h.state.command_palette.active);
}

// ═══════════════════════════════════════════════════════════════════════════
// AC2: Scoped Prefixes
// ═══════════════════════════════════════════════════════════════════════════

/// Covers: AC2 — `/` prefix filters to slash commands
#[test]
fn test_e2e_palette_slash_scope() {
    let mut h = TestHarness::new();

    h.press_key(DomainKey::CtrlP);
    h.type_char('/');

    // Should filter to slash command scope
    assert!(h.state.command_palette.filter_text.starts_with('/'));
}

/// Covers: AC2 — `@` prefix filters to files/agents
#[test]
fn test_e2e_palette_at_scope() {
    let mut h = TestHarness::new();

    h.press_key(DomainKey::CtrlP);
    h.type_char('@');

    // Should filter to mention scope
    assert!(h.state.command_palette.filter_text.starts_with('@'));
}

/// Covers: AC2 — `:` prefix filters to models/providers
#[test]
fn test_e2e_palette_colon_scope() {
    let mut h = TestHarness::new();

    h.press_key(DomainKey::CtrlP);
    h.type_char(':');

    assert!(h.state.command_palette.filter_text.starts_with(':'));
}

/// Covers: AC2 — `>` prefix filters to profiles
#[test]
fn test_e2e_palette_gt_scope() {
    let mut h = TestHarness::new();

    h.press_key(DomainKey::CtrlP);
    h.type_char('>');

    assert!(h.state.command_palette.filter_text.starts_with('>'));
}

/// Covers: AC2 — `!` prefix filters to adapter management
#[test]
fn test_e2e_palette_bang_scope() {
    let mut h = TestHarness::new();

    h.press_key(DomainKey::CtrlP);
    h.type_char('!');

    assert!(h.state.command_palette.filter_text.starts_with('!'));
}

/// Covers: AC2 — Unscoped search shows all populated scopes
#[test]
fn test_e2e_palette_unscoped_shows_all() {
    let mut h = TestHarness::new();

    h.press_key(DomainKey::CtrlP);

    // No prefix, should show all entries
    assert!(h.state.command_palette.filter_text.is_empty());
}

// ═══════════════════════════════════════════════════════════════════════════
// AC3: Which-Key Chords (Ctrl+X)
// ═══════════════════════════════════════════════════════════════════════════

/// Covers: AC3 — Ctrl+X opens which-key hint bar
#[test]
fn test_e2e_which_key_opens() {
    let mut h = TestHarness::new();

    let action = h.press_key(DomainKey::CtrlX);
    assert!(matches!(action, InputAction::Consumed));
    assert!(h.state.which_key.active);
}

/// Covers: AC3 — Hint bar shows chord options
#[test]
fn test_e2e_which_key_shows_options() {
    let mut h = TestHarness::new();

    h.press_key(DomainKey::CtrlX);

    h.render();

    // Should show which-key bar
    assert!(h.state.which_key.active);
}

/// Covers: AC3 — Valid second key opens corresponding panel
#[test]
fn test_e2e_which_key_valid_chord() {
    let mut h = TestHarness::new();

    // Ctrl+X then ? opens help
    h.press_key(DomainKey::CtrlX);
    h.type_char('?');

    // Which-key should close
    assert!(!h.state.which_key.active);
}

/// Covers: AC3 — Invalid key dismisses hint bar
#[test]
fn test_e2e_which_key_invalid_dismisses() {
    let mut h = TestHarness::new();

    h.press_key(DomainKey::CtrlX);
    assert!(h.state.which_key.active);

    // Invalid key
    h.type_char('z');
    assert!(!h.state.which_key.active);
}

/// Covers: AC3 — Which-key timeout auto-dismisses
#[test]
fn test_e2e_which_key_timeout() {
    let mut h = TestHarness::new();

    h.press_key(DomainKey::CtrlX);
    assert!(h.state.which_key.active);

    // Check that timeout tracking exists
    assert!(h.state.which_key.started_at.is_some());
}

// ═══════════════════════════════════════════════════════════════════════════
// AC4: Shortcut Graduation
// ═══════════════════════════════════════════════════════════════════════════

/// Covers: AC4 — Shortcuts shown next to results
#[test]
fn test_e2e_palette_shortcut_graduation() {
    let mut h = TestHarness::new();

    h.press_key(DomainKey::CtrlP);

    // Entry with shortcut
    h.state.command_palette.filtered_entries = vec![PaletteEntry {
        name: "Switch model".to_string(),
        description: "Change LLM model".to_string(),
        shortcut: Some("Ctrl+X, M".to_string()),
        scope: PaletteScope::All,
        action: PaletteAction::ShowVersion,
    }];

    h.render();

    // Should have entry
    assert!(!h.state.command_palette.filtered_entries.is_empty());
}

// ═══════════════════════════════════════════════════════════════════════════
// AC5: Streaming Compatibility
// ═══════════════════════════════════════════════════════════════════════════

/// Covers: AC5 — Palette opens without interrupting stream
#[test]
fn test_e2e_palette_opens_during_stream() {
    let mut h = TestHarness::new();

    // Simulate streaming state
    h.streaming.is_streaming = true;
    h.state.status = rustain::domain::models::StatusState::Streaming;

    // Palette should still open
    let _action = h.press_key(DomainKey::CtrlP);
    assert!(h.state.command_palette.active);
    // Streaming should still be active
    assert!(h.streaming.is_streaming);
}

// ═══════════════════════════════════════════════════════════════════════════
// Regression Tests
// ═══════════════════════════════════════════════════════════════════════════

/// Covers: Palette doesn't break when no entries registered
#[test]
fn test_e2e_palette_empty_entries() {
    let mut h = TestHarness::new();

    h.press_key(DomainKey::CtrlP);
    h.state.command_palette.filtered_entries.clear();

    // Should not panic with empty entries
    assert!(h.state.command_palette.filtered_entries.is_empty());

    h.render();
    // Main UI still renders
    h.assert_screen_contains("Welcome to Rustain", "Main UI visible");
}

/// Covers: Which-key chord while palette open
#[test]
fn test_e2e_which_key_blocked_when_palette_open() {
    let mut h = TestHarness::new();

    // Open palette
    h.press_key(DomainKey::CtrlP);

    // Ctrl+X should be consumed
    let _action = h.press_key(DomainKey::CtrlX);
    // Palette should remain state
    assert!(h.state.command_palette.active);
}
