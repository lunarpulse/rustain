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
        matches!(cli.command, Some(Command::Acp { .. })),
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
async fn acp_session_new_forwards_mcp_servers_to_core_factory() {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::Duration;

    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
    use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

    use rustain::adapters::acp::agent::CoreFactory;
    use rustain::adapters::acp::run::serve_acp_with_core_factory;
    use rustain::domain::models::{AppConfig, McpServerSpec, McpTransport};

    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let ws_for_factory = workspace.path().to_path_buf();
    let observed: Rc<RefCell<Vec<Vec<McpServerSpec>>>> = Rc::new(RefCell::new(Vec::new()));
    let observed_for_factory = observed.clone();

    let (server_incoming, mut client_write) = tokio::io::duplex(8 * 1024);
    let (client_read, server_outgoing) = tokio::io::duplex(8 * 1024);
    let mut client_reader = tokio::io::BufReader::new(client_read);

    let core_factory: CoreFactory = Rc::new(move |_cwd: &Path, mcp_servers| {
        observed_for_factory.borrow_mut().push(mcp_servers.to_vec());
        Ok(make_core(&ws_for_factory))
    });

    let server = serve_acp_with_core_factory(
        server_outgoing.compat_write(),
        server_incoming.compat(),
        AppConfig::default(),
        workspace.path().to_path_buf(),
        None,
        core_factory,
    );

    // P6 — give the AC3 literal-env assertion teeth. `RUSTAIN_ACP_LITERAL` is
    // SET to a sentinel so a mutant routing the value through
    // `expand_env_vars` would substitute the sentinel (and the literal-token
    // assertion reddens). With the var UNSET, `expand_env_vars` leaves unknown
    // vars literal and the bug would pass. Unique var; restored on exit.
    const PROBE_VAR: &str = "RUSTAIN_ACP_LITERAL";
    const SENTINEL: &str = "RUSTAIN_ACP_LITERAL_EXPANDED_SENTINEL_14_10";
    // SAFETY: PROBE_VAR is unique to this test; the function under test
    // (mcp_servers_from_acp) never reads the process env. Restored on exit.
    unsafe { std::env::set_var(PROBE_VAR, SENTINEL) };
    let _probe_guard = scopeguard::guard((), |_| unsafe { std::env::remove_var(PROBE_VAR) });

    let drive = async {
        client_write
            .write_all(line(ACP_INITIALIZE_REQ.to_string()).as_bytes())
            .await
            .expect("write initialize");
        let empty_mcp_session = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": { "cwd": workspace.path(), "mcpServers": [] },
        });
        client_write
            .write_all(line(empty_mcp_session.to_string()).as_bytes())
            .await
            .expect("write empty MCP session/new");
        let stdio_mcp_session = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "session/new",
            "params": {
                "cwd": workspace.path(),
                "mcpServers": [{
                    "type": "stdio",
                    "name": "fixture server",
                    "command": "/bin/echo",
                    "args": ["--flag"],
                    "env": [{ "name": "TOKEN", "value": "${RUSTAIN_ACP_LITERAL}" }]
                }]
            },
        });
        client_write
            .write_all(line(stdio_mcp_session.to_string()).as_bytes())
            .await
            .expect("write stdio MCP session/new");
        client_write.flush().await.expect("flush requests");

        let mut responses = Vec::new();
        // P8 — read until the session/new id:3 result arrives, tolerating
        // interleaved `available_commands_update` notifications (which carry no
        // `id`) emitted by the always-on `/init` advertisement. A fixed 3-line
        // read would break if a deferred notification lands before id:3.
        let deadline = std::time::Instant::now() + Duration::from_secs(4);
        loop {
            if responses
                .iter()
                .any(|value: &serde_json::Value| value.get("id") == Some(&serde_json::json!(3)))
            {
                break;
            }
            if std::time::Instant::now() >= deadline {
                panic!("timed out waiting for session/new id:3 result; got {responses:?}");
            }
            let mut line = String::new();
            tokio::time::timeout(Duration::from_secs(2), client_reader.read_line(&mut line))
                .await
                .expect("timed out waiting for ACP response")
                .expect("read ACP response");
            responses.push(serde_json::from_str::<serde_json::Value>(&line).expect("json line"));
        }
        assert!(
            responses
                .iter()
                .any(|value| value.get("id") == Some(&serde_json::json!(3))),
            "session/new with MCP server should complete successfully: {responses:?}"
        );
    };

    tokio::select! {
        result = server => panic!("ACP server exited before MCP forwarding assertions: {result:?}"),
        _ = drive => {}
    }

    let observed = observed.borrow();
    assert_eq!(
        observed.len(),
        2,
        "core factory should be called once per session/new"
    );
    assert!(
        observed[0].is_empty(),
        "empty mcpServers must forward an empty slice"
    );
    assert_eq!(
        observed[1].len(),
        1,
        "stdio mcp server must reach the core factory"
    );
    let spec = &observed[1][0];
    assert_eq!(spec.id, "fixture_server");
    assert_eq!(spec.transport, McpTransport::Stdio);
    assert_eq!(spec.command.as_deref(), Some("/bin/echo"));
    assert_eq!(spec.args, vec!["--flag"]);
    assert_eq!(
        spec.env.get("TOKEN").map(String::as_str),
        Some("${RUSTAIN_ACP_LITERAL}"),
        "ACP env values must be forwarded literally — the probe var \
         `RUSTAIN_ACP_LITERAL` is SET in the process env, so an expansion \
         mutant would substitute the sentinel here"
    );
}

#[derive(Clone)]
struct StaticAuthStore {
    statuses: Vec<rustain::domain::models::credential::ProviderStatus>,
}

#[async_trait::async_trait]
impl rustain::domain::ports::AuthStorePort for StaticAuthStore {
    async fn get(
        &self,
        _provider: &str,
    ) -> Result<
        Option<rustain::domain::models::credential::Credential>,
        rustain::domain::errors::AuthError,
    > {
        Ok(None)
    }

    async fn set(
        &self,
        _provider: &str,
        _cred: rustain::domain::models::credential::Credential,
    ) -> Result<(), rustain::domain::errors::AuthError> {
        Ok(())
    }

    async fn remove(&self, _provider: &str) -> Result<(), rustain::domain::errors::AuthError> {
        Ok(())
    }

