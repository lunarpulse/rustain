//! Story C-3 AC1–AC7 — the daemon composes the profile's config-resolved MCP
//! servers, through the real `rustain` binary.
//!
//! Front doors only: `daemon start --foreground`, `daemon attach --plain`,
//! `daemon stop`, real `fake-mcp-server` children (feature `test-fake-mcp`) and
//! the recording Anthropic stub of `common::daemon_proc`. Approvals are answered
//! on the plain client's stdin, never through an in-process API. Every wait is
//! a deadline poll; the child environment is cleared by the harness.
//!
//! Each test gets a unique tag, passed to every fake server as an extra argument
//! (`--c3-tag-<pid>-<n>x-<server>`), so `pgrep -f <tag>` finds exactly that
//! test's MCP children; [`Tag`] kills any survivor on drop so a failing
//! assertion cannot leak a child that ignores stdin EOF.
//!
//! At the base the daemon composes no MCP and never connects: every test below
//! goes RED because a `mcp__` tool, an announce line, a waiting line or a
//! shutdown line is missing, never because of setup.

#![cfg(all(unix, feature = "test-fake-mcp"))]

use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::common::daemon_proc::{
    DaemonProc, Env, PlainAttach, Reply, Stub, poll_until, poll_until_opt, profile_toml,
    spawn_daemon_ready,
};
use crate::common::fake_mcp_binary;

const START: Duration = Duration::from_secs(60);
const TURN: Duration = Duration::from_secs(60);
const EXIT: Duration = Duration::from_secs(30);
const LOG: Duration = Duration::from_secs(30);

/// Builtin tool names the daemon's builtin toolset advertises
/// (`src/adapters/toolset_adapter.rs`: `Bash`, `Read`, `Write`, `Edit`,
/// `apply_patch`, …). Glob/Grep are not builtin tool definitions.
const BUILTIN: [&str; 5] = ["Bash", "Read", "Write", "Edit", "apply_patch"];

const ECHO: &str = "mcp__good__echo";
const ADD: &str = "mcp__good__add";

// ───────────────────────────── helpers ─────────────────────────────

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Unique per-test marker for `pgrep -f`; kills survivors on drop. Declare it
/// FIRST in a test so it drops LAST (after the daemon guard).
struct Tag {
    stem: String,
}

impl Tag {
    fn new() -> Self {
        Self {
            stem: format!(
                "c3-tag-{}-{}x",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::SeqCst)
            ),
        }
    }

    /// The argument handed to the fake server `server`.
    fn arg(&self, server: &str) -> String {
        format!("--{}-{server}", self.stem)
    }

    /// PIDs of every process of this test.
    fn pids(&self) -> Vec<u32> {
        pgrep(&self.stem)
    }

    /// PIDs of the fake server `server` of this test.
    fn pids_of(&self, server: &str) -> Vec<u32> {
        pgrep(&format!("{}-{server}", self.stem))
    }
}

impl Drop for Tag {
    fn drop(&mut self) {
        let _ = Command::new("pkill")
            .args(["-KILL", "-f", &self.stem])
            .status();
    }
}

