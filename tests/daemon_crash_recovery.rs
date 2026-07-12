//! Story 12.1b integration tests — the daemon supervision + crash-recovery surface
//! through `assert_cmd` (drives the built binary end-to-end). Unix-only.
//!
//! Three-layer test strategy (party-mode Q4 / AC-12-1b-7):
//!  - **Layer 1** — template-content assertions live as pure unit tests in
//!    `adapters::daemon::service` (rendered-string in → token out, no FS).
//!  - **Layer 2** — the in-process simulated crash-recovery cycle + the
//!    `install`/`uninstall` round-trip live HERE (CI-enforced, no init system).
//!  - **Layer 3** — the real-systemd E2E is a named, runnable `#[ignore]` test
//!    (`daemon_real_systemd_recovery`) wired into a systemd-equipped CI lane.
//!
//! Isolation: every test uses its own temp workspace + `RUSTAIN_DATA_DIR` +
//! `RUSTAIN_CONFIG_DIR` + (for install) `RUSTAIN_SERVICE_DIR`, set per-spawned-process
//! (never the test process's global env), so the tests are parallel-safe.
//!
//! Determinism > realism, NO `sleep`: assertions are on file existence + exit code +
//! stdout, synchronous after each command returns.

#![cfg(unix)]

use std::path::Path;

use assert_cmd::Command;
use predicates::str::contains;
use rustain::domain::models::{JournalEntry, JournalRecord, NodeState};

struct Dirs {
    ws: tempfile::TempDir,
    data: tempfile::TempDir,
    cfg: tempfile::TempDir,
    /// Isolated service-file install root (`RUSTAIN_SERVICE_DIR` override).
    svc: tempfile::TempDir,
    /// Isolated log root (`RUSTAIN_LOG_PATH`) so the recovery line is assertable.
    logdir: tempfile::TempDir,
}

fn dirs() -> Dirs {
    Dirs {
        ws: tempfile::tempdir().unwrap(),
        data: tempfile::tempdir().unwrap(),
        cfg: tempfile::tempdir().unwrap(),
        svc: tempfile::tempdir().unwrap(),
        logdir: tempfile::tempdir().unwrap(),
    }
}

impl Dirs {
    fn cmd(&self) -> Command {
        let mut c = Command::cargo_bin("rustain").expect("cargo bin rustain");
        c.current_dir(self.ws.path())
            .env("RUSTAIN_DATA_DIR", self.data.path())
            .env("RUSTAIN_CONFIG_DIR", self.cfg.path())
            .env("RUSTAIN_SERVICE_DIR", self.svc.path())
            .env("RUSTAIN_LOG_PATH", self.logdir.path().join("rustain.log"));
        c
    }
    fn pid_path(&self) -> std::path::PathBuf {
        self.ws.path().join(".rustain").join("daemon.pid")
    }
    fn crash_path(&self) -> std::path::PathBuf {
        self.ws.path().join(".rustain").join("daemon-crash.json")
    }
    /// Concatenate every rolling log file (`rustain.log.<date>`) so the async,
    /// flushed-on-exit recovery line can be asserted after the daemon has stopped.
    fn read_all_logs(&self) -> String {
        let mut out = String::new();
        if let Ok(entries) = std::fs::read_dir(self.logdir.path()) {
            for e in entries.flatten() {
                if let Ok(body) = std::fs::read_to_string(e.path()) {
                    out.push_str(&body);
                }
            }
        }
        out
    }
}

fn latest_node_state(dirs: &Dirs, node_id: &str) -> Option<NodeState> {
    let rooms = std::fs::read_dir(dirs.ws.path().join(".rustain").join("rooms")).ok()?;
    let mut latest = None;
    for entry in rooms.flatten() {
        let body = std::fs::read_to_string(entry.path()).ok()?;
        for line in body.lines() {
            let entry: JournalEntry = serde_json::from_str(line).ok()?;
            match entry.record {
                JournalRecord::Checkpoint(checkpoint) if checkpoint.id.as_str() == node_id => {
                    latest = Some(checkpoint.state);
                }
                _ => {}
            }
        }
    }
    latest
}

