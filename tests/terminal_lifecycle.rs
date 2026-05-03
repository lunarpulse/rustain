//! Terminal lifecycle tests.
//!
//! Note: Tests that require a real terminal (setup/teardown with raw mode)
//! are marked #[ignore] and can be run manually with `cargo test -- --ignored`.
//! CI runs only non-ignored tests.

use rustain::adapters::tui::terminal;

/// AC2: terminal::setup() returns a functional Terminal on a real terminal.
/// AC3: terminal::teardown() restores the terminal cleanly.
/// Ignored in CI -- requires a real terminal (TTY).
// Covers: FR105 (crash safety)
#[test]
#[ignore]
fn test_terminal_setup_and_teardown() {
    // Setup should succeed on a real terminal
    let result = terminal::setup(true);
    assert!(
        result.is_ok(),
        "terminal::setup() failed: {:?}",
        result.err()
    );

    let tui = result.unwrap();

    // Should be able to get terminal size
    let size = tui.size();
    assert!(size.is_ok());
    let size = size.unwrap();
    assert!(size.width > 0 && size.height > 0);

    // Teardown should succeed
    let teardown_result = terminal::teardown(true);
    assert!(
        teardown_result.is_ok(),
        "terminal::teardown() failed: {:?}",
        teardown_result.err()
    );
}

/// AC3: restore_terminal_raw() does not panic even when called
/// outside of raw mode (idempotent safety).
// Covers: FR105 (crash safety)
#[test]
fn test_restore_terminal_raw_is_safe_outside_raw_mode() {
    // Should not panic even if terminal is not in raw mode
    terminal::restore_terminal_raw();
    // If we get here without panic, the test passes
}
