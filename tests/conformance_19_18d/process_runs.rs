//! Real-binary keystones for Story 19.18d (`A17`, `A19`, `A25`, `P16`, `P19`):
//!
//! - `K10-P` (AC3): isolated `daemon start --foreground --serve-a2a=…` runs
//!   pass through `run_inner`; the 19.16d/19.18 legacy disclosure lines count
//!   only UNBOUND keys. `M29` (remove the `run_inner` disclosure call).
//! - `K12` (AC11): the roster reaches BOTH `run` callers and a refusal reaches
//!   stderr. `M23` (drop the daemon `eprintln!`), `M27` (daemon passes an empty
//!   roster), `M47` (standalone passes an empty roster).
//! - `K14` leg (vi) (AC10(f)): the startup auto-authority widening warning for
//!   a pin a `server.peerKeys` entry reaches. `M65` (daemon call site passes an
//!   empty bound-pin set) and `M64`.
//!
//! Every run is a spawned `rustain` binary with its own workspace, `HOME`,
//! data dir and config dir. The daemon log is `{data_dir}/rustain.log*`; the
//! PID file is the daemon readiness marker (written after the A2A listener
//! handshake). Every child is terminated and reaped by a guard, also when an
//! assertion fails.
//!
//! The three legacy disclosure constants and `AUTO_AUTHORITY_WARNING` are
//! private/`pub(crate)` in the library, so their exact text is COPIED below
//! (`adapters/a2a/server.rs` consts; `adapters/daemon/policy_startup.rs`).

#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use assert_cmd::cargo::cargo_bin;
use base64::Engine as _;
use rustain::adapters::a2a::server::PEER_BINDING_LOOPBACK_WARNING;
use rustain::domain::models::{PeerId, PrincipalKind, RoomEvent};
use rustain::infrastructure::subagent::node_journal::NodeJournal;

/// Copied from `adapters/a2a/server.rs` `CREDENTIAL_SCOPED_RETRACT_DISCLOSURE`.
const CREDENTIAL_SCOPED_RETRACT_DISCLOSURE: &str = "A2A item retraction: ownership is \
     credential-scoped, not peer-scoped — every caller presenting the same API key is one \
     principal. An item a rustain sender tagged with its per-item capability can be retracted \
     only with that capability; for items without one, where peers share a key any of them can \
     retract items another sent. Keep `server.admission` at \"deny\" (the default) on a \
     shared-key deployment.";

/// Copied from `adapters/a2a/server.rs` `KEY_ROTATION_ORPHANS_RETRACT_WARNING`.
const KEY_ROTATION_ORPHANS_RETRACT_WARNING: &str = "A2A item retraction: more than one API key \
     is configured, and an item belongs to the key it was sent with — a peer that rotates to \
     another key can no longer retract the items it sent under the old one.";

/// Copied from `adapters/a2a/server.rs` `THREAD_ACCESS_OFF_DISCLOSURE`.
const THREAD_ACCESS_OFF_DISCLOSURE: &str = "A2A threads: an API-key caller is served a thread \
     read or follow-up reference only with the item's capability, which only its sender can \
     derive; items without one (from an older or non-rustain sender) are refused to API-key \
     callers (server.threadReads is off, the default). Loopback callers are served.";

/// Copied from `adapters/daemon/policy_startup.rs` `AUTO_AUTHORITY_WARNING`.
const AUTO_AUTHORITY_WARNING: &str = "server.admission=\"allow\" authorizes inbound \
    execution and at least one notify-and-auto sender lacks journaled or TOML-implied consent; \
    grant that sender, use admission=\"ask\", or lower its effective response mode";

/// Story 19.18d `A10` `R1`/`R2` (exact text).
fn r1(env: &str, alias: &str) -> String {
    format!(
        "A2A server.peerKeys binds {env} to {alias}, but no A2A peer named {alias} is configured"
    )
}

fn r2(env: &str, alias: &str) -> String {
    format!(
        "A2A server.peerKeys binds {env} to {alias}, but {alias} has no valid pinnedKey; a key \
         can be bound only to a pinned peer"
    )
}

