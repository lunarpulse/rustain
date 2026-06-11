//! `rustain config reload` CLI subcommand — Story 8.1 AC-9.
//!
//! Cross-process IPC for config reload is a deferred follow-up (DF-NNN).
//! This stub prints the user-facing message per AC-9 and exits 0.

use anyhow::Result;

/// Run `rustain config reload` from outside a running rustain process.
/// Prints cross-process-not-supported message and exits 0.
pub async fn run_config_reload() -> Result<()> {
    println!(
        "To reload a running TUI, type /config reload in it. To reload daemon \
         cron jobs on Unix, send SIGHUP to the daemon PID, for example: \
         kill -HUP $(cat <workspace>/.rustain/daemon.pid)."
    );
    Ok(())
}
