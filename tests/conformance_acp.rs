//! Story 14-7 (Task 1) — ACP Server Mode red/green conformance.
//!
//! These tests defend the externally observable contracts of the ACP
//! (Agent Client Protocol) stdio server:
//!
//! 1. **Dependency pin** — `agent-client-protocol` is pinned to EXACTLY
//!    `=0.10.4` in the crate manifest. ACP wire-compat is version-sensitive;
//!    the pin is the contract (no semver drift, no stale `1.0.1`).
//! 2. **CLI surface** — `rustain acp` is a recognized subcommand that parses
//!    to a distinct `Command::Acp` variant — NOT the implicit TUI path
//!    (`command == None`). This is the cheap, observable half of "startup
//!    routes ACP to the adapter rather than the TUI".
//! 3. **Transcript harness** — driving an in-memory (no-listener) transport
//!    through `initialize → session/new → session/prompt` yields the golden
//!    ACP transcript: an `initialize` result advertising protocol version 1,
//!    a streamed `sessionUpdate` notification carrying the model output, and a
//!    `session/prompt` result with `stopReason: end_turn`. Deterministic, no
//!    real provider (scripted chunks), no network listener.
//!
//! The transcript harness exercises `rustain::adapters::acp::run::serve_acp_with_core_factory`
//! over a `tokio::io::duplex` pair (converted to the futures `AsyncRead`/
//! `AsyncWrite` the seam requires via `tokio_util::compat`). The agent inside
//! spawns the real `turn::run_turn` — the SAME behavioral seam the TUI/`ask`
//! paths use — so the streamed update + final stop reason reflect a real
//! agentic turn, not a parallel implementation.

use std::path::Path;
use std::sync::Arc;

use clap::Parser;
use tokio::sync::mpsc;

use rustain::adapters::cli::commands::{Cli, Command};
use rustain::domain::events::AppEvent;
use rustain::domain::ports::{SecurityPort, StoragePort, ToolSetPort, UsageLedgerPort};
use rustain::infrastructure::composition::CliCore;

#[cfg(feature = "test-instrumentation")]
static ACP_RUN_TURN_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

// ─────────────────────────────────────────────────────────────────────
// Section 1 — Dependency pin contract
// ─────────────────────────────────────────────────────────────────────

/// `agent-client-protocol` is pinned EXACTLY to `=0.10.4`.
///
/// ACP is a wire protocol: a bump (or a stale `1.0.1`) silently breaks
/// interop with every conforming client. The manifest pin is the supply-chain
/// contract that defends it. Parsing the manifest (not substring-grepping)
/// means a future `agent-client-protocol = { version = "0.11", ... }` table
/// form also fails this test.
#[test]
fn cargo_manifest_pins_agent_client_protocol_exactly_0_10_4() {
    let manifest: toml::Value =
        toml::from_str(include_str!("../Cargo.toml")).expect("Cargo.toml must parse as TOML");

    let deps = manifest
        .get("dependencies")
        .and_then(toml::Value::as_table)
        .expect("Cargo.toml must have a [dependencies] table");

    let pin = deps
        .get("agent-client-protocol")
        .expect("agent-client-protocol MUST be a declared dependency (Story 14-7)");

    // Accept either the inline-string form `"... =0.10.4"` or a table with a
    // `version` key, but REQUIRE the exact `=0.10.4` requirement (caret/tilde
    // would permit drift and must fail this test).
    let version_req: String = match pin {
        toml::Value::String(s) => s.clone(),
        toml::Value::Table(t) => t
            .get("version")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
            .expect("table-form dep must carry a `version` key"),
        other => panic!("unexpected manifest value kind for agent-client-protocol: {other:?}"),
    };

    assert_eq!(
        version_req, "=0.10.4",
        "agent-client-protocol must be pinned EXACTLY (=) to 0.10.4 — got `{version_req}`"
    );

    // Guard against a stale/wrong pin surviving alongside the correct one.
    assert!(
        !version_req.contains("1.0.1"),
        "stale 1.0.1 pin must not appear for agent-client-protocol"
    );
}

// ─────────────────────────────────────────────────────────────────────
// Section 2 — CLI command surface (ACP is not the TUI default)
// ─────────────────────────────────────────────────────────────────────

/// `rustain acp` parses to the dedicated `Command::Acp` variant.
///
/// Defends the contract that ACP is a first-class subcommand — a regression
/// that removes the variant or merges it back into the implicit TUI path
/// (`command == None`) reddens this test.
#[test]
fn acp_subcommand_parses_to_dedicated_variant() {
    let cli = Cli::parse_from(["rustain", "acp"]);
    assert!(
        matches!(cli.command, Some(Command::Acp)),
        "`rustain acp` must parse to Some(Command::Acp); got {:?}",
        cli.command
    );
}

/// Bare `rustain` (no subcommand) still routes to the interactive TUI, not ACP.
///
/// Positive control for the routing split: ACP is opt-in via the subcommand,
/// never the default. Establishes the non-TUI routing boundary cheaply.
#[test]
fn bare_invocation_is_tui_not_acp() {
    let cli = Cli::parse_from(["rustain"]);
    assert!(
        cli.command.is_none(),
        "bare `rustain` must remain the implicit TUI path (command == None); got {:?}",
        cli.command
    );
}

// ─────────────────────────────────────────────────────────────────────
// Section 3 — Golden ACP transcript harness (deterministic, no listener)
// ─────────────────────────────────────────────────────────────────────

/// Golden initialize request (JSON-RPC, protocol version 1 = ACP LATEST).
const ACP_INITIALIZE_REQ: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}"#;

/// The user prompt carried by `session/prompt`.
const ACP_PROMPT_TEXT: &str = "ship the ACP transcript";

/// The fixed text the scripted provider streams back. The harness asserts this
/// exact string reaches the client inside an `agent_message_chunk` update.
const GOLDEN_AGENT_TEXT: &str = "ACP transcript is golden";

/// Build a `session/prompt` request body. The session id is the deterministic
/// first session id the agent mints (`acp-1`).
fn acp_prompt_req(id: serde_json::Value, session_id: &str) -> String {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "session/prompt",
        "params": {
            "sessionId": session_id,
            "prompt": [ { "type": "text", "text": ACP_PROMPT_TEXT } ],
        }
    });
    serde_json::to_string(&body).expect("prompt request must serialize")
}

/// Append a trailing newline so the newline-delimited JSON-RPC framer reads it.
fn line(mut s: String) -> String {
    if !s.ends_with('\n') {
        s.push('\n');
    }
    s
}

/// A provider that replays a fixed chunk script — deterministic, no network.
/// Mirrors the `ScriptedProvider` pattern in `adapters/daemon/server.rs`.
mod scripted {
    use async_trait::async_trait;
    use futures::StreamExt;
    use futures::stream::BoxStream;
    use std::sync::Arc;

    use rustain::domain::errors::ProviderError;
    use rustain::domain::models::provider::{ModelDescriptor, ProviderDescriptor};
    use rustain::domain::models::{CompletionOptions, Message, StreamChunk};
    use rustain::domain::ports::{ProbeOutcome, StreamingProvider};

    pub struct ScriptedProvider {
        chunks: Vec<StreamChunk>,
    }

