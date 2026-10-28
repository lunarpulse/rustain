//! Copilot client-identity fallback (K08/AC8, M08 concurrent provenance) and
//! 401 vs 403 plus HTTP-attempt and stream resilience counts (K09/AC9, M09),
//! through `build_provider_for_config` → `stream_completion`. Stored rows come
//! from `common::login`; env credentials are `COPILOT_GITHUB_TOKEN` JSON
//! envelopes. Every test uses its own token: the learned-identity cache is
//! process-global and keyed by the token digest.

use super::common::{
    self, ENDPOINT_A, ENDPOINT_B, FORBIDDEN, Held, PROVIDER, ROOT_A, ROOT_B, TOKEN_PATH, World,
    accept, build, chat_path, chat_sse, complete, header, messages_path, messages_sse, oauth,
    options, requests, responses_ok, responses_path, rows, text_of, user,
};
use parking_lot::Mutex;
use rustain::domain::errors::ProviderError;
use rustain::domain::models::{StopReason, StreamChunk};
use rustain::domain::ports::StreamingProvider;
use serde_json::{Value, json};
use serial_test::serial;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use wiremock::{
    Mock, MockServer, Request, Respond, ResponseTemplate,
    matchers::{method, path},
};

const CHAT: &str = "copilot-chat";
const CLI: &str = "copilot-developer-cli";
const MESSAGES_MODEL: &str = "claude-sonnet-4.5";
const RESPONSES_MODEL: &str = "gpt-5.5";
const CHAT_MODEL: &str = "grok-code-fast-1";
const TRUNCATED: &str = "OpenAI completions stream closed before a finish_reason was received";

// ── fixtures ────────────────────────────────────────────────────────────────

/// Inference stub answering by the request's `copilot-integration-id`: a
/// denied id gets its denial, every other id the route's success; each
/// request's id is recorded in arrival order.
#[derive(Clone)]
struct Gate(Arc<Mutex<GateState>>);

struct GateState {
    ok: ResponseTemplate,
    denials: Vec<(String, ResponseTemplate)>,
    seen: Vec<String>,
}

impl Gate {
    fn new(ok: ResponseTemplate) -> Self {
        Self(Arc::new(Mutex::new(GateState {
            ok,
            denials: Vec::new(),
            seen: Vec::new(),
        })))
    }
    /// From now on `ids` are answered `denial`; every other id succeeds.
    fn deny(&self, ids: &[&str], denial: ResponseTemplate) {
        self.0.lock().denials = ids
            .iter()
            .map(|id| ((*id).to_owned(), denial.clone()))
            .collect();
    }
    fn allow(&self) {
        self.0.lock().denials.clear();
    }
    /// The ids received since the last call, in arrival order.
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut self.0.lock().seen)
    }
    async fn mount(&self, server: &MockServer, route: &str) {
        Mock::given(method("POST"))
            .and(path(route))
            .respond_with(self.clone())
            .mount(server)
            .await;
    }
}

impl Respond for Gate {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let id = header(request, "copilot-integration-id").unwrap_or_default();
        let mut state = self.0.lock();
        state.seen.push(id.clone());
        state
            .denials
            .iter()
            .find(|(denied, _)| *denied == id)
            .map(|(_, denial)| denial.clone())
            .unwrap_or_else(|| state.ok.clone())
    }
}

/// Attempts answered in order; an attempt past the script gets a 418, so an
/// extra POST can never pass for a scripted one.
struct Seq(Mutex<VecDeque<ResponseTemplate>>);

impl Seq {
    fn new(replies: Vec<ResponseTemplate>) -> Self {
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

async fn script(server: &MockServer, route: &str, replies: Vec<ResponseTemplate>) {
    Mock::given(method("POST"))
        .and(path(route))
        .respond_with(Seq::new(replies))
        .mount(server)
        .await;
}

fn json_error(status: u16, body: Value) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(body)
}
fn forbidden() -> ResponseTemplate {
    json_error(403, json!({"error":{"message":"Forbidden"}}))
}
fn sse(frames: &[String]) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(frames.concat())
}
fn data(value: Value) -> String {
    format!("data: {value}\n\n")
}
fn event(name: &str, value: Value) -> String {
    format!("event: {name}\ndata: {value}\n\n")
}

/// `COPILOT_GITHUB_TOKEN` as omp's JSON envelope.
fn envelope(token: &str, endpoint: &str) -> String {
    json!({"token": token, "apiEndpoint": endpoint}).to_string()
}