    async fn list(
        &self,
    ) -> Result<
        Vec<rustain::domain::models::credential::ProviderStatus>,
        rustain::domain::errors::AuthError,
    > {
        Ok(self.statuses.clone())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn acp_initialize_advertises_auth_methods_without_secret_leaks() {
    use std::rc::Rc;
    use std::time::Duration;

    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
    use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

    use rustain::adapters::acp::run::serve_acp_with_acp_core_factory_and_node_tree_and_auth_store;
    use rustain::domain::models::AppConfig;
    use rustain::infrastructure::composition::AcpCore;
    use rustain::infrastructure::subagent::NodeTree;

    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let ws_for_factory = workspace.path().to_path_buf();
    let (server_incoming, mut client_write) = tokio::io::duplex(8 * 1024);
    let (mut client_read, server_outgoing) = tokio::io::duplex(8 * 1024);
    let secret_canary = "sk-auth-method-secret-must-not-leak";
    unsafe { std::env::set_var("OPENAI_API_KEY", secret_canary) };
    let _guard = scopeguard::guard((), |_| unsafe { std::env::remove_var("OPENAI_API_KEY") });

    let factory: rustain::adapters::acp::agent::AcpCoreFactory =
        Rc::new(move |_cwd, _mcp_servers| Ok(AcpCore::from(make_core(&ws_for_factory))));
    let server = serve_acp_with_acp_core_factory_and_node_tree_and_auth_store(
        server_outgoing.compat_write(),
        server_incoming.compat(),
        AppConfig::default(),
        None,
        factory,
        NodeTree::with_now_fn(Arc::new(|| 1_700_000_000_000)),
        workspace.path().to_path_buf(),
        rustain::adapters::acp::run::deterministic_acp_id_source(),
        Arc::new(StaticAuthStore {
            statuses: Vec::new(),
        }),
    );

    let drive = async {
        client_write
            .write_all(line(ACP_INITIALIZE_REQ.to_string()).as_bytes())
            .await
            .expect("write initialize");
        client_write.flush().await.expect("flush initialize");

        let mut raw = String::new();
        tokio::time::timeout(
            Duration::from_secs(10),
            tokio::io::BufReader::new(&mut client_read).read_line(&mut raw),
        )
        .await
        .expect("timeout waiting for initialize response")
        .expect("read initialize response");
        let response: serde_json::Value = serde_json::from_str(&raw).expect("initialize JSON");
        assert_eq!(response["id"], serde_json::json!(1));
        assert!(
            !raw.contains(secret_canary),
            "initialize response must never leak configured secret bytes: {raw}"
        );
        let methods = response["result"]["authMethods"]
            .as_array()
            .expect("initialize authMethods array");
        let ids: Vec<&str> = methods
            .iter()
            .filter_map(|method| method["id"].as_str())
            .collect();
        assert!(
            ids.contains(&"anthropic"),
            "authMethods must include Anthropic: {methods:?}"
        );
        assert!(
            ids.contains(&"openai"),
            "authMethods must include OpenAI: {methods:?}"
        );
        assert!(
            !ids.contains(&"ollama"),
            "keyless Ollama must not be advertised as an auth method: {methods:?}"
        );
        assert!(
            methods.iter().all(|method| method["description"]
                .as_str()
                .is_some_and(|description| description.contains("rustain auth login"))),
            "auth method descriptions must explain rustain-owned login: {methods:?}"
        );
        let caps = &response["result"]["agentCapabilities"];
        assert_eq!(
            caps["loadSession"],
            serde_json::json!(true),
            "initialize must preserve loadSession lifecycle capability"
        );
        assert!(
            caps["sessionCapabilities"]["list"].is_object(),
            "initialize must preserve session/list capability: {caps}"
        );
        assert!(
            caps["sessionCapabilities"]["resume"].is_object(),
            "initialize must preserve session/resume capability: {caps}"
        );
        assert!(
            caps["sessionCapabilities"]["close"].is_object(),
            "initialize must preserve session/close capability: {caps}"
        );
        assert_eq!(
            caps["promptCapabilities"]["image"],
            serde_json::json!(true),
            "Task 5 shipped image passthrough, so initialize must honestly advertise image prompt support"
        );
        assert!(
            caps["promptCapabilities"].get("embeddedContext").is_none()
                || caps["promptCapabilities"]["embeddedContext"] == serde_json::json!(false),
            "embeddedContext must stay false/absent because rustain does not embed resource contents: {caps}"
        );
        assert!(
            caps["promptCapabilities"].get("audio").is_none()
                || caps["promptCapabilities"]["audio"] == serde_json::json!(false),
            "audio must stay false/absent because ACP audio passthrough is not implemented: {caps}"
        );
        assert!(
            caps["mcpCapabilities"].get("http").is_none()
                || caps["mcpCapabilities"]["http"] == serde_json::json!(false),
            "MCP HTTP must stay false/absent because rustain forwards stdio MCP only: {caps}"
        );
        assert!(
            caps["mcpCapabilities"].get("sse").is_none()
                || caps["mcpCapabilities"]["sse"] == serde_json::json!(false),
            "MCP SSE must stay false/absent because rustain forwards stdio MCP only: {caps}"
        );
    };

    tokio::select! {
        _ = drive => {}
        server_res = server => panic!("ACP server exited before initialize auth assertion: {server_res:?}"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn acp_authenticate_checks_credential_presence() {
    use std::rc::Rc;
    use std::time::Duration;

    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
    use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

    use rustain::adapters::acp::run::serve_acp_with_acp_core_factory_and_node_tree_and_auth_store;
    use rustain::domain::models::AppConfig;
    use rustain::domain::models::credential::{AuthSource, AuthStatus, ProviderStatus};
    use rustain::infrastructure::composition::AcpCore;
    use rustain::infrastructure::subagent::NodeTree;

    // P7 — hermeticity: the "missing credential" case must NOT be satisfied by
    // an ambient `MOONSHOT_API_KEY` in the developer's shell (`detect_source`
    // checks the process env before the auth store). Save + remove it for the
    // duration of the test and restore the prior value on exit.
    let prior_moonshot = std::env::var("MOONSHOT_API_KEY").ok();
    if prior_moonshot.is_some() {
        // SAFETY: process-global mutation; this is the only test in the binary
        // reading `MOONSHOT_API_KEY`, the runtime is single-threaded, and the
        // prior value is restored on scope exit.
        unsafe { std::env::remove_var("MOONSHOT_API_KEY") };
    }
    let _moonshot_guard = scopeguard::guard(prior_moonshot, |prior| {
        if let Some(v) = prior {
            // SAFETY: same uniqueness rationale.
            unsafe { std::env::set_var("MOONSHOT_API_KEY", v) };
        }
    });

    let run_case = |statuses: Vec<ProviderStatus>| async move {
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let ws_for_factory = workspace.path().to_path_buf();
        let (server_incoming, mut client_write) = tokio::io::duplex(8 * 1024);
        let (mut client_read, server_outgoing) = tokio::io::duplex(8 * 1024);
        let factory: rustain::adapters::acp::agent::AcpCoreFactory =
            Rc::new(move |_cwd, _mcp_servers| Ok(AcpCore::from(make_core(&ws_for_factory))));
        let server = serve_acp_with_acp_core_factory_and_node_tree_and_auth_store(
            server_outgoing.compat_write(),
            server_incoming.compat(),
            AppConfig::default(),
            None,
            factory,
            NodeTree::with_now_fn(Arc::new(|| 1_700_000_000_000)),
            workspace.path().to_path_buf(),
            rustain::adapters::acp::run::deterministic_acp_id_source(),
            Arc::new(StaticAuthStore { statuses }),
        );

        let drive = async {
            client_write
                .write_all(line(ACP_INITIALIZE_REQ.to_string()).as_bytes())
                .await
                .expect("write initialize");
            let auth = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "authenticate",
                "params": { "methodId": "moonshot" },
            });
            client_write
                .write_all(line(auth.to_string()).as_bytes())
                .await
                .expect("write authenticate");
            client_write.flush().await.expect("flush auth");

            let reader = tokio::io::BufReader::new(&mut client_read);
            let mut lines = reader.lines();
            loop {
                let raw = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
                    .await
                    .expect("timeout waiting for auth response")
                    .expect("read auth response")
                    .expect("ACP stream ended before auth response");
                let response: serde_json::Value = serde_json::from_str(&raw).expect("auth JSON");
                if response["id"] == serde_json::json!(2) {
                    return response;
                }
            }
        };

        tokio::select! {
            response = drive => response,
            server_res = server => panic!("ACP server exited before authenticate response: {server_res:?}"),
        }
    };

    let missing = run_case(Vec::new()).await;
    assert!(
        missing.get("error").is_some(),
        "authenticate must fail honestly when no credential exists: {missing}"
    );
    let error_text = missing["error"].to_string();
    assert!(
        error_text.contains("MOONSHOT_API_KEY"),
        "missing-auth error must name env var: {error_text}"
    );
    assert!(
        error_text.contains("rustain auth login moonshot"),
        "missing-auth error must include login hint: {error_text}"
    );

    let success = run_case(vec![ProviderStatus {
        provider: "moonshot".to_string(),
        status: AuthStatus::Authenticated,
        source: AuthSource::AuthJson,
        last_validated: None,
    }])
    .await;
    assert!(
        success.get("result").is_some() && success.get("error").is_none(),
        "authenticate must succeed when auth store reports a credential: {success}"
    );
}

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

    let core_factory: CoreFactory =
        Rc::new(move |_cwd: &Path, _mcp_servers| Ok(make_core(&ws_for_factory)));
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
    let allowed_workspace = workspace.path().to_path_buf();

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

    let core_factory: CoreFactory =
        Rc::new(move |_cwd: &Path, _mcp_servers| Ok(make_core(&ws_for_factory)));

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
        allowed_workspace,
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
        .filter(|e| e.agent_id.as_str().to_string().as_str() == "acp-1")
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
        .filter(|e| e.agent_id.as_str().to_string().as_str() == "acp-1")
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

    let core_factory: CoreFactory = Rc::new(move |_cwd: &Path, _mcp_servers| {
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

    let core_factory: CoreFactory =
        Rc::new(move |_cwd: &Path, _mcp_servers| Ok(make_core(&ws_for_factory)));
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

    let core_factory: CoreFactory = Rc::new(move |_cwd: &Path, _mcp_servers| {
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
        rustain::domain::models::TurnOrigin::Interactive,
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

    let core_factory: CoreFactory =
        Rc::new(move |_cwd: &Path, _mcp_servers| Ok(make_core(&ws_for_factory)));
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
    let allowed_workspace = workspace.path().to_path_buf();
    let (server_incoming, mut client_write) = tokio::io::duplex(16 * 1024);
    let (mut client_read, server_outgoing) = tokio::io::duplex(16 * 1024);

    // Injected NodeTree with a fixed known now_fn — the MockClock stand-in.
    // `42_000` is deliberately small and distinctive so a raw wall-clock stamp
    // or a `0` default can never coincide with it.
    const FIXED_NOW_MS: i64 = 42_000;
    let node_tree = NodeTree::with_now_fn(Arc::new(move || FIXED_NOW_MS));
    let observer = node_tree.clone();

    let core_factory: CoreFactory =
        Rc::new(move |_cwd: &Path, _mcp_servers| Ok(make_core(&ws_for_factory)));

    let mut server = Box::pin(serve_acp_with_core_factory_and_node_tree(
        server_outgoing.compat_write(),
        server_incoming.compat(),
        AppConfig::default(),
        None,
        core_factory,
        node_tree,
        allowed_workspace,
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
        .find(|e| e.agent_id.as_str().to_string().as_str() == "acp-1")
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

    let core_factory: CoreFactory = Rc::new(move |_cwd: &Path, _mcp_servers| {
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

#[tokio::test(flavor = "current_thread")]
async fn acp_init_builtin_expands_before_provider_turn() {
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
    let (observed_tx, mut observed_rx) =
        mpsc::unbounded_channel::<Vec<rustain::domain::models::Message>>();
    let observed_tx_for_factory = observed_tx.clone();

    let (server_incoming, mut client_write) = tokio::io::duplex(16 * 1024);
    let (mut client_read, server_outgoing) = tokio::io::duplex(16 * 1024);

    let core_factory: CoreFactory = Rc::new(move |_cwd: &Path, _mcp_servers| {
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
        client_write
            .write_all(
                line(acp_prompt_req_with_text(
                    serde_json::json!(3),
                    "acp-1",
                    "/init",
                ))
                .as_bytes(),
            )
            .await
            .expect("write /init prompt");
        client_write.flush().await.expect("flush /init transcript");

        let reader = tokio::io::BufReader::new(&mut client_read);
        let mut lines = reader.lines();
        loop {
            let raw = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
                .await
                .expect("timeout waiting for /init ACP frame")
                .expect("read error on ACP stream")
                .expect("ACP stream ended before /init result");
            let v: serde_json::Value = serde_json::from_str(&raw).expect("ACP frame JSON");
            if v["id"] == serde_json::json!(3) {
                assert!(
                    v.get("error").is_none(),
                    "/init ACP prompt returned an error: {v}"
                );
                assert_eq!(
                    v["result"]["stopReason"],
                    serde_json::json!("end_turn"),
                    "/init ACP prompt must run a real turn and end normally"
                );
                break;
            }
        }
    };

    tokio::select! {
        _ = drive => {}
        server_res = server => {
            panic!("ACP server exited before /init prompt completed: {server_res:?}");
        }
    }

    let mut snapshots: Vec<Vec<rustain::domain::models::Message>> = Vec::new();
    while let Ok(messages) = observed_rx.try_recv() {
        snapshots.push(messages);
    }
    assert_eq!(
        snapshots.len(),
        1,
        "exactly one provider turn must be recorded for `/init`"
    );
    let provider_input = &snapshots[0];
    assert_eq!(
        count_user_messages_containing(provider_input, "/init"),
        0,
        "`/init` must expand before provider input; sending the literal slash command is the false-green"
    );
    assert_eq!(
        count_user_messages_containing(provider_input, "create or update a concise context file"),
        1,
        "`/init` must expand into the repository context-file prompt before the turn"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn acp_image_content_reaches_provider_message() {
    use std::rc::Rc;
    use std::time::Duration;

    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
    use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

    use rustain::adapters::acp::agent::CoreFactory;
    use rustain::adapters::acp::run::serve_acp_with_core_factory;
    use rustain::domain::models::AppConfig;

    #[cfg(feature = "test-instrumentation")]
    let _run_turn_guard = ACP_RUN_TURN_TEST_LOCK.lock().await;

    const IMAGE_DATA: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAAB";
    const IMAGE_MIME: &str = "image/png";

    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let ws_for_factory = workspace.path().to_path_buf();
    let (observed_tx, mut observed_rx) =
        mpsc::unbounded_channel::<Vec<rustain::domain::models::Message>>();
    let observed_tx_for_factory = observed_tx.clone();

    let (server_incoming, mut client_write) = tokio::io::duplex(16 * 1024);
    let (mut client_read, server_outgoing) = tokio::io::duplex(16 * 1024);

    let core_factory: CoreFactory = Rc::new(move |_cwd: &Path, _mcp_servers| {
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
        let prompt = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "session/prompt",
            "params": {
                "sessionId": "acp-1",
                "prompt": [
                    { "type": "text", "text": "describe this image" },
                    { "type": "image", "data": IMAGE_DATA, "mimeType": IMAGE_MIME }
                ]
            }
        });
        client_write
            .write_all(line(prompt.to_string()).as_bytes())
            .await
            .expect("write image prompt");
        client_write.flush().await.expect("flush image transcript");

        let reader = tokio::io::BufReader::new(&mut client_read);
        let mut lines = reader.lines();
        loop {
            let raw = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
                .await
                .expect("timeout waiting for image ACP frame")
                .expect("read error on ACP stream")
                .expect("ACP stream ended before image result");
            let v: serde_json::Value = serde_json::from_str(&raw).expect("ACP frame JSON");
            if v["id"] == serde_json::json!(3) {
                assert!(
                    v.get("error").is_none(),
                    "image ACP prompt returned an error: {v}"
                );
                assert_eq!(
                    v["result"]["stopReason"],
                    serde_json::json!("end_turn"),
                    "image ACP prompt must run a real turn and end normally"
                );
                break;
            }
        }
    };

    tokio::select! {
        _ = drive => {}
        server_res = server => {
            panic!("ACP server exited before image prompt completed: {server_res:?}");
        }
    }

    let mut snapshots: Vec<Vec<rustain::domain::models::Message>> = Vec::new();
    while let Ok(messages) = observed_rx.try_recv() {
        snapshots.push(messages);
    }
    assert_eq!(
        snapshots.len(),
        1,
        "exactly one provider turn must be recorded for image prompt"
    );
    let user_message = snapshots[0]
        .iter()
        .find(|message| {
            message.role == rustain::domain::models::MessageRole::User
                && message.content.contains("describe this image")
        })
        .expect("provider input should include the image prompt user message");
    assert_eq!(
        user_message.images.len(),
        1,
        "image block must reach provider input"
    );
    assert_eq!(user_message.images[0].media_type, IMAGE_MIME);
    assert_eq!(user_message.images[0].data, IMAGE_DATA);
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

    let core_factory: CoreFactory = Rc::new(move |_cwd: &Path, _mcp_servers| {
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
    let scope = source.scope_agent_id().as_str().to_string();
    let bare_join = format!("{conversation_id}/{session_id}");
    assert_ne!(
        scope, bare_join,
        "AcpSession.scope_agent_id must use length_prefixed_scope, not a bare delimiter join"
    );
}

/// Count assistant-role API messages whose content contains `needle`.
fn count_assistant_messages_containing(
    messages: &[rustain::domain::models::Message],
    needle: &str,
) -> usize {
    use rustain::domain::models::MessageRole;
    messages
        .iter()
        .filter(|m| m.role == MessageRole::Assistant && m.content.contains(needle))
        .count()
}

/// AC7 keystone — the kill→reload contract test (the feature proof).
///
/// Phase A: a real ACP server over an in-memory duplex drives `initialize →
/// session/new → 2× session/prompt`. The scripted provider answers every turn
/// with `GOLDEN_AGENT_TEXT`. Then the client write-half is DROPPED — the server
/// sees EOF and tears down (the "process kill"; the on-disk store survives).
///
/// Phase B: a FRESH server is stood up over a new duplex pointed at the SAME
/// workspace `TempDir`. `initialize → session/load(id-from-A) → session/prompt`.
/// The Phase-B provider input is captured and asserted to contain BOTH prior
/// user prompts AND the prior ASSISTANT responses (`GOLDEN_AGENT_TEXT`).
///
/// Non-vacuity (the F0 discriminator): asserting ONLY user prompts would pass
/// even without Task-1's post-turn save (the pre-turn save already persists
/// those). The net-new proof is that the model's *answers* survived the kill —
/// counted as ASSISTANT-role messages containing `GOLDEN_AGENT_TEXT` (expect 2).
/// A mutant that drops Task-1's post-turn save reddens this count (0 assistant
/// answers reloaded). This is why the story says "absent this test the story is
/// NOT done".
#[tokio::test(flavor = "current_thread")]
async fn acp_kill_reload_preserves_assistant_turns() {
    use std::rc::Rc;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
    use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

    use rustain::adapters::acp::agent::CoreFactory;
    use rustain::adapters::acp::run::serve_acp_with_core_factory;
    use rustain::domain::models::AppConfig;
    use rustain::domain::models::MessageRole;

    #[cfg(feature = "test-instrumentation")]
    let _run_turn_guard = ACP_RUN_TURN_TEST_LOCK.lock().await;

    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let ws = workspace.path().to_path_buf();

    // Read JSON-RPC lines from a BufReader until the result for `id` arrives
    // with `stopReason == end_turn` (notifications are ignored). Sequential —
    // the SDK dispatches each request concurrently, so a client MUST await one
    // prompt's result before sending the next (mirrors a real editor).
    async fn await_end_turn(
        lines: &mut tokio::io::Lines<tokio::io::BufReader<&mut tokio::io::DuplexStream>>,
        id: serde_json::Value,
    ) -> bool {
        loop {
            let next = tokio::time::timeout(Duration::from_secs(10), lines.next_line()).await;
            let raw = match next {
                Ok(Ok(Some(l))) => l,
                _ => return false,
            };
            let v: serde_json::Value = match serde_json::from_str(&raw) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if v["id"] == id && v["result"]["stopReason"] == serde_json::json!("end_turn") {
                return true;
            }
        }
    }

    // ── Phase A: create + 2 turns, then EOF-"kill" ─────────────────────
    let session_id = {
        let (server_incoming, mut client_write) = tokio::io::duplex(8 * 1024);
        let (mut client_read, server_outgoing) = tokio::io::duplex(8 * 1024);
        let ws_for_factory = ws.clone();
        let core_factory: CoreFactory =
            Rc::new(move |_: &Path, _mcp_servers| Ok(make_core(&ws_for_factory)));
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
            let new = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "session/new",
                "params": { "cwd": workspace.path(), "mcpServers": [] },
            });
            client_write
                .write_all(line(new.to_string()).as_bytes())
                .await
                .expect("write session/new");
            client_write.flush().await.expect("flush A init/new");

            let reader = tokio::io::BufReader::new(&mut client_read);
            let mut lines = reader.lines();
            // Capture the new session id from the id-2 result.
            let mut session_id: Option<String> = None;
            loop {
                let next = tokio::time::timeout(Duration::from_secs(10), lines.next_line()).await;
                let raw = match next {
                    Ok(Ok(Some(l))) => l,
                    _ => break,
                };
                let v: serde_json::Value = match serde_json::from_str(&raw) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if v["id"] == serde_json::json!(2) {
                    if let Some(s) = v["result"]["sessionId"].as_str() {
                        session_id = Some(s.to_string());
                    }
                    break;
                }
            }
            let session_id = session_id.expect("session/new returned a sessionId");

            // Prompt 1, then await its result BEFORE sending prompt 2.
            client_write
                .write_all(
                    line(acp_prompt_req_with_text(
                        serde_json::json!(3),
                        &session_id,
                        "alpha first turn",
                    ))
                    .as_bytes(),
                )
                .await
                .expect("write prompt 1");
            client_write.flush().await.expect("flush A p1");
            let got_p3 = await_end_turn(&mut lines, serde_json::json!(3)).await;

            client_write
                .write_all(
                    line(acp_prompt_req_with_text(
                        serde_json::json!(4),
                        &session_id,
                        "bravo second turn",
                    ))
                    .as_bytes(),
                )
                .await
                .expect("write prompt 2");
            client_write.flush().await.expect("flush A p2");
            let got_p4 = await_end_turn(&mut lines, serde_json::json!(4)).await;

            // Drop the client write-half → the server's stdin read hits EOF →
            // teardown. The on-disk store (with F0's assistant turns) survives.
            drop(client_write);
            (session_id, got_p3, got_p4)
        };

        let (session_id, p3, p4) = tokio::select! {
            outcome = drive => outcome,
            server_res = server => {
                panic!("ACP server (phase A) exited before the client finished: {server_res:?}");
            }
        };
        assert!(p3, "phase A turn 1 must resolve end_turn");
        assert!(p4, "phase A turn 2 must resolve end_turn");
        session_id
    };

    assert_eq!(session_id, "acp-1", "deterministic first session id");

    // ── Phase B: FRESH server, SAME workspace; load + prompt ───────────
    let (observed_tx, mut observed_rx) =
        mpsc::unbounded_channel::<Vec<rustain::domain::models::Message>>();
    {
        let (server_incoming, mut client_write) = tokio::io::duplex(8 * 1024);
        let (mut client_read, server_outgoing) = tokio::io::duplex(8 * 1024);
        let ws_for_factory = ws.clone();
        let core_factory: CoreFactory = Rc::new(move |_: &Path, _mcp_servers| {
            Ok(make_history_core(&ws_for_factory, observed_tx.clone()))
        });
        let server = serve_acp_with_core_factory(
            server_outgoing.compat_write(),
            server_incoming.compat(),
            AppConfig::default(),
            workspace.path().to_path_buf(),
            None,
            core_factory,
        );

        let session_id_b = session_id.clone();
        let drive = async move {
            client_write
                .write_all(line(ACP_INITIALIZE_REQ.to_string()).as_bytes())
                .await
                .expect("write initialize B");
            let load = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "session/load",
                "params": {
                    "cwd": workspace.path(),
                    "sessionId": session_id_b,
                    "mcpServers": [],
                },
            });
            client_write
                .write_all(line(load.to_string()).as_bytes())
                .await
                .expect("write session/load");
            client_write.flush().await.expect("flush B load");

            let reader = tokio::io::BufReader::new(&mut client_read);
            let mut lines = reader.lines();
            // Await the load result (replay notifications have no `id`).
            let mut got_load = false;
            loop {
                let next = tokio::time::timeout(Duration::from_secs(10), lines.next_line()).await;
                let raw = match next {
                    Ok(Ok(Some(l))) => l,
                    _ => break,
                };
                let v: serde_json::Value = match serde_json::from_str(&raw) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if v["id"] == serde_json::json!(2) {
                    got_load = true;
                    break;
                }
            }

            // Prompt AFTER load resolves (sequential — no concurrent dispatch).
            client_write
                .write_all(
                    line(acp_prompt_req_with_text(
                        serde_json::json!(3),
                        &session_id,
                        "charlie third turn",
                    ))
                    .as_bytes(),
                )
                .await
                .expect("write prompt 3");
            client_write.flush().await.expect("flush B p3");
            let got_p3 = await_end_turn(&mut lines, serde_json::json!(3)).await;
            drop(client_write);
            (got_load, got_p3)
        };

        let (got_load, got_p3) = tokio::select! {
            outcome = drive => outcome,
            server_res = server => {
                panic!("ACP server (phase B) exited before the client finished: {server_res:?}");
            }
        };
        assert!(got_load, "session/load must resolve after the kill");
        assert!(got_p3, "the post-load session/prompt must resolve end_turn");

        // The Phase-B provider observed the reloaded history + the new turn.
        let messages = observed_rx
            .recv()
            .await
            .expect("the provider must observe the post-load turn's API messages");

        // AC7: BOTH prior user prompts survived the kill→reload.
        assert!(
            messages
                .iter()
                .any(|m| m.role == MessageRole::User && m.content.contains("alpha first turn")),
            "reloaded history must include the turn-1 user prompt"
        );
        assert!(
            messages
                .iter()
                .any(|m| m.role == MessageRole::User && m.content.contains("bravo second turn")),
            "reloaded history must include the turn-2 user prompt"
        );
        assert!(
            messages
                .iter()
                .any(|m| m.role == MessageRole::User && m.content.contains("charlie third turn")),
            "the post-load turn's user prompt must be present"
        );

        // F0 discriminator (the net-new proof): the model's ANSWERS survived.
        let assistant_answers = count_assistant_messages_containing(&messages, GOLDEN_AGENT_TEXT);
        assert_eq!(
            assistant_answers, 2,
            "F0 keystone: BOTH prior assistant responses ({GOLDEN_AGENT_TEXT:?}) must \
             survive the kill→reload — got {assistant_answers}. A mutant that drops Task-1's \
             post-turn save yields 0 (only user prompts persist) and reddens this assertion."
        );
    }
}

// ─────────────────────────────────────────────────────────────────────
// Section 11 — 14.9 lifecycle anti-vacuous gates (AC4 list, AC8 orphan)
// ─────────────────────────────────────────────────────────────────────

/// AC8 — `session/load` of an id with no persisted conversation (orphan) MUST
/// resolve to `resource_not_found` (code -32002), never a silent empty success.
///
/// Non-vacuity: the assertion is on the JSON-RPC `error.code`, not merely the
/// absence of a `result`. A mutant that returns an empty `LoadSessionResponse`
/// (default success) on a miss reddens this — the response would carry a
/// `result` with no `error`, failing the `-32002` check. DD-2 resolution
/// (prefix-strip) is also exercised: a non-`acp-` id is an orphan too.
#[tokio::test(flavor = "current_thread")]
async fn acp_load_unknown_session_id_is_resource_not_found() {
    use std::rc::Rc;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
    use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

    use rustain::adapters::acp::agent::CoreFactory;
    use rustain::adapters::acp::run::serve_acp_with_core_factory;
    use rustain::domain::models::AppConfig;

    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let ws = workspace.path().to_path_buf();
    let (server_incoming, mut client_write) = tokio::io::duplex(8 * 1024);
    let (mut client_read, server_outgoing) = tokio::io::duplex(8 * 1024);
    let core_factory: CoreFactory = Rc::new(move |_: &Path, _mcp_servers| Ok(make_core(&ws)));
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
        let load = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/load",
            "params": { "cwd": workspace.path(), "sessionId": "acp-never-existed", "mcpServers": [] },
        });
        client_write
            .write_all(line(load.to_string()).as_bytes())
            .await
            .expect("write session/load");
        client_write.flush().await.expect("flush");

        let reader = tokio::io::BufReader::new(&mut client_read);
        let mut lines = reader.lines();
        loop {
            let next = tokio::time::timeout(Duration::from_secs(10), lines.next_line()).await;
            let raw = match next {
                Ok(Ok(Some(l))) => l,
                _ => panic!("no response to the orphan session/load"),
            };
            let v: serde_json::Value = match serde_json::from_str(&raw) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if v["id"] == serde_json::json!(2) {
                drop(client_write);
                return v;
            }
        }
    };

    let response = tokio::select! {
        v = drive => v,
        server_res = server => panic!("server exited early: {server_res:?}"),
    };
    assert_eq!(
        response["error"]["code"],
        serde_json::json!(-32002),
        "an orphan session/load must return resource_not_found (-32002); got {}",
        response
    );
    assert!(
        response["result"].is_null(),
        "an orphan session/load must NOT carry a result; got {}",
        response
    );
}

/// AC4 — `session/list` over a cwd returns the persisted ACP conversation as
/// `acp-{conversation_id}` with the echoed request cwd (DD-2 + DD-1).
///
/// After `new + prompt` persists a conversation, `session/list(cwd)` MUST
/// surface it. The session id is the durable `acp-{conversation_id}` derive
/// (DD-2), and `SessionInfo.cwd` is the request cwd echoed — never read from a
/// per-session field (DD-1: cwd is implicit in the store location).
#[tokio::test(flavor = "current_thread")]
async fn acp_list_sessions_returns_persisted_session() {
    use std::rc::Rc;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
    use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

    use rustain::adapters::acp::agent::CoreFactory;
    use rustain::adapters::acp::run::serve_acp_with_core_factory;
    use rustain::domain::models::AppConfig;

    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let ws = workspace.path().to_path_buf();
    let (server_incoming, mut client_write) = tokio::io::duplex(8 * 1024);
    let (mut client_read, server_outgoing) = tokio::io::duplex(8 * 1024);
    let core_factory: CoreFactory = Rc::new(move |_: &Path, _mcp_servers| Ok(make_core(&ws)));
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
        let new = serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/new",
            "params": { "cwd": workspace.path(), "mcpServers": [] },
        });
        client_write
            .write_all(line(new.to_string()).as_bytes())
            .await
            .expect("write new");
        client_write.flush().await.expect("flush new");
        // Persist the conversation via one prompt (run_turn's pre-turn save).
        client_write
            .write_all(
                line(acp_prompt_req_with_text(
                    serde_json::json!(3),
                    "acp-1",
                    "persist me",
                ))
                .as_bytes(),
            )
            .await
            .expect("write prompt");
        client_write.flush().await.expect("flush prompt");
        // Read until the prompt resolves (so the save is durable on disk).
        let reader = tokio::io::BufReader::new(&mut client_read);
        let mut lines = reader.lines();
        let mut persisted = false;
        loop {
            let next = tokio::time::timeout(Duration::from_secs(10), lines.next_line()).await;
            let raw = match next {
                Ok(Ok(Some(l))) => l,
                _ => break,
            };
            let v: serde_json::Value = match serde_json::from_str(&raw) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if v["id"] == serde_json::json!(3)
                && v["result"]["stopReason"] == serde_json::json!("end_turn")
            {
                persisted = true;
                break;
            }
        }
        assert!(persisted, "the persisting prompt must resolve end_turn");
        // session/list(cwd).
        let list = serde_json::json!({
            "jsonrpc": "2.0", "id": 4, "method": "session/list",
            "params": { "cwd": workspace.path() },
        });
        client_write
            .write_all(line(list.to_string()).as_bytes())
            .await
            .expect("write list");
        client_write.flush().await.expect("flush list");
        let mut list_resp: Option<serde_json::Value> = None;
        loop {
            let next = tokio::time::timeout(Duration::from_secs(10), lines.next_line()).await;
            let raw = match next {
                Ok(Ok(Some(l))) => l,
                _ => break,
            };
            let v: serde_json::Value = match serde_json::from_str(&raw) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if v["id"] == serde_json::json!(4) {
                list_resp = Some(v);
                break;
            }
        }
        drop(client_write);
        list_resp.expect("a session/list response")
    };

    let response = tokio::select! {
        v = drive => v,
        server_res = server => panic!("server exited early: {server_res:?}"),
    };
    let sessions = response["result"]["sessions"]
        .as_array()
        .expect("sessions array");
    assert!(
        sessions.iter().any(|s| s["sessionId"]
            .as_str()
            .is_some_and(|id| id.starts_with("acp-"))),
        "session/list must surface the persisted conversation as acp-{{conversation_id}}; got {response}"
    );
    // DD-1: cwd is the request cwd echoed (the store IS the cwd filter).
    assert!(
        sessions
            .iter()
            .all(|s| s["cwd"] == serde_json::json!(workspace.path())),
        "every SessionInfo.cwd must equal the request cwd (DD-1 echoed, not disk-read); got {response}"
    );
}

// ─────────────────────────────────────────────────────────────────────
// Section 12 — 14.9 review-patch regression gates
// (list pagination, malformed cursor, close-unknown, exact id resolution)
// ─────────────────────────────────────────────────────────────────────

/// AC4 pagination keystone — `session/list` over 26 same-`updated_at`
/// conversations (page size 25 ⇒ forces a 2nd page) MUST page without
/// duplicating or dropping any id.
///
/// Regression guard for the inverted same-timestamp cursor tiebreak: with
/// the buggy filter `(updated_at, id) < cursor`, page 2 re-emitted the
/// page-1 rows whose ids sort BELOW the cursor and silently dropped the
/// 26th. The corrected filter keeps rows whose id sorts STRICTLY AFTER the
/// cursor within the same timestamp, so every id surfaces exactly once.
///
/// Non-vacuity:
/// * 26 rows share ONE `updated_at`, so the cursor timestamp is a tie —
///   the id tiebreak is the ONLY discriminator between a correct and an
///   inverted filter. Different-timestamp rows would page correctly even
///   with the bug, so they cannot defend this contract.
/// * Rows are seeded directly into the per-cwd `FileSystemStorage` the real
///   agent lists, then paginated through the REAL ACP server over an
///   in-memory duplex — no list mock.
/// * `all_ids.len() >= 26` is a self-check: if `SESSION_LIST_PAGE_SIZE`
///   ever grows past 26, this fails loudly instead of the test passing
///   vacuously on a single page (which would not exercise the tiebreak).
/// * Three independent assertions catch the bug: total collected, unique
///   count, and per-id presence.
///
/// Determinism: identical seeded `updated_at` (no wall-clock), in-memory
/// duplex transport (no listener), lexicographic zero-padded ids.
#[tokio::test(flavor = "current_thread")]
async fn acp_list_paginates_same_second_sessions_without_duplicates_or_missing() {
    use std::collections::HashSet;
    use std::rc::Rc;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
    use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

    use rustain::adapters::acp::agent::CoreFactory;
    use rustain::adapters::acp::run::serve_acp_with_core_factory;
    use rustain::adapters::filesystem::FileSystemStorage;
    use rustain::domain::models::{AppConfig, Conversation};

    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let ws = workspace.path().to_path_buf();

    // Seed 26 conversations with an IDENTICAL `updated_at`. Same-second rows
    // are exactly where the inverted-tiebreak bug duplicated/dropped ids;
    // 26 > SESSION_LIST_PAGE_SIZE (25) forces a second page.
    const SAME_SECOND: i64 = 1_700_000_000;
    const SEED_COUNT: usize = 26;
    let sessions_dir = ws.join(".rustain").join("sessions");
    {
        let seeding_store = FileSystemStorage::with_workspace_root(sessions_dir, ws.clone());
        for i in 1..=SEED_COUNT {
            let id = format!("seed-{i:02}");
            let conv = Conversation {
                id: id.clone(),
                title: format!("Seed {i}"),
                updated_at: SAME_SECOND,
                // Mark every seed ACP-origin so the list's origin filter
                // (conversation.session_id starts_with "acp-") keeps it.
                session_id: Some(format!("acp-{id}")),
                ..Default::default()
            };
            seeding_store
                .save_conversation(&conv)
                .await
                .expect("seed save");
        }
    }

    let (server_incoming, mut client_write) = tokio::io::duplex(8 * 1024);
    let (mut client_read, server_outgoing) = tokio::io::duplex(8 * 1024);
    let ws_for_factory = ws.clone();
    let core_factory: CoreFactory =
        Rc::new(move |_: &Path, _mcp_servers| Ok(make_core(&ws_for_factory)));
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

        let reader = tokio::io::BufReader::new(&mut client_read);
        let mut lines = reader.lines();

        let mut next_cursor: Option<String> = None;
        let mut all_ids: Vec<String> = Vec::new();
        let mut req_id = 2i64;
        loop {
            let list = serde_json::json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "method": "session/list",
                "params": {
                    "cwd": workspace.path(),
                    "cursor": next_cursor,
                },
            });
            client_write
                .write_all(line(list.to_string()).as_bytes())
                .await
                .expect("write list");
            client_write.flush().await.expect("flush list");

            // Read until this page's result arrives (notifications carry no id).
            let resp: serde_json::Value = loop {
                let next = tokio::time::timeout(Duration::from_secs(10), lines.next_line()).await;
                let raw = match next {
                    Ok(Ok(Some(l))) => l,
                    _ => panic!("no response to session/list id {req_id}"),
                };
                let v: serde_json::Value = match serde_json::from_str(&raw) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if v["id"] == serde_json::json!(req_id) {
                    break v;
                }
            };
            assert!(
                resp["error"].is_null(),
                "paginated session/list on a valid cursor must not error; got {resp}"
            );
            if let Some(arr) = resp["result"]["sessions"].as_array() {
                for s in arr {
                    if let Some(id) = s["sessionId"].as_str() {
                        all_ids.push(id.to_string());
                    }
                }
            }
            next_cursor = resp["result"]["nextCursor"].as_str().map(|s| s.to_string());
            req_id += 1;
            if next_cursor.is_none() {
                break;
            }
        }
        drop(client_write);
        all_ids
    };

    let all_ids = tokio::select! {
        ids = drive => ids,
        server_res = server => panic!("server exited early: {server_res:?}"),
    };

    // Self-check: 26 rows at page size 25 MUST yield a second page. If the
    // page-size constant grows past 26, fail loudly instead of passing on a
    // single page (which would not exercise the tiebreak at all).
    assert!(
        all_ids.len() >= SEED_COUNT,
        "pagination must traverse >=2 pages for {SEED_COUNT} same-second rows; \
         collected only {} ids -- bump SEED_COUNT if SESSION_LIST_PAGE_SIZE changed. got {all_ids:?}",
        all_ids.len()
    );

    let unique: HashSet<&String> = all_ids.iter().collect();
    assert_eq!(
        unique.len(),
        SEED_COUNT,
        "no duplicate session ids across pages -- the inverted-tiebreak bug \
         re-emits page-1 rows on page 2; got {} unique of {} total: {all_ids:?}",
        unique.len(),
        all_ids.len()
    );
    for i in 1..=SEED_COUNT {
        let expected = format!("acp-seed-{i:02}");
        assert!(
            unique.contains(&expected),
            "paginated list must surface every same-second session exactly \
             once; missing {expected}; got {all_ids:?}"
        );
    }
}

/// Malformed-cursor keystone — `session/list` with a cursor that is not a
/// valid `{updated_at}:{id}` token MUST return a JSON-RPC error
/// (`invalid_params`, -32602), never silently reset to page one.
///
/// Regression guard for the silent-reset bug: the original cursor decoder
/// used `?` inside `and_then`, so an unparseable cursor collapsed to `None`
/// and the server happily re-issued the FIRST page — a client stepping an
/// opaque/malformed cursor would loop forever over page 1 and never advance.
///
/// Non-vacuity:
/// * The cursor has NO colon, so it cannot be a valid `{ts}:{id}` token;
///   the only correct response is an error.
/// * Seeded rows make page 1 non-empty, so "returns page 1" is a real,
///   observable wrong answer (not an empty list that looks like an error).
/// * Asserts BOTH `error.code == -32602` AND `result` is absent — a mutant
///   that returns page 1 carries a `result` and no `error`, reddening both.
#[tokio::test(flavor = "current_thread")]
async fn acp_list_malformed_cursor_returns_error_not_page_one() {
    use std::rc::Rc;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
    use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

    use rustain::adapters::acp::agent::CoreFactory;
    use rustain::adapters::acp::run::serve_acp_with_core_factory;
    use rustain::adapters::filesystem::FileSystemStorage;
    use rustain::domain::models::{AppConfig, Conversation};

    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let ws = workspace.path().to_path_buf();
    // Seed a couple of rows so page 1 is non-empty — a silent reset would
    // return them; the correct behavior is an error with no result.
    {
        let sessions_dir = ws.join(".rustain").join("sessions");
        let seeding_store = FileSystemStorage::with_workspace_root(sessions_dir, ws.clone());
        for i in 1..=2 {
            let id = format!("row-{i:02}");
            let conv = Conversation {
                id: id.clone(),
                updated_at: 1_700_000_000 + i as i64,
                session_id: Some(format!("acp-{id}")),
                ..Default::default()
            };
            seeding_store
                .save_conversation(&conv)
                .await
                .expect("seed save");
        }
    }

    let (server_incoming, mut client_write) = tokio::io::duplex(8 * 1024);
    let (mut client_read, server_outgoing) = tokio::io::duplex(8 * 1024);
    let ws_for_factory = ws.clone();
    let core_factory: CoreFactory =
        Rc::new(move |_: &Path, _mcp_servers| Ok(make_core(&ws_for_factory)));
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
        // A cursor with no colon can never decode to `{ts}:{id}`.
        let list = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/list",
            "params": {
                "cwd": workspace.path(),
                "cursor": "this-is-not-a-valid-cursor",
            },
        });
        client_write
            .write_all(line(list.to_string()).as_bytes())
            .await
            .expect("write list");
        client_write.flush().await.expect("flush list");

        let reader = tokio::io::BufReader::new(&mut client_read);
        let mut lines = reader.lines();
        loop {
            let next = tokio::time::timeout(Duration::from_secs(10), lines.next_line()).await;
            let raw = match next {
                Ok(Ok(Some(l))) => l,
                _ => panic!("no response to the malformed-cursor session/list"),
            };
            let v: serde_json::Value = match serde_json::from_str(&raw) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if v["id"] == serde_json::json!(2) {
                drop(client_write);
                return v;
            }
        }
    };

    let response = tokio::select! {
        v = drive => v,
        server_res = server => panic!("server exited early: {server_res:?}"),
    };
    assert_eq!(
        response["error"]["code"],
        serde_json::json!(-32602),
        "a malformed session/list cursor must return invalid_params (-32602), \
         not silently re-issue page one; got {response}"
    );
    assert!(
        response["result"].is_null(),
        "a malformed session/list cursor must NOT carry a result (no silent \
         page-one reset); got {response}"
    );
}

