//! Daemon adapter (Story 12.1a) — `rustain daemon {start,stop,status}` + the
//! hidden `__run` re-exec body.
//!
//! **Scope (read Dev Notes §"Scope discipline"):** 12.1a is the daemon PROCESS +
//! LIFECYCLE skeleton — start/stop/status, PID file, socket bind, the
//! `daily_reset`/`idle_timeout`/shutdown boundaries, and graceful shutdown. It is
//! NOT a message-processing runtime: `event_loop::run` is TUI-coupled and cannot
//! run headless, and there is no live channel yet (`terminal` → `NoOpChannel`).
//! Message delivery lands in Stories 12.2/12.3/12.4.
//!
//! This adapter owns OS I/O (Unix socket, PID file, process spawn) → it sits in
//! the **adapters** layer per the Hexagonal map. It is Unix-only (Linux P0,
//! macOS P1); on Windows every entrypoint returns an actionable not-supported
//! error (named-pipe support deferred to P2 / NFR33).

use std::path::PathBuf;

use anyhow::Result;

use crate::adapters::cli::commands::DaemonAction;
use crate::domain::models::AppConfig;

#[cfg(unix)]
mod lifecycle;
#[cfg(unix)]
mod pidfile;
#[cfg(unix)]
mod socket;
#[cfg(unix)]
pub mod status;

#[cfg(unix)]
pub use lifecycle::{duration_until_next, emit_session_boundary};

