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

// ── Story 23.1 review round 1 ────────────────────────────────────────────────

/// H2: a confined command that swaps a writable root for a symlink to an
/// outside directory gains nothing on the next spawn: the root's descriptor
/// was opened (no-follow) when the session started.
#[tokio::test]
async fn h2_a_writable_root_swapped_for_a_symlink_widens_nothing() {
    if !require_landlock(
        "h2_a_writable_root_swapped_for_a_symlink_widens_nothing",
        MIN_ABI,
    ) {
        return;
    }
    let (_ws, _out, ws, out) = dirs();
    let inner = ws.join("inner");
    std::fs::create_dir_all(&inner).unwrap();
    let mut config = AppConfig::default();
    config.sandbox.writable_roots = vec![inner.clone()];
    let tools = yolo_tools(&config, &ws, &out, false);
    let (ok, msg) = bash(
        &tools,
        &format!(
            "mv {inner} {inner}.old && ln -s {out} {inner}",
            inner = q(&inner),
            out = q(&out)
        ),
    )
    .await;
    assert!(ok, "the swap itself is inside the workspace: {msg}");
    let (ok, _) = bash(&tools, &format!("echo x > {}", q(&inner.join("f")))).await;
    assert!(!ok, "the symlinked root must not be writable");
    assert!(
        !out.join("f").exists(),
        "the write landed outside the sandbox"
    );
}

/// H3 fixture: a scratch copy whose `.git/config` and `.gitattributes` (both
/// the isolated child's to write) name a clean filter that touches `marker`.
async fn run_planted_scratch_diff(
    provider: &rustain::adapters::isolation::CowIsolationProvider,
    lower: &Path,
    marker: &Path,
) {
    use rustain::domain::ports::IsolationProvider as _;
    let handle = provider.start(lower).await.expect("scratch copy");
    std::fs::write(handle.path().join(".gitattributes"), "*.txt filter=pwn\n").unwrap();
    let mut git_config =
        std::fs::read_to_string(handle.path().join(".git/config")).expect("scratch .git/config");
    git_config.push_str(&format!(
        // Unquoted `;` starts a comment in git config: chain with `&&`.
        "[filter \"pwn\"]\n\tclean = touch {} && cat\n",
        marker.display()
    ));
    std::fs::write(handle.path().join(".git/config"), git_config).unwrap();
    // A different size, so git sees the change without a racy-stat re-hash.
    std::fs::write(handle.path().join("a.txt"), "two, longer\n").unwrap();
    let _ = provider.diff(&handle).await;
    let _ = provider.stop(handle).await;
}

/// H3: rustain's own git on a subagent scratch copy runs inside the session
/// sandbox, so a filter the child planted cannot write outside it. The
/// unconfined provider (control) proves the fixture fires.
#[tokio::test]
async fn h3_scratch_git_runs_inside_the_sandbox() {
    if !require_landlock("h3_scratch_git_runs_inside_the_sandbox", MIN_ABI) {
        return;
    }
    let (_ws, _out, ws, out) = dirs();
    std::fs::write(ws.join("a.txt"), "one\n").unwrap();
    let resolved = rustain::infrastructure::sandbox::resolve_sandbox(
        &AppConfig::default(),
        &ws,
        rustain::domain::models::PermissionMode::Normal,
        &probe(&out.join("data"), false),
    )
    .unwrap();
    assert!(resolved.session.status.is_enforced());

    let control = out.join("control");
    run_planted_scratch_diff(
        &rustain::adapters::isolation::CowIsolationProvider::default(),
        &ws,
        &control,
    )
    .await;
    assert!(
        control.exists(),
        "fixture: an unconfined git runs the planted filter"
    );

    let marker = out.join("pwned");
    let confined = rustain::adapters::isolation::CowIsolationProvider::default()
        .with_sandbox(resolved.manager.clone(), resolved.session.scope.ceiling());
    run_planted_scratch_diff(&confined, &ws, &marker).await;
    assert!(
        !marker.exists(),
        "the planted filter ran outside the sandbox"
    );
}

