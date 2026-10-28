//! K14 (AC15, M14): the Chat stream terminal on the OpenRouter `=0` route and
//! the Copilot Chat route, through `build_provider_for_config` →
//! `stream_completion` at a loopback stub. `[DONE]` without a `finish_reason`
//! after output is a terminal (`EndTurn`, or `ToolUse` with tool calls); a
//! `stop` with tool calls finishes `ToolUse`; a `finish_reason` then `[DONE]`
//! is one terminal; EOF without `[DONE]` after output is still the truncation
//! error; a pre-commit `Stream read error` is re-issued once (CHAT budget).
//! OpenRouter credentials come from `OPENROUTER_API_KEY`; Copilot rows from
//! `common::login` (wiremock) or the `COPILOT_GITHUB_TOKEN` envelope (raw
//! TCP stub). Each test uses its own token: Copilot's learned identity is
//! process-global per token.

use crate::conformance_20_1d3_copilot::common::{
    self as copilot, ENDPOINT_A, Held, ROOT_A, World, accept, chat_path, complete, options,
    requests, text_of, user,
};
use futures::FutureExt;
use parking_lot::Mutex;
use rustain::domain::models::{ProviderConfig, StopReason, StreamChunk};
use rustain::domain::ports::StreamingProvider;
use rustain::infrastructure::provider_factory::build_provider_for_config;
use serde_json::{Value, json};
use serial_test::serial;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use wiremock::{
    Mock, MockServer, Request, Respond, ResponseTemplate,
    matchers::{method, path},
};

pub(super) const OPENROUTER_MODEL: &str = "openai/gpt-5.5";
pub(super) const OPENROUTER_CHAT: &str = "/api/v1/chat/completions";
pub(super) const COPILOT_CHAT_MODEL: &str = "grok-code-fast-1";
/// The Copilot root of the raw-stub envelope (rebased onto the stub origin).
const RAW_COPILOT_ENDPOINT: &str = "https://api.individual.githubcopilot.com/c";
const RAW_COPILOT_CHAT: &str = "/c/chat/completions";
const TRUNCATED: &str = "OpenAI completions stream closed before a finish_reason was received";

// ── fixtures ────────────────────────────────────────────────────────────────

/// Attempts answered in order; an attempt past the script gets a 418, so an
/// extra POST can never pass for a scripted one.
pub(super) struct Seq(Mutex<VecDeque<ResponseTemplate>>);

impl Seq {
    pub(super) fn new(replies: Vec<ResponseTemplate>) -> Self {
        Self(Mutex::new(replies.into()))
    }
}

impl Respond for Seq {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        self.0
            .lock()
            .pop_front()
            .unwrap_or_else(|| ResponseTemplate::new(418))
    }
}

pub(super) fn data(value: Value) -> String {
    format!("data: {value}\n\n")
}
pub(super) const DONE: &str = "data: [DONE]\n\n";

pub(super) fn event_stream(frames: &[String]) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(frames.concat())
}

pub(super) fn content(text: &str) -> String {
    data(
        json!({"object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":text},"finish_reason":null}]}),
    )
}

/// A tool-call delta; `id`/`name` only on the call's first delta.
fn call(index: usize, head: Option<(&str, &str)>, arguments: &str) -> String {
    let mut tool = json!({"index": index, "function": {"arguments": arguments}});
    if let Some((id, name)) = head {
        tool["id"] = json!(id);
        tool["type"] = json!("function");
        tool["function"]["name"] = json!(name);
    }
    data(
        json!({"object":"chat.completion.chunk","choices":[{"index":0,"delta":{"tool_calls":[tool]},"finish_reason":null}]}),
    )
}

fn finish(reason: &str) -> String {
    data(
        json!({"object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":reason}]}),
    )
}

fn usage() -> String {
    data(
        json!({"object":"chat.completion.chunk","choices":[],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}),
    )
}

/// A complete Chat stream answering `text`.
pub(super) fn chat_body(text: &str) -> String {
    [content(text), finish("stop"), usage(), DONE.to_owned()].concat()
}

