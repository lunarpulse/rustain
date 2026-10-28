//! Shared fixtures: a production login through 20-1d4's helpers, the isolated
//! world every Gemini CLI suite uses, discovery stubs and a raw loopback origin
//! for connection-level faults.

pub(crate) use crate::conformance_20_1d4_gemini_cli::auth::{
    ACCESS, CLIENT_SECRET, EMAIL, Env, LOAD, MODEL, PROJECT, PROVIDER, REFRESH, STREAM, TOKEN, env,
    hits, login, ok, options, store, token_body, user,
};
use rustain::adapters::model_catalog_cache::{
    CachedDiscoveryEntry, DiscoveryTarget, ModelCatalogCache,
};
use rustain::domain::models::{AppConfig, ModelDescriptor};
use rustain::infrastructure::startup::ProviderLayer;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wiremock::{
    Mock, MockServer, Request, Respond, ResponseTemplate,
    matchers::{body_string_contains, method, path},
};

pub(crate) const FETCH: &str = "/v1internal:fetchAvailableModels";
pub(crate) const QUOTA: &str = "/v1internal:retrieveUserQuota";
pub(crate) const NAME: &str = "Google Cloud Code Assist (Gemini CLI)";
pub(crate) const HINT: &str = "Run `rustain auth login google-gemini-cli`";
pub(crate) const ANTIGRAVITY_UA: &str =
    "antigravity/hub/2.19.1 (aidev_client; os_type=darwin; arch=arm64; cl=963137146)";
pub(crate) const CLIENT_METADATA: &str =
    "ideType=IDE_UNSPECIFIED,platform=PLATFORM_UNSPECIFIED,pluginType=GEMINI";
/// Nothing listens here; in-process catalogue tests never fetch models.dev.
pub(crate) const NO_MODELS_DEV: &str = "http://127.0.0.1:1";
pub(crate) const REFRESHED: &str = "GEMINI-ACCESS-REFRESHED";
/// omp `models.json` `google-gemini-cli`: id, name, context — the bundled seed.
pub(crate) const SEED: [(&str, &str, u32); 7] = [
    ("gemini-2.0-flash", "Gemini 2.0 Flash", 1_048_576),
    ("gemini-2.5-flash", "Gemini 2.5 Flash", 1_048_576),
    ("gemini-2.5-pro", "Gemini 2.5 Pro", 1_048_576),
    (
        "gemini-3-flash-preview",
        "Gemini 3 Flash Preview",
        1_048_576,
    ),
    ("gemini-3-pro-preview", "Gemini 3 Pro Preview", 1_000_000),
    (
        "gemini-3.1-flash-lite-preview",
        "Gemini 3.1 Flash Lite Preview",
        1_048_576,
    ),
    (
        "gemini-3.1-pro-preview",
        "Gemini 3.1 Pro Preview",
        1_048_576,
    ),
];

/// Every other provider credential is cleared so the Gemini CLI login is the only
/// signed entry; config dir isolated; models.dev points at `models_dev`.
pub(crate) fn isolate(dir: &std::path::Path, models_dev: &str) -> Env {
    let config_dir = dir.join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    Env::set(&[
        ("RUSTAIN_CONFIG_DIR", Some(config_dir.to_str().unwrap())),
        ("RUSTAIN_MODELS_DEV_URL", Some(models_dev)),
        ("ANTHROPIC_API_KEY", None),
        ("ANTHROPIC_AUTH_TOKEN", None),
        ("ANTHROPIC_BASE_URL", None),
        ("OPENAI_API_KEY", None),
        ("OPENAI_CODEX_OAUTH_TOKEN", None),
        ("OPENROUTER_API_KEY", None),
        ("DEEPSEEK_API_KEY", None),
        ("MOONSHOT_API_KEY", None),
        ("XAI_API_KEY", None),
        ("XAI_OAUTH_TOKEN", None),
        ("XAI_BASE_URL", None),
        ("COPILOT_GITHUB_TOKEN", None),
        ("GH_TOKEN", None),
        ("GITHUB_TOKEN", None),
    ])
}

