//! K15 (AC16, M15): Copilot Messages retry parity through
//! `build_provider_for_config` → `stream_completion`. An opaque 429 (empty,
//! whitespace or status-only body) is an account limit: one POST and
//! `RateLimited`; an informative 429 is re-issued (positive control). After a
//! 403 identity denial whose alternate wins, a later pre-commit re-issue of the
//! same request sends the won identity directly, on Messages and on Chat.
//! The per-attempt first-event window is the paused-clock lib leg in
//! `stream_retry`. Each test uses its own token: Copilot's learned identity is
//! process-global per token.

use super::chat_terminal::{
    COPILOT_CHAT_MODEL, Route, Seq, chat_body, cut_body, errors, raw_route, respond,
};
use crate::conformance_20_1d3_copilot::common::{
    self as copilot, ENDPOINT_A, ROOT_A, World, accept, complete, header, messages_path,
    messages_sse, options, requests, text_of, user,
};
use futures::FutureExt;
use parking_lot::Mutex;
use rustain::domain::errors::ProviderError;
use rustain::domain::models::StreamChunk;
use rustain::domain::ports::StreamingProvider;
use serde_json::{Value, json};
use serial_test::serial;
use std::sync::Arc;
use tokio::net::TcpListener;
use wiremock::{
    Mock, MockServer, Request, Respond, ResponseTemplate,
    matchers::{method, path},
};

const MESSAGES_MODEL: &str = "claude-sonnet-4.5";
const CHAT: &str = "copilot-chat";
const CLI: &str = "copilot-developer-cli";
const DENIED: &str = r#"{"error":{"message":"Forbidden"}}"#;

fn json_error(status: u16, body: Value) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(body)
}
fn status_only(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(429).set_body_string(body)
}

/// A Messages stream that fails before its first token with a transient
/// (`overloaded_error`) envelope.
fn messages_overloaded() -> ResponseTemplate {
    let error = json!({"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}});
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(format!("event: error\ndata: {error}\n\n"))
}

/// One completion on the Messages route under `token`'s stored row whose
/// POSTs are answered `replies` in order: the outcome and the POST count.
async fn messages_turn(
    server: &MockServer,
    provider: &Arc<dyn StreamingProvider>,
    replies: Vec<ResponseTemplate>,
) -> (Result<Vec<StreamChunk>, ProviderError>, usize) {
    let route = messages_path(ROOT_A);
    server.reset().await;
    Mock::given(method("POST"))
        .and(path(route.clone()))
        .respond_with(Seq::new(replies))
        .mount(server)
        .await;
    let result = complete(
        provider,
        vec![user("hi")],
        options(MESSAGES_MODEL, 256, None),
    )
    .await;
    (result, requests(server, "POST", &route).await.len())
}

/// K15 (M15): an opaque 429 on Copilot Messages is final after one POST with
/// `RateLimited { retry_after_ms }`; an informative 429 is re-issued.
#[tokio::test]
#[serial]
async fn k15_copilot_messages_opaque_429_is_final_and_informative_429_is_reissued() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    copilot::login(&server, "cop-k15-opaque-429", ENDPOINT_A).await;
    let provider = copilot::build(MESSAGES_MODEL);

    for (reply, retry_after_ms) in [
        (status_only(""), None),
        (status_only(" \n "), None),
        (status_only("429 status code (no body)"), None),
        (
            status_only("{}").insert_header("retry-after", "7"),
            Some(7_000_u64),
        ),
    ] {
        let (result, posts) =
            messages_turn(&server, &provider, vec![reply, messages_sse("never")]).await;
        let error = result.expect_err("an opaque 429 is final");
        assert!(
            matches!(error, ProviderError::RateLimited { retry_after_ms: ms } if ms == retry_after_ms),
            "{error:?}"
        );
        assert_eq!(posts, 1, "an opaque 429 is never re-issued");
    }

    // Positive control: an informative 429 is re-issued and the retry served.
    let (result, posts) = messages_turn(
        &server,
        &provider,
        vec![
            json_error(
                429,
                json!({"error":{"message":"Too many requests, retry later"}}),
            ),
            messages_sse("ok"),
        ],
    )
    .await;
    assert_eq!(text_of(&result.expect("the retry is served")), "ok");
    assert_eq!(posts, 2);
}

/// Copilot Messages stub by `copilot-integration-id`: the default identity is
/// denied; the CLI identity's first answer fails before its first token and
/// every later one succeeds. Each POST's identity is recorded in order.
#[derive(Clone, Default)]
struct Identities(Arc<Mutex<Vec<String>>>);

impl Respond for Identities {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let id = header(request, "copilot-integration-id").unwrap_or_default();
        let mut seen = self.0.lock();
        seen.push(id.clone());
        let cli_answers = seen
            .iter()
            .filter(|seen_id| seen_id.as_str() == CLI)
            .count();
        match id.as_str() {
            CLI if cli_answers == 1 => messages_overloaded(),
            CLI => messages_sse("ok"),
            _ => json_error(403, json!({"error":{"message":"Forbidden"}})),
        }
    }
}

