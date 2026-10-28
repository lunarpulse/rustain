//! Shared Copilot fixtures: the real `flows::login` against the loopback stub,
//! the production `[provider]` entry, request builders and per-route SSE bodies.
#![allow(dead_code)]

pub(crate) use crate::conformance_20_1d1b_codex_replay_discovery::common::{
    GUARD, Held, NOW, World, accept, completed, sse as responses_sse, text as responses_text,
};
pub(crate) use crate::conformance_20_1d2_device_and_xai::device::{
    Event, Recorder, assert_nothing_stored, body, failure, json_reply, oauth, received, rows,
    run_login,
};
use futures::StreamExt;
use futures::stream::BoxStream;
use rustain::adapters::auth::flows::{FlowError, LoginResult};
use rustain::adapters::auth::policies::COPILOT_DEVICE;
use rustain::domain::errors::ProviderError;
use rustain::domain::models::{
    AuthMethod, CompletionOptions, ImageAttachment, Message, MessageRole, ProviderConfig,
    RedactedUrl, StreamChunk, ToolResultMessage,
};
use rustain::domain::ports::StreamingProvider;
use rustain::infrastructure::provider_factory::build_provider_for_config;
use serde_json::{Value, json};
use std::sync::Arc;
use wiremock::{
    Mock, MockGuard, MockServer, Request, ResponseTemplate,
    matchers::{method, path, path_regex},
};

pub(crate) const PROVIDER: &str = "github-copilot";
pub(crate) const METHOD: AuthMethod = AuthMethod::Device(&COPILOT_DEVICE);
pub(crate) const PROMPT: &str =
    "GitHub Enterprise URL/domain (blank for github.com) (company.ghe.com): ";
pub(crate) const DEVICE_PATH: &str = "/login/device/code";
pub(crate) const TOKEN_PATH: &str = "/login/oauth/access_token";
pub(crate) const PROBE_PATH: &str = "/copilot_internal/user";
/// Policy enablement paths under any root: `<root>/models/<id>/policy`.
pub(crate) const POLICY_PATH: &str = r"/models/[^/]+/policy$";
pub(crate) const USER_CODE: &str = "ABCD-1234";
pub(crate) const DEVICE_CODE: &str = "DEVICE-CODE-COPILOT";
pub(crate) const VERIFY: &str = "https://github.com/login/device";
pub(crate) const PUBLIC_CLIENT: &str = "Ov23li8tweQw6odWQebz";
pub(crate) const ENTERPRISE_CLIENT: &str = "Ov23ctDVkRmgkPke0Mmm";
/// What the routing probe reports by default; the stub sees requests under
/// `/tenant-a` (rebasing keeps the path).
pub(crate) const ENDPOINT_A: &str = "https://api.individual.githubcopilot.com/tenant-a";
pub(crate) const ROOT_A: &str = "/tenant-a";
pub(crate) const ENDPOINT_B: &str = "https://api.business.githubcopilot.com/tenant-b";
pub(crate) const ROOT_B: &str = "/tenant-b";
/// omp's ten-Julian-year lifetime of a stored Copilot grant.
pub(crate) const LIFETIME_MS: i64 = 315_576_000_000;
pub(crate) const SEED_IDS: usize = 55;
pub(crate) const FORBIDDEN: &str = "GitHub Copilot access denied (HTTP 403). Your token is valid but the account may not have access to this model or feature. Check your Copilot plan or model policy settings. Business organizations can also restrict which clients may call the API: rustain sends Copilot-Integration-Id copilot-chat by default (COPILOT_INTEGRATION_ID overrides it) and retries a denied default-identity request once as the Copilot CLI (copilot-developer-cli). If both identities are denied, ask your org admin to allow one of them; if you pinned an identity, try the other.";

// ── login stub ──────────────────────────────────────────────────────────────

pub(crate) fn device_reply(interval: f64, expires_in: f64) -> ResponseTemplate {
    json_reply(
        200,
        json!({
            "device_code": DEVICE_CODE,
            "user_code": USER_CODE,
            "verification_uri": VERIFY,
            "interval": interval,
            "expires_in": expires_in,
        }),
    )
}
pub(crate) fn token_reply(token: &str) -> ResponseTemplate {
    json_reply(
        200,
        json!({"access_token": token, "token_type": "bearer", "scope": "read:user"}),
    )
}
pub(crate) fn poll_error(error: &str) -> ResponseTemplate {
    json_reply(200, json!({"error": error}))
}
pub(crate) fn probe_reply(api: &str) -> ResponseTemplate {
    json_reply(200, json!({"login": "octocat", "endpoints": {"api": api}}))
}
pub(crate) fn device_form(client: &str) -> String {
    format!("client_id={client}&scope=read%3Auser")
}
pub(crate) fn poll_form(client: &str) -> String {
    format!(
        "client_id={client}&device_code={DEVICE_CODE}&grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code"
    )
}