fn wait_for_node_state(
    dirs: &Dirs,
    node_id: &str,
    expected: NodeState,
    budget: std::time::Duration,
) -> bool {
    let deadline = std::time::Instant::now() + budget;
    loop {
        if latest_node_state(dirs, node_id) == Some(expected) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

fn wait_for_process_exit(pid: u32, budget: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + budget;
    loop {
        // SAFETY: signal 0 performs existence/permission probing only.
        let mut alive = unsafe { libc::kill(pid as i32, 0) } == 0;
        #[cfg(target_os = "linux")]
        if alive {
            alive = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| stat.rsplit_once(')').map(|(_, tail)| tail.to_string()))
                .and_then(|tail| tail.split_whitespace().next().map(str::to_string))
                .is_some_and(|state| state != "Z");
        }
        if !alive {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

/// Spawn a short-lived child and reap it, returning its now-dead PID. This is the
/// guaranteed-dead PID the cycle test points the PID file at — NOT a random integer
/// (which would reintroduce the recycle risk into the test itself; AC-12-1b-7 L2).
fn reaped_dead_pid() -> u32 {
    let mut child = std::process::Command::new("true")
        .spawn()
        .expect("spawn /bin/true");
    let pid = child.id();
    child.wait().expect("reap true"); // reaped → /proc entry gone → dead
    pid
}

/// Hand-write a PID file pointing at `dead_pid` (the unclean-exit crash signal).
fn write_stale_pid_file(pid_path: &Path, workspace: &Path, dead_pid: u32) {
    std::fs::create_dir_all(pid_path.parent().unwrap()).unwrap();
    let body = format!(
        "pid = {dead_pid}\n\
         socket_path = \"/tmp/rustain-crash-test.sock\"\n\
         workspace = \"{}\"\n\
         started_at_unix = 1700000000\n\
         profile = \"coding\"\n",
        workspace.display()
    );
    std::fs::write(pid_path, body).unwrap();
}

// ── Layer 2 — install / uninstall round-trip (Murat's highest-value cheap test) ──

#[test]
fn install_print_then_install_uninstall_roundtrip() {
    let d = dirs();

    // `--print` → exit 0, directives on stdout, NO file written.
    #[cfg(target_os = "linux")]
    d.cmd()
        .args(["daemon", "install", "--print"])
        .assert()
        .success()
        .stdout(contains("daemon start --foreground"))
        .stdout(contains("Restart=on-failure"));
    #[cfg(target_os = "macos")]
    d.cmd()
        .args(["daemon", "install", "--print"])
        .assert()
        .success()
        .stdout(contains("--foreground"))
        .stdout(contains("SuccessfulExit"));
    assert_eq!(
        std::fs::read_dir(d.svc.path()).unwrap().count(),
        0,
        "--print must NOT write a service file"
    );

    // Install (no --print) → writes the workspace-hashed file + prints follow-up.
    d.cmd()
        .args(["daemon", "install"])
        .assert()
        .success()
        .stdout(contains("Installed service file"));
    let installed: Vec<_> = std::fs::read_dir(d.svc.path())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().into_string().unwrap())
        .collect();
    assert_eq!(installed.len(), 1, "install must write exactly one file");
    #[cfg(target_os = "linux")]
    assert!(installed[0].starts_with("rustain-") && installed[0].ends_with(".service"));
    #[cfg(target_os = "macos")]
    assert!(installed[0].starts_with("com.rustain.") && installed[0].ends_with(".plist"));

    // Uninstall → removes it + prints the disable/unload follow-up.
    d.cmd()
        .args(["daemon", "uninstall"])
        .assert()
        .success()
        .stdout(contains("Removed service file"));
    assert_eq!(
        std::fs::read_dir(d.svc.path()).unwrap().count(),
        0,
        "uninstall must remove the service file"
    );

    // Idempotency: a second uninstall (file already gone) is an exit-0 no-op.
    d.cmd()
        .args(["daemon", "uninstall"])
        .assert()
        .success()
        .stdout(contains("No service file installed"));
}

#[test]
fn install_passes_through_data_dir_env_override() {
    // AC-12-1b-1: RUSTAIN_DATA_DIR set in the generating env survives into the unit so
    // test/CI overrides keep working under supervision.
    let d = dirs();
    #[cfg(target_os = "linux")]
    d.cmd()
        .args(["daemon", "install", "--print"])
        .assert()
        .success()
        .stdout(contains("Environment=RUSTAIN_DATA_DIR="));
    #[cfg(target_os = "macos")]
    d.cmd()
        .args(["daemon", "install", "--print"])
        .assert()
        .success()
        .stdout(contains("EnvironmentVariables"));
}

// ── Layer 2 — simulated crash-recovery cycle (the heart of the story) ────────────

#[test]
fn crash_recovery_cycle_records_and_surfaces_then_starts_normally() {
    let d = dirs();
    let dead_pid = reaped_dead_pid();
    write_stale_pid_file(&d.pid_path(), d.ws.path(), dead_pid);

    // start → the re-exec'd foreground daemon detects the stale PID file, records the
    // crash, announces recovery, reclaims, and proceeds to normal startup (AC-12-1b-4).
    d.cmd()
        .args(["daemon", "start"])
        .assert()
        .success()
        .stdout(contains("Daemon started"));

    // (a) daemon-crash.json with the stale-PID shape (reason + prev pid + count=1).
    let crash_body = std::fs::read_to_string(d.crash_path()).expect("daemon-crash.json written");
    let crash: serde_json::Value = serde_json::from_str(&crash_body).unwrap();
    assert_eq!(crash["reason"], "stale-pidfile");
    assert_eq!(crash["pid"], dead_pid);
    assert_eq!(crash["restart_count"], 1);
    assert_eq!(crash["last_n_crash_unix"].as_array().unwrap().len(), 1);

    // (b) status --json reports last_crash AND the daemon proceeded to run.
    d.cmd()
        .args(["daemon", "status", "--json"])
        .assert()
        .success()
        .stdout(contains("\"running\": true"))
        .stdout(contains("\"last_crash\""))
        .stdout(contains(format!("\"pid\": {dead_pid}")));

    // stop → clean exit (flushes the async daemon log).
    d.cmd().args(["daemon", "stop"]).assert().success();

    // (c) the AC-12-1b-6 recovery line landed in the daemon log (flushed by stop).
    assert!(
        d.read_all_logs().contains("recovered from unclean exit"),
        "recovery line must be logged to the daemon log"
    );
}

#[test]
fn node_journal_real_sigkill_restart_recovers_running_to_suspended() {
    let d = dirs();
    let node_id = "sigkill-recovery-node";
    d.cmd()
        .env("RUSTAIN_TEST_ARM_NODE_RECOVERY", node_id)
        .env("RUSTAIN_HOST_ID", "host-a")
        .args(["daemon", "start"])
        .assert()
        .success()
        .stdout(contains("Daemon started"));
    let pid = wait_for_pid(&d.pid_path(), std::time::Duration::from_secs(10))
        .expect("armed daemon must publish its PID");
    assert!(
        wait_for_node_state(
            &d,
            node_id,
            NodeState::Running,
            std::time::Duration::from_secs(10)
        ),
        "positive control: real composition must durably publish Running before SIGKILL"
    );

    let status = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .expect("send SIGKILL");
    assert!(status.success(), "SIGKILL command must succeed");
    assert!(
        wait_for_process_exit(pid, std::time::Duration::from_secs(10)),
        "killed daemon must exit and release the singleton"
    );

    d.cmd()
        .env("RUSTAIN_HOST_ID", "host-a")
        .args(["daemon", "start"])
        .assert()
        .success()
        .stdout(contains("Daemon started"));
    assert!(
        wait_for_node_state(
            &d,
            node_id,
            NodeState::Suspended,
            std::time::Duration::from_secs(10)
        ),
        "restart under singleton must recover the crashed node to Suspended"
    );
    assert_ne!(
        latest_node_state(&d, node_id),
        Some(NodeState::Running),
        "mutant: restart must never leave a phantom Running node"
    );
    d.cmd().args(["daemon", "stop"]).assert().success();
}

#[test]
fn clean_start_records_no_crash() {
    // AC-12-1b-4: a clean start (no pre-existing PID file) records NO crash event.
    let d = dirs();
    d.cmd().args(["daemon", "start"]).assert().success();
    assert!(
        !d.crash_path().exists(),
        "a clean start must not fabricate a crash record"
    );
    d.cmd().args(["daemon", "stop"]).assert().success();
}

/// Negative case (AC-12-1b-7 L2): a genuinely-running daemon must make a second
/// `start` refuse (ownership confirms it is ours → `Running`) WITHOUT fabricating a
/// crash record. Exercises the AC-12-1b-8 positive ownership path.
#[test]
fn second_start_on_live_daemon_refuses_and_records_no_crash() {
    let d = dirs();
    d.cmd().args(["daemon", "start"]).assert().success();

    d.cmd()
        .args(["daemon", "start"])
        .assert()
        .failure()
        .stderr(contains("Daemon already running"));

    assert!(
        !d.crash_path().exists(),
        "refusing a live daemon must NOT fabricate a crash record"
    );

    d.cmd().args(["daemon", "stop"]).assert().success();
}

// ── Layer 3 — real-init E2E (gated; runnable, not vibes) ─────────────────────────

/// **Layer 3 (AC-12-1b-7):** the ONLY check that proves the integration half of
/// NFR50 — that systemd actually parses + honors our generated unit, relaunches the
/// daemon after a `kill -9`, AND the relaunched instance records the crash.
///
/// `#[ignore]`d out of the default `cargo test` suite (party-mode Q4: a live init
/// system is flaky/root-requiring/container-hostile — determinism > realism). It is
/// **named + runnable** for the systemd-equipped CI lane / epic-close gate for the
/// Linux P0 platform:
///
/// ```text
/// cargo test --test daemon_crash_recovery --ignored daemon_real_systemd_recovery
/// ```
///
/// macOS/launchd (P1) is a documented manual sign-off (see `docs/daemon.md`).
///
/// **Test-review checklist (AC-12-1b-7):** every token this real-init test depends on
/// has a corresponding Layer-1 content assertion in `adapters::daemon::service`, so a
/// token regression fails loudly in CI's cheap layer before this lane runs:
///
/// - `daemon start --foreground` → `systemd_unit_uses_foreground_and_restart_policy`
/// - `Restart=on-failure` / `StartLimit*` / `Type=simple` → same test
/// - launchd `KeepAlive`/`SuccessfulExit=false` → `launchd_plist_keepalive_*`
#[test]
#[ignore = "Layer 3: requires a live systemd --user session (Linux P0 CI lane / epic-close gate)"]
#[cfg(target_os = "linux")]
fn daemon_real_systemd_recovery() {
    use std::process::Command as Proc;

    /// RAII teardown (Story 12.1c P3 — Amelia/Murat). Built BEFORE `enable --now`, so
    /// even if a later step panics (e.g. the daemon hasn't written its PID yet) the
    /// `Drop` still `disable --now`s the unit — otherwise a `Restart=`-respawning unit
    /// would be left enabled on the box, re-spawning forever and stacking every run.
    struct UnitGuard {
        unit: String,
        path: std::path::PathBuf,
    }
    impl Drop for UnitGuard {
        fn drop(&mut self) {
            let _ = Proc::new("systemctl")
                .args(["--user", "disable", "--now", &self.unit])
                .status();
            let _ = std::fs::remove_file(&self.path);
            let _ = Proc::new("systemctl")
                .args(["--user", "reset-failed", &self.unit])
                .status();
            let _ = Proc::new("systemctl")
                .args(["--user", "daemon-reload"])
                .status();
        }
    }

    // Precondition: a *running* user systemd manager (Story 12.1c P3 fix). NOTE:
    // `is-system-running` runs (so `.output()` is Ok) even when the manager is
    // offline — it prints the state and exits non-zero. A bare `.is_err()` therefore
    // passes the guard, and the test then dies later at `enable --now` with a
    // MISLEADING "daemon didn't write its PID" timeout. Detect the real cause HERE:
    // `enable --now` needs a live manager (XDG_RUNTIME_DIR + DBus); without it,
    // skip-with-a-loud-actionable-failure rather than blaming the daemon.
    let state = Proc::new("systemctl")
        .args(["--user", "is-system-running"])
        .output();
    let state_str = state
        .as_ref()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "<systemctl missing>".to_string());
    let manager_running = matches!(state_str.as_str(), "running" | "degraded" | "starting");
    let has_runtime = std::env::var_os("XDG_RUNTIME_DIR").is_some();
    if !manager_running || !has_runtime {
        panic!(
            "Layer 3 needs a RUNNING `systemd --user` session — not present here \
             (state={state_str:?}, XDG_RUNTIME_DIR set={has_runtime}). `enable --now` \
             refuses without a live user manager + DBus. Run this from a graphical \
             login, OR `loginctl enable-linger $USER` then re-run inside a logged-in \
             user session (`machinectl shell $USER@.host` / `ssh` with lingering), OR \
             a systemd-equipped CI lane. This is an ENVIRONMENT gap, not a daemon bug."
        );
    }

    let d = dirs();

    // 1. Render + install the generated unit (isolated RUSTAIN_SERVICE_DIR), then link
    //    it where systemd --user looks. The unit carries WorkingDirectory=<isolated
    //    workspace> and Environment=RUSTAIN_DATA_DIR/CONFIG_DIR (install passes them
    //    through), so the systemd-launched daemon resolves crash/pid artifacts in the
    //    isolated dirs the test polls — NOT the real $HOME.
    d.cmd().args(["daemon", "install"]).assert().success();
    let unit = std::fs::read_dir(d.svc.path())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().into_string().unwrap())
        .next()
        .expect("installed unit");
    let user_unit_dir = dirs::config_dir().unwrap().join("systemd").join("user");
    std::fs::create_dir_all(&user_unit_dir).unwrap();
    let installed_path = user_unit_dir.join(&unit);
    std::fs::copy(d.svc.path().join(&unit), &installed_path).unwrap();

    // Guard armed BEFORE enable — every early-return/panic path now cleans up.
    let _guard = UnitGuard {
        unit: unit.clone(),
        path: installed_path,
    };

    Proc::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status()
        .unwrap();
    Proc::new("systemctl")
        .args(["--user", "enable", "--now", &unit])
        .status()
        .unwrap();

    // 2. Bounded wait for the daemon to write its PID (enable --now returns once the
    //    unit is STARTED, not once our process has written the file). NO bare unwrap
    //    immediately after enable — that race is what leaked a unit before.
    let pid = wait_for_pid(&d.pid_path(), std::time::Duration::from_secs(10))
        .expect("daemon must write its PID within 10s of enable --now");

    // 3. kill -9 the live daemon (unclean exit), let systemd Restart= relaunch it.
    Proc::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .unwrap();

    // 4. Poll (bounded) until the relaunched instance records the crash. Crossing a
    //    real kill→supervisor→file boundary has no in-process seam to await, so a
    //    bounded poll is the correct L3 idiom (sanctioned, not a no-sleep violation).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let recovered = loop {
        if let Ok(body) = std::fs::read_to_string(d.crash_path()) {
            if body.contains("stale-pidfile") {
                break true;
            }
        }
        if std::time::Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    };

    // _guard.drop() runs here (and on any panic above), disabling the respawning unit.
    assert!(
        recovered,
        "systemd must relaunch the killed daemon AND the relaunch must record the crash"
    );
}

