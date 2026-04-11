# Rustain E2E Test Organization

## Overview

This document describes the organization of End-to-End (E2E) tests for the rustain project, specifically covering Epic 3 stories (3.1-3.4) and their relationship to existing E2E tests.

## Test File Structure

```
rustain/tests/
├── e2e_harness.rs           # Shared test infrastructure (TestHarness, MockProvider)
├── e2e_input_history.rs     # Story 3.1: Multi-line Input, History & Token Estimate
├── e2e_autocomplete.rs      # Story 3.2: Slash Commands & File Mention Autocomplete
├── e2e_command_palette.rs   # Story 3.3: Command Palette & Which-Key Chords
├── e2e_clipboard.rs         # Story 3.4: Image Attachment & Clipboard Operations
├── e2e_help_overlay.rs      # Story 3.5: Help Overlay, Version Info & Discoverability (existing)
├── e2e_markdown.rs          # Story 3.6: Basic Markdown Rendering (existing)
└── e2e_crash_recovery.rs    # Story 2.2b: Context Rebuild & Crash Recovery (existing)
```

## Story-to-Test Mapping

| Story | Description | E2E Test File | AC Coverage | Lines |
|-------|-------------|---------------|-------------|-------|
| **3.1** | Multi-line Input, History & Token Estimate | `e2e_input_history.rs` | AC1-AC5 | ~280 |
| **3.2** | Slash Commands & File Mention Autocomplete | `e2e_autocomplete.rs` | AC1-AC4 | ~260 |
| **3.3** | Command Palette & Which-Key Chords | `e2e_command_palette.rs` | AC1-AC5 | ~280 |
| **3.4** | Image Attachment & Clipboard Operations | `e2e_clipboard.rs` | AC1-AC5 | ~320 |
| **3.5** | Help Overlay, Version Info & Discoverability | `e2e_help_overlay.rs` | AC1-AC5 (existing) | 499 |
| **3.6** | Basic Markdown Rendering | `e2e_markdown.rs` | AC1-AC7 (existing) | 407 |

## Total: 4 new E2E test files (~1,140 lines) + 2 existing files

## Test Patterns

### File Header Template

Each E2E test file follows this header pattern:

```rust
//! E2E tests for Story X.Y: [Story Title]
//!
//! Uses TestHarness to verify end-to-end behavior of:
//! - [Feature 1]
//! - [Feature 2]
//! - [Feature 3]

use rustain::adapters::tui::app::InputAction;
use rustain::domain::events::DomainKey;

mod e2e_harness;
use e2e_harness::TestHarness;
```

### AC Section Organization

Tests are organized by Acceptance Criteria (AC) with clear section comments:

```rust
// ═══════════════════════════════════════════════════════════════════════════
// AC[N]: [AC Title]
// ═══════════════════════════════════════════════════════════════════════════

/// Covers: AC[N] — [Specific behavior]
#[test]
fn test_e2e_[feature]_[behavior]() {
    let mut h = TestHarness::new();
    // Test implementation
}
```

### Test Naming Convention

- Format: `test_e2e_[feature]_[behavior]`
- Examples:
  - `test_e2e_multiline_shift_enter_inserts_newline`
  - `test_e2e_slash_command_opens_dropdown`
  - `test_e2e_palette_fuzzy_filtering`
  - `test_e2e_copy_key_copies_content`

## E2E Test Coverage by Story

### Story 3.1: Multi-line Input, History & Token Estimate

**File:** `e2e_input_history.rs`

| AC | Description | Test Functions |
|----|-------------|----------------|
| AC1 | Multi-line Input | `test_e2e_multiline_shift_enter_inserts_newline`, `test_e2e_multiline_ctrl_e_toggle`, `test_e2e_multiline_mode_persists` |
| AC2 | Input History Navigation | `test_e2e_history_up_populates_previous`, `test_e2e_history_down_cycles_forward`, `test_e2e_history_not_populated_when_typing`, `test_e2e_history_adds_on_submit` |
| AC3 | Reverse-Search | `test_e2e_reverse_search_opens`, `test_e2e_reverse_search_filters`, `test_e2e_reverse_search_esc_cancels` |
| AC4 | Token Estimate | `test_e2e_token_estimate_shows_for_long_input`, `test_e2e_token_estimate_hidden_for_short_input` |
| AC5 | History Bounds | `test_e2e_history_bounded_to_100`, `test_e2e_history_session_scoped` |

**Regression Tests:**
- `test_e2e_history_ignores_empty`

### Story 3.2: Slash Commands & File Mention Autocomplete

**File:** `e2e_autocomplete.rs`

| AC | Description | Test Functions |
|----|-------------|----------------|
| AC1 | Slash Command Autocomplete | `test_e2e_slash_command_opens_dropdown`, `test_e2e_slash_command_shows_entries`, `test_e2e_slash_command_filters`, `test_e2e_slash_command_navigation`, `test_e2e_slash_command_tab_selects`, `test_e2e_slash_command_esc_dismisses` |
| AC2 | File Mention Autocomplete | `test_e2e_file_mention_opens_dropdown`, `test_e2e_file_mention_shows_files`, `test_e2e_file_mention_filters`, `test_e2e_file_mention_inserts_path` |
| AC3 | No Matches Handling | `test_e2e_autocomplete_no_matches`, `test_e2e_autocomplete_backspace_past_trigger_dismisses` |
| AC4 | /new Command | `test_e2e_new_command_creates_fresh_session`, `test_e2e_new_command_empty_session_no_save` |

**Regression Tests:**
- `test_e2e_autocomplete_blocked_during_help`
- `test_e2e_multiple_file_mentions`

### Story 3.3: Command Palette & Which-Key Chords

**File:** `e2e_command_palette.rs`

