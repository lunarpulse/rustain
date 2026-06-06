//! Daemon PID file (Story 12.1a Task 5) — atomic write, liveness probe, and the
//! already-running guard (AC-12-1a-9).
//!
//! The PID file is workspace-scoped (`{workspace}/.rustain/daemon.pid`, see
//! `infrastructure::paths::daemon_pid_path`) and records the socket + workspace
//! paths so `status`/`stop`/attach(12.2) read them rather than re-deriving
//! (AC-12-1a-8). It doubles as the readiness marker: the detached child writes it
//! as the last step before entering the lifecycle loop, and the parent `start`
//! polls for it (NFR47 ≤ 3s).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// On-disk daemon record. TOML-encoded (the `toml` crate is already a dep).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonPidFile {
    /// OS process id of the running daemon.
    pub pid: u32,
    /// Resolved Unix socket path (`{data_dir}/daemons/<hash>.sock`).
    pub socket_path: PathBuf,
    /// Canonical workspace the daemon operates in.
    pub workspace: PathBuf,
    /// Unix seconds at daemon start — `status` derives uptime from this.
    pub started_at_unix: u64,
    /// Active profile name at start (shown by `status`).
    pub profile: String,
}

impl DaemonPidFile {
    /// Atomically write the PID file: write a sibling temp file then rename, so a
    /// reader never observes a half-written file (Task 5).
    pub fn write_atomic(&self, path: &Path) -> Result<()> {
        let body = toml::to_string(self).context("serializing daemon PID file")?;
        let tmp = path.with_extension("pid.tmp");
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        let mut f = opts
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        std::io::Write::write_all(&mut f, body.as_bytes())
            .with_context(|| format!("writing {}", tmp.display()))?;
        drop(f);
        std::fs::rename(&tmp, path)
            .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
        Ok(())
    }

    /// Read + parse the PID file. Errors on missing/unparseable file.
    pub fn read(path: &Path) -> Result<Self> {
        let body =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&body).with_context(|| format!("parsing {}", path.display()))
    }
}

/// Liveness probe (Task 5). Returns true only when the process is a *live*
/// daemon — a **zombie** (terminated but not yet reaped) counts as dead.
///
/// This zombie distinction matters: `start` re-execs a detached child and then
/// exits, so the daemon is orphaned. When it later exits (via `stop`) it may sit
/// as a zombie until its reaper collects it — and a bare `kill(pid, 0)` returns
/// success for a zombie, which would make `stop` spin until its SIGKILL deadline
/// and `status` report a dead daemon as running. On Linux we read
/// `/proc/<pid>/stat` and treat state `Z` as dead; elsewhere we fall back to the
/// `kill(pid, 0)` probe (macOS is P1; its reaping is handled by the OS).
#[cfg(unix)]
pub fn process_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => {
                // Format: `pid (comm) state ...`. `comm` may contain spaces and
                // parens, so the state char is the first token AFTER the last ')'.
                let state = stat
                    .rsplit_once(')')
                    .and_then(|(_, rest)| rest.split_whitespace().next());
                !matches!(state, Some("Z") | None)
            }
            // No /proc entry → the PID does not exist → dead.
            Err(_) => false,
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        // SAFETY: `kill` with signal 0 performs error checking without sending a
        // signal — the canonical liveness probe. No memory is touched.
        let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if rc == 0 {
            return true;
        }
        // EPERM: the process exists but we may not signal it → still "alive".
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

/// Result of the already-running guard (AC-12-1a-9).
#[derive(Debug)]
pub enum GuardOutcome {
    /// PID file exists AND the process is alive — refuse to start.
    Running(DaemonPidFile),
    /// PID file exists but the process is dead (or the file is unreadable) —
    /// reclaim it and proceed (do not require manual cleanup).
    Stale,
    /// No PID file — free to start.
    Free,
}

/// Inspect the PID file to decide whether a daemon is already running for this
/// workspace (AC-12-1a-9). An unreadable file is treated as `Stale` (a corrupt
/// leftover should never block a fresh start).
#[cfg(unix)]
pub fn check_running(pid_path: &Path) -> GuardOutcome {
    if !pid_path.exists() {
        return GuardOutcome::Free;
    }
    match DaemonPidFile::read(pid_path) {
        Ok(pf) if process_alive(pf.pid) => GuardOutcome::Running(pf),
        _ => GuardOutcome::Stale,
    }
}

/// Remove the PID file, ignoring "not found".
pub fn remove(pid_path: &Path) {
    let _ = std::fs::remove_file(pid_path);
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn sample(pid: u32) -> DaemonPidFile {
        DaemonPidFile {
            pid,
            socket_path: PathBuf::from("/tmp/x.sock"),
            workspace: PathBuf::from("/ws"),
            started_at_unix: 1_700_000_000,
            profile: "coding".into(),
        }
    }

    #[test]
    fn write_then_read_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("daemon.pid");
        let pf = sample(4242);
        pf.write_atomic(&path).unwrap();
        assert!(path.exists());
        assert_eq!(DaemonPidFile::read(&path).unwrap(), pf);
    }

    #[test]
    fn current_process_is_alive_and_garbage_pid_is_not() {
        assert!(process_alive(std::process::id()));
        // PID 0 is "every process in the group" for kill(2); use a very high,
        // almost-certainly-unused PID for the dead case.
        assert!(!process_alive(0x7FFF_FFF0));
    }

    #[test]
    fn guard_free_then_running_then_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("daemon.pid");
        assert!(matches!(check_running(&path), GuardOutcome::Free));

        // Live PID (ourselves) → Running.
        sample(std::process::id()).write_atomic(&path).unwrap();
        assert!(matches!(check_running(&path), GuardOutcome::Running(_)));

        // Dead PID → Stale.
        sample(0x7FFF_FFF0).write_atomic(&path).unwrap();
        assert!(matches!(check_running(&path), GuardOutcome::Stale));

        // Corrupt file → Stale (never blocks a fresh start).
        std::fs::write(&path, "not toml at all").unwrap();
        assert!(matches!(check_running(&path), GuardOutcome::Stale));
    }
}
