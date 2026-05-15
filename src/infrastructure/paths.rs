use std::path::PathBuf;

use anyhow::{Context, Result};

/// Resolve the `~/.rustain/` data directory, creating it if it doesn't exist.
pub fn data_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("RUSTAIN_DATA_DIR") {
        // CONFORMANCE_EXCEPTION: bootstrapping path resolution
        let path = PathBuf::from(dir);
        std::fs::create_dir_all(&path)?;
        return Ok(path);
    }
    let dir = dirs::home_dir()
        .context("Could not determine home directory")?
        .join(".rustain");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Resolve the `~/.config/rustain/` config directory, creating it if it doesn't exist.
/// Override with `RUSTAIN_CONFIG_DIR` env var for testing/CI.
pub fn config_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("RUSTAIN_CONFIG_DIR") {
        // CONFORMANCE_EXCEPTION: bootstrapping path resolution
        let path = PathBuf::from(dir);
        std::fs::create_dir_all(&path)?;
        return Ok(path);
    }
    let dir = dirs::config_dir()
        .context("Could not determine config directory")?
        .join("rustain");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Path to the main log file.
/// Override with `RUSTAIN_LOG_PATH` env var for testing/CI.
#[allow(dead_code)]
pub fn log_file_path() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("RUSTAIN_LOG_PATH") {
        // CONFORMANCE_EXCEPTION: bootstrapping path resolution
        Ok(PathBuf::from(path))
    } else {
        Ok(data_dir()?.join("rustain.log"))
    }
}

/// Resolve the workspace directory (current working directory).
pub fn workspace_dir() -> Result<PathBuf> {
    std::env::current_dir().context("Could not determine current working directory")
}

/// Resolve the `{workspace}/.claude/sessions/` directory for session persistence.
pub fn sessions_dir(workspace: &std::path::Path) -> PathBuf {
    workspace.join(".claude").join("sessions")
}

/// Resolve the `~/.rustain/usage/` directory for token-usage ledger files.
pub async fn usage_dir() -> Result<PathBuf> {
    let dir = data_dir()?.join("usage");
    tokio::fs::create_dir_all(&dir).await?;
    Ok(dir)
}

/// Path to a per-session usage ledger JSONL file.
pub async fn usage_ledger_path(session_id: &str) -> Result<PathBuf> {
    Ok(usage_dir().await?.join(format!("{}.jsonl", session_id)))
}

/// Path to the budget pause-state JSON file (Story 7.5 AC7).
/// Sync since `data_dir()` already creates the parent dir; no need to async-create.
pub fn budget_state_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("budget_state.json"))
}

/// Path to a crash log file with timestamp.
pub fn crash_log_path() -> Result<PathBuf> {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    Ok(data_dir()?.join(format!("crash-{}.log", timestamp)))
}