fn pgrep(pattern: &str) -> Vec<u32> {
    let out = Command::new("pgrep")
        .args(["-f", pattern])
        .output()
        .expect("pgrep must be installed to find MCP children");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

/// One `.claude/mcp.json` entry for the fake server, tagged with `server`.
fn entry(tag: &Tag, server: &str, env: Value, extra_args: &[&str]) -> Value {
    let mut args = vec![tag.arg(server)];
    args.extend(extra_args.iter().map(|arg| (*arg).to_owned()));
    json!({
        "command": fake_mcp_binary().to_string_lossy().into_owned(),
        "args": args,
        "env": env,
    })
}

fn write_mcp_json(env: &Env, servers: Vec<(&str, Value)>) {
    let dir = env.workspace.join(".claude");
    std::fs::create_dir_all(&dir).expect("create .claude");
    let map: serde_json::Map<String, Value> = servers
        .into_iter()
        .map(|(id, value)| (id.to_owned(), value))
        .collect();
    std::fs::write(
        dir.join("mcp.json"),
        json!({ "mcpServers": map }).to_string(),
    )
    .expect("write .claude/mcp.json");
}

/// Workspace with `good` (healthy) and `bad` (initialize always fails).
fn write_good_and_bad(env: &Env, tag: &Tag) {
    write_mcp_json(
        env,
        vec![
            ("good", entry(tag, "good", json!({}), &[])),
            (
                "bad",
                entry(tag, "bad", json!({"FAKE_MCP_FAIL_INITIALIZE": "1"}), &[]),
            ),
        ],
    );
}

/// Every line of the daemon's tracing log. The appender rolls daily, so the
/// file is `<RUSTAIN_LOG_PATH>.<date>` next to the configured path.
fn log_lines(env: &Env) -> Vec<String> {
    let dir = env.log_path.parent().expect("log dir");
    let prefix = env
        .log_path
        .file_name()
        .expect("log file name")
        .to_string_lossy()
        .into_owned();
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .expect("read log dir")
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
        .map(|entry| entry.path())
        .collect();
    files.sort();
    files
        .iter()
        .flat_map(|path| {
            std::fs::read_to_string(path)
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect()
}

fn log_count(env: &Env, needle: &str) -> usize {
    log_lines(env)
        .iter()
        .filter(|line| line.contains(needle))
        .count()
}

/// First daemon-log line satisfying `pred`, polled with a deadline.
async fn wait_log(env: &Env, what: &str, pred: impl Fn(&str) -> bool) -> String {
    poll_until(LOG, what, || {
        log_lines(env).into_iter().find(|line| pred(line))
    })
    .await
}

fn tool_names(request: &Value) -> Vec<String> {
    request["tools"]
        .as_array()
        .expect("the request carries a tools array")
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name").to_owned())
        .collect()
}

fn tool_def<'a>(request: &'a Value, name: &str) -> &'a Value {
    request["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .find(|tool| tool["name"] == name)
        .unwrap_or_else(|| {
            panic!(
                "tool {name:?} missing from the request; tools = {:?}",
                tool_names(request)
            )
        })
}

fn mcp_tool_names(request: &Value) -> Vec<String> {
    let mut names: Vec<String> = tool_names(request)
        .into_iter()
        .filter(|name| name.starts_with("mcp__"))
        .collect();
    names.sort();
    names
}

/// The `tool_result` text the request carries for `tool_use_id`.
fn tool_result(request: &Value, tool_use_id: &str) -> Option<String> {
    request["messages"]
        .as_array()?
        .iter()
        .filter_map(|message| message["content"].as_array())
        .flatten()
        .find(|block| block["type"] == "tool_result" && block["tool_use_id"] == tool_use_id)
        .map(|block| match &block["content"] {
            Value::String(text) => text.clone(),
            Value::Array(parts) => parts
                .iter()
                .filter_map(|part| part["text"].as_str())
                .collect::<Vec<_>>()
                .join(""),
            other => other.to_string(),
        })
}

async fn expect_line(attach: &PlainAttach, needle: &str) -> String {
    attach
        .wait_for_line(|line| line.contains(needle), TURN)
        .await
        .unwrap_or_else(|| {
            panic!(
                "no attach stdout line containing {needle:?}\n{}",
                attach.proc.diagnostics()
            )
        })
}

/// Detach and require a clean client exit.
async fn detach(mut attach: PlainAttach) {
    attach.close_stdin();
    let status = attach.proc.wait_exit(EXIT).await;
    assert!(status.success(), "{}", attach.proc.diagnostics());
}

/// `daemon stop` must succeed; then wait for the foreground daemon to exit.
async fn stop(env: &Env, daemon: &mut DaemonProc) {
    let stopped = env.daemon_stop().await;
    assert_eq!(stopped.code(), Some(0), "{stopped:?}");
    daemon.proc.wait_exit(EXIT).await;
}

/// Stub + isolated env.
async fn world() -> (Stub, Env) {
    let stub = Stub::start().await;
    let env = Env::new(&stub);
    (stub, env)
}

/// Attach, send `line`, return the attach and the first recorded request.
async fn first_turn(env: &Env, stub: &Stub, line: &str) -> (PlainAttach, Value) {
    let mut attach = PlainAttach::spawn(env);
    attach.send_line(line);
    let requests = stub.wait_for_requests(1, TURN).await;
    (attach, requests[0].clone())
}

// ───────────────────────────── AC1 ─────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ac1_coding_profile_announces_its_servers_and_stays_lazy() {
    let tag = Tag::new();
    let (_stub, env) = world().await;
    write_good_and_bad(&env, &tag);
    let mut daemon = spawn_daemon_ready(&env, "coding", START).await;

    let announce = "daemon composes 2 MCP server(s) on first activity: bad, good";
    wait_log(&env, "the compose announce line", |line| {
        line.contains(announce)
    })
    .await;
    assert_eq!(
        log_count(&env, "daemon composes "),
        1,
        "exactly one announce line: {:?}",
        log_lines(&env)
    );
    assert_eq!(
        log_count(&env, announce),
        1,
        "the announce line is exact: {:?}",
        log_lines(&env)
    );
    assert_eq!(
        log_count(&env, "hosts no MCP servers"),
        0,
        "a composite profile never announces 'not composed'"
    );

    // Laziness: no attach, no turn, no cron pre-warm → no MCP child exists.
    assert_eq!(
        tag.pids(),
        Vec::<u32>::new(),
        "no MCP child may exist before the first activity"
    );

    stop(&env, &mut daemon).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ac1_base_profile_announces_that_it_hosts_no_mcp_servers() {
    let tag = Tag::new();
    let (_stub, env) = world().await;
    write_good_and_bad(&env, &tag);
    let mut daemon = spawn_daemon_ready(&env, "base", START).await;

    let notice = "daemon tools adapter 'builtin-only' hosts no MCP servers; \
                  2 configured MCP server(s) not composed";
    wait_log(&env, "the 'hosts no MCP servers' line", |line| {
        line.contains(notice)
    })
    .await;
    assert_eq!(log_count(&env, notice), 1, "{:?}", log_lines(&env));
    assert_eq!(
        log_count(&env, "daemon composes "),
        0,
        "the builtin-only profile composes nothing: {:?}",
        log_lines(&env)
    );
    assert_eq!(tag.pids(), Vec::<u32>::new());

    stop(&env, &mut daemon).await;
}

/// `--profile base` plus one A2A peer: the toolset upgrade notice stays (once,
/// unchanged) and the upgraded composite stays MCP-free on the wire.
#[cfg(feature = "a2a")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ac1_a2a_upgraded_composite_stays_mcp_free() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let card = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/agent-card.json"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"name":"Base Peer","skills":[{"id":"inspect","name":"Inspect"}]}"#,
            "application/json",
        ))
        .mount(&card)
        .await;

    let tag = Tag::new();
    let (stub, env) = world().await;
    write_good_and_bad(&env, &tag);
    let rustain_dir = env.workspace.join(".rustain");
    std::fs::create_dir_all(&rustain_dir).expect("create .rustain");
    std::fs::write(
        rustain_dir.join("a2a.json"),
        format!(r#"{{"agents":{{"base-peer":{{"url":"{}"}}}}}}"#, card.uri()),
    )
    .expect("write a2a.json");

    let mut daemon = spawn_daemon_ready(&env, "base", START).await;
    let upgrade = "daemon toolset upgraded to host A2A peers";
    wait_log(&env, "the A2A upgrade notice", |line| {
        line.contains(upgrade)
    })
    .await;

    stub.script(0, Reply::text("C3-A2A-DONE\n"));
    let (attach, request) = first_turn(&env, &stub, "C3 a2a probe").await;
    assert_eq!(
        mcp_tool_names(&request),
        Vec::<String>::new(),
        "the upgraded composite carries no MCP tool"
    );
    expect_line(&attach, "C3-A2A-DONE").await;
    detach(attach).await;

    stop(&env, &mut daemon).await;
    assert_eq!(
        log_count(&env, upgrade),
        1,
        "the upgrade notice appears once: {:?}",
        log_lines(&env)
    );
    assert_eq!(tag.pids(), Vec::<u32>::new());
}

