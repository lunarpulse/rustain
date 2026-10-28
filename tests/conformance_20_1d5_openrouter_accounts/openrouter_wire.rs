//! K08–K10 (AC8–AC10): OpenRouter inference through `build_provider_for_config`
//! → `build_openrouter_from_config` → `OpenRouterAdapter::stream_completion`, at
//! a wiremock stub reached through the test origin (the default base rebased)
//! or a configured `base_url`.

use super::mint::{AUTH_KEY, PROVIDER, minted, mount_openrouter, rows};
use crate::conformance_20_1d1b_codex_replay_discovery::common::{
    NOW, World, completed, sse, text, tool_call,
};
use crate::conformance_20_1d3_copilot::chat_d4::KeyUi;
use crate::conformance_20_1d3_copilot::common::{
    assistant, chat_sse, complete, header, json_body, options, reply, requests, text_of, user,
    with_image,
};
use crate::conformance_20_1d4_gemini_cli::replay::Capture;
use parking_lot::Mutex;
use rustain::adapters::auth::flows::{self, FlowCtx};
use rustain::adapters::auth_store::FileAuthStore;
use rustain::domain::errors::ProviderError;
use rustain::domain::models::{
    AuthMethod, Message, ModelCapability, ModelDescriptor, ProviderConfig, StreamChunk,
};
use rustain::domain::ports::{AuthStorePort, StreamingProvider};
use rustain::infrastructure::provider_factory::build_provider_for_config;
use serde_json::{Value, json};
use serial_test::serial;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use wiremock::{
    Mock, MockServer, Request, Respond, ResponseTemplate,
    matchers::{method, path},
};

pub(crate) const RESPONSES: &str = "/api/v1/responses";
pub(crate) const CHAT: &str = "/api/v1/chat/completions";
pub(crate) const ENV_KEY: &str = "sk-or-v1-env-canary";
pub(crate) const SONNET: &str = "anthropic/claude-sonnet-4.6";
pub(crate) const GPT: &str = "openai/gpt-5.5";
const SWITCH: &str = "RUSTAIN_OPENROUTER_RESPONSES";

/// The production `[provider.openrouter]` entry; no `base_url`, so the default
/// base is used (rebased to the stub by the test origin).
pub(crate) fn config(model: &str) -> ProviderConfig {
    serde_json::from_value(json!({
        "provider_id": PROVIDER, "kind": PROVIDER, "model_id": model,
        "api_key_env": "OPENROUTER_API_KEY",
    }))
    .unwrap()
}

pub(crate) fn build(model: &str) -> Arc<dyn StreamingProvider> {
    build_provider_for_config(PROVIDER, &config(model)).expect("an OpenRouter entry builds")
}

/// Attempts answered in order; the last one repeats.
pub(crate) struct Sequence(Vec<ResponseTemplate>, AtomicUsize);
impl Respond for Sequence {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        let index = self.1.fetch_add(1, Ordering::SeqCst);
        self.0[index.min(self.0.len() - 1)].clone()
    }
}
pub(crate) async fn mount_sequence(
    server: &MockServer,
    route: &str,
    answers: Vec<ResponseTemplate>,
) {
    Mock::given(method("POST"))
        .and(path(route))
        .respond_with(Sequence(answers, AtomicUsize::new(0)))
        .mount(server)
        .await;
}

fn turn() -> Vec<Message> {
    vec![user("hi")]
}

/// Asserts every H10 literal on one inference request.
pub(crate) fn assert_attribution(request: &Request) {
    let version = env!("CARGO_PKG_VERSION");
    for (name, value) in [
        ("user-agent", format!("rustain/{version}")),
        (
            "http-referer",
            "https://github.com/lunarpulse/rustain".to_owned(),
        ),
        ("x-openrouter-title", "rustain".to_owned()),
        ("x-openrouter-categories", "cli-agent".to_owned()),
        ("x-openrouter-cache", "true".to_owned()),
        ("x-openrouter-cache-ttl", "3600".to_owned()),
        ("accept", "text/event-stream".to_owned()),
        ("content-type", "application/json".to_owned()),
    ] {
        assert_eq!(
            header(request, name).as_deref(),
            Some(value.as_str()),
            "{name}"
        );
    }
}

