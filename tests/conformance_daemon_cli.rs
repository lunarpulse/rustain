//! Conformance ratchets for the `rustain daemon` CLI subcommand (Story 12.1a
//! AC-12-1a-10). Mirrors `conformance_profile_cli.rs` / `conformance_catalog_cli.rs`.

use clap::CommandFactory;

/// Ratchet 1: EXPECTED_DAEMON_SUBCOMMANDS = 7.
///
/// The count includes the hidden `__run` re-exec target — `get_subcommands()`
/// enumerates hidden subcommands too. Visible: `start`, `stop`, `attach`, `status`,
/// `install`, `uninstall`. Hidden: `__run`. Guards against accidental
/// add/remove/merge regressions.
///
/// **4 → 6 (Story 12.1b, party-mode 2026-06-06):** added BOTH `install` AND
/// `uninstall`. A tool that writes persistent system state must own its teardown —
/// install-without-uninstall is a footgun (orphaned units that keep auto-restarting a
/// daemon the operator thought they killed). The install↔uninstall round-trip is also
/// the single highest-value deterministic test in 12.1b (pure FS + exit code).
///
/// **6 → 7 (Story 12.2b):** added the user-facing `attach` verb (connect an
/// interactive client to the running daemon over the attach wire protocol). The
/// count is verified to enumerate the hidden `__run` too, so 6 visible + 1 hidden
/// = 7.
#[test]
fn test_daemon_subcommand_count_pinned() {
    const EXPECTED_DAEMON_SUBCOMMANDS: usize = 7;
    let cmd = rustain::adapters::cli::commands::Cli::command();
    let daemon_cmd = cmd
        .find_subcommand("daemon")
        .expect("'daemon' subcommand should exist");
    let subcommand_count = daemon_cmd.get_subcommands().count();
    assert_eq!(
        subcommand_count, EXPECTED_DAEMON_SUBCOMMANDS,
        "Expected exactly {} daemon subcommands (start/stop/attach/status/install/uninstall + hidden __run), \
         found {}. If you intentionally added/removed a subcommand, update EXPECTED_DAEMON_SUBCOMMANDS.",
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

    for verb in ["start", "stop", "attach", "status", "install", "uninstall"] {
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
        // Story 12.1b additions.
        "crash.rs",
        "service.rs",
        // Story 12.2b additions (attach wire protocol + turn runtime + server).
        "protocol.rs",
        "runtime.rs",
        "server.rs",
        "attach_client.rs",
    ] {
        assert!(
            daemon_dir.join(file).exists(),
            "Expected file src/adapters/daemon/{} to exist",
            file
        );
    }
}
