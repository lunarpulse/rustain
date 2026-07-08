//! Story 14-7 AC4 — ACP subprocess socket-probe conformance (Linux-only).
//!
//! The in-memory duplex transcript tests in `conformance_acp.rs` (Section 3)
//! prove the *wire* contract, but they structurally CANNOT catch the contract
//! this test defends: **`rustain acp` must never bind a LISTEN socket.**
//! Those tests inject a `tokio::io::duplex` transport straight into
//! `serve_acp_with_core_factory`, so even if the adapter called
//! `TcpListener::bind` / `UnixListener::bind` on the startup or turn path, no
//! socket would ever be created — the test would stay green while a real
//! editor client got a phantom listener bound on its machine.
//!
//! This test closes that gap the only way that can: it spawns the REAL
//! `rustain acp` binary as an OS process, drives genuine newline-delimited
//! JSON-RPC over its actual stdin/stdout through
//! `initialize → session/new → session/prompt`, and probes `/proc/<pid>` to
//! assert the process owns ZERO listening sockets. A mutant that adds a
//! listener bind anywhere on that path reddens the probe; the in-memory
//! duplex tests do not.
//!
//! ## Why BOTH `/proc/<pid>/fd` AND `/proc/<pid>/net/*`
//!
//! `/proc/<pid>/net/{tcp,...}` lists every socket in the process's *network
//! namespace* — on a normal Linux host that includes every other listener on
//! the box (sshd, docker, a dev server, …), not just this process's. Reading
//! a net table alone would false-positive against the host's own listeners.
//! We therefore:
//!   1. read `/proc/<pid>/fd/*` to collect the inodes of the sockets the
//!      process *itself* holds open (`read_link` → `socket:[INODE]`), then
//!   2. intersect that set with the listening entries in the net tables.
//!      Only a socket that is BOTH in a listening/receiving state AND owned by the
//!      pid counts as a violation. The host's listener set cannot perturb the test.
//!
//! ## Two probes
//!
//! Binding could be eager (during `initialize`/handshake) or lazy (during
//! `session/new`, where `build_cli_core` runs, or during `session/prompt`,
//! where `run_turn` + the tool scheduler + provider are built). We probe at
//! three distinct protocol stages — after `initialize`, after `session/new`,
//! and after `session/prompt` — so both timing classes are covered.
//!
//! Linux-only: `/proc` is the substrate. The whole file is cfg-excluded
//! elsewhere, so the cross-platform suite is untouched.

#![cfg(target_os = "linux")]

use std::collections::HashSet;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdout, Command};
use tokio::sync::Mutex;

/// Golden initialize request (JSON-RPC, protocol version 1 = ACP LATEST).
/// Mirrors `conformance_acp.rs` so both tests agree on the wire contract.
const ACP_INITIALIZE_REQ: &str = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":1,\"clientCapabilities\":{}}}";

const ACP_PROMPT_TEXT: &str = "probe";

/// Append a trailing newline so the newline-delimited JSON-RPC framer reads it.
fn line(mut s: String) -> String {
    if !s.ends_with('\n') {
        s.push('\n');
    }
    s
}

fn session_new_req(id: u64, cwd: &std::path::Path) -> String {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "session/new",
        "params": { "cwd": cwd, "mcpServers": [] },
    });
    serde_json::to_string(&body).expect("session/new serializes")
}

fn prompt_req(id: u64, session_id: &str) -> String {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "session/prompt",
        "params": {
            "sessionId": session_id,
            "prompt": [ { "type": "text", "text": ACP_PROMPT_TEXT } ],
        }
    });
    serde_json::to_string(&body).expect("prompt serializes")
}

// ─────────────────────────────────────────────────────────────────────
// /proc probe helpers
// ─────────────────────────────────────────────────────────────────────

/// Inodes of sockets the process holds open, read from `/proc/<pid>/fd/*`.
/// `read_link` on each fd yields `socket:[INODE]`, `pipe:[INODE]`,
/// `anon_inode:[…]`, `/dev/…`, etc.; we keep only the `socket:` inodes — the
/// only fds that count toward the ownership attribution below.
fn owned_socket_inodes(pid: u32) -> HashSet<u64> {
    let mut inodes = HashSet::new();
    let entries = match std::fs::read_dir(format!("/proc/{pid}/fd")) {
        Ok(e) => e,
        // Process gone → its fd table is unreadable → it owns nothing.
        Err(_) => return inodes,
    };
    for entry in entries.flatten() {
        if let Ok(target) = std::fs::read_link(entry.path()) {
            let s = target.to_string_lossy();
            if let Some(rest) = s.strip_prefix("socket:[") {
                if let Some(end) = rest.find(']') {
                    if let Ok(n) = rest[..end].parse::<u64>() {
                        inodes.insert(n);
                    }
                }
            }
        }
    }
    inodes
}

