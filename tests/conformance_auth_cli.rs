//! Conformance ratchets for `rustain auth` CLI subcommands.
//! Story 13.4a.

use clap::CommandFactory;

/// Ratchet: EXPECTED_AUTH_SUBCOMMANDS = 1 (login only in 13.4a).
/// 13.4b bumps to 2, 13.4c to 3.
#[test]
fn test_auth_subcommand_count_is_one() {
    const EXPECTED_AUTH_SUBCOMMANDS: usize = 1;
    let cmd = rustain::adapters::cli::commands::Cli::command();
    let auth_cmd = cmd
        .find_subcommand("auth")
        .expect("'auth' subcommand should exist");
    let subcommand_count = auth_cmd.get_subcommands().count();
    assert_eq!(
        subcommand_count, EXPECTED_AUTH_SUBCOMMANDS,
        "Expected {EXPECTED_AUTH_SUBCOMMANDS} auth subcommand(s), found {subcommand_count}. \
         If you added a new subcommand (e.g. status/list), bump EXPECTED_AUTH_SUBCOMMANDS."
    );
}

/// Ratchet: auth subcommand has a description.
#[test]
fn test_auth_subcommand_has_description() {
    let cmd = rustain::adapters::cli::commands::Cli::command();
    let auth_cmd = cmd
        .find_subcommand("auth")
        .expect("'auth' subcommand should exist");
    let about = auth_cmd
        .get_about()
        .map(|a| a.to_string())
        .unwrap_or_default();
    assert!(
        !about.is_empty(),
        "auth subcommand should have a description"
    );
}

/// Ratchet: auth login subcommand exists and has provider argument.
#[test]
fn test_auth_login_has_provider_argument() {
    let cmd = rustain::adapters::cli::commands::Cli::command();
    let auth_cmd = cmd
        .find_subcommand("auth")
        .expect("'auth' subcommand should exist");
    let login_cmd = auth_cmd
        .find_subcommand("login")
        .expect("'login' subcommand should exist");
    let args: Vec<_> = login_cmd.get_positionals().collect();
    assert!(
        !args.is_empty(),
        "login should have a positional 'provider' argument"
    );
}
