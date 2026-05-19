//! Conformance ratchets for `rustain profile` CLI subcommands.
//! Story 8.6a AC-12 (8.6b extends).

use clap::CommandFactory;

/// Ratchet 1: EXPECTED_PROFILE_SUBCOMMANDS = 9.
/// Guards against accidental removal or merge regression.
#[test]
fn test_profile_subcommand_count_is_eight() {
    const EXPECTED_PROFILE_SUBCOMMANDS: usize = 9;
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
        "install.rs",
        "source.rs",
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
        "list", "show", "create", "edit", "switch", "validate", "export", "import", "install",
    ];
    let cmd = rustain::adapters::cli::commands::Cli::command();
    let profile_cmd = cmd
        .find_subcommand("profile")
        .expect("'profile' subcommand should exist");

    for verb in &verbs {
        let sub = profile_cmd
            .find_subcommand(verb)
            .unwrap_or_else(|| panic!("profile subcommand '{}' should exist", verb));
        assert!(
            !sub.get_name().is_empty(),
            "profile {} should have a non-empty name",
            verb
        );
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

/// Ratchet 4: Network-import isolation. Only `install.rs` may import reqwest/hyper/tokio.
/// Guards against accidental network coupling in non-install profile subcommands.
#[test]
fn test_no_network_call_in_other_profile_subcommands() {
    let non_install_files = &[
        "src/adapters/cli/profile/list.rs",
        "src/adapters/cli/profile/show.rs",
        "src/adapters/cli/profile/create.rs",
        "src/adapters/cli/profile/edit.rs",
        "src/adapters/cli/profile/switch.rs",
        "src/adapters/cli/profile/validate.rs",
        "src/adapters/cli/profile/export.rs",
        "src/adapters/cli/profile/import.rs",
        "src/adapters/cli/profile/source.rs",
        "src/adapters/cli/profile/prompt.rs",
    ];
    let forbidden = &["reqwest", "hyper::", "tokio::net", "std::net::TcpStream"];
    for file in non_install_files {
        let content = std::fs::read_to_string(file)
            .unwrap_or_else(|_| panic!("File {} should exist", file));
        for &pattern in forbidden {
            assert!(
                !content.contains(pattern),
                "File {} must not import or use '{}'. Network calls belong in install.rs only.",
                file,
                pattern
            );
        }
    }
}
