use std::path::PathBuf;

use anyhow::{Context, Result};

/// Resolve the `~/.rustain/` data directory, creating it if it doesn't exist.
pub fn data_dir() -> Result<PathBuf> {
    let dir = dirs::home_dir()
        .context("Could not determine home directory")?
        .join(".rustain");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Resolve the `~/.config/rustain/` config directory, creating it if it doesn't exist.
#[allow(dead_code)]
pub fn config_dir() -> Result<PathBuf> {
    let dir = dirs::config_dir()
        .context("Could not determine config directory")?
        .join("rustain");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Path to the main log file.
#[allow(dead_code)]
pub fn log_file_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("rustain.log"))
}

/// Path to a crash log file with timestamp.
pub fn crash_log_path() -> Result<PathBuf> {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    Ok(data_dir()?.join(format!("crash-{}.log", timestamp)))
}
