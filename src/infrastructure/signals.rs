use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::adapters::tui::terminal::restore_terminal_raw;
use crate::domain::events::AppEvent;
use crate::infrastructure::paths;

static SHUTDOWN_TX: OnceLock<mpsc::UnboundedSender<AppEvent>> = OnceLock::new();
static EVENT_BUS_REF: OnceLock<
    std::sync::Arc<crate::infrastructure::runtime::event_bus::EventBus>,
> = OnceLock::new();
static SESSION_CANCEL: OnceLock<CancellationToken> = OnceLock::new();

/// Story 13.7 AC4 — set to `true` by [`install_panic_hook`] (the TUI hook) so
/// the startup hook no-ops on any later panic, yielding exactly ONE hook chain
/// with no double `panic.log` writes. Release/Acquire is a textbook SPSC flag
/// (party-mode OQ5 — SeqCst would be overkill).
static STARTUP_HOOK_SUPERSEDED: AtomicBool = AtomicBool::new(false);

/// Install the panic hook that restores the terminal, writes a crash log,
/// then calls the original panic hook.
pub fn install_panic_hook() {
    // Story 13.7 AC4 — mark the startup hook superseded FIRST, before
    // `take_hook()`. Once the TUI hook is installed, any later panic chains
    // TUI → startup-hook(no-op) → Rust default, so NO `panic.log` is written.
    // Set before `take_hook()` so the flag is already `true` when the TUI hook
    // later chains to the captured startup-hook closure.
    STARTUP_HOOK_SUPERSEDED.store(true, Ordering::Release);
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Step 1: Restore terminal FIRST
        restore_terminal_raw();

        // Step 2: Write crash report
        if let Ok(path) = paths::crash_log_path() {
            if let Ok(mut file) = std::fs::File::create(&path) {
                let timestamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let _ = writeln!(file, "Rustain Crash Report");
                let _ = writeln!(file, "Timestamp: {}", timestamp);
                let rust_version = option_env!("CARGO_PKG_RUST_VERSION").unwrap_or("unknown");
                let _ = writeln!(file, "Rust version: {}", rust_version);
                let _ = writeln!(file);
                let _ = writeln!(file, "Panic: {}", info);
                let _ = writeln!(file);
                let _ = writeln!(file, "Backtrace:");
                let _ = writeln!(file, "{}", std::backtrace::Backtrace::force_capture());
                eprintln!("Crash report written to: {}", path.display());
            }
        }

        // Step 3: Call original hook
        original_hook(info);
    }));
}

/// Write the latest-only startup panic report to `~/.rustain/panic.log`
/// (Story 13.7 AC2, party-mode F3). Extracted from the hook closure so it is
/// directly unit-testable WITHOUT a real panic or the process-global panic
/// hook — `info` is the panic message string, `backtrace` the captured trace.
///
/// Overwrites any prior `panic.log` (latest-only — party-mode OQ3). Returns the
/// path written on success. The hook closure calls this inside `catch_unwind`;
/// unit tests call it directly.
pub fn record_startup_panic(info: &str, backtrace: &str) -> Result<PathBuf> {
    let path = paths::panic_log_path()?;
    let mut file = std::fs::File::create(&path)?;
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let rust_version = option_env!("CARGO_PKG_RUST_VERSION").unwrap_or("unknown");
    writeln!(file, "Rustain Startup Crash Report")?;
    writeln!(file, "Timestamp: {timestamp}")?;
    writeln!(file, "Rust version: {rust_version}")?;
    writeln!(file)?;
    writeln!(file, "Panic: {info}")?;
    writeln!(file)?;
    writeln!(file, "Backtrace:")?;
    writeln!(file, "{backtrace}")?;
    Ok(path)
}

/// Install the startup panic hook (Story 13.7 AC1, AC3, AC4). Installed as the
/// FIRST operation in `startup::run()` so it captures panics during CLI parsing,
/// logging init, `-c` override parsing, and config loading — the ~180-line
/// window BEFORE the TUI hook ([`install_panic_hook`]) installs.
///
/// Writes the latest-only `panic.log` + a user-friendly stderr message (AC3),
/// then ALWAYS chains to the prior hook (the Rust default until the TUI hook
/// replaces it). Once the TUI hook installs it sets [`STARTUP_HOOK_SUPERSEDED`],
/// so a later panic chains TUI → startup-hook(no-op) → Rust default with NO
/// `panic.log` written (AC4). The I/O body is wrapped in `catch_unwind` so an
/// OOM-induced double-panic cannot abort before the prior hook runs
/// (party-mode OQ2). MUST NOT call `restore_terminal_raw` — no terminal setup
/// has occurred at this lifecycle stage (P0-D).
pub fn install_startup_panic_hook() {
    let prior = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // AC4 — once the TUI hook has installed, no-op: just chain to `prior`
        // so the TUI hook's terminal restore + crash-{ts}.log still run.
        if STARTUP_HOOK_SUPERSEDED.load(Ordering::Acquire) {
            prior(info);
            return;
        }
        // OQ2 — wrap the I/O body so an OOM-induced double-panic doesn't abort
        // before `prior` runs below. On any failure we still print a best-effort
        // stderr message (OQ4 — stderr-only fallback, no /tmp info leak).
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let backtrace = std::backtrace::Backtrace::force_capture();
            match record_startup_panic(&format!("{info}"), &backtrace.to_string()) {
                Ok(path) => {
                    eprintln!(
                        "rustain crashed during startup. Crash log written to: {}",
                        path.display()
                    );
                    eprintln!(
                        "Please report this issue at: https://github.com/lunarpulse/rustain/issues"
                    );
                }
                Err(_) => {
                    eprintln!("rustain crashed during startup. Panic: {info}");
                    eprintln!(
                        "Please report this issue at: https://github.com/lunarpulse/rustain/issues"
                    );
                }
            }
        }));
        // Always chain to the prior hook OUTSIDE catch_unwind (AC4).
        prior(info);
    }));
}

