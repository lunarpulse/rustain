use std::io::Write;
use std::sync::OnceLock;

use tokio::sync::mpsc;

use crate::adapters::tui::terminal::restore_terminal_raw;
use crate::domain::events::AppEvent;
use crate::infrastructure::paths;

/// Global sender for shutdown signals (SIGTERM/SIGINT).
static SHUTDOWN_TX: OnceLock<mpsc::UnboundedSender<AppEvent>> = OnceLock::new();

/// Install the panic hook that restores the terminal, writes a crash log,
/// then calls the original panic hook.
pub fn install_panic_hook() {
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

/// Store the event sender for signal handlers.
pub fn set_shutdown_sender(tx: mpsc::UnboundedSender<AppEvent>) {
    let _ = SHUTDOWN_TX.set(tx);
}

/// Install SIGTERM/SIGINT handlers that send AppEvent::Shutdown.
/// On first signal: send Shutdown event for graceful teardown.
/// On second signal: restore terminal directly and exit (common double-Ctrl-C pattern).
pub async fn install_signal_handlers() {
    let tx = SHUTDOWN_TX.get().cloned();

    tokio::spawn(async move {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler");
        let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .expect("Failed to install SIGINT handler");

        // First signal: graceful shutdown
        tokio::select! {
            _ = sigterm.recv() => {}
            _ = sigint.recv() => {}
        }

        if let Some(ref tx) = tx {
            let _ = tx.send(AppEvent::Shutdown);
        }

        // Second signal: force restore terminal and exit
        tokio::select! {
            _ = sigterm.recv() => {}
            _ = sigint.recv() => {}
        }

        restore_terminal_raw();
        std::process::exit(1);
    });
}
