//! Conformance ratchets for the `rustain daemon` CLI subcommand (Story 12.1a
//! AC-12-1a-10). Mirrors `conformance_profile_cli.rs` / `conformance_catalog_cli.rs`.

use clap::CommandFactory;

/// Ratchet 1: EXPECTED_DAEMON_SUBCOMMANDS = 4.
///
/// The count includes the hidden `__run` re-exec target — `get_subcommands()`
/// enumerates hidden subcommands too. Visible: `start`, `stop`, `status`.
/// Hidden: `__run`. Guards against accidental add/remove/merge regressions.
#[test]
fn test_daemon_subcommand_count_pinned() {
    const EXPECTED_DAEMON_SUBCOMMANDS: usize = 4;
    let cmd = rustain::adapters::cli::commands::Cli::command();
    let daemon_cmd = cmd
        .find_subcommand("daemon")
        .expect("'daemon' subcommand should exist");
    let subcommand_count = daemon_cmd.get_subcommands().count();
    assert_eq!(
        subcommand_count, EXPECTED_DAEMON_SUBCOMMANDS,
        "Expected exactly {} daemon subcommands (start/stop/status + hidden __run), found {}. \
         If you intentionally added/removed a subcommand, update EXPECTED_DAEMON_SUBCOMMANDS.",
        EXPECTED_DAEMON_SUBCOMMANDS, subcommand_count
    );
}

/// Ratchet 2: the three user-facing verbs exist with help descriptions, and the
/// `__run` re-exec target is present-but-hidden (internal, not a user verb).
#[test]
fn test_daemon_subcommands_shape() {
    let cmd = rustain::adapters::cli::commands::Cli::command();
    let daemon_cmd = cmd
        .find_subcommand("daemon")
        .expect("'daemon' subcommand should exist");

    for verb in ["start", "stop", "status"] {
        let sub = daemon_cmd
            .find_subcommand(verb)
            .unwrap_or_else(|| panic!("daemon subcommand '{}' should exist", verb));
        assert!(!sub.get_name().is_empty());
        assert!(
            !sub.is_hide_set(),
            "daemon {verb} is a user verb and must not be hidden"
        );
        let has_help = sub
            .get_about()
            .map(|s| !s.to_string().is_empty())
            .unwrap_or(false);
        assert!(has_help, "daemon {verb} should have a help description");
    }

    let run = daemon_cmd
        .find_subcommand("__run")
        .expect("hidden '__run' re-exec target should exist");
    assert!(
        run.is_hide_set(),
        "__run is the internal re-exec target and MUST stay hidden"
    );
}

/// Ratchet 3: the daemon adapter lives in `src/adapters/daemon/` (Hexagonal map:
/// it owns OS I/O — Unix socket, PID file, process spawn). Guards against drift.
#[test]
fn test_daemon_adapter_lives_in_module_directory() {
    let daemon_dir = std::path::Path::new("src/adapters/daemon");
    assert!(
        daemon_dir.exists() && daemon_dir.is_dir(),
        "src/adapters/daemon/ directory must exist"
    );
    for file in [
        "mod.rs",
        "pidfile.rs",
        "socket.rs",
        "lifecycle.rs",
        "status.rs",
    ] {
        assert!(
            daemon_dir.join(file).exists(),
            "Expected file src/adapters/daemon/{} to exist",
            file
        );
    }
}
