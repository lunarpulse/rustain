//! Provider-neutral restart history (AC3: Anthropic through the daemon, the
//! channel reply and the legacy files), the image carrier's other adapters
//! (AC1: Anthropic and Codex bytes unchanged) and the Codex two-round leg
//! closing DF-20-1D1B-MERGED-ROUND-REPLAY-ORDER on every reload rail.
use super::harness::{
    Daemon, ECHO_TOOL, IMAGE, IMAGE_TOOL, Tools, ask, assert_prefix, attach, bodies, cli_core,
    daemon_turn, mount, raw_array, tui_turn, until_end_turn,
};
use crate::conformance_20_1d1b_codex_replay_discovery::common::{
    self as codex_common, GUARD, RESPONSE_PATH, completed, enc, login, opts, reasoning, storage,
    text, tool_call,
};
use rustain::adapters::acp::agent::CoreFactory;
use rustain::adapters::acp::run::serve_acp_with_core_factory;
use rustain::adapters::anthropic::{AnthropicAdapter, AuthMode};
use rustain::adapters::cli::ask::OutputFormat;
use rustain::adapters::daemon::protocol::ConnectionTier;
use rustain::domain::models::conversation::PersistedConversation;
use rustain::domain::models::{AppConfig, Conversation, MessageRole};
use rustain::domain::ports::{StoragePort, StreamingProvider};
use rustain::domain::services::message_builder::build_api_messages;
use serde_json::{Value, json};
use serial_test::serial;
use std::path::Path;
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use wiremock::{MockServer, ResponseTemplate};

// ── ACP front door ──────────────────────────────────────────────────────────

fn line(value: Value) -> String {
    format!("{value}\n")
}

