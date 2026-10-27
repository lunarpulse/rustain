//! Shared loopback fixtures: environment guard, real `flows::login`, scripted
//! Codex SSE attempts, in-process `ask`, and a raw HTTP stub whose responses the
//! test releases explicitly (structural gates, never sleeps).
#![allow(dead_code)]

use crate::conformance_20_1d_codex_subscription::auth::{jwt, login_oauth};
use rustain::adapters::auth_store::FileAuthStore;
use rustain::adapters::cli::ask::{AskOpts, OutputFormat};
use rustain::adapters::filesystem::FileSystemStorage;
use rustain::adapters::noop::{NoOpApprovalPersistence, NoOpToolSet, NoOpUsageLedger};
use rustain::domain::models::{AppConfig, ProviderConfig};
use rustain::domain::ports::{AuthStorePort, StreamingProvider};
use rustain::domain::services::approval_runtime::ApprovalRuntime;
use rustain::domain::services::tool_scheduler::ToolScheduler;
use rustain::infrastructure::composition::CliCore;
use serde_json::{Value, json};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wiremock::{
    Mock, MockServer, Request, Respond, ResponseTemplate,
    matchers::{method, path},
};

pub(crate) const NOW: i64 = 1_800_000_000_000;
pub(crate) const RESPONSE_PATH: &str = "/backend-api/codex/responses";
pub(crate) const MODEL: &str = "gpt-5.6-luna";
/// Hang guard only: every wait below is released by an explicit event.
pub(crate) const GUARD: Duration = Duration::from_secs(30);

const ENV: &[&str] = &[
    "RUSTAIN_DATA_DIR",
    "RUSTAIN_CONFIG_DIR",
    "RUSTAIN_TEST_AUTH_ORIGIN",
    "RUSTAIN_TEST_AUTH_CALLBACK_PORT",
    "RUSTAIN_TEST_RESPONSES_RETRY_SCALE",
    "RUSTAIN_TEST_NOW_MS",
    "RUSTAIN_MODELS_DEV_URL",
    "OPENAI_CODEX_OAUTH_TOKEN",
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "OPENROUTER_API_KEY",
    "GOOGLE_API_KEY",
    "DEEPSEEK_API_KEY",
    "MOONSHOT_API_KEY",
    "XAI_API_KEY",
    "XAI_OAUTH_TOKEN",
    "XAI_BASE_URL",
];

/// Environment guard; callers hold the shared `#[serial]` lock.
pub(crate) struct World {
    pub(crate) dir: tempfile::TempDir,
    saved: Vec<(&'static str, Option<OsString>)>,
}

impl World {
    pub(crate) fn new(server: &MockServer) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let saved = ENV.iter().map(|k| (*k, std::env::var_os(k))).collect();
        // SAFETY: every environment-mutating test holds the shared serial lock.
        unsafe {
            for k in ENV {
                std::env::remove_var(k);
            }
            std::env::set_var("RUSTAIN_DATA_DIR", dir.path());
            std::env::set_var("RUSTAIN_CONFIG_DIR", dir.path());
            std::env::set_var("RUSTAIN_TEST_AUTH_ORIGIN", server.uri());
            std::env::set_var("RUSTAIN_TEST_AUTH_CALLBACK_PORT", "0");
            std::env::set_var("RUSTAIN_TEST_RESPONSES_RETRY_SCALE", "0");
            std::env::set_var("RUSTAIN_TEST_NOW_MS", NOW.to_string());
            std::env::set_var("RUSTAIN_MODELS_DEV_URL", "http://127.0.0.1:1");
        }
        Self { dir, saved }
    }
    pub(crate) fn set(&self, key: &str, value: &str) {
        // SAFETY: caller holds the serial lock.
        unsafe { std::env::set_var(key, value) }
    }
    pub(crate) fn unset(&self, key: &str) {
        // SAFETY: caller holds the serial lock.
        unsafe { std::env::remove_var(key) }
    }
    pub(crate) fn workspace(&self) -> PathBuf {
        let ws = self.dir.path().join("workspace");
        std::fs::create_dir_all(&ws).unwrap();
        ws
    }
}