/// H3 (merge-back): `git apply` into the workspace runs confined, so a filter
/// planted in the workspace repository cannot write outside the sandbox.
#[tokio::test]
async fn h3_merge_back_git_runs_inside_the_sandbox() {
    use rustain::domain::ports::PatchApplier as _;
    if !require_landlock("h3_merge_back_git_runs_inside_the_sandbox", MIN_ABI) {
        return;
    }
    let (_ws, _out, ws, out) = dirs();
    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(&ws)
            .output()
            .expect("git");
        assert!(status.status.success(), "git {args:?}: {status:?}");
    };
    git(&["init", "--quiet"]);
    std::fs::write(ws.join("a.txt"), "one\n").unwrap();
    std::fs::write(ws.join(".gitattributes"), "*.txt filter=pwn\n").unwrap();
    let patch = "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-one\n+two\n";
    let plant = |marker: &Path| {
        git(&[
            "config",
            "filter.pwn.clean",
            &format!("sh -c 'touch {}; cat'", marker.display()),
        ]);
    };
    let resolved = rustain::infrastructure::sandbox::resolve_sandbox(
        &AppConfig::default(),
        &ws,
        rustain::domain::models::PermissionMode::Normal,
        &probe(&out.join("data"), false),
    )
    .unwrap();

    let control = out.join("control");
    plant(&control);
    let _ = rustain::adapters::merge_back::GitPatchApplier::default()
        .apply(&ws, patch.as_bytes())
        .await;
    assert!(
        control.exists(),
        "fixture: an unconfined git apply runs the filter"
    );
    std::fs::write(ws.join("a.txt"), "one\n").unwrap();

    let marker = out.join("pwned");
    plant(&marker);
    let _ = rustain::adapters::merge_back::GitPatchApplier::with_sandbox(
        resolved.manager.clone(),
        resolved.session.scope.ceiling(),
    )
    .apply(&ws, patch.as_bytes())
    .await;
    assert!(
        !marker.exists(),
        "the planted filter ran outside the sandbox"
    );
}

/// M1: a Bash command runs in its own session (no controlling terminal), and
/// `/dev/tty` is not writable.
#[tokio::test]
async fn m1_bash_runs_in_its_own_session() {
    if !require_landlock("m1_bash_runs_in_its_own_session", MIN_ABI) {
        return;
    }
    let (_ws, _out, ws, out) = dirs();
    let (tools, session) = yolo_tools_and_session(&AppConfig::default(), &ws, &out);
    assert!(
        !session
            .scope
            .writable_roots
            .contains(&PathBuf::from("/dev/tty")),
        "/dev/tty is not in the writable set"
    );
    let (ok, stat) = bash(&tools, "cat /proc/self/stat").await;
    assert!(ok, "{stat}");
    let fields: Vec<&str> = stat
        .rsplit(')')
        .next()
        .unwrap()
        .split_whitespace()
        .collect();
    let child_sid: i32 = fields[3].parse().unwrap();
    // SAFETY: getsid(0) has no preconditions.
    let own_sid = unsafe { libc::getsid(0) };
    assert_ne!(
        child_sid, own_sid,
        "the command must not share rustain's session"
    );
}

/// M2: on Landlock ABI 6+ a command cannot signal processes outside the
/// sandbox nor connect to an abstract Unix socket outside it.
#[tokio::test]
async fn m2_signals_and_abstract_sockets_are_scoped() {
    use rustain::adapters::sandbox::landlock::SCOPE_ABI;
    if !require_landlock("m2_signals_and_abstract_sockets_are_scoped", SCOPE_ABI) {
        return;
    }
    let (_ws, _out, ws, out) = dirs();
    let tools = yolo_tools(&AppConfig::default(), &ws, &out, false);
    let (ok, msg) = bash(&tools, &format!("kill -0 {}", std::process::id())).await;
    assert!(!ok, "signalling rustain must fail: {msg}");
    let (ok, msg) = bash(&tools, "sleep 30 & kill -0 $! && kill $!").await;
    assert!(ok, "signalling its own child works: {msg}");

    use std::os::linux::net::SocketAddrExt as _;
    let name = format!("rustain-23-1-m2-{}", std::process::id());
    let addr = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
    let _listener = std::os::unix::net::UnixListener::bind_addr(&addr).unwrap();
    let connect = format!(
        "perl -e 'use Socket; socket(my $s, PF_UNIX, SOCK_STREAM, 0) or exit 2; \
         connect($s, pack_sockaddr_un(\"\\0{name}\")) or exit 1'"
    );
    let control = std::process::Command::new("sh")
        .args(["-c", &connect])
        .status()
        .unwrap();
    assert!(
        control.success(),
        "fixture: the socket accepts outside the sandbox"
    );
    let (ok, msg) = bash(&tools, &connect).await;
    assert!(!ok, "the abstract socket must be unreachable: {msg}");
}

