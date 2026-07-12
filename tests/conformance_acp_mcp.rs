//! Story 14-10 Task 2 (AC2 / AC3 / AC7) — ACP `new_session` MCP **stdio**
//! forwarding into the core-factory composition seam. **RED tests.**
//!
//! These tests defend the DD-B contract that a client-supplied
//! `McpServer::Stdio` carried by a `session/new` request is mapped to a
//! [`McpServerSpec`] and forwarded into the `AcpCoreFactory` composition seam —
//! while `McpServer::Http` / `McpServer::Sse` are dropped LOUDLY (never
//! forwarded), and the client-supplied `env` is preserved LITERALLY (never run
//! through `expand_env_vars` against rustain's process environment — the AC3
//! exfiltration guard, Vex's sleeper).
//!
//! # Why these are RED today
//!
//! Task 2 (DD-B) changes the `AcpCoreFactory` alias from `Fn(&Path)` to
//! `Fn(&Path, &[McpServerSpec])` — the uniform factory seam — and wires
//! `RustainAcpAgent::new_session` to read `args.mcp_servers`, map
//! `McpServer::Stdio` → `McpServerSpec` (dropping Http/Sse with a `warn!`), and
//! call the factory with that slice. That seam has NOT landed yet: the alias is
//! still 1-arg and `new_session` ignores `args.mcp_servers` entirely. So the
//! recording closures below — which take `&[McpServerSpec]` — do not coerce to
//! `AcpCoreFactory`, and this test binary does not compile.
//!
//! **The compile error naming the missing 2-arg factory signature IS the red**:
//! it pinpoints exactly the production surface Task 2 must open. Once Task 2
//! retires the 1-arg alias, threads the slice through the `serve_acp_*`
//! helpers, and maps stdio → spec inside `new_session`, these tests compile and
//! pass with NO edits to this file.
//!
//! # Observation seam (minimal, not a brittle transcript)
//!
//! Each test drives REAL ACP JSON-RPC (`initialize → session/new`) over an
//! in-memory `tokio::io::duplex` pair (no listener) through
//! [`serve_acp_with_acp_core_factory_and_node_tree`], handing the agent a
//! RECORDING `AcpCoreFactory` that snapshots the `&[McpServerSpec]` slice it
//! receives. That snapshot is the whole contract: it proves the request's MCP
//! servers reached the composition seam with the right shape, filtering, and
//! literal env — without a brittle full-JSON-transcript assertion.
//!
//! Determinism: scripted provider (no network), in-memory duplex (no listener),
//! deterministic first session id `acp-1` via `deterministic_acp_id_source`
//! (mirrors the 14-9 injected-clock/id-source precedent reused across the ACP
//! conformance suite).

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
use tokio::sync::mpsc;
use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

use rustain::adapters::acp::agent::AcpCoreFactory;
use rustain::adapters::acp::run::{
    deterministic_acp_id_source, serve_acp_with_acp_core_factory_and_node_tree,
};
use rustain::domain::events::AppEvent;
use rustain::domain::models::{McpServerSpec, McpTransport};
use rustain::domain::ports::{SecurityPort, StoragePort, ToolSetPort, UsageLedgerPort};
use rustain::infrastructure::composition::{AcpCore, CliCore};
use rustain::infrastructure::subagent::NodeTree;

/// Golden initialize request (JSON-RPC, protocol version 1 = ACP LATEST).
/// Mirrors `conformance_acp.rs` so every ACP conformance file agrees on the
/// handshake wire bytes.
const ACP_INITIALIZE_REQ: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}"#;

/// Append a trailing newline so the newline-delimited JSON-RPC framer reads it.
fn line(mut s: String) -> String {
    if !s.ends_with('\n') {
        s.push('\n');
    }
    s
}

// ─────────────────────────────────────────────────────────────────────
// Deterministic scripted provider + lightweight core (no network, no real deps).
// Mirrors the `scripted` + `make_core` harness in `conformance_acp.rs` so the
// recording factory can return a fully-wired `AcpCore` whose `new_session`
// post-processing (model selection, registry lookup) succeeds — WITHOUT driving
// a real prompt (these tests observe the factory invocation, not a turn).
// ─────────────────────────────────────────────────────────────────────

mod scripted {
    use async_trait::async_trait;
    use futures::StreamExt;
    use futures::stream::BoxStream;
    use std::sync::Arc;