impl Drop for World {
    fn drop(&mut self) {
        for (k, v) in self.saved.drain(..) {
            // SAFETY: the serial guard outlives this environment guard.
            unsafe {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }
}

/// An access JWT for a Codex account; the residency claim must never reach
/// discovery requests.
pub(crate) fn access(account: &str) -> String {
    jwt(json!({
        "https://api.openai.com/auth": {"chatgpt_account_id": account, "chatgpt_plan_type": "plus", "chatgpt_data_residency": "eu"},
        "https://api.openai.com/profile": {"email": "operator@example.test"},
    }))
}

/// A real browser-flow login against the stub (the only way rows are written).
pub(crate) async fn login(server: &MockServer, account: &str) -> String {
    let token = access(account);
    let store: Arc<dyn AuthStorePort> = Arc::new(FileAuthStore::new());
    login_oauth(
        server,
        &store,
        NOW,
        json!({"access_token": token, "refresh_token": format!("REFRESH-{account}"), "expires_in": 3600}),
    )
    .await;
    token
}

pub(crate) fn config() -> ProviderConfig {
    ProviderConfig {
        provider_id: "openai-codex".into(),
        kind: Some("openai-codex".into()),
        model_id: MODEL.into(),
        api_key_env: "OPENAI_CODEX_OAUTH_TOKEN".into(),
        auth_header: None,
        enabled: true,
        base_url: None,
        context_window: None,
        supports_tools: None,
        discover_models: true,
        model_filter: vec!["*".into()],
        cache_ttl_seconds: 3600,
    }
}

pub(crate) fn app_config() -> AppConfig {
    let mut cfg = AppConfig {
        model: MODEL.into(),
        ..Default::default()
    };
    cfg.provider.insert("openai-codex".into(), config());
    cfg
}

pub(crate) fn codex() -> Arc<dyn StreamingProvider> {
    rustain::infrastructure::provider_factory::build_provider_for_config("openai-codex", &config())
        .unwrap()
}

// ── Codex SSE events ───────────────────────────────────────────────────────

pub(crate) fn enc(marker: &str) -> String {
    format!("ENC-{marker}-secret")
}
pub(crate) fn summary(marker: &str) -> String {
    format!("SUMMARY-{marker}-private")
}
pub(crate) fn reasoning(marker: &str) -> Value {
    json!({"type":"response.output_item.done","output_index":0,"item":{
        "type":"reasoning","id":format!("rs_{marker}"),
        "summary":[{"type":"summary_text","text":summary(marker)},{"type":"summary_text","text":"second part"}],
        "encrypted_content":enc(marker)}})
}
pub(crate) fn tool_call(call: &str) -> Value {
    json!({"type":"response.output_item.done","output_index":1,"item":{
        "type":"function_call","id":format!("fc_{call}"),"call_id":call,"name":"lookup","arguments":"{\"q\":1}"}})
}
pub(crate) fn text(delta: &str) -> Value {
    json!({"type":"response.output_text.delta","delta":delta})
}
pub(crate) fn completed() -> Value {
    json!({"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":1,"output_tokens":1}}})
}
pub(crate) fn failed() -> Value {
    json!({"type":"response.failed","response":{"status":"failed","error":{"code":"server_error","message":"upstream server_error"}}})
}
/// One whitespace-only argument delta past the byte guard: a whitespace loop.
pub(crate) fn whitespace_loop() -> Vec<Value> {
    vec![
        json!({"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","id":"loop-item","call_id":"loop-call","name":"lookup"}}),
        json!({"type":"response.function_call_arguments.delta","item_id":"loop-item","output_index":2,"delta":" ".repeat(16 * 1024)}),
    ]
}
pub(crate) fn sse(events: &[Value]) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(
            events
                .iter()
                .map(|v| format!("data: {v}\n\n"))
                .collect::<String>(),
        )
}

/// Attempts answered in order; the last one repeats.
pub(crate) struct Script(Vec<ResponseTemplate>, AtomicUsize);
impl Respond for Script {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        let index = self.1.fetch_add(1, Ordering::SeqCst);
        self.0[index.min(self.0.len() - 1)].clone()
    }
}
pub(crate) async fn mount_script(server: &MockServer, attempts: Vec<ResponseTemplate>) {
    Mock::given(method("POST"))
        .and(path(RESPONSE_PATH))
        .respond_with(Script(attempts, AtomicUsize::new(0)))
        .mount(server)
        .await;
}

pub(crate) async fn posts(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method == "POST" && r.url.path() == RESPONSE_PATH)
        .map(|r| r.body_json::<Value>().unwrap())
        .collect()
}

pub(crate) fn input(body: &Value) -> &Vec<Value> {
    body["input"].as_array().unwrap()
}
/// Replayed reasoning items, in request order.
pub(crate) fn replayed(body: &Value) -> Vec<&Value> {
    input(body)
        .iter()
        .filter(|i| i["type"] == "reasoning")
        .collect()
}
pub(crate) fn position(body: &Value, test: impl Fn(&Value) -> bool) -> Option<usize> {
    input(body).iter().position(test)
}
/// The item omp replays: reduced keys, no `id`, the same encrypted payload.
pub(crate) fn assert_replays(body: &Value, marker: &str) {
    let items = replayed(body);
    let item = items
        .iter()
        .find(|i| i["encrypted_content"] == enc(marker))
        .unwrap_or_else(|| panic!("{marker} not replayed: {body}"));
    assert!(item.get("id").is_none(), "replayed item keeps its id");
    assert_eq!(item["summary"][0]["text"], summary(marker));
    assert_eq!(item["summary"].as_array().unwrap().len(), 2);
    let keys: std::collections::BTreeSet<_> = item.as_object().unwrap().keys().cloned().collect();
    assert_eq!(
        keys,
        ["encrypted_content", "summary", "type"]
            .map(String::from)
            .into_iter()
            .collect()
    );
}
pub(crate) fn mentions(value: &impl ToString, marker: &str) -> bool {
    let text = value.to_string();
    text.contains(&enc(marker)) || text.contains(&summary(marker))
}