/// Close-unknown keystone — `session/close` of an id with no live session
/// MUST return `resource_not_found` (-32002), never a silent success.
///
/// Regression guard for the silent-success bug: the original `close_session`
/// treated an unknown id as a no-op and returned `Ok(CloseSessionResponse)`,
/// so a client could never distinguish a real close from a stale-id no-op.
///
/// Non-vacuity (A/B contrast in one connection):
/// * Positive control: closing a LIVE session (`acp-1`) returns a result
///   with no error — proves the fix did not break the happy path. A mutant
///   that errors on ALL closes reddens this.
/// * Keystone: closing an id that was never created returns
///   `resource_not_found` (-32002) with no `result`. A mutant that silently
///   succeeds on unknown ids reddens this.
#[tokio::test(flavor = "current_thread")]
async fn acp_close_unknown_session_id_is_resource_not_found() {
    use std::rc::Rc;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
    use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

    use rustain::adapters::acp::agent::CoreFactory;
    use rustain::adapters::acp::run::serve_acp_with_core_factory;
    use rustain::domain::models::AppConfig;

    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let ws = workspace.path().to_path_buf();
    let (server_incoming, mut client_write) = tokio::io::duplex(8 * 1024);
    let (mut client_read, server_outgoing) = tokio::io::duplex(8 * 1024);
    let core_factory: CoreFactory = Rc::new(move |_: &Path, _mcp_servers| Ok(make_core(&ws)));
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
        let new = serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/new",
            "params": { "cwd": workspace.path(), "mcpServers": [] },
        });
        client_write
            .write_all(line(new.to_string()).as_bytes())
            .await
            .expect("write new");
        client_write.flush().await.expect("flush new");

        let reader = tokio::io::BufReader::new(&mut client_read);
        let mut lines = reader.lines();

        // Await session/new so acp-1 is live before closing it.
        let mut live = false;
        loop {
            let next = tokio::time::timeout(Duration::from_secs(10), lines.next_line()).await;
            let raw = match next {
                Ok(Ok(Some(l))) => l,
                _ => break,
            };
            let v: serde_json::Value = match serde_json::from_str(&raw) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if v["id"] == serde_json::json!(2) && v["result"]["sessionId"].is_string() {
                live = true;
                break;
            }
        }
        assert!(live, "session/new must establish a live acp-1 session");

        // Positive control: close the LIVE session -> success (result, no error).
        let close_live = serde_json::json!({
            "jsonrpc": "2.0", "id": 3, "method": "session/close",
            "params": { "sessionId": "acp-1" },
        });
        client_write
            .write_all(line(close_live.to_string()).as_bytes())
            .await
            .expect("write close live");
        client_write.flush().await.expect("flush close live");
        let mut live_close: Option<serde_json::Value> = None;
        loop {
            let next = tokio::time::timeout(Duration::from_secs(10), lines.next_line()).await;
            let raw = match next {
                Ok(Ok(Some(l))) => l,
                _ => break,
            };
            let v: serde_json::Value = match serde_json::from_str(&raw) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if v["id"] == serde_json::json!(3) {
                live_close = Some(v);
                break;
            }
        }
        let live_close = live_close.expect("close-live response");
        assert!(
            live_close["result"].is_object(),
            "closing a LIVE session must succeed (positive control); got {live_close}"
        );

        // Keystone: close an UNKNOWN id -> resource_not_found, no result.
        let close_unknown = serde_json::json!({
            "jsonrpc": "2.0", "id": 4, "method": "session/close",
            "params": { "sessionId": "acp-never-existed" },
        });
        client_write
            .write_all(line(close_unknown.to_string()).as_bytes())
            .await
            .expect("write close unknown");
        client_write.flush().await.expect("flush close unknown");
        let mut unknown_close: Option<serde_json::Value> = None;
        loop {
            let next = tokio::time::timeout(Duration::from_secs(10), lines.next_line()).await;
            let raw = match next {
                Ok(Ok(Some(l))) => l,
                _ => break,
            };
            let v: serde_json::Value = match serde_json::from_str(&raw) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if v["id"] == serde_json::json!(4) {
                unknown_close = Some(v);
                break;
            }
        }
        drop(client_write);
        unknown_close.expect("close-unknown response")
    };

    let response = tokio::select! {
        v = drive => v,
        server_res = server => panic!("server exited early: {server_res:?}"),
    };
    assert_eq!(
        response["error"]["code"],
        serde_json::json!(-32002),
        "closing an unknown session id must return resource_not_found (-32002), \
         not a silent success; got {response}"
    );
    assert!(
        response["result"].is_null(),
        "closing an unknown session id must NOT carry a result; got {response}"
    );
}

