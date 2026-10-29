//! xAI refresh and recovery (K06), Generic request body (K07), URL and headers (K08)
//! and transport resilience (K09) through `build_provider_for_config`, plus the xAI
//! plain tests: output limits, generic orphan/hoist, usage orchestration, generic
//! error texts, probe/health statuses, `Debug` redaction and `xai` API-key login.
//! Every stored OAuth row comes from a real `flows::login` device login against the
//! loopback stub; the API-key row comes from `flows::login(ApiKey)`.

use super::device::{
    OIDC_PATH, PROVIDER, TOKEN_PATH, USERINFO_PATH, body, json_reply, login_xai_oauth, oauth,
    received, rows, unreachable_origin, xai_access,
};
use crate::conformance_20_1d1b_codex_replay_discovery::common::{
    self as codex, GUARD, Held, NOW, World, accept, completed, enc, reasoning, sse, text,
};
use futures::{FutureExt, StreamExt, future::BoxFuture};
use parking_lot::Mutex;
use rustain::adapters::auth::flows::{self, FlowCtx, LoginUi};
use rustain::adapters::auth_store::FileAuthStore;
use rustain::adapters::xai::{XaiAdapter, XaiKind};
use rustain::domain::errors::ProviderError;
use rustain::domain::models::{
    AuthMethod, CompletionOptions, Credential, CredentialOrigin, ImageAttachment, Message,
    MessageRole, ProviderConfig, ProviderReplayItem, RedactedUrl, SecretString, StopReason,
    StreamChunk, ToolDefinition, ToolResultMessage, ToolUseMessage,
};
use rustain::domain::ports::{AuthStorePort, StreamingProvider};
use rustain::infrastructure::key_resolver::{CredentialHandle, shared};
use rustain::infrastructure::provider_factory::build_provider_for_config;
use serde_json::{Value, json};
use serial_test::serial;
use std::ffi::OsString;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;
use wiremock::{
    Mock, MockServer, Request, Respond, ResponseTemplate,
    matchers::{method, path},
};

const RESPONSES: &str = "/v1/responses";
const MODELS: &str = "/v1/models";
const KEY: &str = "xai-KEY-0123456789";
const MODEL: &str = "grok-4.6";
const REFRESH: &str = "XAI-REFRESH";
const ROTATED: &str = "XAI-ROTATED";
const EVIL: &str = "https://evil.example/evil-token";
const UNTERMINATED: &str =
    "OpenAI responses stream closed before a terminal response event was received";
const FIRST_EVENT: &str = "OpenAI responses stream timed out while waiting for the first event";
const INTERRUPTED: &str =
    "[No tool output recorded: the tool call was interrupted before it produced a result.]";
const PLACEHOLDER: &str = "[image omitted: model does not support vision]";

// ── fixtures ────────────────────────────────────────────────────────────────