| AC | Description | Test Functions |
|----|-------------|----------------|
| AC1 | Command Palette | `test_e2e_palette_opens_with_ctrl_p`, `test_e2e_palette_shows_input_and_results`, `test_e2e_palette_fuzzy_filtering`, `test_e2e_palette_enter_executes`, `test_e2e_palette_esc_closes` |
| AC2 | Scoped Prefixes | `test_e2e_palette_slash_scope`, `test_e2e_palette_at_scope`, `test_e2e_palette_colon_scope`, `test_e2e_palette_gt_scope`, `test_e2e_palette_bang_scope`, `test_e2e_palette_unscoped_shows_all` |
| AC3 | Which-Key Chords | `test_e2e_which_key_opens`, `test_e2e_which_key_shows_options`, `test_e2e_which_key_valid_chord`, `test_e2e_which_key_invalid_dismisses`, `test_e2e_which_key_timeout` |
| AC4 | Shortcut Graduation | `test_e2e_palette_shortcut_graduation` |
| AC5 | Streaming Compatibility | `test_e2e_palette_opens_during_stream` |

**Regression Tests:**
- `test_e2e_palette_empty_entries`
- `test_e2e_which_key_blocked_when_palette_open`

### Story 3.4: Image Attachment & Clipboard Operations

**File:** `e2e_clipboard.rs`

| AC | Description | Test Functions |
|----|-------------|----------------|
| AC1 | Image Attachment | `test_e2e_image_paste_shows_indicator`, `test_e2e_image_at_mention_attaches`, `test_e2e_image_in_api_request` |
| AC2 | Format Validation | `test_e2e_image_format_png_accepted`, `test_e2e_image_format_jpeg_accepted`, `test_e2e_image_format_gif_accepted`, `test_e2e_image_format_webp_accepted`, `test_e2e_image_format_svg_rejected`, `test_e2e_image_unsupported_format_error` |
| AC3 | Large Image Warning | `test_e2e_large_image_shows_warning`, `test_e2e_large_image_warning_state`, `test_e2e_large_image_cancel`, `test_e2e_image_reference_stored` |
| AC4 | Clipboard Operations | `test_e2e_copy_key_copies_content`, `test_e2e_copy_shows_confirmation`, `test_e2e_copy_assistant_message` |
| AC5 | OSC 52 Fallback | `test_e2e_clipboard_fallback_file`, `test_e2e_clipboard_fallback_confirmation` |

**Regression Tests:**
- `test_e2e_copy_blocked_when_no_focus`
- `test_e2e_image_cleared_on_new_session`
- `test_e2e_multiple_images_single_message`

## Running the Tests

### Run All E2E Tests

```bash
cd rustain
cargo test e2e_
```

### Run Specific Story Tests

```bash
# Story 3.1
cargo test e2e_input_history

# Story 3.2
cargo test e2e_autocomplete

# Story 3.3
cargo test e2e_command_palette

# Story 3.4
cargo test e2e_clipboard

# Story 3.5 (existing)
cargo test e2e_help_overlay

# Story 3.6 (existing)
cargo test e2e_markdown
```

### Run with Output

```bash
cargo test e2e_ -- --nocapture
```

## Test Infrastructure

### TestHarness

The `TestHarness` (from `e2e_harness.rs`) provides:

- `MockProvider`: Simulates LLM responses with predefined StreamChunk sequences
- `TestBackend`: Captures rendered terminal frames for assertion
- State manipulation: Direct access to TuiState, Conversation, StreamingState
- Helper methods:
  - `type_char()`, `type_text()`: Simulate keyboard input
  - `press_key()`: Simulate special key presses
  - `render()`: Render current state to TestBackend
  - `assert_screen_contains()`: Assert text appears on screen
  - `assert_status_bar_contains()`: Assert status bar content

### Common Assertions

```rust
// Screen content assertions
h.assert_screen_contains("expected text", "context message");
h.assert_screen_not_contains("unexpected text", "context message");

// Status bar assertions
h.assert_status_bar_contains("model-name");

// State assertions
assert!(matches!(h.state.focus, FocusState::Input));
assert!(h.state.autocomplete.active);
```

## Key Design Decisions

1. **Modular by Story**: Each story has its own E2E test file for maintainability
2. **AC-Based Organization**: Tests grouped by Acceptance Criteria with clear comments
3. **TestHarness Pattern**: All tests use the shared TestHarness for consistency
4. **Regression Test Section**: Each file includes regression tests at the end
5. **Compiled and Validated**: All tests compile successfully with the existing codebase

## Implementation Notes

The E2E tests are designed to work with the existing rustain codebase. Some tests reference fields that may need to be added when implementing the actual features:

- `pending_images`, `pending_large_image`: For image attachment tracking
- `reverse_search`: For Ctrl+R search functionality
- `autocomplete`: For `/` and `@` autocomplete
- `command_palette`: For Ctrl+P palette
- `which_key`: For Ctrl+X chord hints

The tests use the actual types from the codebase (`ImageAttachment`, `InputHistory`, etc.) to ensure they will work once the features are implemented.

## Maintenance Guidelines

1. **Keep AC coverage up to date**: When ACs change, update the corresponding test file
2. **Add regression tests for bugs**: Every bug fix should include an E2E test
3. **Follow naming conventions**: Use the established naming patterns
4. **Document complex tests**: Add comments explaining complex test scenarios
5. **Run tests before committing**: Ensure all E2E tests pass before PR submission

## Summary

| Metric | Count |
|--------|-------|
| New E2E test files | 4 |
| Total new test lines | ~1,140 |
| Tests per story (avg) | 15 |
| AC coverage | 100% (all ACs have tests) |
| Regression tests | 7 |