    impl ScriptedProvider {
        /// A turn that streams `text` and ends cleanly — the minimal golden turn.
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

/// Build a fully-wired `CliCore` whose provider is the deterministic scripted
/// turn. This is the value the ACP agent consumes for each prompt — every
/// field below is read by `RustainAcpAgent::run_prompt` + `turn::run_turn`, so
/// they must all be live (not just shape-present).
fn make_core(workspace: &Path) -> CliCore {
    use rustain::adapters::filesystem::FileSystemStorage;
    use rustain::adapters::noop::{
        NoOpApprovalPersistence, NoOpSecurity, NoOpToolSet, NoOpUsageLedger,
    };
    use rustain::domain::services::approval_runtime::ApprovalRuntime;
    use rustain::domain::services::tool_scheduler::ToolScheduler;

    let provider = scripted::ScriptedProvider::text_turn(GOLDEN_AGENT_TEXT).into_provider();
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

/// Driving `initialize → session/new → session/prompt` over an in-memory
/// transport (no listener) yields the golden ACP transcript.
///
/// Defends the wire contract end to end:
/// * `initialize` result advertises protocol version 1 and the `rustain` agent.
/// * The first session is minted as `acp-1`.
/// * The turn is driven through the shared `run_turn` seam: the scripted text
///   arrives as a `sessionUpdate`/`agent_message_chunk` BEFORE the prompt
///   result — proving the agent streams model output, not a stub.
/// * `session/prompt` resolves with `stopReason: end_turn`.
///
/// Determinism: the provider is scripted (no network), the transport is an
/// in-memory duplex (no TCP listener), the session id is deterministic.
#[tokio::test(flavor = "current_thread")]
async fn acp_initialize_then_prompt_matches_golden_transcript() {
    use std::rc::Rc;
    use std::time::Duration;

    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
    use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

    use rustain::adapters::acp::agent::CoreFactory;
    use rustain::adapters::acp::run::serve_acp_with_core_factory;
    use rustain::domain::models::AppConfig;
    #[cfg(feature = "test-instrumentation")]
    use rustain::infrastructure::runtime::turn::RUN_TURN_CALLS;
    #[cfg(feature = "test-instrumentation")]
    use std::sync::atomic::Ordering;

    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let ws_for_factory = workspace.path().to_path_buf();

    // In-memory duplex transport — NO network listener is bound.
    //   server_incoming : what the agent reads (client writes requests here)
    //   server_outgoing : what the agent writes (client reads responses here)
    let (server_incoming, mut client_write) = tokio::io::duplex(8 * 1024);
    let (mut client_read, server_outgoing) = tokio::io::duplex(8 * 1024);

    let core_factory: CoreFactory = Rc::new(move |_cwd: &Path| Ok(make_core(&ws_for_factory)));
    #[cfg(feature = "test-instrumentation")]
    let _run_turn_guard = ACP_RUN_TURN_TEST_LOCK.lock().await;
    #[cfg(feature = "test-instrumentation")]
    RUN_TURN_CALLS.store(0, Ordering::SeqCst);

    // Run the ACP server concurrently with the client driver. The server's
    // future is `!Send` (LocalSet/Rc inside the SDK), so it cannot be
    // `tokio::spawn`ed; `select!` polls both on this current thread. The client
    // completing first drops (cancels) the server — the server exiting first
    // would be a real bug, surfaced as a panic.
    let server = serve_acp_with_core_factory(
        server_outgoing.compat_write(),
        server_incoming.compat(),
        AppConfig::default(),
        workspace.path().to_path_buf(),
        None,
        core_factory,
    );

    let drive = async {
        // 1. Send the golden request transcript.
        client_write
            .write_all(line(ACP_INITIALIZE_REQ.to_string()).as_bytes())
            .await
            .expect("write initialize");
        let session_new = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": { "cwd": workspace.path(), "mcpServers": [] },
        });
        client_write
            .write_all(line(session_new.to_string()).as_bytes())
            .await
            .expect("write session/new");
        // The agent mints the first session as `acp-1` (deterministic).
        client_write
            .write_all(line(acp_prompt_req(serde_json::json!(3), "acp-1")).as_bytes())
            .await
            .expect("write session/prompt");
        client_write.flush().await.expect("flush requests");

        // 2. Read the response stream and assert the golden transcript.
        let reader = tokio::io::BufReader::new(&mut client_read);
        let mut lines = reader.lines();

        let mut saw_init_v1_rustain = false;
        let mut saw_session_acp_1 = false;
        let mut saw_agent_message_chunk = false;
        let mut saw_prompt_end_turn = false;

        loop {
            let next = tokio::time::timeout(Duration::from_secs(10), lines.next_line()).await;
            let raw = match next {
                Ok(Ok(Some(line))) => line,
                _ => break,
            };
            let v: serde_json::Value = match serde_json::from_str(&raw) {
                Ok(v) => v,
                Err(_) => continue,
            };

            // initialize result (id 1): protocolVersion 1, agent is rustain.
            if v["id"] == serde_json::json!(1) {
                let result = &v["result"];
                if result["protocolVersion"] == serde_json::json!(1)
                    && result["agentInfo"]["name"] == serde_json::json!("rustain")
                {
                    saw_init_v1_rustain = true;
                }
                continue;
            }
            // session/new result (id 2): first session id `acp-1`.
            if v["id"] == serde_json::json!(2) {
                if v["result"]["sessionId"] == serde_json::json!("acp-1") {
                    saw_session_acp_1 = true;
                }
                continue;
            }
            // sessionUpdate notification: the streamed model text.
            if v["method"] == serde_json::json!("session/update") {
                let update = &v["params"]["update"];
                if update["sessionUpdate"] == serde_json::json!("agent_message_chunk")
                    && update["content"]["text"] == serde_json::json!(GOLDEN_AGENT_TEXT)
                {
                    saw_agent_message_chunk = true;
                }
                continue;
            }
            // session/prompt result (id 3): turn ended cleanly.
            if v["id"] == serde_json::json!(3) {
                if v["result"]["stopReason"] == serde_json::json!("end_turn") {
                    saw_prompt_end_turn = true;
                }
                break;
            }
        }

        (
            saw_init_v1_rustain,
            saw_session_acp_1,
            saw_agent_message_chunk,
            saw_prompt_end_turn,
        )
    };

    let (init_ok, new_ok, chunk_ok, prompt_ok) = tokio::select! {
        outcome = drive => outcome,
        server_res = server => {
            panic!("ACP server exited before the client finished the transcript: {server_res:?}");
        }
    };

    assert!(
        init_ok,
        "initialize must return protocolVersion 1 + agentInfo.name rustain"
    );
    assert!(new_ok, "session/new must return sessionId `acp-1`");
    assert!(
        chunk_ok,
        "the scripted agent text must stream as a sessionUpdate agent_message_chunk BEFORE the prompt result"
    );
    assert!(
        prompt_ok,
        "session/prompt must resolve with stopReason `end_turn`"
    );
    #[cfg(feature = "test-instrumentation")]
    assert_eq!(
        RUN_TURN_CALLS.load(Ordering::SeqCst),
        1,
        "one ACP session/prompt must drive the shared run_turn chokepoint exactly once"
    );
}
// ─────────────────────────────────────────────────────────────────────
// Section 4 — EOF/session teardown deregisters the Self-rooted node
// ─────────────────────────────────────────────────────────────────────

/// Driving `initialize → session/new` over the in-process ACP seam registers
/// `acp-1` as a `Self`-rooted node in the injected `NodeTree`, and closing the
/// client side (stdin EOF) drives the server's SDK I/O loop to completion,
/// after which that live session node is deregistered.
///
/// Defends the teardown contract: `serve_acp_with_core_factory_and_node_tree`
/// MUST remove every session it registered once its byte-stream transport
/// ends. A mutant that returns from `handle_io` without deregistering — or that
/// skips the cleanup loop — leaves `acp-1` in the tree and reddens the final
/// assertion. The mid-point assertion (`acp-1` IS present after `session/new`)
/// is the registration control: without it a no-op `new_session` would pass the
/// teardown check vacuously, and a broken EOF path that never terminated the
/// server would be caught by the bounded await rather than masquerading as a
/// passing teardown.
///
/// The tree is observed through a clone of the injected `NodeTree` (Arc-backed
/// inner state), never by reaching into the agent. Determinism: scripted
/// provider (no network), in-memory duplex transport (no listener), fixed
/// clock, deterministic first session id.
#[tokio::test(flavor = "current_thread")]
async fn acp_eof_teardown_deregisters_self_rooted_session_node() {
    use std::rc::Rc;
    use std::time::Duration;

    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
    use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

    use rustain::adapters::acp::agent::CoreFactory;
    use rustain::adapters::acp::run::serve_acp_with_core_factory_and_node_tree;
    use rustain::domain::models::AppConfig;
    use rustain::infrastructure::subagent::NodeTree;

    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let ws_for_factory = workspace.path().to_path_buf();

    // In-memory duplex transport — NO network listener is bound.
    //   server_incoming : what the agent reads (client writes requests here)
    //   server_outgoing : what the agent writes (client reads responses here)
    let (server_incoming, mut client_write) = tokio::io::duplex(8 * 1024);
    let (mut client_read, server_outgoing) = tokio::io::duplex(8 * 1024);

    // Injected NodeTree. The observer clone shares the Arc-backed inner state
    // with the tree handed to the server, so registration and deregistration
    // are observable from outside the process seam without touching the agent.
    let node_tree = NodeTree::with_now_fn(Arc::new(|| 1_700_000_000_000));
    let observer = node_tree.clone();

    let core_factory: CoreFactory = Rc::new(move |_cwd: &Path| Ok(make_core(&ws_for_factory)));

    // The server future is `!Send` (LocalSet/Rc inside the SDK); pin it and
    // poll it on this current thread across both client phases. Polled by
    // mutable reference in the handshake so it stays owned for phase B.
    let mut server = Box::pin(serve_acp_with_core_factory_and_node_tree(
        server_outgoing.compat_write(),
        server_incoming.compat(),
        AppConfig::default(),
        None,
        core_factory,
        node_tree,
    ));

    // ── Phase A: drive initialize + session/new, confirm `acp-1` lands. ──
    let drive_handshake = async {
        client_write
            .write_all(line(ACP_INITIALIZE_REQ.to_string()).as_bytes())
            .await
            .expect("write initialize");
        let session_new = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": { "cwd": workspace.path(), "mcpServers": [] },
        });
        client_write
            .write_all(line(session_new.to_string()).as_bytes())
            .await
            .expect("write session/new");
        client_write.flush().await.expect("flush handshake");

