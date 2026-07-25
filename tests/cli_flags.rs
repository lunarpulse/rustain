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
    assert!(
        cli.log_level.is_none(),
        "no --log-level flag => None (Story 8.1 D5)"
    );
}

/// --log-level still works alongside new flags.
// Covers: FR10 (session persistence)
#[test]
fn test_cli_combined_flags() {
    let cli = Cli::parse_from(["rustain", "--log-level", "debug", "--new"]);
    assert!(cli.new);
    assert_eq!(cli.log_level.as_deref(), Some("debug"));
}

#[test]
fn serve_a2a_flag_uses_the_loopback_default() {
    let cli = Cli::parse_from(["rustain", "--serve-a2a"]);
    assert_eq!(cli.serve_a2a.as_deref(), Some("127.0.0.1:8080"));
}

#[test]
fn serve_a2a_flag_accepts_an_explicit_loopback_address() {
    let cli = Cli::parse_from(["rustain", "--serve-a2a=127.0.0.2:0"]);
    assert_eq!(cli.serve_a2a.as_deref(), Some("127.0.0.2:0"));
}

/// Clap ACCEPTS the combination — the flag is `global` so Story 18-1b can
/// compose it with daemon mode later. It is not composable *today*: startup's
/// `evaluate_serve_a2a_combination` refuses it before any command intercept
/// runs, because the intercepts return in source order and would otherwise
/// silently drop one of the two modes. This test pins the parse shape only;
/// the refusal is covered by the startup unit test of the same name.
#[test]
fn serve_a2a_flag_parses_alongside_a_subcommand_but_is_not_composable_yet() {
    let cli = Cli::parse_from(["rustain", "--serve-a2a", "daemon", "start"]);
    assert_eq!(cli.serve_a2a.as_deref(), Some("127.0.0.1:8080"));
    assert!(matches!(
        cli.command,
        Some(rustain::adapters::cli::commands::Command::Daemon { .. })
    ));
}
