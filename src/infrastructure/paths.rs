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

/// Path to the main config file (`~/.config/rustain/config.toml`).
pub fn config_file_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

/// Path to the model catalog cache JSON file (Story 7.6 AC4).
/// Sync since `data_dir()` already creates the parent dir; no need to async-create.
pub fn models_cache_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("models_cache.json"))
}

// ── Daemon path resolution (Story 12.1a AC-12-1a-8) ──────────────────────────
//
// "Hybrid" scoping (author decision, 2026-06-06): the PID file is
// **workspace-scoped** — `{workspace}/.rustain/daemon.pid` — consistent with all
// other workspace `.rustain/` runtime artifacts and human-discoverable. The
// socket lives under the short data-dir root —
// `{data_dir}/daemons/<workspace-hash>.sock` — so a deeply-nested workspace path
// can never push the AF_UNIX `sun_path` over its ~108-byte limit (a socket under
// `{workspace}/.rustain/daemon.sock` would). The socket path is recorded inside
// the PID file so `status`/`stop`/attach(12.2) read it rather than re-deriving.
//
// These helpers are the SINGLE SOURCE OF TRUTH for the paths (AC-12-1a-8) — no
// inline `join("daemon.pid")` is allowed elsewhere. Both honor the same
// `RUSTAIN_DATA_DIR` test override as `data_dir()`.

/// Resolve the workspace `{workspace}/.rustain/` runtime directory, creating it.
fn rustain_workspace_dir(workspace: &std::path::Path) -> Result<PathBuf> {
    let dir = workspace.join(".rustain");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

/// Stable per-workspace hash used in the socket filename. Canonicalizes the
/// workspace path first (so `.`/`..`/symlinks resolve to the same daemon) and
/// falls back to the raw path bytes when the path can't be canonicalized.
pub fn workspace_hash(workspace: &std::path::Path) -> String {
    let canonical = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let digest = blake3::hash(canonical.to_string_lossy().as_bytes());
    // 16 hex chars (8 bytes) — collision-free in practice, keeps `sun_path` short.
    digest.to_hex()[..16].to_string()
}

/// Path to the workspace-scoped daemon PID file (Story 12.1a AC-12-1a-8).
/// `{workspace}/.rustain/daemon.pid`.
pub fn daemon_pid_path(workspace: &std::path::Path) -> Result<PathBuf> {
    Ok(rustain_workspace_dir(workspace)?.join("daemon.pid"))
}

/// Path to the per-workspace daemon Unix socket (Story 12.1a AC-12-1a-8).
/// `{data_dir}/daemons/<workspace-hash>.sock` — short root avoids the AF_UNIX
/// path-length limit; the hash keeps it per-workspace and collision-free.
pub fn daemon_socket_path(workspace: &std::path::Path) -> Result<PathBuf> {
    let dir = data_dir()?.join("daemons");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir.join(format!("{}.sock", workspace_hash(workspace))))
}

/// Path to the daemon's stdio/log file (re-exec child redirects stdout+stderr
/// here). `{workspace}/.rustain/daemon.log` — discoverable alongside the PID.
pub fn daemon_log_path(workspace: &std::path::Path) -> Result<PathBuf> {
    Ok(rustain_workspace_dir(workspace)?.join("daemon.log"))
}

/// File name of the generated service file for `workspace` (Story 12.1b
/// AC-12-1b-3). Embeds the workspace hash so multiple workspaces install
/// non-colliding services (consistent with the per-workspace socket name).
/// `rustain-<hash>.service` (systemd) / `com.rustain.<hash>.plist` (launchd).
#[cfg(unix)]
pub fn daemon_service_file_name(workspace: &std::path::Path) -> String {
    let hash = workspace_hash(workspace);
    #[cfg(target_os = "macos")]
    {
        format!("com.rustain.{hash}.plist")
    }
    #[cfg(not(target_os = "macos"))]
    {
        format!("rustain-{hash}.service")
    }
}

/// launchd `Label` for `workspace` (`com.rustain.<hash>`) — the plist `Label` and
/// the basename `launchctl` addresses the agent by.
#[cfg(unix)]
pub fn daemon_service_label(workspace: &std::path::Path) -> String {
    format!("com.rustain.{}", workspace_hash(workspace))
}