        // Read until the session/new result confirms sessionId `acp-1`.
        let reader = tokio::io::BufReader::new(&mut client_read);
        let mut lines = reader.lines();
        loop {
            let next = tokio::time::timeout(Duration::from_secs(10), lines.next_line()).await;
            match next {
                Ok(Ok(Some(raw))) => {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
                        if v["id"] == serde_json::json!(2)
                            && v["result"]["sessionId"] == serde_json::json!("acp-1")
                        {
                            return true;
                        }
                    }
                }
                _ => return false,
            }
        }
    };

    let handshake_ok = tokio::select! {
        outcome = drive_handshake => outcome,
        server_res = &mut server => {
            panic!("ACP server exited before the handshake completed: {server_res:?}");
        }
    };
    assert!(
        handshake_ok,
        "session/new must return sessionId `acp-1` before teardown"
    );

    // Registration control: the Self-rooted node is now in the shared tree.
    // Without this assertion a no-op `new_session` (no registration) would
    // pass the teardown check vacuously — the empty-after check can't tell
    // "removed" from "never added".
    let present: Vec<_> = observer
        .list()
        .await
        .into_iter()
        .filter(|e| e.agent_id.0.as_str() == "acp-1")
        .collect();
    assert_eq!(
        present.len(),
        1,
        "session/new must register exactly one `acp-1` Self-rooted node before teardown"
    );

    // ── Phase B: close client stdin (EOF) and let teardown run. ──
    // Dropping the write half closes the server's read stream; the SDK I/O
    // loop sees EOF and returns, after which the seam deregisters each live
    // session. Bounded so a broken EOF path fails fast instead of hanging.
    drop(client_write);

    // The server's own `Result` (anyhow) is intentionally not asserted here:
    // the contract under test is node teardown, not the exit code the SDK
    // produces on EOF. Binding it keeps the `must_use` lint honest.
    let _server_res = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("ACP server did not shut down within 10s after client EOF");
    // Server completed: the deregister loop ran synchronously before its
    // future resolved, so the tree state is now final.

    let remaining: Vec<_> = observer
        .list()
        .await
        .into_iter()
        .filter(|e| e.agent_id.0.as_str() == "acp-1")
        .collect();
    assert!(
        remaining.is_empty(),
        "EOF teardown must deregister the `acp-1` Self-rooted session node, but it is still registered: {remaining:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────
// Section 5 — AC3 approval bridge contrast over dispatched ACP JSON-RPC
// ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct ToolThenDoneProvider {
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl rustain::domain::ports::StreamingProvider for ToolThenDoneProvider {
    async fn stream_completion(
        &self,
        _messages: Vec<rustain::domain::models::Message>,
        _options: rustain::domain::models::CompletionOptions,
    ) -> Result<
        futures::stream::BoxStream<'static, rustain::domain::models::StreamChunk>,
        rustain::domain::errors::ProviderError,
    > {
        use futures::StreamExt as _;
        use rustain::domain::models::{StopReason, StreamChunk};

        let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let chunks = if call == 0 {
            vec![
                StreamChunk::ToolUse {
                    id: "tool-1".to_string(),
                    name: "Bash".to_string(),
                    input: serde_json::json!({ "command": "touch sentinel" }),
                },
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::ToolUse,
                },
            ]
        } else {
            vec![
                StreamChunk::Text {
                    content: "tool turn complete".to_string(),
                    parent_tool_use_id: None,
                },
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::EndTurn,
                },
            ]
        };
        Ok(futures::stream::iter(chunks).boxed())
    }

    async fn abort(&self) -> Result<(), rustain::domain::errors::ProviderError> {
        Ok(())
    }

    fn provider_id(&self) -> String {
        "tool-then-done".to_string()
    }

    fn list_models(&self) -> Vec<rustain::domain::models::provider::ModelDescriptor> {
        Vec::new()
    }

    async fn health_check(&self) -> Result<(), rustain::domain::errors::ProviderError> {
        Ok(())
    }

    async fn connectivity_probe(
        &self,
    ) -> Result<rustain::domain::ports::ProbeOutcome, rustain::domain::errors::ProviderError> {
        Ok(rustain::domain::ports::ProbeOutcome {
            latency: std::time::Duration::ZERO,
        })
    }

    fn provider_descriptor(&self) -> rustain::domain::models::provider::ProviderDescriptor {
        rustain::domain::models::provider::ProviderDescriptor {
            provider_id: "tool-then-done".to_string(),
            healthy: true,
            model_count: 0,
            display_name: "tool-then-done".to_string(),
        }
    }
}

#[derive(Debug)]
struct SentinelToolSet {
    executions: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl ToolSetPort for SentinelToolSet {
    fn available_tools(&self) -> Vec<rustain::domain::models::ToolDefinition> {
        vec![rustain::domain::models::ToolDefinition {
            name: "Bash".to_string(),
            description: "mutating sentinel tool".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "command": { "type": "string" } },
                "required": ["command"]
            }),
            parallel_safe: false,
        }]
    }

    async fn execute(
        &self,
        _tool_name: &str,
        _input: serde_json::Value,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<rustain::domain::models::ToolResult, rustain::domain::errors::ToolError> {
        self.executions
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(rustain::domain::models::ToolResult {
            tool_use_id: "tool-1".to_string(),
            content: "sentinel touched".to_string(),
            is_error: false,
        })
    }
}

fn make_core_with_sentinel(
    workspace: &Path,
    executions: Arc<std::sync::atomic::AtomicUsize>,
    approval: Arc<rustain::domain::services::approval_runtime::ApprovalRuntime>,
) -> CliCore {
    use rustain::adapters::filesystem::FileSystemStorage;
    use rustain::adapters::noop::{NoOpSecurity, NoOpUsageLedger};
    use rustain::domain::services::tool_scheduler::ToolScheduler;

    let provider: Arc<dyn rustain::domain::ports::StreamingProvider> =
        Arc::new(ToolThenDoneProvider::default());
    let security: Arc<dyn SecurityPort> = Arc::new(NoOpSecurity);
    let tools: Arc<dyn ToolSetPort> = Arc::new(SentinelToolSet { executions });
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

async fn drive_acp_permission_case(option_id: &str) -> (usize, usize) {
    use std::rc::Rc;
    use std::time::Duration;

    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
    use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

    use rustain::adapters::acp::agent::CoreFactory;
    use rustain::adapters::acp::run::serve_acp_with_core_factory;
    use rustain::adapters::noop::NoOpApprovalPersistence;
    use rustain::domain::models::AppConfig;
    use rustain::domain::services::approval_runtime::ApprovalRuntime;

    #[cfg(feature = "test-instrumentation")]
    let _run_turn_guard = ACP_RUN_TURN_TEST_LOCK.lock().await;
    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let ws_for_factory = workspace.path().to_path_buf();
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let executions_for_factory = executions.clone();
    let approval = ApprovalRuntime::new(64, Arc::new(NoOpApprovalPersistence));
    let approval_for_factory = approval.clone();

    let (server_incoming, mut client_write) = tokio::io::duplex(16 * 1024);
    let (mut client_read, server_outgoing) = tokio::io::duplex(16 * 1024);

    let core_factory: CoreFactory = Rc::new(move |_cwd: &Path| {
        Ok(make_core_with_sentinel(
            &ws_for_factory,
            executions_for_factory.clone(),
            approval_for_factory.clone(),
        ))
    });

    let server = serve_acp_with_core_factory(
        server_outgoing.compat_write(),
        server_incoming.compat(),
        AppConfig::default(),
        workspace.path().to_path_buf(),
        None,
        core_factory,
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
            "params": { "cwd": workspace.path(), "mcpServers": [] },
        });
        client_write
            .write_all(line(session_new.to_string()).as_bytes())
            .await
            .expect("write session/new");
        client_write
            .write_all(line(acp_prompt_req(serde_json::json!(3), "acp-1")).as_bytes())
            .await
            .expect("write prompt");
        client_write.flush().await.expect("flush requests");

        let reader = tokio::io::BufReader::new(&mut client_read);
        let mut lines = reader.lines();
        let mut saw_permission = false;

        loop {
            let raw = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
                .await
                .expect("timeout waiting for ACP frame")
                .expect("read ACP frame")
                .expect("ACP stream ended before prompt result");
            let v: serde_json::Value = serde_json::from_str(&raw).expect("ACP frame JSON");
            if v["method"] == serde_json::json!("session/request_permission") {
                saw_permission = true;
                assert_eq!(
                    v["params"]["sessionId"],
                    serde_json::json!("acp-1"),
                    "permission request must be scoped to the ACP session"
                );
                let id = v["id"].clone();
                let response = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "outcome": {
                            "outcome": "selected",
                            "optionId": option_id,
                        }
                    }
                });
                client_write
                    .write_all(line(response.to_string()).as_bytes())
                    .await
                    .expect("write permission response");
                client_write
                    .flush()
                    .await
                    .expect("flush permission response");
                continue;
            }
            if v["id"] == serde_json::json!(3) {
                break;
            }
        }

        assert!(
            saw_permission,
            "ACP prompt must issue session/request_permission"
        );
    };

    tokio::select! {
        _ = drive => {}
        server_res = server => {
            panic!("ACP server exited before permission contrast completed: {server_res:?}");
        }
    }

    (
        executions.load(std::sync::atomic::Ordering::SeqCst),
        approval.rejected_count(),
    )
}

