//! K09 (signature replay through the real turn loop: the two-turn literal, the
//! invalid-signature sentinel, the edited turn, another model and another
//! provider) and K10 (in-turn replay on the TUI, `ask`, daemon and ACP rails,
//! text-only replay after a local reload and an `ask --resume`, the tool-round
//! replay after an `ask` resume and a daemon restart (span-correct since Story
//! 20.1d4a), and privacy of every observer). Every Cloud Code Assist row is
//! written by `flows::login` against the loopback stub.
use super::auth::{
    ACCESS, EMAIL, Env, MODEL, PROJECT, PROVIDER, STREAM, build, config, env, hits, login, options,
    user,
};
use crate::conformance_20_1d1b_codex_replay_discovery::common::{
    GUARD, opts, sessions_contain, storage,
};
use arc_swap::ArcSwap;
use futures::StreamExt;
use rustain::adapters::acp::agent::CoreFactory;
use rustain::adapters::acp::run::serve_acp_with_core_factory;
use rustain::adapters::cli::ask::{AskOpts, OutputFormat};
use rustain::adapters::daemon::protocol::{
    ClientFrame, ConnectionTier, DaemonFrame, PROTOCOL_VERSION, read_frame, write_frame,
};
use rustain::adapters::daemon::runtime::{DaemonCore, DaemonTurnRuntime};
use rustain::adapters::daemon::server::AttachServer;
use rustain::adapters::filesystem::FileSystemStorage;
use rustain::adapters::noop::{
    NoOpApprovalPersistence, NoOpContext, NoOpMemory, NoOpPersona, NoOpSecurity, NoOpUsageLedger,
};
use rustain::adapters::rap::IdentityKeyStore;
use rustain::adapters::security_adapter::SecurityAdapter;
use rustain::adapters::skill_activation::SkillActivator;
use rustain::adapters::tui::state::{TabRenderState, TuiState};
use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::chat_pane;
use rustain::domain::clock::SystemClock;
use rustain::domain::errors::ToolError;
use rustain::domain::events::AppEvent;
use rustain::domain::models::{
    AppConfig, ChannelKind, ChannelTurnRequest, ChatMessage, ContentBlockType, Conversation,
    MessageRole, PermissionMode, ProviderConfig, SessionManager, SessionState, StopReason,
    StreamChunk, StreamingState, ToolDefinition, ToolResult, ViewState,
};
use rustain::domain::ports::{
    ContextAssemblerPort, ContextPort, SecurityPort, StoragePort, StreamingProvider, ToolSetPort,
    UsageLedgerPort,
};
use rustain::domain::services::approval_runtime::ApprovalRuntime;
use rustain::domain::services::message_builder::build_api_messages;
use rustain::domain::services::plan_mode_injector::DefaultPlanInjector;
use rustain::domain::services::tool_scheduler::ToolScheduler;
use rustain::infrastructure::composition::CliCore;
use rustain::infrastructure::runtime::event_bus::{EventBus, RawEvent, RawEventKind};
use rustain::infrastructure::runtime::turn_driver::{
    LocalTurnDriver, TurnViewState, UserSubmission,
};
use rustain::infrastructure::telemetry::ActiveRatioWindow;
use serde_json::{Value, json};
use serial_test::serial;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tokio_util::sync::CancellationToken;
use wiremock::{
    Mock, MockServer, Request, Respond, ResponseTemplate,
    matchers::{method, path},
};

/// Every fixture signature starts with this Base64 prefix (`SIG`).
const MARK: &str = "U0lH";
const SIG0: &str = "U0lHMA==";
const SIG1: &str = "U0lHMQ==";
const SIG2: &str = "U0lHMg==";
const SENTINEL: &str = "skip_thought_signature_validator";
const OTHER_MODEL: &str = "gemini-2.5-pro";
/// The signed thinking + text turn as `request.rs` serializes it (key order included).
const SIGNED_TEXT_TURN: &str = r#"{"role":"model","parts":[{"thought":true,"text":"T","thoughtSignature":"U0lHMA=="},{"text":"V","thoughtSignature":"U0lHMQ=="}]}"#;

// ── loopback world ──────────────────────────────────────────────────────────

pub(crate) struct World {
    server: MockServer,
    dir: tempfile::TempDir,
    _env: (Env, Env),
}

impl World {
    /// Isolated env plus one real browser login of the fixture account.
    pub(crate) async fn start() -> Self {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().to_str().unwrap().to_owned();
        let base = env(dir.path(), &server.uri());
        let extra = Env::set(&[
            ("RUSTAIN_CONFIG_DIR", Some(data.as_str())),
            ("RUSTAIN_MODELS_DEV_URL", Some("http://127.0.0.1:1")),
            ("OPENAI_API_KEY", None),
        ]);
        login(&server, ACCESS, PROJECT, EMAIL).await;
        Self {
            server,
            dir,
            _env: (base, extra),
        }
    }

    pub(crate) fn workspace(&self) -> PathBuf {
        let ws = self.dir.path().join("workspace");
        std::fs::create_dir_all(&ws).unwrap();
        ws
    }

    /// Stream attempts answered in order; the last one repeats.
    pub(crate) async fn script(&self, attempts: Vec<ResponseTemplate>) {
        Mock::given(method("POST"))
            .and(path(STREAM))
            .respond_with(Script(attempts, AtomicUsize::new(0)))
            .mount(&self.server)
            .await;
    }

    /// Every user turn is a tool round: turn N, then `done` once results arrive.
    async fn rounds(&self) {
        Mock::given(method("POST"))
            .and(path(STREAM))
            .respond_with(Rounds {
                first: turn_n(SIG2),
                answered: done("done"),
            })
            .mount(&self.server)
            .await;
    }

    /// Raw bodies of every `streamGenerateContent` POST, in arrival order.
    pub(crate) async fn posts(&self) -> Vec<String> {
        let requests = self.server.received_requests().await.unwrap();
        hits(&requests, STREAM)
            .into_iter()
            .map(|request| String::from_utf8(request.body.clone()).unwrap())
            .collect()
    }
}