/// A real provider whose first text chunk is followed by a client-controlled
/// pause. The ACP client releases it only after observing the in-flight notice.
struct NoticeGate {
    inner: Arc<dyn StreamingProvider>,
    armed: std::sync::atomic::AtomicBool,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl StreamingProvider for NoticeGate {
    async fn stream_completion(
        &self,
        messages: Vec<rustain::domain::models::Message>,
        options: rustain::domain::models::CompletionOptions,
    ) -> Result<
        futures::stream::BoxStream<'static, rustain::domain::models::StreamChunk>,
        rustain::domain::errors::ProviderError,
    > {
        use futures::StreamExt;
        let stream = self.inner.stream_completion(messages, options).await?;
        let arm = self.armed.swap(false, std::sync::atomic::Ordering::SeqCst);
        let release = self.release.clone();
        Ok(Box::pin(futures::stream::unfold(
            (stream, arm, false, release),
            |(mut stream, armed, pending, release)| async move {
                if pending {
                    release.notified().await;
                }
                let chunk = stream.next().await?;
                let gate = armed
                    && matches!(&chunk, rustain::domain::models::StreamChunk::Text { content, .. } if !content.is_empty());
                Some((chunk, (stream, armed && !gate, gate, release)))
            },
        )))
    }

    async fn abort(&self) -> Result<(), rustain::domain::errors::ProviderError> {
        self.inner.abort().await
    }
    fn provider_id(&self) -> String {
        self.inner.provider_id()
    }
    fn list_models(&self) -> Vec<rustain::domain::models::provider::ModelDescriptor> {
        self.inner.list_models()
    }
    async fn health_check(&self) -> Result<(), rustain::domain::errors::ProviderError> {
        self.inner.health_check().await
    }
    async fn connectivity_probe(
        &self,
    ) -> Result<rustain::domain::ports::ProbeOutcome, rustain::domain::errors::ProviderError> {
        self.inner.connectivity_probe().await
    }
}

pub(crate) const QUEUED_NOTICE: &str = "signed-wire queued diagnostic";
pub(crate) const FLIGHT_NOTICE: &str = "signed-wire in-flight diagnostic";

/// Drive one server, optionally loading an existing session. A gated initial
/// turn sends a real MCP event only after the client has received provider text.
pub(crate) async fn acp_notice_turns(
    provider: Arc<dyn StreamingProvider>,
    cfg: &AppConfig,
    ws: &Path,
    session: Option<&str>,
    prompts: &[&str],
    notices: bool,
) -> (String, Vec<Value>) {
    let (server_incoming, mut client_write) = tokio::io::duplex(64 * 1024);
    let (client_read, server_outgoing) = tokio::io::duplex(64 * 1024);
    let release = Arc::new(tokio::sync::Notify::new());
    let provider: Arc<dyn StreamingProvider> = if notices {
        Arc::new(NoticeGate {
            inner: provider,
            armed: true.into(),
            release: release.clone(),
        })
    } else {
        provider
    };
    let sender = Rc::new(std::cell::RefCell::new(None));
    let sender_factory = sender.clone();
    let workspace = ws.to_path_buf();
    let factory: CoreFactory = Rc::new(move |_cwd: &Path, _mcp_servers| {
        let core = cli_core(provider.clone(), Tools::port(true), &workspace);
        if notices {
            core.event_tx
                .send(rustain::domain::events::AppEvent::SystemNotice {
                    conversation_id: None,
                    level: rustain::domain::models::NoticeLevel::Warning,
                    message: QUEUED_NOTICE.into(),
                })
                .unwrap();
        }
        *sender_factory.borrow_mut() = Some(core.event_tx.clone());
        Ok(core)
    });
    let server = serve_acp_with_core_factory(
        server_outgoing.compat_write(),
        server_incoming.compat(),
        cfg.clone(),
        ws.to_path_buf(),
        Some(cfg.model.clone()),
        factory,
    );
    let cwd = ws.to_path_buf();
    let existing = session.map(str::to_owned);
    let drive = async move {
        let mut lines = tokio::io::BufReader::new(client_read).lines();
        let mut frames = Vec::new();
        let mut sent_flight = false;
        let mut warning_seen = false;
        let mut session_id = existing.clone().unwrap_or_default();
        let mut prompt_index = 0usize;
        client_write.write_all(line(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}})).as_bytes()).await.unwrap();
        let open = if let Some(id) = existing {
            json!({"jsonrpc":"2.0","id":2,"method":"session/load","params":{"cwd":cwd,"sessionId":id,"mcpServers":[]}})
        } else {
            json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":cwd,"mcpServers":[]}})
        };
        client_write.write_all(line(open).as_bytes()).await.unwrap();
        client_write.flush().await.unwrap();
        loop {
            let raw = tokio::time::timeout(GUARD, lines.next_line())
                .await
                .expect("ACP frame hang guard")
                .unwrap()
                .expect("ACP stream ended");
            let value: Value = serde_json::from_str(&raw).unwrap();
            assert_ne!(value["method"], "session/request_permission");
            let text = value["params"]["update"]["content"]["text"]
                .as_str()
                .unwrap_or("");
            if notices && !sent_flight && text == "V" {
                sender
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .send(rustain::domain::events::AppEvent::SystemNotice {
                        conversation_id: None,
                        level: rustain::domain::models::NoticeLevel::Warning,
                        message: FLIGHT_NOTICE.into(),
                    })
                    .unwrap();
                sent_flight = true;
            }
            if notices && text == format!("Warning: {FLIGHT_NOTICE}") {
                warning_seen = true;
                release.notify_one();
            }
            let id = value["id"].as_u64();
            if id == Some(2) {
                assert!(value.get("error").is_none(), "{value}");
                if session_id.is_empty() {
                    session_id = value["result"]["sessionId"].as_str().unwrap().to_owned();
                }
            }
            let completed = id == Some(2) || id == Some(3 + prompt_index as u64);
            if id != Some(2) && completed {
                assert_eq!(value["result"]["stopReason"], "end_turn", "{value}");
                prompt_index += 1;
            }
            frames.push(value);
            if completed {
                if prompt_index == prompts.len() {
                    if notices {
                        assert!(sent_flight && warning_seen);
                    }
                    return (session_id, frames);
                }
                let req = json!({"jsonrpc":"2.0","id":3 + prompt_index as u64,"method":"session/prompt","params":{"sessionId":session_id,"prompt":[{"type":"text","text":prompts[prompt_index]}]}});
                client_write.write_all(line(req).as_bytes()).await.unwrap();
                client_write.flush().await.unwrap();
            }
        }
    };
    tokio::select! {
        output = drive => output,
        result = server => panic!("ACP server exited before prompt result: {result:?}"),
    }
}

// ── Anthropic ───────────────────────────────────────────────────────────────

const ANTHROPIC_MODEL: &str = "claude-sonnet-4-6";
const MESSAGES: &str = "/v1/messages";

fn anthropic(server: &MockServer) -> Arc<dyn StreamingProvider> {
    Arc::new(
        AnthropicAdapter::new(
            AuthMode::ApiKey("fixture-key".into()),
            ANTHROPIC_MODEL.into(),
            Some(server.uri()),
        )
        .unwrap(),
    )
}

