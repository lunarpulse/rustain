//! Story 14-8 — Zed ACP skills + model selector red/green conformance.
//!
//! These tests defend the externally observable contracts the Zed editor
//! depends on when it drives rustain over the Agent Client Protocol:
//!
//! 1. **CLI `--client` profile** — `rustain acp --client zed` parses to the
//!    dedicated `Command::Acp` variant. The flag is the opt-in that selects
//!    the Zed client profile (model-selector exposure, command advertisement,
//!    skill activation UX). A regression that drops the flag — or that lets it
//!    redirect away from ACP — reddens the parse test.
//! 2. **`session/new` model selector** — when the provider registry is
//!    non-empty, the dispatched `session/new` result advertises a
//!    `configOptions` entry of category `model` whose choices are exactly the
//!    registered model ids; when the registry is empty, NO model option is
//!    advertised. This is the contrast Zed's model dropdown keys off. Today
//!    `new_session` returns a bare `NewSessionResponse::new(id)` with no
//!    `config_options`, so the non-empty case reddens.
//!
//! All dispatched tests drive REAL ACP JSON-RPC over a `tokio::io::duplex`
//! pair (no network listener) through the SAME `serve_acp_with_core_factory`
//! seam the golden-transcript harness in `conformance_acp.rs` uses. The
//! provider is scripted (deterministic, no real provider). No sleeps.
//!
//! These are RED tests for Story 14-8: they compile against the existing
//! public surface (CLI parser + `serve_acp_with_core_factory` + the
//! `StreamingProvider` trait) and are expected to FAIL until the production
//! implementation lands. Production hooks the implementation must expose are
//! documented inline where a test reaches a boundary the current code does
//! not yet cross.

use std::path::Path;
use std::sync::Arc;

use clap::Parser;
use tokio::sync::mpsc;

use rustain::adapters::cli::commands::{AcpClientProfile, Cli, Command};
use rustain::domain::events::AppEvent;
use rustain::domain::ports::{SecurityPort, StoragePort, ToolSetPort, UsageLedgerPort};
use rustain::infrastructure::composition::CliCore;

// ─────────────────────────────────────────────────────────────────────
// Section 0 — shared deterministic harness (local to this file)
// ─────────────────────────────────────────────────────────────────────

/// Append a trailing newline so the newline-delimited JSON-RPC framer reads it.
fn line(mut s: String) -> String {
    if !s.ends_with('\n') {
        s.push('\n');
    }
    s
}

/// Golden initialize request (JSON-RPC, protocol version 1 = ACP LATEST).
/// Mirrors `conformance_acp.rs` so both files drive an identical handshake.
const ACP_INITIALIZE_REQ: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}"#;

/// A scripted provider whose `list_models` returns a CONFIGURABLE catalog.
///
/// The golden-transcript harness in `conformance_acp.rs` uses a provider with
/// an empty model list; the Story 14-8 model selector needs a provider that
/// actually advertises models, so the `session/new` result can be asserted to
/// carry a `model`-category config option built from them. The streamed turn
/// itself is irrelevant to the selector contract, so it replays a trivial
/// end-turn script (and is only consumed if a test also drives `session/prompt`).
struct CatalogProvider {
    models: Vec<rustain::domain::models::provider::ModelDescriptor>,
}