pub(crate) struct Script(Vec<ResponseTemplate>, AtomicUsize);
impl Respond for Script {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        let index = self.1.fetch_add(1, Ordering::SeqCst);
        self.0[index.min(self.0.len() - 1)].clone()
    }
}

/// A tool round per user turn; a user turn reading exactly `plain` gets the
/// text-only turn instead.
struct Rounds {
    first: ResponseTemplate,
    answered: ResponseTemplate,
}
impl Respond for Rounds {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap_or_default();
        let last = body["request"]["contents"]
            .as_array()
            .and_then(|contents| contents.last())
            .and_then(|last| last["parts"].as_array())
            .cloned()
            .unwrap_or_default();
        if last.iter().any(|p| p.get("functionResponse").is_some()) {
            self.answered.clone()
        } else if last.len() == 1 && last[0]["text"] == "plain" {
            text_turn()
        } else {
            self.first.clone()
        }
    }
}

// ── Cloud Code Assist SSE ───────────────────────────────────────────────────

pub(crate) fn frame(parts: Value, finish: Option<&str>) -> Value {
    let mut candidate = json!({"content": {"role": "model", "parts": parts}});
    if let Some(finish) = finish {
        candidate["finishReason"] = json!(finish);
    }
    json!({"response": {
        "candidates": [candidate],
        "usageMetadata": {"promptTokenCount": 3, "candidatesTokenCount": 2, "totalTokenCount": 5},
    }})
}

pub(crate) fn sse(frames: &[Value]) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(
            frames
                .iter()
                .map(|frame| format!("data: {frame}\n\n"))
                .collect::<String>(),
        )
}

/// Signed thinking `T`, text `V`, then the empty signature-only part.
fn thinking_and_text() -> Vec<Value> {
    vec![
        frame(
            json!([{"thought": true, "text": "T", "thoughtSignature": SIG0}]),
            None,
        ),
        frame(json!([{"text": "V"}]), None),
        frame(json!([{"text": "", "thoughtSignature": SIG1}]), None),
    ]
}

/// Turn N of AC9: call A signed with `sig_a`, call B unsigned, `STOP`.
fn turn_n(sig_a: &str) -> ResponseTemplate {
    let mut frames = thinking_and_text();
    frames.push(frame(
        json!([
            {"functionCall": {"name": "fixture_read", "args": {"path": "a"}}, "thoughtSignature": sig_a},
            {"functionCall": {"name": "fixture_read", "args": {"path": "b"}}},
        ]),
        Some("STOP"),
    ));
    sse(&frames)
}

/// A text-only turn: signed thinking and signed text, `STOP`.
fn text_turn() -> ResponseTemplate {
    let mut frames = thinking_and_text();
    frames.push(frame(json!([]), Some("STOP")));
    sse(&frames)
}

fn done(text: &str) -> ResponseTemplate {
    sse(&[frame(json!([{"text": text}]), Some("STOP"))])
}

/// Turn N+1's `model` content for turn N, with call A's wire signature.
fn signed_model(call_a_signature: &str) -> Value {
    json!({"role": "model", "parts": [
        {"thought": true, "text": "T", "thoughtSignature": SIG0},
        {"text": "V", "thoughtSignature": SIG1},
        {"functionCall": {"name": "fixture_read", "args": {"path": "a"}}, "thoughtSignature": call_a_signature},
        {"functionCall": {"name": "fixture_read", "args": {"path": "b"}}},
    ]})
}

/// Both results, in call order, in one user content.
fn responses() -> Value {
    json!({"role": "user", "parts": [
        {"functionResponse": {"name": "fixture_read", "response": {"output": "contents of a"}}},
        {"functionResponse": {"name": "fixture_read", "response": {"output": "contents of b"}}},
    ]})
}

fn user_content(text: &str) -> Value {
    json!({"role": "user", "parts": [{"text": text}]})
}

pub(crate) fn body(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap()
}

pub(crate) fn contents(raw: &str) -> Vec<Value> {
    body(raw)["request"]["contents"].as_array().unwrap().clone()
}

/// The in-turn POST after the tool round: turn N replayed, then both results.
fn assert_in_turn_replay(raw: &str) {
    let contents = contents(raw);
    assert_eq!(contents.len(), 3, "{raw}");
    assert_eq!(contents[1], signed_model(SIG2), "{raw}");
    assert_eq!(contents[2], responses(), "{raw}");
}

/// Every `thoughtSignature` value in request order.
pub(crate) fn signatures(value: &Value) -> Vec<String> {
    match value {
        Value::Object(map) => map
            .iter()
            .flat_map(|(key, value)| match (key.as_str(), value.as_str()) {
                ("thoughtSignature", Some(signature)) => vec![signature.to_owned()],
                _ => signatures(value),
            })
            .collect(),
        Value::Array(items) => items.iter().flat_map(signatures).collect(),
        _ => Vec::new(),
    }
}

/// A signature, the replay payload or a replayed thought part.
pub(crate) fn leaks(text: &str) -> bool {
    text.contains(MARK)
        || text.contains("gemini_parts")
        || text.contains("thoughtSignature")
        || text.contains("\"thought\":true")
}

// ── shared composition ──────────────────────────────────────────────────────

fn app_config() -> AppConfig {
    let mut cfg = AppConfig {
        model: MODEL.into(),
        ..Default::default()
    };
    cfg.provider.insert(PROVIDER.into(), config(None));
    cfg
}

/// `--yolo` security, as `build_cli_core` composes it, so a tool round never
/// parks on an approval nobody answers.
fn yolo(ws: &Path) -> Arc<dyn SecurityPort> {
    let adapter = SecurityAdapter::new(ws.to_path_buf());
    SecurityPort::set_mode(&adapter, PermissionMode::Yolo);
    Arc::new(adapter)
}

/// The `fixture_read` tool the fixture calls; its output names the requested
/// path. Not `read`: rustain treats that name as the built-in `Read`, whose
/// permission check demands a `file_path`.
struct ReadTools;