    use rustain::domain::errors::ProviderError;
    use rustain::domain::models::provider::{ModelDescriptor, ProviderDescriptor};
    use rustain::domain::models::{CompletionOptions, Message, StreamChunk};
    use rustain::domain::ports::{ProbeOutcome, StreamingProvider};

    /// A provider that streams a fixed chunk script then ends — deterministic,
    /// no network. Never actually consumed by these tests (no prompt is driven);
    /// it exists only so the factory returns a valid `AcpCore`.
    pub struct ScriptedProvider {
        chunks: Vec<StreamChunk>,
    }

    impl ScriptedProvider {
        pub fn text_turn(text: &str) -> Self {
            Self {
                chunks: vec![
                    StreamChunk::Text {
                        content: text.to_string(),
                        parent_tool_use_id: None,
                    },
                    StreamChunk::TurnComplete {
                        stop_reason: rustain::domain::models::StopReason::EndTurn,
                    },
                ],
            }
        }

        pub fn into_provider(self) -> Arc<dyn StreamingProvider> {
            Arc::new(self)
        }
    }

    #[async_trait]
    impl StreamingProvider for ScriptedProvider {
        async fn stream_completion(
            &self,
            _messages: Vec<Message>,
            _options: CompletionOptions,
        ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
            Ok(futures::stream::iter(self.chunks.clone()).boxed())
        }
        async fn abort(&self) -> Result<(), ProviderError> {
            Ok(())
        }
        fn provider_id(&self) -> String {
            "scripted".to_string()
        }
        fn list_models(&self) -> Vec<ModelDescriptor> {
            Vec::new()
        }
        async fn health_check(&self) -> Result<(), ProviderError> {
            Ok(())
        }
        async fn connectivity_probe(&self) -> Result<ProbeOutcome, ProviderError> {
            Ok(ProbeOutcome {
                latency: std::time::Duration::ZERO,
            })
        }
        fn provider_descriptor(&self) -> ProviderDescriptor {
            ProviderDescriptor {
                provider_id: "scripted".to_string(),
                healthy: true,
                model_count: 0,
                display_name: "scripted".to_string(),
            }
        }
    }
}

/// Build a fully-wired `CliCore` backed by the deterministic scripted provider
/// and no-op ports. Mirrors `make_core` in `conformance_acp.rs`; every field is
/// read by `RustainAcpAgent::new_session` post-factory, so they must be live.
fn make_cli_core(workspace: &Path) -> CliCore {
    use rustain::adapters::filesystem::FileSystemStorage;
    use rustain::adapters::noop::{
        NoOpApprovalPersistence, NoOpSecurity, NoOpToolSet, NoOpUsageLedger,
    };
    use rustain::domain::services::approval_runtime::ApprovalRuntime;
    use rustain::domain::services::tool_scheduler::ToolScheduler;

    let provider = scripted::ScriptedProvider::text_turn("mcp forwarding seam").into_provider();
    let security: Arc<dyn SecurityPort> = Arc::new(NoOpSecurity);
    let tools: Arc<dyn ToolSetPort> = Arc::new(NoOpToolSet);
    let approval = ApprovalRuntime::new(64, Arc::new(NoOpApprovalPersistence));
    let tool_scheduler = ToolScheduler::new(security.clone(), tools.clone(), approval.clone(), 64);
    let sessions_dir = workspace.join(".rustain").join("sessions");
    let storage: Arc<dyn StoragePort> = Arc::new(FileSystemStorage::with_workspace_root(
        sessions_dir,
        workspace.to_path_buf(),
    ));
    let (event_tx, event_rx) = mpsc::unbounded_channel::<AppEvent>();
    let ledger: Arc<dyn UsageLedgerPort> = Arc::new(NoOpUsageLedger);

    CliCore {
        provider,
        security,
        tools,
        tool_scheduler,
        approval,
        storage,
        event_tx,
        event_rx,
        ledger,
    }
}

/// Build a recording `AcpCoreFactory` that snapshots the forwarded
/// `&[McpServerSpec]` slice into `recorded` (stored as `Some`, so "called with
/// `[]`" is distinguishable from "never called"), then returns a valid
/// `AcpCore`.
///
/// **RED trigger:** the closure takes `(cwd, specs)` — the 2-arg Task-2 shape.
/// Until `AcpCoreFactory` becomes `Fn(&Path, &[McpServerSpec])`, this closure
/// does not coerce to the alias and the binary fails to compile (the documented
/// red for the missing DD-B seam).
fn recording_factory(
    recorded: Rc<RefCell<Option<Vec<McpServerSpec>>>>,
    ws: PathBuf,
) -> AcpCoreFactory {
    Rc::new(move |_cwd: &Path, specs: &[McpServerSpec]| {
        *recorded.borrow_mut() = Some(specs.to_vec());
        Ok(AcpCore::from(make_cli_core(&ws)))
    })
}