// ───────────────────────────── AC2 ─────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ac2_first_turn_carries_mcp_tools_beside_the_builtins() {
    let tag = Tag::new();
    let (stub, env) = world().await;
    write_good_and_bad(&env, &tag);
    stub.script(0, Reply::text("C3-AC2-DONE\n"));
    let mut daemon = spawn_daemon_ready(&env, "coding", START).await;

    let (attach, request) = first_turn(&env, &stub, "C3 attach probe").await;
    assert!(
        request.to_string().contains("C3 attach probe"),
        "the request is the attached line's turn: {request}"
    );

    assert_eq!(
        mcp_tool_names(&request),
        vec![ADD.to_owned(), ECHO.to_owned()],
        "exactly the healthy server's two tools, namespaced"
    );
    let echo = tool_def(&request, ECHO);
    assert_eq!(echo["input_schema"]["type"], "object");
    assert_eq!(echo["input_schema"]["properties"]["text"]["type"], "string");
    let add = tool_def(&request, ADD);
    assert_eq!(add["input_schema"]["type"], "object");
    assert_eq!(add["input_schema"]["properties"]["a"]["type"], "number");
    assert_eq!(add["input_schema"]["properties"]["b"]["type"], "number");
    for builtin in ["Bash", "Read", "Write", "Edit"] {
        tool_def(&request, builtin);
    }

    expect_line(&attach, "C3-AC2-DONE").await;
    detach(attach).await;
    stop(&env, &mut daemon).await;
}