#[async_trait::async_trait]
impl ToolSetPort for ReadTools {
    fn available_tools(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "fixture_read".into(),
            description: "Read a fixture file".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"],
            }),
            parallel_safe: true,
        }]
    }

    async fn execute(
        &self,
        _tool_name: &str,
        input: Value,
        _cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult {
            tool_use_id: String::new(),
            content: format!("contents of {}", input["path"].as_str().unwrap_or_default()),
            is_error: false,
            images: Vec::new(),
        })
    }
}

fn cli_core(ws: &Path) -> CliCore {
    let provider = build();
    let security = yolo(ws);
    let tools: Arc<dyn ToolSetPort> = Arc::new(ReadTools);
    let approval = ApprovalRuntime::new(16, Arc::new(NoOpApprovalPersistence));
    let tool_scheduler = ToolScheduler::new(security.clone(), tools.clone(), approval.clone(), 16);
    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
    CliCore {
        provider,
        security,
        tools,
        tool_scheduler,
        approval,
        storage: storage(ws),
        event_tx,
        event_rx,
        ledger: Arc::new(NoOpUsageLedger),
    }
}

/// In-process `ask` over the real turn loop and session persistence.
async fn ask(ws: &Path, opts: AskOpts) -> (ExitCode, String, String) {
    let core = cli_core(ws);
    let provider = core.provider.clone();
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = tokio::time::timeout(
        GUARD,
        rustain::adapters::cli::ask::run_ask_core(
            provider,
            core,
            opts,
            &app_config(),
            ws,
            &mut out,
            &mut err,
        ),
    )
    .await
    .unwrap_or_else(|_| panic!("in-process ask did not finish within {GUARD:?}"));
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

/// `ask --output-format json`; returns the session id.
async fn ask_json(ws: &Path, query: &str, session: Option<String>) -> String {
    let (code, stdout, stderr) = ask(ws, opts(query, OutputFormat::Json, session)).await;
    assert_eq!(code, ExitCode::SUCCESS, "{stderr}");
    assert!(!leaks(&stdout) && !leaks(&stderr), "{stdout}\n{stderr}");
    let doc: Value = serde_json::from_str(&stdout).unwrap();
    doc["session_id"].as_str().unwrap().to_owned()
}

pub(crate) fn chat_user(text: &str) -> ChatMessage {
    ChatMessage {
        id: format!("user-{text}"),
        role: MessageRole::User,
        content: text.into(),
        ..Default::default()
    }
}

/// The TUI event loop's consumer, composed from the production steps
/// (`apply_chunk_for_tests`: reduce, drain the committed turn, rebuild the
/// mirror; Story 20.1d4a closes DF-20-1D4-MIRROR-REASONING-IN-CONTENT's test
/// copy).
pub(crate) fn fold(conversation: &mut Conversation, chunks: impl IntoIterator<Item = StreamChunk>) {
    let (mut state, clock) = rustain::domain::services::reducer::test_reducer_state(0);
    let mut streaming = StreamingState::default();
    for chunk in chunks {
        rustain::domain::services::reducer::apply_chunk_for_tests(
            conversation,
            &mut streaming,
            &mut state,
            chunk,
            &clock,
        );
    }
}

pub(crate) fn render(conversation: &Conversation) -> String {
    use ratatui::{Terminal, backend::TestBackend, layout::Rect};
    let mut terminal = Terminal::new(TestBackend::new(120, 60)).unwrap();
    let clock = SystemClock::default();
    let _ = terminal.draw(|frame| {
        let mut tab_render_state = TabRenderState::default();
        let _ = chat_pane::render_with_search(
            frame,
            Rect::new(0, 0, 120, 60),
            conversation,
            None,
            &StreamingState::default(),
            &ViewState::default(),
            &clock,
            0,
            true,
            &Theme::dark(),
            &mut tab_render_state,
            &HashMap::new(),
            &BTreeMap::new(),
            None,
            None,
            &[],
            &[],
            None,
            None,
            None,
            None,
            None,
        );
    });
    let buffer = terminal.backend().buffer().clone();
    (0..buffer.area().height)
        .map(|y| {
            (0..buffer.area().width)
                .map(|x| buffer.cell((x, y)).unwrap().symbol().to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn collect(
    provider: &Arc<dyn StreamingProvider>,
    messages: Vec<rustain::domain::models::Message>,
) -> Vec<StreamChunk> {
    provider
        .stream_completion(messages, options(MODEL))
        .await
        .unwrap()
        .collect()
        .await
}

fn replay_count(chunks: &[StreamChunk]) -> usize {
    chunks
        .iter()
        .filter(|chunk| matches!(chunk, StreamChunk::ProviderReplay { .. }))
        .count()
}

// ── K09 ─────────────────────────────────────────────────────────────────────

/// M08 (signature dropped from `functionCall` parts) and M09 (sentinel on every
/// unsigned call) both change POST 2's `model` content. The turn loop itself
/// executes the tool round and sends POST 2.
#[tokio::test]
#[serial]
async fn k09_turn_loop_replays_every_signed_part_and_leaves_the_second_call_bare() {
    let world = World::start().await;
    world.script(vec![turn_n(SIG2), done("done")]).await;
    let ws = world.workspace();
    ask_json(&ws, "first", None).await;
    let posts = world.posts().await;
    assert_eq!(
        posts.len(),
        2,
        "turn N, then the turn loop's POST after the tool round"
    );
    assert_eq!(contents(&posts[0]), vec![user_content("first")]);
    assert_eq!(body(&posts[1])["model"], MODEL);
    assert_eq!(
        contents(&posts[1]),
        vec![user_content("first"), signed_model(SIG2), responses()],
        "{}",
        posts[1]
    );
}

#[tokio::test]
#[serial]
async fn k09_invalid_signature_is_dropped_and_the_unsigned_first_call_gets_the_sentinel() {
    let world = World::start().await;
    world
        .script(vec![turn_n("not base64!"), done("done")])
        .await;
    let ws = world.workspace();
    ask_json(&ws, "first", None).await;
    let posts = world.posts().await;
    assert_eq!(posts.len(), 2);
    assert!(!posts[1].contains("not base64!"), "{}", posts[1]);
    assert_eq!(
        contents(&posts[1]),
        vec![user_content("first"), signed_model(SENTINEL), responses()],
        "{}",
        posts[1]
    );
}

/// An edited assistant row fails the consistency guard: the unsigned
/// reconstruction is sent, with demoted thinking only when the row carries
/// reasoning content.
#[tokio::test]
#[serial]
async fn k09_edited_turn_falls_back_to_the_unsigned_reconstruction() {
    let world = World::start().await;
    world
        .script(vec![
            text_turn(),
            done("second answer"),
            done("third answer"),
        ])
        .await;
    let ws = world.workspace();
    let session = ask_json(&ws, "first", None).await;
    let store = storage(&ws);

    let mut conversation = store.load_conversation(&session).await.unwrap().unwrap();
    let row = conversation
        .messages
        .iter_mut()
        .find(|message| message.role == MessageRole::Assistant)
        .unwrap();
    assert_eq!(row.content, "V");
    assert!(row.content_blocks.iter().any(
        |block| matches!(block, ContentBlockType::ProviderReplay(item) if item.provider == PROVIDER)
    ));
    row.content = "V edited".into();
    store.save_conversation(&conversation).await.unwrap();
    ask_json(&ws, "second", Some(session.clone())).await;
    let posts = world.posts().await;
    assert_eq!(posts.len(), 2);
    assert_eq!(
        contents(&posts[1])[1],
        json!({"role": "model", "parts": [{"text": "V edited"}]}),
        "{}",
        posts[1]
    );
    assert!(!posts[1].contains(MARK), "{}", posts[1]);

    let mut conversation = store.load_conversation(&session).await.unwrap().unwrap();
    let row = conversation
        .messages
        .iter_mut()
        .find(|message| message.role == MessageRole::Assistant)
        .unwrap();
    assert_eq!(row.content, "V edited");
    row.content_blocks
        .push(ContentBlockType::Thinking("T".into()));
    store.save_conversation(&conversation).await.unwrap();
    ask_json(&ws, "third", Some(session)).await;
    let posts = world.posts().await;
    assert_eq!(posts.len(), 3);
    assert_eq!(
        contents(&posts[2])[1],
        json!({"role": "model", "parts": [{"text": "```thinking\nT\n```"}, {"text": "V edited"}]}),
        "{}",
        posts[2]
    );
    assert!(!posts[2].contains(MARK), "{}", posts[2]);
}

/// Another Gemini model gets the payload's text unsigned and its thinking
/// demoted; another provider's builder never sees the item. (Codex scoping
/// of its own items is 20-1d1b's
/// `replay::k01_ask_tool_round_replays_before_function_call_and_after_session_resume`:
/// the Responses builder sends only items of its `replay_provider()`.)
#[tokio::test]
#[serial]
async fn k09_other_model_gets_unsigned_text_and_demoted_thinking_and_other_providers_get_nothing() {
    let world = World::start().await;
    world.script(vec![text_turn(), done("other model")]).await;
    let ws = world.workspace();
    let session = ask_json(&ws, "first", None).await;
    let mut other = opts("second", OutputFormat::Json, Some(session.clone()));
    other.model_override = Some(OTHER_MODEL.into());
    let (code, stdout, stderr) = ask(&ws, other).await;
    assert_eq!(code, ExitCode::SUCCESS, "{stderr}");
    assert!(!leaks(&stdout), "{stdout}");
    let posts = world.posts().await;
    assert_eq!(posts.len(), 2);
    assert_eq!(body(&posts[1])["model"], OTHER_MODEL);
    assert_eq!(
        contents(&posts[1])[1],
        json!({"role": "model", "parts": [{"text": "```thinking\nT\n```"}, {"text": "V"}]}),
        "{}",
        posts[1]
    );
    assert!(signatures(&body(&posts[1])).is_empty(), "{}", posts[1]);
    assert!(!posts[1].contains(MARK));

    // An OpenAI Chat request over the same persisted history carries none of it.
    let _key = Env::set(&[("OPENAI_API_KEY", Some("sk-k09-chat"))]);
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(
                    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
                ),
        )
        .mount(&world.server)
        .await;
    let chat = rustain::infrastructure::provider_factory::build_provider_for_config(
        "openai",
        &ProviderConfig {
            provider_id: "openai".into(),
            kind: Some("openai".into()),
            model_id: MODEL.into(),
            api_key_env: "OPENAI_API_KEY".into(),
            auth_header: None,
            enabled: true,
            base_url: Some(format!("{}/v1", world.server.uri()).into()),
            context_window: None,
            supports_tools: None,
            discover_models: false,
            model_filter: vec![],
            cache_ttl_seconds: 3600,
        },
    )
    .unwrap();
    let mut history = storage(&ws)
        .load_conversation(&session)
        .await
        .unwrap()
        .unwrap();
    assert!(
        history
            .messages
            .iter()
            .flat_map(|message| message.content_blocks.iter())
            .any(|block| matches!(block, ContentBlockType::ProviderReplay(item) if item.provider == PROVIDER))
    );
    history.messages.push(chat_user("to chat"));
    if let Ok(stream) = chat
        .stream_completion(build_api_messages(&history), options(MODEL))
        .await
    {
        let _: Vec<_> = stream.collect().await;
    }
    let requests = world.server.received_requests().await.unwrap();
    let chat_bodies = hits(&requests, "/v1/chat/completions");
    assert_eq!(chat_bodies.len(), 1);
    let chat_body = String::from_utf8_lossy(&chat_bodies[0].body);
    assert!(!leaks(&chat_body), "{chat_body}");
}

// ── K10: in-turn replay on every rail ───────────────────────────────────────

/// The TUI rail: `LocalTurnDriver::submit` → `run_turn`; its events are folded by
/// the event loop's reducer and rendered.
#[tokio::test]
#[serial]
async fn k10_tui_tool_round_replays_in_turn_and_no_projection_carries_it() {
    let world = World::start().await;
    world.rounds().await;
    let ws = world.workspace();
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let security = yolo(&ws);
    let tools: Arc<dyn ToolSetPort> = Arc::new(ReadTools);
    let approval = ApprovalRuntime::new(16, Arc::new(NoOpApprovalPersistence));
    let scheduler = ToolScheduler::new(security.clone(), tools.clone(), approval, 16);
    let fs = storage(&ws);
    let driver = LocalTurnDriver::new(
        build(),
        Arc::new(ArcSwap::from_pointee(app_config())),
        event_tx,
        security,
        tools,
        scheduler,
        Arc::new(SkillActivator::new()),
        Arc::new(NoOpPersona),
        Arc::new(ArcSwap::from_pointee(
            Arc::new(NoOpContext) as Arc<dyn ContextPort>
        )),
        Arc::new(ArcSwap::from_pointee(
            None as Option<Arc<dyn ContextAssemblerPort>>,
        )),
        ws.clone(),
        fs.clone(),
        fs as Arc<dyn StoragePort>,
        Arc::new(DefaultPlanInjector::new()),
        Arc::new(NoOpUsageLedger) as Arc<dyn UsageLedgerPort>,
        ActiveRatioWindow::new_in_memory(),
    );
    let mut conversation = Conversation {
        id: "k10-tui".into(),
        ..Default::default()
    };
    let mut streaming = StreamingState::default();
    let mut state = TuiState::new(120, 24);
    let mut active_turn = None;
    let mut session_manager = SessionManager::new(SessionState::Empty);
    driver
        .submit(
            UserSubmission {
                text: "first".into(),
                images: vec![],
                synthetic: false,
                activation_set: None,
                agent_snapshot: None,
                turn_cancel: CancellationToken::new(),
            },
            TurnViewState {
                conversation: &mut conversation,
                streaming: &mut streaming,
                state: &mut state,
                active_turn: &mut active_turn,
                session_manager: &mut session_manager,
            },
        )
        .await;
    let handle = active_turn
        .take()
        .expect("LocalTurnDriver launches run_turn");
    tokio::time::timeout(GUARD, handle)
        .await
        .expect("TUI turn hang guard")
        .expect("turn task must not panic");
    drop(driver);

    let posts = world.posts().await;
    assert_eq!(posts.len(), 2, "one turn: turn N, then the tool-round POST");
    assert_in_turn_replay(&posts[1]);

    let mut events = Vec::new();
    while let Ok(event) = event_rx.try_recv() {
        events.push(event);
    }
    let mut chunks = Vec::new();
    for event in &events {
        if let Some(projected) = RawEvent::from_app_event(event) {
            let encoded = serde_json::to_string(&projected).unwrap();
            assert!(!leaks(&encoded), "{encoded}");
        }
        if let AppEvent::ProviderChunk { chunk, .. } = event {
            chunks.push(chunk.clone());
        }
    }
    assert_eq!(
        replay_count(&chunks),
        2,
        "one item per response of the turn"
    );
    fold(&mut conversation, chunks);
    let screen = render(&conversation);
    assert!(screen.contains("done"), "{screen}");
    assert!(!leaks(&screen), "{screen}");
    // The persisted conversation keeps it.
    assert!(serde_json::to_string(&conversation).unwrap().contains(SIG2));
}

/// The `ask` rail: the in-turn literal, marker-free text/JSON output, the
/// session store, and the resumed history replayed exactly as in-turn (Story
/// 20.1d4a T2: per-response spans and provider call ids make the reload pass
/// the consistency guard; it was refused as a merged row before).
#[tokio::test]
#[serial]
async fn k10_ask_tool_round_replays_in_turn_and_after_the_session_resume() {
    let world = World::start().await;
    world.rounds().await;
    let ws = world.workspace();
    let session = ask_json(&ws, "first", None).await;
    let posts = world.posts().await;
    assert_eq!(posts.len(), 2);
    assert_in_turn_replay(&posts[1]);
    assert!(sessions_contain(&ws, SIG2), "the session store keeps it");

    let (code, stdout, stderr) = ask(&ws, opts("again", OutputFormat::Text, Some(session))).await;
    assert_eq!(code, ExitCode::SUCCESS, "{stderr}");
    assert!(stdout.contains("done"), "{stdout}");
    assert!(!leaks(&stdout) && !leaks(&stderr), "{stdout}\n{stderr}");
    let posts = world.posts().await;
    assert_eq!(posts.len(), 4);
    // POST 3 carries the resumed tool round exactly as in-turn: every
    // signature on its own part, no sentinel, both calls and both results.
    let reloaded = contents(&posts[2]);
    assert_eq!(reloaded[1], signed_model(SIG2), "{}", posts[2]);
    assert_eq!(reloaded[2], responses(), "{}", posts[2]);
    assert_eq!(
        signatures(&body(&posts[2])),
        vec![SIG0.to_owned(), SIG1.to_owned(), SIG2.to_owned()],
        "{}",
        posts[2]
    );
    // The new run's own tool round still replays in-turn.
    assert_eq!(
        contents(&posts[3])[contents(&posts[3]).len() - 2],
        signed_model(SIG2)
    );
}

// ── daemon rail (20-1d1b K02 pattern) ───────────────────────────────────────

const CONVERSATION: &str = "k10-daemon";

fn runtime(storage: Arc<dyn StoragePort>, ws: &Path) -> Arc<DaemonTurnRuntime> {
    let security = yolo(ws);
    let tools: Arc<dyn ToolSetPort> = Arc::new(ReadTools);
    let approval = ApprovalRuntime::new(64, Arc::new(NoOpApprovalPersistence));
    let tool_scheduler = ToolScheduler::new(security.clone(), tools.clone(), approval.clone(), 64);
    Arc::new(DaemonTurnRuntime {
        recipient_context: Arc::new(Default::default()),
        provider: build(),
        app_config: Arc::new(ArcSwap::from_pointee(app_config())),
        security,
        tools,
        tool_scheduler,
        persona: Arc::new(NoOpPersona),
        context_assembler: Arc::new(ArcSwap::from_pointee(None)),
        context: Arc::new(ArcSwap::from_pointee(
            Arc::new(NoOpContext) as Arc<dyn ContextPort>
        )),
        storage,
        fs_storage: Arc::new(FileSystemStorage::with_workspace_root(
            rustain::infrastructure::paths::sessions_dir(ws),
            ws.to_path_buf(),
        )),
        usage_ledger: Arc::new(NoOpUsageLedger),
        telemetry: ActiveRatioWindow::new_in_memory(),
        plan_injector: Arc::new(DefaultPlanInjector::new()),
        approval,
        workspace: ws.to_path_buf(),
        #[cfg(feature = "mcp")]
        mcp_task_runtimes: Vec::new(),
    })
}

struct Daemon {
    socket: PathBuf,
    channel: tokio::sync::mpsc::UnboundedSender<ChannelTurnRequest>,
    shutdown: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl Daemon {
    /// The production server: socket attach, forwarder, commit and fan-out.
    fn start(ws: &Path, conversation: Conversation, socket: &str) -> Self {
        let storage: Arc<dyn StoragePort> = storage(ws);
        let core = {
            let (ws, storage) = (ws.to_path_buf(), storage.clone());
            Arc::new(DaemonCore::new(
                ws.clone(),
                Arc::new(ArcSwap::from_pointee(app_config())),
                Arc::new(NoOpMemory),
                storage.clone(),
                Arc::new(NoOpSecurity),
                Arc::new(NoOpPersona),
                Arc::new(rustain::adapters::rap::PeerTopicStore::new()),
                Box::new(move || Ok(runtime(storage.clone(), &ws))),
            ))
        };
        let (domain_tx, domain_rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
        let server = AttachServer::new(
            core,
            Arc::new(tokio::sync::Mutex::new(conversation)),
            domain_tx,
        );
        let socket = ws.join(socket);
        let listener = UnixListener::bind(&socket).unwrap();
        let (channel, channel_rx) = tokio::sync::mpsc::unbounded_channel();
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(server.run(
            listener,
            domain_rx,
            Some(channel_rx),
            None,
            shutdown.clone(),
        ));
        Self {
            socket,
            channel,
            shutdown,
            task,
        }
    }

    async fn stop(self) {
        self.shutdown.cancel();
        self.task.abort();
        let _ = self.task.await;
    }
}

async fn daemon_frame(stream: &mut UnixStream) -> DaemonFrame {
    tokio::time::timeout(GUARD, read_frame::<_, DaemonFrame>(stream))
        .await
        .expect("daemon frame hang guard")
        .expect("decode daemon frame")
        .expect("daemon frame present")
}

async fn attach(socket: &Path) -> (UnixStream, DaemonFrame) {
    let mut stream = UnixStream::connect(socket).await.unwrap();
    let DaemonFrame::AttachChallenge { nonce } = daemon_frame(&mut stream).await else {
        panic!("challenge expected")
    };
    let keys = tempfile::tempdir().unwrap();
    let signer = IdentityKeyStore::new(keys.path())
        .load_or_generate()
        .unwrap();
    let tier = ConnectionTier::TrustedLocal;
    let proof = signer.attach_proof(&nonce, PROTOCOL_VERSION, tier.proof_tag(), false);
    write_frame(
        &mut stream,
        &ClientFrame::Attach {
            protocol_version: PROTOCOL_VERSION,
            read_only_ok: false,
            tier,
            challenge_nonce: nonce,
            identity: signer.identity().clone(),
            proof,
        },
    )
    .await
    .unwrap();
    let ack = daemon_frame(&mut stream).await;
    assert!(matches!(ack, DaemonFrame::AttachAck { .. }), "{ack:?}");
    (stream, ack)
}

/// One operator turn; every frame up to its final `EndTurn` (a tool round
/// forwards a `ToolUse` completion first).
async fn daemon_turn(stream: &mut UnixStream, text: &str) -> Vec<DaemonFrame> {
    write_frame(
        stream,
        &ClientFrame::UserMessage {
            text: text.into(),
            images: vec![],
        },
    )
    .await
    .unwrap();
    let mut frames = Vec::new();
    loop {
        let next = daemon_frame(stream).await;
        let done = matches!(
            &next,
            DaemonFrame::Event(event) if matches!(
                &event.kind,
                RawEventKind::Provider(StreamChunk::TurnComplete { stop_reason: StopReason::EndTurn })
            )
        );
        frames.push(next);
        if done {
            return frames;
        }
    }
}

fn clean(frames: &[DaemonFrame]) {
    for frame in frames {
        let encoded = serde_json::to_string(frame).unwrap();
        assert!(!leaks(&encoded), "{encoded}");
        assert!(!encoded.contains("providerReplay"), "{encoded}");
    }
}

#[tokio::test]
#[serial]
async fn k10_daemon_tool_round_replays_in_turn_projects_nothing_and_after_restart() {
    let world = World::start().await;
    world
        .script(vec![
            turn_n(SIG2),
            done("done"),
            done("channel answer"),
            done("restarted answer"),
        ])
        .await;
    let ws = world.workspace();
    let daemon = Daemon::start(
        &ws,
        Conversation {
            id: CONVERSATION.into(),
            ..Default::default()
        },
        "first.sock",
    );
    let (mut client, ack) = attach(&daemon.socket).await;
    clean(&[ack]);
    let frames = daemon_turn(&mut client, "first").await;
    clean(&frames);
    let posts = world.posts().await;
    assert_eq!(posts.len(), 2, "one daemon-hosted turn with a tool round");
    assert_in_turn_replay(&posts[1]);

    // A newly attached client's transcript snapshot and history carry none of it.
    let (mut observer, snapshot) = attach(&daemon.socket).await;
    clean(std::slice::from_ref(&snapshot));
    write_frame(
        &mut observer,
        &ClientFrame::HistoryRequest {
            before_index: None,
            count: 50,
        },
    )
    .await
    .unwrap();
    loop {
        let next = daemon_frame(&mut observer).await;
        let history = matches!(next, DaemonFrame::History { .. });
        clean(&[next]);
        if history {
            break;
        }
    }

    // The channel response is the plain text.
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    daemon
        .channel
        .send(ChannelTurnRequest {
            text: "via channel".into(),
            origin: ChannelKind::Telegram,
            response_tx,
        })
        .unwrap();
    let response = tokio::time::timeout(GUARD, response_rx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response, "channel answer");
    assert!(!leaks(&response));
    drop((client, observer));
    daemon.stop().await;

    // The session store keeps the items.
    let restored = storage(&ws)
        .load_conversation(CONVERSATION)
        .await
        .unwrap()
        .unwrap();
    let stored = restored
        .messages
        .iter()
        .flat_map(|message| message.content_blocks.iter())
        .filter(|block| matches!(block, ContentBlockType::ProviderReplay(item) if item.provider == PROVIDER))
        .count();
    assert!(stored >= 1, "the daemon persists the turn's items");
    assert!(sessions_contain(&ws, SIG2));

    // After a restart the per-response rows replay exactly as in-turn (Story
    // 20.1d4a T2; before it they failed the guard and were sent unsigned).
    let daemon = Daemon::start(&ws, restored, "second.sock");
    let (mut client, ack) = attach(&daemon.socket).await;
    clean(&[ack]);
    clean(&daemon_turn(&mut client, "after restart").await);
    let posts = world.posts().await;
    assert_eq!(posts.len(), 4);
    let reloaded = contents(&posts[3]);
    assert_eq!(reloaded[1], signed_model(SIG2), "{}", posts[3]);
    assert_eq!(reloaded[2], responses(), "{}", posts[3]);
    assert_eq!(
        signatures(&body(&posts[3])),
        vec![SIG0.to_owned(), SIG1.to_owned(), SIG2.to_owned()],
        "{}",
        posts[3]
    );
    drop(client);
    daemon.stop().await;
}

// ── ACP rail ────────────────────────────────────────────────────────────────

fn line(value: Value) -> String {
    format!("{value}\n")
}

/// `initialize → session/new → session/prompt` over the real ACP dispatcher;
/// returns every line the client received.
async fn acp_turn(ws: &Path, prompt: &str) -> Vec<String> {
    let (server_incoming, mut client_write) = tokio::io::duplex(64 * 1024);
    let (client_read, server_outgoing) = tokio::io::duplex(64 * 1024);
    let ws_for_factory = ws.to_path_buf();
    let factory: CoreFactory =
        Rc::new(move |_cwd: &Path, _mcp_servers| Ok(cli_core(&ws_for_factory)));
    let server = serve_acp_with_core_factory(
        server_outgoing.compat_write(),
        server_incoming.compat(),
        app_config(),
        ws.to_path_buf(),
        Some(MODEL.into()),
        factory,
    );
    let cwd = ws.to_path_buf();
    let prompt = prompt.to_owned();
    let drive = async move {
        let mut lines = tokio::io::BufReader::new(client_read).lines();
        for request in [
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}),
            json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":cwd,"mcpServers":[]}}),
        ] {
            client_write
                .write_all(line(request).as_bytes())
                .await
                .unwrap();
        }
        client_write.flush().await.unwrap();
        let mut seen = Vec::new();
        loop {
            let raw = tokio::time::timeout(GUARD, lines.next_line())
                .await
                .expect("ACP frame hang guard")
                .expect("read ACP frame")
                .expect("ACP stream ended before the prompt result");
            let value: Value = serde_json::from_str(&raw).unwrap();
            seen.push(raw);
            assert_ne!(
                value["method"], "session/request_permission",
                "a --yolo core never asks"
            );
            if value["id"] == json!(2) {
                let session = value["result"]["sessionId"].as_str().unwrap().to_owned();
                let request = json!({
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "session/prompt",
                    "params": {"sessionId": session, "prompt": [{"type": "text", "text": prompt}]},
                });
                client_write
                    .write_all(line(request).as_bytes())
                    .await
                    .unwrap();
                client_write.flush().await.unwrap();
            }
            if value["id"] == json!(3) {
                assert_eq!(value["result"]["stopReason"], "end_turn", "{value}");
                return seen;
            }
        }
    };
    tokio::select! {
        seen = drive => seen,
        result = server => panic!("ACP server exited before the prompt result: {result:?}"),
    }
}

#[tokio::test]
#[serial]
async fn k10_acp_tool_round_replays_in_turn_and_no_update_carries_it() {
    let world = World::start().await;
    world.rounds().await;
    let ws = world.workspace();
    let seen = acp_turn(&ws, "first").await;
    for raw in &seen {
        assert!(!leaks(raw), "{raw}");
    }
    assert!(
        seen.iter().any(|raw| raw.contains("done")),
        "the visible answer reaches the client"
    );
    let posts = world.posts().await;
    assert_eq!(posts.len(), 2, "one ACP turn with a tool round");
    assert_in_turn_replay(&posts[1]);
    assert!(sessions_contain(&ws, SIG2), "the session store keeps it");
}

// ── K10: text-only replay after a reload ────────────────────────────────────

#[tokio::test]
#[serial]
async fn k10_text_only_turn_replays_byte_identically_after_ask_resume() {
    let world = World::start().await;
    world.script(vec![text_turn(), done("again")]).await;
    let ws = world.workspace();
    let session = ask_json(&ws, "first", None).await;
    ask_json(&ws, "second", Some(session)).await;
    let posts = world.posts().await;
    assert_eq!(posts.len(), 2);
    assert!(posts[1].contains(SIGNED_TEXT_TURN), "{}", posts[1]);
    assert_eq!(contents(&posts[1])[1], body(SIGNED_TEXT_TURN));
}

/// A local session reload: the TUI reducer persists the turn, the session store
/// round-trips it, `edit` may change the reloaded history, and the next request
/// is built by `build_api_messages`. Returns that request's body.
async fn local_reload_with(
    first: ResponseTemplate,
    edit: impl FnOnce(&mut Conversation),
) -> String {
    let world = World::start().await;
    world.script(vec![first, done("again")]).await;
    let ws = world.workspace();
    let provider = build();
    let chunks = collect(&provider, vec![user("hello")]).await;
    assert_eq!(replay_count(&chunks), 1);
    let mut conversation = Conversation {
        id: "k10-local".into(),
        ..Default::default()
    };
    conversation.messages.push(chat_user("hello"));
    fold(&mut conversation, chunks);
    let store = storage(&ws);
    store.save_conversation(&conversation).await.unwrap();
    assert!(sessions_contain(&ws, SIG1), "replay state persists locally");
    let mut reloaded = store.load_conversation("k10-local").await.unwrap().unwrap();
    edit(&mut reloaded);
    reloaded.messages.push(chat_user("next"));
    collect(&provider, build_api_messages(&reloaded)).await;
    let mut posts = world.posts().await;
    assert_eq!(posts.len(), 2);
    posts.remove(1)
}

async fn local_reload(first: ResponseTemplate, expected: &str) {
    let post = local_reload_with(first, |_| {}).await;
    assert!(post.contains(expected), "{post}");
    assert_eq!(contents(&post)[1], body(expected));
}

#[tokio::test]
#[serial]
async fn k10_signed_text_turn_replays_byte_identically_after_a_local_reload() {
    local_reload(
        sse(&[frame(
            json!([{"text": "V", "thoughtSignature": SIG1}]),
            Some("STOP"),
        )]),
        r#"{"role":"model","parts":[{"text":"V","thoughtSignature":"U0lHMQ=="}]}"#,
    )
    .await;
}

#[tokio::test]
#[serial]
async fn k10_thinking_text_turn_replays_byte_identically_after_a_local_reload() {
    local_reload(text_turn(), SIGNED_TEXT_TURN).await;
}

/// The guard's negative: the reloaded assistant text is edited, so the turn is
/// rebuilt unsigned and no signature (nor the replay payload) is sent.
#[tokio::test]
#[serial]
async fn k10_an_edited_local_reload_drops_the_signatures() {
    let post = local_reload_with(text_turn(), |conversation| {
        let assistant = conversation
            .messages
            .iter_mut()
            .find(|message| message.role == MessageRole::Assistant)
            .expect("the reloaded assistant row");
        assert_eq!(assistant.content, "VT", "the TUI mirror's fold of [T, V]");
        assistant.content = "VT edited".into();
    })
    .await;
    assert!(signatures(&body(&post)).is_empty(), "{post}");
    assert!(!post.contains("gemini_parts"), "{post}");
    assert!(!post.contains("\"thought\":true"), "{post}");
    let model = &contents(&post)[1];
    assert_eq!(model["role"], "model", "{post}");
    assert!(
        model["parts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|part| part["text"] == "VT edited"),
        "{post}"
    );
}

// ── K10: privacy (20-1d1b K07 pattern) ──────────────────────────────────────

#[derive(Clone, Default)]
pub(crate) struct Capture(pub(crate) Arc<Mutex<Vec<u8>>>);
impl std::io::Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
#[serial]
async fn k10_signatures_reach_only_the_session_store() {
    let world = World::start().await;
    world.rounds().await;
    let capture = Capture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .finish();
    // Thread-scoped: the current-thread runtime runs every task of this test.
    let trace_guard = tracing::subscriber::set_default(subscriber);

    // `ask` text, JSON and stream-JSON output of a tool round.
    let ws = world.workspace();
    for format in [
        OutputFormat::Text,
        OutputFormat::Json,
        OutputFormat::StreamJson,
    ] {
        let (code, stdout, stderr) = ask(&ws, opts("hello", format, None)).await;
        assert_eq!(code, ExitCode::SUCCESS, "{stderr}");
        assert!(stdout.contains("done"), "{stdout}");
        assert!(!leaks(&stdout) && !leaks(&stderr), "{stdout}\n{stderr}");
    }
    assert!(sessions_contain(&ws, SIG0) && sessions_contain(&ws, SIG2));

    // The same provider stream through every observer projection.
    let provider = build();
    let chunks = collect(&provider, vec![user("plain")]).await;
    let replay: Vec<_> = chunks
        .iter()
        .filter(|chunk| matches!(chunk, StreamChunk::ProviderReplay { .. }))
        .cloned()
        .collect();
    assert_eq!(replay.len(), 1);
    tracing::info!(chunk = ?replay[0], "replay chunk observed");
    let (bus, _domain_rx) = EventBus::new(64);
    let mut raw = bus.subscribe_raw();
    for chunk in &chunks {
        let provider_event = AppEvent::ProviderChunk {
            conversation_id: "k10".into(),
            chunk: chunk.clone(),
        };
        let peer_event = AppEvent::PeerProviderChunk {
            node: Arc::new(rustain::domain::models::AgentId::new()),
            conversation_id: "k10".into(),
            chunk: chunk.clone(),
        };
        for event in [&provider_event, &peer_event] {
            if let Some(projected) = RawEvent::from_app_event(event) {
                assert!(!leaks(&serde_json::to_string(&projected).unwrap()));
            }
        }
        let _ = bus.emit_domain(provider_event);
        let acp = rustain::adapters::acp::translate::stream_chunk_to_session_update(
            chunk,
            Path::new("/workspace"),
        );
        if matches!(chunk, StreamChunk::ProviderReplay { .. }) {
            assert!(acp.is_none(), "replay is never an ACP update");
        }
        if let Some(update) = acp {
            assert!(!leaks(&format!("{update:?}")));
        }
    }
    let mut observed = 0;
    while let Ok(event) = raw.try_recv() {
        observed += 1;
        assert!(!leaks(&serde_json::to_string(&event).unwrap()));
    }
    assert_eq!(
        observed,
        chunks.len() - replay.len(),
        "only the replay chunk is withheld"
    );

    // The TUI renders the turn without it; the persisted conversation keeps it.
    let mut conversation = Conversation {
        id: "k10".into(),
        ..Default::default()
    };
    conversation.messages.push(chat_user("hello"));
    fold(&mut conversation, chunks);
    assert!(serde_json::to_string(&conversation).unwrap().contains(SIG0));
    let screen = render(&conversation);
    assert!(screen.contains('V'), "{screen}");
    assert!(!leaks(&screen), "{screen}");

    drop(trace_guard);
    let traced = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(traced.contains("replay chunk observed"), "capture is armed");
    // The redacting Debug prints only the byte count for a Gemini payload.
    assert!(traced.contains("bytes"));
    assert!(!leaks(&traced), "{traced}");
}