/// Drive `initialize → session/new` (carrying `mcp_servers` as the wire
/// `mcpServers` array) over the in-memory ACP seam and return the
/// `McpServerSpec` slice the recording factory observed.
///
/// Returns `None` when the factory was never invoked — the "no forwarding at
/// all" failure mode, kept distinct from `Some(vec![])` (factory called with an
/// empty slice, the builtin-full fast path).
///
/// The factory is called synchronously inside `new_session` BEFORE the response
/// is enqueued, so reading the `id:2` result guarantees the snapshot was taken.
async fn drive_session_new_capture_specs(
    mcp_servers: serde_json::Value,
) -> Option<Vec<McpServerSpec>> {
    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let ws_for_factory = workspace.path().to_path_buf();

    let (server_incoming, mut client_write) = tokio::io::duplex(8 * 1024);
    let (mut client_read, server_outgoing) = tokio::io::duplex(8 * 1024);

    let node_tree = NodeTree::with_now_fn(Arc::new(|| 1_700_000_000_000));

    let recorded: Rc<RefCell<Option<Vec<McpServerSpec>>>> = Rc::new(RefCell::new(None));
    let recorded_observer = recorded.clone();
    let factory = recording_factory(recorded.clone(), ws_for_factory);

    // The server future is `!Send` (LocalSet + Rc inside the SDK); poll it on
    // this current thread alongside the client driver — the same shape every
    // other in-memory ACP conformance test uses.
    let server = serve_acp_with_acp_core_factory_and_node_tree(
        server_outgoing.compat_write(),
        server_incoming.compat(),
        rustain::domain::models::AppConfig::default(),
        None,
        factory,
        node_tree,
        workspace.path().to_path_buf(),
        deterministic_acp_id_source(),
    );

    let drive = async {
        client_write
            .write_all(line(ACP_INITIALIZE_REQ.to_string()).as_bytes())
            .await
            .expect("write initialize");
        let session_new = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": { "cwd": workspace.path(), "mcpServers": mcp_servers },
        });
        client_write
            .write_all(line(session_new.to_string()).as_bytes())
            .await
            .expect("write session/new");
        client_write.flush().await.expect("flush handshake");

        let reader = tokio::io::BufReader::new(&mut client_read);
        let mut lines = reader.lines();
        loop {
            // Per-line bound: a stuck transport fails fast instead of hanging.
            let next = tokio::time::timeout(Duration::from_secs(10), lines.next_line()).await;
            match next {
                Ok(Ok(Some(raw))) => {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
                        if v["id"] == serde_json::json!(2) {
                            assert!(
                                v.get("result").is_some(),
                                "session/new (id:2) returned an error response: {v}"
                            );
                            return;
                        }
                    }
                }
                _ => return,
            }
        }
    };

    tokio::select! {
        _ = drive => {}
        server_res = server => {
            panic!("ACP server exited before session/new completed: {server_res:?}");
        }
    }
    recorded_observer.borrow().clone()
}

// ─────────────────────────────────────────────────────────────────────
// Section 1 — AC2: a stdio server reaches the composition seam
// ─────────────────────────────────────────────────────────────────────