/// The byte offsets of `keys` in the raw body, which must be increasing.
fn assert_key_order(body: &[u8], keys: &[&str]) {
    let body = std::str::from_utf8(body).unwrap();
    let offsets: Vec<usize> = keys
        .iter()
        .map(|key| {
            body.find(&format!("\"{key}\":"))
                .unwrap_or_else(|| panic!("{key} missing from {body}"))
        })
        .collect();
    assert!(
        offsets.windows(2).all(|w| w[0] < w[1]),
        "{keys:?} in {body}"
    );
}

fn chunk_values(chunks: &[StreamChunk]) -> Value {
    serde_json::to_value(chunks).unwrap()
}

fn stream_events() -> Vec<Value> {
    vec![text("Hel"), text("lo"), tool_call("call-1"), completed()]
}

#[tokio::test]
#[serial]
async fn k08_responses_is_the_default_route_with_rustains_identity_and_omps_body() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    world.set("OPENROUTER_API_KEY", ENV_KEY);
    reply(&server, "POST", RESPONSES, sse(&stream_events())).await;

    let mut opts = options(SONNET, 8_192, Some("session-1"));
    opts.system_prompt = "be brief".into();
    let chunks = complete(&build(SONNET), turn(), opts).await.unwrap();

    let posts = requests(&server, "POST", RESPONSES).await;
    assert_eq!(posts.len(), 1);
    let request = &posts[0];
    assert_attribution(request);
    assert_eq!(
        header(request, "authorization").as_deref(),
        Some(format!("Bearer {ENV_KEY}").as_str())
    );
    assert_eq!(
        header(request, "x-stainless-timeout").as_deref(),
        Some("300")
    );
    // Generic key order, session ids normalised, the Anthropic breakpoint, and
    // no `max_output_tokens` although `max_tokens` is 8 192.
    assert_eq!(
        json_body(request),
        json!({
            "model": SONNET,
            "input": [{"role":"user","content":[{"type":"input_text","text":"hi"}]}],
            "instructions": "be brief",
            "stream": true,
            "prompt_cache_key": "session-1",
            "session_id": "session-1",
            "store": false,
            "cache_control": {"type": "ephemeral"},
        })
    );
    assert_key_order(
        &request.body,
        &[
            "model",
            "input",
            "instructions",
            "stream",
            "prompt_cache_key",
            "session_id",
            "store",
            "cache_control",
        ],
    );
    assert!(requests(&server, "POST", CHAT).await.is_empty());

    // The Generic mapper's chunks, exactly as xAI's for the same stream.
    assert_eq!(text_of(&chunks), "Hello");
    assert!(
        chunks
            .iter()
            .any(|c| matches!(c, StreamChunk::ToolUse { id, .. } if id == "call-1"))
    );
    world.set("XAI_API_KEY", "xai-key-canary");
    let xai: ProviderConfig = serde_json::from_value(json!({
        "provider_id": "xai", "kind": "xai", "model_id": "grok-4.6",
        "api_key_env": "XAI_API_KEY", "base_url": format!("{}/api/v1", server.uri()),
    }))
    .unwrap();
    let xai = build_provider_for_config("xai", &xai).unwrap();
    let xai_chunks = complete(&xai, turn(), options("grok-4.6", 8_192, None))
        .await
        .unwrap();
    assert_eq!(chunk_values(&chunks), chunk_values(&xai_chunks));

    // A non-Anthropic model gets no breakpoint; without a session no ids.
    complete(&build(GPT), turn(), options(GPT, 8_192, None))
        .await
        .unwrap();
    let posts = requests(&server, "POST", RESPONSES).await;
    let body = json_body(posts.last().unwrap());
    for key in [
        "cache_control",
        "session_id",
        "prompt_cache_key",
        "max_output_tokens",
        "include",
    ] {
        assert!(body.get(key).is_none(), "{key}: {body}");
    }
    assert_eq!(body["model"], GPT);
}