async fn turn(
    provider: &Arc<dyn StreamingProvider>,
    model: &str,
) -> Result<Vec<StreamChunk>, ProviderError> {
    complete(provider, vec![user("hi")], options(model, 256, None)).await
}

fn ok_text(result: Result<Vec<StreamChunk>, ProviderError>) -> String {
    text_of(&result.expect("the request succeeds"))
}

/// The request failed with omp's 403 text; the bearer appears nowhere.
fn assert_forbidden(result: Result<Vec<StreamChunk>, ProviderError>, token: &str) {
    let error = result.expect_err("a denied request fails");
    assert!(!format!("{error} {error:?}").contains(token), "{error:?}");
    match error {
        ProviderError::Other(text) => assert_eq!(text, FORBIDDEN),
        other => panic!("expected the Copilot 403 text, got {other:?}"),
    }
}

fn errors(chunks: &[StreamChunk]) -> Vec<String> {
    chunks
        .iter()
        .filter_map(|chunk| match chunk {
            StreamChunk::Error { content } => Some(content.clone()),
            _ => None,
        })
        .collect()
}

// ── K08: fallback ───────────────────────────────────────────────────────────

/// One credential and adapter through the full learned-identity cycle on
/// `route`: chat denied → CLI learned; CLI denied with CLI provenance → chat
/// learned; a failed alternate clears the entry (twice: from learned chat and
/// from learned CLI, the latter observable as a default start).
async fn learned_identity_cycle(
    model: &str,
    route: fn(&str) -> String,
    ok: ResponseTemplate,
    token: &str,
) {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    common::login(&server, token, ENDPOINT_A).await;
    let gate = Gate::new(ok);
    gate.mount(&server, &route(ROOT_A)).await;
    let provider = build(model);

    // chat 403 → one CLI alternate, 200 → learned.
    gate.deny(&[CHAT], forbidden());
    assert_eq!(ok_text(turn(&provider, model).await), "ok");
    assert_eq!(gate.take(), [CHAT, CLI], "{model}: chat 403 → CLI 200");
    assert_eq!(ok_text(turn(&provider, model).await), "ok");
    assert_eq!(
        gate.take(),
        [CLI],
        "{model}: the learned CLI starts the next request"
    );

    // learned CLI denied → one chat alternate, 200 → chat learned.
    gate.deny(&[CLI], forbidden());
    assert_eq!(ok_text(turn(&provider, model).await), "ok");
    assert_eq!(gate.take(), [CLI, CHAT], "{model}: CLI 403 → chat 200");
    assert_eq!(ok_text(turn(&provider, model).await), "ok");
    assert_eq!(
        gate.take(),
        [CHAT],
        "{model}: the learned chat starts the next request"
    );

    // both denied from learned chat → FORBIDDEN, cleared → default start.
    gate.deny(&[CHAT, CLI], forbidden());
    assert_forbidden(turn(&provider, model).await, token);
    assert_eq!(gate.take(), [CHAT, CLI], "{model}: a failed CLI alternate");
    gate.allow();
    assert_eq!(ok_text(turn(&provider, model).await), "ok");
    assert_eq!(
        gate.take(),
        [CHAT],
        "{model}: the default after a failed alternate"
    );

    // learn CLI again, then both denied → FORBIDDEN, cleared → default chat.
    gate.deny(&[CHAT], forbidden());
    assert_eq!(ok_text(turn(&provider, model).await), "ok");
    assert_eq!(gate.take(), [CHAT, CLI]);
    gate.deny(&[CHAT, CLI], forbidden());
    assert_forbidden(turn(&provider, model).await, token);
    assert_eq!(gate.take(), [CLI, CHAT], "{model}: a failed chat alternate");
    gate.allow();
    assert_eq!(ok_text(turn(&provider, model).await), "ok");
    assert_eq!(
        gate.take(),
        [CHAT],
        "{model}: a failed alternate clears the learned CLI; the next starts at the default"
    );
}

/// K08 (M08): chat 403 → CLI 200 is learned (next starts CLI, 1 POST); a CLI
/// denial with CLI provenance → chat 200 is learned (next starts chat); a
/// failed alternate fails with the 403 text and clears the entry — Chat route.
#[tokio::test]
#[serial]
async fn k08_chat_route_learns_and_clears_the_working_identity() {
    learned_identity_cycle(
        CHAT_MODEL,
        chat_path,
        chat_sse("ok"),
        "cop-identity-k08-chat-route",
    )
    .await;
}

/// K08 (M08): the same learned-identity cycle on the Messages route.
#[tokio::test]
#[serial]
async fn k08_messages_route_learns_and_clears_the_working_identity() {
    learned_identity_cycle(
        MESSAGES_MODEL,
        messages_path,
        messages_sse("ok"),
        "cop-identity-k08-messages-route",
    )
    .await;
}