// Env-var names this file's children read (per-child `Command::env`; nothing is
// written to this process's environment).
const ENV_BOUND_A: &str = "RUSTAIN_19_18D_PROCESS_RUNS_BOUND_A";
const ENV_UNBOUND_1: &str = "RUSTAIN_19_18D_PROCESS_RUNS_UNBOUND_1";
const ENV_UNBOUND_2: &str = "RUSTAIN_19_18D_PROCESS_RUNS_UNBOUND_2";
const ENV_UNBOUND_3: &str = "RUSTAIN_19_18D_PROCESS_RUNS_UNBOUND_3";
const SHORT_BUDGET: Duration = Duration::from_secs(60);
const LOG_BUDGET: Duration = Duration::from_secs(60);
const INDIVIDUAL_POLICY_FILE: &str = "a2a-interaction.toml";
const LISTENER_BOUND_LOG: &str = "A2A listener bound";

/// RFC 8032 test-vector 1 public key: a valid Ed25519 point, which the daemon's
/// egress composition requires of every pinned roster peer.
const VALID_PIN_KEY: [u8; 32] = [
    0xd7, 0x5a, 0x98, 0x01, 0x82, 0xb1, 0x0a, 0xb7, 0xd5, 0x4b, 0xfe, 0xd3, 0xc9, 0x64, 0x07, 0x3a,
    0x0e, 0xe1, 0x72, 0xf3, 0xda, 0xa6, 0x23, 0x25, 0xaf, 0x02, 0x1a, 0x68, 0xf7, 0x07, 0x51, 0x1a,
];

fn b64(bytes: [u8; 32]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn pin_of(key: [u8; 32]) -> PeerId {
    PeerId::from_jwk_ed25519_x(&b64(key)).expect("32-byte ed25519 x")
}

/// An exclusively owned peer endpoint. Discovery gets a local 404 rather than
/// attempting external egress; the roster still carries real pinned/unpinned
/// entries through the production resolver.
struct PeerFixture {
    address: SocketAddr,
    stopping: Arc<AtomicBool>,
    requests: Receiver<String>,
    worker: Option<JoinHandle<()>>,
}

impl PeerFixture {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind owned roster endpoint");
        let address = listener.local_addr().expect("roster endpoint address");
        let stopping = Arc::new(AtomicBool::new(false));
        let worker_stopping = stopping.clone();
        let (request_tx, requests) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else {
                    break;
                };
                if worker_stopping.load(Ordering::Acquire) {
                    break;
                }
                if let Ok(request) = Self::respond(stream) {
                    let _ = request_tx.send(request);
                }
            }
        });
        Self {
            address,
            stopping,
            requests,
            worker: Some(worker),
        }
    }

    fn respond(mut stream: TcpStream) -> std::io::Result<String> {
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        let mut request = String::new();
        {
            let mut reader = BufReader::new(&mut stream);
            reader.read_line(&mut request)?;
            let mut header = String::new();
            loop {
                header.clear();
                if reader.read_line(&mut header)? == 0 {
                    return Err(std::io::ErrorKind::UnexpectedEof.into());
                }
                if header == "\r\n" {
                    break;
                }
            }
        }
        stream.write_all(
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )?;
        Ok(request)
    }

    fn expect_card_requests(&self, aliases: &[&str]) {
        let mut actual = aliases
            .iter()
            .map(|_| {
                self.requests
                    .recv_timeout(LOG_BUDGET)
                    .expect("child must discover its owned loopback roster endpoint")
            })
            .collect::<Vec<_>>();
        let mut expected = aliases
            .iter()
            .map(|alias| format!("GET /{alias}/a2a/.well-known/agent-card.json HTTP/1.1\r\n"))
            .collect::<Vec<_>>();
        actual.sort();
        expected.sort();
        assert_eq!(
            actual, expected,
            "boot discovery must stay on the local roster fixture"
        );
    }
}

impl Drop for PeerFixture {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        // Wake the blocking accept, then join before releasing the bound socket.
        let _ = TcpStream::connect_timeout(&self.address, Duration::from_secs(1));
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

// ── One isolated run: workspace, HOME, data dir, config dir ─────────────────
struct Run {
    workspace: tempfile::TempDir,
    data_dir: tempfile::TempDir,
    config_dir: tempfile::TempDir,
    home: tempfile::TempDir,
    peers: PeerFixture,
}

impl Run {
    fn new() -> Self {
        Self {
            workspace: tempfile::tempdir().expect("workspace"),
            data_dir: tempfile::tempdir().expect("data dir"),
            config_dir: tempfile::tempdir().expect("config dir"),
            home: tempfile::tempdir().expect("home"),
            peers: PeerFixture::new(),
        }
    }

