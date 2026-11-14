//! cp-free-2: Copilot's Chat route decodes the chunks GitHub really streams
//! (no `object` field), and a 2xx that is not a usable stream says what it
//! was. Every case enters through `build_provider_for_config` →
//! `stream_completion` at a loopback stub and counts the POSTs the stub saw.
//! Each test uses its own token: Copilot's learned identity is process-global
//! per token.

use super::common::{
    self, ENDPOINT_A, Held, ROOT_A, World, accept, build, chat_path, chat_sse, complete, options,
    requests, text_of, user,
};
use super::identity::{data, errors, json_error, script, sse};
use futures::FutureExt;
use rustain::adapters::github_copilot::plan;
use rustain::domain::errors::ProviderError;
use rustain::domain::models::{StopReason, StreamChunk};
use serde_json::{Value, json};
use serial_test::serial;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const OWNER_MODEL: &str = "gpt-4.1";
const CHAT_MODEL: &str = "grok-code-fast-1";
const RAW_ENDPOINT: &str = "https://api.individual.githubcopilot.com/c";
const RAW_CHAT: &str = "/c/chat/completions";

/// The four lines of a real Copilot `gpt-4.1` answer (the owner's v0.4.0 log,
/// 2026-11-14), verbatim: no top-level `object` anywhere.
pub(crate) const OWNER_LINES: [&str; 4] = [
    r#"{"choices":[],"created":0,"id":"","model":"gpt-4.1-2025-04-14","prompt_filter_results":[{"content_filter_results":{"hate":{"filtered":false,"severity":"safe"},"self_harm":{"filtered":false,"severity":"safe"},"sexual":{"filtered":false,"severity":"safe"},"violence":{"filtered":false,"severity":"safe"}},"prompt_index":0}]}"#,
    r#"{"choices":[{"index":0,"content_filter_results":{},"delta":{"content":"","role":"assistant"}}],"created":1794649681,"id":"chatcmpl-Ek4eXEDaj9CRNMPUZQUFQK9SoiBw5","model":"gpt-4.1-2025-04-14","system_fingerprint":"fp_8c4c3e6d7a"}"#,
    r#"{"choices":[{"index":0,"content_filter_results":{},"delta":{"content":"Hello"}}],"created":1794649681,"id":"chatcmpl-Ek4eXEDaj9CRNMPUZQUFQK9SoiBw5","model":"gpt-4.1-2025-04-14","system_fingerprint":"fp_8c4c3e6d7a"}"#,
    r#"{"choices":[{"finish_reason":"stop","index":0,"content_filter_results":{},"delta":{"content":null}}],"created":1794649681,"id":"chatcmpl-Ek4eXEDaj9CRNMPUZQUFQK9SoiBw5","usage":{"completion_tokens":10,"completion_tokens_details":{"accepted_prediction_tokens":0,"rejected_prediction_tokens":0},"prompt_tokens":1549,"prompt_tokens_details":{"cached_tokens":1408},"total_tokens":1559},"model":"gpt-4.1-2025-04-14","system_fingerprint":"fp_8c4c3e6d7a","copilot_usage":{"token_details":[{"batch_size":1000000,"cost_per_batch":0,"model":"gpt-4.1","token_count":141,"token_type":"input"},{"batch_size":1000000,"cost_per_batch":0,"model":"gpt-4.1","token_count":1408,"token_type":"cache_read"},{"batch_size":1000000,"cost_per_batch":0,"model":"gpt-4.1","token_count":0,"token_type":"cache_write"},{"batch_size":1000000,"cost_per_batch":0,"model":"gpt-4.1","token_count":10,"token_type":"output"}],"total_nano_aiu":0}}"#,
];

// ── fixtures ────────────────────────────────────────────────────────────────