#[tokio::test]
#[serial]
async fn k08_a_routing_suffix_and_long_session_ids_are_sent_as_omp_does() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    world.set("OPENROUTER_API_KEY", ENV_KEY);
    reply(&server, "POST", RESPONSES, sse(&[text("ok"), completed()])).await;
    let session = "s".repeat(300);
    let model = "openai/gpt-5.5:nitro";
    complete(&build(model), turn(), options(model, 8_192, Some(&session)))
        .await
        .unwrap();
    let body = json_body(&requests(&server, "POST", RESPONSES).await[0]);
    assert_eq!(body["model"], model);
    assert!(
        body["prompt_cache_key"]
            .as_str()
            .unwrap()
            .starts_with("pc_")
    );
    assert!(body["session_id"].as_str().unwrap().starts_with("session_"));
}

#[tokio::test]
#[serial]
async fn k09_switch_zero_sends_chat_with_the_same_identity_and_omps_body() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    world.set("OPENROUTER_API_KEY", ENV_KEY);
    world.set(SWITCH, "0");
    reply(&server, "POST", CHAT, chat_sse("chat ok")).await;

    // A non-vision model: images become the placeholder part; the Anthropic
    // breakpoint lands on the last non-blank text part of the last user or
    // assistant message that has one, skipping a tool-call-only assistant turn.
    let mut call = assistant("");
    call.tool_uses
        .push(rustain::domain::models::ToolUseMessage {
            id: "c1".into(),
            name: "read".into(),
            input: json!({}),
        });
    let messages = vec![with_image(user("look")), call];
    let chunks = complete(&build(SONNET), messages, options(SONNET, 8_192, Some("s")))
        .await
        .unwrap();
    assert_eq!(text_of(&chunks), "chat ok");
    assert!(requests(&server, "POST", RESPONSES).await.is_empty());
    let posts = requests(&server, "POST", CHAT).await;
    assert_eq!(posts.len(), 1);
    assert_attribution(&posts[0]);
    assert_eq!(
        header(&posts[0], "authorization").as_deref(),
        Some(format!("Bearer {ENV_KEY}").as_str())
    );
    let body = json_body(&posts[0]);
    assert_eq!(
        body["messages"][0]["content"],
        json!([
            {"type":"text","text":"look"},
            {"type":"text","text":"[image omitted: model does not support vision]","cache_control":{"type":"ephemeral"}}
        ])
    );
    for key in ["max_tokens", "max_completion_tokens", "store"] {
        assert!(body.get(key).is_none(), "{key}: {body}");
    }
    assert_eq!(body["stream_options"], json!({"include_usage": true}));

    // A non-Anthropic model never gets a breakpoint.
    complete(&build(GPT), vec![user("plain")], options(GPT, 8_192, None))
        .await
        .unwrap();
    let posts = requests(&server, "POST", CHAT).await;
    assert_eq!(json_body(&posts[1])["messages"][0]["content"], "plain");
}

#[tokio::test]
#[serial]
async fn k09_a_responses_failure_never_falls_back_to_chat() {
    for answer in [
        ResponseTemplate::new(400).set_body_json(json!({"error": {"message": "bad request"}})),
        ResponseTemplate::new(500).set_body_json(json!({"error": {"message": "down"}})),
        // A stream that fails in band.
        sse(&[
            json!({"type":"response.failed","response":{"status":"failed","error":{"code":"server_error","message":"upstream failed"}}}),
        ]),
    ] {
        let server = MockServer::start().await;
        let world = World::new(&server);
        world.set("OPENROUTER_API_KEY", ENV_KEY);
        reply(&server, "POST", RESPONSES, answer).await;
        reply(&server, "POST", CHAT, chat_sse("never")).await;
        let outcome = complete(&build(GPT), turn(), options(GPT, 8_192, None)).await;
        let failed = match outcome {
            Err(_) => true,
            Ok(chunks) => chunks
                .iter()
                .any(|c| matches!(c, StreamChunk::Error { .. })),
        };
        assert!(failed);
        assert!(!requests(&server, "POST", RESPONSES).await.is_empty());
        assert!(requests(&server, "POST", CHAT).await.is_empty());
    }
}