/// Exact-id-resolution keystone — `session/load` resolves an id by an EXACT
/// `acp-{conversation_id}` match. Near-miss ids (truncated, extended,
/// case-folded) MUST each fail with `resource_not_found`, never resolve to
/// a sibling/prefix conversation.
///
/// Regression guard for AC8 ("no orphans/ambiguity"): the durable-id scheme
/// is `SessionId = acp-{conversation_id}` with a prefix-strip + EXACT
/// `load_conversation`. This pins that resolution is exact-string — a future
/// change to prefix/substring matching (which would let `acp-abc` resolve a
/// real `acp-abcd`) reddens the near-miss assertions. The existing orphan
/// test only used a wholly-fake id; this adds the ambiguity discriminator:
/// ids that are CLOSE to a real one but not equal.
///
/// Non-vacuity:
/// * Positive control: the EXACT id loads successfully (result present) —
///   proves the seeded conversation is reachable, so the near-miss failures
///   are about exactness, not absence.
/// * Three distinct near-miss shapes (trailing-char, dropped-char,
///   case-fold) each fail with -32002. A prefix-match mutant resolves the
///   truncated/extended cases and reddens them.
///
/// Determinism: a fixed seeded `conversation_id` (no id source involved),
/// in-memory duplex transport (no listener).
#[tokio::test(flavor = "current_thread")]
async fn acp_session_id_resolution_is_exact_not_prefix_or_substring() {
    use std::rc::Rc;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
    use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

    use rustain::adapters::acp::agent::CoreFactory;
    use rustain::adapters::acp::run::serve_acp_with_core_factory;
    use rustain::adapters::filesystem::FileSystemStorage;
    use rustain::domain::models::{AppConfig, Conversation};

    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let ws = workspace.path().to_path_buf();

    // A distinctive, mixed-case conversation id (sanitize-safe: [a-zA-Z0-9_-]).
    const CONV_ID: &str = "exactKeyStone7q";
    {
        let sessions_dir = ws.join(".rustain").join("sessions");
        let seeding_store = FileSystemStorage::with_workspace_root(sessions_dir, ws.clone());
        let conv = Conversation {
            id: CONV_ID.to_string(),
            updated_at: 1_700_000_000,
            session_id: Some(format!("acp-{CONV_ID}")),
            ..Default::default()
        };
        seeding_store
            .save_conversation(&conv)
            .await
            .expect("seed save");
    }

    let (server_incoming, mut client_write) = tokio::io::duplex(8 * 1024);
    let (mut client_read, server_outgoing) = tokio::io::duplex(8 * 1024);
    let ws_for_factory = ws.clone();
    let core_factory: CoreFactory =
        Rc::new(move |_: &Path, _mcp_servers| Ok(make_core(&ws_for_factory)));
    let server = serve_acp_with_core_factory(
        server_outgoing.compat_write(),
        server_incoming.compat(),
        AppConfig::default(),
        workspace.path().to_path_buf(),
        None,
        core_factory,
    );

    // The exact id MUST load; every near-miss MUST fail with resource_not_found.
    let exact = format!("acp-{CONV_ID}");
    let extended = format!("acp-{CONV_ID}X");
    let truncated = format!("acp-{}", &CONV_ID[..CONV_ID.len() - 1]);
    let case_folded = format!("acp-{}", CONV_ID.to_uppercase());
    let cases: Vec<(&str, String)> = vec![
        ("exact", exact),
        ("extended", extended),
        ("truncated", truncated),
        ("case_folded", case_folded),
    ];

    let drive = async move {
        client_write
            .write_all(line(ACP_INITIALIZE_REQ.to_string()).as_bytes())
            .await
            .expect("write initialize");
        client_write.flush().await.expect("flush initialize");

        // ONE persistent line reader across all loads -- recreating a
        // BufReader per iteration would discard buffered bytes and stall.
        let reader = tokio::io::BufReader::new(&mut client_read);
        let mut lines = reader.lines();
        let mut results: Vec<(String, bool, serde_json::Value)> = Vec::new();
        for (i, (label, sid)) in cases.iter().enumerate() {
            let req_id = (2 + i) as i64;
            let load = serde_json::json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "method": "session/load",
                "params": {
                    "cwd": workspace.path(),
                    "sessionId": sid,
                    "mcpServers": [],
                },
            });
            client_write
                .write_all(line(load.to_string()).as_bytes())
                .await
                .expect("write load");
            client_write.flush().await.expect("flush load");
            // Read until this load's result arrives. Replay notifications
            // (for the exact-id load) carry no `id` and are skipped here.
            let resp: serde_json::Value = loop {
                let next = tokio::time::timeout(Duration::from_secs(10), lines.next_line()).await;
                let raw = match next {
                    Ok(Ok(Some(l))) => l,
                    _ => panic!("no response to session/load id {req_id} ({label})"),
                };
                let v: serde_json::Value = match serde_json::from_str(&raw) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if v["id"] == serde_json::json!(req_id) {
                    break v;
                }
            };
            let ok = resp["result"].is_object();
            results.push((label.to_string(), ok, resp));
        }
        drop(client_write);
        results
    };

    let results = tokio::select! {
        r = drive => r,
        server_res = server => panic!("server exited early: {server_res:?}"),
    };

    for (label, ok, resp) in &results {
        match label.as_str() {
            "exact" => {
                assert!(
                    *ok,
                    "the EXACT id must load (positive control -- proves the \
                     seeded conversation is reachable); got {resp}"
                );
            }
            "extended" | "truncated" | "case_folded" => {
                assert!(
                    !*ok && resp["error"]["code"] == serde_json::json!(-32002),
                    "near-miss id `{label}` must NOT resolve -- exact resolution \
                     only; expected resource_not_found (-32002), got {resp}"
                );
            }
            _ => unreachable!("unhandled id-resolution case label {label}"),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// Section 13 — Story 14-10 Task 5: ACP `ContentBlock::Image` passthrough
// ─────────────────────────────────────────────────────────────────────

/// The prompt text sent alongside the image blocks. Distinctive so the
/// provider-facing user message carrying the attachments is unambiguous.
const ACP_IMAGE_PROMPT_TEXT: &str = "describe these two attached images";

/// Two distinct image payloads (mime + base64 `data`) sent as
/// `ContentBlock::Image`. Using TWO images — not one — makes the test catch a
/// mutant that forwards only the first image; a single-image case could not.
const IMG1_MIME: &str = "image/png";
const IMG1_DATA: &str = "aW1hZ2Utb25lLWRhdGE="; // base64("image-one-data")
const IMG2_MIME: &str = "image/jpeg";
const IMG2_DATA: &str = "aW1hZ2UtdHdvLWRhdGE="; // base64("image-two-data")

/// Build a `session/prompt` request body carrying a text block FOLLOWED by one
/// or more `image` content blocks (base64 `data` + `mimeType`, per the ACP
/// content schema). Mirrors [`acp_prompt_req_with_text`] but exercises the
/// `ContentBlock::Image` arm the ACP seam historically dropped via `_ => {}`.
fn acp_prompt_req_with_image(
    id: serde_json::Value,
    session_id: &str,
    text: &str,
    images: &[(&str, &str)],
) -> String {
    let mut prompt = vec![serde_json::json!({ "type": "text", "text": text })];
    for (mime, data) in images {
        prompt.push(serde_json::json!({
            "type": "image",
            "data": data,
            "mimeType": mime,
        }));
    }
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "session/prompt",
        "params": {
            "sessionId": session_id,
            "prompt": prompt,
        }
    });
    serde_json::to_string(&body).expect("image prompt request must serialize")
}

