//! Conformance ratchets for `rustain session` CLI subcommands (Stories 13.5a / 13.5b).

use clap::CommandFactory;

/// Ratchet: EXPECTED_SESSION_SUBCOMMANDS = 2 (`list`, `delete`).
#[test]
fn test_session_subcommand_count_is_two() {
    const EXPECTED_SESSION_SUBCOMMANDS: usize = 2;
    let cmd = rustain::adapters::cli::commands::Cli::command();
    let session_cmd = cmd
        .find_subcommand("session")
        .expect("'session' subcommand should exist");
    let subcommand_count = session_cmd.get_subcommands().count();
    assert_eq!(
        subcommand_count, EXPECTED_SESSION_SUBCOMMANDS,
        "Expected {EXPECTED_SESSION_SUBCOMMANDS} session subcommand(s) (list, delete), found {subcommand_count}."
    );
}

/// Ratchet: `session list` subcommand exists and exposes local `--json` + `--all`.
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
    assert!(
        list_cmd.get_arguments().any(|arg| arg.get_id() == "all"),
        "session list should expose a local --all flag"
    );
}

/// Ratchet: `session delete` subcommand exists and exposes expected flags.
#[test]
fn test_session_delete_subcommand_exists() {
    let cmd = rustain::adapters::cli::commands::Cli::command();
    let session_cmd = cmd
        .find_subcommand("session")
        .expect("'session' subcommand should exist");
    let delete_cmd = session_cmd
        .find_subcommand("delete")
        .expect("'session delete' subcommand should exist");
    assert!(
        delete_cmd
            .get_arguments()
            .any(|arg| arg.get_long() == Some("all")),
        "session delete should expose --all"
    );
    assert!(
        delete_cmd
            .get_arguments()
            .any(|arg| arg.get_long() == Some("all-workspaces")),
        "session delete should expose --all-workspaces"
    );
    assert!(
        delete_cmd
            .get_arguments()
            .any(|arg| arg.get_long() == Some("workspace")),
        "session delete should expose --workspace"
    );
    assert!(
        delete_cmd
            .get_arguments()
            .any(|arg| arg.get_long() == Some("force")),
        "session delete should expose --force"
    );
    assert!(
        delete_cmd
            .get_arguments()
            .any(|arg| arg.get_long() == Some("dry-run")),
        "session delete should expose --dry-run"
    );
    assert!(
        delete_cmd
            .get_arguments()
            .any(|arg| arg.get_long() == Some("json")),
        "session delete should expose --json"
    );
    let about = delete_cmd
        .get_about()
        .map(|a| a.to_string())
        .unwrap_or_default();
    assert!(
        !about.is_empty(),
        "session delete should have a description"
    );
    assert!(
        !delete_cmd.is_hide_set(),
        "session delete should not be hidden"
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