pub fn set_shutdown_sender(tx: mpsc::UnboundedSender<AppEvent>) {
    let _ = SHUTDOWN_TX.set(tx);
}

pub fn set_event_bus(bus: std::sync::Arc<crate::infrastructure::runtime::event_bus::EventBus>) {
    let _ = EVENT_BUS_REF.set(bus);
}

pub fn set_session_cancel(token: CancellationToken) {
    let _ = SESSION_CANCEL.set(token);
}

pub async fn install_signal_handlers() {
    let tx_shutdown = SHUTDOWN_TX.get().cloned();
    let bus = EVENT_BUS_REF.get().cloned();
    let cancel = SESSION_CANCEL.get().cloned();

    tokio::spawn(async move {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler");
        let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .expect("Failed to install SIGINT handler");
        let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
            .expect("Failed to install SIGHUP handler");

        loop {
            tokio::select! {
                // SIGHUP — reload config (Story 8.1 AC-8). Does NOT shut down.
                _ = sighup.recv() => {
                    if let Some(ref bus) = bus {
                        let _ = bus.emit_domain(AppEvent::ConfigReload);
                    }
                }
                // SIGTERM / SIGINT — graceful shutdown. Second signal force-exits.
                _ = sigterm.recv() => break,
                _ = sigint.recv() => break,
            }
        }

        if let Some(ref token) = cancel {
            token.cancel();
        }
        if let Some(ref tx) = tx_shutdown {
            let _ = tx.send(AppEvent::Shutdown);
        }

        tokio::select! {
            _ = sigterm.recv() => {},
            _ = sigint.recv() => {},
            _ = sighup.recv() => {},
        }

        restore_terminal_raw();
        std::process::exit(1);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    /// AC2 / Task 4.2 — `record_startup_panic` writes `panic.log` with the
    /// correct format. Anti-vacuous: asserting the sentinel header
    /// "Rustain Startup Crash Report" proves the RIGHT hook's write function ran,
    /// not just any file write.
    #[test]
    #[serial]
    fn record_startup_panic_writes_expected_format() {
        let tmp = tempfile::tempdir().unwrap();
        // SAFETY: #[serial] guarantees no concurrent env mutation; RUSTAIN_DATA_DIR
        // is the documented test seam for data_dir().
        unsafe {
            std::env::set_var("RUSTAIN_DATA_DIR", tmp.path());
        }
        let path = record_startup_panic("test panic sentinel", "fake-backtrace-XYZ").unwrap();
        unsafe {
            std::env::remove_var("RUSTAIN_DATA_DIR");
        }
        assert_eq!(path, tmp.path().join("panic.log"));
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.contains("Rustain Startup Crash Report"),
            "missing header sentinel"
        );
        assert!(content.contains("Timestamp:"), "missing timestamp");
        assert!(content.contains("Rust version:"), "missing rust version");
        assert!(
            content.contains("Panic: test panic sentinel"),
            "missing panic message"
        );
        assert!(content.contains("Backtrace:"), "missing backtrace header");
        assert!(
            content.contains("fake-backtrace-XYZ"),
            "missing backtrace content"
        );
    }

    /// AC4 / Task 4.3 — `STARTUP_HOOK_SUPERSEDED` starts `false`, and
    /// `install_panic_hook()` (the TUI hook) flips it to `true` as its FIRST
    /// act, so the startup hook no-ops on any later panic. Hook installation is
    /// process-global, so this is `#[serial]`; we restore the default hook +
    /// reset the flag afterwards so no state leaks into sibling tests.
    #[test]
    #[serial]
    fn install_panic_hook_sets_supersession_flag() {
        STARTUP_HOOK_SUPERSEDED.store(false, Ordering::SeqCst);
        assert!(
            !STARTUP_HOOK_SUPERSEDED.load(Ordering::SeqCst),
            "flag must start false"
        );
        install_panic_hook();
        assert!(
            STARTUP_HOOK_SUPERSEDED.load(Ordering::SeqCst),
            "install_panic_hook must set the supersession flag"
        );
        // Cleanup — restore the default panic hook + reset the flag.
        let _ = std::panic::take_hook();
        STARTUP_HOOK_SUPERSEDED.store(false, Ordering::SeqCst);
    }

    /// P0-A / Task 4.7 — directory/path creation positive control: a fresh data
    /// dir with NO pre-existing `panic.log` yields a created file at the expected
    /// path (proves `record_startup_panic` creates the file, not just writes to
    /// a pre-existing one).
    #[test]
    #[serial]
    fn record_startup_panic_creates_file_in_fresh_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let expected = tmp.path().join("panic.log");
        assert!(!expected.exists(), "precondition: no panic.log yet");
        // SAFETY: #[serial] guarantees no concurrent env mutation.
        unsafe {
            std::env::set_var("RUSTAIN_DATA_DIR", tmp.path());
        }
        let written = record_startup_panic("boom", "bt").unwrap();
        unsafe {
            std::env::remove_var("RUSTAIN_DATA_DIR");
        }
        assert_eq!(written, expected);
        assert!(expected.exists(), "panic.log must be created");
        assert!(expected.is_file());
    }
}