/// The login routes of one successful login, mounted scoped: device code,
/// a first-poll success with `token`, the probe answering `probe`, and every
/// policy POST answered 200.
pub(crate) async fn mount_login(
    server: &MockServer,
    recorder: &Recorder,
    token: &str,
    probe: ResponseTemplate,
) -> Vec<MockGuard> {
    vec![
        recorder
            .mount_scoped(server, "POST", DEVICE_PATH, vec![device_reply(5.0, 900.0)])
            .await,
        recorder
            .mount_scoped(server, "POST", TOKEN_PATH, vec![token_reply(token)])
            .await,
        recorder
            .mount_scoped(server, "GET", PROBE_PATH, vec![probe])
            .await,
        Mock::given(method("POST"))
            .and(path_regex(POLICY_PATH))
            .respond_with(json_reply(200, json!({})))
            .mount_as_scoped(server)
            .await,
    ]
}

/// A real `github-copilot` device login (the only way rows are written): the
/// prompt is answered `answer`, the first poll yields `token` and the probe
/// reports `probe`. The login mocks are unmounted on return. Callers hold
/// `World` with the origin set to `server`.
pub(crate) async fn login_with(
    server: &MockServer,
    answer: &str,
    token: &str,
    probe: ResponseTemplate,
) -> LoginResult {
    let recorder = Recorder::new().answering(answer);
    let _mocks = mount_login(server, &recorder, token, probe).await;
    run_login(&recorder, PROVIDER, &METHOD)
        .await
        .expect("the Copilot login succeeds")
}

/// A public login whose probe reports `endpoint`.
pub(crate) async fn login(server: &MockServer, token: &str, endpoint: &str) -> LoginResult {
    login_with(server, "", token, probe_reply(endpoint)).await
}

/// The login of `recorder` (prompt answer, sleeps, cancellation) with
/// `flows::login` and the production policy.
pub(crate) async fn try_login(recorder: &Recorder) -> Result<LoginResult, FlowError> {
    run_login(recorder, PROVIDER, &METHOD).await
}

// ── provider entry ──────────────────────────────────────────────────────────

pub(crate) fn config(model: &str, base: Option<&str>) -> ProviderConfig {
    ProviderConfig {
        provider_id: PROVIDER.into(),
        kind: Some(PROVIDER.into()),
        model_id: model.into(),
        api_key_env: "COPILOT_GITHUB_TOKEN".into(),
        auth_header: None,
        enabled: true,
        base_url: base.map(RedactedUrl::from),
        context_window: None,
        supports_tools: None,
        discover_models: true,
        model_filter: vec!["*".into()],
        cache_ttl_seconds: 3600,
    }
}

/// The production front door for a Copilot entry.
pub(crate) fn build(model: &str) -> Arc<dyn StreamingProvider> {
    build_provider_for_config(PROVIDER, &config(model, None)).expect("Copilot provider builds")
}
pub(crate) fn build_at(
    model: &str,
    base: &str,
) -> Result<Arc<dyn StreamingProvider>, ProviderError> {
    build_provider_for_config(PROVIDER, &config(model, Some(base)))
}

// ── requests ────────────────────────────────────────────────────────────────

pub(crate) fn message(role: MessageRole, content: &str) -> Message {
    Message {
        role,
        content: content.into(),
        images: vec![],
        tool_results: vec![],
        tool_uses: vec![],
        context_prefix: None,
        reasoning_content: None,
        provider_replay: vec![],
    }
}
pub(crate) fn user(content: &str) -> Message {
    message(MessageRole::User, content)
}
pub(crate) fn assistant(content: &str) -> Message {
    message(MessageRole::Assistant, content)
}
pub(crate) fn tool_result(call: &str, output: &str) -> Message {
    let mut message = message(MessageRole::User, "");
    message.tool_results.push(ToolResultMessage {
        tool_use_id: call.into(),
        content: output.into(),
        is_error: false,
    });
    message
}
pub(crate) fn with_image(mut message: Message) -> Message {
    message.images.push(ImageAttachment {
        media_type: "image/png".into(),
        data: "AA==".into(),
    });
    message
}
pub(crate) fn options(model: &str, max_tokens: u32, session: Option<&str>) -> CompletionOptions {
    CompletionOptions {
        model: model.into(),
        max_tokens,
        system_prompt: String::new(),
        temperature: None,
        tools: vec![],
        session_id: session.map(str::to_owned),
    }
}