/// K08 (M08): the same learned-identity cycle on the Responses route.
#[tokio::test]
#[serial]
async fn k08_responses_route_learns_and_clears_the_working_identity() {
    learned_identity_cycle(
        RESPONSES_MODEL,
        responses_path,
        responses_ok("ok"),
        "cop-identity-k08-responses-route",
    )
    .await;
}

/// K08 (M08): a 400 `model_not_supported` is a denial (CLI alternate); a 400
/// with any other code is final after one POST with the Copilot HTTP text.
#[tokio::test]
#[serial]
async fn k08_model_not_supported_takes_the_alternate_and_other_400s_do_not() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let token = "cop-identity-k08-model-not-supported";
    common::login(&server, token, ENDPOINT_A).await;
    let gate = Gate::new(chat_sse("ok"));
    gate.mount(&server, &chat_path(ROOT_A)).await;
    let provider = build(CHAT_MODEL);

    gate.deny(
        &[CHAT],
        json_error(
            400,
            json!({"error":{"code":"model_not_supported","message":"unsupported"}}),
        ),
    );
    assert_eq!(ok_text(turn(&provider, CHAT_MODEL).await), "ok");
    assert_eq!(gate.take(), [CHAT, CLI]);
    assert_eq!(ok_text(turn(&provider, CHAT_MODEL).await), "ok");
    assert_eq!(gate.take(), [CLI], "the alternate was learned");

    // A different credential (fresh default start) and a plain 400.
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let token = "cop-identity-k08-bad-request";
    common::login(&server, token, ENDPOINT_A).await;
    let gate = Gate::new(chat_sse("ok"));
    gate.mount(&server, &chat_path(ROOT_A)).await;
    let provider = build(CHAT_MODEL);
    gate.deny(
        &[CHAT],
        json_error(
            400,
            json!({"error":{"code":"bad_request","message":"nope"}}),
        ),
    );
    let error = turn(&provider, CHAT_MODEL)
        .await
        .expect_err("a plain 400 is final");
    assert_eq!(gate.take(), [CHAT], "no alternate for a plain 400");
    match error {
        ProviderError::Other(text) => assert_eq!(text, "GitHub Copilot HTTP 400: nope"),
        other => panic!("expected the Copilot HTTP 400 text, got {other:?}"),
    }
}

/// K08 (M08): `COPILOT_INTEGRATION_ID` wins over a learned entry and is never
/// followed by an alternate, even when the pin is `copilot-chat` itself.
#[tokio::test]
#[serial]
async fn k08_a_pinned_integration_id_never_takes_an_alternate() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    let token = "cop-identity-k08-pinned";
    common::login(&server, token, ENDPOINT_A).await;
    let gate = Gate::new(chat_sse("ok"));
    gate.mount(&server, &chat_path(ROOT_A)).await;
    let provider = build(CHAT_MODEL);

    // Unpinned: CLI is learned for this credential.
    gate.deny(&[CHAT], forbidden());
    assert_eq!(ok_text(turn(&provider, CHAT_MODEL).await), "ok");
    assert_eq!(gate.take(), [CHAT, CLI]);

    world.set("COPILOT_INTEGRATION_ID", CHAT);
    assert_forbidden(turn(&provider, CHAT_MODEL).await, token);
    assert_eq!(
        gate.take(),
        [CHAT],
        "a pinned copilot-chat is never retried as the CLI"
    );

    world.set("COPILOT_INTEGRATION_ID", "custom-id");
    gate.deny(&["custom-id"], forbidden());
    assert_forbidden(turn(&provider, CHAT_MODEL).await, token);
    assert_eq!(gate.take(), ["custom-id"], "a pinned id has no alternate");
    gate.allow();
    assert_eq!(ok_text(turn(&provider, CHAT_MODEL).await), "ok");
    assert_eq!(gate.take(), ["custom-id"]);

    // Unpinned again: the learned CLI entry is untouched by pinned requests.
    world.unset("COPILOT_INTEGRATION_ID");
    assert_eq!(ok_text(turn(&provider, CHAT_MODEL).await), "ok");
    assert_eq!(gate.take(), [CLI]);
}