/// M3: YOLO is refused when the workspace is the home directory.
#[tokio::test]
async fn m3_yolo_is_refused_when_the_workspace_is_home() {
    if !require_landlock("m3_yolo_is_refused_when_the_workspace_is_home", MIN_ABI) {
        return;
    }
    let (_ws, _out, ws, out) = dirs();
    let mut p = probe(&out.join("data"), false);
    p.host.home = Some(ws.clone());
    let refused = build_cli_core_with_sandbox(
        &AppConfig::default(),
        &ws,
        true,
        Arc::new(rustain::adapters::noop::NoOpPersona),
        &p,
    );
    match refused {
        Err(rustain::domain::errors::AdapterCompositionError::Sandbox(text)) => {
            assert!(text.contains("covers your home directory"), "{text}")
        }
        Err(other) => panic!("expected the sandbox refusal, got {other}"),
        Ok(_) => panic!("YOLO must be refused when the workspace is home"),
    }
    // Normal mode runs, with the warning.
    assert!(
        build_cli_core_with_sandbox(
            &AppConfig::default(),
            &ws,
            false,
            Arc::new(rustain::adapters::noop::NoOpPersona),
            &p,
        )
        .is_ok()
    );
}

/// CR231-06: a denied directory that contains a writable root stays denied
/// (a sibling file is unreadable) while the writable subtree is reported.
#[tokio::test]
async fn cr231_06_denied_directory_with_a_writable_root_inside() {
    if !require_landlock(
        "cr231_06_denied_directory_with_a_writable_root_inside",
        MIN_ABI,
    ) {
        return;
    }
    let (_ws, _out, ws, out) = dirs();
    let denied = out.join("d");
    let inner = denied.join("w");
    std::fs::create_dir_all(&inner).unwrap();
    std::fs::write(denied.join("secret.txt"), "TOPSECRET").unwrap();
    std::fs::write(inner.join("ok.txt"), "visible").unwrap();
    let mut config = AppConfig::default();
    config.sandbox.deny_read = vec![denied.clone()];
    config.sandbox.writable_roots = vec![inner.clone()];
    let (tools, session) = yolo_tools_and_session(&config, &ws, &out);
    match &session.status {
        rustain::domain::models::SandboxStatus::Enforced { unprotected, .. } => assert!(
            unprotected.contains(&std::fs::canonicalize(&inner).unwrap()),
            "{unprotected:?}"
        ),
        other => panic!("expected Enforced, got {other:?}"),
    }
    let (ok, msg) = bash(&tools, &format!("cat {}", q(&denied.join("secret.txt")))).await;
    assert!(!ok && !msg.contains("TOPSECRET"), "{msg}");
    let (ok, msg) = bash(&tools, &format!("cat {}", q(&inner.join("ok.txt")))).await;
    assert!(ok && msg.contains("visible"), "{msg}");
}

/// [`yolo_tools`] that also returns the session (no precondition on `out`:
/// the caller may put writable roots inside it).
fn yolo_tools_and_session(
    config: &AppConfig,
    workspace: &Path,
    out: &Path,
) -> (
    Arc<dyn ToolSetPort>,
    rustain::domain::models::SandboxSession,
) {
    let (core, session) = build_cli_core_with_sandbox(
        config,
        workspace,
        true,
        Arc::new(rustain::adapters::noop::NoOpPersona),
        &probe(&out.join("data"), false),
    )
    .expect("the YOLO ask core composes on a Landlock host");
    assert!(session.status.is_enforced(), "{:?}", session.status);
    (core.tools, session)
}