// ───────────────────────────── AC3 ─────────────────────────────

/// `bad` is built by `bad_entry`; `good` is healthy. The turn must still reach
/// its end with `good`'s tools only, `bad` named by a warn, the daemon alive.
async fn failing_server_is_isolated(bad_entry: impl FnOnce(&Tag, &Env) -> Value) {
    let tag = Tag::new();
    let (stub, env) = world().await;
    let bad = bad_entry(&tag, &env);
    write_mcp_json(
        &env,
        vec![("good", entry(&tag, "good", json!({}), &[])), ("bad", bad)],
    );
    stub.script(0, Reply::text("C3-AC3-DONE\n"));
    let mut daemon = spawn_daemon_ready(&env, "coding", START).await;

    let (attach, request) = first_turn(&env, &stub, "C3 isolation probe").await;
    assert_eq!(
        mcp_tool_names(&request),
        vec![ADD.to_owned(), ECHO.to_owned()],
        "good's tools present, no mcp__bad__ tool"
    );
    // The turn reaches its end: the stub's reply text arrives on the client.
    expect_line(&attach, "C3-AC3-DONE").await;

    let warn = wait_log(&env, "a warn line naming `bad`", |line| {
        line.contains("WARN")
            && line.contains("MCP server connection failed")
            && line.contains("bad")
    })
    .await;
    assert!(
        !warn.contains("good"),
        "the failure line names only the failing server: {warn}"
    );

    assert!(
        daemon.proc.try_exit().is_none(),
        "the daemon survives a failing MCP server\n{}",
        daemon.proc.diagnostics()
    );
    let status = env
        .run(&["daemon", "status"], Duration::from_secs(20))
        .await;
    assert_eq!(status.code(), Some(0), "{status:?}");

    detach(attach).await;
    stop(&env, &mut daemon).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ac3_a_server_failing_initialize_leaves_the_others_and_the_turn_intact() {
    failing_server_is_isolated(|tag, _| {
        entry(tag, "bad", json!({"FAKE_MCP_FAIL_INITIALIZE": "1"}), &[])
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ac3_a_server_that_fails_to_spawn_leaves_the_others_and_the_turn_intact() {
    failing_server_is_isolated(|tag, env| {
        let missing = env.workspace.join("c3-no-such-mcp-binary");
        assert!(!missing.exists());
        json!({
            "command": missing.to_string_lossy().into_owned(),
            "args": [tag.arg("bad")],
            "env": {},
        })
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ac3_include_builtin_false_leaves_only_the_mcp_tools() {
    let tag = Tag::new();
    let (stub, env) = world().await;
    env.write_profile(
        "nobuiltin",
        &profile_toml("nobuiltin", "coding", None).replace(
            "adapter = \"composite\"\n",
            "adapter = \"composite\"\nconfig = { include_builtin = false }\n",
        ),
    );
    write_mcp_json(&env, vec![("good", entry(&tag, "good", json!({}), &[]))]);
    stub.script(0, Reply::text("C3-AC3B-DONE\n"));
    let mut daemon = spawn_daemon_ready(&env, "nobuiltin", START).await;

    let (attach, request) = first_turn(&env, &stub, "C3 no builtin probe").await;
    let names = tool_names(&request);
    assert!(
        names.iter().any(|name| name == ECHO) && names.iter().any(|name| name == ADD),
        "both mcp__good__ tools present: {names:?}"
    );
    for builtin in BUILTIN {
        assert!(
            !names.iter().any(|name| name == builtin),
            "builtin {builtin:?} must be absent with include_builtin = false: {names:?}"
        );
    }

    expect_line(&attach, "C3-AC3B-DONE").await;
    detach(attach).await;
    stop(&env, &mut daemon).await;
}

// ───────────────────────────── AC4 ─────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ac4_the_first_turn_waits_for_servers_to_settle_before_reading_tools() {
    let tag = Tag::new();
    let gate = tempfile::Builder::new()
        .prefix("c3-gate-")
        .tempdir_in("/tmp")
        .expect("gate dir");
    let gate_dir = gate.path().canonicalize().expect("canonical gate dir");
    let fifo = gate_dir.join("release");
    let made = Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo must be installed");
    assert!(made.success(), "mkfifo {}", fifo.display());

    let (stub, env) = world().await;
    write_mcp_json(
        &env,
        vec![(
            "good",
            entry(
                &tag,
                "good",
                json!({"FAKE_MCP_INIT_GATE": gate_dir.to_string_lossy()}),
                &[],
            ),
        )],
    );
    stub.script(0, Reply::text("C3-AC4-DONE\n"));
    let mut daemon = spawn_daemon_ready(&env, "coding", START).await;

    let mut attach = PlainAttach::spawn(&env);
    attach.send_line("C3 settle probe");

    // The server is parked inside `initialize`, and the turn says it is waiting.
    poll_until(LOG, "the fake server to enter initialize", || {
        gate_dir.join("entered").exists().then_some(())
    })
    .await;
    wait_log(&env, "the settle-wait line", |line| {
        line.contains("waiting up to 15 s for 1 MCP server(s) to settle: good")
    })
    .await;
    assert_eq!(
        stub.requests().len(),
        0,
        "no model request may be sent while a server is still settling"
    );

    // Release the gate: the FIFO write end opens once the server reads.
    let release = fifo.clone();
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let mut writer = std::fs::OpenOptions::new()
            .write(true)
            .open(release)
            .expect("open the release FIFO for writing");
        writer.write_all(b"x").expect("release the gate");
    })
    .await
    .expect("release task");

    let requests = stub.wait_for_requests(1, TURN).await;
    assert_eq!(
        mcp_tool_names(&requests[0]),
        vec![ADD.to_owned(), ECHO.to_owned()],
        "the first request carries the settled server's tools"
    );
    expect_line(&attach, "C3-AC4-DONE").await;
    assert_eq!(
        log_count(&env, "waiting up to 15 s for"),
        1,
        "the waiting line is logged once: {:?}",
        log_lines(&env)
    );
    detach(attach).await;
    stop(&env, &mut daemon).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ac4_a_daemon_with_no_mcp_clients_never_waits() {
    let (stub, env) = world().await;
    stub.script(0, Reply::text("C3-AC4-ZERO-DONE\n"));
    let mut daemon = spawn_daemon_ready(&env, "coding", START).await;

    let (attach, request) = first_turn(&env, &stub, "C3 zero client probe").await;
    assert_eq!(mcp_tool_names(&request), Vec::<String>::new());
    expect_line(&attach, "C3-AC4-ZERO-DONE").await;
    detach(attach).await;

    // A graceful stop flushes the log; only then is "no line" meaningful.
    stop(&env, &mut daemon).await;
    assert_eq!(
        log_count(&env, "waiting up to 15 s for"),
        0,
        "no MCP clients, no waiting line: {:?}",
        log_lines(&env)
    );
    assert_eq!(log_count(&env, "daemon composes "), 0);
}

// ───────────────────────────── AC5 ─────────────────────────────

const DENIAL: &str = "denied: a side-effecting tool needs approval but no client is attached";
const NEEDLE: &str = "echo: C3-ECHO-NEEDLE";

/// Unattended: the elevated `echo` is denied with the headless policy's text
/// and never runs; the read-only `add` runs once. The stub holds each tool_use
/// reply until the sending client has exited, so no writer is attached when the
/// daemon asks for approval.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ac5_unattended_mcp_calls_follow_the_headless_policy() {
    let tag = Tag::new();
    let (stub, env) = world().await;
    write_mcp_json(&env, vec![("good", entry(&tag, "good", json!({}), &[]))]);

    let (echo_call, release_echo) = Reply::tool_use(
        "toolu_c3_echo",
        "mcp__good__echo",
        &json!({"text": "C3-ECHO-NEEDLE"}),
    )
    .held();
    let (add_call, release_add) =
        Reply::tool_use("toolu_c3_add", "mcp__good__add", &json!({"a": 17, "b": 25})).held();
    stub.script(0, echo_call);
    stub.script(1, Reply::text("C3-DENIED-DONE\n"));
    stub.script(2, add_call);
    stub.script(3, Reply::text("C3-SAFE-DONE\n"));
    let mut daemon = spawn_daemon_ready(&env, "coding", START).await;

    // Turn 1: Elevated echo, nobody attached when the call is decided.
    let mut first = PlainAttach::spawn(&env);
    expect_line(&first, "attached (writer)").await;
    first.send_line("C3 unattended echo probe");
    stub.wait_for_requests(1, TURN).await;
    first.close_stdin();
    first.proc.wait_exit(EXIT).await;
    release_echo.release();

    let requests = stub.wait_for_requests(2, TURN).await;
    let denied = tool_result(&requests[1], "toolu_c3_echo")
        .unwrap_or_else(|| panic!("no tool_result for the echo call: {}", requests[1]));
    assert!(
        denied.contains(DENIAL),
        "the unattended denial text, got {denied:?}"
    );
    assert!(
        !denied.contains(NEEDLE) && !requests[1].to_string().contains(NEEDLE),
        "the elevated echo must not have run: {denied:?}"
    );

    // A later attach shows the persisted skip note; it then drives turn 2.
    let mut second = PlainAttach::spawn(&env);
    let skipped = expect_line(&second, "\u{23f8} Skipped:").await;
    assert!(
        skipped.contains("Skipped: mcp__good__echo needed approval"),
        "the skip note names the MCP tool: {skipped}"
    );
    second.send_line("C3 unattended add probe");
    stub.wait_for_requests(3, TURN).await;
    second.close_stdin();
    second.proc.wait_exit(EXIT).await;
    release_add.release();

    // Turn 2: Safe add runs once without any approval.
    let requests = stub.wait_for_requests(4, TURN).await;
    let sum = tool_result(&requests[3], "toolu_c3_add")
        .unwrap_or_else(|| panic!("no tool_result for the add call: {}", requests[3]));
    assert_eq!(sum.trim(), "42", "the read-only add runs unattended");

    stop(&env, &mut daemon).await;
}

/// Attended positive control: the same elevated call runs once the attached
/// writer approves it on the plain client's own stdin.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ac5_an_attached_writer_can_approve_the_elevated_mcp_call() {
    let tag = Tag::new();
    let (stub, env) = world().await;
    write_mcp_json(&env, vec![("good", entry(&tag, "good", json!({}), &[]))]);
    stub.script(
        0,
        Reply::tool_use(
            "toolu_c3_echo",
            "mcp__good__echo",
            &json!({"text": "C3-ECHO-NEEDLE"}),
        ),
    );
    stub.script(1, Reply::text("C3-APPROVED-DONE\n"));
    let mut daemon = spawn_daemon_ready(&env, "coding", START).await;

    let mut attach = PlainAttach::spawn(&env);
    expect_line(&attach, "attached (writer)").await;
    attach.send_line("C3 attended echo probe");

    let request_line = expect_line(
        &attach,
        "[APPROVAL REQUESTED] tool=mcp__good__echo request_id=",
    )
    .await;
    let request_id = request_line
        .split("request_id=")
        .nth(1)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| panic!("no request id in {request_line:?}"))
        .to_owned();
    attach.send_line(&format!("approve {request_id}"));

    let requests = stub.wait_for_requests(2, TURN).await;
    let result = tool_result(&requests[1], "toolu_c3_echo")
        .unwrap_or_else(|| panic!("no tool_result for the echo call: {}", requests[1]));
    assert!(
        result.contains(NEEDLE),
        "the approved echo ran, got {result:?}"
    );
    expect_line(&attach, "C3-APPROVED-DONE").await;

    detach(attach).await;
    stop(&env, &mut daemon).await;
}

// ───────────────────────────── AC6 ─────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ac6_daemon_stop_reaps_every_mcp_child_inside_the_stop_budget() {
    let tag = Tag::new();
    let (stub, env) = world().await;
    write_mcp_json(
        &env,
        vec![
            ("good", entry(&tag, "good", json!({}), &[])),
            (
                "stubborn",
                entry(
                    &tag,
                    "stubborn",
                    json!({"FAKE_MCP_IGNORE_STDIN_EOF": "1"}),
                    &[],
                ),
            ),
            (
                "bad",
                entry(&tag, "bad", json!({"FAKE_MCP_FAIL_INITIALIZE": "1"}), &[]),
            ),
        ],
    );
    stub.script(0, Reply::text("C3-AC6-DONE\n"));
    let mut daemon = spawn_daemon_ready(&env, "coding", START).await;

    let (attach, _request) = first_turn(&env, &stub, "C3 reap probe").await;
    expect_line(&attach, "C3-AC6-DONE").await;
    detach(attach).await;

    // Observed before the stop, so the later absence cannot be vacuous.
    poll_until(LOG, "the good and stubborn children to be running", || {
        (!tag.pids_of("good").is_empty() && !tag.pids_of("stubborn").is_empty()).then_some(())
    })
    .await;

    let started = Instant::now();
    let stopped = env.daemon_stop().await;
    let elapsed = started.elapsed();
    assert_eq!(stopped.code(), Some(0), "{stopped:?}");
    assert!(
        !stopped.stdout.contains("escalated to SIGKILL")
            && !stopped.stderr.contains("escalated to SIGKILL"),
        "the daemon must exit inside the 5 s stop budget, not be SIGKILLed: {stopped:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "daemon stop took {elapsed:?}, over the 5 s budget"
    );
    daemon.proc.wait_exit(EXIT).await;

    // Log order is the deterministic ordering check: the MCP reap line comes
    // before the graceful-shutdown-complete line.
    let complete = "daemon graceful shutdown complete";
    poll_until(LOG, "the shutdown-complete line", || {
        log_lines(&env)
            .iter()
            .any(|line| line.contains(complete))
            .then_some(())
    })
    .await;
    let lines = log_lines(&env);
    let shutdown_indexes: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.contains("daemon MCP shutdown: "))
        .map(|(index, _)| index)
        .collect();
    assert_eq!(
        shutdown_indexes.len(),
        1,
        "exactly one MCP shutdown line: {lines:?}"
    );
    let shutdown = &lines[shutdown_indexes[0]];
    let counts = shutdown
        .split("daemon MCP shutdown: ")
        .nth(1)
        .expect("shutdown line tail");
    let (closed, killed) = counts
        .split_once(" closed, ")
        .unwrap_or_else(|| panic!("unexpected shutdown line {shutdown:?}"));
    let closed: u32 = closed.trim().parse().expect("closed count");
    if rustain::adapters::mcp::child_handle::pidfd_supported() {
        assert!(
            closed >= 2,
            "good and stubborn were both closed: {shutdown:?}"
        );
        assert!(
            killed.starts_with("0 killed"),
            "stubborn is closed by the transport's own kill path, nothing is force-killed: {shutdown:?}"
        );
    } else {
        let _ = std::io::Write::write_all(
            &mut std::io::stderr(),
            b"SKIPPED ac6 pidfd-specific closed accounting: pidfd_open unavailable; checking Unstopped accounting instead\n",
        );
        let killed: u32 = killed
            .split_whitespace()
            .next()
            .expect("killed count")
            .parse()
            .expect("numeric killed count");
        assert_eq!(closed, 0, "unresolved handles are not closed: {shutdown:?}");
        assert!(
            killed >= 2,
            "good and stubborn must be counted as Unstopped: {shutdown:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("MCP child could NOT be stopped")),
            "Unstopped must be logged: {lines:?}"
        );
    }
    let complete_index = lines
        .iter()
        .position(|line| line.contains(complete))
        .expect("complete line");
    assert!(
        shutdown_indexes[0] < complete_index,
        "the MCP shutdown line must precede {complete:?}: {lines:?}"
    );

    // No tagged process remains once the daemon is gone…
    poll_until(
        Duration::from_secs(5),
        "every tagged MCP child to be gone",
        || tag.pids().is_empty().then_some(()),
    )
    .await;
    // …and none appears afterwards (a reconnect supervisor respawning `bad`):
    // absence is asserted on every poll of a bounded 5 s window.
    let reappeared = poll_until_opt(Duration::from_secs(5), || {
        let pids = tag.pids();
        (!pids.is_empty()).then_some(pids)
    })
    .await;
    assert_eq!(
        reappeared, None,
        "no MCP child may appear after the daemon exited"
    );
}