#[async_trait::async_trait]
impl rustain::domain::ports::StreamingProvider for CatalogProvider {
    async fn stream_completion(
        &self,
        _messages: Vec<rustain::domain::models::Message>,
        _options: rustain::domain::models::CompletionOptions,
    ) -> Result<
        futures::stream::BoxStream<'static, rustain::domain::models::StreamChunk>,
        rustain::domain::errors::ProviderError,
    > {
        use futures::StreamExt;
        use rustain::domain::models::{StopReason, StreamChunk};
        Ok(futures::stream::iter(vec![
            StreamChunk::Text {
                content: "ok".to_string(),
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
        "catalog".to_string()
    }
    fn list_models(&self) -> Vec<rustain::domain::models::provider::ModelDescriptor> {
        self.models.clone()
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
            provider_id: "catalog".to_string(),
            healthy: true,
            model_count: self.models.len(),
            display_name: "catalog".to_string(),
        }
    }
}

/// Build a model descriptor with the distinctive id/name Zed would display.
fn model(id: &str, name: &str) -> rustain::domain::models::provider::ModelDescriptor {
    use rustain::domain::models::provider::{ModelCapability, ModelDescriptor};
    ModelDescriptor {
        model_id: id.to_string(),
        display_name: name.to_string(),
        provider_id: "catalog".to_string(),
        context_window: 200_000,
        capabilities: std::collections::HashSet::from([ModelCapability::ToolUse]),
        pricing_tier: None,
        stale: false,
    }
}

/// Build a fully-wired `CliCore` whose provider advertises `models`.
///
/// Mirrors `make_core` in `conformance_acp.rs` exactly except the provider is
/// the catalog-aware one; every field the ACP agent reads stays live.
fn make_catalog_core(
    workspace: &Path,
    models: Vec<rustain::domain::models::provider::ModelDescriptor>,
) -> CliCore {
    use rustain::adapters::filesystem::FileSystemStorage;
    use rustain::adapters::noop::{
        NoOpApprovalPersistence, NoOpSecurity, NoOpToolSet, NoOpUsageLedger,
    };
    use rustain::domain::services::approval_runtime::ApprovalRuntime;
    use rustain::domain::services::tool_scheduler::ToolScheduler;

    let provider: Arc<dyn rustain::domain::ports::StreamingProvider> =
        Arc::new(CatalogProvider { models });
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

// ─────────────────────────────────────────────────────────────────────
// Section 1 — CLI `--client zed` profile parse
// ─────────────────────────────────────────────────────────────────────

/// `rustain acp --client zed` parses successfully and stays on the Acp path.
///
/// Defends the contract that the Zed client profile is an opt-in CLI flag on
/// the ACP subcommand — NOT a redirect to another subcommand, and NOT a parse
/// error. Today `Command::Acp` carries no `--client` flag, so clap rejects the
/// unknown argument and `try_parse_from` returns `Err` (RED). Once the flag
/// lands the parse succeeds and the command remains `Some(Command::Acp)`.
///
/// A regression that removes the flag reddens the `is_ok()` assertion; a
/// regression that makes `--client` consume the subcommand position reddens
/// the `matches!` assertion.
#[test]
fn acp_client_zed_flag_parses_and_stays_on_acp_subcommand() {
    let parsed = Cli::try_parse_from(["rustain", "acp", "--client", "zed"]);
    assert!(
        parsed.is_ok(),
        "`rustain acp --client zed` must parse; got: {:?}",
        parsed.err()
    );
    let cli = parsed.expect("parsed cli");
    assert!(
        matches!(
            cli.command,
            Some(Command::Acp {
                client: AcpClientProfile::Zed,
                ..
            })
        ),
        "`rustain acp --client zed` must parse to the Acp subcommand with the Zed profile; got {:?}",
        cli.command
    );
}

/// Bare `rustain acp` (no `--client`) still parses to ACP — auto/default path.
///
/// Positive control for the flag being OPTIONAL: the Zed profile is opt-in, so
/// omitting `--client` must keep ACP a valid invocation (the client profile
/// then defaults/auto-detects). Keeps the parse boundary honest while the
/// explicit-flag test above defends the opt-in.
#[test]
fn acp_without_client_flag_still_parses_to_acp() {
    let parsed = Cli::try_parse_from(["rustain", "acp"]);
    assert!(
        parsed.is_ok(),
        "bare `rustain acp` must still parse; got: {:?}",
        parsed.err()
    );
    let cli = parsed.expect("parsed cli");
    assert!(
        matches!(
            cli.command,
            Some(Command::Acp {
                client: AcpClientProfile::Auto,
                ..
            })
        ),
        "bare `rustain acp` must remain Acp and default the client profile to Auto; got {:?}",
        cli.command
    );
}

// ─────────────────────────────────────────────────────────────────────
// Section 2 — `session/new` model selector over dispatched JSON-RPC
// ─────────────────────────────────────────────────────────────────────

/// Drive `initialize → session/new` over the in-memory ACP seam and return the
/// parsed `session/new` result object (the JSON value at `result`).
///
/// Determinism: scripted provider (no network), in-memory duplex transport
/// (no listener), deterministic first session id `acp-1`. The server future is
/// `!Send`, so it is polled on this thread via `select!` against the client
/// driver — exactly the shape `conformance_acp.rs` uses.
async fn drive_session_new_result(
    models: Vec<rustain::domain::models::provider::ModelDescriptor>,
) -> serde_json::Value {
    use std::rc::Rc;
    use std::time::Duration;

    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
    use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

    use rustain::adapters::acp::agent::CoreFactory;
    use rustain::adapters::acp::run::serve_acp_with_core_factory;
    use rustain::domain::models::AppConfig;

    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let ws_for_factory = workspace.path().to_path_buf();
    let core_factory: CoreFactory = Rc::new(move |_cwd: &Path, _mcp_servers| {
        Ok(make_catalog_core(&ws_for_factory, models.clone()))
    });

    let (server_incoming, mut client_write) = tokio::io::duplex(8 * 1024);
    let (mut client_read, server_outgoing) = tokio::io::duplex(8 * 1024);

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
        client_write.flush().await.expect("flush");

        let reader = tokio::io::BufReader::new(&mut client_read);
        let mut lines = reader.lines();
        loop {
            let next = tokio::time::timeout(Duration::from_secs(10), lines.next_line()).await;
            match next {
                Ok(Ok(Some(raw))) => {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
                        if v["id"] == serde_json::json!(2) {
                            return v["result"].clone();
                        }
                    }
                }
                _ => return serde_json::Value::Null,
            }
        }
    };

    tokio::select! {
        result = drive => result,
        server_res = server => {
            panic!("ACP server exited before session/new completed: {server_res:?}");
        }
    }
}

/// A non-empty provider registry makes `session/new` advertise a `model`-
/// category config option whose choices are EXACTLY the registered model ids.
///
/// Defends the contract Zed's model dropdown keys off: the agent must surface
/// the provider's catalog as a `select` config option (category `model`) on
/// `session/new`, with one choice per registered model and a default
/// `currentValue` that is one of those choices.
///
/// Non-vacuity: today `new_session` returns `NewSessionResponse::new(id)` with
/// `config_options == None`, so `result.configOptions` is absent on the wire —
/// the "configOptions is an array" assertion reddens immediately. A mutant
/// that advertises a model option but with the WRONG choices (e.g. hardcoded
/// ids, or only the first) reddens the per-id presence checks. A mutant that
/// sets `currentValue` to a non-listed id reddens the membership check.
///
/// Determinism: scripted catalog provider (no network), in-memory duplex
/// transport (no listener), deterministic first session id.
#[tokio::test(flavor = "current_thread")]
async fn session_new_advertises_model_selector_for_nonempty_registry() {
    let result = drive_session_new_result(vec![
        model("zed-alpha", "Zed Alpha"),
        model("zed-bravo", "Zed Bravo"),
    ])
    .await;
    assert!(
        !result.is_null(),
        "session/new must return a result before we can inspect config_options"
    );

    let config_options = result
        .get("configOptions")
        .and_then(serde_json::Value::as_array);
    assert!(
        config_options.is_some(),
        "session/new result must carry a `configOptions` array for a non-empty model registry; \
         got result = {result}"
    );

    // Find the model-category select option.
    let model_option = config_options
        .expect("configOptions array present")
        .iter()
        .find(|o| o["category"] == serde_json::json!("model"));

    let model_option = match model_option {
        Some(o) => o.clone(),
        None => panic!(
            "session/new configOptions must include a `model`-category option; got = {:?}",
            config_options
        ),
    };

    assert_eq!(
        model_option["type"],
        serde_json::json!("select"),
        "the model config option must be a `select` (dropdown) kind"
    );

    // The advertised choices must be EXACTLY the registered model ids.
    let choice_ids: Vec<String> = model_option["options"]
        .as_array()
        .expect("select option has an `options` array")
        .iter()
        .filter_map(|o| o["value"].as_str().map(str::to_owned))
        .collect();
    assert!(
        choice_ids.iter().any(|c| c == "catalog:zed-alpha"),
        "model selector must list `catalog:zed-alpha` (provider_id:model_id — Story AC1/AC5) among its choices; got {choice_ids:?}"
    );
    assert!(
        choice_ids.iter().any(|c| c == "catalog:zed-bravo"),
        "model selector must list `catalog:zed-bravo` (provider_id:model_id — Story AC1/AC5) among its choices; got {choice_ids:?}"
    );

    // The default selection must be one of the advertised choices (not a
    // phantom id).
    let current = model_option["currentValue"].as_str().unwrap_or("");
    assert!(
        choice_ids.iter().any(|c| c == current),
        "model selector `currentValue` ({current:?}) must be one of the choices {choice_ids:?}"
    );
}

/// An empty provider registry advertises NO model-category config option.
///
/// Defends the contract that the model selector is registry-driven: with no
/// models there is nothing to select, so the agent MUST NOT surface a model
/// dropdown (Zed renders an empty selector as a broken control). This is the
/// contrast half of the selector contract — paired with the non-empty test it
/// proves the option is conditional on the catalog, not unconditionally
/// present.
///
/// Non-vacuity: today this is already vacuously true (no `config_options` at
/// all), so it is a GUARD against the production impl unconditionally
/// appending a (possibly empty) model option. A mutant that always advertises
/// a model option reddens the absence assertion.
#[tokio::test(flavor = "current_thread")]
async fn session_new_omits_model_selector_for_empty_registry() {
    let result = drive_session_new_result(Vec::new()).await;
    assert!(
        !result.is_null(),
        "session/new must return a result before we can inspect config_options"
    );

    let has_model_option = result
        .get("configOptions")
        .and_then(serde_json::Value::as_array)
        .map(|opts| {
            opts.iter()
                .any(|o| o["category"] == serde_json::json!("model"))
        })
        .unwrap_or(false);

    assert!(
        !has_model_option,
        "session/new must NOT advertise a `model`-category config option when the registry is \
         empty; got result = {result}"
    );
}

// ─────────────────────────────────────────────────────────────────────
// Section 3 — `session/set_config_option` switch + invalid-value contrast
// ─────────────────────────────────────────────────────────────────────

/// Drive `initialize → session/new → session/set_config_option` over the
/// in-memory ACP seam and return the full `session/set_config_option`
/// response envelope (the JSON object with `id == 3`), so callers can inspect
/// either `result` or `error`.
///
/// `value` is sent verbatim as the config option value (e.g. a model id).
/// Determinism: scripted catalog provider (no network), in-memory duplex
/// transport (no listener), deterministic first session id `acp-1`.
async fn drive_set_config_option_response(
    models: Vec<rustain::domain::models::provider::ModelDescriptor>,
    config_id: &str,
    value: &str,
) -> serde_json::Value {
    use std::rc::Rc;
    use std::time::Duration;

    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
    use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

    use rustain::adapters::acp::agent::CoreFactory;
    use rustain::adapters::acp::run::serve_acp_with_core_factory;
    use rustain::domain::models::AppConfig;

    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let ws_for_factory = workspace.path().to_path_buf();
    let core_factory: CoreFactory = Rc::new(move |_cwd: &Path, _mcp_servers| {
        Ok(make_catalog_core(&ws_for_factory, models.clone()))
    });

    let (server_incoming, mut client_write) = tokio::io::duplex(8 * 1024);
    let (mut client_read, server_outgoing) = tokio::io::duplex(8 * 1024);

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
        let set_opt = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "session/set_config_option",
            "params": {
                "sessionId": "acp-1",
                "configId": config_id,
                "value": value,
            }
        });
        client_write
            .write_all(line(set_opt.to_string()).as_bytes())
            .await
            .expect("write set_config_option");
        client_write.flush().await.expect("flush");

        let reader = tokio::io::BufReader::new(&mut client_read);
        let mut lines = reader.lines();
        loop {
            let next = tokio::time::timeout(Duration::from_secs(10), lines.next_line()).await;
            match next {
                Ok(Ok(Some(raw))) => {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
                        if v["id"] == serde_json::json!(3) {
                            return v;
                        }
                    }
                }
                _ => return serde_json::Value::Null,
            }
        }
    };

    tokio::select! {
        envelope = drive => envelope,
        server_res = server => {
            panic!("ACP server exited before set_config_option completed: {server_res:?}");
        }
    }
}

/// A valid `session/set_config_option` switch for the `model` config updates
/// the selector's `currentValue` to the chosen option.
///
/// Defends the contract Zed exercises when the user picks a different model
/// from the dropdown: the agent acknowledges the switch by returning the FULL
/// config-option set with the model option's `currentValue` set to the new id.
/// The response is the source of truth for the dropdown's new state.
///
/// Non-vacuity: today the `acp::Agent` default impl returns `method_not_found`
/// for `set_session_config_option`, so the envelope carries an `error` and no
/// `result` — the `result` inspection reddens immediately. A mutant that
/// accepts the request but does NOT mutate `currentValue` reddens the equality
/// check; a mutant that echoes the REQUESTED value without validating it is a
/// listed model reddens the (paired) invalid-value test.
#[tokio::test(flavor = "current_thread")]
async fn set_session_config_option_switches_model_to_chosen_value() {
    let envelope = drive_set_config_option_response(
        vec![
            model("zed-alpha", "Zed Alpha"),
            model("zed-bravo", "Zed Bravo"),
        ],
        "model",
        "catalog:zed-bravo",
    )
    .await;
    assert!(
        envelope.get("error").is_none(),
        "a valid session/set_config_option must not return an error; got {envelope}"
    );
    let result = &envelope["result"];
    assert!(!result.is_null(), "set_config_option must return a result");

    let model_option = result["configOptions"]
        .as_array()
        .and_then(|opts| {
            opts.iter()
                .find(|o| o["category"] == serde_json::json!("model"))
                .cloned()
        })
        .unwrap_or_else(|| {
            panic!(
                "set_config_option result must include the `model` config option; got result = {result}"
            )
        });

    assert_eq!(
        model_option["currentValue"],
        serde_json::json!("catalog:zed-bravo"),
        "switching to catalog:zed-bravo must update the model selector currentValue; got {model_option}"
    );
    // The choice set itself is unchanged by a switch.
    let still_lists_alpha = model_option["options"]
        .as_array()
        .map(|opts| {
            opts.iter()
                .any(|o| o["value"] == serde_json::json!("catalog:zed-alpha"))
        })
        .unwrap_or(false);
    assert!(
        still_lists_alpha,
        "switching the selection must not drop other choices from the list; got {model_option}"
    );
}

