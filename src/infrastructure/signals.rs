use std::io::Write;
use std::sync::OnceLock;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::adapters::tui::terminal::restore_terminal_raw;
use crate::domain::events::AppEvent;
use crate::infrastructure::paths;

static SHUTDOWN_TX: OnceLock<mpsc::UnboundedSender<AppEvent>> = OnceLock::new();
static EVENT_BUS_REF: OnceLock<std::sync::Arc<crate::infrastructure::runtime::event_bus::EventBus>> = OnceLock::new();
static SESSION_CANCEL: OnceLock<CancellationToken> = OnceLock::new();

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
        let mut sighup =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
                .expect("Failed to install SIGHUP handler");

        loop {
            tokio::select! {
                // SIGHUP — reload config (Story 8.1 AC-8). Does NOT shut down.
                _ = sighup.recv() => {
                    if let Some(ref bus) = bus {
                        bus.emit_domain(AppEvent::ConfigReload);
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