/// A single client-supplied `McpServer::Stdio` in `session/new` reaches the
/// `AcpCoreFactory` seam as a `McpServerSpec` carrying stdio transport and the
/// forwarded command/args.
///
/// Non-vacuity: the recording factory snapshots the EXACT slice it receives.
/// Today `new_session` ignores `args.mcp_servers`, so the factory (once the
/// signature exists) receives nothing — the length + shape assertions redden.
/// A mutant that forwards but mangles the transport/command (e.g. drops the
/// command, or stamps `Http`) reddens the field assertions. The distinctive
/// name `echo-server` makes a false match implausible.
#[tokio::test(flavor = "current_thread")]
async fn acp_session_new_forwards_stdio_mcp_server_into_core_factory() {
    let mcp_servers = serde_json::json!([
        { "name": "echo-server", "command": "echo", "args": ["--foo"], "env": [] }
    ]);
    let recorded = drive_session_new_capture_specs(mcp_servers).await;

    let specs = recorded.expect("core factory must be invoked during session/new");
    assert_eq!(
        specs.len(),
        1,
        "the single stdio server must be forwarded, got {specs:?}"
    );
    let spec = &specs[0];
    assert_eq!(
        spec.id, "echo-server",
        "forwarded spec id is the request server name"
    );
    assert_eq!(
        spec.transport,
        McpTransport::Stdio,
        "a forwarded stdio server keeps stdio transport"
    );
    assert_eq!(
        spec.command.as_deref(),
        Some("echo"),
        "forwarded command is the request command"
    );
    assert_eq!(
        spec.args,
        vec!["--foo".to_string()],
        "forwarded args match the request args"
    );
}

// ─────────────────────────────────────────────────────────────────────
// Section 2 — AC2 fast path: empty list still invokes the factory
// ─────────────────────────────────────────────────────────────────────

