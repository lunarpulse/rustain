//! Story 23.1 keystones K1-K6 (ADR-23-1-01): Bash through the tools port of
//! the production `ask` composition (`build_cli_core_with_sandbox`, YOLO),
//! confined by the running kernel. The "outside" directory lives under
//! `CARGO_TARGET_TMPDIR`, never under the writable set (asserted).
//!
//! Each test skips loudly without Landlock ABI 3 (4 for K6) and fails instead
//! under `RUSTAIN_REQUIRE_LANDLOCK=1` (hosted CI).
#![cfg(all(target_os = "linux", feature = "sandbox"))]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rustain::adapters::sandbox::landlock::{MIN_ABI, NETWORK_ABI, require_landlock};
use rustain::domain::models::AppConfig;
use rustain::domain::ports::ToolSetPort;
use rustain::infrastructure::composition::build_cli_core_with_sandbox;
use rustain::infrastructure::sandbox::{HostPaths, LandlockProbe, SandboxProbe};
use tokio_util::sync::CancellationToken;

/// A directory outside the writable set.
fn outside() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("sandbox-23-1-out")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("outside dir")
}

/// The real host, with rustain's credential store moved into `data`.
fn probe(data: &Path, inject_not_enforced: bool) -> SandboxProbe {
    let mut host = HostPaths::detect();
    host.credential_files = vec![data.join("auth.json")];
    SandboxProbe {
        landlock: LandlockProbe::detect(),
        host,
        inject_not_enforced,
    }
}

/// The YOLO tools port of the `ask` composition.
fn yolo_tools(
    config: &AppConfig,
    workspace: &Path,
    out: &Path,
    inject_not_enforced: bool,
) -> Arc<dyn ToolSetPort> {
    let (core, session) = build_cli_core_with_sandbox(
        config,
        workspace,
        true,
        Arc::new(rustain::adapters::noop::NoOpPersona),
        &probe(&out.join("data"), inject_not_enforced),
    )
    .expect("the YOLO ask core composes on a Landlock host");
    assert!(session.status.is_enforced(), "{:?}", session.status);
    let out = std::fs::canonicalize(out).unwrap();
    for root in &session.scope.writable_roots {
        assert!(
            !out.starts_with(root),
            "precondition: {} must not be under the writable root {}",
            out.display(),
            root.display()
        );
    }
    core.tools
}

/// Run `command` through the Bash tool: (succeeded, output or error).
async fn bash(tools: &Arc<dyn ToolSetPort>, command: &str) -> (bool, String) {
    match tools
        .execute(
            "Bash",
            serde_json::json!({ "command": command }),
            CancellationToken::new(),
        )
        .await
    {
        Ok(result) => (!result.is_error, result.content),
        Err(error) => (false, error.to_string()),
    }
}