/// An INVALID `session/set_config_option` value for the `model` config is
/// rejected and leaves the PREVIOUS selection intact.
///
/// Defends the contract that the selector only accepts advertised choices: a
/// value that is not among the model option's listed ids must NOT silently
/// become the current selection (Zed would then display a phantom model and
/// route turns to a non-existent model). The response must either be an error
/// OR a result whose model `currentValue` is still a real listed id — never
/// the bogus one.
///
/// Non-vacuity: today the method returns `method_not_found` (error), so the
/// "no result echoes the bogus value" check passes vacuously; the paired
/// valid-switch test carries the red weight. Once implemented, a mutant that
/// accepts any string as `currentValue` reddens the `assert_ne!`.
#[tokio::test(flavor = "current_thread")]
async fn set_session_config_option_rejects_unknown_value_and_preserves_previous() {
    let envelope = drive_set_config_option_response(
        vec![
            model("zed-alpha", "Zed Alpha"),
            model("zed-bravo", "Zed Bravo"),
        ],
        "model",
        "does-not-exist-model",
    )
    .await;

    // If the agent honored the request at all, the model option's currentValue
    // must NEVER be the bogus id. An error response (no result) trivially
    // satisfies this; a successful response must preserve a real listed id.
    if let Some(result) = envelope.get("result") {
        if !result.is_null() {
            let model_option = result["configOptions"]
                .as_array()
                .and_then(|opts| {
                    opts.iter()
                        .find(|o| o["category"] == serde_json::json!("model"))
                        .cloned()
                })
                .unwrap_or_else(|| panic!("model config option missing from result {result}"));
            assert_ne!(
                model_option["currentValue"],
                serde_json::json!("does-not-exist-model"),
                "an unknown model value must never become the current selection; got {model_option}"
            );
            // And it must remain one of the real choices.
            let choice_ids: Vec<String> = model_option["options"]
                .as_array()
                .expect("select option has choices")
                .iter()
                .filter_map(|o| o["value"].as_str().map(str::to_owned))
                .collect();
            let current = model_option["currentValue"].as_str().unwrap_or("");
            assert!(
                choice_ids.iter().any(|c| c == current),
                "preserved currentValue ({current:?}) must still be a listed choice {choice_ids:?}"
            );
        }
    }
    // No assertion when `error` is present: rejecting the bad value as an error
    // is a valid implementation of the contract.
}

// ─────────────────────────────────────────────────────────────────────
// Section 4 — skill advertisement + activation over dispatched JSON-RPC
// ─────────────────────────────────────────────────────────────────────
//
// These tests use the `AcpCoreFactory` seam (`serve_acp_with_acp_core_factory_and_node_tree`)
// rather than the `CliCore`-converting seam, because skills live on the
// `AcpCore.skill_activator` — which `AcpCore::from(CliCore)` builds EMPTY. To
// exercise skill advertisement/activation we pre-populate a `SkillRegistry`
// (`SkillRegistry::from_skills`, the integration-test helper) and hand the
// agent an `AcpCore` whose activator actually knows about a skill.

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::RwLock as StdRwLock;
use std::time::Duration;

use rustain::adapters::acp::agent::AcpCoreFactory;
use rustain::adapters::acp::run::serve_acp_with_acp_core_factory_and_node_tree;
use rustain::adapters::skill_activation::SkillActivator;
use rustain::adapters::skill_registry::SkillRegistry;
use rustain::domain::models::{SkillDef, SkillSource};
use rustain::infrastructure::composition::AcpCore;
use rustain::infrastructure::subagent::NodeTree;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

/// A workspace fixture skill — distinct name/body so assertions are unambiguous.
fn fixture_skill(name: &str) -> SkillDef {
    SkillDef {
        name: name.to_string(),
        description: format!("Fixture skill {name} for ACP conformance"),
        file: PathBuf::from(format!(".agents/skills/{name}.md")),
        directory: PathBuf::from(format!(".agents/skills/{name}")),
        source: SkillSource::WorkspaceAgents,
        allowed_tools: None,
        terse: None,
    }
}

/// Build an `AcpCore` whose `skill_activator` is pre-populated with `skills`.
///
/// Starts from the catalog `CliCore` (so provider/models/noop deps stay live),
/// converts to `AcpCore`, then OVERWRITES the empty activator with one backed
/// by `SkillRegistry::from_skills`. This is the only seam that lets a test
/// hand the agent a non-empty skill set without filesystem discovery.
fn make_acp_core_with_skills(
    workspace: &Path,
    models: Vec<rustain::domain::models::provider::ModelDescriptor>,
    skills: Vec<SkillDef>,
) -> AcpCore {
    let mut acp_core = AcpCore::from(make_catalog_core(workspace, models));
    let registry = SkillRegistry::from_skills(skills);
    acp_core.skill_activator = Arc::new(SkillActivator::with_registry(Arc::new(
        tokio::sync::RwLock::new(registry),
    )));
    acp_core
}

/// A provider that records the `system_prompt` it receives on each turn, then
/// streams a trivial end-turn. The recorded snapshots let a test assert —
/// outside the spawned task — whether a skill body reached the model's system
/// prompt. Post-hoc capture (not inside the provider) so an activation bug
/// surfaces as a clean failure rather than a swallowed panic.
struct SystemPromptCapturingProvider {
    captured: Arc<StdRwLock<Vec<String>>>,
}

