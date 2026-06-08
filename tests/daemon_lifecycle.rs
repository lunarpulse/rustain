//! Story 12.1a integration tests — the real `rustain daemon` lifecycle through
//! `assert_cmd` (drives the built binary, so `start`'s re-exec/`setsid` detach is
//! exercised end-to-end). Unix-only (the daemon is Unix-only in 12.1a).
//!
//! Isolation: each test uses its own temp workspace + `RUSTAIN_DATA_DIR` +
//! `RUSTAIN_CONFIG_DIR`, set per-spawned-process (never via the test process's
//! global env), so the tests are parallel-safe. The PID file is workspace-scoped
//! and the socket lives under the temp data dir, so daemons never collide.

#![cfg(unix)]

use std::path::Path;
use std::time::{Duration, Instant};

use assert_cmd::Command;
use predicates::str::contains;

/// Build a `rustain` invocation pinned to the given isolated dirs.
fn daemon_cmd(workspace: &Path, data: &Path, config: &Path) -> Command {
    let mut c = Command::cargo_bin("rustain").expect("cargo bin rustain");
    c.current_dir(workspace)
        .env("RUSTAIN_DATA_DIR", data)
        .env("RUSTAIN_CONFIG_DIR", config);
    c
}

struct Dirs {
    ws: tempfile::TempDir,
    data: tempfile::TempDir,
    cfg: tempfile::TempDir,
}

fn dirs() -> Dirs {
    Dirs {
        ws: tempfile::tempdir().unwrap(),
        data: tempfile::tempdir().unwrap(),
        cfg: tempfile::tempdir().unwrap(),
    }
}

impl Dirs {
    fn cmd(&self) -> Command {
        daemon_cmd(self.ws.path(), self.data.path(), self.cfg.path())
    }
    fn pid_path(&self) -> std::path::PathBuf {
        self.ws.path().join(".rustain").join("daemon.pid")
    }
}

#[test]
fn start_status_stop_full_lifecycle() {
    let d = dirs();

    // start → ready within budget, prints PID, writes PID file (AC-1).
    d.cmd()
        .args(["daemon", "start"])
        .assert()
        .success()
        .stdout(contains("Daemon started"));
    assert!(d.pid_path().exists(), "start must write the PID file");

    // status → structured snapshot with the AC-2 fields; "Active conversations: 0"
    // is the honest 12.1a state.
    d.cmd()
        .args(["daemon", "status"])
        .assert()
        .success()
        .stdout(contains("Daemon running"))
        .stdout(contains("PID:"))
        .stdout(contains("Active conversations: 0"))
        .stdout(contains("Resident memory:"));

    // status --json → scriptable, running=true.
    d.cmd()
        .args(["daemon", "status", "--json"])
        .assert()
        .success()
        .stdout(contains("\"running\": true"))
        .stdout(contains("\"active_conversations\": 0"));

    // stop → graceful, removes PID file + socket (AC-3).
    d.cmd()
        .args(["daemon", "stop"])
        .assert()
        .success()
        .stdout(contains("Daemon stopped"));
    assert!(!d.pid_path().exists(), "stop must remove the PID file");

    // status after stop → clear "not running" line + non-zero exit (AC-2).
    d.cmd()
        .args(["daemon", "status"])
        .assert()
        .failure()
        .stdout(contains("Daemon not running"));
}

#[test]
fn second_start_reports_already_running_with_exact_message() {
    let d = dirs();
    d.cmd().args(["daemon", "start"]).assert().success();

    let pid_body = std::fs::read_to_string(d.pid_path()).expect("pid file");
    // pull the numeric pid out of the TOML `pid = N` line for the exact-message check
    let pid: u32 = pid_body
        .lines()
        .find_map(|l| l.strip_prefix("pid = "))
        .and_then(|n| n.trim().parse().ok())
        .expect("pid in file");

    d.cmd()
        .args(["daemon", "start"])
        .assert()
        .failure()
        .stderr(contains(format!(
            "Daemon already running (PID: {pid}). Use 'rustain daemon stop' first."
        )));

    d.cmd().args(["daemon", "stop"]).assert().success();
}

