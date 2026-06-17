//! Conformance ratchets for `rustain session` CLI subcommands (Story 13.5a).

use clap::CommandFactory;

/// Ratchet: EXPECTED_SESSION_SUBCOMMANDS = 1 (`list`). 13.5b bumps to 2 (`delete`).
#[test]
fn test_session_subcommand_count_is_one() {
    const EXPECTED_SESSION_SUBCOMMANDS: usize = 1;
    let cmd = rustain::adapters::cli::commands::Cli::command();
    let session_cmd = cmd
        .find_subcommand("session")
        .expect("'session' subcommand should exist");
    let subcommand_count = session_cmd.get_subcommands().count();
    assert_eq!(
        subcommand_count, EXPECTED_SESSION_SUBCOMMANDS,
        "Expected {EXPECTED_SESSION_SUBCOMMANDS} session subcommand(s) (list), found {subcommand_count}."
    );
}

/// Ratchet: `session list` subcommand exists and exposes local --json.
#[test]
fn test_session_list_subcommand_exists() {
    let cmd = rustain::adapters::cli::commands::Cli::command();
    let session_cmd = cmd
        .find_subcommand("session")
        .expect("'session' subcommand should exist");
    let list_cmd = session_cmd
        .find_subcommand("list")
        .expect("'session list' subcommand should exist");
    assert!(
        list_cmd.get_arguments().any(|arg| arg.get_id() == "json"),
        "session list should expose a local --json flag"
    );
}

/// Ratchet: `session` subcommand has a description.
#[test]
fn test_session_subcommand_has_description() {
    let cmd = rustain::adapters::cli::commands::Cli::command();
    let session_cmd = cmd
        .find_subcommand("session")
        .expect("'session' subcommand should exist");
    let about = session_cmd
        .get_about()
        .map(|a| a.to_string())
        .unwrap_or_default();
    assert!(
        !about.is_empty(),
        "session subcommand should have a description"
    );
}