/// Dispatch a `daemon` subcommand. `memory_adapter` is the active profile's
/// resolved memory port name (used only by the `__run`/foreground body to compose
/// the headless memory sink); `config` carries `[daemon]` settings + profile.
pub async fn run_daemon(
    action: DaemonAction,
    workspace: PathBuf,
    config: AppConfig,
    memory_adapter: String,
) -> Result<()> {
    #[cfg(unix)]
    {
        match action {
            DaemonAction::Start { foreground } => {
                if foreground {
                    run_daemon_foreground(workspace, config, memory_adapter).await
                } else {
                    run_daemon_start(workspace, config).await
                }
            }
            DaemonAction::Run => run_daemon_foreground(workspace, config, memory_adapter).await,
            DaemonAction::Stop => run_daemon_stop(workspace).await,
            DaemonAction::Status { json } => run_daemon_status(workspace, config, json).await,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (action, workspace, config, memory_adapter);
        windows_not_supported()
    }
}

#[cfg(not(unix))]
fn windows_not_supported() -> Result<()> {
    eprintln!(
        "Error: `rustain daemon` requires Unix sockets (Linux or macOS). \
         Windows daemon support (named pipes) is deferred to a future release (NFR33). \
         Run rustain in interactive (TUI) mode instead."
    );
    anyhow::bail!("daemon mode is not supported on this platform")
}

// ── Unix implementation ──────────────────────────────────────────────────────

#[cfg(unix)]
use lifecycle::DaemonRuntime;
#[cfg(unix)]
use pidfile::{DaemonPidFile, GuardOutcome};

/// `daemon start` — re-exec a detached child (NOT `fork()`; forking a live
/// multi-threaded tokio runtime is unsafe — only async-signal-safe calls are
/// legal between fork and exec). The parent waits for the readiness handshake
/// (the child writing its PID file) within the NFR47 3s budget, then returns.
#[cfg(unix)]
async fn run_daemon_start(workspace: PathBuf, config: AppConfig) -> Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    // Validate `[daemon]` config here so a bad value fails the foreground `start`
    // command rather than silently dying in the detached child (Task 7).
    config.daemon.validate().map_err(|e| anyhow::anyhow!(e))?;

    let pid_path = crate::infrastructure::paths::daemon_pid_path(&workspace)?;

    // Already-running guard (AC-12-1a-9) — exact message, stale reclaim.
    match pidfile::check_running(&pid_path) {
        GuardOutcome::Running(pf) => {
            eprintln!(
                "Daemon already running (PID: {}). Use 'rustain daemon stop' first.",
                pf.pid
            );
            anyhow::bail!("daemon already running");
        }
        GuardOutcome::Stale => {
            tracing::info!("reclaiming stale daemon PID file at {}", pid_path.display());
            pidfile::remove(&pid_path);
        }
        GuardOutcome::Free => {}
    }

    let exe = std::env::current_exe()?;
    let log_path = crate::infrastructure::paths::daemon_log_path(&workspace)?;
    let log = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&log_path)?;
    let log_err = log.try_clone()?;

    let mut cmd = Command::new(exe);
    // Forward the resolved profile so the child composes the SAME memory adapter.
    cmd.arg("--profile")
        .arg(&config.active_profile)
        .arg("daemon")
        .arg("__run");
    cmd.current_dir(&workspace);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::from(log));
    cmd.stderr(Stdio::from(log_err));
    // Detach from the controlling terminal: new session via setsid in the child,
    // after fork() but before exec(). setsid is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            // SAFETY: setsid takes no args, touches no process memory, and is
            // async-signal-safe — legal in the post-fork/pre-exec window.
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = cmd.spawn()?;
    let child_pid = child.id();

    // Readiness handshake: poll for the child's PID file (written last, just
    // before the loop) within 3s (NFR47). Bail early if the child dies.
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Ok(pf) = DaemonPidFile::read(&pid_path) {
            if pf.pid == child_pid {
                println!("Daemon started (PID: {child_pid}).");
                return Ok(());
            }
        }
        if let Ok(Some(exit)) = child.try_wait() {
            anyhow::bail!(
                "daemon child exited before becoming ready ({exit}); see {}",
                log_path.display()
            );
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            anyhow::bail!(
                "daemon did not become ready within 3s; see {}",
                log_path.display()
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The daemon body (the `__run` re-exec target and `start --foreground`): compose
/// the headless memory sink, bind the socket, write the PID file (the readiness
/// marker), and run the lifecycle loop until a shutdown signal.
#[cfg(unix)]
async fn run_daemon_foreground(
    workspace: PathBuf,
    config: AppConfig,
    memory_adapter: String,
) -> Result<()> {
    config.daemon.validate().map_err(|e| anyhow::anyhow!(e))?;

    let pid_path = crate::infrastructure::paths::daemon_pid_path(&workspace)?;
    let socket_path = crate::infrastructure::paths::daemon_socket_path(&workspace)?;

    // Defense-in-depth guard inside the child too (the parent already checked,
    // but a foreground invocation skips the parent path entirely).
    if let GuardOutcome::Running(pf) = pidfile::check_running(&pid_path) {
        eprintln!(
            "Daemon already running (PID: {}). Use 'rustain daemon stop' first.",
            pf.pid
        );
        anyhow::bail!("daemon already running");
    }

    // Compose ONLY the memory port (see composition::build_daemon_memory docs):
    // 12.1a has no message runtime, so the only port with a shutdown obligation
    // is memory (its flush must route through 12.0's hardened prepare_detach sink).
    let memory =
        crate::infrastructure::composition::build_daemon_memory(&workspace, &memory_adapter)
            .map_err(|e| anyhow::anyhow!("composing daemon memory adapter: {e}"))?;

    let rt = DaemonRuntime {
        config: config.clone(),
        memory,
        pid_path: pid_path.clone(),
        socket_path: socket_path.clone(),
    };

    // Write the PID file LAST (after we know paths resolve) — it is the readiness
    // marker the parent `start` polls for. Records socket + workspace + start time
    // (AC-12-1a-8) so status/stop/attach read rather than re-derive.
    let pf = DaemonPidFile {
        pid: std::process::id(),
        socket_path,
        workspace: workspace.clone(),
        started_at_unix: now_unix(),
        profile: config.active_profile.clone(),
    };
    pf.write_atomic(&pid_path)?;

    let result = lifecycle::run_lifecycle(rt).await;

    // Belt-and-suspenders: ensure the PID file is gone even if the loop errored
    // before its own cleanup ran.
    pidfile::remove(&pid_path);
    result
}

/// `daemon stop` — SIGTERM, wait up to 5s for exit + PID-file removal (NFR48),
/// then escalate to SIGKILL and report the timeout (AC-12-1a-3).
#[cfg(unix)]
async fn run_daemon_stop(workspace: PathBuf) -> Result<()> {
    use std::time::{Duration, Instant};

    let pid_path = crate::infrastructure::paths::daemon_pid_path(&workspace)?;
    let pf = match pidfile::check_running(&pid_path) {
        GuardOutcome::Running(pf) => pf,
        GuardOutcome::Stale => {
            pidfile::remove(&pid_path);
            println!("Daemon not running (cleaned up stale PID file).");
            return Ok(());
        }
        GuardOutcome::Free => {
            println!("Daemon not running.");
            return Ok(());
        }
    };

    // SAFETY: kill() with a real signal; no memory touched.
    unsafe {
        let rc = libc::kill(pf.pid as libc::pid_t, libc::SIGTERM);
        if rc == -1 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EPERM) {
                anyhow::bail!(
                    "permission denied sending SIGTERM to PID {} — \
                     the daemon may be owned by a different user",
                    pf.pid
                );
            }
        }
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if !pidfile::process_alive(pf.pid) {
            pidfile::remove(&pid_path);
            socket::cleanup(&pf.socket_path);
            println!("Daemon stopped (PID: {}).", pf.pid);
            return Ok(());
        }
        if Instant::now() >= deadline {
            // SAFETY: see above.
            unsafe {
                libc::kill(pf.pid as libc::pid_t, libc::SIGKILL);
            }
            pidfile::remove(&pid_path);
            socket::cleanup(&pf.socket_path);
            eprintln!(
                "Daemon did not exit within 5s; escalated to SIGKILL (PID: {}).",
                pf.pid
            );
            anyhow::bail!("daemon stop timed out; sent SIGKILL");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// `daemon status` — structured snapshot (AC-12-1a-2). Not-running prints a clear
/// line and exits non-zero so scripts can branch.
#[cfg(unix)]
async fn run_daemon_status(workspace: PathBuf, config: AppConfig, json: bool) -> Result<()> {
    let pid_path = crate::infrastructure::paths::daemon_pid_path(&workspace)?;
    let pf = match pidfile::check_running(&pid_path) {
        GuardOutcome::Running(pf) => pf,
        GuardOutcome::Stale | GuardOutcome::Free => {
            if json {
                println!("{}", serde_json::json!({ "running": false }));
            } else {
                println!("Daemon not running.");
            }
            anyhow::bail!("daemon not running");
        }
    };

    let snapshot = status::StatusSnapshot::gather(&pf, &config);
    if json {
        println!("{}", snapshot.to_json());
    } else {
        println!("{}", snapshot.to_human());
    }
    Ok(())
}

#[cfg(unix)]
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
