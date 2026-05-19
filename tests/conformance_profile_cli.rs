//! Conformance ratchets for `rustain profile` CLI subcommands.
//! Story 8.6a AC-12.
//!
//! These tests ensure the module structure, subcommand count, and help output
//! remain correct and don't regress.

use clap::CommandFactory;

/// Ratchet 1: EXPECTED_PROFILE_SUBCOMMANDS = 8.
/// Guards against accidental removal or merge regression.
#[test]
fn test_profile_subcommand_count_is_eight() {
    const EXPECTED_PROFILE_SUBCOMMANDS: usize = 8;
    let cmd = rustain::adapters::cli::commands::Cli::command();
    let profile_cmd = cmd
        .find_subcommand("profile")
        .expect("'profile' subcommand should exist");
    let subcommand_count = profile_cmd.get_subcommands().count();
    assert_eq!(
        subcommand_count, EXPECTED_PROFILE_SUBCOMMANDS,
        "Expected exactly {} profile subcommands, found {}. \
         If you intentionally added/removed a subcommand, update EXPECTED_PROFILE_SUBCOMMANDS.",
        EXPECTED_PROFILE_SUBCOMMANDS, subcommand_count
    );
}

/// Ratchet 2: All profile subcommands live in `src/adapters/cli/profile/`.
/// Guards against codebase drift where profile logic leaks to other modules.
#[test]
fn test_profile_subcommands_live_in_module_directory() {
    let profile_dir = std::path::Path::new("src/adapters/cli/profile");
    assert!(
        profile_dir.exists() && profile_dir.is_dir(),
        "src/adapters/cli/profile/ directory must exist"
    );

    let expected_files = &[
        "mod.rs",
        "switch.rs",
        "list.rs",
        "show.rs",
        "create.rs",
        "edit.rs",
        "validate.rs",
        "export.rs",
        "import.rs",
        "prompt.rs",
    ];
    for file in expected_files {
        assert!(
            profile_dir.join(file).exists(),
            "Expected file src/adapters/cli/profile/{} to exist",
            file
        );
    }
}

/// Ratchet 3: `rustain profile {verb}` subcommands all have non-empty names
/// and descriptions. Guards against clap-attribute regressions.
#[test]
fn test_each_profile_subcommand_valid() {
    let verbs = [
        "list", "show", "create", "edit", "switch", "validate", "export", "import",
    ];
    let cmd = rustain::adapters::cli::commands::Cli::command();
    let profile_cmd = cmd
        .find_subcommand("profile")
        .expect("'profile' subcommand should exist");

    for verb in &verbs {
        let sub = profile_cmd
            .find_subcommand(verb)
            .unwrap_or_else(|| panic!("profile subcommand '{}' should exist", verb));
        // Verify the subcommand has a non-empty name
        assert!(
            !sub.get_name().is_empty(),
            "profile {} should have a non-empty name",
            verb
        );
        // Verify the subcommand has a non-empty about/description
        let has_description = sub
            .get_about()
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
            .is_some()
            || sub
                .get_long_about()
                .map(|s| s.to_string())
                .filter(|s| !s.is_empty())
                .is_some();
        assert!(
            has_description,
            "profile {} should have a help description",
            verb
        );
    }
}