/// No configured providers: `google-gemini-cli` is the bundled entry.
pub(crate) fn app_config() -> AppConfig {
    AppConfig {
        model: MODEL.into(),
        ..Default::default()
    }
}

/// An isolated world signed in by a production login of `ACCESS`/`PROJECT`/`EMAIL`.
pub(crate) struct World {
    pub(crate) server: MockServer,
    pub(crate) temp: tempfile::TempDir,
    _env: Env,
    _isolated: Env,
}

impl World {
    pub(crate) async fn signed_in() -> Self {
        let world = Self::signed_out().await;
        login(&world.server, ACCESS, PROJECT, EMAIL).await;
        world
    }

    pub(crate) async fn signed_out() -> Self {
        let server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();
        let env = env(temp.path(), &server.uri());
        let isolated = isolate(temp.path(), NO_MODELS_DEV);
        Self {
            server,
            temp,
            _env: env,
            _isolated: isolated,
        }
    }

    pub(crate) async fn requests(&self) -> Vec<Request> {
        self.server.received_requests().await.unwrap()
    }

    /// Requests after the first `baseline` ones.
    pub(crate) async fn since(&self, baseline: usize) -> Vec<Request> {
        self.requests().await.split_off(baseline)
    }
}

pub(crate) fn target<'a>(layer: &'a ProviderLayer, id: &str) -> &'a DiscoveryTarget {
    layer
        .discovery_targets
        .iter()
        .find(|t| t.provider_id == id)
        .unwrap_or_else(|| panic!("{id} is not a live discovery target"))
}

pub(crate) fn served(layer: &ProviderLayer) -> Vec<ModelDescriptor> {
    layer.registry.list_models_by_provider(PROVIDER)
}

pub(crate) fn rows(models: &[ModelDescriptor]) -> Vec<(String, String, u32)> {
    models
        .iter()
        .map(|m| (m.model_id.clone(), m.display_name.clone(), m.context_window))
        .collect()
}

pub(crate) fn ids(models: &[ModelDescriptor]) -> Vec<String> {
    models.iter().map(|m| m.model_id.clone()).collect()
}

/// The seed rows followed by `extra`, as served `(id, name, context)` literals.
pub(crate) fn seed_then(extra: &[(&str, &str, u32)]) -> Vec<(String, String, u32)> {
    SEED.iter()
        .chain(extra)
        .map(|(id, name, context)| ((*id).to_owned(), (*name).to_owned(), *context))
        .collect()
}

pub(crate) async fn disk(id: &str) -> CachedDiscoveryEntry {
    ModelCatalogCache::new()
        .load()
        .await
        .providers
        .remove(id)
        .unwrap_or_else(|| panic!("no cache entry for {id}"))
}

pub(crate) fn header<'a>(request: &'a Request, name: &str) -> &'a str {
    request
        .headers
        .get(name)
        .unwrap_or_else(|| panic!("missing header {name}"))
        .to_str()
        .unwrap()
}

/// The Gemini CLI wire identity with Node platform/arch names.
pub(crate) fn gemini_cli_agent(model: &str) -> String {
    let platform = match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    };
    format!("GeminiCLI/0.46.0/{model} ({platform}; {arch}; terminal)")
}

/// Responses served in order, once each; any further request answers 599.
pub(crate) struct Script(Mutex<VecDeque<ResponseTemplate>>);
impl Script {
    pub(crate) fn new(responses: Vec<ResponseTemplate>) -> Self {
        Self(Mutex::new(responses.into()))
    }
}
impl Respond for Script {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        self.0
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| ResponseTemplate::new(599))
    }
}

pub(crate) async fn mount_antigravity(server: &MockServer, responses: Vec<ResponseTemplate>) {
    Mock::given(method("POST"))
        .and(path(FETCH))
        .respond_with(Script::new(responses))
        .mount(server)
        .await;
}

pub(crate) async fn mount_quota(server: &MockServer, responses: Vec<ResponseTemplate>) {
    Mock::given(method("POST"))
        .and(path(QUOTA))
        .respond_with(Script::new(responses))
        .mount(server)
        .await;
}