#[async_trait::async_trait]
impl rustain::domain::ports::StreamingProvider for SystemPromptCapturingProvider {
    async fn stream_completion(
        &self,
        _messages: Vec<rustain::domain::models::Message>,
        options: rustain::domain::models::CompletionOptions,
    ) -> Result<
        futures::stream::BoxStream<'static, rustain::domain::models::StreamChunk>,
        rustain::domain::errors::ProviderError,
    > {
        use futures::StreamExt;
        use rustain::domain::models::{StopReason, StreamChunk};
        if let Ok(mut guard) = self.captured.write() {
            guard.push(options.system_prompt.clone());
        }
        Ok(futures::stream::iter(vec![
            StreamChunk::Text {
                content: "ok".to_string(),
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
        "capturing".to_string()
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
            provider_id: "capturing".to_string(),
            healthy: true,
            model_count: 0,
            display_name: "capturing".to_string(),
        }
    }
}

/// Post-fence drain bound: comfortably exceeds the advertisement delay so a
/// legitimately-deferred advertisement is still collected, while bounding the
/// no-skills absence case to the fence. Under the caller's paused test clock
/// this duration is virtual and costs no wall-clock time.
const ADVERTISEMENT_DRAIN_BOUND: Duration = Duration::from_millis(600);

/// Drive `initialize → session/new` followed by a DD-3 fence request, and
/// return the `session/new` result plus every `available_commands_update`
/// notification observed. Lets callers assert on the advertisement without
/// racing the result.
///
/// Termination (DD-3): the follow-up `session/set_config_option` (id:3) is the
/// fence — its response (success OR error) proves `session/new` processing ran
/// to completion. Because the advertisement is DEFERRED past the response (and
/// past the fence) by Story 14.8b, the loop keeps draining after the fence for
/// one advertisement-delay window; under the caller's paused test clock that
/// drain is virtual, so the no-skills absence branch terminates at the fence
/// instead of idling on a real wall-clock timeout. The presence branch breaks
/// the instant the target advertisement is collected.
///
/// Fail-loud: malformed JSON and an id:2 error response abort here rather than
/// being swallowed as `Null` or stalling to a timeout.
async fn drive_handshake_collect_notifications(
    acp_factory: AcpCoreFactory,
) -> (serde_json::Value, Vec<serde_json::Value>) {
    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let (server_incoming, mut client_write) = tokio::io::duplex(8 * 1024);
    let (mut client_read, server_outgoing) = tokio::io::duplex(8 * 1024);
    let node_tree = NodeTree::with_now_fn(Arc::new(|| 1_700_000_000_000));

    let server = serve_acp_with_acp_core_factory_and_node_tree(
        server_outgoing.compat_write(),
        server_incoming.compat(),
        rustain::domain::models::AppConfig::default(),
        None,
        acp_factory,
        node_tree,
        workspace.path().to_path_buf(),
        rustain::adapters::acp::run::deterministic_acp_id_source(),
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
        // DD-3 fence: a follow-up request whose response (success OR error)
        // proves session/new processing completed. Bounds the no-skills absence
        // case deterministically rather than a 10s idle drain.
        let fence = serde_json::json!({
            "jsonrpc": "2.0", "id": 3, "method": "session/set_config_option",
            "params": { "sessionId": "acp-1", "configId": "model", "value": "x:y" }
        });
        client_write
            .write_all(line(fence.to_string()).as_bytes())
            .await
            .expect("write fence");
        client_write.flush().await.expect("flush");

        let reader = tokio::io::BufReader::new(&mut client_read);
        let mut lines = reader.lines();
        let mut result = serde_json::Value::Null;
        let mut notifications = Vec::new();
        let mut fence_seen = false;
        loop {
            // After the fence, the only thing left to wait for is the deferred
            // advertisement; bound that wait by the advertisement delay so
            // absence terminates at the fence. Under the paused test clock this
            // bound is virtual (no wall-clock cost).
            let line_bound = if fence_seen {
                ADVERTISEMENT_DRAIN_BOUND
            } else {
                Duration::from_secs(10)
            };
            let next = tokio::time::timeout(line_bound, lines.next_line()).await;
            let raw = match next {
                Ok(Ok(Some(raw))) => raw,
                _ => break, // EOF, or post-fence drain found no advertisement (absence)
            };
            // Fail loud on malformed frames rather than swallowing as Null.
            let v: serde_json::Value =
                serde_json::from_str(&raw).expect("malformed JSON-RPC frame from server");
            if v["id"] == serde_json::json!(2) {
                assert!(
                    v.get("result").is_some(),
                    "session/new (id:2) returned an error response: {v}"
                );
                result = v["result"].clone();
            } else if v["id"] == serde_json::json!(3) {
                // DD-3 fence response (success OR error). Do NOT break: the
                // advertisement is deferred past the fence, so keep draining.
                fence_seen = true;
            } else if v["method"] == serde_json::json!("session/update") {
                if v["params"]["update"]["sessionUpdate"]
                    == serde_json::json!("available_commands_update")
                {
                    notifications.push(v);
                    break; // presence: collected the target advertisement
                }
                // Unrelated post-response session/update: ignore, keep draining.
            }
        }
        (result, notifications)
    };

    tokio::select! {
        outcome = drive => outcome,
        server_res = server => {
            panic!("ACP server exited before session/new completed: {server_res:?}");
        }
    }
}

/// `available_commands_update` is advertised after `session/new` even when no
/// skills are enabled because Story 14.10 adds the built-in `/init` command.
///
/// Defends the contract Zed's slash-command palette keys off: the agent always
/// advertises the built-in command `init`, prepends it before skill commands,
/// and still advertises enabled skills alongside it. The no-skills branch proves
/// the update is not skill-gated anymore and that R1 advertises no extra
/// built-ins such as `/mode`, `/model`, `/clear`, or `/compact`.
///
/// Non-vacuity: a mutant that removes `builtin_commands()` reddens the
/// no-skills assertion; a mutant that keeps the old skill-gate reddens both
/// branches; a mutant that advertises extra built-ins reddens the exact
/// no-skills command-name assertion.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn available_commands_update_always_advertises_builtin_init() {
    // ── With skills: an available_commands_update carrying a skill command. ──
    let ws = tempfile::tempdir().expect("workspace tempdir");
    let ws_path = ws.path().to_path_buf();
    let skills = vec![fixture_skill("zed-skill-a")];
    let factory: AcpCoreFactory = Rc::new(move |_cwd, _mcp_servers| {
        Ok(make_acp_core_with_skills(
            &ws_path,
            Vec::new(),
            skills.clone(),
        ))
    });
    let (_result, notifications) = drive_handshake_collect_notifications(factory).await;

    let mut advertised_names: Vec<String> = Vec::new();
    for n in &notifications {
        let update = &n["params"]["update"];
        if update["sessionUpdate"] == serde_json::json!("available_commands_update") {
            if let Some(cmds) = update["availableCommands"].as_array() {
                for c in cmds {
                    if let Some(name) = c["name"].as_str() {
                        advertised_names.push(name.to_string());
                    }
                }
            }
        }
    }
    assert!(
        advertised_names.iter().any(|n| n == "init"),
        "session/new must always advertise built-in command `init`; \
         got advertised commands = {advertised_names:?}, notifications = {notifications:?}"
    );
    assert!(
        advertised_names.iter().any(|n| n == "zed-skill-a"),
        "session/new must still advertise enabled skill `zed-skill-a`; \
         got advertised commands = {advertised_names:?}, notifications = {notifications:?}"
    );

    // ── Without skills: the built-in `/init` command is still advertised. ──
    let ws2 = tempfile::tempdir().expect("workspace tempdir");
    let ws2_path = ws2.path().to_path_buf();
    let factory_empty: AcpCoreFactory = Rc::new(move |_cwd, _mcp_servers| {
        Ok(make_acp_core_with_skills(&ws2_path, Vec::new(), Vec::new()))
    });
    let (_result2, notifications2) = drive_handshake_collect_notifications(factory_empty).await;
    let mut advertised_without_skills: Vec<(String, String)> = Vec::new();
    for n in &notifications2 {
        let update = &n["params"]["update"];
        if update["sessionUpdate"] == serde_json::json!("available_commands_update") {
            if let Some(cmds) = update["availableCommands"].as_array() {
                for c in cmds {
                    if let Some(name) = c["name"].as_str() {
                        advertised_without_skills.push((
                            name.to_string(),
                            c["description"].as_str().unwrap_or("").to_string(),
                        ));
                    }
                }
            }
        }
    }
    assert_eq!(
        advertised_without_skills,
        vec![(
            "init".to_string(),
            "create a context file with instructions for rustain.".to_string()
        )],
        "no-skills sessions must advertise exactly the built-in `/init` command"
    );
}

// ─────────────────────────────────────────────────────────────────────
// Section 4a — `available_commands_update` WIRE ORDER (Story 14.8b AC-1)
// ─────────────────────────────────────────────────────────────────────

/// One-line summary of a raw JSON-RPC frame, for readable assertion failures.
fn frame_summary(v: &serde_json::Value) -> String {
    if let Some(id) = v.get("id") {
        let kind = if v.get("result").is_some() {
            "result"
        } else {
            "error"
        };
        return format!("id:{id} ({kind})");
    }
    if let Some(method) = v.get("method").and_then(serde_json::Value::as_str) {
        let update = &v["params"]["update"]["sessionUpdate"];
        if update.is_string() {
            return format!("{method} [{update}]");
        }
        return method.to_string();
    }
    format!("{}", v)
}

/// The `session/new` response frame MUST appear on the wire BEFORE the
/// `available_commands_update` notification (Story 14.8b root-cause fix).
///
/// Zed only registers a session once it receives the `NewSessionResponse`
/// carrying the `sessionId`; an `available_commands_update` for a session Zed
/// does not yet know is DROPPED, leaving the slash-command palette empty
/// ("Available commands: none"). The pre-fix code emitted the notification
/// from INSIDE `new_session` (with an awaited ack), forcing it into the SDK's
/// single FIFO `outgoing_tx` BEFORE the response — so this assertion is RED on
/// the pre-fix code and GREEN only once the emit is deferred past the response.
///
/// Non-vacuity (DD-2): this test PINS the SDK ordering invariant — the request
/// handler enqueues the response synchronously, with no intervening `.await`,
/// the instant the handler future resolves (`rpc.rs:285-290`). A future SDK
/// bump that inserts an await there reddens this test BY DESIGN: it is the
/// canary, replacing codex's untested 200 ms sleep with a tested guarantee.
///
/// Determinism (DD-3): the read is bounded by a fence — a follow-up
/// `session/set_config_option(id:3)` request whose response (present whether
/// the switch succeeds or errors) proves session processing ran to
/// completion. No wall-clock sleep, no flaky idle-drain; a per-line safety
/// timeout (matching the rest of the suite) bounds a stuck transport. Per
/// DD-7 the assertion constrains ordering only relative to the `session/new`
/// *response* — interleaving with later frames is legal and unchecked.
#[tokio::test(flavor = "current_thread")]
async fn available_commands_update_arrives_after_session_new_response() {
    let ws = tempfile::tempdir().expect("workspace tempdir");
    let ws_path = ws.path().to_path_buf();
    let skills = vec![fixture_skill("zed-order-skill")];
    let models = vec![model("zed-alpha", "Zed Alpha")];
    let factory: AcpCoreFactory = Rc::new(move |_cwd, _mcp_servers| {
        Ok(make_acp_core_with_skills(
            &ws_path,
            models.clone(),
            skills.clone(),
        ))
    });

    let (server_incoming, mut client_write) = tokio::io::duplex(8 * 1024);
    let (mut client_read, server_outgoing) = tokio::io::duplex(8 * 1024);
    let node_tree = NodeTree::with_now_fn(Arc::new(|| 1_700_000_000_000));

    let server = serve_acp_with_acp_core_factory_and_node_tree(
        server_outgoing.compat_write(),
        server_incoming.compat(),
        rustain::domain::models::AppConfig::default(),
        None,
        factory,
        node_tree,
        ws.path().to_path_buf(),
        rustain::adapters::acp::run::deterministic_acp_id_source(),
    );

    let drive = async {
        client_write
            .write_all(line(ACP_INITIALIZE_REQ.to_string()).as_bytes())
            .await
            .expect("write initialize");
        let session_new = serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/new",
            "params": { "cwd": ws.path(), "mcpServers": [] },
        });
        client_write
            .write_all(line(session_new.to_string()).as_bytes())
            .await
            .expect("write session/new");
        // DD-3 fence: a follow-up request whose response marks that session
        // processing completed. `set_config_option` emits NO notification, so
        // its id:3 response is a deterministic read bound — not a guess.
        let set_opt = serde_json::json!({
            "jsonrpc": "2.0", "id": 3, "method": "session/set_config_option",
            "params": { "sessionId": "acp-1", "configId": "model", "value": "catalog:zed-alpha" }
        });
        client_write
            .write_all(line(set_opt.to_string()).as_bytes())
            .await
            .expect("write set_config_option");
        client_write.flush().await.expect("flush");

        let reader = tokio::io::BufReader::new(&mut client_read);
        let mut lines = reader.lines();
        // Record every frame in arrival order until we hold the three markers
        // the assertion keys off (id:2 response, id:3 fence, the update), or
        // the transport stalls. Indices are arrival order on the wire.
        let mut frames: Vec<(usize, serde_json::Value)> = Vec::new();
        let mut have_id2 = false;
        let mut have_id3 = false;
        let mut have_update = false;
        for idx in 0..32usize {
            let next = tokio::time::timeout(Duration::from_secs(10), lines.next_line()).await;
            let raw = match next {
                Ok(Ok(Some(raw))) => raw,
                _ => break,
            };
            let v: serde_json::Value =
                serde_json::from_str(&raw).expect("malformed JSON-RPC frame from server");
            if v["id"] == serde_json::json!(2) {
                assert!(
                    v.get("result").is_some(),
                    "session/new (id:2) returned an error response: {v}"
                );
                have_id2 = true;
            }
            if v["id"] == serde_json::json!(3) {
                have_id3 = true;
            }
            if v["method"] == serde_json::json!("session/update")
                && v["params"]["update"]["sessionUpdate"]
                    == serde_json::json!("available_commands_update")
            {
                have_update = true;
            }
            frames.push((idx, v));
            if have_id2 && have_id3 && have_update {
                break;
            }
        }
        frames
    };