/// Directory the service file installs into for the given scope (Story 12.1b
/// AC-12-1b-3). `RUSTAIN_SERVICE_DIR` overrides it for tests/CI (the isolated
/// install-root seam). Linux user units → `~/.config/systemd/user`; `--system` →
/// `/etc/systemd/system`. macOS → `~/Library/LaunchAgents` (system LaunchDaemons are
/// out of P1 scope).
#[cfg(unix)]
fn daemon_service_dir(system: bool) -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("RUSTAIN_SERVICE_DIR") {
        // CONFORMANCE_EXCEPTION: bootstrapping path resolution (test/CI override)
        return Ok(PathBuf::from(dir));
    }
    #[cfg(target_os = "macos")]
    {
        let _ = system;
        let home = dirs::home_dir().context("Could not determine home directory")?;
        Ok(home.join("Library").join("LaunchAgents"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        if system {
            Ok(PathBuf::from("/etc/systemd/system"))
        } else {
            let cfg = dirs::config_dir().context("Could not determine config directory")?;
            Ok(cfg.join("systemd").join("user"))
        }
    }
}

/// Resolved absolute path of the service file for `workspace` + scope (Story 12.1b
/// AC-12-1b-3). **Single source of truth** — `install` and `uninstall` MUST resolve
/// the identical path through this one helper. Does not touch the filesystem; the
/// installer creates the parent dir (user scope) as needed.
#[cfg(unix)]
pub fn daemon_service_path(workspace: &std::path::Path, system: bool) -> Result<PathBuf> {
    Ok(daemon_service_dir(system)?.join(daemon_service_file_name(workspace)))
}

/// Path to the daemon's latest-only crash record (Story 12.1b AC-12-1b-5).
/// `{workspace}/.rustain/daemon-crash.json` — single source of truth, mirrors the
/// AC-12-1a-8 path-helper rule (no inline `join("daemon-crash.json")` elsewhere).
/// Overwritten atomically on each crash; `daemon status` reads exactly this path.
pub fn daemon_crash_path(workspace: &std::path::Path) -> Result<PathBuf> {
    Ok(rustain_workspace_dir(workspace)?.join("daemon-crash.json"))
}

/// Path to a timestamped daemon backtrace file (Story 12.1b AC-12-1b-5). These are
/// the immutable append-as-new-files forensic trail (capped/rotated by the writer so
/// a crash loop can't fill the disk). Workspace-scoped + daemon-context, distinct
/// from the TUI's `~/.rustain/crash-<ts>.log` ([`crash_log_path`]).
pub fn daemon_crash_log_path(workspace: &std::path::Path, ts: u64) -> Result<PathBuf> {
    Ok(rustain_workspace_dir(workspace)?.join(format!("crash-{ts}.log")))
}

/// Path to a crash log file with timestamp.
pub fn crash_log_path() -> Result<PathBuf> {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    Ok(data_dir()?.join(format!("crash-{}.log", timestamp)))
}

#[cfg(test)]
mod daemon_path_tests {
    use super::*;

    #[test]
    fn pid_path_is_workspace_scoped() {
        let tmp = tempfile::tempdir().unwrap();
        let p = daemon_pid_path(tmp.path()).unwrap();
        assert_eq!(p, tmp.path().join(".rustain").join("daemon.pid"));
        assert!(tmp.path().join(".rustain").is_dir());
    }

    #[test]
    fn workspace_hash_is_stable_and_distinct() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        assert_eq!(workspace_hash(a.path()), workspace_hash(a.path()));
        assert_ne!(workspace_hash(a.path()), workspace_hash(b.path()));
        assert_eq!(workspace_hash(a.path()).len(), 16);
    }

    #[test]
    fn socket_path_lives_under_data_dir_and_is_short() {
        let data = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        // SAFETY: single-threaded test; RUSTAIN_DATA_DIR override is the documented
        // test seam consistent with data_dir().
        unsafe {
            std::env::set_var("RUSTAIN_DATA_DIR", data.path());
        }
        let sock = daemon_socket_path(ws.path()).unwrap();
        unsafe {
            std::env::remove_var("RUSTAIN_DATA_DIR");
        }
        assert!(sock.starts_with(data.path().join("daemons")));
        assert!(sock.extension().unwrap() == "sock");
        // AF_UNIX sun_path limit guard (Linux 108) — the whole point of the hash.
        assert!(
            sock.to_string_lossy().len() < 108,
            "socket path must stay under the AF_UNIX limit: {}",
            sock.display()
        );
    }
}