/// An EMPTY `mcp_servers` list still invokes the factory, with an empty slice —
/// preserving the `builtin-full` fast path (NFR10 first-frame). The factory
/// must NOT be skipped, and must NOT receive a phantom spec.
///
/// Non-vacuity: the snapshot distinguishes `Some(vec![])` (factory called,
/// empty slice — the fast path) from `None` (factory never called — a wiring
/// regression that would also starve the stdio-forwarding test). A mutant that
/// only forwards when servers are present and SKIPS the factory call otherwise
/// reddens the `expect`. This is the positive control paired with the composite
/// keystone (Task 6 AC7a): no-MCP builds `builtin-full` and works.
#[tokio::test(flavor = "current_thread")]
async fn acp_session_new_with_empty_mcp_list_invokes_factory_with_empty_slice() {
    let recorded = drive_session_new_capture_specs(serde_json::json!([])).await;

    let specs = recorded.expect("factory must be invoked even with no MCP servers");
    assert!(
        specs.is_empty(),
        "empty mcp list must forward an empty slice (builtin-full fast path), got {specs:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────
// Section 3 — AC2: Http/Sse are dropped, never forwarded
// ─────────────────────────────────────────────────────────────────────

/// `McpServer::Http` / `McpServer::Sse` are DROPPED, never forwarded —
/// rustain's `McpClientAdapter` connects stdio only, so forwarding them would
/// advertise a dead-end. A mixed list (http + sse + stdio) must reach the seam
/// as the SINGLE stdio spec.
///
/// Non-vacuity: THREE servers go in, only the stdio one may come out. A mutant
/// that forwards all transports reddens `len == 1` (it would be 3); a mutant
/// that forwards nothing reddens `len == 1` (it would be 0). The distinctive
/// stdio name `stdio-keeper` makes the survivor assertion unambiguous.
#[tokio::test(flavor = "current_thread")]
async fn acp_session_new_drops_http_and_sse_servers_forwarding_only_stdio() {
    let mcp_servers = serde_json::json!([
        { "type": "http", "name": "dropped-http", "url": "https://example.invalid/mcp", "headers": [] },
        { "type": "sse",  "name": "dropped-sse",  "url": "https://example.invalid/sse", "headers": [] },
        { "name": "stdio-keeper", "command": "echo", "args": [], "env": [] }
    ]);
    let recorded = drive_session_new_capture_specs(mcp_servers).await;

    let specs = recorded.expect("factory must be invoked");
    assert_eq!(
        specs.len(),
        1,
        "only the stdio server may be forwarded; http/sse must be dropped, got {specs:?}"
    );
    assert_eq!(
        specs[0].id, "stdio-keeper",
        "the survivor must be the stdio server"
    );
    assert_eq!(specs[0].transport, McpTransport::Stdio);
}

// ─────────────────────────────────────────────────────────────────────
// Section 4 — AC3 SECURITY: forwarded env is LITERAL, never expanded
// ─────────────────────────────────────────────────────────────────────

/// A forwarded stdio `env` value like `"${VAR}"` is preserved LITERALLY and is
/// NEVER expanded against rustain's process environment. A client must not be
/// able to set `env: {DOWNSTREAM_TOKEN: "${RUSTAIN_SECRET}"}` to exfiltrate a
/// rustain secret into the spawned child (the Vex exfiltration sleeper).
///
/// Non-vacuity: the probe var is SET in the process env to a distinctive
/// sentinel. `expand_env_vars` WOULD expand `${PROBE}` to that sentinel;
/// asserting the recorded spec still carries the LITERAL `${PROBE}` token
/// catches the expansion mutant. (If the var were UNSET, expansion would leave
/// the token literal — per `expand_env_vars`'s "unknown variables left as
/// literal tokens" rule — and the bug would pass. So the var MUST be set.)
///
/// Hermeticity: `PROBE_VAR` is unique to this test and restored on scope exit,
/// so the sentinel never leaks to other tests. The runtime is single-threaded
/// (`current_thread`), so the process-env mutation is race-free within the test.
#[tokio::test(flavor = "current_thread")]
async fn acp_session_new_preserves_forwarded_env_literally_without_expansion() {
    /// Unique probe var — distinct from any real config so cross-test env
    /// interference is impossible.
    const PROBE_VAR: &str = "RUSTAIN_TEST_ACP_MCP_EXFIL_PROBE_14_10";
    const SENTINEL: &str = "EXFIL-SENTINEL-VALUE-14-10";
    const CHILD_KEY: &str = "DOWNSTREAM_TOKEN";

    // SAFETY: single-threaded runtime (`current_thread`) and `PROBE_VAR` is
    // unique to this test — no concurrent reader in the process observes it.
    // Restored on scope exit so the sentinel never leaks.
    unsafe { std::env::set_var(PROBE_VAR, SENTINEL) };
    let _guard = scopeguard::guard((), |_| {
        // SAFETY: same uniqueness / single-thread rationale.
        unsafe { std::env::remove_var(PROBE_VAR) };
    });

    let literal_value = format!("${{{PROBE_VAR}}}");
    let mcp_servers = serde_json::json!([
        {
            "name": "env-probe",
            "command": "echo",
            "args": [],
            "env": [{ "name": CHILD_KEY, "value": literal_value }]
        }
    ]);
    let recorded = drive_session_new_capture_specs(mcp_servers).await;

    let specs = recorded.expect("factory must be invoked");
    assert_eq!(
        specs.len(),
        1,
        "the single stdio server must be forwarded, got {specs:?}"
    );
    let forwarded = specs[0].env.get(CHILD_KEY).unwrap_or_else(|| {
        panic!(
            "child env key `{CHILD_KEY}` was not forwarded into the spec env: {:?}",
            specs[0].env
        );
    });
    assert_eq!(
        forwarded, &literal_value,
        "forwarded env must be the LITERAL token `{literal_value}`, not the expanded process \
         value `{SENTINEL}` — expanding client env against rustain's process environment would \
         exfiltrate rustain secrets into the spawned child (AC3 exfiltration guard)"
    );
}

// ─────────────────────────────────────────────────────────────────────
// Section 5 — AC7a anti-vacuity keystone: a forwarded stdio MCP tool is
// CALLABLE through the production `build_acp_core` composite branch.
// ─────────────────────────────────────────────────────────────────────

/// **AC7a anti-vacuity keystone** — a forwarded stdio MCP server's tool is
/// not merely *forwarded into* the composition seam, it is CALLABLE through
/// the production composite toolset.
///
/// Every other test in this file drives the ACP transport with a RECORDING
/// factory that returns a hand-built `make_cli_core` backed by `NoOpToolSet`,
/// then stops at `session/new`. None construct the production composite
/// branch. The result: reverting the composite switch in `build_acp_core`
/// (the AC7a mutant below) keeps the entire suite green — a 14-8b-class
/// false-green, because no test ever asks "is the forwarded tool actually
/// callable?".
///
/// This keystone calls the REAL `build_acp_core` with a `fake-mcp-server`
/// stdio spec, bounded-polls the returned `AcpCore`'s tools until the
/// projected tool `mcp__echo-svr__echo` appears (a missing tool reddens
/// directly via the poll deadline), then EXECUTES it through the public
/// `ToolSetPort::execute` and asserts the server echoes the input text.
/// That round-trip only succeeds when the composite branch in `build_acp_core`
/// actually built the `McpClientAdapter` + `CompositeToolsetAdapter` and
/// started the connections.
///
/// **The mutant this reddens (AC7a):** in
/// `src/infrastructure/composition/mod.rs` `build_acp_core`, force the
/// non-empty `mcp_servers` branch to `build_tools("builtin-full", None, &ctx)?`
/// unconditionally — bypassing the composite construction. Under that mutant
/// `mcp__echo-svr__echo` is never advertised, the bounded poll times out, and
/// this test FAILS.
///
/// Hermeticity: the `fake-mcp-server` binary is the compiled in-tree fixture
/// (no network, no external process); provider construction in `build_acp_core`
/// is lazy at build time (no credentials/network at compose — see the
/// `build_cli_core_does_not_expose_activate_skill_tool` precedent); the
/// workspace is a tempdir. The deterministic id `echo-svr` makes a false
/// match on `mcp__echo-svr__echo` implausible. Timing is a bounded poll
/// (≤5s, 50ms step) tolerating the detached `start_mcp_connections` connect —
/// never a wall-clock assertion.
#[tokio::test(flavor = "current_thread")]
#[cfg(feature = "mcp")]
async fn build_acp_core_composite_branch_makes_forwarded_mcp_tool_callable() {
    use std::collections::BTreeMap;

    use rustain::domain::models::McpServerSource;
    use rustain::infrastructure::composition::build_acp_core;
    use tokio_util::sync::CancellationToken;

    // Locate the compiled `fake-mcp-server` binary beside the test exe — the
    // same resolution `fake_spec` in conformance_mcp_tool_invocation.rs uses.
    // The fixture exposes an `echo` tool: `{"text": "..."}` -> `echo: <text>`,
    // projected to the wire name `mcp__echo-svr__echo`.
    let binary_name = if cfg!(target_os = "windows") {
        "fake-mcp-server.exe"
    } else {
        "fake-mcp-server"
    };
    let exe_dir = std::env::current_exe()
        .expect("current exe")
        .parent()
        .expect("parent")
        .to_path_buf();
    let mut candidates = vec![
        exe_dir.join(binary_name),
        exe_dir.parent().expect("deps parent").join(binary_name),
    ];
    let command = candidates
        .iter()
        .find(|p| p.exists())
        .cloned()
        .unwrap_or_else(|| candidates.remove(0));

    let spec = McpServerSpec {
        id: "echo-svr".to_string(),
        transport: McpTransport::Stdio,
        command: Some(command.to_string_lossy().into_owned()),
        args: vec![],
        env: BTreeMap::new(),
        url: None,
        persistent: false,
        source: McpServerSource::Workspace,
    };

    let workspace = tempfile::tempdir().expect("workspace tempdir");

    // REAL production composition: a non-empty `mcp_servers` slice drives the
    // `else` (composite) branch of `build_acp_core`'s `#[cfg(feature = "mcp")]`
    // block — the AC7a mutant target.
    let core = build_acp_core(
        &rustain::domain::models::AppConfig::default(),
        workspace.path(),
        false,
        &[spec],
    )
    .expect(
        "build_acp_core must compose hermetically with AppConfig::default() + a temp workspace",
    );

    let tools = core.tools;
    let echo_wire = "mcp__echo-svr__echo";

    // Bounded-poll the projected catalog until the forwarded echo tool
    // appears — tolerating the detached `start_mcp_connections` connect. If it
    // never appears, the composite branch was bypassed (the AC7a mutant) and
    // this panics directly: the mutant reddens.
    let deadline = std::time::Instant::now() + Duration::from_millis(5000);
    let mut catalog: Vec<String> = tools
        .available_tools()
        .into_iter()
        .map(|t| t.name)
        .collect();
    while !catalog.iter().any(|n| n == echo_wire) {
        if std::time::Instant::now() > deadline {
            panic!(
                "AC7a composite-branch keystone: forwarded MCP tool `{echo_wire}` never appeared \
                 in `build_acp_core`'s tool catalog within 5s — the composite branch \
                 (CompositeToolsetAdapter + McpClientAdapter) was NOT constructed (the AC7a \
                 composite-switch mutant). Observed tools: {catalog:?}"
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        catalog = tools
            .available_tools()
            .into_iter()
            .map(|t| t.name)
            .collect();
    }

    // CALLABILITY: execute the forwarded tool through the public
    // `ToolSetPort::execute` and assert the server echoes the input text.
    let result = tools
        .execute(
            echo_wire,
            serde_json::json!({ "text": "hello-acp" }),
            CancellationToken::new(),
        )
        .await
        .expect("executing the forwarded echo tool must not surface a transport error");
    assert!(
        !result.is_error,
        "the forwarded echo tool must not report an error; got content: {}",
        result.content
    );
    assert!(
        result.content.contains("hello-acp"),
        "the forwarded echo tool must echo the input text; got content: {}",
        result.content
    );
    let composite = tools
        .as_any()
        .downcast_ref::<rustain::adapters::composite_toolset_adapter::CompositeToolsetAdapter>()
        .expect("production MCP branch uses the composite adapter");
    composite.stop_mcp_connections().await;
    composite.stop_mcp_connections().await;
}

#[tokio::test(flavor = "current_thread")]
#[cfg(all(feature = "mcp", target_os = "linux"))]
async fn close_session_reaps_real_forwarded_mcp_child() {
    use rustain::adapters::acp::run::{
        deterministic_acp_id_source, serve_acp_with_acp_core_factory_and_node_tree,
    };
    use rustain::domain::models::AppConfig;
    use rustain::infrastructure::composition::build_acp_core;

    fn matching_pids(token: &str) -> Vec<u32> {
        std::fs::read_dir("/proc")
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| entry.file_name().to_string_lossy().parse::<u32>().ok())
            .filter(|pid| {
                std::fs::read(format!("/proc/{pid}/cmdline"))
                    .ok()
                    .is_some_and(|cmdline| String::from_utf8_lossy(&cmdline).contains(token))
            })
            .collect()
    }

    let binary_name = "fake-mcp-server";
    let exe_dir = std::env::current_exe()
        .expect("current exe")
        .parent()
        .expect("parent")
        .to_path_buf();
    let command = [
        exe_dir.join(binary_name),
        exe_dir.parent().unwrap().join(binary_name),
    ]
    .into_iter()
    .find(|path| path.exists())
    .expect("compiled fake-mcp-server");
    let token = format!("reap-probe-{}", std::process::id());
    let workspace = tempfile::tempdir().expect("workspace");
    let config = AppConfig::default();
    let factory: AcpCoreFactory = Rc::new(move |cwd, specs| {
        build_acp_core(&config, cwd, false, specs)
            .map_err(|_| agent_client_protocol::Error::internal_error())
    });
    let (server_incoming, mut client_write) = tokio::io::duplex(16 * 1024);
    let (mut client_read, server_outgoing) = tokio::io::duplex(16 * 1024);
    let server = serve_acp_with_acp_core_factory_and_node_tree(
        server_outgoing.compat_write(),
        server_incoming.compat(),
        AppConfig::default(),
        None,
        factory,
        NodeTree::new(),
        workspace.path().to_path_buf(),
        deterministic_acp_id_source(),
    );
    let wire_servers = serde_json::json!([{
        "name": "reap-server",
        "command": command,
        "args": [token.clone()],
        "env": []
    }]);

    let drive = async {
        client_write
            .write_all(line(ACP_INITIALIZE_REQ.to_string()).as_bytes())
            .await
            .unwrap();
        let new = serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/new",
            "params": { "cwd": workspace.path(), "mcpServers": wire_servers },
        });
        client_write
            .write_all(line(new.to_string()).as_bytes())
            .await
            .unwrap();
        client_write.flush().await.unwrap();
        let reader = tokio::io::BufReader::new(&mut client_read);
        let mut lines = reader.lines();
        loop {
            let raw = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
                .await
                .expect("session/new timeout")
                .unwrap()
                .expect("session/new response");
            let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
            if value["id"] == 2 {
                break;
            }
        }
        let mut poll = tokio::time::interval(Duration::from_millis(10));
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                poll.tick().await;
                if !matching_pids(&token).is_empty() {
                    break;
                }
            }
        })
        .await
        .expect("positive control: real MCP child never appeared");

        let close = serde_json::json!({
            "jsonrpc": "2.0", "id": 3, "method": "session/close",
            "params": { "sessionId": "acp-1" },
        });
        client_write
            .write_all(line(close.to_string()).as_bytes())
            .await
            .unwrap();
        client_write.flush().await.unwrap();
        loop {
            let raw = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
                .await
                .expect("session/close timeout")
                .unwrap()
                .expect("session/close response");
            let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
            if value["id"] == 3 {
                assert!(value["error"].is_null(), "close failed: {value}");
                break;
            }
        }
        drop(client_write);
        assert!(
            matching_pids(&token).is_empty(),
            "session/close returned while the MCP child was still alive"
        );
    };

    tokio::select! {
        () = drive => {}
        result = server => panic!("ACP server exited before close: {result:?}"),
    }
}