    let frames = tokio::select! {
        f = drive => f,
        server_res = server => {
            panic!("ACP server exited before ordering read completed: {server_res:?}");
        }
    };

    let traced: Vec<String> = frames
        .iter()
        .map(|(i, v)| format!("[{i}] {}", frame_summary(v)))
        .collect();

    let id2_idx = frames
        .iter()
        .find(|(_, v)| v["id"] == serde_json::json!(2))
        .map(|(i, _)| *i)
        .unwrap_or_else(|| {
            panic!(
                "never saw session/new (id:2) response;\nframes:\n{}",
                traced.join("\n")
            )
        });
    let id3_idx = frames
        .iter()
        .find(|(_, v)| v["id"] == serde_json::json!(3))
        .map(|(i, _)| *i);
    assert!(
        id3_idx.is_some(),
        "fence response (id:3) never arrived — session processing did not complete;\nframes:\n{}",
        traced.join("\n")
    );
    let update_idx = frames
        .iter()
        .find(|(_, v)| {
            v["method"] == serde_json::json!("session/update")
                && v["params"]["update"]["sessionUpdate"]
                    == serde_json::json!("available_commands_update")
        })
        .map(|(i, _)| *i)
        .unwrap_or_else(|| {
            panic!(
                "never saw available_commands_update for session acp-1;\nframes:\n{}",
                traced.join("\n")
            )
        });

    assert!(
        id2_idx < update_idx,
        "AC-1 WIRE-ORDER VIOLATION: the session/new (id:2) response MUST appear on the wire \
         BEFORE the available_commands_update notification (Zed drops updates for sessions it \
         does not yet know). got id:2 at [{id2_idx}], update at [{update_idx}];\nframes:\n{}",
        traced.join("\n")
    );
}

// ─────────────────────────────────────────────────────────────────────
// Section 4b — `available_commands_update` DEFERRAL (Story 14.8b host-smoke)
// ─────────────────────────────────────────────────────────────────────

/// The `available_commands_update` notification is DEFERRED by the codex-style
/// delay after the `session/new` response — the host-smoke-proven behavior the
/// real (async) Zed client needs.
///
/// Wire order alone (Section 4a) is NECESSARY but NOT SUFFICIENT: Zed still
/// reports "Available commands: none" when the update lands immediately after
/// the response, because its session layer has not finished registering the
/// session. codex (proven in Zed) defers via a spawned task + delay; this test
/// pins that the deferral is actually applied.
///
/// Non-vacuity: this is the regression canary for "immediate fire-and-forget."
/// A mutant that drops the `sleep` (sending the update the instant
/// `new_session` returns) yields a measured gap of ~0 and reddens the
/// `>= SKILL_ADVERTISEMENT_DELAY` assertion. The delayed implementation
/// measures exactly the delay.
///
/// Determinism: the test runs on a paused clock, so `tokio::time::sleep`
/// auto-advances virtual time — the gap is measured with `tokio::time::Instant`
/// (the virtual clock), not wall-clock, and costs no real time. No `sleep`
/// races, no flake.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn available_commands_update_deferred_after_session_new_response() {
    use rustain::adapters::acp::agent::SKILL_ADVERTISEMENT_DELAY;

    let ws = tempfile::tempdir().expect("workspace tempdir");
    let ws_path = ws.path().to_path_buf();
    let skills = vec![fixture_skill("zed-defer-skill")];
    let factory: AcpCoreFactory = Rc::new(move |_cwd, _mcp_servers| {
        Ok(make_acp_core_with_skills(
            &ws_path,
            Vec::new(),
            skills.clone(),
        ))
    });

    let (server_incoming, mut client_write) = tokio::io::duplex(8 * 1024);
    let (mut client_read, server_outgoing) = tokio::io::duplex(8 * 1024);
    let node_tree = NodeTree::with_now_fn(Arc::new(|| 1_700_000_000_000));

    let server = serve_acp_with_acp_core_factory_and_node_tree(
        server_outgoing.compat_write(),
        server_incoming.compat(),
        rustain::domain::models::AppConfig::default(),
        None,
        factory,
        node_tree,
        ws.path().to_path_buf(),
        rustain::adapters::acp::run::deterministic_acp_id_source(),
    );

    let drive = async {
        client_write
            .write_all(line(ACP_INITIALIZE_REQ.to_string()).as_bytes())
            .await
            .expect("write initialize");
        let session_new = serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/new",
            "params": { "cwd": ws.path(), "mcpServers": [] },
        });
        client_write
            .write_all(line(session_new.to_string()).as_bytes())
            .await
            .expect("write session/new");
        client_write.flush().await.expect("flush");

        let reader = tokio::io::BufReader::new(&mut client_read);
        let mut lines = reader.lines();
        let mut response_at: Option<tokio::time::Instant> = None;
        let mut update_at: Option<tokio::time::Instant> = None;
        for _ in 0..32usize {
            let next = tokio::time::timeout(Duration::from_secs(10), lines.next_line()).await;
            let raw = match next {
                Ok(Ok(Some(raw))) => raw,
                _ => break,
            };
            let v: serde_json::Value =
                serde_json::from_str(&raw).expect("malformed JSON-RPC frame from server");
            if v["id"] == serde_json::json!(2) {
                assert!(
                    v.get("result").is_some(),
                    "session/new (id:2) returned an error response: {v}"
                );
                response_at = Some(tokio::time::Instant::now());
            } else if v["method"] == serde_json::json!("session/update")
                && v["params"]["update"]["sessionUpdate"]
                    == serde_json::json!("available_commands_update")
            {
                update_at = Some(tokio::time::Instant::now());
                break;
            }
            if response_at.is_some() && update_at.is_some() {
                break;
            }
        }
        (response_at, update_at)
    };

    let (response_at, update_at) = tokio::select! {
        r = drive => r,
        server_res = server => {
            panic!("ACP server exited before deferral read completed: {server_res:?}");
        }
    };

    let response_at = response_at.expect("never saw session/new (id:2) response");
    let update_at =
        update_at.expect("never saw available_commands_update after the session/new response");
    // `update_at >= response_at` holds (the advertisement lands after the
    // response on the wire), so `duration_since` cannot underflow.
    let elapsed = update_at.duration_since(response_at);
    assert!(
        elapsed >= SKILL_ADVERTISEMENT_DELAY,
        "available_commands_update must be DEFERRED by at least {:?} after the session/new \
         response (host-smoke-proven codex behavior the real async Zed client needs; without \
         the deferral Zed drops the update and reports 'Available commands: none'). measured \
         gap = {elapsed:?} — a near-zero gap means the spawn_local sleep was removed \
         (immediate fire-and-forget regression).",
        SKILL_ADVERTISEMENT_DELAY,
    );
}