/// K08 (M08): two credentials, or one credential on two roots, never share a
/// learned identity.
#[tokio::test]
#[serial]
async fn k08_credentials_and_roots_do_not_share_a_learned_identity() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    let gate = Gate::new(chat_sse("ok"));
    gate.mount(&server, &chat_path(ROOT_A)).await;
    gate.mount(&server, &chat_path(ROOT_B)).await;
    let first = "cop-identity-k08-isolation-one";
    let second = "cop-identity-k08-isolation-two";

    world.set("COPILOT_GITHUB_TOKEN", &envelope(first, ENDPOINT_A));
    let provider = build(CHAT_MODEL);
    gate.deny(&[CHAT], forbidden());
    assert_eq!(ok_text(turn(&provider, CHAT_MODEL).await), "ok");
    assert_eq!(gate.take(), [CHAT, CLI]);
    assert_eq!(ok_text(turn(&provider, CHAT_MODEL).await), "ok");
    assert_eq!(gate.take(), [CLI], "credential one learned the CLI");
    gate.allow();

    world.set("COPILOT_GITHUB_TOKEN", &envelope(second, ENDPOINT_A));
    let provider = build(CHAT_MODEL);
    assert_eq!(ok_text(turn(&provider, CHAT_MODEL).await), "ok");
    assert_eq!(gate.take(), [CHAT], "credential two starts at the default");
    assert_eq!(
        header(
            &requests(&server, "POST", &chat_path(ROOT_A)).await[3],
            "authorization"
        )
        .as_deref(),
        Some(format!("Bearer {second}").as_str())
    );

    world.set("COPILOT_GITHUB_TOKEN", &envelope(first, ENDPOINT_B));
    let provider = build(CHAT_MODEL);
    assert_eq!(ok_text(turn(&provider, CHAT_MODEL).await), "ok");
    assert_eq!(
        gate.take(),
        [CHAT],
        "credential one on another root starts at the default"
    );
    assert_eq!(requests(&server, "POST", &chat_path(ROOT_B)).await.len(), 1);

    world.set("COPILOT_GITHUB_TOKEN", &envelope(first, ENDPOINT_A));
    let provider = build(CHAT_MODEL);
    assert_eq!(ok_text(turn(&provider, CHAT_MODEL).await), "ok");
    assert_eq!(
        gate.take(),
        [CLI],
        "credential one's entry on its root is kept"
    );
}

// ── K08: concurrent provenance on a raw held stub ───────────────────────────

fn chat_body(text: &str) -> String {
    [
        data(json!({"object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":text},"finish_reason":null}]})),
        data(json!({"object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}})),
        "data: [DONE]\n\n".to_owned(),
    ]
    .concat()
}

/// Releases a held request with a complete `connection: close` response.
async fn answer(mut held: Held, status: u16, body: &str) {
    let (reason, kind) = match status {
        200 => ("OK", "text/event-stream"),
        _ => ("Forbidden", "application/json"),
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: {kind}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    held.stream.write_all(response.as_bytes()).await.unwrap();
    held.stream.shutdown().await.ok();
}

const DENIED_BODY: &str = r#"{"error":{"message":"Forbidden"}}"#;

/// K08 (M08): a request decides its alternate from the provenance captured
/// at its own `begin`: A starts as the learned CLI and is held; sibling B on
/// the same credential is denied as CLI and learns chat; A's CLI 403 is then
/// still followed by exactly one chat alternate, and A succeeds.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn k08_concurrent_request_uses_its_begin_snapshot_for_the_alternate() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    world.set("RUSTAIN_TEST_AUTH_ORIGIN", &format!("http://{addr}"));
    let token = "cop-identity-k08-concurrent";
    world.set(
        "COPILOT_GITHUB_TOKEN",
        &envelope(token, "https://api.individual.githubcopilot.com/c"),
    );
    let route = "/c/chat/completions";
    let provider = build(CHAT_MODEL);
    let spawn = |provider: &Arc<dyn StreamingProvider>, prompt: &'static str| {
        let provider = provider.clone();
        tokio::spawn(async move {
            complete(
                &provider,
                vec![user(prompt)],
                options(CHAT_MODEL, 256, None),
            )
            .await
        })
    };

    // Learn the CLI for credential C.
    let learn = spawn(&provider, "learn");
    let held = accept(&listener).await;
    assert_eq!(held.path(), route);
    assert_eq!(held.header("copilot-integration-id").as_deref(), Some(CHAT));
    answer(held, 403, DENIED_BODY).await;
    let held = accept(&listener).await;
    assert_eq!(held.header("copilot-integration-id").as_deref(), Some(CLI));
    answer(held, 200, &chat_body("learned")).await;
    assert_eq!(ok_text(learn.await.unwrap()), "learned");

    // A begins with CLI provenance and is held.
    let mut a = spawn(&provider, "a");
    let held_a = accept(&listener).await;
    assert_eq!(held_a.path(), route);
    assert_eq!(
        held_a.header("copilot-integration-id").as_deref(),
        Some(CLI)
    );

    // B: CLI 403 → chat 200, learning chat while A is in flight.
    let b = spawn(&provider, "b");
    let held = accept(&listener).await;
    assert_eq!(held.header("copilot-integration-id").as_deref(), Some(CLI));
    answer(held, 403, DENIED_BODY).await;
    let held = accept(&listener).await;
    assert_eq!(held.header("copilot-integration-id").as_deref(), Some(CHAT));
    answer(held, 200, &chat_body("b")).await;
    assert_eq!(ok_text(b.await.unwrap()), "b");

    // A's CLI 403: its begin snapshot was CLI → exactly one chat alternate.
    answer(held_a, 403, DENIED_BODY).await;
    let held = tokio::select! {
        held = accept(&listener) => held,
        result = &mut a => panic!("A must resend once as copilot-chat, got {result:?}"),
    };
    assert_eq!(held.path(), route);
    assert_eq!(held.header("copilot-integration-id").as_deref(), Some(CHAT));
    answer(held, 200, &chat_body("a")).await;
    assert_eq!(ok_text(a.await.unwrap()), "a");
}

