//! Story C-2 AC4/AC5 — every surface carries the `[persona] config = { file }`
//! body on the wire, and a missing file fails closed, through the real `rustain`
//! binary (daemon, `ask`, ACP). A local Anthropic stub records the request
//! bodies; the child runs with a cleared environment (`common::daemon_proc`).
//!
//! The profile is written as `[persona] adapter = "coding", config = { file =
//! "p.md" }`, which the base binary accepts and ignores: at the base these tests
//! go RED because the recorded `system` lacks the body and a missing file is not
//! an error, not because of any setup failure.

#![cfg(unix)]

use std::time::Duration;

use crate::common::daemon_proc::{
    AcpProc, DEFAULT_REPLY, Env, PlainAttach, Settled, Stub, profile_toml, spawn_daemon_ready,
    system_text,
};

const START: Duration = Duration::from_secs(60);
const TURN: Duration = Duration::from_secs(60);
const EXIT: Duration = Duration::from_secs(30);

const DAEMON_BODY: &str = "C2-DAEMON-BODY-1";
const ASK_BODY: &str = "C2-ASK-BODY-1";
const ACP_BODY: &str = "C2-ACP-BODY-1";

/// Stub + env with profile `p` referencing `personas/p.md`; the file is written
/// only when `body` is `Some`.
async fn fixture(body: Option<&str>) -> (Stub, Env) {
    let stub = Stub::start().await;
    let env = Env::new(&stub);
    env.write_profile("p", &profile_toml("p", "coding", Some("p.md")));
    if let Some(body) = body {
        env.write_persona("p.md", body);
    }
    (stub, env)
}

fn assert_starts_with_body(request: &serde_json::Value, body: &str) {
    let system = system_text(request);
    assert!(
        system.starts_with(body),
        "system prompt must start with the persona body {body:?}; got {system:?}"
    );
}

// ───────────────────────────── (a) daemon ─────────────────────────────