/// Invoking a skill via the prompt injects the skill body into the provider's
/// system prompt — proving the ACP path actually activates skills rather than
/// silently dropping them.
///
/// Defends the contract that `/skill <name>` (the form Zed sends when a user
/// picks a command from the palette) drives the shared skill activator and
/// the assembled system prompt the model receives carries the activated skill
/// body. Today `run_prompt` assembles the prompt from an EMPTY
/// `SkillActivationSet`, so the captured system prompt contains no skill body
/// — the assertion reddens.
///
/// Non-vacuity: the system prompt is read from the REAL provider call (the
/// `SystemPromptCapturingProvider` records the exact `CompletionOptions`
/// handed to `stream_completion`), so a mutant that parses the command but
/// forgets to inject the body reddens the contains-check. The skill name
/// `zed-skill-a` is distinctive, so a false match is implausible.
#[tokio::test(flavor = "current_thread")]
async fn slash_skill_activation_injects_skill_body_into_provider_system_prompt() {
    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let ws_for_factory = workspace.path().to_path_buf();
    let captured: Arc<StdRwLock<Vec<String>>> = Arc::new(StdRwLock::new(Vec::new()));
    let skill_dir = workspace.path().join(".agents/skills/zed-skill-a");
    std::fs::create_dir_all(&skill_dir).expect("create skill dir");
    let skill_file = skill_dir.join("SKILL.md");
    std::fs::write(
        &skill_file,
        "---\nname: zed-skill-a\ndescription: Zed skill A\n---\nBody for zed-skill-a\n",
    )
    .expect("write skill file");
    let mut skill = fixture_skill("zed-skill-a");
    skill.file = skill_file;
    skill.directory = skill_dir;
    skill.source = SkillSource::GlobalAgents;
    let skills = vec![skill];

    // Build an AcpCore that uses the capturing provider AND has the skill.
    let captured_for_factory = captured.clone();
    let factory: AcpCoreFactory = Rc::new(move |_cwd, _mcp_servers| {
        let mut acp_core = AcpCore::from({
            // CliCore with the capturing provider.
            use rustain::adapters::filesystem::FileSystemStorage;
            use rustain::adapters::noop::{
                NoOpApprovalPersistence, NoOpSecurity, NoOpToolSet, NoOpUsageLedger,
            };
            use rustain::domain::services::approval_runtime::ApprovalRuntime;
            use rustain::domain::services::tool_scheduler::ToolScheduler;

            let provider: Arc<dyn rustain::domain::ports::StreamingProvider> =
                Arc::new(SystemPromptCapturingProvider {
                    captured: captured_for_factory.clone(),
                });
            let security: Arc<dyn SecurityPort> = Arc::new(NoOpSecurity);
            let tools: Arc<dyn ToolSetPort> = Arc::new(NoOpToolSet);
            let approval = ApprovalRuntime::new(64, Arc::new(NoOpApprovalPersistence));
            let tool_scheduler =
                ToolScheduler::new(security.clone(), tools.clone(), approval.clone(), 64);
            let sessions_dir = ws_for_factory.join(".rustain").join("sessions");
            let storage: Arc<dyn StoragePort> = Arc::new(FileSystemStorage::with_workspace_root(
                sessions_dir,
                ws_for_factory.clone(),
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
        });
        let registry = SkillRegistry::from_skills(skills.clone());
        acp_core.skill_activator = Arc::new(SkillActivator::with_registry(Arc::new(
            tokio::sync::RwLock::new(registry),
        )));
        Ok(acp_core)
    });

    let (server_incoming, mut client_write) = tokio::io::duplex(8 * 1024);
    let (mut client_read, server_outgoing) = tokio::io::duplex(8 * 1024);
    let node_tree = NodeTree::with_now_fn(Arc::new(|| 1_700_000_000_000));

    let server = serve_acp_with_acp_core_factory_and_node_tree(
        server_outgoing.compat_write(),
        server_incoming.compat(),
        rustain::domain::models::AppConfig::default(),
        None,
        factory,
        node_tree,
        workspace.path().to_path_buf(),
        rustain::adapters::acp::run::deterministic_acp_id_source(),
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
        // Invoke the skill via the slash-command form Zed sends.
        let prompt = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "session/prompt",
            "params": {
                "sessionId": "acp-1",
                "prompt": [ { "type": "text", "text": "/skill zed-skill-a" } ],
            }
        });
        client_write
            .write_all(line(prompt.to_string()).as_bytes())
            .await
            .expect("write session/prompt");
        client_write.flush().await.expect("flush");

        // Read until the session/prompt result lands.
        let reader = tokio::io::BufReader::new(&mut client_read);
        let mut lines = reader.lines();
        loop {
            let next = tokio::time::timeout(Duration::from_secs(10), lines.next_line()).await;
            match next {
                Ok(Ok(Some(raw))) => {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
                        if v["id"] == serde_json::json!(3) {
                            return v;
                        }
                    }
                }
                _ => return serde_json::Value::Null,
            }
        }
    };

    let _ = tokio::select! {
        envelope = drive => envelope,
        server_res = server => {
            panic!("ACP server exited before session/prompt completed: {server_res:?}");
        }
    };

    // The provider must have received a system prompt carrying the skill body.
    let guard = captured.read().expect("captured lock");
    let saw_skill = guard.iter().any(|prompt| prompt.contains("zed-skill-a"));
    assert!(
        saw_skill,
        "activating `/skill zed-skill-a` must inject the skill into the provider system prompt; \
         captured system prompts = {guard:?}"
    );
    drop(guard);
}

// ─────────────────────────────────────────────────────────────────────
// Section 5 — no-spurious-activation tripwire (build path does not auto-activate)
// ─────────────────────────────────────────────────────────────────────

/// Build an `AcpCoreFactory` whose provider captures every system prompt it
/// receives and whose skill activator is pre-populated with `skills`.
///
/// Shared by the activation tripwires so the only varying input between the
/// `/skill` (inject) and plain-prompt (no-inject) cases is the prompt text —
/// making the contrast a clean behavioral A/B rather than a harness difference.
fn capturing_acp_factory(
    ws: PathBuf,
    captured: Arc<StdRwLock<Vec<String>>>,
    skills: Vec<SkillDef>,
) -> AcpCoreFactory {
    use rustain::adapters::filesystem::FileSystemStorage;
    use rustain::adapters::noop::{
        NoOpApprovalPersistence, NoOpSecurity, NoOpToolSet, NoOpUsageLedger,
    };
    use rustain::domain::services::approval_runtime::ApprovalRuntime;
    use rustain::domain::services::tool_scheduler::ToolScheduler;

    Rc::new(move |_cwd, _mcp_servers| {
        let provider: Arc<dyn rustain::domain::ports::StreamingProvider> =
            Arc::new(SystemPromptCapturingProvider {
                captured: captured.clone(),
            });
        let security: Arc<dyn SecurityPort> = Arc::new(NoOpSecurity);
        let tools: Arc<dyn ToolSetPort> = Arc::new(NoOpToolSet);
        let approval = ApprovalRuntime::new(64, Arc::new(NoOpApprovalPersistence));
        let tool_scheduler =
            ToolScheduler::new(security.clone(), tools.clone(), approval.clone(), 64);
        let sessions_dir = ws.join(".rustain").join("sessions");
        let storage: Arc<dyn StoragePort> = Arc::new(FileSystemStorage::with_workspace_root(
            sessions_dir,
            ws.clone(),
        ));
        let (event_tx, event_rx) = mpsc::unbounded_channel::<AppEvent>();
        let ledger: Arc<dyn UsageLedgerPort> = Arc::new(NoOpUsageLedger);
        let cli_core = CliCore {
            provider,
            security,
            tools,
            tool_scheduler,
            approval,
            storage,
            event_tx,
            event_rx,
            ledger,
        };
        let mut acp_core = AcpCore::from(cli_core);
        let registry = SkillRegistry::from_skills(skills.clone());
        acp_core.skill_activator = Arc::new(SkillActivator::with_registry(Arc::new(
            tokio::sync::RwLock::new(registry),
        )));
        Ok(acp_core)
    })
}

/// Write a Global-tier skill to disk and return its `SkillDef` pointing at the
/// real file (so the activator can load the body). Shared fixture shape with
/// the `/skill` activation test.
fn write_global_skill(workspace: &Path, name: &str) -> SkillDef {
    let skill_dir = workspace.join(".agents/skills").join(name);
    std::fs::create_dir_all(&skill_dir).expect("create skill dir");
    let skill_file = skill_dir.join("SKILL.md");
    std::fs::write(
        &skill_file,
        format!("---\nname: {name}\ndescription: {name} fixture\n---\nBody for {name}\n"),
    )
    .expect("write skill file");
    let mut skill = fixture_skill(name);
    skill.file = skill_file;
    skill.directory = skill_dir;
    skill.source = SkillSource::GlobalAgents;
    skill
}

/// A PLAIN prompt (no `/skill` invocation) with a populated skill registry must
/// NOT inject any skill body into the provider's system prompt.
///
/// This is the no-auto-activation tripwire: skills activate ONLY on an explicit
/// `/skill` command, never implicitly. It is the behavioral contrast to
/// `slash_skill_activation_injects_skill_body_into_provider_system_prompt` —
/// same harness, same discovered skill, only the prompt text differs. A
/// regression that pre-activates a discovered skill at session/turn setup
/// (e.g. composition eagerly calling `activate`) reddens the `!contains`
/// assertion, because the skill body would then appear on EVERY turn.
///
/// Non-vacuity: the provider records the real `CompletionOptions.system_prompt`
/// it received; the distinctive skill name `zed-skill-a` makes a false
/// "absent" pass implausible (it would require the body to genuinely not be
/// there). Paired with the `/skill` inject test, the two together prove the
/// activation is prompt-gated, not unconditional.
#[tokio::test(flavor = "current_thread")]
async fn plain_prompt_does_not_auto_inject_skill_into_system_prompt() {
    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let ws_for_factory = workspace.path().to_path_buf();
    let captured: Arc<StdRwLock<Vec<String>>> = Arc::new(StdRwLock::new(Vec::new()));
    let skill = write_global_skill(workspace.path(), "zed-skill-a");
    let factory = capturing_acp_factory(ws_for_factory, captured.clone(), vec![skill]);

    let (server_incoming, mut client_write) = tokio::io::duplex(8 * 1024);
    let (mut client_read, server_outgoing) = tokio::io::duplex(8 * 1024);
    let node_tree = NodeTree::with_now_fn(Arc::new(|| 1_700_000_000_000));

    let server = serve_acp_with_acp_core_factory_and_node_tree(
        server_outgoing.compat_write(),
        server_incoming.compat(),
        rustain::domain::models::AppConfig::default(),
        None,
        factory,
        node_tree,
        workspace.path().to_path_buf(),
        rustain::adapters::acp::run::deterministic_acp_id_source(),
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
        // A plain prompt with NO /skill invocation.
        let prompt = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "session/prompt",
            "params": {
                "sessionId": "acp-1",
                "prompt": [ { "type": "text", "text": "hello there, no skill invocation" } ],
            }
        });
        client_write
            .write_all(line(prompt.to_string()).as_bytes())
            .await
            .expect("write session/prompt");
        client_write.flush().await.expect("flush");

        let reader = tokio::io::BufReader::new(&mut client_read);
        let mut lines = reader.lines();
        loop {
            let next = tokio::time::timeout(Duration::from_secs(10), lines.next_line()).await;
            match next {
                Ok(Ok(Some(raw))) => {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
                        if v["id"] == serde_json::json!(3) {
                            return v;
                        }
                    }
                }
                _ => return serde_json::Value::Null,
            }
        }
    };

    let _ = tokio::select! {
        envelope = drive => envelope,
        server_res = server => {
            panic!("ACP server exited before session/prompt completed: {server_res:?}");
        }
    };

    let guard = captured.read().expect("captured lock");
    let leaked = guard.iter().any(|prompt| prompt.contains("zed-skill-a"));
    assert!(
        !leaked,
        "a plain prompt (no `/skill`) must NOT inject a skill into the provider system prompt — \
         activation must be explicit, but the skill body leaked on every turn; \
         captured system prompts = {guard:?}"
    );
    drop(guard);
}