// ── in-process `ask` ────────────────────────────────────────────────────────

pub(crate) fn storage(ws: &Path) -> Arc<FileSystemStorage> {
    Arc::new(FileSystemStorage::with_workspace_root(
        rustain::infrastructure::paths::sessions_dir(ws),
        ws.to_path_buf(),
    ))
}

pub(crate) fn opts(query: &str, format: OutputFormat, session: Option<String>) -> AskOpts {
    AskOpts {
        query: query.into(),
        files: vec![],
        yolo: true,
        final_message_only: false,
        output_format: format,
        model_override: None,
        session_id: session,
        dry_run: false,
    }
}

/// Run `ask` in process over the real turn loop and persistence. Security is
/// composed as `build_cli_core` does: `--yolo` sets the real adapter to Yolo, so
/// a tool round never parks on an approval nobody answers.
pub(crate) async fn ask(
    provider: Arc<dyn StreamingProvider>,
    ws: &Path,
    opts: AskOpts,
) -> (ExitCode, String, String) {
    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
    let adapter = rustain::adapters::security_adapter::SecurityAdapter::new(ws.to_path_buf());
    if opts.yolo {
        rustain::domain::ports::SecurityPort::set_mode(
            &adapter,
            rustain::domain::models::PermissionMode::Yolo,
        );
    }
    let security: Arc<dyn rustain::domain::ports::SecurityPort> = Arc::new(adapter);
    let tools: Arc<dyn rustain::domain::ports::ToolSetPort> = Arc::new(NoOpToolSet);
    let approval = ApprovalRuntime::new(16, Arc::new(NoOpApprovalPersistence));
    let tool_scheduler = ToolScheduler::new(security.clone(), tools.clone(), approval.clone(), 16);
    let core = CliCore {
        provider: provider.clone(),
        security,
        tools,
        tool_scheduler,
        approval,
        storage: storage(ws),
        event_tx,
        event_rx,
        ledger: Arc::new(NoOpUsageLedger),
    };
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

/// Whether any persisted session file under `ws` contains `needle`.
pub(crate) fn sessions_contain(ws: &Path, needle: &str) -> bool {
    fn walk(dir: &Path, needle: &str) -> bool {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        entries.flatten().any(|entry| {
            let p = entry.path();
            if p.is_dir() {
                walk(&p, needle)
            } else {
                std::fs::read(&p)
                    .map(|bytes| String::from_utf8_lossy(&bytes).contains(needle))
                    .unwrap_or(false)
            }
        })
    }
    walk(&rustain::infrastructure::paths::sessions_dir(ws), needle)
}

// ── raw loopback HTTP: the test decides when each response is released ──────

pub(crate) struct Held {
    pub(crate) head: String,
    pub(crate) stream: tokio::net::TcpStream,
}

impl Held {
    pub(crate) fn header(&self, name: &str) -> Option<String> {
        self.head.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_owned())
        })
    }
    pub(crate) fn path(&self) -> String {
        self.head
            .split_whitespace()
            .nth(1)
            .unwrap_or_default()
            .to_owned()
    }
    pub(crate) async fn json(mut self, body: &Value) {
        let body = body.to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        self.stream.write_all(response.as_bytes()).await.unwrap();
        self.stream.shutdown().await.ok();
    }
}

pub(crate) async fn accept(listener: &tokio::net::TcpListener) -> Held {
    tokio::time::timeout(GUARD, async {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            let n = stream.read(&mut chunk).await.unwrap();
            assert!(n > 0, "client closed before a full request");
            buf.extend_from_slice(&chunk[..n]);
            if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..end]).into_owned();
                let held = Held { head, stream };
                let length: usize = held
                    .header("content-length")
                    .map(|v| v.parse().unwrap())
                    .unwrap_or(0);
                let mut stream = held.stream;
                while buf.len() < end + 4 + length {
                    let n = stream.read(&mut chunk).await.unwrap();
                    assert!(n > 0);
                    buf.extend_from_slice(&chunk[..n]);
                }
                return Held {
                    head: held.head,
                    stream,
                };
            }
        }
    })
    .await
    .expect("stub request hang guard")
}