fn owner_frames(done: bool) -> Vec<String> {
    let mut frames: Vec<String> = OWNER_LINES
        .iter()
        .map(|line| format!("data: {line}\n\n"))
        .collect();
    if done {
        frames.push("data: [DONE]\n\n".into());
    }
    frames
}

fn content(text: &str) -> String {
    data(
        json!({"object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":text},"finish_reason":null}]}),
    )
}

fn finish_json() -> Value {
    json!({"object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})
}

fn error_event(message: &str, code: &str) -> String {
    data(json!({"error":{"message":message,"code":code}}))
}

fn terminals(chunks: &[StreamChunk]) -> Vec<StopReason> {
    chunks
        .iter()
        .filter_map(|chunk| match chunk {
            StreamChunk::TurnComplete { stop_reason } => Some(stop_reason.clone()),
            _ => None,
        })
        .collect()
}

fn usages(chunks: &[StreamChunk]) -> Vec<(u32, u32, Option<u32>)> {
    chunks
        .iter()
        .filter_map(|chunk| match chunk {
            StreamChunk::Usage { usage, .. } => Some((
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_read_input_tokens,
            )),
            _ => None,
        })
        .collect()
}

async fn turn(
    provider: &Arc<dyn rustain::domain::ports::StreamingProvider>,
    model: &str,
) -> Result<Vec<StreamChunk>, ProviderError> {
    complete(provider, vec![user("hi")], options(model, 256, None)).await
}

/// A Copilot Chat stub: login, the provider, and the route's POST path.
struct Stub {
    server: MockServer,
    _world: World,
    provider: Arc<dyn rustain::domain::ports::StreamingProvider>,
    route: String,
    model: &'static str,
}

impl Stub {
    async fn new(model: &'static str, token: &str) -> Self {
        let server = MockServer::start().await;
        let world = World::new(&server);
        common::login(&server, token, ENDPOINT_A).await;
        let provider = build(model);
        Self {
            server,
            _world: world,
            provider,
            route: chat_path(ROOT_A),
            model,
        }
    }

    /// One completion whose POSTs are answered `replies` in order: the open
    /// result and the number of POSTs.
    async fn turn(
        &self,
        replies: Vec<ResponseTemplate>,
    ) -> (Result<Vec<StreamChunk>, ProviderError>, usize) {
        self.server.reset().await;
        script(&self.server, &self.route, replies).await;
        let result = turn(&self.provider, self.model).await;
        let posts = requests(&self.server, "POST", &self.route).await.len();
        (result, posts)
    }

    /// Like `turn`, for a stream that must open.
    async fn stream(&self, replies: Vec<ResponseTemplate>) -> (Vec<StreamChunk>, usize) {
        let (result, posts) = self.turn(replies).await;
        (result.expect("the request opens"), posts)
    }
}

// ── AC1: the owner's stream ─────────────────────────────────────────────────

/// AC1/K1: the owner's four lines through the real Chat route, with and
/// without `[DONE]`, answered on EVERY POST: one POST, `Hello`, one terminal,
/// one usage.
#[tokio::test]
#[serial]
async fn ac1_the_owners_copilot_stream_decodes() {
    let stub = Stub::new(OWNER_MODEL, "cop-diag-ac1-owner").await;
    for done in [true, false] {
        stub.server.reset().await;
        Mock::given(method("POST"))
            .and(path(stub.route.clone()))
            .respond_with(sse(&owner_frames(done)))
            .mount(&stub.server)
            .await;
        let chunks = turn(&stub.provider, OWNER_MODEL)
            .await
            .expect("the request opens");
        let posts = requests(&stub.server, "POST", &stub.route).await.len();
        let what = format!("done={done}, {posts} POSTs: {chunks:?}");
        assert_eq!(posts, 1, "{what}");
        assert_eq!(text_of(&chunks), "Hello", "{what}");
        assert_eq!(terminals(&chunks), [StopReason::EndTurn], "{what}");
        assert_eq!(usages(&chunks), [(1549, 10, Some(1408))], "{what}");
        assert!(errors(&chunks).is_empty(), "{what}");
    }
}

// ── AC3: a 2xx without a stream event ───────────────────────────────────────

fn no_content_type() -> ResponseTemplate {
    ResponseTemplate::new(200)
}

fn event_stream_empty() -> ResponseTemplate {
    ResponseTemplate::new(200).insert_header("content-type", "text/event-stream")
}

fn assert_clean(text: &str, token: &str) {
    assert!(!text.contains(token), "{text}");
    assert!(!text.contains("gho_"), "{text}");
    assert!(!text.chars().any(char::is_control), "{text:?}");
}

/// The error text of a zero-event answer and the POST count.
async fn zero_event(stub: &Stub, case: ResponseTemplate) -> (String, usize) {
    let (result, posts) = stub.turn(vec![case, chat_sse("never")]).await;
    match result {
        Err(ProviderError::Other(text)) => (text, posts),
        other => panic!("a zero-event 2xx is a final error, got {other:?} after {posts} POSTs"),
    }
}

/// AC3/K3: every zero-event shape is one POST and a final error that carries
/// the status, the content type and GitHub's own words (or a redacted, bounded
/// body prefix), never the bearer.
#[tokio::test]
#[serial]
async fn ac3_a_zero_event_2xx_is_a_final_error_with_what_was_sent() {
    let token = "cop-diag-ac3-zero-event";
    let stub = Stub::new(CHAT_MODEL, token).await;

    let (empty, posts) = zero_event(&stub, event_stream_empty()).await;
    assert_eq!(posts, 1, "{empty}");
    assert!(empty.contains("200"), "{empty}");
    assert!(empty.contains("text/event-stream"), "{empty}");
    assert_clean(&empty, token);
    let fixed = empty.chars().count();

    let (bare, posts) = zero_event(&stub, no_content_type()).await;
    assert_eq!(posts, 1, "{bare}");
    assert!(bare.contains("200"), "{bare}");
    assert!(!bare.contains("text/"), "{bare}");
    assert_clean(&bare, token);

    let message = "quota words from GitHub";
    let code = "copilot_zz_code";
    let (json_text, posts) = zero_event(
        &stub,
        ResponseTemplate::new(200).set_body_json(
            json!({"error":{"message":format!("{message} Bearer {token}"),"code":code}}),
        ),
    )
    .await;
    assert_eq!(posts, 1, "{json_text}");
    for needle in ["200", "application/json", message, code] {
        assert!(json_text.contains(needle), "{needle}: {json_text}");
    }
    assert_clean(&json_text, token);

    let mut page = format!(
        "<!DOCTYPE html><title>Blocked</title>\u{1b}]0;evil\u{7} Bearer {token} {token} gho_abcdefghijklmnop "
    );
    page.push_str(&"x".repeat(5 * 1024));
    let (html, posts) = zero_event(
        &stub,
        ResponseTemplate::new(200).set_body_raw(page.into_bytes(), "text/html"),
    )
    .await;
    assert_eq!(posts, 1, "{html}");
    for needle in ["200", "text/html", "Blocked"] {
        assert!(html.contains(needle), "{needle}: {html}");
    }
    assert_clean(&html, token);
    assert!(
        html.chars().count() <= 500 + fixed,
        "{} characters",
        html.chars().count()
    );

    // Positive control: a frame-bearing empty completion keeps its re-issue rule.
    let (chunks, posts) = stub
        .stream(vec![sse(&["data: [DONE]\n\n".to_owned()]), chat_sse("ok")])
        .await;
    assert_eq!(text_of(&chunks), "ok");
    assert_eq!(posts, 2);
}

/// AC3: a provider with a static Chat profile (no resilience) reports a
/// zero-event 2xx the same way, after one POST.
#[tokio::test]
#[serial]
async fn ac3_a_static_profile_reports_a_zero_event_2xx_too() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    world.set("STUBAI_API_KEY", "stubai-diag-ac3-secret");
    let config: rustain::domain::models::ProviderConfig = serde_json::from_value(json!({
        "provider_id": "stubai",
        "kind": "openai",
        "model_id": "stub-model",
        "api_key_env": "STUBAI_API_KEY",
        "base_url": format!("{}/v1", server.uri()),
    }))
    .unwrap();
    let provider =
        rustain::infrastructure::provider_factory::build_provider_for_config("stubai", &config)
            .expect("the entry builds");
    script(
        &server,
        "/v1/chat/completions",
        vec![event_stream_empty(), chat_sse("never")],
    )
    .await;
    let result = turn(&provider, "stub-model").await;
    let posts = requests(&server, "POST", "/v1/chat/completions")
        .await
        .len();
    match result {
        Err(ProviderError::Other(text)) => {
            assert!(text.contains("200"), "{text}");
            assert_clean(&text, "stubai-diag-ac3-secret");
        }
        other => panic!("expected a final error, got {other:?}"),
    }
    assert_eq!(posts, 1);
}

// ── AC4: the last unterminated event ────────────────────────────────────────

/// AC4/K4: a final `data:` line without its blank line is still an event.
#[tokio::test]
#[serial]
async fn ac4_the_last_unterminated_event_is_kept() {
    let stub = Stub::new(CHAT_MODEL, "cop-diag-ac4-unterminated").await;
    let body = [content("hi"), format!("data: {}", finish_json())];
    let (chunks, posts) = stub.stream(vec![sse(&body)]).await;
    assert_eq!(text_of(&chunks), "hi", "{chunks:?}");
    assert_eq!(terminals(&chunks), [StopReason::EndTurn], "{chunks:?}");
    assert!(errors(&chunks).is_empty(), "{chunks:?}");
    assert_eq!(posts, 1);
}

// ── AC5: error events inside the stream ─────────────────────────────────────

/// AC5/K5: an error event is one `Error` with GitHub's code and message; a
/// transient message still spends the CHAT budget; an error after output is
/// final; `event: error` and `error`-plus-`choices` payloads are errors; a
/// chunk that merely fails the chunk schema is not.
#[tokio::test]
#[serial]
async fn ac5_error_events_become_errors() {
    let stub = Stub::new(CHAT_MODEL, "cop-diag-ac5-error-events").await;
    let (message, code) = ("request rejected", "zz_rejected");

    // (i) the stream opens, one Error with both parts, nothing else.
    let (chunks, posts) = stub
        .stream(vec![sse(&[error_event(message, code)]), chat_sse("never")])
        .await;
    let texts = errors(&chunks);
    assert_eq!(texts.len(), 1, "{chunks:?}");
    assert!(
        texts[0].contains(message) && texts[0].contains(code),
        "{texts:?}"
    );
    assert!(terminals(&chunks).is_empty(), "{chunks:?}");
    assert!(text_of(&chunks).is_empty(), "{chunks:?}");
    assert_eq!(posts, 1);

    // (ii) a transient message is re-issued once (the CHAT budget).
    let (chunks, posts) = stub
        .stream(vec![
            sse(&[error_event("Server overloaded", "zz_busy")]),
            chat_sse("ok"),
        ])
        .await;
    assert_eq!(text_of(&chunks), "ok", "{chunks:?}");
    assert!(errors(&chunks).is_empty(), "{chunks:?}");
    assert_eq!(posts, 2);

    // (iii) after output the error is final.
    let (chunks, posts) = stub
        .stream(vec![
            sse(&[content("partial"), error_event(message, code)]),
            chat_sse("never"),
        ])
        .await;
    assert_eq!(text_of(&chunks), "partial", "{chunks:?}");
    assert_eq!(errors(&chunks).len(), 1, "{chunks:?}");
    assert_eq!(posts, 1);

    // (iv) `event: error` with plain data.
    let (chunks, posts) = stub
        .stream(vec![
            sse(&["event: error\ndata: plain words\n\n".to_owned()]),
            chat_sse("never"),
        ])
        .await;
    let texts = errors(&chunks);
    assert_eq!(texts.len(), 1, "{chunks:?}");
    assert!(texts[0].contains("plain words"), "{texts:?}");
    assert_eq!(posts, 1);

    // (vi) guard: Copilot's filter-annotation chunk and a choice-less chunk
    // are not errors.
    let mut frames = vec![
        format!("data: {}\n\n", OWNER_LINES[0]),
        data(json!({"id":"x","model":"m"})),
    ];
    frames.extend(
        OWNER_LINES[1..]
            .iter()
            .map(|line| format!("data: {line}\n\n")),
    );
    let (chunks, posts) = stub.stream(vec![sse(&frames)]).await;
    assert_eq!(text_of(&chunks), "Hello", "{chunks:?}");
    assert_eq!(terminals(&chunks), [StopReason::EndTurn], "{chunks:?}");
    assert!(errors(&chunks).is_empty(), "{chunks:?}");
    assert_eq!(posts, 1);

    // (vii) the error wins over `choices` in the same payload.
    let both = data(json!({
        "error": {"message": message, "code": code},
        "choices": [{"index":0,"delta":{"content":"x"},"finish_reason":"stop"}],
    }));
    let (chunks, posts) = stub.stream(vec![sse(&[both])]).await;
    assert_eq!(errors(&chunks).len(), 1, "{chunks:?}");
    assert!(text_of(&chunks).is_empty(), "{chunks:?}");
    assert!(terminals(&chunks).is_empty(), "{chunks:?}");
    assert_eq!(posts, 1);
}

/// AC5 (v): a refusal inside the stream carries GitHub's code and message and
/// the plan text, and never an `HTTP 400`.
#[tokio::test]
#[serial]
async fn ac5_a_refused_model_inside_the_stream_gets_the_plan_text() {
    let model = "gemini-3.8-flash";
    let stub = Stub::new(model, "cop-diag-ac5-refused").await;
    let (chunks, posts) = stub
        .stream(vec![
            sse(&[error_event(
                "The requested model is not supported.",
                "model_not_supported",
            )]),
            chat_sse("never"),
        ])
        .await;
    let texts = errors(&chunks);
    assert_eq!(texts.len(), 1, "{chunks:?}");
    assert!(
        texts[0].contains("The requested model is not supported."),
        "{texts:?}"
    );
    assert!(texts[0].contains("model_not_supported"), "{texts:?}");
    assert!(texts[0].ends_with(&plan::refusal_tail(model)), "{texts:?}");
    assert!(!texts[0].contains("HTTP 400"), "{texts:?}");
    assert_eq!(posts, 1);
}

// ── patch round 1 ───────────────────────────────────────────────────────────

fn tool_chunk() -> String {
    data(
        json!({"object":"chat.completion.chunk","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"Bash","arguments":"{}"}}]},"finish_reason":null}]}),
    )
}

fn tool_uses(chunks: &[StreamChunk]) -> usize {
    chunks
        .iter()
        .filter(|chunk| matches!(chunk, StreamChunk::ToolUse { .. }))
        .count()
}

/// CP2-R2: tool-call delta, error, `[DONE]` is the Error alone on the
/// resilient Copilot route and on a static (non-resilient) route: the tool is
/// never emitted, and one POST is made.
#[tokio::test]
#[serial]
async fn cp2_r2_an_error_ends_the_turn_on_resilient_and_static_routes() {
    let frames = [
        tool_chunk(),
        error_event("request rejected", "zz_rejected"),
        "data: [DONE]\n\n".to_owned(),
    ];
    let stub = Stub::new(CHAT_MODEL, "cop-diag-r2-terminal").await;
    let (chunks, posts) = stub.stream(vec![sse(&frames), chat_sse("never")]).await;
    assert_eq!(errors(&chunks).len(), 1, "{chunks:?}");
    assert_eq!(tool_uses(&chunks), 0, "{chunks:?}");
    assert!(terminals(&chunks).is_empty(), "{chunks:?}");
    assert_eq!(posts, 1);

    let server = MockServer::start().await;
    let world = World::new(&server);
    world.set("STUBAI_API_KEY", "stubai-diag-r2-secret");
    let config: rustain::domain::models::ProviderConfig = serde_json::from_value(json!({
        "provider_id": "stubai",
        "kind": "openai",
        "model_id": "stub-model",
        "api_key_env": "STUBAI_API_KEY",
        "base_url": format!("{}/v1", server.uri()),
    }))
    .unwrap();
    let provider =
        rustain::infrastructure::provider_factory::build_provider_for_config("stubai", &config)
            .expect("the entry builds");
    script(
        &server,
        "/v1/chat/completions",
        vec![sse(&frames), chat_sse("never")],
    )
    .await;
    let chunks = turn(&provider, "stub-model")
        .await
        .expect("the stream opens");
    assert_eq!(errors(&chunks).len(), 1, "{chunks:?}");
    assert_eq!(tool_uses(&chunks), 0, "{chunks:?}");
    assert!(terminals(&chunks).is_empty(), "{chunks:?}");
    assert!(text_of(&chunks).is_empty(), "{chunks:?}");
    let posts = requests(&server, "POST", "/v1/chat/completions")
        .await
        .len();
    assert_eq!(posts, 1);
}

/// CP2-R5: an error key spelled with a JSON escape is still the error, also
/// beside valid `choices`; one POST.
#[tokio::test]
#[serial]
async fn cp2_r5_an_escaped_error_key_is_an_error() {
    let stub = Stub::new(CHAT_MODEL, "cop-diag-r5-escaped").await;
    let escaped = "data: {\"err\\u006fr\":{\"message\":\"request rejected\",\"code\":\"zz\"}}\n\n";
    let both = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"x\"},\"finish_reason\":\"stop\"}],\"\\u0065rror\":{\"message\":\"request rejected\"}}\n\n";
    for frame in [escaped, both] {
        let (chunks, posts) = stub
            .stream(vec![sse(&[frame.to_owned()]), chat_sse("never")])
            .await;
        let texts = errors(&chunks);
        assert_eq!(texts.len(), 1, "{chunks:?}");
        assert!(texts[0].contains("request rejected"), "{texts:?}");
        assert!(text_of(&chunks).is_empty(), "{chunks:?}");
        assert!(terminals(&chunks).is_empty(), "{chunks:?}");
        assert_eq!(posts, 1, "{frame}");
    }
}

/// CP2-R1/R6: a credential split by an escape sequence is removed after
/// normalising (zero-event body and in-stream error), and a long code plus a
/// long message share one bound.
#[tokio::test]
#[serial]
async fn cp2_r1_r6_the_seam_normalises_before_redacting_and_bounds_the_detail_once() {
    let token = "cop-diag-r1-sent-bearer";
    let stub = Stub::new(CHAT_MODEL, token).await;
    let split = format!("{}\u{1b}[0m{}", &token[..8], &token[8..]);
    let body = json!({"error":{"code":format!("c {split}"),"message":format!("m {split}")}});
    let (zero, posts) = zero_event(&stub, ResponseTemplate::new(200).set_body_json(&body)).await;
    assert_eq!(posts, 1);
    assert_clean(&zero, token);
    assert!(!zero.contains(&token[..8]), "{zero}");

    let (chunks, _) = stub
        .stream(vec![sse(&[data(body.clone())]), chat_sse("never")])
        .await;
    let texts = errors(&chunks);
    assert_eq!(texts.len(), 1, "{chunks:?}");
    assert_clean(&texts[0], token);
    assert!(!texts[0].contains(&token[..8]), "{texts:?}");

    let (empty, _) = zero_event(&stub, event_stream_empty()).await;
    let long = json!({"error":{"code":"c".repeat(600),"message":"m".repeat(600)}});
    let (text, _) = zero_event(&stub, ResponseTemplate::new(200).set_body_json(&long)).await;
    assert!(
        text.chars().count() <= empty.chars().count() + 500,
        "{} characters",
        text.chars().count()
    );
}

// ── AC6: retry controls and the watchdog ────────────────────────────────────

/// AC6: an HTTP 503 and a 429 with a hint are still re-sent once.
#[tokio::test]
#[serial]
async fn ac6_open_failures_are_still_resent() {
    let stub = Stub::new(CHAT_MODEL, "cop-diag-ac6-resend").await;
    let (chunks, posts) = stub
        .stream(vec![
            json_error(503, json!({"error":{"message":"unavailable"}})),
            chat_sse("ok"),
        ])
        .await;
    assert_eq!(text_of(&chunks), "ok", "{chunks:?}");
    assert_eq!(posts, 2);
    let (chunks, posts) = stub
        .stream(vec![
            json_error(429, json!({"error":{"message":"slow down"}}))
                .insert_header("retry-after", "1"),
            chat_sse("ok"),
        ])
        .await;
    assert_eq!(text_of(&chunks), "ok", "{chunks:?}");
    assert_eq!(posts, 2);
}

const HEADERS_ONLY: &[u8] =
    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";

/// A raw TCP Copilot Chat route (the test writes every response).
fn raw_provider(
    world: &World,
    origin: &str,
    token: &str,
) -> Arc<dyn rustain::domain::ports::StreamingProvider> {
    world.set("RUSTAIN_TEST_AUTH_ORIGIN", origin);
    world.set(
        "COPILOT_GITHUB_TOKEN",
        &json!({"token": token, "apiEndpoint": RAW_ENDPOINT}).to_string(),
    );
    build(CHAT_MODEL)
}

/// AC6/K3: the body never starts: the open is inside the first-event window
/// and ends as the watchdog's `ConnectionFailed`, after exactly one POST.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn ac6_a_stalled_open_ends_at_the_first_event_window() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    // 300 ms first-event window.
    world.set("RUSTAIN_TEST_RESPONSES_RETRY_SCALE", "0.001");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let provider = raw_provider(&world, &origin, "cop-diag-ac6-stalled");
    let call = tokio::spawn(async move { turn(&provider, CHAT_MODEL).await });

    let mut held: Held = accept(&listener).await;
    assert_eq!(held.path(), RAW_CHAT);
    held.stream.write_all(HEADERS_ONLY).await.unwrap();
    let result = call.await.unwrap();
    assert!(
        matches!(result, Err(ProviderError::ConnectionFailed(_))),
        "{result:?}"
    );
    assert!(
        listener.accept().now_or_never().is_none(),
        "exactly one POST"
    );
    drop(held);
}

/// AC6: an abort while the open waits for the body resolves the call.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn ac6_abort_ends_a_stalled_open() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    // A full 300 s window: only the abort can end this call.
    world.set("RUSTAIN_TEST_RESPONSES_RETRY_SCALE", "1");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let provider = raw_provider(&world, &origin, "cop-diag-ac6-abort");
    let caller = provider.clone();
    let call = tokio::spawn(async move { turn(&caller, CHAT_MODEL).await });

    let mut held: Held = accept(&listener).await;
    held.stream.write_all(HEADERS_ONLY).await.unwrap();
    // The headers are on the wire; give the client the chance to read them,
    // then abort: the call must resolve without any further server action.
    provider.abort().await.unwrap();
    let result = call.await.unwrap();
    assert!(
        matches!(result, Err(ProviderError::Cancelled)),
        "{result:?}"
    );
    drop(held);
}
