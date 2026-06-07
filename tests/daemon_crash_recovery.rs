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
#[cfg(target_os = "linux")]
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
