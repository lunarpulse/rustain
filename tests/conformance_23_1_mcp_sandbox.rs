#![cfg(all(target_os = "linux", feature = "test-fake-mcp", feature = "sandbox"))]
//! Story 23.1 keystones K10, K11 (AC9, ADR-23-1-01 §9): MCP stdio servers
//! composed through `build_tools("composite")` run under the session ceiling
//! (`WorkspaceWrite{W, network}`) in their own process group, and shutdown
//! stops the group while its leader is alive. Real `fake-mcp-server` behind a
//! `sh -c` wrapper; the "outside" directory is under `CARGO_TARGET_TMPDIR`.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustain::adapters::composite_toolset_adapter::CompositeToolsetAdapter;
use rustain::adapters::sandbox::landlock::{MIN_ABI, require_landlock};
use rustain::domain::models::{
    AppConfig, McpServerSource, McpServerSpec, McpTransport, PermissionMode,
};
use rustain::domain::ports::ToolSetPort;
use rustain::infrastructure::composition::{ComposeContext, build_tools};
use rustain::infrastructure::sandbox::{
    HostPaths, LandlockProbe, ResolvedSandbox, SandboxProbe, resolve_sandbox,
};

use crate::common;

fn outside() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("sandbox-23-1-mcp-out")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("outside dir")
}

fn resolved(workspace: &Path, out: &Path) -> ResolvedSandbox {
    let mut host = HostPaths::detect();
    host.credential_files = vec![];
    let probe = SandboxProbe {
        landlock: LandlockProbe::detect(),
        host,
        inject_not_enforced: false,
    };
    let resolved = resolve_sandbox(
        &AppConfig::default(),
        workspace,
        PermissionMode::Normal,
        &probe,
    )
    .expect("auto resolves on a Landlock host");
    assert!(resolved.session.status.is_enforced());
    let out = std::fs::canonicalize(out).unwrap();
    assert!(
        !resolved
            .session
            .scope
            .writable_roots
            .iter()
            .any(|root| out.starts_with(root)),
        "precondition: {} is outside the writable set",
        out.display()
    );
    resolved
}

fn wrapper(id: &str, script: &str) -> McpServerSpec {
    let mut env = BTreeMap::new();
    env.insert(
        "FAKE_MCP".to_string(),
        common::fake_mcp_binary().to_string_lossy().into_owned(),
    );
    McpServerSpec {
        id: id.to_string(),
        transport: McpTransport::Stdio,
        command: Some("sh".into()),
        args: vec!["-c".into(), script.into()],
        env,
        url: None,
        persistent: false,
        source: McpServerSource::Workspace,
    }
}

fn ctx(workspace: &Path, sandbox: &ResolvedSandbox, spec: McpServerSpec) -> ComposeContext {
    ComposeContext {
        workspace_path: workspace.to_path_buf(),
        config_dir: std::path::PathBuf::new(),
        project_context: rustain::domain::models::project_context::ProjectContext::empty(),
        storage: Arc::new(rustain::adapters::noop::NoOpStorage)
            as Arc<dyn rustain::domain::ports::StoragePort>,
        skill_activator: Arc::new(rustain::adapters::skill_activation::SkillActivator::new()),
        mcp_servers: vec![spec],
        include_builtin_tools: true,
        domain_tx: None,
        channel_turn_tx: None,
        tool_exposure: "static-full".into(),
        assembler: "passthrough".into(),
        skill_exposure: "l1-metadata".into(),
        skill_cache: Arc::new(rustain::infrastructure::skill_cache::SkillCache::new_in_memory()),
        sandbox: sandbox.session.clone(),
        sandbox_slot: sandbox.slot(),
        sandbox_policy: sandbox.policy_cell(),
        memory_slot: Arc::new(arc_swap::ArcSwap::from_pointee(Arc::new(
            rustain::adapters::noop::NoOpMemory,
        )
            as Arc<dyn rustain::domain::ports::MemoryPort>)),
        memory_write_gate: Arc::new(tokio::sync::RwLock::new(())),
        peer_topic_store: Arc::new(rustain::adapters::rap::PeerTopicStore::new()),
        #[cfg(feature = "meta-search")]
        search_config: rustain::domain::models::SearchConfig::default(),
        #[cfg(feature = "meta-search")]
        meta_search_engine: None,
        a2a_peers: Vec::new(),
    }
}