async fn eventually_exists(path: &Path) -> bool {
    for _ in 0..50 {
        if path.exists() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

fn q(path: &Path) -> String {
    format!("'{}'", path.display())
}

fn dirs() -> (tempfile::TempDir, tempfile::TempDir, PathBuf, PathBuf) {
    let ws = tempfile::tempdir().unwrap();
    let out = outside();
    std::fs::create_dir_all(out.path().join("data")).unwrap();
    let ws_path = ws.path().to_path_buf();
    let out_path = out.path().to_path_buf();
    (ws, out, ws_path, out_path)
}

/// K1 (AC3, AC4): a YOLO command cannot write outside the workspace; inside,
/// `/dev/null`, `mktemp` and `/dev/stderr` keep working.
#[tokio::test]
async fn k1_yolo_bash_writes_only_inside_the_writable_set() {
    if !require_landlock("k1_yolo_bash_writes_only_inside_the_writable_set", MIN_ABI) {
        return;
    }
    let (_ws, _out, ws, out) = dirs();
    let tools = yolo_tools(&AppConfig::default(), &ws, &out, false);

    let target = out.join("f");
    let (ok, msg) = bash(&tools, &format!("echo x > {}", q(&target))).await;
    assert!(!ok, "writing outside must fail: {msg}");
    assert!(!target.exists(), "{} must not exist", target.display());

    let (ok, msg) = bash(
        &tools,
        &format!(
            "echo x > {} && echo y > /dev/null && t=$(mktemp) && echo z > \"$t\" && rm \"$t\" \
             && echo e >/dev/stderr",
            q(&ws.join("f"))
        ),
    )
    .await;
    assert!(ok, "writes inside the writable set must work: {msg}");
    assert_eq!(std::fs::read_to_string(ws.join("f")).unwrap(), "x\n");
}

/// K2 (AC4): `setsid` and `nohup` descendants stay confined.
#[tokio::test]
async fn k2_detached_descendants_stay_confined() {
    if !require_landlock("k2_detached_descendants_stay_confined", MIN_ABI) {
        return;
    }
    let (_ws, _out, ws, out) = dirs();
    let tools = yolo_tools(&AppConfig::default(), &ws, &out, false);
    let script = format!(
        "setsid sh -c 'echo x > {og}; echo y > {wg}' >/dev/null 2>&1 & \
         nohup sh -c 'echo x > {oh}; echo y > {wh}' >/dev/null 2>&1 & wait",
        og = out.join("g").display(),
        wg = ws.join("g").display(),
        oh = out.join("h").display(),
        wh = ws.join("h").display(),
    );
    let _ = bash(&tools, &script).await;
    assert!(eventually_exists(&ws.join("g")).await, "setsid child ran");
    assert!(eventually_exists(&ws.join("h")).await, "nohup child ran");
    assert!(!out.join("g").exists(), "setsid escapee wrote outside");
    assert!(!out.join("h").exists(), "nohup child wrote outside");
}

/// K3 (AC4): an outside file cannot be truncated.
#[tokio::test]
async fn k3_outside_files_cannot_be_truncated() {
    if !require_landlock("k3_outside_files_cannot_be_truncated", MIN_ABI) {
        return;
    }
    let (_ws, _out, ws, out) = dirs();
    let existing = out.join("existing");
    std::fs::write(&existing, "keep").unwrap();
    std::fs::write(ws.join("inside"), "drop").unwrap();
    let tools = yolo_tools(&AppConfig::default(), &ws, &out, false);
    // `truncate(1)` and `: >` open the file for writing (WRITE_FILE); a path
    // `truncate(2)` needs only TRUNCATE, which ABI 3 handles.
    let (ok, _) = bash(&tools, &format!("truncate -s0 {}", q(&existing))).await;
    assert!(!ok, "truncate(1) outside must fail");
    let (ok, _) = bash(&tools, &format!(": > {}", q(&existing))).await;
    assert!(!ok, "O_TRUNC outside must fail");
    let path_truncate =
        |path: &Path| format!("perl -e 'truncate($ARGV[0], 0) or exit 1' {}", q(path));
    let (ok, _) = bash(&tools, &path_truncate(&existing)).await;
    assert!(!ok, "truncate(2) by path outside must fail");
    assert_eq!(std::fs::read_to_string(&existing).unwrap(), "keep");
    let (ok, msg) = bash(&tools, &path_truncate(&ws.join("inside"))).await;
    assert!(ok, "truncate(2) inside works: {msg}");
    assert_eq!(std::fs::read_to_string(ws.join("inside")).unwrap(), "");
}

/// K4 (AC4): rustain's credential store is unreadable; its sibling is not.
#[tokio::test]
async fn k4_the_credential_store_is_unreadable() {
    if !require_landlock("k4_the_credential_store_is_unreadable", MIN_ABI) {
        return;
    }
    let (_ws, _out, ws, out) = dirs();
    std::fs::write(out.join("data/auth.json"), "secret-token").unwrap();
    std::fs::write(out.join("data/sibling.txt"), "visible").unwrap();
    let tools = yolo_tools(&AppConfig::default(), &ws, &out, false);
    let (ok, msg) = bash(&tools, &format!("cat {}", q(&out.join("data/auth.json")))).await;
    assert!(!ok && !msg.contains("secret-token"), "{msg}");
    let (ok, msg) = bash(&tools, &format!("cat {}", q(&out.join("data/sibling.txt")))).await;
    assert!(ok && msg.contains("visible"), "{msg}");
}

/// K5 (AC4, AC7): a restriction the child sees as not enforced fails the tool
/// call; the command never runs.
#[tokio::test]
async fn k5_an_unenforced_restriction_fails_the_tool_call() {
    if !require_landlock("k5_an_unenforced_restriction_fails_the_tool_call", MIN_ABI) {
        return;
    }
    let (_ws, _out, ws, out) = dirs();
    let tools = yolo_tools(&AppConfig::default(), &ws, &out, true);
    let marker = ws.join("marker");
    let (ok, msg) = bash(&tools, &format!("echo m > {}", q(&marker))).await;
    assert!(!ok, "the tool call must fail: {msg}");
    assert!(!marker.exists(), "the command must not run unconfined");
}

/// K6 (AC5): `network = false` blocks TCP connect on ABI 4+; `true` allows it.
#[tokio::test]
async fn k6_network_false_blocks_tcp_connect() {
    if !require_landlock("k6_network_false_blocks_tcp_connect", NETWORK_ABI) {
        return;
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let connect = format!("exec 3<>/dev/tcp/127.0.0.1/{port} && echo connected");
    let (_ws, _out, ws, out) = dirs();

    let mut config = AppConfig::default();
    config.sandbox.network = false;
    let tools = yolo_tools(&config, &ws, &out, false);
    let (ok, msg) = bash(&tools, &connect).await;
    assert!(!ok && !msg.contains("connected"), "{msg}");

    let tools = yolo_tools(&AppConfig::default(), &ws, &out, false);
    let (ok, msg) = bash(&tools, &connect).await;
    assert!(ok && msg.contains("connected"), "{msg}");
}