#[tokio::test(flavor = "current_thread")]
async fn acp_permission_bridge_rejects_and_allows_side_effecting_tool() {
    let (reject_executions, reject_count) =
        drive_acp_permission_case(rustain::adapters::acp::translate::PERMISSION_REJECT_ONCE).await;
    assert_eq!(
        reject_executions, 0,
        "Rejecting the ACP permission request must prevent the side-effecting tool"
    );
    assert_eq!(
        reject_count, 1,
        "Rejecting through ACP must resolve via ApprovalRuntime and increment rejected_count"
    );

    let (allow_executions, allow_reject_count) =
        drive_acp_permission_case(rustain::adapters::acp::translate::PERMISSION_ALLOW_ONCE).await;
    assert_eq!(
        allow_executions, 1,
        "Allowing the ACP permission request must execute the side-effecting tool exactly once"
    );
    assert_eq!(
        allow_reject_count, 0,
        "Allowing through ACP must not increment rejected_count"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn acp_session_accepts_consecutive_prompt_turns_without_invalid_params() {
    #[cfg(feature = "test-instrumentation")]
    let _run_turn_guard = ACP_RUN_TURN_TEST_LOCK.lock().await;

    use std::rc::Rc;
    use std::time::Duration;

    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
    use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

    use rustain::adapters::acp::agent::CoreFactory;
    use rustain::adapters::acp::run::serve_acp_with_core_factory;
    use rustain::domain::models::AppConfig;

    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let ws_for_factory = workspace.path().to_path_buf();
    let (server_incoming, mut client_write) = tokio::io::duplex(16 * 1024);
    let (mut client_read, server_outgoing) = tokio::io::duplex(16 * 1024);

    let core_factory: CoreFactory = Rc::new(move |_cwd: &Path| Ok(make_core(&ws_for_factory)));
    let server = serve_acp_with_core_factory(
        server_outgoing.compat_write(),
        server_incoming.compat(),
        AppConfig::default(),
        workspace.path().to_path_buf(),
        None,
        core_factory,
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
            "params": { "cwd": workspace.path(), "mcpServers": [] },
        });
        client_write
            .write_all(line(session_new.to_string()).as_bytes())
            .await
            .expect("write session/new");
        client_write
            .write_all(line(acp_prompt_req(serde_json::json!(3), "acp-1")).as_bytes())
            .await
            .expect("write first prompt");
        client_write.flush().await.expect("flush first prompt");

        let reader = tokio::io::BufReader::new(&mut client_read);
        let mut lines = reader.lines();
        let mut agent_chunks = 0usize;

        loop {
            let raw = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
                .await
                .expect("timeout waiting for first ACP frame")
                .expect("read first ACP frame")
                .expect("ACP stream ended before first prompt result");
            let v: serde_json::Value = serde_json::from_str(&raw).expect("first ACP frame JSON");
            if v["method"] == serde_json::json!("session/update") {
                agent_chunks += 1;
            }
            if v["id"] == serde_json::json!(3) {
                assert!(
                    v.get("error").is_none(),
                    "first ACP prompt returned error: {v}"
                );
                assert_eq!(v["result"]["stopReason"], serde_json::json!("end_turn"));
                break;
            }
        }

        client_write
            .write_all(line(acp_prompt_req(serde_json::json!(4), "acp-1")).as_bytes())
            .await
            .expect("write second prompt");
        client_write.flush().await.expect("flush second prompt");

        loop {
            let raw = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
                .await
                .expect("timeout waiting for second ACP frame")
                .expect("read second ACP frame")
                .expect("ACP stream ended before second prompt result");
            let v: serde_json::Value = serde_json::from_str(&raw).expect("second ACP frame JSON");
            if v["method"] == serde_json::json!("session/update") {
                agent_chunks += 1;
            }
            if v["id"] == serde_json::json!(4) {
                assert!(
                    v.get("error").is_none(),
                    "second ACP prompt returned error: {v}"
                );
                assert_eq!(v["result"]["stopReason"], serde_json::json!("end_turn"));
                break;
            }
        }

        assert!(
            agent_chunks >= 2,
            "each prompt should stream at least one agent message chunk"
        );
    };

    tokio::select! {
        _ = drive => {}
        server_res = server => {
            panic!("ACP server exited before consecutive prompts completed: {server_res:?}");
        }
    }
}
// ─────────────────────────────────────────────────────────────────────
// Section 6 — AC1 cancel keystone (session/cancel → stopReason "cancelled")
// ─────────────────────────────────────────────────────────────────────

/// A scripted provider that sleeps once before yielding its chunk script.
///
/// The delay opens a window in which the client can dispatch a real
/// `session/cancel` notification while `run_turn` is still mid-turn — the
/// only way to exercise the cancel path without racing the (otherwise
/// instantaneous) scripted stream. The stream itself is finite, so even if a
/// cancel is lost the turn still completes and the bounded test timeout fires
/// rather than hanging.
struct DelayedScriptedProvider {
    delay: std::time::Duration,
    chunks: Vec<rustain::domain::models::StreamChunk>,
}

impl DelayedScriptedProvider {
    /// A turn that sleeps `delay`, then streams `text` and ends cleanly.
    fn text_turn(text: &str, delay: std::time::Duration) -> Self {
        use rustain::domain::models::{StopReason, StreamChunk};
        Self {
            delay,
            chunks: vec![
                StreamChunk::Text {
                    content: text.to_string(),
                    parent_tool_use_id: None,
                },
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::EndTurn,
                },
            ],
        }
    }
}

#[async_trait::async_trait]
impl rustain::domain::ports::StreamingProvider for DelayedScriptedProvider {
    async fn stream_completion(
        &self,
        _messages: Vec<rustain::domain::models::Message>,
        _options: rustain::domain::models::CompletionOptions,
    ) -> Result<
        futures::stream::BoxStream<'static, rustain::domain::models::StreamChunk>,
        rustain::domain::errors::ProviderError,
    > {
        use futures::StreamExt as _;
        // Sleep BEFORE returning the stream. run_turn awaits this future, so
        // the turn is in flight (event_tx still open) for the whole delay —
        // the window in which session/cancel is dispatched and the cancel
        // token is armed. A finite chunk script follows so a lost cancel still
        // terminates the turn instead of hanging.
        tokio::time::sleep(self.delay).await;
        Ok(futures::stream::iter(self.chunks.clone()).boxed())
    }

    async fn abort(&self) -> Result<(), rustain::domain::errors::ProviderError> {
        Ok(())
    }
    fn provider_id(&self) -> String {
        "delayed-scripted".to_string()
    }
    fn list_models(&self) -> Vec<rustain::domain::models::provider::ModelDescriptor> {
        Vec::new()
    }
    async fn health_check(&self) -> Result<(), rustain::domain::errors::ProviderError> {
        Ok(())
    }
    async fn connectivity_probe(
        &self,
    ) -> Result<rustain::domain::ports::ProbeOutcome, rustain::domain::errors::ProviderError> {
        Ok(rustain::domain::ports::ProbeOutcome {
            latency: std::time::Duration::ZERO,
        })
    }
    fn provider_descriptor(&self) -> rustain::domain::models::provider::ProviderDescriptor {
        rustain::domain::models::provider::ProviderDescriptor {
            provider_id: "delayed-scripted".to_string(),
            healthy: true,
            model_count: 0,
            display_name: "delayed-scripted".to_string(),
        }
    }
}

