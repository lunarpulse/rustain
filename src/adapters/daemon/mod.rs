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

use anyhow::{Context, Result};

use crate::adapters::cli::commands::DaemonAction;
use crate::domain::models::AppConfig;

#[cfg(unix)]
mod crash;
#[cfg(unix)]
mod lifecycle;
#[cfg(unix)]
mod pidfile;
#[cfg(unix)]
mod service;
#[cfg(unix)]
pub mod session_queue;
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
            // install/uninstall are pure generate/remove — no memory composition, no
            // async I/O (AC-12-1b-3/3b). Called synchronously inside the async fn.
            DaemonAction::Install { print, system } => {
                run_daemon_install(workspace, config, print, system)
            }
            DaemonAction::Uninstall { system } => run_daemon_uninstall(workspace, system),
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
            // A leftover PID file whose process is gone is an unclean exit
            // (AC-12-1b-4). Do NOT reclaim/record here: leave it for the re-exec'd
            // foreground child, which owns the SINGLE crash-detection path so the
            // recovery line lands in the daemon log (the child's stdout). The
            // readiness poll below waits for the child to overwrite this stale file
            // with its own PID, so leaving it is safe.
            tracing::info!(
                "found stale daemon PID file at {}; the daemon will record + reclaim it",
                pid_path.display()
            );
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
    // Lineage nonce injection (Story 12.1c P1): generate the nonce HERE and pass it to
    // the child via the environment so the live daemon *carries* it (observable via
    // `/proc/<pid>/environ`). This makes the nonce load-bearing for ownership — a
    // recycled foreign PID won't echo it, so `stop`/the guard won't mistake it for
    // ours (D-1). The child writes this same nonce into its PID file.
    cmd.env(pidfile::DAEMON_NONCE_ENV, pidfile::generate_nonce());
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

    // Defense-in-depth guard inside the child too (the parent already checked, but a
    // foreground invocation — the supervised systemd/launchd entrypoint — skips the
    // parent path entirely). This is THE crash-detection seam (AC-12-1b-4): when the
    // supervisor relaunches us after an unclean exit, the leftover PID file shows up
    // here as `Stale`.
    match pidfile::check_running(&pid_path) {
        GuardOutcome::Running(pf) => {
            eprintln!(
                "Daemon already running (PID: {}). Use 'rustain daemon stop' first.",
                pf.pid
            );
            anyhow::bail!("daemon already running");
        }
        GuardOutcome::Stale => {
            // Unclean prior exit → record + announce, then reclaim and start normally
            // (MUST NOT refuse to start / require manual cleanup — AC-12-1b-4).
            crash::detect_and_record_stale(&workspace, &pid_path);
            pidfile::remove(&pid_path);
        }
        // Clean start (no pre-existing PID file) records no crash event.
        GuardOutcome::Free => {}
    }

    // Compose ONLY the memory port (see composition::build_daemon_memory docs):
    // 12.1a has no message runtime, so the only port with a shutdown obligation
    // is memory (its flush must route through 12.0's hardened prepare_detach sink).
    let memory =
        crate::infrastructure::composition::build_daemon_memory(&workspace, &memory_adapter)
            .map_err(|e| anyhow::anyhow!("composing daemon memory adapter: {e}"))?;

    // Story 12.1c AC4 — the recall provider defaults to the explicit offline no-op
    // (`NoopRecallProvider`). The daemon has no external recall backend composed; a
    // real provider arrives with the message runtime (Story 11.5/12.2). The seam is
    // still DRIVEN every boundary (prove-the-seam-is-called, 12.1b lesson).
    let recall: std::sync::Arc<dyn crate::domain::ports::RecallProviderPort> =
        std::sync::Arc::new(crate::adapters::noop::NoopRecallProvider);

    let rt = DaemonRuntime {
        config: config.clone(),
        memory,
        recall,
        workspace: workspace.clone(),
        pid_path: pid_path.clone(),
        socket_path: socket_path.clone(),
    };

    // Write the PID file LAST (after we know paths resolve) — it is the readiness
    // marker the parent `start` polls for. Records socket + workspace + start time
    // (AC-12-1a-8) so status/stop/attach read rather than re-derive.
    let started_at_unix = now_unix();
    let pid = std::process::id();
    let pf = DaemonPidFile {
        pid,
        socket_path,
        workspace: workspace.clone(),
        started_at_unix,
        profile: config.active_profile.clone(),
        // Lineage hardening (Story 12.1b AC-12-1b-8, 12.1c P1): use the nonce the
        // parent `start` injected via env (so the PID-file nonce == the nonce this
        // process carries in its environment, making ownership verifiable); fall back
        // to a fresh nonce when started directly (systemd/launchd/`--foreground`),
        // where ownership instead rests on the exact-comm + argv-token fallback.
        nonce: crate::infrastructure::utils::env_var_trimmed(pidfile::DAEMON_NONCE_ENV)
            .unwrap_or_else(pidfile::generate_nonce),
        boot_id: pidfile::current_boot_id(),
    };
    pf.write_atomic(&pid_path)?;

    // Headless daemon panic hook (AC-12-1b-5) — installed AFTER composition + PID
    // write so it carries full daemon context (pid/profile/workspace/started). It
    // writes a `reason: "panic: …"` crash record + a capped backtrace file WITHOUT
    // terminal assumptions, then chains to the prior (global TUI) hook. The stale-PID
    // detector above is the PRIMARY signal (catches SIGKILL/OOM, which no hook can);
    // this hook is best-effort backtrace enrichment for the panic death mode.
    crash::install_daemon_panic_hook(crash::DaemonPanicContext {
        pid,
        profile: config.active_profile.clone(),
        workspace: workspace.clone(),
        started_at_unix,
    });

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

