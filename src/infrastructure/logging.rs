use std::path::PathBuf;

use anyhow::Result;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

use crate::infrastructure::paths;

/// Initialize the tracing/logging system.
/// Logs write to `~/.rustain/rustain.log` with size-based rotation.
/// Override the log file path with `RUSTAIN_LOG_PATH` env var (used by E2E tests).
/// ZERO output goes to stdout/stderr (ratatui owns the terminal).
///
/// Returns a `WorkerGuard` that **must be held alive** for the duration of the
/// process. Dropping it flushes all buffered log writes to disk.
pub fn init(log_level: &str) -> Result<WorkerGuard> {
    let (log_dir, log_prefix) = if let Ok(path) = std::env::var("RUSTAIN_LOG_PATH") {
        let path = PathBuf::from(path);
        let dir = path.parent().unwrap_or(&path).to_path_buf();
        // tracing_appender::rolling::daily uses {prefix}.{date} naming.
        // If path is .../rustain.log we want prefix "rustain.log" so the
        // resulting file is rustain.log.2026-04-26, not rustain.2026-04-26.
        let prefix = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "rustain.log".to_string());
        (dir, prefix)
    } else {
        (paths::data_dir()?, "rustain.log".to_string())
    };

    // Daily rotation, retains log files by date
    let file_appender = tracing_appender::rolling::daily(&log_dir, &log_prefix);
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("rustain={}", log_level)));

    tracing_subscriber::registry()
        .with(filter)
        .with(
            fmt::layer()
                .with_writer(non_blocking)
                .with_ansi(false)
                .with_target(true)
                .with_thread_ids(false),
        )
        .init();

    Ok(guard)
}