/// Build a `CliCore` whose provider delays before streaming the golden text.
/// Mirrors [`make_core`] exactly except for the provider, so every other
/// dependency the ACP agent reads stays live.
fn make_delayed_core(workspace: &Path, delay: std::time::Duration) -> CliCore {
    use rustain::adapters::filesystem::FileSystemStorage;
    use rustain::adapters::noop::{
        NoOpApprovalPersistence, NoOpSecurity, NoOpToolSet, NoOpUsageLedger,
    };
    use rustain::domain::services::approval_runtime::ApprovalRuntime;
    use rustain::domain::services::tool_scheduler::ToolScheduler;

    let provider: Arc<dyn rustain::domain::ports::StreamingProvider> =
        Arc::new(DelayedScriptedProvider::text_turn(GOLDEN_AGENT_TEXT, delay));
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

/// AC1 cancel keystone: dispatching `session/cancel` mid-turn makes the
/// in-flight `session/prompt` resolve with `stopReason: "cancelled"` — NOT
/// `end_turn`, NOT a timeout, NOT a hang.
///
/// This is the 2nd-preflight cancel ruling made non-vacuous: `session/cancel`
/// fires the cooperative `CancellationToken`, and the prompt future returns
/// `cancelled` on channel-close by inspecting the token state (it never waits
/// on a courtesy `Cancelled` chunk that an aborted task would never emit).
///
/// Non-vacuity:
/// * The provider delays before streaming, opening a real mid-turn window in
///   which the cancel notification is dispatched and the token is armed.
/// * A mutant that ignores `session/cancel` (or maps it to `end_turn`)
///   reddens the `stopReason == "cancelled"` assertion.
/// * A mutant that aborts the task raw (no token inspection) would hang → the
///   bounded `tokio::time::timeout` wrapper turns that into a failure instead
///   of a false green (the test must NOT hang).
///
/// Determinism: scripted delayed provider (no network), in-memory duplex
/// transport (no listener), deterministic first session id.
#[tokio::test(flavor = "current_thread")]
async fn acp_cancel_mid_turn_returns_stop_reason_cancelled() {
    use std::rc::Rc;
    use std::time::Duration;

    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
    use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

    use rustain::adapters::acp::agent::CoreFactory;
    use rustain::adapters::acp::run::serve_acp_with_core_factory;
    use rustain::domain::models::AppConfig;

    #[cfg(feature = "test-instrumentation")]
    let _run_turn_guard = ACP_RUN_TURN_TEST_LOCK.lock().await;

    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let ws_for_factory = workspace.path().to_path_buf();

    let (server_incoming, mut client_write) = tokio::io::duplex(16 * 1024);
    let (mut client_read, server_outgoing) = tokio::io::duplex(16 * 1024);

    let core_factory: CoreFactory = Rc::new(move |_cwd: &Path| {
        Ok(make_delayed_core(
            &ws_for_factory,
            // Long enough that the cancel notification is reliably dispatched
            // before the turn completes; short enough to keep the test snappy.
            Duration::from_millis(600),
        ))
    });

    let server = serve_acp_with_core_factory(
        server_outgoing.compat_write(),
        server_incoming.compat(),
        AppConfig::default(),
        workspace.path().to_path_buf(),
        None,
        core_factory,
    );

    let drive = async {
        // initialize → session/new → session/prompt, then cancel mid-turn.
        client_write
            .write_all(line(ACP_INITIALIZE_REQ.to_string()).as_bytes())
            .await
            .expect("write initialize");
        let session_new = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": { "cwd": workspace.path(), "mcpServers": [] },
        });
        client_write
            .write_all(line(session_new.to_string()).as_bytes())
            .await
            .expect("write session/new");
        client_write
            .write_all(line(acp_prompt_req(serde_json::json!(3), "acp-1")).as_bytes())
            .await
            .expect("write session/prompt");
        // session/cancel notification (no id) — dispatched while the turn is
        // still suspended inside the provider's delay window.
        let cancel = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": { "sessionId": "acp-1" }
        });
        client_write
            .write_all(line(cancel.to_string()).as_bytes())
            .await
            .expect("write session/cancel");
        client_write.flush().await.expect("flush prompt + cancel");

        let reader = tokio::io::BufReader::new(&mut client_read);
        let mut lines = reader.lines();
        loop {
            let raw = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
                .await
                .expect("timed out waiting for an ACP frame (cancel path hung)")
                .expect("read error on ACP stream")
                .expect("ACP stream ended before the prompt result");
            let v: serde_json::Value = serde_json::from_str(&raw).expect("ACP frame JSON");
            if v["id"] == serde_json::json!(3) {
                assert!(
                    v.get("error").is_none(),
                    "session/prompt returned an error after cancel: {v}"
                );
                assert_eq!(
                    v["result"]["stopReason"],
                    serde_json::json!("cancelled"),
                    "session/prompt must resolve with stopReason `cancelled` after session/cancel (not end_turn, not a hang), got: {v}"
                );
                return;
            }
        }
    };

    tokio::select! {
        _ = drive => {}
        server_res = server => {
            panic!("ACP server exited before the cancel-path prompt completed: {server_res:?}");
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// Section 7 — AC2 stream-equivalence golden: ACP path vs the ask/run_turn seam
// ─────────────────────────────────────────────────────────────────────

/// Drive `turn::run_turn` directly with a fresh `CliCore` (scripted provider)
/// and concatenate every `StreamChunk::Text` off the event stream — the same
/// seam `rustain ask` drains (ask.rs spawns run_turn and reads `event_rx`).
/// Returns the full assistant text the engine emits, so it can be compared
/// byte-for-byte with the ACP `session/update` path.
async fn run_direct_turn_collect_text(workspace: &Path) -> String {
    use rustain::domain::models::CompletionOptions;
    use rustain::domain::models::Message;
    use rustain::domain::models::MessageRole;
    use rustain::domain::models::StreamChunk;
    use rustain::domain::models::conversation::generate_conversation_id;
    use rustain::domain::models::router::{EscalationReason, ModelTier};
    use rustain::domain::services::model_router::ResolvedModel;
    use rustain::infrastructure::runtime::turn::run_turn;
    use tokio_util::sync::CancellationToken;

    let core = make_core(workspace);
    let CliCore {
        provider,
        security,
        tools,
        tool_scheduler,
        approval: _,
        storage,
        event_tx,
        mut event_rx,
        ledger,
    } = core;

    // The same user prompt the ACP harness sends — both paths must see it.
    let messages = vec![Message {
        role: MessageRole::User,
        content: ACP_PROMPT_TEXT.to_string(),
        images: vec![],
        tool_results: vec![],
        tool_uses: vec![],
        context_prefix: None,
        reasoning_content: None,
    }];
    let options = CompletionOptions {
        model: "test".into(),
        max_tokens: 8192,
        system_prompt: String::new(),
        temperature: None,
        tools: tools.available_tools(),
    };
    let conversation = rustain::domain::models::Conversation {
        id: generate_conversation_id(),
        session_id: Some("direct-test".into()),
        ..Default::default()
    };

    let handle = tokio::spawn(run_turn(
        provider,
        messages,
        options,
        event_tx,
        security,
        tools.clone(),
        tool_scheduler,
        conversation.id.clone(),
        storage,
        conversation,
        None,
        CancellationToken::new(),
        ledger,
        ResolvedModel {
            model: "test".into(),
            tier: ModelTier::CheapAgentic,
            escalation_reason: EscalationReason::None,
        },
        None,
        0,
        None,
        "direct-test".into(),
    ));
    // `tools`/`tool_scheduler` were moved (or cloned) into run_turn above; the
    // turn owns the only live ToolSetPort senders now, so event_tx closes when
    // it finishes and the event_rx loop below terminates naturally.

    let mut text = String::new();
    while let Some(event) = event_rx.recv().await {
        if let AppEvent::ProviderChunk {
            chunk: StreamChunk::Text { content, .. },
            ..
        } = event
        {
            text.push_str(&content);
        }
    }
    // The turn task must complete without panicking (a panic would silently
    // close the channel and truncate the collected text).
    handle.await.expect("direct run_turn task panicked");
    text
}

/// AC2 stream-equivalence keystone: the SAME scripted provider, driven through
/// (a) the ACP `session/update` path and (b) a direct `run_turn` call (the
/// `ask`/daemon seam), yields the SAME assistant text.
///
/// This proves "one agent binary, multiple call paths" at the behavioral level
/// — the ACP adapter does not fork the agent loop or reimplement streaming; it
/// forwards the same `StreamChunk::Text` the shared `run_turn` emits. A private
/// loop that "mostly works" but diverges from the golden reddens the equality
/// assertion (the mutant the epic AC2 wording demands).
///
/// Non-vacuity: both paths consume `ScriptedProvider::text_turn(GOLDEN_AGENT_TEXT)`
/// — the ACP path through the dispatched JSON-RPC forwarder, the direct path
/// through the same `run_turn` the `ask` subcommand drives. The two outputs are
/// compared to EACH OTHER (not just to a hardcoded constant), so a divergence
/// in either path is caught.
#[tokio::test(flavor = "current_thread")]
async fn acp_and_ask_produce_equivalent_stream_output() {
    use std::rc::Rc;
    use std::time::Duration;

    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
    use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

    use rustain::adapters::acp::agent::CoreFactory;
    use rustain::adapters::acp::run::serve_acp_with_core_factory;
    use rustain::domain::models::AppConfig;

    #[cfg(feature = "test-instrumentation")]
    let _run_turn_guard = ACP_RUN_TURN_TEST_LOCK.lock().await;

    // ── (a) ACP path: collect agent_message_chunk text from session/update. ──
    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let ws_for_factory = workspace.path().to_path_buf();
    let (server_incoming, mut client_write) = tokio::io::duplex(16 * 1024);
    let (mut client_read, server_outgoing) = tokio::io::duplex(16 * 1024);

    let core_factory: CoreFactory = Rc::new(move |_cwd: &Path| Ok(make_core(&ws_for_factory)));
    let server = serve_acp_with_core_factory(
        server_outgoing.compat_write(),
        server_incoming.compat(),
        AppConfig::default(),
        workspace.path().to_path_buf(),
        None,
        core_factory,
    );

    let acp_drive = async {
        client_write
            .write_all(line(ACP_INITIALIZE_REQ.to_string()).as_bytes())
            .await
            .expect("write initialize");
        let session_new = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": { "cwd": workspace.path(), "mcpServers": [] },
        });
        client_write
            .write_all(line(session_new.to_string()).as_bytes())
            .await
            .expect("write session/new");
        client_write
            .write_all(line(acp_prompt_req(serde_json::json!(3), "acp-1")).as_bytes())
            .await
            .expect("write prompt");
        client_write.flush().await.expect("flush ACP requests");

        let reader = tokio::io::BufReader::new(&mut client_read);
        let mut lines = reader.lines();
        let mut acp_text = String::new();
        loop {
            let raw = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
                .await
                .expect("ACP frame timeout")
                .expect("read error on ACP stream")
                .expect("ACP stream ended before prompt result");
            let v: serde_json::Value = serde_json::from_str(&raw).expect("ACP frame JSON");
            if v["method"] == serde_json::json!("session/update") {
                let update = &v["params"]["update"];
                if update["sessionUpdate"] == serde_json::json!("agent_message_chunk") {
                    if let Some(text) = update["content"]["text"].as_str() {
                        acp_text.push_str(text);
                    }
                }
                continue;
            }
            if v["id"] == serde_json::json!(3) {
                assert_eq!(
                    v["result"]["stopReason"],
                    serde_json::json!("end_turn"),
                    "ACP path must complete cleanly for the equivalence comparison: {v}"
                );
                return acp_text;
            }
        }
    };

    let acp_text = tokio::select! {
        text = acp_drive => text,
        server_res = server => {
            panic!("ACP server exited before the equivalence prompt completed: {server_res:?}");
        }
    };

    // ── (b) Direct path: drive run_turn (the ask/daemon seam) with the same
    //        scripted provider and collect StreamChunk::Text from event_rx. ──
    let workspace_b = tempfile::tempdir().expect("workspace B tempdir");
    let direct_text = run_direct_turn_collect_text(workspace_b.path()).await;

    // ── Equivalence: both paths produced the same assistant text. ──
    assert_eq!(
        acp_text, GOLDEN_AGENT_TEXT,
        "ACP session/update path must carry the scripted agent text verbatim"
    );
    assert_eq!(
        direct_text, GOLDEN_AGENT_TEXT,
        "the direct run_turn (ask) path must carry the scripted agent text verbatim"
    );
    assert_eq!(
        acp_text, direct_text,
        "ACP and ask paths must produce byte-identical assistant text from the same provider"
    );
}