// ─────────────────────────────────────────────────────────────────────
// Section 6 — CLI builder tool-exposure tripwire (no `activate_skill` tool)
// ─────────────────────────────────────────────────────────────────────

/// `build_cli_core` MUST NOT expose an `activate_skill` tool in its toolset.
///
/// Defends the boundary between the lean CLI/`ask` path and the ACP/TUI skill
/// machinery: skill activation is USER-driven (the `/skill` command + trust
/// flow in ACP, the palette in TUI), NEVER a model-callable builtin on the
/// shared CLI builder. Merging the ACP/TUI activation into `build_cli_core`
/// would let the model silently self-activate skills on every `ask`/CLI turn,
/// bypassing both the command gate and the trust prompt.
///
/// Non-vacuity: `ToolsetAdapter::available_tools()` currently lists
/// `activate_skill` UNCONDITIONALLY, so `build_cli_core(...).tools.available_tools()`
/// contains it today and this assertion REDDENS. It turns green once the CLI
/// builder's toolset drops the tool (or gates it behind the ACP/TUI builders).
/// The assertion checks by EXACT tool name, so a rename to `activateSkill` or
/// `activate-skill` would not sneak past — only the genuine absence passes.
///
/// Hermeticity: `build_cli_core` composes against a temp workspace with
/// `AppConfig::default()` (provider construction is lazy — no network/credentials
/// at build time; health checks run only on the first turn). If that ever
/// changes, fall back to the catalog/noop seam and assert there instead.
#[test]
fn build_cli_core_does_not_expose_activate_skill_tool() {
    use rustain::infrastructure::composition::build_cli_core;

    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let core = build_cli_core(
        &rustain::domain::models::AppConfig::default(),
        workspace.path(),
        false,
    )
    .expect("build_cli_core must compose hermetically with AppConfig::default() + temp workspace");

    let names: Vec<String> = core
        .tools
        .available_tools()
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert!(
        !names.iter().any(|n| n == "activate_skill"),
        "the CLI core toolset must NOT expose an `activate_skill` tool — skill activation is \
         user-driven (/skill in ACP/TUI), never a model-callable builtin on the shared CLI \
         builder; got tool names = {names:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────
// Section 7 — workspace skill trust allow/reject contrast
// ─────────────────────────────────────────────────────────────────────

/// Write a skill to disk with an explicit source tier (workspace skills
/// require trust; global skills do not).
fn write_skill(workspace: &Path, name: &str, source: SkillSource) -> SkillDef {
    let skill_dir = workspace.join(".agents/skills").join(name);
    std::fs::create_dir_all(&skill_dir).expect("create skill dir");
    let skill_file = skill_dir.join("SKILL.md");
    std::fs::write(
        &skill_file,
        format!("---\nname: {name}\ndescription: {name} fixture\n---\nBody for {name}\n"),
    )
    .expect("write skill file");
    let mut skill = fixture_skill(name);
    skill.file = skill_file;
    skill.directory = skill_dir;
    skill.source = source;
    skill
}

/// Drive `/skill <name>` for a WORKSPACE-tier skill and answer the agent's
/// `session/requestPermission` trust prompt with `trust_option`
/// (`skill_trust_allow` / `skill_trust_reject`). Returns the system prompts the
/// provider received, so the caller can contrast allow (skill present) vs reject
/// (skill absent).
///
/// Determinism: scripted capturing provider (no network), in-memory duplex
/// transport (no listener), deterministic first session id. The trust prompt is
/// answered inline while the prompt turn is in flight (no sleep — the turn only
/// resolves once the client replies, so the bounded read loop cannot race it).
async fn drive_workspace_skill_with_trust(
    workspace: &Path,
    skill_name: &str,
    trust_option: &str,
) -> Vec<String> {
    use rustain::adapters::acp::translate::{SKILL_TRUST_ALLOW, SKILL_TRUST_REJECT};
    // Pin the option strings to the live constants so a rename reddens at
    // compile time rather than silently desyncing the test from the agent.
    let _ = (SKILL_TRUST_ALLOW, SKILL_TRUST_REJECT);

    let captured: Arc<StdRwLock<Vec<String>>> = Arc::new(StdRwLock::new(Vec::new()));
    let skill = write_skill(workspace, skill_name, SkillSource::WorkspaceAgents);
    let factory = capturing_acp_factory(workspace.to_path_buf(), captured.clone(), vec![skill]);

    let (server_incoming, mut client_write) = tokio::io::duplex(8 * 1024);
    let (mut client_read, server_outgoing) = tokio::io::duplex(8 * 1024);
    let node_tree = NodeTree::with_now_fn(Arc::new(|| 1_700_000_000_000));

    let server = serve_acp_with_acp_core_factory_and_node_tree(
        server_outgoing.compat_write(),
        server_incoming.compat(),
        rustain::domain::models::AppConfig::default(),
        None,
        factory,
        node_tree,
        workspace.to_path_buf(),
        rustain::adapters::acp::run::deterministic_acp_id_source(),
    );

    let trust = trust_option.to_string();
    let drive = async {
        client_write
            .write_all(line(ACP_INITIALIZE_REQ.to_string()).as_bytes())
            .await
            .expect("write initialize");
        let session_new = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": { "cwd": workspace, "mcpServers": [] },
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
                "prompt": [ { "type": "text", "text": format!("/skill {skill_name}") } ],
            }
        });
        client_write
            .write_all(line(prompt.to_string()).as_bytes())
            .await
            .expect("write session/prompt");
        client_write.flush().await.expect("flush requests");

        let reader = tokio::io::BufReader::new(&mut client_read);
        let mut lines = reader.lines();
        loop {
            let next = tokio::time::timeout(Duration::from_secs(10), lines.next_line()).await;
            match next {
                Ok(Ok(Some(raw))) => {
                    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
                        continue;
                    };
                    // Server→client trust request: answer it with the chosen option.
                    if v["method"] == serde_json::json!("session/request_permission") {
                        if let Some(id) = v["id"].as_i64() {
                            let resp = serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": {
                                    "outcome": {
                                        "outcome": "selected",
                                        "optionId": trust,
                                    }
                                }
                            });
                            client_write
                                .write_all(line(resp.to_string()).as_bytes())
                                .await
                                .expect("write trust response");
                            client_write.flush().await.expect("flush trust response");
                        }
                        continue;
                    }
                    // session/prompt result (id 3): the turn is done.
                    if v["id"] == serde_json::json!(3) {
                        break;
                    }
                }
                _ => break,
            }
        }
    };

    tokio::select! {
        _ = drive => {},
        server_res = server => {
            panic!("ACP server exited before the trust turn completed: {server_res:?}");
        }
    };

    captured.read().expect("captured lock").clone()
}

/// Granting trust to a WORKSPACE-tier skill lets `/skill` activate it — the
/// skill body reaches the provider's system prompt.
///
/// Defends the allow half of the trust contract: a workspace skill is
/// untrusted by default, so the agent gates activation behind a
/// `session/requestPermission` trust prompt; when the client (user) allows it,
/// the skill is activated and injected like any other. A mutant that skips the
/// prompt and activates unconditionally reddens the REJECT test; a mutant that
/// never activates even when allowed reddens this one.
#[tokio::test(flavor = "current_thread")]
async fn workspace_skill_trust_allow_lets_activation_inject_skill() {
    use rustain::adapters::acp::translate::SKILL_TRUST_ALLOW;
    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let prompts =
        drive_workspace_skill_with_trust(workspace.path(), "ws-skill", SKILL_TRUST_ALLOW).await;
    assert!(
        prompts.iter().any(|p| p.contains("ws-skill")),
        "allowing trust on the workspace skill must let `/skill ws-skill` inject it into the \
         provider system prompt; captured = {prompts:?}"
    );
}