/// Compose, connect, and wait until the server's tools are listed.
async fn connected(tools: &Arc<dyn ToolSetPort>, id: &str) -> bool {
    let composite = tools
        .as_any()
        .downcast_ref::<CompositeToolsetAdapter>()
        .expect("composite");
    composite.start_mcp_connections();
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if tools
            .available_tools()
            .iter()
            .any(|t| t.name.starts_with(&format!("mcp__{id}__")))
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

fn gone(pid: i32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat
            .rsplit(')')
            .next()
            .is_some_and(|rest| rest.trim_start().starts_with('Z')),
    }
}

/// K10 (AC9): the server process tree writes inside the workspace only; the
/// handshake still succeeds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn k10_mcp_stdio_servers_run_under_the_ceiling_policy() {
    if !require_landlock(
        "k10_mcp_stdio_servers_run_under_the_ceiling_policy",
        MIN_ABI,
    ) {
        return;
    }
    let ws = tempfile::tempdir().unwrap();
    let out = outside();
    let sandbox = resolved(ws.path(), out.path());
    let script = format!(
        "touch '{ws}/in'; touch '{out}/out'; exec \"$FAKE_MCP\"",
        ws = ws.path().display(),
        out = out.path().display()
    );
    let tools = build_tools(
        "composite",
        None,
        &ctx(ws.path(), &sandbox, wrapper("k10", &script)),
    )
    .expect("composite composes");
    assert!(connected(&tools, "k10").await, "the handshake succeeds");
    assert!(ws.path().join("in").exists(), "writes inside W land");
    assert!(
        !out.path().join("out").exists(),
        "writes outside W are denied"
    );
    let composite = tools
        .as_any()
        .downcast_ref::<CompositeToolsetAdapter>()
        .unwrap();
    composite.shutdown_mcp_clients(Duration::from_secs(3)).await;
}

/// K11 (AC9): a wrapper's background descendant is stopped with the server's
/// process group on shutdown.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn k11_shutdown_stops_the_servers_process_group() {
    if !require_landlock("k11_shutdown_stops_the_servers_process_group", MIN_ABI) {
        return;
    }
    if !rustain::adapters::mcp::child_handle::pidfd_supported() {
        eprintln!("SKIPPED k11_shutdown_stops_the_servers_process_group: pidfd_open unavailable");
        return;
    }
    let ws = tempfile::tempdir().unwrap();
    let out = outside();
    let sandbox = resolved(ws.path(), out.path());
    let pid_file = ws.path().join("pid");
    let script = format!(
        "sleep 300 & echo $! > '{}'; exec \"$FAKE_MCP\"",
        pid_file.display()
    );
    let tools = build_tools(
        "composite",
        None,
        &ctx(ws.path(), &sandbox, wrapper("k11", &script)),
    )
    .expect("composite composes");
    assert!(connected(&tools, "k11").await, "the handshake succeeds");
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .expect("the wrapper wrote the background pid")
        .trim()
        .parse()
        .unwrap();
    assert!(!gone(pid), "control: the background child runs");
    let composite = tools
        .as_any()
        .downcast_ref::<CompositeToolsetAdapter>()
        .unwrap();
    let (closed, killed) = composite.shutdown_mcp_clients(Duration::from_secs(3)).await;
    assert_eq!(closed + killed, 1, "the leader is stopped as before");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !gone(pid) {
        assert!(
            Instant::now() < deadline,
            "the background descendant {pid} survived shutdown"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