// ─────────────────────────────────────────────────────────────────────
// Section 8 — AC6 MockClock determinism keystone
// ─────────────────────────────────────────────────────────────────────

/// AC6(b) determinism keystone: an ACP `session/new` driven through the real
/// dispatched JSON-RPC path, with an injected `NodeTree` whose `now_fn` returns
/// a KNOWN fixed timestamp, materializes the `acp-1` Self-rooted node with
/// `spawned_at` EXACTLY that timestamp.
///
/// This proves the Clock-backed `now_fn` from Task 5 is actually used to stamp
/// the session node — the AC6 non-vacuity condition. A mutant that stamps
/// `spawned_at` with a raw `chrono::Utc::now()` (or hardcodes `0`) reddens the
/// exact-equality assertion: the injected `42_000` would never appear.
///
/// Non-vacuity: the node is observed through a clone of the injected `NodeTree`
/// (Arc-backed inner state), read AFTER the real dispatched `session/new`
/// result confirms `acp-1`. Determinism: scripted provider (no network),
/// in-memory duplex transport (no listener), fixed clock.
#[tokio::test(flavor = "current_thread")]
async fn acp_session_node_spawned_at_reflects_mock_clock() {
    use std::rc::Rc;
    use std::time::Duration;

    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
    use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

    use rustain::adapters::acp::agent::CoreFactory;
    use rustain::adapters::acp::run::serve_acp_with_core_factory_and_node_tree;
    use rustain::domain::models::AppConfig;
    use rustain::infrastructure::subagent::NodeTree;

    #[cfg(feature = "test-instrumentation")]
    let _run_turn_guard = ACP_RUN_TURN_TEST_LOCK.lock().await;

    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let ws_for_factory = workspace.path().to_path_buf();
    let (server_incoming, mut client_write) = tokio::io::duplex(16 * 1024);
    let (mut client_read, server_outgoing) = tokio::io::duplex(16 * 1024);

    // Injected NodeTree with a fixed known now_fn — the MockClock stand-in.
    // `42_000` is deliberately small and distinctive so a raw wall-clock stamp
    // or a `0` default can never coincide with it.
    const FIXED_NOW_MS: i64 = 42_000;
    let node_tree = NodeTree::with_now_fn(Arc::new(move || FIXED_NOW_MS));
    let observer = node_tree.clone();

    let core_factory: CoreFactory = Rc::new(move |_cwd: &Path| Ok(make_core(&ws_for_factory)));

    let mut server = Box::pin(serve_acp_with_core_factory_and_node_tree(
        server_outgoing.compat_write(),
        server_incoming.compat(),
        AppConfig::default(),
        None,
        core_factory,
        node_tree,
    ));

    // Drive the real dispatched initialize → session/new and confirm acp-1.
    let drive = async {
        client_write
            .write_all(line(ACP_INITIALIZE_REQ.to_string()).as_bytes())
            .await
            .expect("write initialize");
        let session_new = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": { "cwd": workspace.path(), "mcpServers": [] },
        });
        client_write
            .write_all(line(session_new.to_string()).as_bytes())
            .await
            .expect("write session/new");
        client_write.flush().await.expect("flush handshake");

        let reader = tokio::io::BufReader::new(&mut client_read);
        let mut lines = reader.lines();
        loop {
            let raw = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
                .await
                .expect("timeout waiting for session/new frame")
                .expect("read error on ACP stream")
                .expect("ACP stream ended before session/new result");
            let v: serde_json::Value = serde_json::from_str(&raw).expect("ACP frame JSON");
            if v["id"] == serde_json::json!(2)
                && v["result"]["sessionId"] == serde_json::json!("acp-1")
            {
                return;
            }
        }
    };

    tokio::select! {
        _ = drive => {}
        server_res = &mut server => {
            panic!("ACP server exited before session/new completed: {server_res:?}");
        }
    }

    // The injected Clock value MUST surface on the materialized Self node.
    let entry = observer
        .list()
        .await
        .into_iter()
        .find(|e| e.agent_id.0.as_str() == "acp-1")
        .expect("session/new must register the `acp-1` Self-rooted node");
    assert_eq!(
        entry.spawned_at, FIXED_NOW_MS,
        "the ACP session node's spawned_at must equal the injected Clock now_fn value \
         ({}), proving the Clock-backed now_fn is used — got {}",
        FIXED_NOW_MS, entry.spawned_at
    );

    // Drop the client write half so the server sees EOF and shuts down cleanly
    // (bounded — a broken teardown fails fast instead of leaking the task).
    drop(client_write);
    let _ = tokio::time::timeout(Duration::from_secs(10), server).await;
}

// ─────────────────────────────────────────────────────────────────────
// Section 9 — P-14 non-vacuous consecutive-turn history keystone
// ─────────────────────────────────────────────────────────────────────

/// A provider that records the API messages it receives on each turn, then
/// streams the golden text and ends. The recorded per-turn snapshots let the
/// test assert — outside the spawned task — that conversation history
/// accumulates across prompts (the DN-1 / P-14 contract). Assertions are
/// post-hoc (not inside the provider) so an amnesia regression surfaces as a
/// clean test failure rather than a swallowed task panic.
struct HistoryAssertingProvider {
    /// Per-turn API-message snapshot, sent over an unbounded channel so the
    /// test can assert history accumulation outside the spawned turn task.
    observed_tx: mpsc::UnboundedSender<Vec<rustain::domain::models::Message>>,
}

