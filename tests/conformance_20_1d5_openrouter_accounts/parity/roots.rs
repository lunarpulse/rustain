//! K17 (AC18): per-account Copilot roots through `init_provider_layer` +
//! `refresh_target` and the served adapter's `stream_completion`. One root per
//! attempt set: a re-probe never moves another account's requests nor an
//! in-flight request's re-issue, and a 401 is final on the attempt set's root.
//! Every stored row comes from a real `flows::login` against the loopback stub.

use crate::conformance_20_1d_codex_subscription::boundaries::{
    RawRequest, raw_listener, read_request,
};
use crate::conformance_20_1d3_copilot::catalog::{
    app_config, live_payload, models_path, refresh, target,
};
use crate::conformance_20_1d3_copilot::common::{
    ENDPOINT_A, GUARD, PROBE_PATH, PROVIDER, ROOT_A, World, build, complete, header, json_reply,
    login, login_with, messages_path, mount_routes, options, probe_reply, reply, requests,
    responses_path, text_of, user,
};
use rustain::domain::errors::ProviderError;
use rustain::domain::models::StreamChunk;
use rustain::domain::models::provider::DiscoveryStatus;
use rustain::domain::ports::StreamingProvider;
use rustain::infrastructure::startup::init_provider_layer;
use serde_json::{Value, json};
use serial_test::serial;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

/// A Responses-route model and a Messages-route model.
const RESPONSES_MODEL: &str = "gpt-5.5";
const MESSAGES_MODEL: &str = "claude-sonnet-4.5";
/// The canonical root as the rebased requests see it (no path).
const CANONICAL: &str = "";

fn probe_down() -> ResponseTemplate {
    json_reply(500, json!({"message": "probe down"}))
}

/// The next connection the raw stub accepts and its complete request.
async fn next(listener: &TcpListener) -> (TcpStream, RawRequest) {
    let (mut socket, _) = tokio::time::timeout(GUARD, listener.accept())
        .await
        .expect("the client connects")
        .unwrap();
    let request = read_request(&mut socket).await;
    (socket, request)
}

/// A complete response on a raw connection, which then closes.
async fn answer(socket: &mut TcpStream, status: &str, content_type: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(response.as_bytes()).await.unwrap();
    socket.flush().await.unwrap();
}

/// An Anthropic Messages event stream answering `text`.
fn messages_body(text: &str) -> String {
    let frame = |event: &str, data: Value| format!("event: {event}\ndata: {data}\n\n");
    [
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
    ]
    .concat()
}

/// One Messages completion on its own task, so the stub can hold its POSTs.
fn spawn_messages(
    provider: &Arc<dyn StreamingProvider>,
) -> JoinHandle<Result<Vec<StreamChunk>, ProviderError>> {
    let provider = provider.clone();
    tokio::spawn(async move {
        complete(
            &provider,
            vec![user("hi")],
            options(MESSAGES_MODEL, 1024, None),
        )
        .await
    })
}

async fn finished(
    task: JoinHandle<Result<Vec<StreamChunk>, ProviderError>>,
) -> Result<Vec<StreamChunk>, ProviderError> {
    tokio::time::timeout(GUARD, task)
        .await
        .expect("the completion finishes")
        .expect("the completion task joins")
}

/// K17(a) (M17): two canonical-root accounts on one entry. Discovery re-probes
/// A's scope to a tenant root and A's next request goes there; B's next request
/// goes to B's own (canonical) root, never A's.
#[tokio::test]
#[serial]
async fn k17a_a_reprobed_account_root_never_serves_another_account() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    login_with(&server, "", "t-root-a", probe_down()).await;
    reply(
        &server,
        "GET",
        &models_path(ROOT_A),
        json_reply(200, live_payload()),
    )
    .await;
    mount_routes(&server, ROOT_A, "tenant-a").await;
    mount_routes(&server, CANONICAL, "canonical").await;

    let layer = init_provider_layer(&app_config());
    let provider = target(&layer, PROVIDER).provider.clone();
    let scope_a = provider.discovery_scope(PROVIDER).await;
    assert!(scope_a.is_some());
    {
        let _probe = Mock::given(method("GET"))
            .and(path(PROBE_PATH))
            .respond_with(probe_reply(ENDPOINT_A))
            .mount_as_scoped(&server)
            .await;
        assert_eq!(refresh(&layer, PROVIDER).await.status, DiscoveryStatus::Ok);
    }
    let chunks = complete(
        &provider,
        vec![user("a")],
        options(RESPONSES_MODEL, 1024, None),
    )
    .await
    .expect("account A answers");
    assert_eq!(text_of(&chunks), "tenant-a", "A uses its re-probed root");

    // Account B: another canonical-root login on the same entry, never re-probed.
    login_with(&server, "", "t-root-b", probe_down()).await;
    assert_ne!(
        provider.discovery_scope(PROVIDER).await,
        scope_a,
        "B is another credential scope"
    );
    let chunks = complete(
        &provider,
        vec![user("b")],
        options(RESPONSES_MODEL, 1024, None),
    )
    .await
    .expect("account B answers");
    assert_eq!(text_of(&chunks), "canonical", "B never uses A's root");

    let canonical = requests(&server, "POST", &responses_path(CANONICAL)).await;
    assert_eq!(canonical.len(), 1);
    assert_eq!(
        header(&canonical[0], "authorization").as_deref(),
        Some("Bearer t-root-b")
    );
    let tenant = requests(&server, "POST", &responses_path(ROOT_A)).await;
    assert_eq!(tenant.len(), 1);
    assert_eq!(
        header(&tenant[0], "authorization").as_deref(),
        Some("Bearer t-root-a")
    );
}