/// The invoking user, for `--system` systemd `User=`. `$USER`/`$LOGNAME` with a
/// `root` fallback (the only plausible `--system` install context anyway).
#[cfg(unix)]
fn invoking_user() -> String {
    use crate::infrastructure::utils::env_var_trimmed;
    env_var_trimmed("USER")
        .or_else(|| env_var_trimmed("LOGNAME"))
        .unwrap_or_else(|| "root".to_string())
}

/// `daemon install` (AC-12-1b-3) — render the platform service file and either print
/// it (`--print`, stdout only) or write it to the resolved location + print the
/// follow-up commands. Pure generate; no memory composition, no daemon runtime state.
#[cfg(unix)]
fn run_daemon_install(
    workspace: PathBuf,
    config: AppConfig,
    print: bool,
    system: bool,
) -> Result<()> {
    use crate::infrastructure::paths;

    let exe = std::env::current_exe()
        .and_then(|p| {
            p.canonicalize().map_err(|e| {
                std::io::Error::new(e.kind(), format!("canonicalizing {}: {e}", p.display()))
            })
        })
        .context("resolving the rustain executable path (current_exe)")?
        .display()
        .to_string();
    let params = service::ServiceParams {
        exe,
        profile: config.active_profile.clone(),
        workspace: workspace.display().to_string(),
        user: invoking_user(),
        system,
        log_path: paths::daemon_log_path(&workspace)?.display().to_string(),
        label: paths::daemon_service_label(&workspace),
        // Pass env overrides through ONLY when set in the generating environment
        // (AC-12-1b-1): test/CI overrides survive; default installs rely on $HOME.
        data_dir: crate::infrastructure::utils::env_var_trimmed("RUSTAIN_DATA_DIR"),
        config_dir: crate::infrastructure::utils::env_var_trimmed("RUSTAIN_CONFIG_DIR"),
    };

    #[cfg(target_os = "macos")]
    let rendered = service::render_launchd_plist(&params);
    #[cfg(not(target_os = "macos"))]
    let rendered = service::render_systemd_unit(&params);

    if print {
        // stdout ONLY — no filesystem write (for inspection / piping).
        print!("{rendered}");
        return Ok(());
    }

    let dest = paths::daemon_service_path(&workspace, system)?;
    if let Some(parent) = dest.parent() {
        // User scope: create `~/.config/systemd/user` (or LaunchAgents). System scope:
        // /etc/systemd/system already exists; create_dir_all is a no-op there.
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating service dir {}", parent.display()))?;
    }
    std::fs::write(&dest, &rendered)
        .with_context(|| format!("writing service file {}", dest.display()))?;

    println!("Installed service file: {}", dest.display());
    let name = paths::daemon_service_file_name(&workspace);
    println!("\nNext, enable + start it:");
    #[cfg(target_os = "macos")]
    {
        let _ = (system, &name);
        println!("  launchctl load {}", dest.display());
    }
    #[cfg(not(target_os = "macos"))]
    {
        if system {
            println!("  sudo systemctl daemon-reload && sudo systemctl enable --now {name}");
        } else {
            println!("  systemctl --user daemon-reload && systemctl --user enable --now {name}");
        }
    }
    Ok(())
}

/// `daemon uninstall` (AC-12-1b-3b) — remove the workspace's service file
/// idempotently (missing file → exit 0 no-op) and print the disable/unload follow-up.
/// Touches NO daemon runtime state (PID file/socket/crash records are the lifecycle's
/// concern, not the installer's).
#[cfg(unix)]
fn run_daemon_uninstall(workspace: PathBuf, system: bool) -> Result<()> {
    use crate::infrastructure::paths;

    let dest = paths::daemon_service_path(&workspace, system)?;
    if !dest.exists() {
        // Idempotent: a second uninstall (or never-installed) is a success no-op.
        println!(
            "No service file installed for this workspace ({}).",
            dest.display()
        );
        return Ok(());
    }

    let name = paths::daemon_service_file_name(&workspace);
    println!("First disable + stop the running service (recommended):");
    #[cfg(target_os = "macos")]
    {
        let _ = (system, &name);
        println!("  launchctl unload {}", dest.display());
    }
    #[cfg(not(target_os = "macos"))]
    {
        if system {
            println!("  sudo systemctl disable --now {name}");
        } else {
            println!("  systemctl --user disable --now {name}");
        }
    }

    if let Err(e) = std::fs::remove_file(&dest) {
        if e.kind() != std::io::ErrorKind::NotFound {
            return Err(e).with_context(|| format!("removing service file {}", dest.display()));
        }
        // Concurrent uninstall already removed it — idempotent success.
        println!(
            "No service file installed for this workspace ({}).",
            dest.display()
        );
    } else {
        println!("Removed service file: {}", dest.display());
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