// ── K09: 401 vs 403 ─────────────────────────────────────────────────────────

/// Every route path under the login root with its model.
fn routes() -> [(&'static str, String); 3] {
    [
        (CHAT_MODEL, chat_path(ROOT_A)),
        (MESSAGES_MODEL, messages_path(ROOT_A)),
        (RESPONSES_MODEL, responses_path(ROOT_A)),
    ]
}

/// K09 (M09): a stored-row 401 is final on every route — exactly one
/// inference request, no token-endpoint POST or other HTTP (refresh-same is
/// zero HTTP), `AuthenticationFailed`, the row kept enabled with its token.
#[tokio::test]
#[serial]
async fn k09_stored_row_401_is_final_without_http_refresh_or_row_loss() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let token = "cop-identity-k09-unauthorized";
    common::login(&server, token, ENDPOINT_A).await;
    let token_posts = requests(&server, "POST", TOKEN_PATH).await.len();
    let before = server.received_requests().await.unwrap().len();
    for (_, route) in routes() {
        common::reply(
            &server,
            "POST",
            &route,
            json_error(
                401,
                json!({"error":{"message":format!("bad credentials {token}")}}),
            ),
        )
        .await;
    }

    for (model, route) in routes() {
        let provider = build(model);
        let error = turn(&provider, model).await.expect_err("a 401 is final");
        assert!(!format!("{error} {error:?}").contains(token), "{error:?}");
        assert!(
            matches!(error, ProviderError::AuthenticationFailed),
            "{model}: {error:?}"
        );
        assert_eq!(
            requests(&server, "POST", &route).await.len(),
            1,
            "{model}: exactly one inference request"
        );
    }

    let after = server.received_requests().await.unwrap();
    let paths: Vec<String> = after[before..]
        .iter()
        .map(|request| request.url.path().to_owned())
        .collect();
    let expected: Vec<String> = routes().into_iter().map(|(_, route)| route).collect();
    assert_eq!(paths, expected, "no refresh, recovery or other HTTP");
    assert_eq!(
        requests(&server, "POST", TOKEN_PATH).await.len(),
        token_posts
    );
    let stored = rows(PROVIDER).await;
    assert_eq!(stored.len(), 1, "the row is not deleted");
    assert!(
        stored[0].disabled_cause.is_none(),
        "the row is not disabled"
    );
    assert_eq!(oauth(&stored[0]).access.expose_secret(), token);
}

/// K09 (M09): on its own fresh login, a 403 after the CLI alternate is the
/// omp 403 text on every route and never recovers: the row's stamp (`id`,
/// `rev`, `generation`) is unchanged and it is never disabled.
#[tokio::test]
#[serial]
async fn k09_403_after_its_alternate_never_touches_the_fresh_row() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let token = "cop-identity-k09-forbidden-fresh";
    common::login(&server, token, ENDPOINT_A).await;
    let fresh = rows(PROVIDER).await;
    assert_eq!(fresh.len(), 1);
    let stamp = fresh[0].stamp();
    let before = server.received_requests().await.unwrap().len();
    for (_, route) in routes() {
        common::reply(&server, "POST", &route, forbidden()).await;
    }

    for (model, route) in routes() {
        let provider = build(model);
        assert_forbidden(turn(&provider, model).await, token);
        let sent: Vec<Option<String>> = requests(&server, "POST", &route)
            .await
            .iter()
            .map(|request| header(request, "copilot-integration-id"))
            .collect();
        assert_eq!(
            sent,
            [Some(CHAT.to_owned()), Some(CLI.to_owned())],
            "{model}: one alternate, then the 403 text"
        );
        let stored = rows(PROVIDER).await;
        assert_eq!(stored.len(), 1);
        assert_eq!(
            stored[0].stamp(),
            stamp,
            "{model}: a 403 never recovers (rev bump)"
        );
        assert!(
            stored[0].disabled_cause.is_none(),
            "{model}: never disabled"
        );
        assert_eq!(oauth(&stored[0]).access.expose_secret(), token);
    }
    assert_eq!(
        server.received_requests().await.unwrap().len() - before,
        6,
        "only the inference POSTs and their alternates"
    );
}