/// K17(b): without sleeps, the raw stub holds POST1 of a Messages request
/// while `refresh_target` re-probes the same scope to a tenant root; POST1 then
/// answers 503 and the re-issue goes to the original root. The next request
/// (positive control) uses the re-probed root.
#[tokio::test]
#[serial]
async fn k17b_a_reissue_keeps_its_attempt_sets_root_across_a_concurrent_reprobe() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    login_with(&server, "", "t-held", probe_down()).await;
    let layer = init_provider_layer(&app_config());
    let provider = target(&layer, PROVIDER).provider.clone();
    // Every derived Copilot URL now reaches the raw stub.
    let (listener, origin) = raw_listener().await;
    world.set("RUSTAIN_TEST_AUTH_ORIGIN", &origin);

    let held = spawn_messages(&provider);
    let (mut post1, request) = next(&listener).await;
    assert_eq!(request.method, "POST");
    assert_eq!(request.target, messages_path(CANONICAL));
    assert_eq!(
        request.headers.get("authorization").map(String::as_str),
        Some("Bearer t-held")
    );

    // The same scope is re-probed to a tenant root while POST1 is held.
    let (report, ()) = tokio::join!(refresh(&layer, PROVIDER), async {
        let (mut probe, request) = next(&listener).await;
        assert_eq!(request.target, PROBE_PATH);
        let body = json!({"login": "octocat", "endpoints": {"api": ENDPOINT_A}});
        answer(&mut probe, "200 OK", "application/json", &body.to_string()).await;
        let (mut models, request) = next(&listener).await;
        assert_eq!(request.target, models_path(ROOT_A));
        answer(
            &mut models,
            "200 OK",
            "application/json",
            &live_payload().to_string(),
        )
        .await;
    });
    assert_eq!(report.status, DiscoveryStatus::Ok);

    answer(
        &mut post1,
        "503 Service Unavailable",
        "application/json",
        r#"{"error":{"message":"overloaded"}}"#,
    )
    .await;
    drop(post1);
    let (mut post2, request) = next(&listener).await;
    assert_eq!(
        request.target,
        messages_path(CANONICAL),
        "the re-issue keeps its attempt set's root"
    );
    answer(
        &mut post2,
        "200 OK",
        "text/event-stream",
        &messages_body("held"),
    )
    .await;
    let chunks = finished(held).await.expect("the re-issue answers");
    assert_eq!(text_of(&chunks), "held");

    // Positive control: the next logical request takes the re-probed root.
    let next_request = spawn_messages(&provider);
    let (mut post3, request) = next(&listener).await;
    assert_eq!(request.target, messages_path(ROOT_A));
    answer(
        &mut post3,
        "200 OK",
        "text/event-stream",
        &messages_body("tenant"),
    )
    .await;
    let chunks = finished(next_request)
        .await
        .expect("the tenant root answers");
    assert_eq!(text_of(&chunks), "tenant");
}

/// K17(c): a Copilot 401 is final — exactly one POST, to the attempt set's
/// root, and `AuthenticationFailed`.
#[tokio::test]
#[serial]
async fn k17c_a_copilot_401_is_final_with_one_post_to_the_attempt_sets_root() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    login(&server, "t-final", ENDPOINT_A).await;
    reply(
        &server,
        "POST",
        &messages_path(ROOT_A),
        json_reply(401, json!({"message": "Bad credentials"})),
    )
    .await;
    let provider = build(MESSAGES_MODEL);
    let error = complete(
        &provider,
        vec![user("hi")],
        options(MESSAGES_MODEL, 1024, None),
    )
    .await
    .expect_err("a Copilot 401 is final");
    assert!(
        matches!(error, ProviderError::AuthenticationFailed),
        "{error:?}"
    );
    let posts: Vec<String> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method.as_str() == "POST" && r.url.path().ends_with("/v1/messages"))
        .map(|r| r.url.path().to_owned())
        .collect();
    assert_eq!(posts, [messages_path(ROOT_A)]);
}
