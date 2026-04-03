use anyhow::Result;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

use crate::infrastructure::paths;

/// Initialize the tracing/logging system.
/// Logs write to `~/.rustain/rustain.log` with size-based rotation.
/// ZERO output goes to stdout/stderr (ratatui owns the terminal).
///
/// Returns a `WorkerGuard` that **must be held alive** for the duration of the
/// process. Dropping it flushes all buffered log writes to disk.
pub fn init(log_level: &str) -> Result<WorkerGuard> {
    let log_dir = paths::data_dir()?;

    // Daily rotation, retains log files by date
    let file_appender = tracing_appender::rolling::daily(&log_dir, "rustain.log");
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