/// Rejecting trust for a WORKSPACE-tier skill BLOCKS `/skill` activation — the
/// skill body never reaches the provider's system prompt.
///
/// Defends the reject half of the trust contract: a declined trust prompt is a
/// user choice (not an error), and the agent MUST honor it by NOT activating
/// the skill. This is the contrast to the allow test — same skill, same prompt,
/// only the trust answer differs — so together they prove activation is
/// trust-gated for workspace skills. A mutant that activates workspace skills
/// without asking, or that ignores a rejection, reddens this assertion.
#[tokio::test(flavor = "current_thread")]
async fn workspace_skill_trust_reject_blocks_activation() {
    use rustain::adapters::acp::translate::SKILL_TRUST_REJECT;
    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let prompts =
        drive_workspace_skill_with_trust(workspace.path(), "ws-skill", SKILL_TRUST_REJECT).await;
    assert!(
        !prompts.iter().any(|p| p.contains("ws-skill")),
        "rejecting trust on the workspace skill must BLOCK activation — the skill body must NOT \
         reach the provider system prompt; captured = {prompts:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────
// Section 8 — 14.9 resume-trust-gate load-path behavioral keystone (AC6)
// ─────────────────────────────────────────────────────────────────────

/// Resume-trust-gate keystone (AC6) — a workspace-tier skill invoked on a
/// session LOADED via `session/load` MUST gate behind `session/requestPermission`
/// byte-identically to the SAME skill invoked on a fresh `session/new`.
/// Nothing read from the on-disk conversation store may relax the gate; skill
/// trust starts empty after a restart, so the loaded path fails closed exactly
/// like the fresh path.
///
/// Non-vacuity (the load-bypass mutant this kills): a variant that read a
/// "trusted" flag from disk and skipped the prompt would emit NO
/// `session/requestPermission` on the loaded path. This test captures the
/// permission frame on BOTH paths and asserts both fire with identical
/// option ids. The fresh-path frame is the positive control (the existing
/// `workspace_skill_trust_*` tests prove the gate fires fresh); the
/// loaded-path frame is the net-new keystone.
///
/// Determinism: scripted capturing provider (no network), in-memory duplex
/// transport (no listener), fixed clock, deterministic first session id.
#[tokio::test(flavor = "current_thread")]
async fn acp_load_path_re_runs_skill_trust_gate_identical_to_fresh_session() {
    use rustain::domain::models::AppConfig;

    // On an established session, drive `/skill ws-skill` and capture the
    // `session/requestPermission` params (answering REJECT so the turn
    // resolves without injecting the skill). Returns None if no trust prompt
    // fired — a None on the loaded path is exactly the mutant this kills.
    async fn drive_skill_capture_permission(
        client_write: &mut tokio::io::DuplexStream,
        lines: &mut tokio::io::Lines<tokio::io::BufReader<&mut tokio::io::DuplexStream>>,
        prompt_id: i64,
    ) -> Option<serde_json::Value> {
        use rustain::adapters::acp::translate::SKILL_TRUST_REJECT;
        let prompt = serde_json::json!({
            "jsonrpc": "2.0",
            "id": prompt_id,
            "method": "session/prompt",
            "params": {
                "sessionId": "acp-1",
                "prompt": [ { "type": "text", "text": "/skill ws-skill" } ],
            }
        });
        client_write
            .write_all(line(prompt.to_string()).as_bytes())
            .await
            .expect("write /skill prompt");
        client_write.flush().await.expect("flush /skill prompt");

        let mut permission_params: Option<serde_json::Value> = None;
        loop {
            let next = tokio::time::timeout(Duration::from_secs(10), lines.next_line()).await;
            let raw = match next {
                Ok(Ok(Some(l))) => l,
                _ => break,
            };
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
                continue;
            };
            if v["method"] == serde_json::json!("session/request_permission") {
                permission_params = Some(v["params"].clone());
                if let Some(id) = v["id"].as_i64() {
                    let resp = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "outcome": {
                                "type": "selected",
                                "optionId": SKILL_TRUST_REJECT,
                            }
                        }
                    });
                    client_write
                        .write_all(line(resp.to_string()).as_bytes())
                        .await
                        .expect("write reject");
                    client_write.flush().await.expect("flush reject");
                }
                continue;
            }
            if v["id"] == serde_json::json!(prompt_id) {
                break;
            }
        }
        permission_params
    }

    // Offered permission option ids from a requestPermission params frame.
    // Defends both the direct (`params.options`) and nested
    // (`params.request.options`) wire shapes.
    fn permission_option_ids(params: &serde_json::Value) -> Vec<String> {
        let opts = params["options"]
            .as_array()
            .or_else(|| params["request"]["options"].as_array());
        opts.map(|arr| {
            arr.iter()
                .filter_map(|o| o["optionId"].as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
    }

    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let ws = workspace.path().to_path_buf();
    let skill = write_skill(workspace.path(), "ws-skill", SkillSource::WorkspaceAgents);
    let captured: Arc<StdRwLock<Vec<String>>> = Arc::new(StdRwLock::new(Vec::new()));
    let factory = capturing_acp_factory(ws.clone(), captured.clone(), vec![skill]);

    // ── Phase A: fresh session/new + a seed prompt (persists acp-1 so Phase
    //    B can load it) + /skill → capture the FRESH trust frame (control). ──
    let fresh_frame = {
        let (server_incoming, mut client_write) = tokio::io::duplex(8 * 1024);
        let (mut client_read, server_outgoing) = tokio::io::duplex(8 * 1024);
        let node_tree = NodeTree::with_now_fn(Arc::new(|| 1_700_000_000_000));
        let server = serve_acp_with_acp_core_factory_and_node_tree(
            server_outgoing.compat_write(),
            server_incoming.compat(),
            AppConfig::default(),
            None,
            factory.clone(),
            node_tree,
            workspace.path().to_path_buf(),
            rustain::adapters::acp::run::deterministic_acp_id_source(),
        );

        let phase_a = async {
            client_write
                .write_all(line(ACP_INITIALIZE_REQ.to_string()).as_bytes())
                .await
                .expect("write initialize A");
            let new = serde_json::json!({
                "jsonrpc": "2.0", "id": 2, "method": "session/new",
                "params": { "cwd": workspace.path(), "mcpServers": [] },
            });
            client_write
                .write_all(line(new.to_string()).as_bytes())
                .await
                .expect("write new");
            client_write.flush().await.expect("flush new");
            // ONE persistent reader for the whole phase — recreating a
            // BufReader would discard buffered bytes mid-frame and stall.
            let reader = tokio::io::BufReader::new(&mut client_read);
            let mut lines = reader.lines();
            // Await session/new so acp-1 is live before the seed turn.
            loop {
                let next = tokio::time::timeout(Duration::from_secs(10), lines.next_line()).await;
                let raw = match next {
                    Ok(Ok(Some(l))) => l,
                    _ => panic!("session/new did not resolve (phase A)"),
                };
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
                    continue;
                };
                if v["id"] == serde_json::json!(2) && v["result"]["sessionId"].is_string() {
                    break;
                }
            }
            // Seed prompt persists the conversation so Phase B can load acp-1.
            let seed = serde_json::json!({
                "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
                "params": {
                    "sessionId": "acp-1",
                    "prompt": [ { "type": "text", "text": "seed turn" } ],
                }
            });
            client_write
                .write_all(line(seed.to_string()).as_bytes())
                .await
                .expect("write seed");
            client_write.flush().await.expect("flush seed");
            loop {
                let next = tokio::time::timeout(Duration::from_secs(10), lines.next_line()).await;
                let raw = match next {
                    Ok(Ok(Some(l))) => l,
                    _ => panic!("seed prompt did not resolve (phase A)"),
                };
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
                    continue;
                };
                if v["id"] == serde_json::json!(3)
                    && v["result"]["stopReason"] == serde_json::json!("end_turn")
                {
                    break;
                }
            }
            let frame = drive_skill_capture_permission(&mut client_write, &mut lines, 4).await;
            drop(client_write);
            frame
        };

        tokio::select! {
            f = phase_a => f,
            server_res = server => panic!("ACP server (phase A) exited early: {server_res:?}"),
        }
    };

    // ── Phase B: FRESH server, SAME workspace; session/load(acp-1) + /skill
    //    → capture the LOADED trust frame. The keystone: a load-bypass mutant
    //    that read "trusted" from disk and skipped the prompt yields None. ──
    let load_frame = {
        let (server_incoming, mut client_write) = tokio::io::duplex(8 * 1024);
        let (mut client_read, server_outgoing) = tokio::io::duplex(8 * 1024);
        let node_tree = NodeTree::with_now_fn(Arc::new(|| 1_700_000_000_000));
        let server = serve_acp_with_acp_core_factory_and_node_tree(
            server_outgoing.compat_write(),
            server_incoming.compat(),
            AppConfig::default(),
            None,
            factory.clone(),
            node_tree,
            workspace.path().to_path_buf(),
            rustain::adapters::acp::run::deterministic_acp_id_source(),
        );

        let phase_b = async {
            client_write
                .write_all(line(ACP_INITIALIZE_REQ.to_string()).as_bytes())
                .await
                .expect("write initialize B");
            let load = serde_json::json!({
                "jsonrpc": "2.0", "id": 2, "method": "session/load",
                "params": {
                    "cwd": workspace.path(),
                    "sessionId": "acp-1",
                    "mcpServers": [],
                }
            });
            client_write
                .write_all(line(load.to_string()).as_bytes())
                .await
                .expect("write load");
            client_write.flush().await.expect("flush load");
            let reader = tokio::io::BufReader::new(&mut client_read);
            let mut lines = reader.lines();
            loop {
                let next = tokio::time::timeout(Duration::from_secs(10), lines.next_line()).await;
                let raw = match next {
                    Ok(Ok(Some(l))) => l,
                    _ => panic!("session/load did not resolve (phase B)"),
                };
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
                    continue;
                };
                if v["id"] == serde_json::json!(2) {
                    break;
                }
            }
            let frame = drive_skill_capture_permission(&mut client_write, &mut lines, 3).await;
            drop(client_write);
            frame
        };

        tokio::select! {
            f = phase_b => f,
            server_res = server => panic!("ACP server (phase B) exited early: {server_res:?}"),
        }
    };

    let fresh = fresh_frame.expect(
        "POSITIVE CONTROL: fresh session/new + /skill MUST fire \
         session/requestPermission for an untrusted workspace skill -- a None \
         here means the trust gate is broken on the fresh path, not just load",
    );
    let loaded = load_frame.expect(
        "KEYSTONE: the LOADED session + /skill MUST ALSO fire \
         session/requestPermission -- a load-bypass mutant that read a \
         'trusted' flag from disk and skipped the prompt yields None here",
    );
    let fresh_opts = permission_option_ids(&fresh);
    let load_opts = permission_option_ids(&loaded);
    assert_eq!(
        fresh_opts, load_opts,
        "the loaded-path and fresh-path trust prompts must offer identical \
         permission options (byte-identical trust gate); fresh={fresh:?} load={loaded:?}"
    );
    assert!(
        fresh_opts
            .iter()
            .any(|o| o == rustain::adapters::acp::translate::SKILL_TRUST_ALLOW),
        "the trust prompt must offer the allow option; got {fresh_opts:?}"
    );
}