/// Every LISTEN socket owned by `pid`, across tcp/tcp6/udp/udp6/unix.
///
/// A socket is reported only if it is (a) in a listening/receiving state per
/// its net table AND (b) its inode appears in the process's own fd table. The
/// intersection is what makes the check immune to unrelated host listeners
/// that share the network namespace.
fn owned_listeners(pid: u32) -> Vec<String> {
    let owned = owned_socket_inodes(pid);
    let mut found = Vec::new();

    // TCP / TCP6 — the `st` column == "0A" (TCP_LISTEN = 10). Rock-solid.
    for fam in ["tcp", "tcp6"] {
        let body = std::fs::read_to_string(format!("/proc/{pid}/net/{fam}")).unwrap_or_default();
        for line in body.lines().skip(1) {
            let c: Vec<&str> = line.split_whitespace().collect();
            // sl local_address rem_address st tx:rx tr:tm retrnsmt uid timeout inode ...
            if c.len() < 10 || c[3] != "0A" {
                continue;
            }
            if let Ok(inode) = c[9].parse::<u64>()
                && owned.contains(&inode)
            {
                found.push(format!("{fam} LISTEN {} inode={inode}", c[1]));
            }
        }
    }

    // UDP / UDP6 — connectionless, so "listen" == bound to a non-ephemeral-zero
    // local port AND not connect()ed to a peer (remote ends in `:0000`). A
    // connect()ed UDP socket (e.g. an outbound DNS resolver) is excluded; a
    // bound-only receiver is the only UDP shape that can take uninvited input.
    for fam in ["udp", "udp6"] {
        let body = std::fs::read_to_string(format!("/proc/{pid}/net/{fam}")).unwrap_or_default();
        for line in body.lines().skip(1) {
            let c: Vec<&str> = line.split_whitespace().collect();
            if c.len() < 10 {
                continue;
            }
            let local_port = c[1].rsplit(':').next().unwrap_or("0000");
            if local_port == "0000" || !c[2].ends_with(":0000") {
                continue;
            }
            if let Ok(inode) = c[9].parse::<u64>()
                && owned.contains(&inode)
            {
                found.push(format!("{fam} bound {} inode={inode}", c[1]));
            }
        }
    }

    // Unix — a bound Path is the reliable cross-kernel signal of a listener:
    // the kernel only records a Path for a socket that has called bind(). The
    // daemon's `UnixListener::bind` shows up here. Anonymous socketpair
    // sockets (tokio/stdlib internal plumbing, eventfd, etc.) carry NO path
    // and are correctly ignored — they are not listeners.
    let body = std::fs::read_to_string(format!("/proc/{pid}/net/unix")).unwrap_or_default();
    for line in body.lines().skip(1) {
        let c: Vec<&str> = line.split_whitespace().collect();
        // Num RefCnt Protocol Flags Type St Inode [Path...]
        if c.len() < 7 {
            continue;
        }
        let has_path = c.len() > 7 && !c[7].is_empty();
        if !has_path {
            continue;
        }
        if let Ok(inode) = c[6].parse::<u64>()
            && owned.contains(&inode)
        {
            let path = c[7..].join(" ");
            found.push(format!("unix LISTEN {path} inode={inode}"));
        }
    }

    found
}

/// Assert the process is still alive right before a probe. A dead process
/// makes `/proc/<pid>/fd` unreadable → `owned_socket_inodes` returns the empty
/// set → the listener check would vacuously pass. Catching that here turns a
/// premature exit into a loud, diagnosable failure instead of a false green.
async fn assert_alive(pid: u32, stage: &str, stderr: &Arc<Mutex<Vec<u8>>>) {
    if std::fs::metadata(format!("/proc/{pid}")).is_ok() {
        return;
    }
    let stderr = String::from_utf8_lossy(&stderr.lock().await).into_owned();
    panic!(
        "ACP subprocess (pid {pid}) exited before the probe `{stage}` could run — \
         cannot assert socket posture against a dead process.\n--- stderr ---\n{stderr}"
    );
}

