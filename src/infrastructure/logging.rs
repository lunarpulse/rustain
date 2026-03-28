use anyhow::Result;
use rolling_file::{BasicRollingFileAppender, RollingConditionBasic};
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

use crate::infrastructure::paths;

/// Initialize the tracing/logging system.
/// Logs write to `~/.rustain/rustain.log` with size-based rotation.
/// ZERO output goes to stdout/stderr (ratatui owns the terminal).
pub fn init(log_level: &str) -> Result<()> {
    let log_dir = paths::data_dir()?;

    // Size-based rotation: 10MB max, retain last 3 files
    let rolling_writer = BasicRollingFileAppender::new(
        log_dir.join("rustain.log"),
        RollingConditionBasic::new().max_size(10 * 1024 * 1024), // 10MB
        3,                                                       // retain 3 files
    )?;

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("rustain={}", log_level)));

    tracing_subscriber::registry()
        .with(filter)
        .with(
            fmt::layer()
                .with_writer(std::sync::Mutex::new(rolling_writer))
                .with_ansi(false)
                .with_target(true)
                .with_thread_ids(false),
        )
        .init();

    Ok(())
}