// ── K09: HTTP attempts ──────────────────────────────────────────────────────

fn assert_clean(error: &ProviderError, token: &str) {
    assert!(
        !format!("{error} {error:?}").contains(token),
        "bearer leaked: {error:?}"
    );
}

/// K09 (M09): HTTP-attempt policies with the retry knob at 0 — Chat
/// 503,503,200 → 3; Messages (omp provider loop, RF-16) 529 with
/// `x-should-retry: false` → 2 (the header is not consulted); Messages 409 →
/// 1, final; Chat 429 `retry-after: 120` → 1 `RateLimited{120000}`; a Chat
/// admission 429 → 1. Error texts never carry the bearer.
#[tokio::test]
#[serial]
async fn k09_http_attempt_counts_follow_each_route_policy() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let token = "cop-identity-k09-attempts";
    common::login(&server, token, ENDPOINT_A).await;
    let chat = chat_path(ROOT_A);
    let messages = messages_path(ROOT_A);
    let echo = || json!({"error":{"message":format!("busy for Bearer {token} {token}")}});

    // Chat 503, 503, 200 → 3 POSTs.
    server.reset().await;
    script(
        &server,
        &chat,
        vec![
            json_error(503, echo()),
            json_error(503, echo()),
            chat_sse("ok"),
        ],
    )
    .await;
    let provider = build(CHAT_MODEL);
    assert_eq!(ok_text(turn(&provider, CHAT_MODEL).await), "ok");
    assert_eq!(requests(&server, "POST", &chat).await.len(), 3);

    // Messages 529 with x-should-retry: false → retried anyway, 2 POSTs: omp's
    // provider loop pins the client retries to 0 and never reads the header.
    server.reset().await;
    script(
        &server,
        &messages,
        vec![
            json_error(
                529,
                json!({"type":"error","error":{"type":"overloaded_error","message":format!("Overloaded for {token}")}}),
            )
            .insert_header("x-should-retry", "false"),
            messages_sse("ok"),
        ],
    )
    .await;
    let provider = build(MESSAGES_MODEL);
    assert_eq!(ok_text(turn(&provider, MESSAGES_MODEL).await), "ok");
    assert_eq!(requests(&server, "POST", &messages).await.len(), 2);

    // Messages 409 → final after 1 POST (omp: every 4xx but 408/429 is terminal).
    server.reset().await;
    script(
        &server,
        &messages,
        vec![json_error(409, echo()), messages_sse("ok")],
    )
    .await;
    let error = turn(&provider, MESSAGES_MODEL)
        .await
        .expect_err("a 409 is final");
    assert_clean(&error, token);
    match &error {
        ProviderError::Other(text) => {
            assert!(text.starts_with("GitHub Copilot HTTP 409: "), "{text}");
        }
        other => panic!("expected the Copilot HTTP text, got {other:?}"),
    }
    assert_eq!(requests(&server, "POST", &messages).await.len(), 1);

    // Chat 429 with retry-after: 120 → 1 POST, RateLimited{120000}.
    server.reset().await;
    script(
        &server,
        &chat,
        vec![
            json_error(429, echo()).insert_header("retry-after", "120"),
            chat_sse("ok"),
        ],
    )
    .await;
    let provider = build(CHAT_MODEL);
    let error = turn(&provider, CHAT_MODEL)
        .await
        .expect_err("a hint over 60 s is not retried");
    assert_clean(&error, token);
    assert!(
        matches!(
            error,
            ProviderError::RateLimited {
                retry_after_ms: Some(120_000)
            }
        ),
        "{error:?}"
    );
    assert_eq!(requests(&server, "POST", &chat).await.len(), 1);

    // Chat admission 429 → 1 POST.
    server.reset().await;
    script(
        &server,
        &chat,
        vec![
            json_error(
                429,
                json!({"error":{"type":"rate_limit","rate_limit_type":"max_parallel_requests","message":format!("too many parallel requests {token}")}}),
            ),
            chat_sse("ok"),
        ],
    )
    .await;
    let error = turn(&provider, CHAT_MODEL)
        .await
        .expect_err("an admission rejection is not retried");
    assert_clean(&error, token);
    assert!(
        matches!(error, ProviderError::RateLimited { .. }),
        "{error:?}"
    );
    assert_eq!(requests(&server, "POST", &chat).await.len(), 1);

    // A final 4xx echoing the bearer → the Copilot HTTP text without it.
    server.reset().await;
    script(&server, &chat, vec![json_error(422, echo())]).await;
    let error = turn(&provider, CHAT_MODEL)
        .await
        .expect_err("a 422 is final");
    assert_clean(&error, token);
    match &error {
        ProviderError::Other(text) => {
            assert!(text.starts_with("GitHub Copilot HTTP 422: "), "{text}");
        }
        other => panic!("expected the Copilot HTTP text, got {other:?}"),
    }
    assert_eq!(requests(&server, "POST", &chat).await.len(), 1);
}