#[async_trait::async_trait]
impl rustain::domain::ports::StreamingProvider for HistoryAssertingProvider {
    async fn stream_completion(
        &self,
        messages: Vec<rustain::domain::models::Message>,
        _options: rustain::domain::models::CompletionOptions,
    ) -> Result<
        futures::stream::BoxStream<'static, rustain::domain::models::StreamChunk>,
        rustain::domain::errors::ProviderError,
    > {
        // Record exactly what run_turn fed us — the history under test. The
        // unbounded send never blocks, so this needs no lock and never panics.
        let _ = self.observed_tx.send(messages);
        use futures::StreamExt as _;
        use rustain::domain::models::{StopReason, StreamChunk};
        Ok(futures::stream::iter(vec![
            StreamChunk::Text {
                content: GOLDEN_AGENT_TEXT.to_string(),
                parent_tool_use_id: None,
            },
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            },
        ])
        .boxed())
    }

    async fn abort(&self) -> Result<(), rustain::domain::errors::ProviderError> {
        Ok(())
    }
    fn provider_id(&self) -> String {
        "history-asserting".to_string()
    }
    fn list_models(&self) -> Vec<rustain::domain::models::provider::ModelDescriptor> {
        Vec::new()
    }
    async fn health_check(&self) -> Result<(), rustain::domain::errors::ProviderError> {
        Ok(())
    }
    async fn connectivity_probe(
        &self,
    ) -> Result<rustain::domain::ports::ProbeOutcome, rustain::domain::errors::ProviderError> {
        Ok(rustain::domain::ports::ProbeOutcome {
            latency: std::time::Duration::ZERO,
        })
    }
    fn provider_descriptor(&self) -> rustain::domain::models::provider::ProviderDescriptor {
        rustain::domain::models::provider::ProviderDescriptor {
            provider_id: "history-asserting".to_string(),
            healthy: true,
            model_count: 0,
            display_name: "history-asserting".to_string(),
        }
    }
}

/// Build a `CliCore` whose provider records per-turn API messages into `observed`.
fn make_history_core(
    workspace: &Path,
    observed_tx: mpsc::UnboundedSender<Vec<rustain::domain::models::Message>>,
) -> CliCore {
    use rustain::adapters::filesystem::FileSystemStorage;
    use rustain::adapters::noop::{
        NoOpApprovalPersistence, NoOpSecurity, NoOpToolSet, NoOpUsageLedger,
    };
    use rustain::domain::services::approval_runtime::ApprovalRuntime;
    use rustain::domain::services::tool_scheduler::ToolScheduler;
    let provider: Arc<dyn rustain::domain::ports::StreamingProvider> =
        Arc::new(HistoryAssertingProvider { observed_tx });
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

/// Build a `session/prompt` request carrying an arbitrary text prompt.
fn acp_prompt_req_with_text(id: serde_json::Value, session_id: &str, text: &str) -> String {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "session/prompt",
        "params": {
            "sessionId": session_id,
            "prompt": [ { "type": "text", "text": text } ],
        }
    });
    serde_json::to_string(&body).expect("prompt request must serialize")
}

/// Count user-role API messages whose content contains `needle`.
fn count_user_messages_containing(
    messages: &[rustain::domain::models::Message],
    needle: &str,
) -> usize {
    use rustain::domain::models::MessageRole;
    messages
        .iter()
        .filter(|m| m.role == MessageRole::User && m.content.contains(needle))
        .count()
}

/// P-14 keystone: consecutive `session/prompt` turns in ONE ACP session
/// accumulate conversation history — the turn-2 provider input includes the
/// turn-1 user prompt, proving the DN-1 fix (load_conversation before each
/// prompt) de-amnesiaced multi-turn ACP. This is the non-vacuous replacement
/// for the prior "two prompts don't return Invalid params" test, which could
/// not tell amnesia from success.
///
/// Non-vacuity:
/// * Uses an ASSERTING provider that records the real API messages each turn
///   (the scripted provider ignores them, so it could never catch amnesia).
/// * Turn-1 input must contain the turn-1 prompt and NOT the turn-2 prompt.
/// * Turn-2 input must contain BOTH the turn-1 prompt (accumulated history)
///   AND the turn-2 prompt — a per-prompt fresh-history mutant reddens the
///   "turn-1 prompt present on turn 2" assertion.
/// * Two distinct, greppable prompts ("alpha first turn" / "bravo second turn")
///   make the cross-turn presence check unambiguous.
///
/// Determinism: scripted provider (no network), in-memory duplex transport
/// (no listener), deterministic first session id.
#[tokio::test(flavor = "current_thread")]
async fn acp_consecutive_turns_accumulate_conversation_history() {
    use std::rc::Rc;
    use std::time::Duration;

    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
    use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

    use rustain::adapters::acp::agent::CoreFactory;
    use rustain::adapters::acp::run::serve_acp_with_core_factory;
    use rustain::domain::models::AppConfig;

    #[cfg(feature = "test-instrumentation")]
    let _run_turn_guard = ACP_RUN_TURN_TEST_LOCK.lock().await;

    const TURN1_PROMPT: &str = "alpha first turn";
    const TURN2_PROMPT: &str = "bravo second turn";

    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let ws_for_factory = workspace.path().to_path_buf();
    let (observed_tx, mut observed_rx) =
        mpsc::unbounded_channel::<Vec<rustain::domain::models::Message>>();
    let observed_tx_for_factory = observed_tx.clone();

    let (server_incoming, mut client_write) = tokio::io::duplex(16 * 1024);
    let (mut client_read, server_outgoing) = tokio::io::duplex(16 * 1024);

    let core_factory: CoreFactory = Rc::new(move |_cwd: &Path| {
        Ok(make_history_core(
            &ws_for_factory,
            observed_tx_for_factory.clone(),
        ))
    });
    let server = serve_acp_with_core_factory(
        server_outgoing.compat_write(),
        server_incoming.compat(),
        AppConfig::default(),
        workspace.path().to_path_buf(),
        None,
        core_factory,
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
            "params": { "cwd": workspace.path(), "mcpServers": [] },
        });
        client_write
            .write_all(line(session_new.to_string()).as_bytes())
            .await
            .expect("write session/new");
        client_write.flush().await.expect("flush handshake");

        // One framed reader shared across both turns — each turn writes its
        // prompt, flushes, then drains frames until its own prompt result id.
        let reader = tokio::io::BufReader::new(&mut client_read);
        let mut lines = reader.lines();

        // Turn 1: write prompt, drain frames until id == 3.
        client_write
            .write_all(
                line(acp_prompt_req_with_text(
                    serde_json::json!(3),
                    "acp-1",
                    TURN1_PROMPT,
                ))
                .as_bytes(),
            )
            .await
            .expect("write turn-1 prompt");
        client_write.flush().await.expect("flush turn 1");
        loop {
            let raw = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
                .await
                .expect("timeout waiting for turn-1 ACP frame")
                .expect("read error on ACP stream")
                .expect("ACP stream ended before turn-1 result");
            let v: serde_json::Value = serde_json::from_str(&raw).expect("ACP frame JSON");
            if v["id"] == serde_json::json!(3) {
                assert!(
                    v.get("error").is_none(),
                    "turn-1 ACP prompt returned an error: {v}"
                );
                assert_eq!(
                    v["result"]["stopReason"],
                    serde_json::json!("end_turn"),
                    "turn-1 ACP prompt must resolve with stopReason `end_turn`, got: {v}"
                );
                break;
            }
        }

        // Turn 2: write prompt, drain frames until id == 4.
        client_write
            .write_all(
                line(acp_prompt_req_with_text(
                    serde_json::json!(4),
                    "acp-1",
                    TURN2_PROMPT,
                ))
                .as_bytes(),
            )
            .await
            .expect("write turn-2 prompt");
        client_write.flush().await.expect("flush turn 2");
        loop {
            let raw = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
                .await
                .expect("timeout waiting for turn-2 ACP frame")
                .expect("read error on ACP stream")
                .expect("ACP stream ended before turn-2 result");
            let v: serde_json::Value = serde_json::from_str(&raw).expect("ACP frame JSON");
            if v["id"] == serde_json::json!(4) {
                assert!(
                    v.get("error").is_none(),
                    "turn-2 ACP prompt returned an error: {v}"
                );
                assert_eq!(
                    v["result"]["stopReason"],
                    serde_json::json!("end_turn"),
                    "turn-2 ACP prompt must resolve with stopReason `end_turn`, got: {v}"
                );
                break;
            }
        }
    };

    tokio::select! {
        _ = drive => {}
        server_res = server => {
            panic!("ACP server exited before consecutive prompts completed: {server_res:?}");
        }
    }

    // ── Assert history accumulation from the recorded per-turn snapshots. ──
    // The provider sent each turn's API messages over the channel before the
    // turn result reached us, so both snapshots are buffered now.
    let mut snapshots: Vec<Vec<rustain::domain::models::Message>> = Vec::new();
    while let Ok(messages) = observed_rx.try_recv() {
        snapshots.push(messages);
    }
    assert_eq!(
        snapshots.len(),
        2,
        "exactly two provider turns must have been recorded"
    );
    let turn1 = &snapshots[0];
    let turn2 = &snapshots[1];

    // Turn 1: sees its own prompt, not turn 2's.
    assert_eq!(
        count_user_messages_containing(turn1, TURN1_PROMPT),
        1,
        "turn-1 provider input must include the turn-1 user prompt exactly once"
    );
    assert_eq!(
        count_user_messages_containing(turn1, TURN2_PROMPT),
        0,
        "turn-1 provider input must NOT contain the turn-2 prompt (no look-ahead)"
    );

    // Turn 2: history accumulated — the turn-1 prompt is still present.
    assert_eq!(
        count_user_messages_containing(turn2, TURN1_PROMPT),
        1,
        "turn-2 provider input must include the turn-1 user prompt (accumulated history)"
    );
    assert_eq!(
        count_user_messages_containing(turn2, TURN2_PROMPT),
        1,
        "turn-2 provider input must include the turn-2 user prompt exactly once"
    );
}

