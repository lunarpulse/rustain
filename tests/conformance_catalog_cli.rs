//! Conformance ratchets for `rustain catalog` CLI subcommands.
//! Story 9.8.

#[cfg(feature = "meta-search")]
mod catalog_conformance {
    use clap::CommandFactory;

    /// Ratchet 1: EXPECTED_CATALOG_SUBCOMMANDS = 4.
    #[test]
    fn test_catalog_subcommand_count_is_four() {
        const EXPECTED_CATALOG_SUBCOMMANDS: usize = 4;
        let cmd = rustain::adapters::cli::commands::Cli::command();
        let catalog_cmd = cmd
            .find_subcommand("catalog")
            .expect("'catalog' subcommand should exist when meta-search feature is enabled");
        let subcommand_count = catalog_cmd.get_subcommands().count();
        assert_eq!(
            subcommand_count, EXPECTED_CATALOG_SUBCOMMANDS,
            "Expected exactly {} catalog subcommands, found {}. \
             If you intentionally added/removed a subcommand, update EXPECTED_CATALOG_SUBCOMMANDS.",
            EXPECTED_CATALOG_SUBCOMMANDS, subcommand_count
        );
    }

    /// Ratchet 2: All catalog subcommands live in `src/adapters/cli/catalog/`.
    #[test]
    fn test_catalog_subcommands_live_in_module_directory() {
        let catalog_dir = std::path::Path::new("src/adapters/cli/catalog");
        assert!(
            catalog_dir.exists() && catalog_dir.is_dir(),
            "src/adapters/cli/catalog/ directory must exist"
        );

        let expected_files = &["mod.rs", "list.rs", "explain.rs", "stats.rs", "search.rs"];
        for file in expected_files {
            assert!(
                catalog_dir.join(file).exists(),
                "Expected file src/adapters/cli/catalog/{} to exist",
                file
            );
        }
    }

    /// Ratchet 3: All catalog subcommands have non-empty descriptions.
    #[test]
    fn test_each_catalog_subcommand_valid() {
        let verbs = ["list", "explain", "stats", "search"];
        let cmd = rustain::adapters::cli::commands::Cli::command();
        let catalog_cmd = cmd
            .find_subcommand("catalog")
            .expect("'catalog' subcommand should exist");

        for verb in &verbs {
            let sub = catalog_cmd
                .find_subcommand(verb)
                .unwrap_or_else(|| panic!("catalog subcommand '{}' should exist", verb));
            assert!(
                !sub.get_name().is_empty(),
                "catalog {} should have a non-empty name",
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
                "catalog {} should have a help description",
                verb
            );
        }
    }

    /// Ratchet 4: No `/catalog` slash command registration.
    #[test]
    fn test_no_slash_command_registration() {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
        let slash_files = &[
            format!(
                "{}/src/adapters/tui/widgets/slash_commands.rs",
                manifest_dir
            ),
            format!("{}/src/adapters/tui/widgets/autocomplete.rs", manifest_dir),
        ];
        for file in slash_files {
            if !std::path::Path::new(file).exists() {
                continue;
            }
            let content = std::fs::read_to_string(file)
                .unwrap_or_else(|_| panic!("File {} should be readable", file));
            assert!(
                !content.contains("/catalog"),
                "File {} must NOT contain '/catalog' slash command registration",
                file
            );
            assert!(
                !content.contains("\"catalog\""),
                "File {} must NOT contain '\"catalog\"' slash command registration",
                file
            );
        }
    }

    /// Ratchet 5: `catalog` about text includes developer-tool marker.
    #[test]
    fn test_catalog_about_includes_developer_tool_marker() {
        let cmd = rustain::adapters::cli::commands::Cli::command();
        let catalog_cmd = cmd
            .find_subcommand("catalog")
            .expect("'catalog' subcommand should exist");
        let about = catalog_cmd
            .get_about()
            .map(|s| s.to_string())
            .unwrap_or_default()
            .to_lowercase();
        assert!(
            about.contains("developer tool") || about.contains("dev-tool"),
            "catalog subcommand 'about' must contain 'developer tool' or 'dev-tool' substring. Got: {}",
            about
        );
    }
}