/// The gate. Asserts the process owns zero listening sockets at `stage`.
/// This reads live kernel state, NOT source text — a bind anywhere on the
/// served path reddens it.
fn assert_no_owned_listeners(pid: u32, stage: &str) {
    let offenders = owned_listeners(pid);
    assert!(
        offenders.is_empty(),
        "AC4 VIOLATION at `{stage}`: `rustain acp` (pid {pid}) owns LISTEN socket(s): \
         [{}]\nAn ACP stdio server must never bind a listener — it serves over the \
         stdin/stdout pair the editor gave it.",
        offenders.join(", ")
    );
}

// ─────────────────────────────────────────────────────────────────────
// JSON-RPC driver
// ─────────────────────────────────────────────────────────────────────

/// Read newline-delimited JSON-RPC lines until one whose `id` == `want` is
/// seen, bounded by `timeout`. Notifications (no matching id) and unparseable
/// lines are skipped. Returns `None` on timeout or EOF.
async fn next_response_for_id(
    lines: &mut tokio::io::Lines<BufReader<ChildStdout>>,
    want: u64,
    timeout: Duration,
) -> Option<serde_json::Value> {
    let want = serde_json::json!(want);
    loop {
        match tokio::time::timeout(timeout, lines.next_line()).await {
            Err(_) => return None,                    // timed out
            Ok(Err(_)) | Ok(Ok(None)) => return None, // read error / EOF
            Ok(Ok(Some(raw))) => match serde_json::from_str::<serde_json::Value>(&raw) {
                Ok(v) if v.get("id") == Some(&want) => return Some(v),
                _ => continue, // notification or a different request id
            },
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// AC4 — the subprocess socket probe
// ─────────────────────────────────────────────────────────────────────

/// Spawning the real `rustain acp` binary, driving real JSON-RPC over its
/// stdin/stdout through `initialize → session/new → session/prompt`, and
/// probing `/proc/<pid>` at each stage yields ZERO process-owned LISTEN
/// sockets.
///
/// Defends the AC4 "no listener" contract end to end — the one guarantee the
/// in-memory duplex transcript tests cannot reach, because they inject the
/// transport and so would never create a real socket even if the adapter bound
/// one.
///
/// Hermetic + deterministic:
/// * Provider credentials are stripped from the child env, so `build_cli_core`
///   registers NO provider and `session/prompt` fast-fails locally (stopReason
///   `refusal`) instead of making a live network call. The turn completes, the
///   process stays alive, and the probe runs against a live server.
/// * The workspace / data / config dirs are temp; `RUSTAIN_DATA_DIR` and
///   `RUSTAIN_CONFIG_DIR` are set on the child only (never the test process's
///   global env), so parallel runs never collide.
/// * Every step is timeout-bounded; the child is `kill_on_drop`, so a failed
///   assertion cannot leak a process.
#[tokio::test]
async fn acp_subprocess_binds_no_listen_socket() {
    let bin = env!("CARGO_BIN_EXE_rustain");
    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let data = tempfile::tempdir().expect("data tempdir");
    let cfg = tempfile::tempdir().expect("config tempdir");

    let mut cmd = Command::new(bin);
    cmd.arg("acp")
        .current_dir(workspace.path())
        // Isolation dirs live on the CHILD env only — parallel-safe.
        .env("RUSTAIN_DATA_DIR", data.path())
        .env("RUSTAIN_CONFIG_DIR", cfg.path())
        .env("RUSTAIN_LOG", "error")
        // Strip inherited provider credentials so build_cli_core registers no
        // provider and session/prompt fast-fails instead of a live API call.
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_AUTH_TOKEN")
        .env_remove("OPENAI_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // A failed assertion unwinds and drops the Child → SIGKILL. No leak.
        .kill_on_drop(true);

    let mut child = cmd.spawn().expect("spawn `rustain acp`");
    let pid = child.id().expect("child pid");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let stderr = child.stderr.take().expect("stderr");

    // Drain stderr into a buffer for failure diagnostics. The binary's tracing
    // output must never reach stdout — stdout is the JSON-RPC transport — so
    // this also keeps the stream clean. tokio::sync::Mutex because the buffer
    // is async coordination between this task and the test (no guard crosses
    // an .await).
    let stderr_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let sb = stderr_buf.clone();
    tokio::spawn(async move {
        let mut stderr = stderr;
        let mut buf = [0u8; 4096];
        loop {
            match stderr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut g = sb.lock().await;
                    g.extend_from_slice(&buf[..n]);
                }
            }
        }
    });

    let stderr_snapshot = || async {
        let g = stderr_buf.lock().await;
        String::from_utf8_lossy(&g).into_owned()
    };

    let mut lines = BufReader::new(stdout).lines();

    // ── initialize ──────────────────────────────────────────────────────
    stdin
        .write_all(line(ACP_INITIALIZE_REQ.to_string()).as_bytes())
        .await
        .expect("write initialize");
    stdin.flush().await.expect("flush initialize");

    let init = match next_response_for_id(&mut lines, 1, Duration::from_secs(20)).await {
        Some(v) => v,
        None => {
            let stderr = stderr_snapshot().await;
            panic!(
                "no initialize response within 20s — the real ACP server never answered\n\
                 --- stderr ---\n{stderr}"
            );
        }
    };
    assert_eq!(
        init["result"]["protocolVersion"], 1,
        "initialize must advertise protocolVersion 1 (proves a real JSON-RPC round-trip, \
         not merely a live process)"
    );
    assert_eq!(
        init["result"]["agentInfo"]["name"], "rustain",
        "initialize must identify the agent as `rustain`"
    );
    // Probe #1 — eager binding during init/handshake.
    assert_alive(pid, "after initialize", &stderr_buf).await;
    assert_no_owned_listeners(pid, "after initialize");

    // ── session/new ─────────────────────────────────────────────────────
    stdin
        .write_all(line(session_new_req(2, workspace.path())).as_bytes())
        .await
        .expect("write session/new");
    stdin.flush().await.expect("flush session/new");

    let new = match next_response_for_id(&mut lines, 2, Duration::from_secs(15)).await {
        Some(v) => v,
        None => {
            let stderr = stderr_snapshot().await;
            panic!("no session/new response within 15s\n--- stderr ---\n{stderr}");
        }
    };
    let session_id = new["result"]["sessionId"]
        .as_str()
        .expect("session/new must return a sessionId string")
        .to_string();
    // DD-2: production mints `acp-{conversation_id}` (unique nanoid). The exact
    // id is non-deterministic across the real binary — assert the ACP prefix
    // (this test's purpose is the no-listener probe, not the id scheme).
    assert!(
        session_id.starts_with("acp-"),
        "first session must be minted with the `acp-` prefix (DD-2); got `{session_id}`"
    );
    // Probe #2 — lazy binding during session creation (build_cli_core runs
    // inside new_session, so the provider/tools/storage layer is composed here).
    assert_alive(pid, "after session/new", &stderr_buf).await;
    assert_no_owned_listeners(pid, "after session/new");

    // ── session/prompt (best-effort; widens the detection window into the
    //    turn path: run_turn + tool scheduler + provider are built here) ──
    let _ = stdin
        .write_all(line(prompt_req(3, &session_id)).as_bytes())
        .await;
    let _ = stdin.flush().await;
    // With no provider registered the turn fast-fails (stopReason `refusal`);
    // we do NOT require success — we only need the server still alive so the
    // probe is meaningful. A real listener bind on the turn path still reddens.
    let _ = next_response_for_id(&mut lines, 3, Duration::from_secs(20)).await;
    assert_alive(pid, "after session/prompt", &stderr_buf).await;
    assert_no_owned_listeners(pid, "after session/prompt");

    // Graceful teardown: close stdin so the server sees EOF and exits, then
    // reap within a budget. If it won't quit, kill_on_drop SIGKILLs on return.
    drop(stdin);
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
}
// ─────────────────────────────────────────────────────────────────────
// AC4 mutant-mkillers — M1 (break serve loop) + M2 (inject listener)
// ─────────────────────────────────────────────────────────────────────
//
// The two mutants AC4 demands both die are, by construction, killed by the
// SAME mechanisms `acp_subprocess_binds_no_listen_socket` already exercises
// against the REAL binary:
//   * M1 "server does nothing" (break the serve loop) → the handshake never
//     completes → the bounded `next_response_for_id` await returns `None`
//     instead of a result → the main test panics ("no initialize response").
//   * M2 "quiet debug bind" (inject a listener in adapter init) → the /proc
//     probe observes a LISTEN fd owned by the child → `assert_no_owned_listeners`
//     panics.
//
// We cannot mutate the real binary at test time, so these two tests prove the
// KILLER MECHANISMS themselves are armed and correct — the positive controls
// that make the main test's negative assertions non-vacuous. Each documents
// exactly which mutant it backs.

/// M1 killer-mechanism proof: a server that never answers the handshake is
/// caught by the bounded `next_response_for_id` await — it returns `None`
/// within the window rather than hanging the test.
///
/// We spawn the REAL `rustain acp` binary and send NOTHING. A correctly
/// serving server blocks on stdin (so no initialize arrives) and the bounded
/// read times out. The positive control is
/// `acp_subprocess_binds_no_listen_socket`, where a real handshake DOES
/// complete over the same transport — so an M1 mutant (a broken serve loop
/// that never reads stdin / never dispatches) reddens THAT test through this
/// identical bounded await. Without this test a regression that made the
/// await unbounded (e.g. dropping the `tokio::time::timeout` wrapper) would
/// let M1 hang the suite instead of failing it.
///
/// Non-vacuity: the assertion is not merely "no response" (true of any quiet
/// pipe) — it also checks the bounded await actually WAITED for the window
/// (elapsed >= ~1.5s), proving the timeout is in the loop and not
/// short-circuited.
#[tokio::test]
async fn acp_subprocess_handshake_is_bounded_against_a_non_serving_binary() {
    let bin = env!("CARGO_BIN_EXE_rustain");
    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let data = tempfile::tempdir().expect("data tempdir");
    let cfg = tempfile::tempdir().expect("config tempdir");

    let mut cmd = Command::new(bin);
    cmd.arg("acp")
        .current_dir(workspace.path())
        .env("RUSTAIN_DATA_DIR", data.path())
        .env("RUSTAIN_CONFIG_DIR", cfg.path())
        .env("RUSTAIN_LOG", "error")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_AUTH_TOKEN")
        .env_remove("OPENAI_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd.spawn().expect("spawn `rustain acp`");
    // Intentionally do NOT take stdin: the child must stay blocked on its
    // stdin read so stdout stays open-but-empty for the whole window. Taking
    // and dropping stdin would EOF the child and make next_line return at once.
    let stdout = child.stdout.take().expect("stdout");
    let mut lines = BufReader::new(stdout).lines();

    let window = Duration::from_secs(2);
    let started = std::time::Instant::now();
    let resp = next_response_for_id(&mut lines, 1, window).await;
    let elapsed = started.elapsed();

    assert!(
        resp.is_none(),
        "no initialize response should arrive when nothing is sent, but got: {resp:?}"
    );
    assert!(
        elapsed >= Duration::from_millis(1500),
        "the bounded await must actually wait the timeout window (elapsed {elapsed:?}), not \
         short-circuit to None — otherwise it cannot tell a slow-but-serving server from a \
         dead (M1) one"
    );

    // kill_on_drop reaps the still-blocked child on return.
    drop(lines);
    drop(child);
}

/// M2 killer-mechanism proof: the `/proc` socket probe DETECTS a real LISTEN
/// socket owned by a process — the positive control that makes the main test's
/// "zero LISTEN sockets" assertion able to catch an injected adapter listener.
///
/// We bind a real `TcpListener` IN THIS TEST PROCESS and assert
/// `owned_listeners(<our pid>)` reports it (the count rises). The negative
/// assertion lives in `acp_subprocess_binds_no_listen_socket` (the real binary
/// owns zero); together they prove an M2 mutant (a listener bound on the ACP
/// init/turn path) would redden the main test through this exact probe. A
/// regression that made `owned_listeners` always return empty (e.g. a broken
/// fd/net-table intersection) would silently disable M2 detection — this test
/// catches that.
///
/// Non-vacuity: the assertion is `after > before` (the probe noticed the
/// NEWLY bound socket), not merely `after >= 1`, so a baseline of unrelated
/// listeners cannot mask a broken probe.
#[tokio::test]
async fn proc_probe_detects_a_real_listen_socket() {
    let pid = std::process::id();
    let before = owned_listeners(pid).len();

    // Bind a real TCP listener on an ephemeral port and hold it across the
    // probe so the LISTEN fd stays open and owned by this process.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral TcpListener for M2 probe");

    let after = owned_listeners(pid).len();
    assert!(
        after > before,
        "the /proc socket probe MUST detect the LISTEN socket this test just bound \
         (before={before}, after={after}) — otherwise M2 (an injected adapter listener) \
         could never be caught by `assert_no_owned_listeners`"
    );

    drop(listener);
}
