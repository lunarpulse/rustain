//! Integration tests for CLI flags (Story 2.2b).

use clap::Parser;
use rustain::adapters::cli::commands::Cli;

/// 7.4: --new sets new_session flag.
// Covers: FR10 (session persistence)
#[test]
fn test_cli_new_flag() {
    let cli = Cli::parse_from(["rustain", "--new"]);
    assert!(cli.new);
    assert!(cli.session.is_none());
}

/// 7.4: --session sets session_id.
// Covers: FR10 (session persistence)
#[test]
fn test_cli_session_flag() {
    let cli = Cli::parse_from(["rustain", "--session", "abc-123"]);
    assert!(!cli.new);
    assert_eq!(cli.session, Some("abc-123".to_string()));
}

/// Default: no --new, no --session.
// Covers: FR10 (session persistence)
#[test]
fn test_cli_default_flags() {
    let cli = Cli::parse_from(["rustain"]);
    assert!(!cli.new);
    assert!(cli.session.is_none());
    assert_eq!(cli.log_level, "info");
}

/// --log-level still works alongside new flags.
// Covers: FR10 (session persistence)
#[test]
fn test_cli_combined_flags() {
    let cli = Cli::parse_from(["rustain", "--log-level", "debug", "--new"]);
    assert!(cli.new);
    assert_eq!(cli.log_level, "debug");
}