#[test]
fn stale_pid_file_is_reclaimed_automatically() {
    let d = dirs();
    // Hand-write a PID file pointing at an almost-certainly-dead PID (AC-9 stale path).
    std::fs::create_dir_all(d.ws.path().join(".rustain")).unwrap();
    let stale = format!(
        "pid = 2147483632\n\
         socket_path = \"/tmp/rustain-stale.sock\"\n\
         workspace = \"{}\"\n\
         started_at_unix = 1700000000\n\
         profile = \"coding\"\n",
        d.ws.path().display()
    );
    std::fs::write(d.pid_path(), stale).unwrap();

    // start must reclaim it and proceed (no manual cleanup required).
    d.cmd()
        .args(["daemon", "start"])
        .assert()
        .success()
        .stdout(contains("Daemon started"));

    d.cmd().args(["daemon", "stop"]).assert().success();
}

#[test]
fn stop_when_not_running_is_idempotent() {
    let d = dirs();
    d.cmd()
        .args(["daemon", "stop"])
        .assert()
        .success()
        .stdout(contains("Daemon not running"));
}

#[test]
fn nfr47_startup_and_nfr48_shutdown_within_budget() {
    let d = dirs();

    let t0 = Instant::now();
    d.cmd().args(["daemon", "start"]).assert().success();
    let startup = t0.elapsed();
    assert!(
        startup < Duration::from_secs(3),
        "NFR47: startup→ready must be < 3s, took {startup:?}"
    );

    let t1 = Instant::now();
    d.cmd().args(["daemon", "stop"]).assert().success();
    let shutdown = t1.elapsed();
    assert!(
        shutdown < Duration::from_secs(5),
        "NFR48: graceful shutdown must be < 5s, took {shutdown:?}"
    );
}

/// NFR46 — idle RSS < 30MB. The **slow truth gate** of Story 12.2b's two-speed
/// laziness check (AC1b): the in-process FAST gate
/// (`daemon::runtime::tests::runtime_is_lazy_and_built_exactly_once` — build-counter
/// 0/1/1, `OnceCell` unset-then-set) proves the daemon does not eagerly build its
/// turn runtime; THIS gate proves the RSS consequence — an idle daemon that has
/// NEVER been attached/messaged holds no live provider connection and stays under
/// 30MB.
///
/// `#[ignore]`d by default per the AC-12-1a-4 exemption: idle RSS depends on which
/// cargo features are compiled in (default pulls anthropic/openai/ollama/mcp/
/// meta-search) and on the build profile; a debug+all-features build is a known
/// ~39MB outlier. **Evidence = a dated recorded GREEN from the EXISTING 12.1a/12.1d
/// systemd nightly lane on a release build** (per AC1b; never the mere existence of
/// this `#[ignore]`d test — the 12.1b scar). Run with `--ignored` on `--release`.
///
/// Anti-theater (AC1b): the daemon is sampled while genuinely IDLE — the test never
/// attaches or sends a `UserMessage`, so the lazy `OnceCell` is never initialised;
/// and RSS is the PEAK over a sampling window, not a single lucky read.
#[test]
#[ignore = "NFR46 idle RSS<30MB (AC1b slow gate) — feature/profile dependent; release-only on the systemd nightly lane (AC-12-1a-4 exemption)"]
#[cfg(target_os = "linux")]
fn nfr46_idle_rss_under_30mb() {
    let d = dirs();
    d.cmd().args(["daemon", "start"]).assert().success();

    let pid: u32 = std::fs::read_to_string(d.pid_path())
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("pid = "))
        .and_then(|n| n.trim().parse().ok())
        .unwrap();

    let read_rss = || -> u64 {
        std::fs::read_to_string(format!("/proc/{pid}/status"))
            .unwrap()
            .lines()
            .find_map(|l| l.strip_prefix("VmRSS:"))
            .and_then(|r| r.split_whitespace().next())
            .and_then(|n| n.parse().ok())
            .expect("VmRSS readable")
    };

    // Sample the PEAK over a window (10×@200ms) — a single read could miss a
    // transient allocation spike (AC1b "assert max — not a single read").
    let mut peak_kb = 0u64;
    for _ in 0..10 {
        peak_kb = peak_kb.max(read_rss());
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    d.cmd().args(["daemon", "stop"]).assert().success();

    assert!(
        peak_kb < 30 * 1024,
        "NFR46/AC1b: idle (never-activated) daemon peak RSS must be < 30MB, was {:.1}MB",
        peak_kb as f64 / 1024.0
    );
}