/// Start the daemon, optionally delete the persona file once it is ready, then
/// attach and send one turn; the model request must still start with the body.
async fn daemon_turn_carries_body(delete_file_after_ready: bool) {
    let (stub, env) = fixture(Some(DAEMON_BODY)).await;
    let mut daemon = spawn_daemon_ready(&env, "p", START).await;
    if delete_file_after_ready {
        std::fs::remove_file(env.persona_path("p.md")).expect("delete persona file");
    }

    let mut attach = PlainAttach::spawn(&env);
    attach.send_line("C2 daemon probe");
    let requests = stub.wait_for_requests(1, TURN).await;
    assert_eq!(requests.len(), 1, "exactly one model turn");
    assert_starts_with_body(&requests[0], DAEMON_BODY);
    assert!(
        requests[0].to_string().contains("C2 daemon probe"),
        "the request is the attached line's turn: {}",
        requests[0]
    );

    attach.close_stdin();
    let attach_exit = attach.proc.wait_exit(EXIT).await;
    assert!(attach_exit.success(), "{}", attach.proc.diagnostics());

    let stopped = env.daemon_stop().await;
    assert_eq!(stopped.code(), Some(0), "{stopped:?}");
    // `daemon stop` ends the foreground daemon; only the exit itself is awaited.
    daemon.proc.wait_exit(EXIT).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_turn_system_prompt_starts_with_the_persona_file_body() {
    daemon_turn_carries_body(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_reads_the_persona_file_once_at_start() {
    daemon_turn_carries_body(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_start_fails_closed_when_the_persona_file_is_missing() {
    let (stub, env) = fixture(None).await;
    let mut daemon = env.spawn_daemon_foreground("p");
    let status = match daemon.settle(&env, START).await {
        Settled::Exited(status) => status,
        Settled::Ready => panic!("daemon became ready although personas/p.md is missing"),
    };
    let stderr = {
        let out = daemon.proc.stdout.clone();
        let err = daemon.proc.stderr.clone();
        crate::common::daemon_proc::poll_until(EXIT, "daemon pipes to reach EOF", || {
            (out.is_closed() && err.is_closed()).then_some(())
        })
        .await;
        err.text()
    };
    assert_eq!(status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("composing daemon core:"), "{stderr}");
    assert!(
        stderr.contains(&env.persona_path("p.md").display().to_string()),
        "stderr must name the persona path: {stderr}"
    );
    assert!(
        stub.requests().is_empty(),
        "a refused start sends no request"
    );
}

// ───────────────────────────── (b) ask ─────────────────────────────

const ASK_TAIL: [&str; 4] = ["ask", "--output-format", "json", "C2 ask probe"];

fn ask_args<'a>(global: &[&'a str]) -> Vec<&'a str> {
    let mut args = global.to_vec();
    args.extend_from_slice(&ASK_TAIL);
    args
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ask_system_prompt_starts_with_the_persona_file_body() {
    let (stub, env) = fixture(Some(ASK_BODY)).await;
    let out = env.run(&ask_args(&["--profile", "p"]), TURN).await;
    assert_eq!(out.code(), Some(0), "{out:?}");
    let doc: serde_json::Value = serde_json::from_str(&out.stdout)
        .unwrap_or_else(|e| panic!("ask --output-format json stdout is JSON ({e}): {out:?}"));
    assert!(
        doc["response"]
            .as_str()
            .is_some_and(|text| text.contains(DEFAULT_REPLY)),
        "{doc}"
    );
    let requests = stub.requests();
    assert_eq!(requests.len(), 1, "exactly one model turn");
    assert_starts_with_body(&requests[0], ASK_BODY);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ask_exits_2_and_sends_nothing_when_the_persona_file_is_missing() {
    let (stub, env) = fixture(None).await;
    let out = env.run(&ask_args(&["--profile", "p"]), TURN).await;
    assert_eq!(out.code(), Some(2), "{out:?}");
    assert!(
        out.stderr.contains("Adapter composition failed:"),
        "{out:?}"
    );
    assert!(
        out.stderr
            .contains(&env.persona_path("p.md").display().to_string()),
        "stderr must name the persona path: {out:?}"
    );
    assert!(stub.requests().is_empty(), "a refused ask sends no request");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ask_persona_override_replaces_the_missing_file_reference() {
    let (stub, env) = fixture(None).await;
    let out = env
        .run(&ask_args(&["--profile", "p", "--persona", "coding"]), TURN)
        .await;
    assert_eq!(out.code(), Some(0), "{out:?}");
    let requests = stub.requests();
    assert_eq!(
        requests.len(),
        1,
        "the override still serves one model turn"
    );
    let system = system_text(&requests[0]);
    assert!(
        !system.contains("p.md") && !system.contains(ASK_BODY),
        "no persona-file text under the override: {system:?}"
    );
}

// ───────────────────────────── (c) ACP ─────────────────────────────

/// initialize → session/new → session/prompt against a served ACP child.
async fn acp_prompt(env: &Env, global: &[&str]) {
    let mut acp = AcpProc::spawn(env, global);
    acp.initialize(TURN).await;
    let session = acp.new_session(&env.workspace, TURN).await;
    let response = acp.prompt(&session, "C2 acp probe", TURN).await;
    assert!(
        response.get("result").is_some(),
        "session/prompt must succeed: {response}"
    );
    acp.proc.close_stdin();
    let _ = acp.proc.wait_exit(EXIT).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acp_system_prompt_starts_with_the_persona_file_body() {
    let (stub, env) = fixture(Some(ACP_BODY)).await;
    acp_prompt(&env, &["--profile", "p"]).await;
    let requests = stub.requests();
    assert_eq!(requests.len(), 1, "exactly one model turn");
    assert_starts_with_body(&requests[0], ACP_BODY);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acp_exits_2_before_any_response_when_the_persona_file_is_missing() {
    let (stub, env) = fixture(None).await;
    let mut acp = AcpProc::spawn(&env, &["--profile", "p"]);
    // An `Err` here only means the child already exited, which is the point.
    let _ = acp.request(
        "initialize",
        serde_json::json!({"protocolVersion":1,"clientCapabilities":{}}),
    );
    // A served `initialize` answer means the missing file was not refused.
    let end = std::time::Instant::now() + EXIT;
    let status = loop {
        if let Some(status) = acp.proc.try_exit() {
            break status;
        }
        assert!(
            acp.proc.stdout.lines().is_empty(),
            "ACP answered although the persona file is missing\n{}",
            acp.proc.diagnostics()
        );
        assert!(
            std::time::Instant::now() < end,
            "ACP neither exited nor answered within {EXIT:?}\n{}",
            acp.proc.diagnostics()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let finished = acp.proc.finish(EXIT).await;
    assert_eq!(status.code(), Some(2), "{finished:?}");
    assert!(
        finished.stdout.is_empty(),
        "no JSON-RPC output before the refusal: {finished:?}"
    );
    assert!(
        finished
            .stderr
            .contains(&env.persona_path("p.md").display().to_string()),
        "stderr must name the persona path: {finished:?}"
    );
    assert!(stub.requests().is_empty(), "a refused ACP sends no request");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acp_persona_override_serves_with_no_persona_file_body() {
    let (stub, env) = fixture(None).await;
    acp_prompt(&env, &["--profile", "p", "--persona", "coding"]).await;
    let requests = stub.requests();
    assert_eq!(
        requests.len(),
        1,
        "the override still serves one model turn"
    );
    let system = system_text(&requests[0]);
    assert!(
        !system.contains("p.md") && !system.contains(ACP_BODY),
        "no persona-file text under the override: {system:?}"
    );
}

// ───────────────────────── AC5 positive controls ─────────────────────────

/// Commands that compose no persona keep working with the file missing. With no
/// daemon running `daemon status` prints "Daemon not running." and exits 1
/// (`run_daemon_status` bails; the startup arm maps it to `SubcommandExit::GENERIC`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn commands_that_compose_no_persona_ignore_a_missing_persona_file() {
    let (stub, env) = fixture(None).await;

    let stop = env.run(&["--profile", "p", "daemon", "stop"], EXIT).await;
    assert_eq!(stop.code(), Some(0), "{stop:?}");
    assert!(stop.stdout.contains("Daemon not running."), "{stop:?}");

    let status = env.run(&["--profile", "p", "daemon", "status"], EXIT).await;
    assert_eq!(status.code(), Some(1), "{status:?}");
    assert!(status.stdout.contains("Daemon not running."), "{status:?}");

    let list = env.run(&["profile", "list"], EXIT).await;
    assert_eq!(list.code(), Some(0), "{list:?}");
    assert!(list.stdout.contains("coding"), "{list:?}");

    let show = env.run(&["profile", "show", "p"], EXIT).await;
    assert_eq!(show.code(), Some(0), "{show:?}");
    assert!(!show.stdout.is_empty(), "{show:?}");

    assert!(stub.requests().is_empty(), "none of these sends a request");
}