/// Poll for the daemon PID file to exist + parse, up to `budget`. Returns the PID, or
/// `None` on timeout. Bounded poll is the correct idiom for an external-writer file
/// (Story 12.1c P3) — no in-process seam to await.
#[cfg(unix)]
fn wait_for_pid(pid_path: &Path, budget: std::time::Duration) -> Option<u32> {
    let deadline = std::time::Instant::now() + budget;
    loop {
        if let Ok(body) = std::fs::read_to_string(pid_path) {
            if let Some(pid) = body
                .lines()
                .find_map(|l| l.strip_prefix("pid = "))
                .and_then(|n| n.trim().parse::<u32>().ok())
            {
                return Some(pid);
            }
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Read an env-overridable wait budget in seconds (Story 12-1d AC-12-1d-1).
/// CI lanes set 2-3× local defaults because shared runners are the #1 L3 flake source.
#[cfg(unix)]
fn env_budget_secs(var: &str, default: u64) -> std::time::Duration {
    std::time::Duration::from_secs(
        std::env::var(var)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(default),
    )
}

/// **Layer 3 (AC-12-1d-1):** `--system` variant of the systemd recovery gate.
///
/// GH `ubuntu-latest` runs systemd as PID 1 but has NO user-session manager
/// (`XDG_RUNTIME_DIR`/DBus absent), so the existing `--user` L3 correctly panics
/// there. This variant uses `--system` scope + passwordless `sudo` to sidestep
/// the user-session gap, proving NFR50 on CI. The `--user` test stays for local
/// real-session runs.
///
/// Wait budgets are env-overridable (`RUSTAIN_L3_PID_WAIT_SECS`,
/// `RUSTAIN_L3_CRASH_WAIT_SECS`) so CI lanes can set 2-3× local values.
#[test]
#[ignore = "Layer 3: requires systemd --system scope + passwordless sudo (CI lane / epic-close gate)"]
#[cfg(target_os = "linux")]
fn daemon_real_systemd_recovery_system() {
    use std::process::Command as Proc;

    struct SystemUnitGuard {
        unit: String,
        dest: std::path::PathBuf,
    }
    impl Drop for SystemUnitGuard {
        fn drop(&mut self) {
            let _ = Proc::new("sudo")
                .args(["systemctl", "disable", "--now", &self.unit])
                .status();
            let _ = Proc::new("sudo")
                .args(["rm", "-f", &self.dest.display().to_string()])
                .status();
            let _ = Proc::new("sudo")
                .args(["systemctl", "reset-failed", &self.unit])
                .status();
            let _ = Proc::new("sudo")
                .args(["systemctl", "daemon-reload"])
                .status();
        }
    }

    // ── Precondition: running system manager + passwordless sudo ──────────
    let state = Proc::new("systemctl").args(["is-system-running"]).output();
    let state_str = state
        .as_ref()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "<systemctl missing>".to_string());
    let manager_running = matches!(state_str.as_str(), "running" | "degraded" | "starting");
    if !manager_running {
        panic!(
            "Layer 3 --system needs a RUNNING system manager — not present here \
             (state={state_str:?}). This is an ENVIRONMENT gap, not a daemon bug."
        );
    }
    let sudo_ok = Proc::new("sudo")
        .args(["-n", "true"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !sudo_ok {
        panic!(
            "Layer 3 --system needs passwordless sudo (`sudo -n true` failed). \
             On GH runners this is automatic; locally, configure NOPASSWD or use \
             the --user L3 test instead. This is an ENVIRONMENT gap, not a daemon bug."
        );
    }

    // Clean up any failed prior run before we start.
    let d = dirs();

    // 1. Render the unit via `daemon install --system` with isolated RUSTAIN_SERVICE_DIR.
    d.cmd()
        .args(["daemon", "install", "--system"])
        .assert()
        .success();
    let unit = std::fs::read_dir(d.svc.path())
        .unwrap()
        .map(|e| e.unwrap())
        .map(|e| e.file_name().into_string().unwrap())
        .find(|name| name.ends_with(".service"))
        .expect("installed unit");

    // Verify the rendered unit carries User= and Environment= passthrough.
    let unit_body = std::fs::read_to_string(d.svc.path().join(&unit)).unwrap();
    assert!(
        unit_body.contains("User="),
        "--system unit must carry User= directive"
    );
    if std::env::var_os("RUSTAIN_DATA_DIR").is_some() {
        assert!(
            unit_body.contains("Environment=RUSTAIN_DATA_DIR="),
            "unit must pass through RUSTAIN_DATA_DIR when set"
        );
    }

    // Copy into the system unit directory with correct permissions.
    let dest = std::path::PathBuf::from("/etc/systemd/system").join(&unit);
    Proc::new("sudo")
        .args([
            "install",
            "-m",
            "644",
            &d.svc.path().join(&unit).display().to_string(),
            &dest.display().to_string(),
        ])
        .status()
        .expect("sudo install");

    // Guard armed BEFORE enable — every panic path cleans up.
    let _guard = SystemUnitGuard {
        unit: unit.clone(),
        dest: dest.clone(),
    };

    // Reset any prior failed state for this unit name.
    let _ = Proc::new("sudo")
        .args(["systemctl", "reset-failed", &unit])
        .status();
    Proc::new("sudo")
        .args(["systemctl", "daemon-reload"])
        .status()
        .unwrap();
    Proc::new("sudo")
        .args(["systemctl", "enable", "--now", &unit])
        .status()
        .unwrap();

    // 2. Wait for the daemon to write its PID.
    let pid_wait = env_budget_secs("RUSTAIN_L3_PID_WAIT_SECS", 10);
    let pid = wait_for_pid(&d.pid_path(), pid_wait)
        .expect("daemon must write its PID within the PID-wait budget after enable --now");

    // 3. kill -9 the live daemon, let systemd Restart= relaunch.
    Proc::new("sudo")
        .args(["kill", "-9", &pid.to_string()])
        .status()
        .unwrap();

    // 4. Bounded poll for the crash record.
    let crash_wait = env_budget_secs("RUSTAIN_L3_CRASH_WAIT_SECS", 20);
    let deadline = std::time::Instant::now() + crash_wait;
    let recovered = loop {
        if let Ok(body) = std::fs::read_to_string(d.crash_path()) {
            if body.contains("stale-pidfile") {
                break true;
            }
        }
        if std::time::Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    };

    assert!(
        recovered,
        "systemd --system must relaunch the killed daemon AND the relaunch must record the crash"
    );
    let new_pid =
        wait_for_pid(&d.pid_path(), pid_wait).expect("relaunched daemon must write a new PID");
    assert_ne!(
        new_pid, pid,
        "relaunched daemon must have a different PID from the killed one"
    );
}

// ── Layer 3 — launchd real-init E2E (macOS P1) ─────────────────────────────────

/// **Layer 3 (AC-12-1d-2):** launchd supervision + crash-recovery gate.
///
/// Installs the plist, bootstraps the agent, `kill -9`s the daemon, and asserts
/// launchd relaunches it AND the relaunched instance records the crash. The 60s
/// poll budget accounts for launchd's default `ThrottleInterval` (10s).
///
/// Wait budgets are env-overridable (`RUSTAIN_L3_PID_WAIT_SECS`,
/// `RUSTAIN_L3_CRASH_WAIT_SECS`) so the CI lane sets defensive values.
#[test]
#[ignore = "Layer 3: requires a macOS launchd user domain (macos-latest CI lane / epic-close gate)"]
#[cfg(target_os = "macos")]
fn daemon_real_launchd_recovery() {
    use std::process::Command as Proc;

    struct PlistGuard {
        label: String,
        uid: String,
        plist_path: std::path::PathBuf,
    }
    impl Drop for PlistGuard {
        fn drop(&mut self) {
            // bootout BEFORE any residual pid handling (KeepAlive races kill with respawn).
            let target = format!("gui/{}/{}", self.uid, self.label);
            let bootout_ok = Proc::new("launchctl")
                .args(["bootout", &target])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !bootout_ok {
                let _ = Proc::new("launchctl")
                    .args(["unload", "-w", &self.plist_path.display().to_string()])
                    .status();
            }
            let _ = std::fs::remove_file(&self.plist_path);
        }
    }

    // ── Precondition: launchd user domain accessible ──────────────────────
    let uid = Proc::new("id")
        .arg("-u")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let domain_target = format!("gui/{uid}");
    let domain_ok = Proc::new("launchctl")
        .args(["print", &domain_target])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !domain_ok {
        panic!(
            "Layer 3 launchd needs a user domain (`launchctl print gui/{uid}` failed). \
             On GH macos-latest the GUI domain should exist. This is an ENVIRONMENT gap \
             — not a daemon bug. If `gui/{uid}` is unavailable, the launchd supervision \
             claim cannot be proven on this runner."
        );
    }

    let d = dirs();

    // 1. Render the plist via `daemon install` with isolated RUSTAIN_SERVICE_DIR.
    d.cmd().args(["daemon", "install"]).assert().success();
    let plist_name = std::fs::read_dir(d.svc.path())
        .unwrap()
        .map(|e| e.unwrap())
        .map(|e| e.file_name().into_string().unwrap())
        .find(|name| name.ends_with(".plist"))
        .expect("installed plist");
    let label = plist_name.strip_suffix(".plist").unwrap().to_string();

    // Copy into ~/Library/LaunchAgents.
    let launch_agents = dirs::home_dir()
        .unwrap()
        .join("Library")
        .join("LaunchAgents");
    std::fs::create_dir_all(&launch_agents).unwrap();
    let plist_dest = launch_agents.join(&plist_name);
    std::fs::copy(d.svc.path().join(&plist_name), &plist_dest).unwrap();

    // Re-run safety: bootout stale label before bootstrap.
    let label_target = format!("gui/{uid}/{label}");
    let label_exists = Proc::new("launchctl")
        .args(["print", &label_target])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if label_exists {
        let _ = Proc::new("launchctl")
            .args(["bootout", &label_target])
            .status();
        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    // Guard armed BEFORE bootstrap.
    let _guard = PlistGuard {
        label: label.clone(),
        uid: uid.clone(),
        plist_path: plist_dest.clone(),
    };

    // Bootstrap ladder: bootstrap first, fall back to load -w on failure.
    let bootstrap_ok = Proc::new("launchctl")
        .args([
            "bootstrap",
            &domain_target,
            &plist_dest.display().to_string(),
        ])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !bootstrap_ok {
        let bootstrap_err = Proc::new("launchctl")
            .args([
                "bootstrap",
                &domain_target,
                &plist_dest.display().to_string(),
            ])
            .output()
            .map(|o| {
                format!(
                    "stdout={} stderr={}",
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr)
                )
            })
            .unwrap_or_else(|e| format!("failed to run: {e}"));
        let load_ok = Proc::new("launchctl")
            .args(["load", "-w", &plist_dest.display().to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !load_ok {
            let load_err = Proc::new("launchctl")
                .args(["load", "-w", &plist_dest.display().to_string()])
                .output()
                .map(|o| {
                    format!(
                        "stdout={} stderr={}",
                        String::from_utf8_lossy(&o.stdout),
                        String::from_utf8_lossy(&o.stderr)
                    )
                })
                .unwrap_or_else(|e| format!("failed to run: {e}"));
            panic!(
                "Both `launchctl bootstrap gui/{uid} {plist}` and `launchctl load -w {plist}` failed.\n\
                 Bootstrap output: {bootstrap_err}\n\
                 Load output: {load_err}\n\
                 This is descope evidence (AC-12-1d-2 showstopper): the runner \
                 environment does not permit LaunchAgent bootstrap.",
                uid = uid,
                plist = plist_dest.display(),
                bootstrap_err = bootstrap_err,
                load_err = load_err,
            );
        }
    }

    // 2. Wait for daemon PID.
    let pid_wait = env_budget_secs("RUSTAIN_L3_PID_WAIT_SECS", 15);
    let pid = wait_for_pid(&d.pid_path(), pid_wait)
        .expect("daemon must write its PID within the PID-wait budget after launchd bootstrap");

    // 3. kill -9 → launchd KeepAlive{SuccessfulExit=false} relaunches.
    Proc::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .unwrap();

    // 4. Bounded poll for crash record. 60s to account for ThrottleInterval (10s default).
    let crash_wait = env_budget_secs("RUSTAIN_L3_CRASH_WAIT_SECS", 60);
    let deadline = std::time::Instant::now() + crash_wait;
    let recovered = loop {
        if let Ok(body) = std::fs::read_to_string(d.crash_path()) {
            if body.contains("stale-pidfile") {
                break true;
            }
        }
        if std::time::Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    };

    assert!(
        recovered,
        "launchd must relaunch the killed daemon AND the relaunch must record the crash"
    );
    let new_pid =
        wait_for_pid(&d.pid_path(), pid_wait).expect("relaunched daemon must write a new PID");
    assert_ne!(
        new_pid, pid,
        "relaunched daemon must have a different PID from the killed one"
    );
}