// ─────────────────────────────────────────────────────────────────────
// Section 10 — P-15 approval routing-matrix regression
// ─────────────────────────────────────────────────────────────────────

/// Drive a real tool-call turn through the dispatched ACP JSON-RPC path and
/// capture the `ApprovalSource` the live `ApprovalRuntime` broadcasts for the
/// tool permission request. The ACP approval bridge subscribes to the same
/// runtime and forwards the request as `session/request_permission`; this
/// helper subscribes a SECOND receiver and drains it inline (non-blocking
/// `try_recv`) while reading the ACP frames, so the `source` is observed
/// without a separate spawned task. It allows the tool so the turn completes.
///
/// Acquires `ACP_RUN_TURN_TEST_LOCK` itself; callers MUST NOT hold it (the
/// mutex is not re-entrant — a caller holding it would deadlock here).
async fn drive_acp_capture_approval_source() -> rustain::domain::models::tool_call::ApprovalSource {
    use std::rc::Rc;
    use std::time::Duration;

    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
    use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

    use rustain::adapters::acp::agent::CoreFactory;
    use rustain::adapters::acp::run::serve_acp_with_core_factory;
    use rustain::adapters::noop::NoOpApprovalPersistence;
    use rustain::domain::models::AppConfig;
    use rustain::domain::models::tool_call::ApprovalSource;
    use rustain::domain::services::approval_runtime::{ApprovalRuntime, ApprovalRuntimeEvent};

    #[cfg(feature = "test-instrumentation")]
    let _run_turn_guard = ACP_RUN_TURN_TEST_LOCK.lock().await;

    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let ws_for_factory = workspace.path().to_path_buf();
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let executions_for_factory = executions.clone();
    let approval = ApprovalRuntime::new(64, Arc::new(NoOpApprovalPersistence));
    let approval_for_factory = approval.clone();

    // Subscribe BEFORE the turn so the first `Requested` is observed. The ACP
    // bridge subscribes again inside run_prompt; broadcast delivers to both.
    let mut sub = approval.subscribe();

    let (server_incoming, mut client_write) = tokio::io::duplex(16 * 1024);
    let (mut client_read, server_outgoing) = tokio::io::duplex(16 * 1024);

    let core_factory: CoreFactory = Rc::new(move |_cwd: &Path| {
        Ok(make_core_with_sentinel(
            &ws_for_factory,
            executions_for_factory.clone(),
            approval_for_factory.clone(),
        ))
    });

    let server = serve_acp_with_core_factory(
        server_outgoing.compat_write(),
        server_incoming.compat(),
        AppConfig::default(),
        workspace.path().to_path_buf(),
        None,
        core_factory,
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
            "params": { "cwd": workspace.path(), "mcpServers": [] },
        });
        client_write
            .write_all(line(session_new.to_string()).as_bytes())
            .await
            .expect("write session/new");
        client_write
            .write_all(line(acp_prompt_req(serde_json::json!(3), "acp-1")).as_bytes())
            .await
            .expect("write prompt");
        client_write.flush().await.expect("flush requests");

        let reader = tokio::io::BufReader::new(&mut client_read);
        let mut lines = reader.lines();
        let mut captured: Option<ApprovalSource> = None;
        loop {
            // Non-blocking drain of the approval broadcast: capture the live
            // `ApprovalSource` the tool scheduler broadcast for the permission
            // request. `Requested` is broadcast before the bridge forwards
            // `session/request_permission`, so it is buffered in our receiver
            // by the time we read that frame — and we drain every iteration.
            while let Ok(event) = sub.try_recv() {
                if let ApprovalRuntimeEvent::Requested { source, .. } = event {
                    captured = Some(source);
                }
            }
            let raw = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
                .await
                .expect("timeout waiting for ACP frame")
                .expect("read error on ACP stream")
                .expect("ACP stream ended before prompt result");
            let v: serde_json::Value = serde_json::from_str(&raw).expect("ACP frame JSON");
            if v["method"] == serde_json::json!("session/request_permission") {
                assert_eq!(
                    v["params"]["sessionId"],
                    serde_json::json!("acp-1"),
                    "permission request must be scoped to the ACP session"
                );
                let id = v["id"].clone();
                let response = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "outcome": {
                            "outcome": "selected",
                            "optionId": rustain::adapters::acp::translate::PERMISSION_ALLOW_ONCE,
                        }
                    }
                });
                client_write
                    .write_all(line(response.to_string()).as_bytes())
                    .await
                    .expect("write permission response");
                client_write
                    .flush()
                    .await
                    .expect("flush permission response");
                continue;
            }
            if v["id"] == serde_json::json!(3) {
                return captured;
            }
        }
    };

    let captured = tokio::select! {
        src = drive => src,
        server_res = server => {
            panic!("ACP server exited before the routing-matrix turn completed: {server_res:?}");
        }
    };

    captured.expect("no ApprovalRuntimeEvent::Requested observed — ACP approval routing did not fire through the ApprovalRuntime")
}

/// P-15 routing-matrix regression: a tool-call turn driven through the real
/// dispatched ACP path routes its permission request through
/// `ApprovalSource::AcpSession` — NOT `ForegroundTurn` (and not a subagent
/// source). The source is captured from the LIVE `ApprovalRuntime` broadcast
/// (not constructed in the test body), so a mutant that files the ACP turn
/// under the wrong source reddens the match.
///
/// Also defends the DD1 node-binding guarantee on the live path: `AcpSession`'s
/// `scope_agent_id` must use the length-prefixed join, so it cannot equal a
/// bare `"{conversation_id}/{session_id}"` delimiter join (the collision-prone
/// form). A mutant that drops the length prefix reddens the `assert_ne!`.
///
/// Non-vacuity: the turn drives a real side-effecting builtin tool through the
/// dispatched JSON-RPC path (the same harness as the AC3 contrast), and the
/// source is read off the runtime the tool scheduler actually broadcasts to.
#[tokio::test(flavor = "current_thread")]
async fn acp_approval_source_routing_matrix() {
    use rustain::domain::models::tool_call::ApprovalSource;

    // NOTE: `ACP_RUN_TURN_TEST_LOCK` is acquired INSIDE
    // `drive_acp_capture_approval_source` — do NOT acquire it here too, the
    // tokio mutex is not re-entrant and a second acquisition deadlocks.
    let source = drive_acp_capture_approval_source().await;

    // Routing: the ACP turn must file under AcpSession, NOT ForegroundTurn.
    let (session_id, conversation_id) = match source {
        ApprovalSource::AcpSession {
            ref session_id,
            ref conversation_id,
        } => (session_id.clone(), conversation_id.clone()),
        ref other => panic!(
            "ACP tool-call turn must route through ApprovalSource::AcpSession, got {other:?}"
        ),
    };
    assert_eq!(
        session_id, "acp-1",
        "the ACP approval source must carry the live session id `acp-1`"
    );
    assert!(
        !conversation_id.is_empty(),
        "the ACP approval source must carry the editor conversation id, not be empty"
    );

    // DD1 node-binding on the live path: scope must be length-prefixed, so it
    // cannot equal a bare delimiter join of the two components.
    let scope = source.scope_agent_id().0;
    let bare_join = format!("{conversation_id}/{session_id}");
    assert_ne!(
        scope, bare_join,
        "AcpSession.scope_agent_id must use length_prefixed_scope, not a bare delimiter join"
    );
}