/// Every chunk of one completion, under the hang guard.
pub(crate) async fn drain(stream: BoxStream<'static, StreamChunk>) -> Vec<StreamChunk> {
    tokio::time::timeout(GUARD, stream.collect::<Vec<_>>())
        .await
        .expect("stream hang guard")
}
/// The concatenated text of a completion.
pub(crate) fn text_of(chunks: &[StreamChunk]) -> String {
    chunks
        .iter()
        .filter_map(|chunk| match chunk {
            StreamChunk::Text { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .collect()
}
/// One full completion: stream and drain, or the open error.
pub(crate) async fn complete(
    provider: &Arc<dyn StreamingProvider>,
    messages: Vec<Message>,
    options: CompletionOptions,
) -> Result<Vec<StreamChunk>, ProviderError> {
    let stream = tokio::time::timeout(GUARD, provider.stream_completion(messages, options))
        .await
        .expect("open hang guard")?;
    Ok(drain(stream).await)
}

pub(crate) fn header(request: &Request, name: &str) -> Option<String> {
    request
        .headers
        .get(name)
        .map(|value| value.to_str().unwrap().to_owned())
}
pub(crate) fn json_body(request: &Request) -> Value {
    serde_json::from_slice(&request.body).unwrap()
}
/// Requests to `route` with `verb`, in arrival order.
pub(crate) async fn requests(server: &MockServer, verb: &str, route: &str) -> Vec<Request> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.method.as_str() == verb && request.url.path() == route)
        .collect()
}
/// Policy POSTs, in arrival order.
pub(crate) async fn policy_posts(server: &MockServer) -> Vec<Request> {
    let pattern = regex::Regex::new(POLICY_PATH).unwrap();
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.method.as_str() == "POST" && pattern.is_match(request.url.path()))
        .collect()
}

// ── route answers ───────────────────────────────────────────────────────────

/// Mounts `verb route` answering `template` (unscoped).
pub(crate) async fn reply(
    server: &MockServer,
    verb: &str,
    route: &str,
    template: ResponseTemplate,
) {
    Mock::given(method(verb))
        .and(path(route))
        .respond_with(template)
        .mount(server)
        .await;
}

fn event_stream(frames: &[String]) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(frames.concat())
}

/// A Chat Completions stream answering `text`.
pub(crate) fn chat_sse(text: &str) -> ResponseTemplate {
    event_stream(&[
        format!(
            "data: {}\n\n",
            json!({"object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":text},"finish_reason":null}]})
        ),
        format!(
            "data: {}\n\n",
            json!({"object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}})
        ),
        "data: [DONE]\n\n".into(),
    ])
}

/// An Anthropic Messages stream answering `text`.
pub(crate) fn messages_sse(text: &str) -> ResponseTemplate {
    let frame = |event: &str, data: Value| format!("event: {event}\ndata: {data}\n\n");
    event_stream(&[
        frame(
            "message_start",
            json!({"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"m","content":[],"stop_reason":null,"usage":{"input_tokens":3,"output_tokens":1}}}),
        ),
        frame(
            "content_block_start",
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        ),
        frame(
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
        ),
        frame(
            "content_block_stop",
            json!({"type":"content_block_stop","index":0}),
        ),
        frame(
            "message_delta",
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}),
        ),
        frame("message_stop", json!({"type":"message_stop"})),
    ])
}

/// A Responses stream answering `text`.
pub(crate) fn responses_ok(text: &str) -> ResponseTemplate {
    responses_sse(&[responses_text(text), completed()])
}

/// The three route paths under `root`.
pub(crate) fn messages_path(root: &str) -> String {
    format!("{root}/v1/messages")
}
pub(crate) fn responses_path(root: &str) -> String {
    format!("{root}/responses")
}
pub(crate) fn chat_path(root: &str) -> String {
    format!("{root}/chat/completions")
}
/// Mounts all three routes under `root`, each answering `text`.
pub(crate) async fn mount_routes(server: &MockServer, root: &str, text: &str) {
    reply(server, "POST", &messages_path(root), messages_sse(text)).await;
    reply(server, "POST", &responses_path(root), responses_ok(text)).await;
    reply(server, "POST", &chat_path(root), chat_sse(text)).await;
}