/// Antigravity rejects on both hosts, then the quota answers `buckets`.
pub(crate) async fn mount_quota_path(server: &MockServer, buckets: &[&str]) {
    mount_antigravity(
        server,
        vec![ResponseTemplate::new(403), ResponseTemplate::new(403)],
    )
    .await;
    mount_quota(server, vec![ok(quota_body(buckets))]).await;
}

pub(crate) fn quota_body(ids: &[&str]) -> Value {
    json!({"buckets": ids.iter().map(|id| json!({"modelId": id, "remainingFraction": 1})).collect::<Vec<_>>()})
}

/// The refresh grant answers a new access token and omits refresh token, email
/// and project, which the store merge keeps.
pub(crate) async fn mount_refresh(server: &MockServer) {
    mount_refresh_to(server, REFRESHED).await;
}

pub(crate) async fn mount_refresh_to(server: &MockServer, access: &str) {
    Mock::given(method("POST"))
        .and(path(TOKEN))
        .and(body_string_contains("grant_type=refresh_token"))
        .respond_with(ok(token_body(access, None)))
        .with_priority(1)
        .mount(server)
        .await;
}

/// Asserts no discovery request carries the refresh token or the client secret.
pub(crate) fn assert_no_long_lived_secrets(requests: &[Request]) {
    for request in requests
        .iter()
        .filter(|r| [FETCH, QUOTA, LOAD].contains(&r.url.path()))
    {
        let mut seen = String::from_utf8_lossy(&request.body).into_owned();
        for (name, value) in &request.headers {
            seen.push_str(name.as_str());
            seen.push_str(value.to_str().unwrap_or_default());
        }
        assert!(
            !seen.contains(REFRESH) && !seen.contains(CLIENT_SECRET),
            "{}",
            request.url
        );
    }
}

/// One `streamGenerateContent` SSE answer.
pub(crate) fn text_reply(text: &str) -> ResponseTemplate {
    let frame = json!({"response": {"candidates": [{
        "content": {"role": "model", "parts": [{"text": text}]},
        "finishReason": "STOP"
    }]}});
    ResponseTemplate::new(200).set_body_raw(format!("data: {frame}\n\n"), "text/event-stream")
}

pub(crate) async fn mount_stream(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path(STREAM))
        .respond_with(text_reply("ok"))
        .mount(server)
        .await;
}

/// One request a [`RawOrigin`] accepted.
#[derive(Debug, Clone)]
pub(crate) struct RawRequest {
    pub(crate) path: String,
    pub(crate) body: String,
}

/// A loopback origin answering connection `n` with `script[n]`: `Some((status,
/// body))` responds and closes; `None` drops the connection unanswered.
pub(crate) struct RawOrigin {
    pub(crate) uri: String,
    pub(crate) seen: Arc<Mutex<Vec<RawRequest>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for RawOrigin {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl RawOrigin {
    pub(crate) async fn start(script: Vec<Option<(u16, String)>>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let uri = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let task = tokio::spawn(async move {
            for step in script {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let request = read_request(&mut stream).await;
                log.lock().unwrap().push(request);
                if let Some((status, body)) = step {
                    let response = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                }
            }
        });
        Self { uri, seen, task }
    }

    pub(crate) fn paths(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.path.clone())
            .collect()
    }
}

async fn read_request(stream: &mut tokio::net::TcpStream) -> RawRequest {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];
    let head_end = loop {
        let read = stream.read(&mut chunk).await.unwrap();
        assert_ne!(read, 0, "connection closed before the request head");
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(at) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let length: usize = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())
                .flatten()
        })
        .unwrap_or(0);
    while buffer.len() < head_end + length {
        let read = stream.read(&mut chunk).await.unwrap();
        assert_ne!(read, 0, "connection closed before the request body");
        buffer.extend_from_slice(&chunk[..read]);
    }
    let path = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or_default()
        .to_owned();
    RawRequest {
        path,
        body: String::from_utf8_lossy(&buffer[head_end..head_end + length]).into_owned(),
    }
}