/// Saves and clears the xAI credential/base variables; `World` owns the rest.
/// Callers hold the shared `#[serial]` lock.
struct XaiEnv(Vec<(&'static str, Option<OsString>)>);
impl XaiEnv {
    fn new() -> Self {
        let names = ["XAI_API_KEY", "XAI_OAUTH_TOKEN", "XAI_BASE_URL"];
        let saved = names.map(|k| (k, std::env::var_os(k))).to_vec();
        // SAFETY: every environment-mutating test holds the shared serial lock.
        unsafe {
            for k in names {
                std::env::remove_var(k);
            }
        }
        Self(saved)
    }
}
impl Drop for XaiEnv {
    fn drop(&mut self) {
        for (k, v) in self.0.drain(..) {
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

/// `World` (data dir, test origin, retry knob 0, fixed clock) plus cleared xAI env.
/// Fields drop in order: the xAI variables are restored before `World`'s.
struct Scene {
    _env: XaiEnv,
    world: World,
}
fn scene(server: &MockServer) -> Scene {
    let world = World::new(server);
    Scene {
        _env: XaiEnv::new(),
        world,
    }
}
/// A scene whose `xai` provider resolves `XAI_API_KEY`.
fn key_scene(server: &MockServer) -> Scene {
    let scene = scene(server);
    scene.world.set("XAI_API_KEY", KEY);
    scene
}

fn config(kind: &str, base: Option<&str>) -> ProviderConfig {
    ProviderConfig {
        provider_id: kind.into(),
        kind: Some(kind.into()),
        model_id: MODEL.into(),
        api_key_env: if kind == "xai" {
            "XAI_API_KEY"
        } else {
            "XAI_OAUTH_TOKEN"
        }
        .into(),
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
/// The production front door: `build_provider_for_config` for an xAI kind.
fn build(kind: &str, base: Option<&str>) -> Arc<dyn StreamingProvider> {
    build_provider_for_config(kind, &config(kind, base)).expect("xAI provider builds")
}

fn message(role: MessageRole, content: &str) -> Message {
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
fn user(content: &str) -> Message {
    message(MessageRole::User, content)
}
fn with_image(mut message: Message) -> Message {
    message.images.push(ImageAttachment {
        media_type: "image/png".into(),
        data: "AA==".into(),
    });
    message
}
fn options(model: &str, max_tokens: u32, session: Option<&str>) -> CompletionOptions {
    CompletionOptions {
        model: model.into(),
        max_tokens,
        system_prompt: String::new(),
        temperature: None,
        tools: vec![],
        session_id: session.map(str::to_owned),
    }
}
fn hi() -> Vec<Message> {
    vec![user("hi")]
}
fn opts() -> CompletionOptions {
    options(MODEL, 1024, Some("conv-1"))
}

/// A non-empty successful turn (a text delta keeps it out of the empty-completion rule).
fn ok_sse() -> ResponseTemplate {
    sse(&[text("ok"), completed()])
}
fn completed_with(usage: Value) -> Value {
    json!({"type":"response.completed","response":{"status":"completed","usage":usage}})
}
/// A plain stop with no output and `output_tokens: 0`: an empty completion.
fn empty_completion() -> Value {
    completed_with(json!({"input_tokens":5,"output_tokens":0}))
}

/// Attempts answered in order; the last one repeats.
struct Attempts(Vec<ResponseTemplate>, AtomicUsize);
impl Respond for Attempts {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        let index = self.1.fetch_add(1, Ordering::SeqCst);
        self.0[index.min(self.0.len() - 1)].clone()
    }
}
async fn mount_attempts(server: &MockServer, attempts: Vec<ResponseTemplate>) {
    Mock::given(method("POST"))
        .and(path(RESPONSES))
        .respond_with(Attempts(attempts, AtomicUsize::new(0)))
        .mount(server)
        .await;
}

/// `route` answers `ok` to requests carrying `Bearer <accepted>`, otherwise 401.
async fn mount_bearer_gate(
    server: &MockServer,
    verb: &str,
    route: &str,
    accepted: &str,
    ok: ResponseTemplate,
) {
    let bearer = format!("Bearer {accepted}");
    Mock::given(method(verb))
        .and(path(route))
        .respond_with(move |request: &Request| {
            if header(request, "authorization").as_deref() == Some(bearer.as_str()) {
                ok.clone()
            } else {
                json_reply(401, json!({"error":{"message":"token expired"}}))
            }
        })
        .mount(server)
        .await;
}

/// OIDC discovery returning `endpoint`, the token endpoint issuing `access` with a
/// rotated refresh token, and userinfo naming the refreshed identity.
async fn mount_refresh(server: &MockServer, endpoint: &str, access: &str) {
    Mock::given(method("GET"))
        .and(path(OIDC_PATH))
        .respond_with(json_reply(
            200,
            json!({"issuer":"https://auth.x.ai","token_endpoint":endpoint}),
        ))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(json_reply(
            200,
            json!({"access_token":access,"refresh_token":ROTATED,"expires_in":3600,"token_type":"Bearer"}),
        ))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(USERINFO_PATH))
        .respond_with(json_reply(
            200,
            json!({"sub":"refreshed-sub","email":"refreshed@example.test"}),
        ))
        .mount(server)
        .await;
}

/// A real `xai-oauth` device login storing `access` / `REFRESH`.
async fn login(server: &MockServer, access: &str) {
    login_xai_oauth(
        server,
        access,
        REFRESH,
        json_reply(200, json!({"sub":"login-sub","email":"grok@example.test"})),
    )
    .await;
}

fn header(request: &Request, name: &str) -> Option<String> {
    request
        .headers
        .get(name)
        .map(|value| value.to_str().unwrap().to_owned())
}
/// Every request the stub received from index `from` on, as `METHOD path`.
async fn requests_since(server: &MockServer, from: usize) -> Vec<String> {
    server.received_requests().await.unwrap()[from..]
        .iter()
        .map(|r| format!("{} {}", r.method, r.url.path()))
        .collect()
}
async fn request_count(server: &MockServer) -> usize {
    server.received_requests().await.unwrap().len()
}
fn auth_file(scene: &Scene) -> Vec<u8> {
    std::fs::read(scene.world.dir.path().join("auth.json")).expect("auth.json written by login")
}

async fn turn(
    provider: &Arc<dyn StreamingProvider>,
    messages: Vec<Message>,
    options: CompletionOptions,
) -> Result<Vec<StreamChunk>, ProviderError> {
    match provider.stream_completion(messages, options).await {
        Ok(stream) => Ok(stream.collect().await),
        Err(error) => Err(error),
    }
}
fn texts(chunks: &[StreamChunk]) -> Vec<&str> {
    chunks
        .iter()
        .filter_map(|chunk| match chunk {
            StreamChunk::Text { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .collect()
}
fn errors(chunks: &[StreamChunk]) -> Vec<&str> {
    chunks
        .iter()
        .filter_map(|chunk| match chunk {
            StreamChunk::Error { content } => Some(content.as_str()),
            _ => None,
        })
        .collect()
}
fn replays(chunks: &[StreamChunk]) -> Vec<&ProviderReplayItem> {
    chunks
        .iter()
        .filter_map(|chunk| match chunk {
            StreamChunk::ProviderReplay { item } => Some(item),
            _ => None,
        })
        .collect()
}
fn stop(chunks: &[StreamChunk]) -> Option<&StopReason> {
    match chunks.last() {
        Some(StreamChunk::TurnComplete { stop_reason }) => Some(stop_reason),
        _ => None,
    }
}
fn assert_ok_turn(chunks: &[StreamChunk]) {
    assert_eq!(texts(chunks), ["ok"], "{chunks:?}");
    assert!(errors(chunks).is_empty(), "{chunks:?}");
    assert_eq!(stop(chunks), Some(&StopReason::EndTurn), "{chunks:?}");
}
async fn posted_bodies(server: &MockServer) -> Vec<String> {
    received(server, RESPONSES).await.iter().map(body).collect()
}

/// Serialised exactly as the product serialises a `serde_json::Value`.
fn js(value: Value) -> String {
    serde_json::to_string(&value).unwrap()
}
/// A JSON object whose top-level keys appear in exactly this order.
fn object(fields: &[(&str, String)]) -> String {
    let fields: Vec<String> = fields
        .iter()
        .map(|(key, value)| format!("{}:{value}", js(json!(key))))
        .collect();
    format!("{{{}}}", fields.join(","))
}
fn tool_literal(name: &str, description: &str, parameters: Value) -> String {
    object(&[
        ("type", js(json!("function"))),
        ("name", js(json!(name))),
        ("description", js(json!(description))),
        ("parameters", js(parameters)),
    ])
}

/// K2: capture, reducer/session persistence, real resolver refresh and another
/// request on the same provider. The opaque login cannot supply a JWT identity.
#[tokio::test]
#[serial]
async fn login_generation_replay_survives_identity_enrichment_and_routine_refresh() {
    use crate::conformance_20_1d1b_codex_replay_discovery::replay::{fold, user as chat_user};
    use rustain::domain::models::Conversation;
    use rustain::domain::services::message_builder::build_api_messages;

    for identified in [false, true] {
        for expired in [false, true] {
            let server = MockServer::start().await;
            let scene = scene(&server);
            let initial = "OPAQUE-XAI-LOGIN";
            login_xai_oauth(
                &server,
                initial,
                REFRESH,
                if identified {
                    json_reply(
                        200,
                        json!({"sub":"refreshed-sub","email":"refreshed@example.test"}),
                    )
                } else {
                    json_reply(503, json!({}))
                },
            )
            .await;
            let before = rows(PROVIDER).await.remove(0);
            assert_eq!(oauth(&before).identity.account_id.is_some(), identified);
            let expected = rustain::domain::models::secret::scope_digest(&[
                PROVIDER.as_bytes(),
                b"login",
                &before.generation.0,
            ]);
            let provider = build(PROVIDER, None);
            mount_bearer_gate(
                &server,
                "POST",
                RESPONSES,
                initial,
                sse(&[reasoning("before-refresh"), text("ok"), completed()]),
            )
            .await;
            let chunks = turn(&provider, hi(), opts()).await.unwrap();
            assert_ok_turn(&chunks);
            let original = replays(&chunks)[0].payload.clone();
            assert_eq!(
                replays(&chunks)[0].scope.as_deref(),
                Some(expected.as_str())
            );
            let mut conversation = Conversation::default();
            conversation.messages.push(chat_user("hi"));
            fold(&mut conversation, chunks);
            let saved = serde_json::to_string(&conversation).unwrap();
            conversation = serde_json::from_str(&saved).unwrap();
            conversation.messages.push(chat_user("before refresh"));
            assert_ok_turn(
                &turn(&provider, build_api_messages(&conversation), opts())
                    .await
                    .unwrap(),
            );
            let sent: Value =
                serde_json::from_str(posted_bodies(&server).await.last().unwrap()).unwrap();
            assert!(
                sent["input"]
                    .as_array()
                    .unwrap()
                    .contains(original.as_ref())
            );

            // Explicit checkpoint removes the initial bearer gate, not history.
            server.reset().await;
            mount_refresh(
                &server,
                super::device::TOKEN_ENDPOINT,
                "OPAQUE-XAI-REFRESHED",
            )
            .await;
            mount_bearer_gate(
                &server,
                "POST",
                RESPONSES,
                "OPAQUE-XAI-REFRESHED",
                sse(&[reasoning("after-refresh"), text("ok"), completed()]),
            )
            .await;
            if expired {
                scene
                    .world
                    .set("RUSTAIN_TEST_NOW_MS", &(NOW + 10_000_000).to_string());
            }
            let chunks = turn(&provider, build_api_messages(&conversation), opts())
                .await
                .unwrap();
            assert_ok_turn(&chunks);
            let refreshed = replays(&chunks)[0].payload.clone();
            assert_eq!(
                replays(&chunks)[0].scope.as_deref(),
                Some(expected.as_str())
            );
            let after = rows(PROVIDER).await.remove(0);
            assert_eq!(before.id, after.id);
            assert_eq!(before.generation, after.generation);
            assert_eq!(after.rev, before.rev + 1);
            assert_eq!(
                oauth(&after).identity.account_id.as_deref(),
                Some("refreshed-sub")
            );
            let sends = received(&server, RESPONSES).await;
            assert_eq!(sends.len(), if expired { 1 } else { 2 });
            assert_eq!(
                header(sends.last().unwrap(), "authorization").as_deref(),
                Some("Bearer OPAQUE-XAI-REFRESHED")
            );
            for request in &sends {
                let sent: Value = serde_json::from_slice(&request.body).unwrap();
                assert!(
                    sent["input"]
                        .as_array()
                        .unwrap()
                        .contains(original.as_ref())
                );
            }
            if !expired {
                assert_eq!(
                    header(&sends[0], "authorization").as_deref(),
                    Some("Bearer OPAQUE-XAI-LOGIN")
                );
                assert_eq!(sends[0].body, sends[1].body);
            }
            assert_eq!(received(&server, TOKEN_PATH).await.len(), 1);

            fold(&mut conversation, chunks);
            let saved = serde_json::to_string(&conversation).unwrap();
            conversation = serde_json::from_str(&saved).unwrap();
            conversation.messages.push(chat_user("after refresh"));
            assert_ok_turn(
                &turn(&provider, build_api_messages(&conversation), opts())
                    .await
                    .unwrap(),
            );
            let sends = received(&server, RESPONSES).await;
            let last = sends.last().unwrap();
            assert_eq!(
                header(last, "authorization").as_deref(),
                Some("Bearer OPAQUE-XAI-REFRESHED")
            );
            let sent: Value = serde_json::from_slice(&last.body).unwrap();
            let input = sent["input"].as_array().unwrap();
            assert!(input.contains(original.as_ref()) && input.contains(refreshed.as_ref()));
            assert_eq!(received(&server, TOKEN_PATH).await.len(), 1);
        }
    }
}
// ── K06: refresh and recovery ───────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn k06_stored_401_rediscovers_refreshes_with_three_fields_and_resends_once() {
    let server = MockServer::start().await;
    let _scene = scene(&server);
    let initial = xai_access("login-sub");
    let next = xai_access("refreshed-sub");
    login(&server, &initial).await;
    let provider = build("xai-oauth", None);
    mount_refresh(&server, super::device::TOKEN_ENDPOINT, &next).await;
    mount_bearer_gate(&server, "POST", RESPONSES, &next, ok_sse()).await;
    let from = request_count(&server).await;

    let chunks = turn(&provider, hi(), opts()).await.expect("recovered turn");
    assert_ok_turn(&chunks);

    // One 401, then discovery, the refresh POST, userinfo, and one resend.
    assert_eq!(
        requests_since(&server, from).await,
        [
            format!("POST {RESPONSES}"),
            format!("GET {OIDC_PATH}"),
            format!("POST {TOKEN_PATH}"),
            format!("GET {USERINFO_PATH}"),
            format!("POST {RESPONSES}"),
        ]
    );
    let all = server.received_requests().await.unwrap();
    let after = &all[from..];
    let sends: Vec<_> = after.iter().filter(|r| r.url.path() == RESPONSES).collect();
    assert_eq!(
        header(sends[0], "authorization"),
        Some(format!("Bearer {initial}"))
    );
    assert_eq!(
        header(sends[1], "authorization"),
        Some(format!("Bearer {next}"))
    );
    // The resend carries the same once-encoded body.
    assert_eq!(sends[0].body, sends[1].body);
    let refresh = after
        .iter()
        .find(|r| r.url.path() == TOKEN_PATH)
        .expect("refresh POST");
    assert_eq!(
        body(refresh),
        format!(
            "grant_type=refresh_token&client_id=b1a00492-073a-47ea-816f-4c329264a828&refresh_token={REFRESH}"
        )
    );
    assert_eq!(
        header(refresh, "content-type").as_deref(),
        Some("application/x-www-form-urlencoded")
    );
    // omp's refresh request has no extra header; only the client default may appear.
    assert!(
        matches!(header(refresh, "accept").as_deref(), None | Some("*/*")),
        "refresh sends no Accept policy header: {:?}",
        header(refresh, "accept")
    );
    let info = after
        .iter()
        .find(|r| r.url.path() == USERINFO_PATH)
        .unwrap();
    assert_eq!(
        header(info, "authorization"),
        Some(format!("Bearer {next}"))
    );

    let rows = rows(PROVIDER).await;
    assert_eq!(rows.len(), 1);
    assert!(rows[0].disabled_cause.is_none());
    let row = oauth(&rows[0]);
    assert_eq!(row.access.expose_secret(), next);
    assert_eq!(row.refresh.expose_secret(), ROTATED);
    assert_eq!(row.expires_at_ms, NOW + 3_600_000 - 300_000);
    assert_eq!(row.identity.account_id.as_deref(), Some("refreshed-sub"));
    assert_eq!(
        row.identity.email.as_deref(),
        Some("refreshed@example.test")
    );
}

#[tokio::test]
#[serial]
async fn k06_stale_row_refreshes_before_the_first_send() {
    let server = MockServer::start().await;
    let scene = scene(&server);
    let initial = xai_access("login-sub");
    let next = xai_access("refreshed-sub");
    login(&server, &initial).await;
    let provider = build("xai-oauth", None);
    mount_refresh(&server, super::device::TOKEN_ENDPOINT, &next).await;
    mount_bearer_gate(&server, "POST", RESPONSES, &next, ok_sse()).await;
    let later = NOW + 10_000_000;
    scene.world.set("RUSTAIN_TEST_NOW_MS", &later.to_string());
    let from = request_count(&server).await;

    assert_ok_turn(&turn(&provider, hi(), opts()).await.unwrap());
    assert_eq!(
        requests_since(&server, from).await,
        [
            format!("GET {OIDC_PATH}"),
            format!("POST {TOKEN_PATH}"),
            format!("GET {USERINFO_PATH}"),
            format!("POST {RESPONSES}"),
        ]
    );
    let row = rows(PROVIDER).await;
    let row = oauth(&row[0]);
    assert_eq!(row.access.expose_secret(), next);
    assert_eq!(row.expires_at_ms, later + 3_600_000 - 300_000);
}

#[tokio::test]
#[serial]
async fn k06_foreign_discovered_endpoint_never_receives_the_refresh_token() {
    let server = MockServer::start().await;
    let scene = scene(&server);
    let initial = xai_access("login-sub");
    login(&server, &initial).await;
    let provider = build("xai-oauth", None);
    // A validator that let the foreign endpoint through would rebase it to this stub.
    mount_refresh(&server, EVIL, &xai_access("never")).await;
    let foreign = Arc::new(AtomicUsize::new(0));
    let seen = foreign.clone();
    Mock::given(method("POST"))
        .and(path("/evil-token"))
        .respond_with(move |_: &Request| {
            seen.fetch_add(1, Ordering::SeqCst);
            json_reply(200, json!({"access_token":"stolen","expires_in":3600}))
        })
        .mount(&server)
        .await;
    mount_attempts(
        &server,
        vec![json_reply(
            401,
            json!({"error":{"message":"token expired"}}),
        )],
    )
    .await;
    let before = auth_file(&scene);
    let from = request_count(&server).await;

    let error = turn(&provider, hi(), opts())
        .await
        .expect_err("the refused refresh fails the request");
    // 20-1b2 seam: a transient refresh failure surfaces as ConnectionFailed and
    // neither rotates nor disables the row.
    match &error {
        ProviderError::ConnectionFailed(message) => {
            assert!(
                message.contains(&format!("Invalid xAI token_endpoint: {EVIL}")),
                "{message}"
            );
            for secret in [initial.as_str(), REFRESH, "stolen"] {
                assert!(!message.contains(secret), "{message}");
            }
        }
        other => panic!("refused refresh endpoint mapped to {other:?}"),
    }
    assert_eq!(foreign.load(Ordering::SeqCst), 0);
    assert_eq!(
        requests_since(&server, from).await,
        [format!("POST {RESPONSES}"), format!("GET {OIDC_PATH}")],
        "exactly one 401 send, one discovery, no refresh POST and no resend"
    );
    assert_eq!(auth_file(&scene), before, "row bytes unchanged");

    // The refused refresh leaves the row usable until it expires.
    server.reset().await;
    mount_bearer_gate(&server, "POST", RESPONSES, &initial, ok_sse()).await;
    assert_ok_turn(&turn(&provider, hi(), opts()).await.unwrap());
    let rows = rows(PROVIDER).await;
    assert!(rows[0].disabled_cause.is_none());
    assert_eq!(oauth(&rows[0]).access.expose_secret(), initial);
}

#[tokio::test]
#[serial]
async fn k06_env_token_and_api_key_401_never_refresh() {
    for kind in ["xai-oauth", "xai"] {
        let server = MockServer::start().await;
        let scene = scene(&server);
        if kind == "xai" {
            scene.world.set("XAI_API_KEY", KEY);
        } else {
            scene
                .world
                .set("XAI_OAUTH_TOKEN", &xai_access("env-token-sub"));
        }
        mount_refresh(&server, super::device::TOKEN_ENDPOINT, "never").await;
        mount_attempts(
            &server,
            vec![json_reply(
                401,
                json!({"error":{"message":"bad credential"}}),
            )],
        )
        .await;
        let provider = build(kind, None);
        assert!(
            matches!(
                turn(&provider, hi(), opts()).await,
                Err(ProviderError::AuthenticationFailed)
            ),
            "{kind}"
        );
        assert_eq!(
            requests_since(&server, 0).await,
            [format!("POST {RESPONSES}")],
            "{kind}: one send, no discovery, no refresh"
        );
    }
}

// ── K07: Generic body per model class ───────────────────────────────────────

fn lookup_tool() -> ToolDefinition {
    ToolDefinition {
        name: "lookup".into(),
        description: "Find a record".into(),
        // A typeless required-key root union: flattened for xAI. `anyOf` stays last.
        input_schema: json!({"type":"object","properties":{"id":{"type":"string"},"name":{"type":"string"}},"anyOf":[{"required":["id"]},{"required":["name"]}]}),
        parallel_safe: false,
    }
}
fn contradictory_tool() -> ToolDefinition {
    ToolDefinition {
        name: "pick".into(),
        description: "Pick a mode".into(),
        // `enum` contradicts `type`: quarantined, the tool is omitted.
        input_schema: json!({"type":"object","properties":{"mode":{"type":"string","enum":["a",1]}}}),
        parallel_safe: false,
    }
}
fn plain_tool() -> ToolDefinition {
    ToolDefinition {
        name: "search".into(),
        description: "Search".into(),
        input_schema: json!({"type":"object","properties":{"q":{"type":"string"}},"required":["q"]}),
        parallel_safe: false,
    }
}
fn replay(provider: &str, model: &str, marker: &str, scope: Option<&str>) -> ProviderReplayItem {
    ProviderReplayItem {
        provider: provider.into(),
        model: model.into(),
        payload: Arc::new(
            json!({"type":"reasoning","summary":[],"encrypted_content":format!("ENC-{marker}")}),
        ),
        scope: scope.map(str::to_owned),
    }
}

#[tokio::test]
#[serial]
async fn k07_generic_bodies_per_model_class_and_codex_unchanged_in_process() {
    let server = MockServer::start().await;
    let _scene = scene(&server);
    login(&server, &xai_access("login-sub")).await;
    let provider = build("xai-oauth", None);
    mount_attempts(
        &server,
        vec![sse(&[reasoning("producer"), text("ok"), completed()])],
    )
    .await;
    let produced = turn(&provider, hi(), opts()).await.unwrap();
    assert_ok_turn(&produced);
    let scope = replays(&produced)[0].scope.clone().unwrap();
    // Deliberate K07 repair: identified OAuth replay follows login generation,
    // not discovery identity. Preserve discovery's independently checked policy.
    let row = rows(PROVIDER).await.remove(0);
    let identity = oauth(&row).identity.key(false).unwrap();
    assert_eq!(
        provider.discovery_scope(PROVIDER).await,
        Some(rustain::adapters::model_catalog_cache::credential_scope(
            PROVIDER,
            rustain::adapters::model_catalog_cache::ScopeCredential::OAuthIdentity(&identity),
        )),
    );
    assert_ne!(
        provider.discovery_scope(PROVIDER).await.as_deref(),
        Some(scope.as_str())
    );
    // Explicit request checkpoint: the setup producer is not a literal-body case.
    server.reset().await;
    mount_attempts(&server, vec![ok_sse()]).await;

    // Reasoning, vision model: every optional field, `include` last. Only the
    // item this account produced (its replay scope) for this kind and model replays.
    let scope = Some(scope.as_str());
    let mut assistant = message(MessageRole::Assistant, "hi");
    assistant.provider_replay = vec![
        replay("xai-oauth", MODEL, "same", scope),
        replay("xai-oauth", "grok-4.5", "other-model", scope),
        replay("xai", MODEL, "other-kind", scope),
        replay(
            "xai-oauth",
            MODEL,
            "other-account",
            Some("0000000000000000"),
        ),
        replay("xai-oauth", MODEL, "unscoped", None),
    ];
    let history = vec![
        message(MessageRole::System, "History rule"),
        with_image(user("hello")),
        assistant,
        user("next"),
    ];
    let mut reasoning_options = options(MODEL, 8192, Some("conv-K07"));
    reasoning_options.system_prompt = "Be brief".into();
    reasoning_options.temperature = Some(0.5);
    reasoning_options.tools = vec![lookup_tool(), contradictory_tool(), plain_tool()];
    assert_ok_turn(&turn(&provider, history, reasoning_options).await.unwrap());

    // Non-reasoning model: no `include`, no temperature, no session, no tools.
    assert_ok_turn(
        &turn(
            &provider,
            vec![user("hello")],
            options("grok-4.20-0309-non-reasoning", 100_000, None),
        )
        .await
        .unwrap(),
    );

    // Non-vision model: the image becomes the placeholder text; no output limit.
    assert_ok_turn(
        &turn(
            &provider,
            vec![with_image(user("look"))],
            options("grok-composer-2.5-fast", 0, None),
        )
        .await
        .unwrap(),
    );

    let user_item = |content: Value| json!({"role":"user","content":content});
    let expected = [
        object(&[
            ("model", js(json!(MODEL))),
            (
                "input",
                js(json!([
                    user_item(json!([
                        {"type":"input_text","text":"hello"},
                        {"type":"input_image","detail":"auto","image_url":"data:image/png;base64,AA=="}
                    ])),
                    {"type":"reasoning","summary":[],"encrypted_content":"ENC-same"},
                    {"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi","annotations":[]}]},
                    user_item(json!([{"type":"input_text","text":"next"}])),
                ])),
            ),
            ("instructions", js(json!("Be brief\n\nHistory rule"))),
            ("stream", js(json!(true))),
            ("prompt_cache_key", js(json!("conv-K07"))),
            ("store", js(json!(false))),
            ("max_output_tokens", js(json!(8192))),
            ("temperature", js(json!(0.5))),
            (
                "tools",
                format!(
                    "[{},{}]",
                    tool_literal(
                        "lookup",
                        "Find a record",
                        json!({"type":"object","properties":{"id":{"type":"string"},"name":{"type":"string"}}}),
                    ),
                    tool_literal(
                        "search",
                        "Search",
                        json!({"type":"object","properties":{"q":{"type":"string"}},"required":["q"]}),
                    ),
                ),
            ),
            ("include", js(json!(["reasoning.encrypted_content"]))),
        ]),
        object(&[
            ("model", js(json!("grok-4.20-0309-non-reasoning"))),
            (
                "input",
                js(json!([user_item(
                    json!([{"type":"input_text","text":"hello"}])
                )])),
            ),
            ("stream", js(json!(true))),
            ("store", js(json!(false))),
            ("max_output_tokens", js(json!(64000))),
        ]),
        object(&[
            ("model", js(json!("grok-composer-2.5-fast"))),
            (
                "input",
                js(json!([user_item(json!([
                    {"type":"input_text","text":"look"},
                    {"type":"input_text","text":PLACEHOLDER}
                ]))])),
            ),
            ("stream", js(json!(true))),
            ("store", js(json!(false))),
        ]),
    ];
    assert_eq!(posted_bodies(&server).await, expected);

    // The Codex request in the same process keeps every Codex rule.
    codex::login(&server, "acct-k07").await;
    codex::mount_script(&server, vec![sse(&[text("ok"), completed()])]).await;
    let mut codex_options = options(codex::MODEL, 8192, Some("conv-K07"));
    codex_options.system_prompt = "Be brief".into();
    codex_options.temperature = Some(0.5);
    codex_options.tools = vec![lookup_tool(), contradictory_tool(), plain_tool()];
    let codex_provider = codex::codex();
    let chunks = turn(
        &codex_provider,
        vec![message(MessageRole::System, "History rule"), user("hello")],
        codex_options,
    )
    .await
    .unwrap();
    assert!(errors(&chunks).is_empty(), "{chunks:?}");
    let raw = body(&received(&server, codex::RESPONSE_PATH).await[0]);
    let mut previous = 0;
    for key in [
        "\"model\"",
        "\"input\"",
        "\"stream\"",
        "\"prompt_cache_key\"",
        "\"tools\"",
        "\"instructions\"",
        "\"store\"",
        "\"include\"",
        "\"client_metadata\"",
    ] {
        let position = raw
            .find(key)
            .unwrap_or_else(|| panic!("Codex {key}: {raw}"));
        assert!(position >= previous, "Codex field order {key}: {raw}");
        previous = position;
    }
    let codex_body: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(
        codex_body["input"][0],
        json!({"type":"message","role":"developer","content":[{"type":"input_text","text":"History rule"}]})
    );
    assert_eq!(codex_body["instructions"], "Be brief");
    assert_eq!(
        codex_body["include"],
        json!(["reasoning.encrypted_content"])
    );
    assert_eq!(codex_body["tools"].as_array().unwrap().len(), 3);
    assert!(codex_body["tools"][0]["parameters"].get("anyOf").is_some());
    for field in ["temperature", "max_output_tokens"] {
        assert!(codex_body.get(field).is_none(), "Codex sent {field}");
    }
}

#[tokio::test]
#[serial]
async fn output_limit_and_traits_follow_the_kind_table_and_unknown_ids_have_neither() {
    let server = MockServer::start().await;
    let _scene = key_scene(&server);
    mount_attempts(&server, vec![ok_sse()]).await;
    let provider = build("xai", None);
    // (model, requested, max_output_tokens, include sent, image kept)
    let cases = [
        ("grok-2-vision", 8192, Some(4096), false, true),
        (MODEL, 8192, Some(8192), true, true),
        (MODEL, 100_000, Some(64_000), true, true),
        ("grok-code-fast-1", 100_000, Some(10_000), true, false),
        ("grok-live-only", 100_000, Some(64_000), false, false),
        (MODEL, 0, None, true, true),
    ];
    for (model, requested, _, _, _) in cases {
        let chunks = turn(
            &provider,
            vec![with_image(user("look"))],
            options(model, requested, None),
        )
        .await
        .unwrap();
        assert_ok_turn(&chunks);
    }
    let bodies = posted_bodies(&server).await;
    assert_eq!(bodies.len(), cases.len());
    for ((model, requested, limit, include, image), raw) in cases.into_iter().zip(bodies) {
        let body: Value = serde_json::from_str(&raw).unwrap();
        let case = format!("{model} {requested}");
        assert_eq!(body["max_output_tokens"].as_u64(), limit, "{case}");
        assert_eq!(body.get("include").is_some(), include, "{case}");
        let content = &body["input"][0]["content"][1];
        if image {
            assert_eq!(content["type"], "input_image", "{case}");
        } else {
            assert_eq!(
                content,
                &json!({"type":"input_text","text":PLACEHOLDER}),
                "{case}"
            );
        }
    }
}

#[tokio::test]
#[serial]
async fn generic_orphan_note_truncation_interrupted_placeholder_and_hoist() {
    let server = MockServer::start().await;
    let _scene = key_scene(&server);
    mount_attempts(&server, vec![ok_sse()]).await;
    let provider = build("xai", None);
    let mut call = message(MessageRole::Assistant, "");
    call.tool_uses = vec![ToolUseMessage {
        id: "c1".into(),
        name: "lookup".into(),
        input: json!({"id":1}),
    }];
    let interim = message(MessageRole::Assistant, "interim");
    let mut results = user("");
    let result = |id: &str, content: String| ToolResultMessage {
        tool_use_id: id.into(),
        content,
        is_error: false,
        images: Vec::new(),
    };
    results.tool_results = vec![
        result("c1", "found".into()),
        result("orphan", "old".into()),
        result("long", "x".repeat(16_001)),
    ];
    let mut interrupted = message(MessageRole::Assistant, "");
    interrupted.tool_uses = vec![ToolUseMessage {
        id: "c2".into(),
        name: "lookup".into(),
        input: json!({}),
    }];
    let history = vec![user("q"), call, interim, results, interrupted, user("next")];
    assert_ok_turn(
        &turn(&provider, history, options(MODEL, 1024, None))
            .await
            .unwrap(),
    );
    let body: Value = serde_json::from_str(&posted_bodies(&server).await[0]).unwrap();
    let note = |text: String| json!({"type":"message","role":"assistant","content":text});
    assert_eq!(
        body["input"],
        json!([
            {"role":"user","content":[{"type":"input_text","text":"q"}]},
            // Hoisted ahead of the call → output batch it was wedged into.
            {"type":"message","role":"assistant","content":[{"type":"output_text","text":"interim","annotations":[]}]},
            {"type":"function_call","call_id":"c1","name":"lookup","arguments":"{\"id\":1}"},
            {"type":"function_call_output","call_id":"c1","output":"found"},
            note("[Orphan tool result; call_id=orphan]: old".into()),
            note(format!("[Orphan tool result; call_id=long]: {}\n...[truncated]", "x".repeat(16_000))),
            {"type":"function_call","call_id":"c2","name":"lookup","arguments":"{}"},
            {"type":"function_call_output","call_id":"c2","output":INTERRUPTED},
            {"role":"user","content":[{"type":"input_text","text":"next"}]},
        ])
    );
}

// ── K08: URL and headers ────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn k08_base_url_rules_per_kind_and_configured_base_wins() {
    let stub2 = MockServer::start().await;
    let stub3 = MockServer::start().await;
    let env_base = format!("{}/v1", stub2.uri());
    // (case, kind, configured base, credential) → the stub that must receive the send
    for case in [
        "xai env base",
        "xai-oauth row",
        "xai-oauth env jwt",
        "xai configured",
    ] {
        let stub1 = MockServer::start().await;
        let scene = scene(&stub1);
        for stub in [&stub1, &stub2, &stub3] {
            stub.reset().await;
            mount_attempts(stub, vec![ok_sse()]).await;
        }
        let (kind, configured, target) = match case {
            "xai env base" => {
                scene.world.set("XAI_API_KEY", KEY);
                ("xai", None, &stub2)
            }
            "xai-oauth row" => {
                login(&stub1, &xai_access("login-sub")).await;
                ("xai-oauth", None, &stub1)
            }
            "xai-oauth env jwt" => {
                scene.world.set("XAI_OAUTH_TOKEN", &xai_access("env-sub"));
                ("xai-oauth", None, &stub1)
            }
            _ => {
                scene.world.set("XAI_API_KEY", KEY);
                ("xai", Some(format!("{}/v1/", stub3.uri())), &stub3)
            }
        };
        scene.world.set("XAI_BASE_URL", &format!("{env_base}/"));
        let provider = build(kind, configured.as_deref());
        assert_ok_turn(&turn(&provider, hi(), opts()).await.unwrap());
        for stub in [&stub1, &stub2, &stub3] {
            let expected = usize::from(std::ptr::eq(stub, target));
            assert_eq!(
                received(stub, RESPONSES).await.len(),
                expected,
                "{case}: {}",
                stub.uri()
            );
        }
    }
}

#[tokio::test]
#[serial]
async fn k08_every_header_literal_and_no_session_sends_neither_key_nor_header() {
    let server = MockServer::start().await;
    let _scene = scene(&server);
    let access = xai_access("login-sub");
    login(&server, &access).await;
    let provider = build("xai-oauth", None);
    mount_attempts(&server, vec![ok_sse()]).await;
    let long_session = "s".repeat(65);
    for session in [Some("conv-K08"), Some(long_session.as_str()), None] {
        assert_ok_turn(
            &turn(&provider, hi(), options(MODEL, 1024, session))
                .await
                .unwrap(),
        );
    }
    let sends = received(&server, RESPONSES).await;
    assert_eq!(sends.len(), 3);
    for (index, send) in sends.iter().enumerate() {
        for (name, value) in [
            ("content-type", "application/json".to_owned()),
            ("accept", "text/event-stream".to_owned()),
            ("user-agent", "omp/18.12.6".to_owned()),
            ("authorization", format!("Bearer {access}")),
            ("x-stainless-timeout", "300".to_owned()),
        ] {
            assert_eq!(header(send, name), Some(value), "send {index}: {name}");
        }
        for absent in [
            "x-xai-token-auth",
            "originator",
            "openai-beta",
            "chatgpt-account-id",
            "session_id",
            "x-client-request-id",
            "conversation_id",
            "version",
            "x-api-key",
        ] {
            assert!(
                header(send, absent).is_none(),
                "send {index} carries {absent}"
            );
        }
        let body: Value = send.body_json().unwrap();
        assert_eq!(
            header(send, "x-grok-conv-id").as_deref(),
            body["prompt_cache_key"].as_str(),
            "send {index}: header equals prompt_cache_key"
        );
    }
    let key = |index: usize| header(&sends[index], "x-grok-conv-id");
    assert_eq!(key(0).as_deref(), Some("conv-K08"));
    let hashed = key(1).unwrap();
    assert!(hashed.starts_with("pc_") && hashed.len() <= 64, "{hashed}");
    assert_eq!(key(2), None);
    let unsessioned: Value = sends[2].body_json().unwrap();
    assert!(unsessioned.get("prompt_cache_key").is_none());
}

// ── K09: transport resilience ───────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn k09_http_retry_policy_counts_hint_ceiling_admission_and_unretried_statuses() {
    let fail =
        |status: u16, message: &str| json_reply(status, json!({"error":{"message":message}}));
    let admission_body = || {
        ResponseTemplate::new(429).set_body_string(
            r#"{"error":{"message":"Too many concurrent requests"},"rate_limit_type":"max_parallel_requests"}"#,
        )
    };
    // (case, attempts, POSTs, expected outcome: Ok turn or the error's Debug)
    let cases: Vec<(&str, Vec<ResponseTemplate>, usize, Option<String>)> = vec![
        (
            "503 twice",
            vec![fail(503, "busy"), fail(503, "busy"), ok_sse()],
            3,
            None,
        ),
        (
            "500 always",
            vec![fail(500, "down")],
            6,
            Some(r#"ConnectionFailed("xAI server error 500: down")"#.into()),
        ),
        (
            "hint above the 60 s ceiling",
            vec![fail(429, "slow").insert_header("retry-after", "120")],
            1,
            Some("RateLimited { retry_after_ms: Some(120000) }".into()),
        ),
        (
            "hint at the ceiling",
            vec![
                fail(429, "slow").insert_header("retry-after", "60"),
                ok_sse(),
            ],
            2,
            None,
        ),
        (
            "429 without hint",
            vec![fail(429, "slow"), ok_sse()],
            2,
            None,
        ),
        (
            "admission body",
            vec![admission_body()],
            1,
            Some("RateLimited { retry_after_ms: None }".into()),
        ),
        (
            "admission header",
            vec![fail(429, "busy").insert_header("rate_limit_type", "max_parallel_requests")],
            1,
            Some("RateLimited { retry_after_ms: None }".into()),
        ),
        (
            "408 body-read timeout",
            vec![ResponseTemplate::new(408).set_body_string("Timed out reading request body")],
            1,
            Some(r#"Other("xAI HTTP 408: Timed out reading request body")"#.into()),
        ),
        ("408 retried", vec![fail(408, "timeout"), ok_sse()], 2, None),
        (
            "401",
            vec![fail(401, "bad key")],
            1,
            Some("AuthenticationFailed".into()),
        ),
        (
            "403",
            vec![fail(403, "Forbidden")],
            1,
            Some("AuthenticationFailed".into()),
        ),
        (
            "403 credit cap",
            vec![fail(403, "Your team has run out of credits")],
            1,
            Some(r#"Other("xAI HTTP 403: Your team has run out of credits")"#.into()),
        ),
        (
            "404",
            vec![fail(404, "no such model")],
            1,
            Some(r#"Other("xAI HTTP 404: no such model")"#.into()),
        ),
        (
            "bearer echoed",
            vec![fail(400, &format!("bad key {KEY}"))],
            1,
            Some(r#"Other("xAI HTTP 400: bad key [redacted]")"#.into()),
        ),
    ];
    for (case, attempts, posts, expected) in cases {
        let server = MockServer::start().await;
        let _scene = key_scene(&server);
        mount_attempts(&server, attempts).await;
        let provider = build("xai", None);
        let result = turn(&provider, hi(), opts()).await;
        match (&result, &expected) {
            (Ok(chunks), None) => assert_ok_turn(chunks),
            (Err(error), Some(debug)) => {
                assert_eq!(&format!("{error:?}"), debug, "{case}");
                assert!(!error.to_string().contains(KEY), "{case}");
            }
            _ => panic!("{case}: {result:?}"),
        }
        assert_eq!(received(&server, RESPONSES).await.len(), posts, "{case}");
    }
}

#[tokio::test]
#[serial]
async fn k09_stream_replay_empty_completion_and_generic_event_texts() {
    let created = || json!({"type":"response.created","response":{"status":"in_progress"}});
    let failed = |response: Value| json!({"type":"response.failed","response":response});
    // (case, attempts, POSTs, final error chunk or None for the ok turn)
    let cases: Vec<(&str, Vec<ResponseTemplate>, usize, Option<String>)> = vec![
        (
            "EOF before output",
            vec![sse(&[created()]), ok_sse()],
            2,
            None,
        ),
        (
            "EOF after a text delta",
            vec![sse(&[text("partial")])],
            1,
            Some(UNTERMINATED.into()),
        ),
        (
            "at most one replay",
            vec![sse(&[created()])],
            2,
            Some(UNTERMINATED.into()),
        ),
        (
            "EOF after an output item",
            vec![sse(&[reasoning("A")])],
            1,
            Some(UNTERMINATED.into()),
        ),
        (
            "transient error event",
            vec![
                sse(&[
                    json!({"type":"error","code":"server_error","message":"retry your request"}),
                ]),
                ok_sse(),
            ],
            2,
            None,
        ),
        (
            "content_filter",
            vec![sse(&[
                json!({"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"content_filter"}}}),
            ])],
            1,
            Some("incomplete: content_filter".into()),
        ),
        (
            "error event",
            vec![sse(&[
                json!({"type":"error","code":"invalid_prompt","message":"blocked"}),
            ])],
            1,
            Some("Error Code invalid_prompt: blocked".into()),
        ),
        (
            "bare error event",
            vec![sse(&[json!({"type":"error"})])],
            1,
            Some("Error Code unknown: no message".into()),
        ),
        (
            "failed with error",
            vec![sse(&[failed(
                json!({"status":"failed","error":{"code":"invalid_request","message":"bad"}}),
            )])],
            1,
            Some("invalid_request: bad".into()),
        ),
        (
            "failed incomplete",
            vec![sse(&[failed(
                json!({"status":"failed","incomplete_details":{"reason":"max_output_tokens"}}),
            )])],
            1,
            Some("incomplete: max_output_tokens".into()),
        ),
        (
            "failed without details is transient",
            vec![sse(&[failed(json!({"status":"failed"}))])],
            2,
            Some("Unknown error (no error details in response)".into()),
        ),
        (
            "bearer echoed in-band",
            vec![sse(&[
                json!({"type":"error","code":"bad","message":format!("key {KEY} leaked")}),
            ])],
            1,
            Some("Error Code bad: key [redacted] leaked".into()),
        ),
        (
            "one empty completion",
            vec![sse(&[empty_completion()]), ok_sse()],
            2,
            None,
        ),
    ];
    for (case, attempts, posts, expected) in cases {
        let server = MockServer::start().await;
        let _scene = key_scene(&server);
        mount_attempts(&server, attempts).await;
        let provider = build("xai", None);
        let chunks = turn(&provider, hi(), opts())
            .await
            .unwrap_or_else(|e| panic!("{case}: {e:?}"));
        match &expected {
            None => assert_ok_turn(&chunks),
            Some(message) => {
                assert_eq!(errors(&chunks), [message.as_str()], "{case}: {chunks:?}");
                assert!(
                    matches!(chunks.last(), Some(StreamChunk::Error { .. })),
                    "{case}: {chunks:?}"
                );
            }
        }
        if case == "EOF after a text delta" {
            assert_eq!(texts(&chunks), ["partial"]);
        }
        assert_eq!(received(&server, RESPONSES).await.len(), posts, "{case}");
    }
}

#[tokio::test]
#[serial]
async fn k09_empty_completions_retry_twice_and_discard_their_replay_items() {
    let server = MockServer::start().await;
    let _scene = key_scene(&server);
    mount_attempts(
        &server,
        ["A", "B", "C"]
            .map(|marker| sse(&[reasoning(marker), empty_completion()]))
            .to_vec(),
    )
    .await;
    let provider = build("xai", None);
    let chunks = turn(&provider, hi(), opts()).await.unwrap();
    assert_eq!(received(&server, RESPONSES).await.len(), 3);
    assert!(errors(&chunks).is_empty(), "{chunks:?}");
    assert!(texts(&chunks).is_empty());
    assert_eq!(stop(&chunks), Some(&StopReason::EndTurn));
    let kept = replays(&chunks);
    assert_eq!(
        kept.len(),
        1,
        "only the kept attempt's replay item: {chunks:?}"
    );
    assert_eq!(kept[0].provider, "xai");
    assert_eq!(kept[0].model, MODEL);
    assert_eq!(kept[0].payload["encrypted_content"], enc("C"));
}

#[tokio::test]
#[serial]
async fn usage_orchestration_is_subtracted_only_when_the_total_includes_it() {
    let usage = |total: u64| {
        json!({
            "input_tokens":100,"output_tokens":50,"total_tokens":total,
            "input_tokens_details":{"cached_tokens":10,"orchestration_input_tokens":30,"orchestration_input_cached_tokens":4},
            "output_tokens_details":{"orchestration_output_tokens":20,"reasoning_tokens":7}
        })
    };
    // (reported total, input, output, cache read)
    for (total, input, output, cached) in [(150, 64, 30, 6), (200, 90, 50, 10)] {
        let server = MockServer::start().await;
        let _scene = key_scene(&server);
        mount_attempts(
            &server,
            vec![sse(&[text("ok"), completed_with(usage(total))])],
        )
        .await;
        let chunks = turn(&build("xai", None), hi(), opts()).await.unwrap();
        assert_ok_turn(&chunks);
        let reported = chunks
            .iter()
            .find_map(|chunk| match chunk {
                StreamChunk::Usage { usage, .. } => Some(usage.clone()),
                _ => None,
            })
            .expect("usage chunk");
        assert_eq!(reported.input_tokens, input, "total {total}");
        assert_eq!(reported.output_tokens, output, "total {total}");
        assert_eq!(
            reported.cache_read_input_tokens,
            Some(cached),
            "total {total}"
        );
        assert_eq!(reported.cache_creation_input_tokens, None);
        assert_eq!(reported.reasoning_tokens, Some(7));
    }
}

// ── raw loopback HTTP: timing and connection lifetime ───────────────────────

async fn reply_raw(mut held: Held, status: &str, headers: &[(&str, &str)], body: &str) {
    let mut response = format!("HTTP/1.1 {status}\r\n");
    for (name, value) in headers {
        response.push_str(&format!("{name}: {value}\r\n"));
    }
    response.push_str(&format!(
        "content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    ));
    held.stream.write_all(response.as_bytes()).await.unwrap();
    held.stream.shutdown().await.ok();
}

#[tokio::test]
#[serial]
async fn first_event_watchdog_spans_slow_503_retries() {
    let server = MockServer::start().await;
    let scene = key_scene(&server);
    // Scale 0.01: watchdog 3 s; each 60 000 ms hint (at the ceiling, so retried) waits 600 ms.
    scene
        .world
        .set("RUSTAIN_TEST_RESPONSES_RETRY_SCALE", "0.01");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let provider = build("xai", Some(&base));
    let start = Instant::now();
    let task =
        tokio::spawn(async move { provider.stream_completion(hi(), opts()).await.map(|_| ()) });
    for _ in 0..4 {
        let held = accept(&listener).await;
        assert_eq!(held.path(), "/v1/responses");
        reply_raw(
            held,
            "503 Service Unavailable",
            &[("retry-after-ms", "60000")],
            "busy",
        )
        .await;
    }
    // The fifth attempt never answers: only the watchdog armed before attempt one ends it.
    let _held = accept(&listener).await;
    let result = tokio::time::timeout(GUARD, task)
        .await
        .expect("watchdog fired")
        .unwrap();
    let elapsed = start.elapsed();
    match result {
        Err(ProviderError::ConnectionFailed(message)) => assert_eq!(message, FIRST_EVENT),
        other => panic!("held attempt ended with {other:?}"),
    }
    assert!(
        elapsed >= Duration::from_millis(2_400),
        "four hinted backoffs ran: {elapsed:?}"
    );
    // A per-attempt timer would wait 2.4 s of backoff plus a full 3 s for attempt five.
    assert!(
        elapsed < Duration::from_millis(5_400),
        "the watchdog spans attempts: {elapsed:?}"
    );
    assert!(
        listener.accept().now_or_never().is_none(),
        "no attempt after the watchdog"
    );
}

#[tokio::test]
#[serial]
async fn generic_backoff_doubles_between_attempts() {
    let server = MockServer::start().await;
    let scene = key_scene(&server);
    // Scale 0.04: 20, 40, 80, 160, 320 ms (Codex's linear 500·n would be 300 ms total).
    scene
        .world
        .set("RUSTAIN_TEST_RESPONSES_RETRY_SCALE", "0.04");
    mount_attempts(
        &server,
        vec![json_reply(500, json!({"error":{"message":"down"}}))],
    )
    .await;
    let start = Instant::now();
    let result = turn(&build("xai", None), hi(), opts()).await;
    let elapsed = start.elapsed();
    assert!(matches!(result, Err(ProviderError::ConnectionFailed(_))));
    assert_eq!(received(&server, RESPONSES).await.len(), 6);
    assert!(
        elapsed >= Duration::from_millis(620),
        "exponential backoff: {elapsed:?}"
    );
}

#[tokio::test]
#[serial]
async fn dropping_the_stream_closes_the_connection_and_sends_nothing_more() {
    let server = MockServer::start().await;
    let _scene = key_scene(&server);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let provider = build("xai", Some(&base));
    let task = tokio::spawn(async move { provider.stream_completion(hi(), opts()).await });
    let mut held = accept(&listener).await;
    let event = format!("data: {}\n\n", text("partial"));
    held.stream
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{event}"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut stream = task.await.unwrap().expect("stream opened");
    let first = tokio::time::timeout(GUARD, stream.next())
        .await
        .expect("first chunk");
    assert!(
        matches!(&first, Some(StreamChunk::Text { content, .. }) if content == "partial"),
        "{first:?}"
    );
    drop(stream);
    // The client side closes: the stub reads EOF (or a reset), never a new request.
    let mut buffer = [0u8; 1024];
    let closed = tokio::time::timeout(GUARD, async {
        loop {
            match held.stream.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "the dropped stream closed its connection");
    assert!(
        listener.accept().now_or_never().is_none(),
        "no further request after the drop"
    );
}

#[tokio::test]
#[serial]
async fn generic_stall_after_non_progress_events_times_out_without_a_replay() {
    let server = MockServer::start().await;
    let scene = key_scene(&server);
    // Scale 0.001: first-event and idle timers of 300 ms.
    scene
        .world
        .set("RUSTAIN_TEST_RESPONSES_RETRY_SCALE", "0.001");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let provider = build("xai", Some(&base));
    let task = tokio::spawn(async move { provider.stream_completion(hi(), opts()).await });
    let mut held = accept(&listener).await;
    // `response.in_progress` is not an omp progress event: it is not the first
    // event and does not restart the timer.
    let event = format!("data: {}\n\n", json!({"type": "response.in_progress"}));
    held.stream
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{event}"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let stream = task.await.unwrap().expect("stream opened");
    let chunks = tokio::time::timeout(GUARD, stream.collect::<Vec<_>>())
        .await
        .expect("the watchdog ends the turn");
    assert!(
        matches!(chunks.last(), Some(StreamChunk::Error { content }) if content == FIRST_EVENT),
        "{chunks:?}"
    );
    assert!(
        listener.accept().now_or_never().is_none(),
        "a timed-out attempt is never replayed"
    );
    drop(held);
}

// ── probe / health ─────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn probe_and_health_map_models_statuses_per_port() {
    // (status, expected health/probe outcome Debug; "ok" for success)
    let cases = [
        (200, "ok"),
        (401, "AuthenticationFailed"),
        (403, "AuthenticationFailed"),
        (404, "EndpointUnsupported(404)"),
        (405, "EndpointUnsupported(405)"),
        (500, r#"Other("xAI models HTTP 500")"#),
    ];
    for (status, expected) in cases {
        let server = MockServer::start().await;
        let _scene = key_scene(&server);
        Mock::given(method("GET"))
            .and(path(MODELS))
            .respond_with(ResponseTemplate::new(status).set_body_string("{}"))
            .mount(&server)
            .await;
        let provider = build("xai", None);
        let health = provider.health_check().await;
        let probe = provider.connectivity_probe().await;
        let outcome = |result: Result<(), ProviderError>| match result {
            Ok(()) => "ok".to_owned(),
            Err(error) => format!("{error:?}"),
        };
        assert_eq!(outcome(health), expected, "health {status}");
        assert_eq!(outcome(probe.map(|_| ())), expected, "probe {status}");
        let gets = received(&server, MODELS).await;
        assert_eq!(gets.len(), 2, "status {status}: one GET per call, no retry");
        for get in &gets {
            assert_eq!(header(get, "accept").as_deref(), Some("application/json"));
            assert_eq!(header(get, "authorization"), Some(format!("Bearer {KEY}")));
        }
    }
    // Transport failure: health classifies the transport error, the probe is Offline.
    let server = MockServer::start().await;
    let _scene = key_scene(&server);
    let provider = build("xai", Some(&format!("{}/v1", unreachable_origin())));
    assert!(matches!(
        provider.health_check().await,
        Err(ProviderError::ConnectionFailed(_) | ProviderError::Offline(_))
    ));
    assert!(matches!(
        provider.connectivity_probe().await,
        Err(ProviderError::Offline(_))
    ));
}

#[tokio::test]
#[serial]
async fn health_recovers_a_stored_row_401_once() {
    let server = MockServer::start().await;
    let _scene = scene(&server);
    let initial = xai_access("login-sub");
    let next = xai_access("refreshed-sub");
    login(&server, &initial).await;
    let provider = build("xai-oauth", None);
    mount_refresh(&server, super::device::TOKEN_ENDPOINT, &next).await;
    mount_bearer_gate(
        &server,
        "GET",
        MODELS,
        &next,
        json_reply(200, json!({"data":[]})),
    )
    .await;
    let from = request_count(&server).await;
    provider.health_check().await.expect("recovered health");
    assert_eq!(
        requests_since(&server, from).await,
        [
            format!("GET {MODELS}"),
            format!("GET {OIDC_PATH}"),
            format!("POST {TOKEN_PATH}"),
            format!("GET {USERINFO_PATH}"),
            format!("GET {MODELS}"),
        ]
    );
}

#[tokio::test]
async fn adapter_debug_never_shows_the_credential() {
    for kind in [XaiKind::Xai, XaiKind::XaiOauth] {
        let handle =
            CredentialHandle::runtime(shared(), kind.id().into(), SecretString::new(KEY.into()));
        let adapter = XaiAdapter::new(handle, kind, Some("https://proxy.example/v1".into()))
            .expect("adapter");
        let debug = format!("{adapter:?}");
        assert!(debug.contains("XaiAdapter"), "{debug}");
        assert!(debug.contains("[redacted]"), "{debug}");
        assert!(!debug.contains(KEY), "{debug}");
        assert_eq!(adapter.provider_id(), kind.id());
    }
}

// ── `xai` API-key login ─────────────────────────────────────────────────────

struct KeyUi {
    key: &'static str,
    prompts: Mutex<Vec<String>>,
    progress: Mutex<Vec<String>>,
}
impl LoginUi for KeyUi {
    fn on_auth_url(&self, _: &str, _: &str, _: &str) {
        panic!("an API-key login never publishes an authorization URL");
    }
    fn on_device_code(&self, _: &str, _: &str, _: &str) {
        panic!("an API-key login never displays a device code");
    }
    fn on_progress(&self, message: &str) {
        self.progress.lock().push(message.into());
    }
    fn prompt_secret(&self, label: &str) -> BoxFuture<'_, Option<SecretString>> {
        self.prompts.lock().push(label.into());
        let key = self.key;
        Box::pin(async move { Some(SecretString::new(key.into())) })
    }
    fn manual_input(&self, _: &str) -> Option<BoxFuture<'static, Option<String>>> {
        None
    }
    fn prompt_line(&self, _: &str) -> BoxFuture<'_, Option<String>> {
        Box::pin(async { None })
    }
    fn cancel_token(&self) -> CancellationToken {
        CancellationToken::new()
    }
}
async fn key_login(
    key: &'static str,
) -> (Arc<KeyUi>, Result<flows::LoginResult, flows::FlowError>) {
    let ui = Arc::new(KeyUi {
        key,
        prompts: Mutex::new(Vec::new()),
        progress: Mutex::new(Vec::new()),
    });
    let store: Arc<dyn AuthStorePort> = Arc::new(FileAuthStore::new());
    let ctx = FlowCtx::new(ui.clone(), Arc::new(|| NOW));
    let result = flows::login("xai", &AuthMethod::ApiKey, &ctx, &store).await;
    (ui, result)
}

#[tokio::test]
#[serial]
async fn xai_key_login_probes_models_stores_a_login_row_and_serves_requests() {
    let server = MockServer::start().await;
    let _scene = scene(&server);
    mount_bearer_gate(
        &server,
        "GET",
        MODELS,
        KEY,
        json_reply(200, json!({"data":[]})),
    )
    .await;
    mount_bearer_gate(&server, "POST", RESPONSES, KEY, ok_sse()).await;

    let (ui, result) = key_login(KEY).await;
    let result = result.expect("validated key login");
    assert_eq!(result.provider, "xai");
    assert_eq!(result.method, "api_key");
    assert!(result.validated);
    assert_eq!(*ui.prompts.lock(), ["Enter xAI API key: "]);
    assert_eq!(*ui.progress.lock(), ["Validating key against xAI…"]);
    let probes = received(&server, MODELS).await;
    assert_eq!(probes.len(), 1);
    assert_eq!(
        header(&probes[0], "authorization"),
        Some(format!("Bearer {KEY}"))
    );
    assert_eq!(
        header(&probes[0], "accept").as_deref(),
        Some("application/json")
    );

    let rows = rows("xai").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].origin, CredentialOrigin::Login);
    assert!(rows[0].last_validated.is_some());
    match &rows[0].credential {
        Credential::ApiKey(key) => assert_eq!(key.expose_secret(), KEY),
        _ => panic!("an API-key login stores a key row"),
    }

    // The stored key serves `xai` inference without any environment variable.
    assert_ok_turn(&turn(&build("xai", None), hi(), opts()).await.unwrap());
    let sends = received(&server, RESPONSES).await;
    assert_eq!(
        header(&sends[0], "authorization"),
        Some(format!("Bearer {KEY}"))
    );
}

#[tokio::test]
#[serial]
async fn xai_key_login_refused_by_the_probe_stores_nothing() {
    let server = MockServer::start().await;
    let scene = scene(&server);
    Mock::given(method("GET"))
        .and(path(MODELS))
        .respond_with(json_reply(401, json!({"error":"invalid api key"})))
        .mount(&server)
        .await;
    let (_, result) = key_login("xai-WRONG").await;
    assert_eq!(
        result.expect_err("refused key").to_string(),
        "✗ Invalid API key for xAI.\n  Get one at: https://console.x.ai/team/default/api-keys"
    );
    assert_eq!(received(&server, MODELS).await.len(), 1);
    assert!(!scene.world.dir.path().join("auth.json").exists());
}