fn anthropic_cfg() -> AppConfig {
    AppConfig {
        model: ANTHROPIC_MODEL.into(),
        ..Default::default()
    }
}

enum Block<'a> {
    Text(&'a str),
    Tool(&'a str, &'a str, Value),
}

/// One Messages API SSE stream of `blocks`, ending with `stop`.
fn anthropic_sse(blocks: &[Block<'_>], stop: &str) -> ResponseTemplate {
    let mut events = vec![(
        "message_start",
        json!({"type": "message_start", "message": {"usage": {"input_tokens": 10}}}),
    )];
    for (index, block) in blocks.iter().enumerate() {
        let (start, delta) = match block {
            Block::Text(text) => (
                json!({"type": "text", "text": ""}),
                json!({"type": "text_delta", "text": text}),
            ),
            Block::Tool(id, name, input) => (
                json!({"type": "tool_use", "id": id, "name": name, "input": {}}),
                json!({"type": "input_json_delta", "partial_json": input.to_string()}),
            ),
        };
        events.push((
            "content_block_start",
            json!({"type": "content_block_start", "index": index, "content_block": start}),
        ));
        events.push((
            "content_block_delta",
            json!({"type": "content_block_delta", "index": index, "delta": delta}),
        ));
        events.push((
            "content_block_stop",
            json!({"type": "content_block_stop", "index": index}),
        ));
    }
    events.push((
        "message_delta",
        json!({"type": "message_delta", "delta": {"stop_reason": stop}, "usage": {"output_tokens": 3}}),
    ));
    events.push(("message_stop", json!({"type": "message_stop"})));
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(
            events
                .into_iter()
                .map(|(name, data)| format!("event: {name}\ndata: {data}\n\n"))
                .collect::<String>(),
        )
}

/// The three-request Anthropic tool loop: V + calls A (image) and B, then C,
/// then the final text; plain answers follow.
fn anthropic_script(answers: &[&str]) -> Vec<ResponseTemplate> {
    let mut script = vec![
        anthropic_sse(
            &[
                Block::Text("V"),
                Block::Tool("toolu_A", IMAGE_TOOL, json!({"label": "a"})),
                Block::Tool("toolu_B", ECHO_TOOL, json!({"text": "b"})),
            ],
            "tool_use",
        ),
        anthropic_sse(
            &[Block::Tool("toolu_C", ECHO_TOOL, json!({"text": "c"}))],
            "tool_use",
        ),
        anthropic_sse(&[Block::Text("done")], "end_turn"),
    ];
    script.extend(
        answers
            .iter()
            .map(|text| anthropic_sse(&[Block::Text(text)], "end_turn")),
    );
    script
}

/// The channel runs the tool loop; the next operator turn follows, with or
/// without a daemon restart on the same store.
async fn anthropic_daemon(restart: bool) -> (Vec<String>, String) {
    let server = MockServer::start().await;
    let world = codex_common::World::new(&server);
    mount(&server, MESSAGES, anthropic_script(&["after restart"])).await;
    let ws = world.workspace();
    let cfg = anthropic_cfg();
    let mut daemon = Daemon::start(
        anthropic(&server),
        &cfg,
        &ws,
        Conversation {
            id: "ac3-daemon".into(),
            ..Default::default()
        },
        "first.sock",
    );
    // The reply leaves at the first response's commit; an observer sees the
    // whole channel turn end.
    let (mut observer, _) = attach(&daemon.socket, ConnectionTier::TrustedLocal, true).await;
    let reply = daemon.channel_turn("first").await;
    until_end_turn(&mut observer).await;
    drop(observer);
    assert_eq!(bodies(&server, MESSAGES).await.len(), 3);
    if restart {
        daemon = daemon
            .restart(anthropic(&server), &cfg, &ws, "ac3-daemon", "second.sock")
            .await;
    }
    let (mut client, _) = attach(&daemon.socket, ConnectionTier::TrustedLocal, false).await;
    daemon_turn(&mut client, "next").await;
    drop(client);
    daemon.stop().await;
    let posts = bodies(&server, MESSAGES).await;
    assert_eq!(posts.len(), 4);
    (posts, reply)
}

/// AC3 / K3 (Anthropic): structured `tool_use`/`tool_result` with the
/// provider's ids after a restart, equal to the no-restart control; the channel
/// reply is today's first-response prose fold.
#[tokio::test]
#[serial]
async fn k3_anthropic_daemon_restart_reproduces_the_in_turn_history() {
    let (posts, reply) = anthropic_daemon(true).await;
    assert_eq!(
        reply,
        "V\n[tool use: fixture_image (id: toolu_A)]\n\n[tool use: fixture_echo (id: toolu_B)]\n"
    );
    let in_turn: Vec<Value> = serde_json::from_str(raw_array(&posts[2], "messages")).unwrap();
    let ids: Vec<_> = in_turn
        .iter()
        .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
        .filter_map(|block| match block["type"].as_str() {
            Some("tool_use") => Some(format!("use:{}", block["id"].as_str().unwrap())),
            Some("tool_result") => {
                Some(format!("result:{}", block["tool_use_id"].as_str().unwrap()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        ids,
        [
            "use:toolu_A",
            "use:toolu_B",
            "result:toolu_A",
            "result:toolu_B",
            "use:toolu_C",
            "result:toolu_C"
        ]
    );
    let rest = assert_prefix(&posts[2], &posts[3], "messages");
    assert_eq!(rest.len(), 2, "{rest:?}");
    assert_eq!(rest[0]["role"], "assistant");
    assert_eq!(rest[0]["content"][0]["text"], "done");
    assert_eq!(rest[1]["role"], "user");
    assert!(
        !posts[3].contains("[tool use:"),
        "never prose: {}",
        posts[3]
    );
    let (control, _) = anthropic_daemon(false).await;
    assert_eq!(
        raw_array(&posts[3], "messages"),
        raw_array(&control[3], "messages"),
        "the no-restart control sends the same history"
    );
}

/// A pre-story daemon transcript: one prose row per response.
const LEGACY_DAEMON: &str = r#"{"id":"legacy-daemon","title":"t","createdAt":1,"messages":[
 {"id":"u1","role":"user","content":"first","contentBlocks":[],"toolCalls":[],"createdAt":1},
 {"id":"a1","role":"assistant","content":"V\n[tool use: fixture_echo (id: toolu_B)]\n","contentBlocks":[],"toolCalls":[],"createdAt":2},
 {"id":"a2","role":"assistant","content":"[tool result: echo b]\ndone","contentBlocks":[],"toolCalls":[],"createdAt":3}]}"#;

/// A pre-story `ask` session: one merged row with the provider's call ids.
const LEGACY_ASK: &str = r#"{"id":"legacy-ask","title":"t","createdAt":1,"messages":[
 {"id":"u1","role":"user","content":"first","contentBlocks":[],"toolCalls":[],"createdAt":1},
 {"id":"a1","role":"assistant","content":"Vdone","contentBlocks":[],"createdAt":2,"toolCalls":[
  {"id":"toolu_A","name":"fixture_echo","input":{"text":"a"},"result":{"content":"echo a","isError":false}},
  {"id":"toolu_C","name":"fixture_echo","input":{"text":"c"},"result":{"content":"echo c","isError":false}}]}]}"#;

fn shape(conversation: &Conversation) -> Vec<(MessageRole, String, Vec<String>, Vec<String>)> {
    build_api_messages(conversation)
        .into_iter()
        .map(|m| {
            (
                m.role,
                m.content,
                m.tool_uses.into_iter().map(|u| u.id).collect(),
                m.tool_results.into_iter().map(|r| r.tool_use_id).collect(),
            )
        })
        .collect()
}

/// AC3 legacy: a prose daemon transcript and a merged `ask` session load and
/// rebuild as before the story (prose rows; one merged message with the
/// load-time `tc_…` ids), before and after a mirror rebuild.
#[test]
fn plain_legacy_prose_and_merged_sessions_rebuild_as_before() {
    let mut daemon = serde_json::from_str::<PersistedConversation>(LEGACY_DAEMON)
        .unwrap()
        .to_conversation();
    let expected = vec![
        (MessageRole::User, "first".to_owned(), vec![], vec![]),
        (
            MessageRole::Assistant,
            "V\n[tool use: fixture_echo (id: toolu_B)]\n".to_owned(),
            vec![],
            vec![],
        ),
        (
            MessageRole::Assistant,
            "[tool result: echo b]\ndone".to_owned(),
            vec![],
            vec![],
        ),
    ];
    assert_eq!(shape(&daemon), expected);
    daemon.rebuild_messages_mirror();
    assert_eq!(shape(&daemon), expected);

    let mut merged = serde_json::from_str::<PersistedConversation>(LEGACY_ASK)
        .unwrap()
        .to_conversation();
    let ids = vec!["tc_a1_1".to_owned(), "tc_a1_3".to_owned()];
    let expected = vec![
        (MessageRole::User, "first".to_owned(), vec![], vec![]),
        (
            MessageRole::Assistant,
            "Vdone".to_owned(),
            ids.clone(),
            vec![],
        ),
        (MessageRole::User, String::new(), vec![], ids),
    ];
    assert_eq!(
        shape(&merged),
        expected,
        "one merged message, load-time ids"
    );
    merged.rebuild_messages_mirror();
    assert_eq!(shape(&merged), expected);
}

// ── AC1: other adapters ignore result images ────────────────────────────────

/// AC1: with the image result, the Anthropic and Codex requests are byte-equal
/// to the pre-story projection's (placeholder text only).
#[tokio::test]
#[serial]
async fn k4_anthropic_and_codex_requests_ignore_result_images() {
    let mut histories = Vec::new();
    for images in [true, false] {
        let server = MockServer::start().await;
        let world = codex_common::World::new(&server);
        mount(&server, MESSAGES, anthropic_script(&[])).await;
        let ws = world.workspace();
        let (code, _, stderr) = ask(
            anthropic(&server),
            Tools::port(images),
            &anthropic_cfg(),
            &ws,
            opts("first", OutputFormat::Text, None),
        )
        .await;
        assert_eq!(code, ExitCode::SUCCESS, "{stderr}");
        let posts = bodies(&server, MESSAGES).await;
        assert!(!posts[1].contains(IMAGE));
        histories.push(raw_array(&posts[1], "messages").to_owned());
    }
    assert_eq!(histories[0], histories[1], "Anthropic bytes unchanged");

    let mut histories = Vec::new();
    for images in [true, false] {
        let server = MockServer::start().await;
        let world = codex_common::World::new(&server);
        login(&server, "k4-account").await;
        mount(
            &server,
            RESPONSE_PATH,
            vec![
                codex_sse(&[
                    function_call(IMAGE_TOOL, "call-a", 1, json!({"label": "a"})),
                    function_call(ECHO_TOOL, "call-b", 2, json!({"text": "b"})),
                    completed(),
                ]),
                codex_sse(&[text("done"), completed()]),
            ],
        )
        .await;
        let ws = world.workspace();
        let (code, _, stderr) = ask(
            codex_common::codex(),
            Tools::port(images),
            &codex_common::app_config(),
            &ws,
            opts("first", OutputFormat::Text, None),
        )
        .await;
        assert_eq!(code, ExitCode::SUCCESS, "{stderr}");
        let posts = bodies(&server, RESPONSE_PATH).await;
        assert_eq!(posts.len(), 2);
        assert!(!posts[1].contains(IMAGE));
        histories.push(raw_array(&posts[1], "input").to_owned());
    }
    assert_eq!(histories[0], histories[1], "Codex bytes unchanged");
}

// ── Codex two rounds: DF-20-1D1B-MERGED-ROUND-REPLAY-ORDER ──────────────────

fn codex_sse(events: &[Value]) -> ResponseTemplate {
    codex_common::sse(events)
}

fn function_call(name: &str, call: &str, index: usize, args: Value) -> Value {
    json!({"type": "response.output_item.done", "output_index": index, "item": {
        "type": "function_call", "id": format!("fc_{call}"), "call_id": call,
        "name": name, "arguments": args.to_string()}})
}

/// Two reasoning tool rounds, then reasoning and the final text.
fn codex_script(answer: &str) -> Vec<ResponseTemplate> {
    vec![
        codex_sse(&[reasoning("r1"), tool_call("c1"), completed()]),
        codex_sse(&[reasoning("r2"), tool_call("c2"), completed()]),
        codex_sse(&[reasoning("r3"), text("done"), completed()]),
        codex_sse(&[text(answer), completed()]),
    ]
}

fn index(items: &[Value], test: impl Fn(&Value) -> bool) -> usize {
    items
        .iter()
        .position(test)
        .unwrap_or_else(|| panic!("missing item: {items:?}"))
}

/// Each round's item sits right before its own call in the in-turn POST 3; the
/// reloaded POST 4 starts with it byte for byte, then round 3's item ahead of
/// its own text, then the prompt.
fn assert_codex_reload(posts: &[String]) {
    assert_eq!(posts.len(), 4);
    let in_turn: Vec<Value> = serde_json::from_str(raw_array(&posts[2], "input")).unwrap();
    let r1 = index(&in_turn, |i| i["encrypted_content"] == enc("r1"));
    let c1 = index(&in_turn, |i| {
        i["type"] == "function_call" && i["call_id"] == "c1"
    });
    let r2 = index(&in_turn, |i| i["encrypted_content"] == enc("r2"));
    let c2 = index(&in_turn, |i| {
        i["type"] == "function_call" && i["call_id"] == "c2"
    });
    assert!(r1 < c1 && c1 < r2 && r2 < c2, "{in_turn:?}");
    let rest = assert_prefix(&posts[2], &posts[3], "input");
    assert_eq!(rest[0]["encrypted_content"], enc("r3"), "{rest:?}");
    assert!(rest[1].to_string().contains("done"), "{rest:?}");
    assert!(
        rest.last().unwrap().to_string().contains("next"),
        "{rest:?}"
    );
}

/// The DF's three reload rails: `ask` resume, TUI reload, daemon restart. M03
/// (ask) and M04 (mirror) merge the rounds and turn this RED.
#[tokio::test]
#[serial]
async fn codex_two_rounds_replay_each_round_before_its_own_call_after_every_reload() {
    // `rustain --session <id> ask`.
    let server = MockServer::start().await;
    let world = codex_common::World::new(&server);
    login(&server, "codex-ask").await;
    mount(&server, RESPONSE_PATH, codex_script("after resume")).await;
    let ws = world.workspace();
    let cfg = codex_common::app_config();
    let (code, stdout, stderr) = ask(
        codex_common::codex(),
        Tools::port(true),
        &cfg,
        &ws,
        opts("first", OutputFormat::Json, None),
    )
    .await;
    assert_eq!(code, ExitCode::SUCCESS, "{stderr}");
    let session = serde_json::from_str::<Value>(&stdout).unwrap()["session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (code, _, stderr) = ask(
        codex_common::codex(),
        Tools::port(true),
        &cfg,
        &ws,
        opts("next", OutputFormat::Text, Some(session)),
    )
    .await;
    assert_eq!(code, ExitCode::SUCCESS, "{stderr}");
    assert_codex_reload(&bodies(&server, RESPONSE_PATH).await);
    drop(world);

    // TUI reload.
    let server = MockServer::start().await;
    let world = codex_common::World::new(&server);
    login(&server, "codex-tui").await;
    mount(&server, RESPONSE_PATH, codex_script("after reload")).await;
    let ws = world.workspace();
    let store = storage(&ws);
    let mut conversation = Conversation {
        id: "codex-tui".into(),
        ..Default::default()
    };
    tui_turn(
        codex_common::codex(),
        Tools::port(true),
        &cfg,
        &ws,
        &mut conversation,
        "first",
    )
    .await;
    store.save_conversation(&conversation).await.unwrap();
    let mut conversation = store.load_conversation("codex-tui").await.unwrap().unwrap();
    tui_turn(
        codex_common::codex(),
        Tools::port(true),
        &cfg,
        &ws,
        &mut conversation,
        "next",
    )
    .await;
    assert_codex_reload(&bodies(&server, RESPONSE_PATH).await);
    drop(world);

    // Daemon restart.
    let server = MockServer::start().await;
    let world = codex_common::World::new(&server);
    login(&server, "codex-daemon").await;
    mount(&server, RESPONSE_PATH, codex_script("after restart")).await;
    let ws = world.workspace();
    let daemon = Daemon::start(
        codex_common::codex(),
        &cfg,
        &ws,
        Conversation {
            id: "codex-daemon".into(),
            ..Default::default()
        },
        "first.sock",
    );
    let (mut client, _) = attach(&daemon.socket, ConnectionTier::TrustedLocal, false).await;
    daemon_turn(&mut client, "first").await;
    drop(client);
    let daemon = daemon
        .restart(
            codex_common::codex(),
            &cfg,
            &ws,
            "codex-daemon",
            "second.sock",
        )
        .await;
    let (mut client, _) = attach(&daemon.socket, ConnectionTier::TrustedLocal, false).await;
    daemon_turn(&mut client, "next").await;
    drop(client);
    daemon.stop().await;
    assert_codex_reload(&bodies(&server, RESPONSE_PATH).await);
}