pub(super) fn errors(chunks: &[StreamChunk]) -> Vec<String> {
    chunks
        .iter()
        .filter_map(|chunk| match chunk {
            StreamChunk::Error { content } => Some(content.clone()),
            _ => None,
        })
        .collect()
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

fn tools(chunks: &[StreamChunk]) -> Vec<(String, String, Value)> {
    chunks
        .iter()
        .filter_map(|chunk| match chunk {
            StreamChunk::ToolUse { id, name, input } => {
                Some((id.clone(), name.clone(), input.clone()))
            }
            _ => None,
        })
        .collect()
}

/// The production OpenRouter entry at `base` (used verbatim).
pub(super) fn openrouter(base: &str) -> Arc<dyn StreamingProvider> {
    let config: ProviderConfig = serde_json::from_value(json!({
        "provider_id": "openrouter",
        "kind": "openrouter",
        "model_id": OPENROUTER_MODEL,
        "api_key_env": "OPENROUTER_API_KEY",
        "base_url": base,
    }))
    .expect("the OpenRouter entry parses");
    build_provider_for_config("openrouter", &config).expect("the OpenRouter provider builds")
}

#[derive(Clone, Copy, Debug)]
pub(super) enum Route {
    /// OpenRouter with `RUSTAIN_OPENROUTER_RESPONSES=0`.
    OpenRouter,
    /// GitHub Copilot's Chat Completions route.
    Copilot,
}

/// One Chat route at a wiremock stub.
struct Stub {
    server: MockServer,
    _world: World,
    provider: Arc<dyn StreamingProvider>,
    route: String,
    model: &'static str,
}

impl Stub {
    async fn new(route: Route, token: &str) -> Self {
        let server = MockServer::start().await;
        let world = World::new(&server);
        let (provider, route, model) = match route {
            Route::OpenRouter => {
                world.set("OPENROUTER_API_KEY", token);
                world.set("RUSTAIN_OPENROUTER_RESPONSES", "0");
                let provider = openrouter(&format!("{}/api/v1", server.uri()));
                (provider, OPENROUTER_CHAT.to_owned(), OPENROUTER_MODEL)
            }
            Route::Copilot => {
                copilot::login(&server, token, ENDPOINT_A).await;
                let provider = copilot::build(COPILOT_CHAT_MODEL);
                (provider, chat_path(ROOT_A), COPILOT_CHAT_MODEL)
            }
        };
        Self {
            server,
            _world: world,
            provider,
            route,
            model,
        }
    }

    /// One completion whose POSTs are answered `replies` in order: its chunks
    /// and the number of POSTs.
    async fn turn(&self, replies: Vec<ResponseTemplate>) -> (Vec<StreamChunk>, usize) {
        self.server.reset().await;
        Mock::given(method("POST"))
            .and(path(self.route.clone()))
            .respond_with(Seq::new(replies))
            .mount(&self.server)
            .await;
        let chunks = complete(
            &self.provider,
            vec![user("hi")],
            options(self.model, 256, None),
        )
        .await
        .expect("the request opens");
        let posts = requests(&self.server, "POST", &self.route).await.len();
        (chunks, posts)
    }
}

/// One Chat route at a raw TCP stub (the test writes every response): the
/// provider, its POST path and model.
pub(super) fn raw_route(
    world: &World,
    route: Route,
    origin: &str,
    token: &str,
) -> (Arc<dyn StreamingProvider>, &'static str, &'static str) {
    match route {
        Route::OpenRouter => {
            world.set("OPENROUTER_API_KEY", token);
            world.set("RUSTAIN_OPENROUTER_RESPONSES", "0");
            let provider = openrouter(&format!("{origin}/api/v1"));
            (provider, OPENROUTER_CHAT, OPENROUTER_MODEL)
        }
        Route::Copilot => {
            world.set("RUSTAIN_TEST_AUTH_ORIGIN", origin);
            world.set(
                "COPILOT_GITHUB_TOKEN",
                &json!({"token": token, "apiEndpoint": RAW_COPILOT_ENDPOINT}).to_string(),
            );
            let provider = copilot::build(COPILOT_CHAT_MODEL);
            (provider, RAW_COPILOT_CHAT, COPILOT_CHAT_MODEL)
        }
    }
}

/// Releases a held request with a complete `connection: close` response.
pub(super) async fn respond(mut held: Held, status: u16, kind: &str, body: &str) {
    let reason = match status {
        200 => "OK",
        403 => "Forbidden",
        _ => "Error",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: {kind}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    held.stream.write_all(response.as_bytes()).await.unwrap();
    held.stream.shutdown().await.ok();
}

/// A 200 event stream whose connection closes before any of the promised
/// body: the client's body read fails before output (`Stream read error`).
pub(super) async fn cut_body(mut held: Held) {
    held.stream
        .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: 64\r\nconnection: close\r\n\r\n")
        .await
        .unwrap();
    held.stream.shutdown().await.ok();
}

// ── K14 ─────────────────────────────────────────────────────────────────────

/// Every AC15 terminal case on `route`, one POST each.
async fn terminal_cases(route: Route, token: &str) {
    let stub = Stub::new(route, token).await;
    let tool_deltas = || {
        vec![
            call(0, Some(("call_a", "read")), r#"{"path":"#),
            call(1, Some(("call_b", "grep")), r#"{"q":"x"}"#),
            call(0, None, r#""a"}"#),
        ]
    };
    let expected_tools = vec![
        ("call_a".to_owned(), "read".to_owned(), json!({"path": "a"})),
        ("call_b".to_owned(), "grep".to_owned(), json!({"q": "x"})),
    ];

    // text … [DONE] without a finish_reason → one EndTurn, no error.
    let reply = event_stream(&[content("hel"), content("lo"), DONE.to_owned()]);
    let (chunks, posts) = stub.turn(vec![reply]).await;
    assert_eq!(text_of(&chunks), "hello", "{route:?}");
    assert_eq!(
        terminals(&chunks),
        [StopReason::EndTurn],
        "{route:?}: {chunks:?}"
    );
    assert!(errors(&chunks).is_empty(), "{route:?}: {chunks:?}");
    assert_eq!(posts, 1, "{route:?}");

    // tool-call deltas, then [DONE] without a finish_reason → ToolUse.
    let mut frames = tool_deltas();
    frames.push(DONE.to_owned());
    let (chunks, posts) = stub.turn(vec![event_stream(&frames)]).await;
    assert_eq!(tools(&chunks), expected_tools, "{route:?}: {chunks:?}");
    assert_eq!(
        terminals(&chunks),
        [StopReason::ToolUse],
        "{route:?}: {chunks:?}"
    );
    assert!(errors(&chunks).is_empty(), "{route:?}: {chunks:?}");
    assert_eq!(posts, 1, "{route:?}");

    // tool-call deltas with finish_reason "stop" → the same ToolUse turn.
    let mut frames = tool_deltas();
    frames.extend([finish("stop"), usage(), DONE.to_owned()]);
    let (chunks, posts) = stub.turn(vec![event_stream(&frames)]).await;
    assert_eq!(tools(&chunks), expected_tools, "{route:?}: {chunks:?}");
    assert_eq!(
        terminals(&chunks),
        [StopReason::ToolUse],
        "{route:?}: {chunks:?}"
    );
    assert!(errors(&chunks).is_empty(), "{route:?}: {chunks:?}");
    assert_eq!(posts, 1, "{route:?}");

    // A finish_reason, then [DONE] → exactly one terminal.
    for (reason, stop) in [
        ("stop", StopReason::EndTurn),
        ("length", StopReason::MaxTokens),
    ] {
        let what = format!("{route:?} {reason}");
        let reply = event_stream(&[content("ok"), finish(reason), usage(), DONE.to_owned()]);
        let (chunks, posts) = stub.turn(vec![reply]).await;
        assert_eq!(text_of(&chunks), "ok", "{what}");
        assert_eq!(terminals(&chunks), [stop], "{what}: {chunks:?}");
        assert!(errors(&chunks).is_empty(), "{what}: {chunks:?}");
        assert_eq!(posts, 1, "{what}");
    }

    // Positive control: EOF without [DONE] after output → the truncation error.
    let replies = vec![
        event_stream(&[content("partial")]),
        event_stream(&[chat_body("never")]),
    ];
    let (chunks, posts) = stub.turn(replies).await;
    assert_eq!(text_of(&chunks), "partial", "{route:?}");
    assert!(terminals(&chunks).is_empty(), "{route:?}: {chunks:?}");
    assert_eq!(errors(&chunks), [TRUNCATED], "{route:?}");
    assert!(
        matches!(chunks.last(), Some(StreamChunk::Error { .. })),
        "{route:?}: {chunks:?}"
    );
    assert_eq!(posts, 1, "{route:?}: no re-issue after commit");
}

/// A pre-commit `Stream read error` on `route` → exactly 2 POSTs; the second
/// attempt's text is served without an error.
async fn read_error_case(route: Route, token: &str) {
    let server = MockServer::start().await;
    let world = World::new(&server);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let (provider, route_path, model) = raw_route(&world, route, &origin, token);
    let turn = tokio::spawn(async move {
        complete(&provider, vec![user("hi")], options(model, 256, None)).await
    });

    let held = accept(&listener).await;
    assert_eq!(held.path(), route_path, "{route:?}");
    cut_body(held).await;
    let held = accept(&listener).await;
    assert_eq!(held.path(), route_path, "{route:?}");
    respond(held, 200, "text/event-stream", &chat_body("second")).await;

    let chunks = turn.await.unwrap().expect("the request opens");
    assert_eq!(text_of(&chunks), "second", "{route:?}");
    assert!(errors(&chunks).is_empty(), "{route:?}: {chunks:?}");
    assert_eq!(terminals(&chunks), [StopReason::EndTurn], "{route:?}");
    assert!(
        listener.accept().now_or_never().is_none(),
        "{route:?}: exactly 2 POSTs"
    );
}

/// K14 (M14): every AC15 terminal case on OpenRouter's Chat route.
#[tokio::test]
#[serial]
async fn k14_openrouter_chat_stream_terminal() {
    terminal_cases(Route::OpenRouter, "sk-or-v1-k14-terminal").await;
}

/// K14 (M14): every AC15 terminal case on Copilot's Chat route.
#[tokio::test]
#[serial]
async fn k14_copilot_chat_stream_terminal() {
    terminal_cases(Route::Copilot, "cop-k14-terminal").await;
}

/// K14: a pre-commit `Stream read error` is transient on OpenRouter's Chat
/// route (re-issued once, the second attempt served).
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn k14_openrouter_pre_commit_read_error_is_reissued_once() {
    read_error_case(Route::OpenRouter, "sk-or-v1-k14-read-error").await;
}

/// K14: the same on Copilot's Chat route.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn k14_copilot_pre_commit_read_error_is_reissued_once() {
    read_error_case(Route::Copilot, "cop-k14-read-error").await;
}