/// K09 (RF-16): the Messages route's one provider loop (omp
/// `PROVIDER_MAX_RETRIES = 10`, client retries 0) — ten 503s then a stream →
/// 11 POSTs and the text; eleven 503s → the 11th failure surfaces after
/// exactly 11 POSTs; a 408 and a 429 are retried; a 429 hint over 60 s, a 400
/// and a usage-limit 429 are final after 1 POST.
#[tokio::test]
#[serial]
async fn k09_messages_provider_loop_retries_ten_times_at_most() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let token = "cop-identity-k09-provider-loop";
    common::login(&server, token, ENDPOINT_A).await;
    let messages = messages_path(ROOT_A);
    let provider = build(MESSAGES_MODEL);
    let busy = || json_error(503, json!({"error":{"message":format!("busy {token}")}}));

    // Ten 503s, then a stream → ten re-issues, 11 POSTs.
    server.reset().await;
    let mut replies: Vec<_> = (0..10).map(|_| busy()).collect();
    replies.push(messages_sse("ok"));
    script(&server, &messages, replies).await;
    assert_eq!(ok_text(turn(&provider, MESSAGES_MODEL).await), "ok");
    assert_eq!(requests(&server, "POST", &messages).await.len(), 11);

    // Eleven 503s → the budget is spent: the last 503 surfaces, 11 POSTs.
    server.reset().await;
    let mut replies: Vec<_> = (0..11).map(|_| busy()).collect();
    replies.push(messages_sse("never"));
    script(&server, &messages, replies).await;
    let error = turn(&provider, MESSAGES_MODEL)
        .await
        .expect_err("ten retries at most");
    assert_clean(&error, token);
    match &error {
        ProviderError::ConnectionFailed(text) => {
            assert!(
                text.starts_with("GitHub Copilot server error 503: "),
                "{text}"
            );
        }
        other => panic!("expected the Copilot server error, got {other:?}"),
    }
    assert_eq!(requests(&server, "POST", &messages).await.len(), 11);

    // 408, then 429 with a short hint, then a stream → 3 POSTs.
    server.reset().await;
    script(
        &server,
        &messages,
        vec![
            json_error(408, json!({"error":{"message":"request timeout"}})),
            json_error(429, json!({"error":{"message":"slow down"}}))
                .insert_header("retry-after", "1"),
            messages_sse("ok"),
        ],
    )
    .await;
    assert_eq!(ok_text(turn(&provider, MESSAGES_MODEL).await), "ok");
    assert_eq!(requests(&server, "POST", &messages).await.len(), 3);

    // Final after 1 POST: a hint over 60 s, a 400, a usage-limit 429.
    for (reply, what) in [
        (
            json_error(429, json!({"error":{"message":"slow down"}}))
                .insert_header("retry-after", "61"),
            "a hint over 60 s",
        ),
        (
            json_error(400, json!({"error":{"message":"bad request"}})),
            "a 400",
        ),
        (
            json_error(
                429,
                json!({"error":{"message":"You have exceeded your monthly quota exceeded allowance"}}),
            ),
            "a usage limit",
        ),
    ] {
        server.reset().await;
        script(&server, &messages, vec![reply, messages_sse("ok")]).await;
        turn(&provider, MESSAGES_MODEL).await.expect_err(what);
        assert_eq!(
            requests(&server, "POST", &messages).await.len(),
            1,
            "{what} is final"
        );
    }
}

// ── K09: stream resilience ──────────────────────────────────────────────────