#[tokio::test]
#[serial]
async fn k09_a_chat_attempt_that_ends_before_output_is_posted_again_once() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    world.set("OPENROUTER_API_KEY", ENV_KEY);
    world.set(SWITCH, "0");
    let empty = ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string("data: [DONE]\n\n");
    mount_sequence(&server, CHAT, vec![empty, chat_sse("second")]).await;
    let chunks = complete(&build(GPT), turn(), options(GPT, 8_192, None))
        .await
        .unwrap();
    assert_eq!(text_of(&chunks), "second");
    assert_eq!(requests(&server, "POST", CHAT).await.len(), 2);
}

/// omp `postOpenAIStream` → `fetchWithRetry`: the Chat route resends a 5xx and
/// a 429 open under the OpenAI-wire HTTP policy instead of failing the turn.
#[tokio::test]
#[serial]
async fn k09_chat_resends_a_transient_open_under_the_openai_wire_policy() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    world.set("OPENROUTER_API_KEY", ENV_KEY);
    world.set(SWITCH, "0");
    mount_sequence(
        &server,
        CHAT,
        vec![
            ResponseTemplate::new(503).set_body_json(json!({"error": {"message": "down"}})),
            ResponseTemplate::new(429).set_body_json(json!({"error": {"message": "slow down"}})),
            chat_sse("third"),
        ],
    )
    .await;
    let chunks = complete(&build(GPT), turn(), options(GPT, 8_192, None))
        .await
        .unwrap();
    assert_eq!(text_of(&chunks), "third");
    assert_eq!(requests(&server, "POST", CHAT).await.len(), 3);
}

/// AC9's vision half on both transports, also for an id with a routing suffix
/// (no catalogue row lists `:nitro`; omp reads the base model's traits): a
/// served `Thinking` + `Vision` row sends the image and requests encrypted
/// reasoning on Responses, and sends an image part on Chat.
#[tokio::test]
#[serial]
async fn k09_served_traits_reach_both_transports_also_through_a_routing_suffix() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    world.set("OPENROUTER_API_KEY", ENV_KEY);
    reply(&server, "POST", RESPONSES, sse(&[text("ok"), completed()])).await;
    reply(&server, "POST", CHAT, chat_sse("ok")).await;
    let row = || ModelDescriptor {
        model_id: GPT.into(),
        display_name: "GPT-5.5".into(),
        provider_id: PROVIDER.into(),
        context_window: 1_050_000,
        capabilities: [
            ModelCapability::ToolUse,
            ModelCapability::Thinking,
            ModelCapability::Vision,
        ]
        .into_iter()
        .collect(),
        pricing_tier: None,
        stale: false,
        wire_model_id: None,
    };
    let models = [GPT.to_owned(), format!("{GPT}:nitro")];
    for model in &models {
        let provider = build(model);
        provider.set_discovered_models(vec![row()]);
        complete(
            &provider,
            vec![with_image(user("look"))],
            options(model, 8_192, None),
        )
        .await
        .unwrap();
        let body = json_body(requests(&server, "POST", RESPONSES).await.last().unwrap());
        assert_eq!(body["model"], model.as_str());
        assert_eq!(
            body["include"],
            json!(["reasoning.encrypted_content"]),
            "{model}"
        );
        assert!(
            body.to_string().contains("\"type\":\"input_image\""),
            "{model}: {body}"
        );
    }
    world.set(SWITCH, "0");
    for model in &models {
        let provider = build(model);
        provider.set_discovered_models(vec![row()]);
        complete(
            &provider,
            vec![with_image(user("look"))],
            options(model, 8_192, None),
        )
        .await
        .unwrap();
        let body = json_body(requests(&server, "POST", CHAT).await.last().unwrap());
        let parts = body["messages"][0]["content"].as_array().unwrap();
        assert!(
            parts.iter().any(|part| part["type"] == "image_url"),
            "{model}: {body}"
        );
        assert!(
            !body.to_string().contains("image omitted"),
            "{model}: {body}"
        );
    }
}