/// Story 14-10 Task 5 keystone — an ACP `session/prompt` carrying a text block
/// plus `ContentBlock::Image` blocks (base64 `data` + `mimeType`) drives the
/// real ACP agent, and the EXACT mime type and bytes of every image reach the
/// provider-facing message as an attachment.
///
/// The ACP seam previously dropped image blocks (`prompt_text` matched only
/// `Text`/`ResourceLink`, falling through `_ => {}`) and hardcoded
/// `images: vec![]`. This test reddens BOTH regressions: it captures the real
/// `Vec<Message>` the provider receives — via the same recording-provider seam
/// the P-14 history test uses — and asserts the user message carrying the
/// prompt text also carries the images verbatim.
///
/// Non-vacuity:
/// * A mutant that drops `ContentBlock::Image` in `prompt_parts` (`_ => {}`)
///   forwards zero images → `images.len() == 2` reddens it.
/// * A mutant that keeps `images: vec![]` hardcoded (i.e. removes the
///   `last_user.images = images` attachment applied after `build_api_messages`)
///   reddens it identically — `build_api_messages` emits no images.
/// * Two distinct images catch a first-only forwarder; asserting each pair's
///   exact mime AND data catches a mime/data swap or truncation.
///
/// Determinism: scripted recording provider (no network), in-memory duplex
/// transport (no listener), deterministic first session id (`acp-1`).
#[tokio::test(flavor = "current_thread")]
async fn acp_prompt_image_block_reaches_provider_as_exact_attachment() {
    use std::rc::Rc;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
    use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

    use rustain::adapters::acp::agent::CoreFactory;
    use rustain::adapters::acp::run::serve_acp_with_core_factory;
    use rustain::domain::models::{AppConfig, MessageRole};

    #[cfg(feature = "test-instrumentation")]
    let _run_turn_guard = ACP_RUN_TURN_TEST_LOCK.lock().await;

    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let ws_for_factory = workspace.path().to_path_buf();
    let (observed_tx, mut observed_rx) =
        mpsc::unbounded_channel::<Vec<rustain::domain::models::Message>>();
    let observed_tx_for_factory = observed_tx.clone();

    let (server_incoming, mut client_write) = tokio::io::duplex(16 * 1024);
    let (mut client_read, server_outgoing) = tokio::io::duplex(16 * 1024);

    let core_factory: CoreFactory = Rc::new(move |_cwd: &Path, _mcp_servers| {
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

    let images: &[(&str, &str)] = &[(IMG1_MIME, IMG1_DATA), (IMG2_MIME, IMG2_DATA)];

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

        client_write
            .write_all(
                line(acp_prompt_req_with_image(
                    serde_json::json!(3),
                    "acp-1",
                    ACP_IMAGE_PROMPT_TEXT,
                    images,
                ))
                .as_bytes(),
            )
            .await
            .expect("write image prompt");
        client_write.flush().await.expect("flush image prompt");
        loop {
            let raw = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
                .await
                .expect("timeout waiting for image-prompt ACP frame")
                .expect("read error on ACP stream")
                .expect("ACP stream ended before image-prompt result");
            let v: serde_json::Value = serde_json::from_str(&raw).expect("ACP frame JSON");
            if v["id"] == serde_json::json!(3) {
                assert!(
                    v.get("error").is_none(),
                    "image-prompt ACP prompt returned an error: {v}"
                );
                assert_eq!(
                    v["result"]["stopReason"],
                    serde_json::json!("end_turn"),
                    "image-prompt must resolve with stopReason `end_turn`, got: {v}"
                );
                break;
            }
        }
    };

    tokio::select! {
        _ = drive => {}
        server_res = server => {
            panic!("ACP server exited before image prompt completed: {server_res:?}");
        }
    }

    // The recording provider sent its input messages over the channel BEFORE
    // the prompt result reached the client, so the snapshot is buffered now.
    let messages = observed_rx
        .recv()
        .await
        .expect("the recording provider must have observed exactly one turn");

    // The user-role message carrying the prompt text is the one the ACP agent
    // attaches the image payload to (last user message after build_api_messages).
    let user_with_prompt = messages
        .iter()
        .find(|m| m.role == MessageRole::User && m.content.contains(ACP_IMAGE_PROMPT_TEXT))
        .expect("a user message carrying the prompt text must reach the provider");

    assert_eq!(
        user_with_prompt.images.len(),
        2,
        "both image blocks must reach the provider as attachments; got {:?}",
        user_with_prompt.images
    );

    // Exact mime + bytes, in order. The ACP seam copies `ImageContent.data`
    // and `ImageContent.mime_type` verbatim into `ImageAttachment`, so a
    // swap / truncation / mime-mismatch mutant reddens these equalities.
    assert_eq!(
        user_with_prompt.images[0].media_type, IMG1_MIME,
        "first attachment media type must match the prompt exactly"
    );
    assert_eq!(
        user_with_prompt.images[0].data, IMG1_DATA,
        "first attachment data (base64) must reach the provider unchanged"
    );
    assert_eq!(
        user_with_prompt.images[1].media_type, IMG2_MIME,
        "second attachment media type must match the prompt exactly"
    );
    assert_eq!(
        user_with_prompt.images[1].data, IMG2_DATA,
        "second attachment data (base64) must reach the provider unchanged"
    );
}