    fn pinned_agent(&self, key: [u8; 32]) -> serde_json::Value {
        serde_json::json!({
            "url": format!("http://{}/p1/a2a", self.peers.address),
            "pinnedKey": { "alg": "EdDSA", "x": b64(key) },
        })
    }

    fn unpinned_agent(&self) -> serde_json::Value {
        serde_json::json!({ "url": format!("http://{}/u/a2a", self.peers.address) })
    }

    fn write_rustain_file(&self, name: &str, body: &str) {
        let dir = self.workspace.path().join(".rustain");
        std::fs::create_dir_all(&dir).expect("create .rustain");
        std::fs::write(dir.join(name), body).expect("write workspace file");
    }

    fn write_a2a(&self, agents: Option<serde_json::Value>, server: serde_json::Value) {
        let mut doc = serde_json::json!({ "server": server });
        if let Some(agents) = agents {
            doc["agents"] = agents;
        }
        self.write_rustain_file("a2a.json", &doc.to_string());
    }

    /// Spawn `rustain <args>` with every location isolated. Copied from
    /// `conformance_18_3b_policy.rs:122-133` (+ HOME/XDG isolation).
    fn spawn(&self, args: &[String], env: &[(&str, &str)], stderr: Stdio) -> Guard<'_> {
        let mut command = Command::new(cargo_bin("rustain"));
        command
            .args(args)
            .current_dir(self.workspace.path())
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path().join("xdg-config"))
            .env("XDG_DATA_HOME", self.home.path().join("xdg-data"))
            .env("RUSTAIN_DATA_DIR", self.data_dir.path())
            .env("RUSTAIN_CONFIG_DIR", self.config_dir.path())
            .env("RUSTAIN_LOG_PATH", self.data_dir.path().join("rustain.log"))
            .env("RUST_LOG", "rustain=info")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr);
        for (name, value) in env {
            command.env(name, value);
        }
        Guard::new(command.spawn().expect("spawn rustain"), self)
    }

    fn daemon_args(&self, serve: Option<u16>) -> Vec<String> {
        let mut args = vec![
            "daemon".to_owned(),
            "start".to_owned(),
            "--foreground".to_owned(),
        ];
        if let Some(port) = serve {
            args.push(format!("--serve-a2a=127.0.0.1:{port}"));
        }
        args
    }

    fn standalone_args() -> Vec<String> {
        vec!["--serve-a2a=127.0.0.1:0".to_owned()]
    }

    fn log(&self) -> String {
        read_daemon_log(self.data_dir.path())
    }

    fn pid_path(&self) -> PathBuf {
        self.workspace.path().join(".rustain").join("daemon.pid")
    }
}

// ── Child guard: terminate + reap on every exit path ────────────────────────
struct Guard<'run> {
    child: Option<Child>,
    // The child must be terminated and reaped before its peer socket or dirs
    // can be released, including when an assertion unwinds.
    _run: &'run Run,
}

impl<'run> Guard<'run> {
    fn new(child: Child, run: &'run Run) -> Self {
        Self {
            child: Some(child),
            _run: run,
        }
    }

    fn child(&mut self) -> &mut Child {
        self.child.as_mut().expect("child already reaped")
    }