/// A stored key row written by the real API-key login (validated at `/auth/key`).
async fn stored_key(key: &str) {
    let ui = Arc::new(KeyUi {
        key: key.into(),
        prompts: Mutex::new(Vec::new()),
    });
    let ctx = FlowCtx::new(ui, Arc::new(|| NOW));
    let store: Arc<dyn AuthStorePort> = Arc::new(FileAuthStore::new());
    flows::login(PROVIDER, &AuthMethod::ApiKey, &ctx, &store)
        .await
        .expect("the key login succeeds");
}

#[tokio::test]
#[serial]
async fn k10_errors_are_openrouter_branded_and_never_carry_the_bearer() {
    const KEY: &str = "sk-or-v1-stored-error-canary";
    let server = MockServer::start().await;
    let world = World::new(&server);
    mount_openrouter(&server, minted("unused")).await;
    stored_key(KEY).await;
    assert_eq!(rows().await.len(), 1);
    let capture = Capture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .finish();
    // Thread-scoped: the current-thread runtime runs every task of this test.
    let trace_guard = tracing::subscriber::set_default(subscriber);
    let auth_json = world.dir.path().join("auth.json");
    let mut errors = Vec::new();

    // 402 with upstream raw text: message, newline, the trimmed raw.
    server.reset().await;
    reply(
        &server,
        "POST",
        RESPONSES,
        ResponseTemplate::new(402).set_body_json(json!({"error": {
            "message": format!("Insufficient credits for {KEY}"),
            "metadata": {"raw": format!("  upstream echoed {KEY}  ")},
        }})),
    )
    .await;
    let error = complete(&build(GPT), turn(), options(GPT, 8_192, None))
        .await
        .unwrap_err();
    let ProviderError::Other(text) = &error else {
        panic!("402 is Other: {error:?}");
    };
    assert_eq!(
        text,
        "OpenRouter HTTP 402: Insufficient credits for [redacted]\nupstream echoed [redacted]"
    );
    errors.push(error.to_string());

    // 401: one request, no store write, `AuthenticationFailed`.
    server.reset().await;
    reply(
        &server,
        "POST",
        RESPONSES,
        ResponseTemplate::new(401)
            .set_body_json(json!({"error": {"message": format!("bad {KEY}")}})),
    )
    .await;
    let before = std::fs::read(&auth_json).unwrap();
    let error = complete(&build(GPT), turn(), options(GPT, 8_192, None))
        .await
        .unwrap_err();
    assert!(
        matches!(error, ProviderError::AuthenticationFailed),
        "{error:?}"
    );
    assert_eq!(requests(&server, "POST", RESPONSES).await.len(), 1);
    assert_eq!(std::fs::read(&auth_json).unwrap(), before);
    errors.push(error.to_string());

    // 429 with `retry-after`, after the Generic retries.
    server.reset().await;
    reply(
        &server,
        "POST",
        RESPONSES,
        ResponseTemplate::new(429)
            .insert_header("retry-after", "2")
            .set_body_json(json!({"error": {"message": format!("slow down {KEY}")}})),
    )
    .await;
    let error = complete(&build(GPT), turn(), options(GPT, 8_192, None))
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            ProviderError::RateLimited {
                retry_after_ms: Some(2_000)
            }
        ),
        "{error:?}"
    );
    assert!(!requests(&server, "POST", RESPONSES).await.is_empty());
    errors.push(error.to_string());

    drop(trace_guard);
    let log = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    for text in errors.iter().chain(std::iter::once(&log)) {
        assert!(!text.contains(KEY), "the bearer leaked: {text}");
    }
    assert!(requests(&server, "GET", AUTH_KEY).await.is_empty());
}