/// K15 (AC16): Messages — POST1 default identity 403, POST2 alternate 200
/// then a pre-commit transient failure, POST3 carries the won alternate (no
/// second denied round trip).
#[tokio::test]
#[serial]
async fn k15_copilot_messages_reissue_reuses_the_won_identity() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    copilot::login(&server, "cop-k15-messages-won-identity", ENDPOINT_A).await;
    let provider = copilot::build(MESSAGES_MODEL);
    let route = messages_path(ROOT_A);
    let identities = Identities::default();
    server.reset().await;
    Mock::given(method("POST"))
        .and(path(route.clone()))
        .respond_with(identities.clone())
        .mount(&server)
        .await;

    let chunks = complete(
        &provider,
        vec![user("hi")],
        options(MESSAGES_MODEL, 256, None),
    )
    .await
    .expect("the request opens");
    assert_eq!(text_of(&chunks), "ok");
    assert!(errors(&chunks).is_empty(), "{chunks:?}");
    assert_eq!(*identities.0.lock(), [CHAT, CLI, CLI]);
    assert_eq!(requests(&server, "POST", &route).await.len(), 3);
}

/// K15 (AC16): Chat — POST1 default identity 403, POST2 alternate 200 whose
/// body read fails before any output, POST3 carries the won alternate.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn k15_copilot_chat_reissue_reuses_the_won_identity() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let (provider, route, model) =
        raw_route(&world, Route::Copilot, &origin, "cop-k15-chat-won-identity");
    assert_eq!(model, COPILOT_CHAT_MODEL);
    let turn = tokio::spawn(async move {
        complete(&provider, vec![user("hi")], options(model, 256, None)).await
    });

    let held = accept(&listener).await;
    assert_eq!(held.path(), route);
    assert_eq!(held.header("copilot-integration-id").as_deref(), Some(CHAT));
    respond(held, 403, "application/json", DENIED).await;
    let held = accept(&listener).await;
    assert_eq!(held.header("copilot-integration-id").as_deref(), Some(CLI));
    cut_body(held).await;
    let held = accept(&listener).await;
    assert_eq!(held.path(), route);
    assert_eq!(
        held.header("copilot-integration-id").as_deref(),
        Some(CLI),
        "the re-issue sends the won identity directly"
    );
    respond(held, 200, "text/event-stream", &chat_body("ok")).await;

    let chunks = turn.await.unwrap().expect("the request opens");
    assert_eq!(text_of(&chunks), "ok");
    assert!(errors(&chunks).is_empty(), "{chunks:?}");
    assert!(
        listener.accept().now_or_never().is_none(),
        "exactly 3 POSTs"
    );
}