    /// Poll for exit within `budget`; `None` on timeout (the guard still reaps).
    fn wait_exit(&mut self, budget: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + budget;
        loop {
            if let Some(status) = self.child().try_wait().expect("poll child") {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// SIGTERM, bounded wait, then SIGKILL; always reaps.
    fn terminate(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        // SAFETY: plain `kill(2)` on a child pid this guard owns.
        unsafe {
            libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
    }

    fn stderr_after_exit(&mut self) -> String {
        let mut text = String::new();
        if let Some(mut pipe) = self.child().stderr.take() {
            let _ = pipe.read_to_string(&mut text);
        }
        text
    }
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        self.terminate();
    }
}

// ── Log + PID helpers ───────────────────────────────────────────────────────

/// Poll the daemon's log until it contains every needle. Copied from
/// `conformance_18_3b_policy.rs:64-78`.
fn wait_for_log(data_dir: &Path, needles: &[&str], budget: Duration) -> Result<String, String> {
    let deadline = Instant::now() + budget;
    let mut last = String::new();
    while Instant::now() < deadline {
        let text = read_daemon_log(data_dir);
        if needles.iter().all(|needle| text.contains(needle)) {
            return Ok(text);
        }
        if !text.is_empty() {
            last = text;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(last)
}

/// Concatenate every `rustain.log*` file. Copied from
/// `conformance_18_3b_policy.rs:81-100`.
fn read_daemon_log(data_dir: &Path) -> String {
    let Ok(entries) = std::fs::read_dir(data_dir) else {
        return String::new();
    };
    let mut names: Vec<_> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("rustain.log"))
        })
        .collect();
    names.sort();
    names
        .iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Poll for the daemon PID file. Copied from
/// `daemon_crash_recovery.rs:597-616`.
fn wait_for_pid(pid_path: &Path, budget: Duration) -> Option<u32> {
    let deadline = Instant::now() + budget;
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
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Start the daemon and wait for its readiness marker (the PID file, written
/// only after the A2A listener handshake). Fails early with the log when the
/// child exits instead.
fn start_ready_daemon<'run>(
    run: &'run Run,
    serve: Option<u16>,
    env: &[(&str, &str)],
) -> Guard<'run> {
    let mut guard = run.spawn(&run.daemon_args(serve), env, Stdio::null());
    wait_ready_daemon(run, &mut guard);
    guard
}

fn wait_ready_daemon(run: &Run, guard: &mut Guard<'_>) {
    let deadline = Instant::now() + LOG_BUDGET;
    loop {
        if wait_for_pid(&run.pid_path(), Duration::from_millis(200)).is_some() {
            return;
        }
        if let Some(status) = guard.child().try_wait().expect("poll daemon") {
            panic!(
                "daemon exited early ({status}) before publishing its PID:\n{}",
                run.log()
            );
        }
        assert!(
            Instant::now() < deadline,
            "daemon never published its PID:\n{}",
            run.log()
        );
    }
}

/// Discover the kernel-assigned socket from this child's isolated log. The
/// address publication follows the bind safety gates; HTTP remains the
/// standalone readiness assertion.
fn wait_for_listener_addr(run: &Run) -> SocketAddr {
    let log = wait_for_log(run.data_dir.path(), &[LISTENER_BOUND_LOG], LOG_BUDGET)
        .unwrap_or_else(|log| panic!("child never published its bound listener address:\n{log}"));
    let mut publications = log.lines().filter(|line| line.contains(LISTENER_BOUND_LOG));
    let line = publications.next().expect("bound-address log is present");
    assert!(
        publications.next().is_none(),
        "one child must publish exactly one bound listener address:\n{log}"
    );
    let address: SocketAddr = line
        .split_whitespace()
        .find_map(|field| field.strip_prefix("bound_addr="))
        .expect("listener publication must contain bound_addr")
        .parse()
        .expect("listener publication must contain a socket address");
    assert!(address.ip().is_loopback(), "listener must remain loopback");
    assert_ne!(address.port(), 0, "listener must publish its actual port");
    address
}

fn assert_contains(log: &str, needle: &str, what: &str) {
    assert!(log.contains(needle), "{what} missing from the log:\n{log}");
}

fn assert_absent(log: &str, needle: &str, what: &str) {
    assert!(!log.contains(needle), "{what} must not be logged:\n{log}");
}

// ═════════════════════════════════════════════════════════════════════════
// K10-P — disclosures through `run_inner` (AC3, P16; M29)
// ═════════════════════════════════════════════════════════════════════════

/// One foreground-daemon run over `server`; returns the log once the
/// listener-bound loopback warning (a positive control that `run_inner`'s
/// disclosure block ran) is present. The legacy lines are emitted BEFORE it.
fn k10p_run(server: serde_json::Value, env: &[(&str, &str)]) -> String {
    let run = Run::new();
    run.write_a2a(
        Some(serde_json::json!({ "p1": run.pinned_agent(VALID_PIN_KEY) })),
        server,
    );
    let mut daemon = start_ready_daemon(&run, Some(0), env);
    let outcome = wait_for_log(
        run.data_dir.path(),
        &[PEER_BINDING_LOOPBACK_WARNING],
        LOG_BUDGET,
    );
    daemon.terminate();
    outcome.unwrap_or_else(|log| panic!("no loopback binding warning from run_inner:\n{log}"))
}

/// Bound-only, mixed-one-unbound and mixed-two-distinct-unbound configs, each a
/// real `daemon start --foreground --serve-a2a=…` run. `M29` (no disclosure
/// call in `run_inner`) fails the mixed phases' positive assertions.
#[test]
fn k10_p_run_inner_discloses_only_unbound_keys() {
    // Bound-only: none of the three legacy lines; the loopback warning is the
    // positive control (`k10p_run` already required it).
    let bound_only = k10p_run(
        serde_json::json!({ "peerKeys": { ENV_BOUND_A: "p1" } }),
        &[(ENV_BOUND_A, "k10p-bound-a")],
    );
    assert_contains(
        &bound_only,
        PEER_BINDING_LOOPBACK_WARNING,
        "loopback binding warning (bound-only)",
    );
    assert_absent(
        &bound_only,
        CREDENTIAL_SCOPED_RETRACT_DISCLOSURE,
        "credential-scoped retract line (bound-only)",
    );
    assert_absent(
        &bound_only,
        KEY_ROTATION_ORPHANS_RETRACT_WARNING,
        "key-rotation warning (bound-only)",
    );
    assert_absent(
        &bound_only,
        THREAD_ACCESS_OFF_DISCLOSURE,
        "thread posture line (bound-only)",
    );

    // Mixed with ONE unbound key: credential-scoped + thread line, no rotation.
    let one_unbound = k10p_run(
        serde_json::json!({
            "peerKeys": { ENV_BOUND_A: "p1" },
            "apiKeyEnv": ENV_UNBOUND_1,
        }),
        &[
            (ENV_BOUND_A, "k10p-bound-a"),
            (ENV_UNBOUND_1, "k10p-unbound-1"),
        ],
    );
    assert_contains(
        &one_unbound,
        CREDENTIAL_SCOPED_RETRACT_DISCLOSURE,
        "credential-scoped retract line (one unbound)",
    );
    assert_contains(
        &one_unbound,
        THREAD_ACCESS_OFF_DISCLOSURE,
        "thread posture line (one unbound)",
    );
    assert_absent(
        &one_unbound,
        KEY_ROTATION_ORPHANS_RETRACT_WARNING,
        "key-rotation warning (one unbound)",
    );

    // Mixed with TWO distinct unbound values: the rotation warning appears.
    let two_unbound = k10p_run(
        serde_json::json!({
            "peerKeys": { ENV_BOUND_A: "p1" },
            "apiKeyEnv": ENV_UNBOUND_1,
            "apiKeys": [ENV_UNBOUND_2],
        }),
        &[
            (ENV_BOUND_A, "k10p-bound-a"),
            (ENV_UNBOUND_1, "k10p-unbound-1"),
            (ENV_UNBOUND_2, "k10p-unbound-2"),
        ],
    );
    assert_contains(
        &two_unbound,
        CREDENTIAL_SCOPED_RETRACT_DISCLOSURE,
        "credential-scoped retract line (two unbound)",
    );
    assert_contains(
        &two_unbound,
        KEY_ROTATION_ORPHANS_RETRACT_WARNING,
        "key-rotation warning (two distinct unbound)",
    );

    // Two env names holding the SAME unbound value are one distinct key: no
    // rotation warning (control for the "distinct" qualifier).
    let same_value = k10p_run(
        serde_json::json!({
            "peerKeys": { ENV_BOUND_A: "p1" },
            "apiKeyEnv": ENV_UNBOUND_1,
            "apiKeys": [ENV_UNBOUND_3],
        }),
        &[
            (ENV_BOUND_A, "k10p-bound-a"),
            (ENV_UNBOUND_1, "k10p-unbound-shared"),
            (ENV_UNBOUND_3, "k10p-unbound-shared"),
        ],
    );
    assert_contains(
        &same_value,
        CREDENTIAL_SCOPED_RETRACT_DISCLOSURE,
        "credential-scoped retract line (same-value unbound)",
    );
    assert_absent(
        &same_value,
        KEY_ROTATION_ORPHANS_RETRACT_WARNING,
        "key-rotation warning (one distinct unbound value)",
    );
}

// ═════════════════════════════════════════════════════════════════════════
// K12 — roster wiring and visible refusals (AC11, A17, A19, P19; M23/M27/M47)
// ═════════════════════════════════════════════════════════════════════════

fn expect_refusal(run: &Run, args: &[String], expected: &str, what: &str) {
    let mut guard = run.spawn(args, &[(ENV_BOUND_A, "k12-bound-a")], Stdio::piped());
    let status = guard
        .wait_exit(SHORT_BUDGET)
        .unwrap_or_else(|| panic!("{what}: the process kept running instead of refusing"));
    let stderr = guard.stderr_after_exit();
    assert!(!status.success(), "{what}: must exit nonzero, got {status}");
    assert!(
        stderr.contains(expected),
        "{what}: stderr lacks the exact refusal.\nexpected: {expected}\nstderr: {stderr}"
    );
}

/// Unpinned alias + `peerKeys` → exact `R2` on stderr for both roots.
/// `M23` (daemon `eprintln!` dropped) fails the daemon leg.
#[test]
fn k12_an_unpinned_alias_is_refused_with_r2_on_both_roots() {
    let expected = r2(ENV_BOUND_A, "u");
    let server = serde_json::json!({ "peerKeys": { ENV_BOUND_A: "u" } });

    let standalone = Run::new();
    standalone.write_a2a(
        Some(serde_json::json!({ "u": standalone.unpinned_agent() })),
        server.clone(),
    );
    expect_refusal(
        &standalone,
        &Run::standalone_args(),
        &expected,
        "standalone R2",
    );

    let daemon = Run::new();
    daemon.write_a2a(
        Some(serde_json::json!({ "u": daemon.unpinned_agent() })),
        server,
    );
    expect_refusal(
        &daemon,
        &daemon.daemon_args(Some(0)),
        &expected,
        "daemon R2",
    );
}

/// An alias absent from the roster (absent or empty `agents`) → exact `R1` on
/// stderr for both roots. `M27`/`M47` (empty roster forwarded) are caught by
/// the pinned positive controls below; this pins the R1 text itself.
#[test]
fn k12_an_alias_absent_from_the_roster_is_refused_with_r1_on_both_roots() {
    let expected = r1(ENV_BOUND_A, "p1");
    let server = serde_json::json!({ "peerKeys": { ENV_BOUND_A: "p1" } });

    let standalone = Run::new();
    standalone.write_a2a(None, server.clone());
    expect_refusal(
        &standalone,
        &Run::standalone_args(),
        &expected,
        "standalone R1 (absent roster)",
    );

    let daemon = Run::new();
    daemon.write_a2a(Some(serde_json::json!({})), server);
    expect_refusal(
        &daemon,
        &daemon.daemon_args(Some(0)),
        &expected,
        "daemon R1 (empty roster)",
    );
}

/// Positive control, daemon root: a pinned roster + `peerKeys` starts (PID
/// published, process alive) and logs the loopback binding warning. `M27`
/// (daemon forwards an empty roster) turns this into an `R1` refusal.
#[test]
fn k12_a_pinned_roster_starts_the_daemon_root() {
    let run = Run::new();
    run.write_a2a(
        Some(serde_json::json!({ "p1": run.pinned_agent(VALID_PIN_KEY) })),
        serde_json::json!({ "peerKeys": { ENV_BOUND_A: "p1" } }),
    );
    let mut daemon = start_ready_daemon(&run, Some(0), &[(ENV_BOUND_A, "k12-bound-a")]);
    assert!(
        daemon.child().try_wait().expect("poll daemon").is_none(),
        "the daemon must still be alive once ready:\n{}",
        run.log()
    );
    let outcome = wait_for_log(
        run.data_dir.path(),
        &[PEER_BINDING_LOOPBACK_WARNING],
        LOG_BUDGET,
    );
    daemon.terminate();
    outcome.unwrap_or_else(|log| panic!("no loopback binding warning in the daemon log:\n{log}"));
}

/// Fetch `GET path` over plain HTTP/1.1 from a loopback listener; `None` while
/// nothing answers.
fn http_get_status(address: SocketAddr, path: &str) -> Option<String> {
    let mut stream = TcpStream::connect(address).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut text = String::new();
    stream.read_to_string(&mut text).ok()?;
    text.lines().next().map(str::to_owned)
}

/// Positive control, standalone root: the log locates the kernel-assigned
/// address, but readiness still requires the real served AgentCard. `M47`
/// (standalone forwards an empty roster) turns this into an `R1` refusal.
#[test]
fn k12_a_pinned_roster_starts_the_standalone_root() {
    let run = Run::new();
    run.write_a2a(
        Some(serde_json::json!({ "p1": run.pinned_agent(VALID_PIN_KEY) })),
        serde_json::json!({ "peerKeys": { ENV_BOUND_A: "p1" } }),
    );
    let mut server = run.spawn(
        &Run::standalone_args(),
        &[(ENV_BOUND_A, "k12-bound-a")],
        Stdio::piped(),
    );
    let address = wait_for_listener_addr(&run);
    let deadline = Instant::now() + LOG_BUDGET;
    let status_line = loop {
        if let Some(line) = http_get_status(address, "/.well-known/agent-card.json") {
            break line;
        }
        if let Some(status) = server.child().try_wait().expect("poll standalone") {
            let stderr = server.stderr_after_exit();
            panic!("standalone exited early ({status}); stderr: {stderr}");
        }
        assert!(
            Instant::now() < deadline,
            "the standalone server never served its agent card"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(
        server
            .child()
            .try_wait()
            .expect("poll standalone")
            .is_none(),
        "the standalone server must still be alive"
    );
    server.terminate();
    assert!(
        status_line.contains(" 200"),
        "agent card fetch must succeed, got `{status_line}`"
    );
}

/// Both real children start before either readiness wait. Their listener ports
/// stay allocated by the children, and both pinned and unpinned boot discovery
/// requests are observed on each run's still-owned loopback peer fixture.
#[test]
fn child_daemons_bind_distinct_kernel_assigned_ports_concurrently() {
    let first = Run::new();
    let second = Run::new();
    for run in [&first, &second] {
        run.write_a2a(
            Some(serde_json::json!({
                "p1": run.pinned_agent(VALID_PIN_KEY),
                "u": run.unpinned_agent(),
            })),
            serde_json::json!({ "peerKeys": { ENV_BOUND_A: "p1" } }),
        );
    }
    let env = [(ENV_BOUND_A, "concurrent-bound-a")];
    let mut first_child = first.spawn(&first.daemon_args(Some(0)), &env, Stdio::null());
    let mut second_child = second.spawn(&second.daemon_args(Some(0)), &env, Stdio::null());
    wait_ready_daemon(&first, &mut first_child);
    wait_ready_daemon(&second, &mut second_child);
    let first_address = wait_for_listener_addr(&first);
    let second_address = wait_for_listener_addr(&second);
    assert_ne!(
        first_address, second_address,
        "live children must own distinct loopback sockets"
    );
    for (run, child, address) in [
        (&first, &mut first_child, first_address),
        (&second, &mut second_child, second_address),
    ] {
        assert!(
            child.child().try_wait().expect("poll child").is_none(),
            "child must remain alive after its PID readiness marker"
        );
        assert_contains(
            &run.log(),
            PEER_BINDING_LOOPBACK_WARNING,
            "loopback binding warning (concurrent child)",
        );
        run.peers.expect_card_requests(&["p1", "u"]);
        let status = http_get_status(address, "/.well-known/agent-card.json")
            .expect("real child must serve its AgentCard");
        assert!(status.contains(" 200"), "AgentCard fetch failed: {status}");
    }
    first_child.terminate();
    second_child.terminate();
}

/// A genuinely occupied socket must fail the production bind handshake, not
/// publish a PID or a successful listener address.
#[test]
fn an_occupied_a2a_address_refuses_daemon_readiness_without_a_pid() {
    let occupied = TcpListener::bind("127.0.0.1:0").expect("hold occupied address");
    let address = occupied.local_addr().expect("occupied address");
    let run = Run::new();
    run.write_a2a(
        Some(serde_json::json!({ "p1": run.pinned_agent(VALID_PIN_KEY) })),
        serde_json::json!({ "peerKeys": { ENV_BOUND_A: "p1" } }),
    );
    let mut child = run.spawn(
        &run.daemon_args(Some(address.port())),
        &[(ENV_BOUND_A, "occupied-bound-a")],
        Stdio::piped(),
    );
    let status = child
        .wait_exit(SHORT_BUDGET)
        .expect("occupied listener must cause daemon startup to exit");
    let stderr = child.stderr_after_exit();
    let bind_error = std::io::Error::from_raw_os_error(libc::EADDRINUSE).to_string();
    assert!(!status.success(), "occupied-address startup must fail");
    assert!(
        stderr.contains("A2A listener failed to start") && stderr.contains(&bind_error),
        "the real bind failure must reach stderr: {stderr}"
    );
    assert!(
        !run.pid_path().exists(),
        "failed startup must not publish a PID"
    );
    assert_absent(
        &run.log(),
        LISTENER_BOUND_LOG,
        "successful bound-address publication (occupied address)",
    );
    child.terminate();
    drop(occupied);
}

// ═════════════════════════════════════════════════════════════════════════
// K14 (vi) — startup widening for bound callers (AC10(f), A25; M65, M64)
// ═════════════════════════════════════════════════════════════════════════

/// Journal one `ConsentGranted` row for `sender` with the production journal
/// writer (`NodeJournal::append_room`), before the daemon starts. `None` is a
/// LEGACY row (no `principal` field on the wire).
fn plant_consent(run: &Run, sender: &PeerId, principal: Option<PrincipalKind>) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(async {
        let journal = NodeJournal::open_workspace(run.workspace.path())
            .await
            .expect("open the workspace journal");
        journal
            .append_room(RoomEvent::ConsentGranted {
                sender: Some(sender.clone()),
                granted_at: 1,
                principal,
            })
            .await
            .expect("journal the consent grant");
    });
}

struct WideningCase {
    peer_keys: bool,
    grant: Option<Option<PrincipalKind>>,
    p1_override: bool,
}

/// One real `daemon start --foreground` over a written workspace (admission
/// `allow`, default `notify-and-auto`, roster pin `p1`). No listener is needed:
/// `run_daemon_foreground` reports the widening before `spawn_a2a_listener`.
/// Returns the log once startup has passed the report (the later
/// "durable node recovery complete" line).
fn widening_log(case: &WideningCase) -> String {
    let key = VALID_PIN_KEY;
    let run = Run::new();
    let mut server = serde_json::json!({ "admission": "allow" });
    if case.peer_keys {
        server["peerKeys"] = serde_json::json!({ ENV_BOUND_A: "p1" });
    }
    run.write_a2a(
        Some(serde_json::json!({ "p1": run.pinned_agent(key) })),
        server,
    );
    let mut policy = String::from("[interaction.defaults]\nresponse_mode = \"notify-and-auto\"\n");
    if case.p1_override {
        policy.push_str("[interaction.overrides.\"p1\"]\nresponse_mode = \"notify-and-auto\"\n");
    }
    run.write_rustain_file(INDIVIDUAL_POLICY_FILE, &policy);
    if let Some(principal) = case.grant {
        plant_consent(&run, &pin_of(key), principal);
    }
    let mut daemon = run.spawn(
        &run.daemon_args(None),
        &[(ENV_BOUND_A, "k14-bound-a")],
        Stdio::null(),
    );
    let outcome = wait_for_log(
        run.data_dir.path(),
        &[
            "interaction policy resolved",
            "durable node recovery complete",
        ],
        LOG_BUDGET,
    );
    daemon.terminate();
    outcome.unwrap_or_else(|log| panic!("the daemon never finished startup:\n{log}"))
}

/// `K14` (vi): a legacy-only grant no longer silences the warning for a pin a
/// `peerKeys` entry reaches; a typed pinned grant, no binding, or a TOML
/// override does. `M65` (empty bound-pin set at the daemon call site) and
/// `M64` (`bound_pins` ignored) turn the main phase RED.
#[test]
fn k14_vi_the_widening_warning_follows_the_bound_rail() {
    let main = widening_log(&WideningCase {
        peer_keys: true,
        grant: Some(None),
        p1_override: false,
    });
    assert_contains(
        &main,
        AUTO_AUTHORITY_WARNING,
        "auto-authority warning (legacy grant + peerKeys)",
    );

    let typed = widening_log(&WideningCase {
        peer_keys: true,
        grant: Some(Some(PrincipalKind::Pinned)),
        p1_override: false,
    });
    assert_absent(
        &typed,
        AUTO_AUTHORITY_WARNING,
        "auto-authority warning (typed pinned grant)",
    );

    let unbound = widening_log(&WideningCase {
        peer_keys: false,
        grant: Some(None),
        p1_override: false,
    });
    assert_absent(
        &unbound,
        AUTO_AUTHORITY_WARNING,
        "auto-authority warning (legacy grant, no peerKeys)",
    );

    let overridden = widening_log(&WideningCase {
        peer_keys: true,
        grant: Some(None),
        p1_override: true,
    });
    assert_absent(
        &overridden,
        AUTO_AUTHORITY_WARNING,
        "auto-authority warning (legacy grant + peerKeys + override)",
    );
}