// ───────────────────────────── AC7 ─────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ac7_env_and_argument_canaries_reach_neither_the_log_nor_the_provider() {
    const ENV_CANARY: &str = "C3-CANARY-ENV-9F2B";
    const ARG_CANARY: &str = "C3-CANARY-ARG-77D1";

    let tag = Tag::new();
    let (stub, env) = world().await;
    write_mcp_json(
        &env,
        vec![
            (
                "good",
                entry(
                    &tag,
                    "good",
                    json!({"C3_PROBE_SECRET": ENV_CANARY}),
                    &[ARG_CANARY],
                ),
            ),
            (
                "bad",
                entry(&tag, "bad", json!({"FAKE_MCP_FAIL_INITIALIZE": "1"}), &[]),
            ),
        ],
    );
    stub.script(0, Reply::text("C3-AC7-DONE\n"));
    let mut daemon = spawn_daemon_ready(&env, "coding", START).await;

    let (attach, request) = first_turn(&env, &stub, "C3 canary probe").await;
    // Positive controls: the server really was composed and connected.
    assert_eq!(
        mcp_tool_names(&request),
        vec![ADD.to_owned(), ECHO.to_owned()]
    );
    expect_line(&attach, "C3-AC7-DONE").await;
    detach(attach).await;
    stop(&env, &mut daemon).await;

    let lines = log_lines(&env);
    assert!(
        lines
            .iter()
            .any(|line| line
                .contains("daemon composes 2 MCP server(s) on first activity: bad, good")),
        "the announce line exists, so the log scan is not vacuous: {lines:?}"
    );
    for canary in [ENV_CANARY, ARG_CANARY] {
        assert!(
            !lines.iter().any(|line| line.contains(canary)),
            "{canary} leaked into the daemon log"
        );
        for (index, recorded) in stub.requests().iter().enumerate() {
            assert!(
                !recorded.to_string().contains(canary),
                "{canary} leaked into provider request {index}"
            );
        }
    }
}