fn chat_empty() -> ResponseTemplate {
    sse(&[
        data(
            json!({"object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":0,"total_tokens":3}}),
        ),
        "data: [DONE]\n\n".to_owned(),
    ])
}

fn messages_empty() -> ResponseTemplate {
    sse(&[
        event(
            "message_start",
            json!({"type":"message_start","message":{"id":"msg_empty","type":"message","role":"assistant","model":"m","content":[],"stop_reason":null,"usage":{"input_tokens":3,"output_tokens":0}}}),
        ),
        event(
            "message_delta",
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":0}}),
        ),
        event("message_stop", json!({"type":"message_stop"})),
    ])
}

fn assert_empty_turn(chunks: &[StreamChunk], route: &str) {
    assert_eq!(text_of(chunks), "", "{route}: no text");
    assert!(errors(chunks).is_empty(), "{route}: {chunks:?}");
    assert!(
        chunks.iter().any(|chunk| matches!(
            chunk,
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn
            }
        )),
        "{route}: the empty turn completes: {chunks:?}"
    );
}

/// K09 (M09): Chat/Messages stream resilience (H8b) — EOF before content →
/// one re-POST then success; text then EOF → an error chunk and no re-POST;
/// empty completions → 3 POSTs then the empty turn; a Messages transient
/// envelope error before the first token → a re-POST.
#[tokio::test]
#[serial]
async fn k09_stream_resilience_reissues_only_before_content() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let token = "cop-identity-k09-resilience";
    common::login(&server, token, ENDPOINT_A).await;
    let chat = chat_path(ROOT_A);
    let messages = messages_path(ROOT_A);
    let chat_provider = build(CHAT_MODEL);
    let messages_provider = build(MESSAGES_MODEL);

    // Chat: a stream that ends before any content → one re-POST, success.
    server.reset().await;
    script(
        &server,
        &chat,
        vec![sse(&["data: [DONE]\n\n".to_owned()]), chat_sse("ok")],
    )
    .await;
    let chunks = turn(&chat_provider, CHAT_MODEL).await.unwrap();
    assert_eq!(text_of(&chunks), "ok");
    assert!(errors(&chunks).is_empty(), "{chunks:?}");
    assert_eq!(requests(&server, "POST", &chat).await.len(), 2);

    // Chat: text then EOF without finish_reason or [DONE] → error, no re-POST.
    server.reset().await;
    script(
        &server,
        &chat,
        vec![
            sse(&[data(
                json!({"object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":"partial"},"finish_reason":null}]}),
            )]),
            chat_sse("ok"),
        ],
    )
    .await;
    let chunks = turn(&chat_provider, CHAT_MODEL).await.unwrap();
    assert_eq!(text_of(&chunks), "partial");
    assert_eq!(errors(&chunks), [TRUNCATED]);
    assert!(
        matches!(chunks.last(), Some(StreamChunk::Error { .. })),
        "{chunks:?}"
    );
    assert_eq!(requests(&server, "POST", &chat).await.len(), 1);

    // Chat: empty completions → 3 POSTs, then the empty turn.
    server.reset().await;
    script(
        &server,
        &chat,
        vec![chat_empty(), chat_empty(), chat_empty(), chat_sse("late")],
    )
    .await;
    let chunks = turn(&chat_provider, CHAT_MODEL).await.unwrap();
    assert_empty_turn(&chunks, "chat");
    assert_eq!(requests(&server, "POST", &chat).await.len(), 3);

    // Messages: empty completions → 3 POSTs, then the empty turn.
    server.reset().await;
    script(
        &server,
        &messages,
        vec![
            messages_empty(),
            messages_empty(),
            messages_empty(),
            messages_sse("late"),
        ],
    )
    .await;
    let chunks = turn(&messages_provider, MESSAGES_MODEL).await.unwrap();
    assert_empty_turn(&chunks, "messages");
    assert_eq!(requests(&server, "POST", &messages).await.len(), 3);

    // Messages: a transient error envelope before the first token → re-POST.
    server.reset().await;
    script(
        &server,
        &messages,
        vec![
            sse(&[event(
                "error",
                json!({"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}),
            )]),
            messages_sse("ok"),
        ],
    )
    .await;
    let chunks = turn(&messages_provider, MESSAGES_MODEL).await.unwrap();
    assert_eq!(text_of(&chunks), "ok");
    assert!(errors(&chunks).is_empty(), "{chunks:?}");
    assert_eq!(requests(&server, "POST", &messages).await.len(), 2);
    for chunk in &chunks {
        assert!(!format!("{chunk:?}").contains(token));
    }
}
