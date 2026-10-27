//! Runtime boundaries exercised through login, the factory, and actual HTTP streams.
use super::auth::{jwt, login_oauth};
use futures::{FutureExt, StreamExt};
use rustain::{
    adapters::auth_store::FileAuthStore,
    domain::{
        errors::ProviderError,
        models::{
            CompletionOptions, Credential, Message, MessageRole, ProviderConfig, StreamChunk,
        },
        ports::{AuthStorePort, StreamingProvider},
    },
    infrastructure::provider_factory::build_provider_for_config,
};
use serde_json::{Value, json};
use serial_test::serial;
use std::{
    collections::HashMap,
    ffi::OsString,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
};
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{method, path},
};

const NOW: i64 = 1_800_000_000_000;
const RESPONSES: &str = "/backend-api/codex/responses";
const MODELS: &str = "/backend-api/codex/models";
const COMPLETE: &str =
    "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n";

struct Env(Vec<(&'static str, Option<OsString>)>);
impl Env {
    fn new(dir: &std::path::Path, origin: &str) -> Self {
        let keys = [
            "RUSTAIN_DATA_DIR",
            "RUSTAIN_TEST_AUTH_ORIGIN",
            "RUSTAIN_TEST_AUTH_CALLBACK_PORT",
            "RUSTAIN_TEST_RESPONSES_RETRY_SCALE",
            "RUSTAIN_TEST_NOW_MS",
            "OPENAI_CODEX_OAUTH_TOKEN",
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
        ];
        let guard = Self(
            keys.iter()
                .map(|key| (*key, std::env::var_os(key)))
                .collect(),
        );
        // SAFETY: every caller holds the unkeyed serial environment lock.
        unsafe {
            for key in keys {
                std::env::remove_var(key);
            }
            std::env::set_var("RUSTAIN_DATA_DIR", dir);
            std::env::set_var("RUSTAIN_TEST_AUTH_ORIGIN", origin);
            std::env::set_var("RUSTAIN_TEST_RESPONSES_RETRY_SCALE", "0");
            std::env::set_var("RUSTAIN_TEST_NOW_MS", NOW.to_string());
            let callback = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            std::env::set_var(
                "RUSTAIN_TEST_AUTH_CALLBACK_PORT",
                callback.local_addr().unwrap().port().to_string(),
            );
        }
        guard
    }
    fn set(key: &str, value: &str) {
        // SAFETY: the caller holds the same serial lock as Env::new.
        unsafe {
            std::env::set_var(key, value);
        }
    }
}
impl Drop for Env {
    fn drop(&mut self) {
        for (key, value) in self.0.drain(..) {
            // SAFETY: the serial lock outlives restoration.
            unsafe {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}
fn access(account: &str, residency: &str) -> String {
    jwt(json!({
        "https://api.openai.com/auth": {"chatgpt_account_id":account,"chatgpt_data_residency":residency,"chatgpt_plan_type":"plus"},
        "https://api.openai.com/profile": {"email":"boundary@example.test"},
    }))
}
fn config() -> ProviderConfig {
    ProviderConfig {
        provider_id: "openai-codex".into(),
        kind: Some("openai-codex".into()),
        model_id: "gpt-5.6-luna".into(),
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
fn options(id: &str) -> CompletionOptions {
    CompletionOptions {
        model: "gpt-5.6-luna".into(),
        max_tokens: 4096,
        system_prompt: String::new(),
        temperature: None,
        tools: vec![],
        session_id: Some(id.into()),
    }
}
fn user() -> Message {
    Message {
        role: MessageRole::User,
        content: "boundary proof".into(),
        images: vec![],
        tool_results: vec![],
        tool_uses: vec![],
        context_prefix: None,
        reasoning_content: None,
        provider_replay: vec![],
    }
}
async fn login(server: &MockServer, token: &str) -> Arc<dyn AuthStorePort> {
    let store: Arc<dyn AuthStorePort> = Arc::new(FileAuthStore::new());
    login_oauth(
        server,
        &store,
        NOW,
        json!({
            "access_token":token,"refresh_token":"INITIAL-BOUNDARY-REFRESH","expires_in":3600,
        }),
    )
    .await;
    store
}
fn build() -> Arc<dyn StreamingProvider> {
    build_provider_for_config("openai-codex", &config()).unwrap()
}
async fn collect(provider: &Arc<dyn StreamingProvider>, id: &str) -> Vec<StreamChunk> {
    provider
        .stream_completion(vec![user()], options(id))
        .await
        .unwrap()
        .collect()
        .await
}
fn frames(events: &[Value]) -> String {
    events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect()
}
fn response_count(requests: &[Request]) -> usize {
    requests
        .iter()
        .filter(|request| request.url.path() == RESPONSES)
        .count()
}
fn assert_no_chat(requests: &[Request]) {
    assert!(
        requests
            .iter()
            .all(|request| !request.url.path().contains("chat/completions")),
        "Codex must never fall back to Chat Completions"
    );
}
async fn mount_stream(server: &MockServer, body: String) {
    Mock::given(method("POST"))
        .and(path(RESPONSES))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(server)
        .await;
}
async fn raw_listener() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    (listener, origin)
}

/// Read a real request completely before the fixture writes or closes its response.
struct RawRequest {
    method: String,
    target: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}
async fn read_request(socket: &mut TcpStream) -> RawRequest {
    let mut bytes = Vec::new();
    let header_end = loop {
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
        let mut buffer = [0; 4096];
        let count = socket.read(&mut buffer).await.unwrap();
        assert_ne!(count, 0, "client closed before request headers");
        bytes.extend_from_slice(&buffer[..count]);
    };
    let head = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
    let mut lines = head.split("\r\n");
    let mut start = lines.next().unwrap().split_whitespace();
    let method = start.next().unwrap().to_owned();
    let target = start.next().unwrap().to_owned();
    assert!(
        !target.contains("chat/completions"),
        "no Chat request on raw fixture"
    );
    let headers: HashMap<_, _> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let length: usize = headers
        .get("content-length")
        .map_or(0, |value| value.parse().unwrap());
    while bytes.len() < header_end + length {
        let mut buffer = [0; 4096];
        let count = socket.read(&mut buffer).await.unwrap();
        assert_ne!(count, 0, "client closed before request body");
        bytes.extend_from_slice(&buffer[..count]);
    }
    RawRequest {
        method,
        target,
        headers,
        body: bytes[header_end..header_end + length].to_vec(),
    }
}
async fn stream_headers(socket: &mut TcpStream) {
    socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").await.unwrap();
}
async fn write_chunk(socket: &mut TcpStream, bytes: &[u8]) {
    socket
        .write_all(format!("{:x}\r\n", bytes.len()).as_bytes())
        .await
        .unwrap();
    socket.write_all(bytes).await.unwrap();
    socket.write_all(b"\r\n").await.unwrap();
    socket.flush().await.unwrap();
}
async fn finish_stream(socket: &mut TcpStream, body: &str) {
    write_chunk(socket, body.as_bytes()).await;
    socket.write_all(b"0\r\n\r\n").await.unwrap();
}
async fn assert_peer_eof(socket: &mut TcpStream) {
    let mut byte = [0];
    let read = tokio::time::timeout(Duration::from_secs(5), socket.read(&mut byte))
        .await
        .expect("server must observe EOF promptly after consumer cancellation");
    assert_eq!(
        read.unwrap(),
        0,
        "dropping the consumer must close the response connection"
    );
}
fn assert_no_connection(listener: &TcpListener) {
    assert!(
        listener.accept().now_or_never().is_none(),
        "cancelled operation must not open another connection"
    );
}

#[tokio::test]
#[serial]
async fn k05_four_byte_utf8_survives_structurally_split_http_chunks() {
    let auth = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let _env = Env::new(dir.path(), &auth.uri());
    login(&auth, &access("utf8-account", "eu")).await;
    let (listener, origin) = raw_listener().await;
    Env::set("RUSTAIN_TEST_AUTH_ORIGIN", &origin);
    let provider = build();
    let (continue_tx, continue_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_request(&mut socket).await;
        assert_eq!(
            (request.method.as_str(), request.target.as_str()),
            ("POST", RESPONSES)
        );
        assert_eq!(request.headers["chatgpt-account-id"], "utf8-account");
        assert_eq!(
            serde_json::from_slice::<Value>(&request.body).unwrap()["model"],
            "gpt-5.6-luna"
        );
        stream_headers(&mut socket).await;
        let emoji_frame = frames(&[json!({"type":"response.output_text.delta","delta":"🦀"})]);
        let split = emoji_frame
            .as_bytes()
            .windows(4)
            .position(|bytes| bytes == "🦀".as_bytes())
            .unwrap()
            + 2;
        let mut first =
            frames(&[json!({"type":"response.output_text.delta","delta":"prefix "})]).into_bytes();
        first.extend_from_slice(&emoji_frame.as_bytes()[..split]);
        // Chunk framing preserves the byte boundary even if TCP coalesces writes.
        write_chunk(&mut socket, &first).await;
        continue_rx.await.unwrap();
        let mut last = emoji_frame.as_bytes()[split..].to_vec();
        last.extend_from_slice(COMPLETE.as_bytes());
        write_chunk(&mut socket, &last).await;
        socket.write_all(b"0\r\n\r\n").await.unwrap();
        listener
    });
    let mut stream = provider
        .stream_completion(vec![user()], options("utf8"))
        .await
        .unwrap();
    // Seeing this event proves the first body chunk (including the incomplete codepoint) was consumed.
    assert!(
        matches!(stream.next().await, Some(StreamChunk::Text { content, .. }) if content == "prefix ")
    );
    continue_tx.send(()).unwrap();
    let rest: Vec<_> = stream.collect().await;
    let text: String = rest
        .iter()
        .filter_map(|chunk| match chunk {
            StreamChunk::Text { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "🦀");
    assert!(matches!(
        rest.last(),
        Some(StreamChunk::TurnComplete { .. })
    ));
    assert_no_connection(&task.await.unwrap());
}

#[tokio::test]
#[serial]
async fn k09_drop_after_first_frame_observes_server_eof_and_no_retry_connection() {
    let auth = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let _env = Env::new(dir.path(), &auth.uri());
    login(&auth, &access("cancel-account", "eu")).await;
    let (listener, origin) = raw_listener().await;
    Env::set("RUSTAIN_TEST_AUTH_ORIGIN", &origin);
    let provider = build();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_request(&mut socket).await;
        assert_eq!(request.target, RESPONSES);
        stream_headers(&mut socket).await;
        write_chunk(
            &mut socket,
            frames(&[json!({"type":"response.output_text.delta","delta":"first frame"})])
                .as_bytes(),
        )
        .await;
        assert_peer_eof(&mut socket).await;
        listener
    });
    let mut stream = provider
        .stream_completion(vec![user()], options("cancel-stream"))
        .await
        .unwrap();
    assert!(
        matches!(stream.next().await, Some(StreamChunk::Text { content, .. }) if content == "first frame")
    );
    drop(stream);
    let listener = task.await.unwrap();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(86_400)).await;
    assert_no_connection(&listener);
    tokio::time::resume();
    assert_no_chat(&auth.received_requests().await.unwrap());
}

#[tokio::test]
#[serial]
async fn rf06_abort_during_in_band_retry_backoff_completes_as_cancelled() {
    assert_abort_during_in_band_retry(false).await;
}

#[tokio::test]
#[serial]
async fn rf06_abort_during_in_band_retry_reopen_completes_as_cancelled() {
    assert_abort_during_in_band_retry(true).await;
}

async fn assert_abort_during_in_band_retry(reopen: bool) {
    use rustain::domain::models::StopReason;

    let auth = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let _env = Env::new(dir.path(), &auth.uri());
    login(&auth, &access("retry-abort-account", "eu")).await;
    let (listener, origin) = raw_listener().await;
    Env::set("RUSTAIN_TEST_AUTH_ORIGIN", &origin);
    // Env normally disables retry delays. This proof requires the real 500 ms wait.
    Env::set("RUSTAIN_TEST_RESPONSES_RETRY_SCALE", "1");
    let provider = build();
    let (release_tx, release_rx) = oneshot::channel();
    let mut first = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_request(&mut socket).await;
        assert_eq!(
            (request.method.as_str(), request.target.as_str()),
            ("POST", RESPONSES)
        );
        assert_eq!(request.headers["chatgpt-account-id"], "retry-abort-account");
        stream_headers(&mut socket).await;
        release_rx.await.unwrap();
        write_chunk(
            &mut socket,
            frames(&[json!({
                "type":"response.failed",
                "response":{"error":{"code":"server_error","message":"retry this uncommitted turn"}}
            })])
            .as_bytes(),
        )
        .await;
        // Do not send EOF or a terminal event: only consuming the retryable error
        // can make the adapter drop this live response and enter its retry wait.
        assert_peer_eof(&mut socket).await;
        listener
    });
    let mut stream = provider
        .stream_completion(vec![user()], options("in-band-retry-abort"))
        .await
        .unwrap();
    tokio::time::pause();
    let started = tokio::time::Instant::now();
    release_tx.send(()).unwrap();
    let listener = loop {
        assert!(
            stream.next().now_or_never().is_none(),
            "the uncommitted error must enter backoff without emitting a chunk"
        );
        if let Some(result) = (&mut first).now_or_never() {
            break result.unwrap();
        }
        // Keep runnable work while waiting for the structural peer-EOF gate:
        // paused time must not auto-advance over the very wait being tested.
        tokio::task::yield_now().await;
    };
    assert_eq!(tokio::time::Instant::now(), started);
    assert_no_connection(&listener);
    tokio::time::advance(Duration::from_millis(250)).await;
    assert!(
        stream.next().now_or_never().is_none(),
        "the normal first retry delay must still be pending halfway through"
    );
    assert_no_connection(&listener);

    let mut reopened_socket = None;
    let listener = if reopen {
        let mut second = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_request(&mut socket).await;
            assert_eq!(
                (request.method.as_str(), request.target.as_str()),
                ("POST", RESPONSES)
            );
            // Withhold HTTP headers so the retry is structurally inside open().
            (listener, socket)
        });
        tokio::time::advance(Duration::from_millis(251)).await;
        let (listener, socket) = loop {
            assert!(
                stream.next().now_or_never().is_none(),
                "reopen must await the gated HTTP response"
            );
            if let Some(result) = (&mut second).now_or_never() {
                break result.unwrap();
            }
            tokio::task::yield_now().await;
        };
        reopened_socket = Some(socket);
        listener
    } else {
        listener
    };

    // abort(), not dropping the stream, cancels the actual factory-built consumer.
    provider.abort().await.unwrap();
    let terminal = stream
        .next()
        .now_or_never()
        .expect("abort must wake the pending retry without advancing its timer");
    assert!(
        matches!(
            terminal,
            Some(StreamChunk::TurnComplete {
                stop_reason: StopReason::Cancelled
            })
        ),
        "retry cancellation must emit Cancelled, never StreamChunk::Error: {terminal:?}"
    );
    assert!(matches!(stream.next().now_or_never(), Some(None)));
    tokio::time::resume();
    if let Some(mut socket) = reopened_socket {
        assert_peer_eof(&mut socket).await;
    }
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(86_400)).await;
    assert_no_connection(&listener);
    tokio::time::resume();
    assert_no_chat(&auth.received_requests().await.unwrap());
}

#[tokio::test]
#[serial]
async fn k09_cancel_gated_401_recovery_still_persists_the_rotated_refresh_token() {
    let auth = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let _env = Env::new(dir.path(), &auth.uri());
    let store = login(&auth, &access("original-account", "eu")).await;
    let before = store.snapshot().await.unwrap().rows("openai-codex")[0].clone();
    let next_access = access("refreshed-account", "us");
    let expected_access = next_access.clone();
    let (listener, origin) = raw_listener().await;
    Env::set("RUSTAIN_TEST_AUTH_ORIGIN", &origin);
    let provider = build();
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut request_socket, _) = listener.accept().await.unwrap();
        let rejected = read_request(&mut request_socket).await;
        assert_eq!(rejected.target, RESPONSES);
        request_socket
            .write_all(
                b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        drop(request_socket);
        let (mut refresh_socket, _) = listener.accept().await.unwrap();
        let refresh = read_request(&mut refresh_socket).await;
        assert_eq!(
            (refresh.method.as_str(), refresh.target.as_str()),
            ("POST", "/oauth/token")
        );
        assert_eq!(
            refresh.headers["content-type"],
            "application/x-www-form-urlencoded"
        );
        let fields: HashMap<_, _> = url::form_urlencoded::parse(&refresh.body)
            .into_owned()
            .collect();
        assert_eq!(fields.len(), 3);
        assert_eq!(fields["grant_type"], "refresh_token");
        assert_eq!(fields["client_id"], "app_EMoamEEZ73f0CkXaXp7hrann");
        assert_eq!(fields["refresh_token"], "INITIAL-BOUNDARY-REFRESH");
        entered_tx.send(()).unwrap();
        release_rx.await.unwrap();
        let body = json!({"access_token":next_access,"refresh_token":"ROTATED-AFTER-CANCELLATION","expires_in":3600}).to_string();
        refresh_socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).as_bytes()).await.unwrap();
        listener
    });
    let request = tokio::spawn(async move {
        provider
            .stream_completion(vec![user()], options("cancel-refresh"))
            .await
    });
    entered_rx.await.unwrap();
    request.abort();
    match request.await {
        Err(error) => assert!(error.is_cancelled()),
        Ok(_) => panic!("the gated caller must be cancelled"),
    }
    release_tx.send(()).unwrap();
    let listener = server.await.unwrap();
    // The refresh already holds this real cross-process lease. Acquiring it is a
    // persistence completion fence, not polling the store or sleeping for a race.
    let lease = store.refresh_lease().await.unwrap();
    let snapshot = store.snapshot().await.unwrap();
    let rows = snapshot.rows("openai-codex");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, before.id);
    assert!(rows[0].rev > before.rev);
    let Credential::OAuth(oauth) = &rows[0].credential else {
        panic!("OAuth row required")
    };
    assert_eq!(oauth.access.expose_secret(), expected_access);
    assert_eq!(oauth.refresh.expose_secret(), "ROTATED-AFTER-CANCELLATION");
    assert_eq!(oauth.identity.org_id.as_deref(), Some("original-account"));
    assert_eq!(oauth.identity.org_name.as_deref(), Some("plus"));
    assert_eq!(oauth.expires_at_ms, NOW + 3_600_000);
    drop(lease);
    assert_no_connection(&listener);
    assert_eq!(
        auth.received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path() == "/oauth/token")
            .count(),
        1
    );
}

#[tokio::test]
#[serial]
async fn status_only_models_get_maps_health_and_probe_without_chat_or_discovery() {
    for probe in [false, true] {
        for status in [200, 401, 403, 404, 405] {
            let server = MockServer::start().await;
            let dir = tempfile::tempdir().unwrap();
            let _env = Env::new(dir.path(), &server.uri());
            let token = access("probe-account", "eu");
            login(&server, &token).await;
            let provider = build();
            let refreshed = access("probe-rotated-account", "eu");
            Mock::given(method("POST")).and(path("/oauth/token"))
                .and(|request: &Request| String::from_utf8_lossy(&request.body).contains("grant_type=refresh_token"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token":refreshed,"refresh_token":"PROBE-ROTATED","expires_in":3600})))
                .with_priority(1).mount(&server).await;
            // Deliberately invalid JSON: neither front door may try to parse a roster.
            Mock::given(method("GET"))
                .and(path(MODELS))
                .respond_with(
                    ResponseTemplate::new(status).set_body_string("not a model roster: {"),
                )
                .mount(&server)
                .await;
            let result = if probe {
                provider.connectivity_probe().await.map(|_| ())
            } else {
                provider.health_check().await
            };
            match status {
                200 => assert!(result.is_ok(), "probe={probe}"),
                401 | 403 => assert!(
                    matches!(result, Err(ProviderError::AuthenticationFailed)),
                    "status={status}, probe={probe}"
                ),
                404 | 405 if probe => assert!(
                    matches!(result, Err(ProviderError::Other(_))),
                    "probe must use its port mapping"
                ),
                404 | 405 => assert!(
                    matches!(result, Err(ProviderError::EndpointUnsupported(code)) if code == status)
                ),
                _ => unreachable!(),
            }
            let requests = server.received_requests().await.unwrap();
            let gets: Vec<_> = requests
                .iter()
                .filter(|request| request.method == "GET")
                .collect();
            assert_eq!(
                gets.len(),
                if status == 401 { 2 } else { 1 },
                "exact stored-row 401 recovery boundary"
            );
            for request in &gets {
                assert_eq!(request.url.path(), MODELS);
                assert_eq!(request.url.query(), Some("client_version=0.159.0"));
                assert_eq!(request.headers["accept"], "application/json");
                assert_eq!(request.headers["openai-beta"], "responses=experimental");
                assert_eq!(request.headers["originator"], "omp");
                assert_eq!(request.headers["version"], "0.159.0");
                // omp's discovery header set carries no residency claim.
                assert!(
                    !request
                        .headers
                        .contains_key("x-openai-internal-codex-residency")
                );
                assert!(request.body.is_empty());
            }
            assert_eq!(gets[0].headers["authorization"], format!("Bearer {token}"));
            assert_eq!(gets[0].headers["chatgpt-account-id"], "probe-account");
            assert_eq!(response_count(&requests), 0);
            assert_eq!(
                requests
                    .iter()
                    .filter(|r| r.url.path() == "/oauth/token")
                    .count(),
                if status == 401 { 2 } else { 1 }
            );
            assert_no_chat(&requests);
        }
    }
}

#[tokio::test]
#[serial]
async fn stored_401_models_get_recovers_once_and_sends_the_new_bearer() {
    for probe in [false, true] {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let _env = Env::new(dir.path(), &server.uri());
        let first = access("probe-initial", "eu");
        let next = access("probe-refreshed", "us");
        let store = login(&server, &first).await;
        let provider = build();
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .and(|request: &Request| {
                String::from_utf8_lossy(&request.body).contains("grant_type=refresh_token")
            })
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"access_token":next,"refresh_token":"MODELS-ROTATED","expires_in":3600}),
            ))
            .with_priority(1)
            .mount(&server)
            .await;
        let bearer = format!("Bearer {next}");
        Mock::given(method("GET"))
            .and(path(MODELS))
            .respond_with(move |request: &Request| {
                ResponseTemplate::new(
                    if request
                        .headers
                        .get("authorization")
                        .is_some_and(|value| value == bearer.as_str())
                    {
                        200
                    } else {
                        401
                    },
                )
                .set_body_string("body is deliberately not JSON")
            })
            .mount(&server)
            .await;
        if probe {
            assert!(provider.connectivity_probe().await.is_ok());
        } else {
            assert!(provider.health_check().await.is_ok());
        }
        let requests = server.received_requests().await.unwrap();
        let gets: Vec<_> = requests
            .iter()
            .filter(|request| request.url.path() == MODELS)
            .collect();
        assert_eq!(gets.len(), 2);
        assert_eq!(gets[0].headers["authorization"], format!("Bearer {first}"));
        assert_eq!(gets[1].headers["authorization"], format!("Bearer {next}"));
        assert_eq!(gets[1].headers["chatgpt-account-id"], "probe-refreshed");
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.url.path() == "/oauth/token")
                .count(),
            2,
            "one login plus exactly one refresh"
        );
        let snapshot = store.snapshot().await.unwrap();
        let Credential::OAuth(oauth) = &snapshot.rows("openai-codex")[0].credential else {
            panic!("OAuth row required")
        };
        assert_eq!(oauth.refresh.expose_secret(), "MODELS-ROTATED");
        assert_eq!(oauth.identity.org_id.as_deref(), Some("probe-initial"));
        assert_eq!(response_count(&requests), 0);
        assert_no_chat(&requests);
    }
}

#[tokio::test]
#[serial]
async fn status_only_models_transport_errors_follow_each_port_contract() {
    let auth = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let _env = Env::new(dir.path(), &auth.uri());
    login(&auth, &access("offline-probe", "eu")).await;
    let (listener, origin) = raw_listener().await;
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_request(&mut socket).await;
            assert_eq!(request.method, "GET");
            assert_eq!(request.target, format!("{MODELS}?client_version=0.159.0"));
            // A clean TCP close before any HTTP status is a transport error.
            drop(socket);
        }
        listener
    });
    Env::set("RUSTAIN_TEST_AUTH_ORIGIN", &origin);
    let provider = build();
    assert!(matches!(
        provider.health_check().await,
        Err(ProviderError::Offline(_))
    ));
    assert!(matches!(
        provider.connectivity_probe().await,
        Err(ProviderError::Offline(_))
    ));
    assert_no_connection(&server.await.unwrap());
    assert_no_chat(&auth.received_requests().await.unwrap());
}

#[tokio::test]
#[serial]
async fn retry_http_dates_reset_after_and_header_precedence_reach_the_runtime_ceiling() {
    let date = chrono::DateTime::from_timestamp_millis(NOW + 300_001)
        .unwrap()
        .to_rfc2822();
    let date_over = chrono::DateTime::from_timestamp_millis(NOW + 301_000)
        .unwrap()
        .to_rfc2822();
    let date_under = chrono::DateTime::from_timestamp_millis(NOW + 60_000)
        .unwrap()
        .to_rfc2822();
    let cases = [
        (vec![("retry-after", date_under)], 6, 60_000),
        // HTTP dates have whole-second precision: exactly 300 s is allowed.
        (vec![("retry-after", date)], 6, 300_000),
        (vec![("retry-after", date_over)], 1, 301_000),
        (vec![("x-ratelimit-reset-after", "2.5".into())], 6, 2_500),
        (
            vec![("x-ratelimit-reset-after", "300.001".into())],
            1,
            300_001,
        ),
        (vec![("retry-after-ms", "300000".into())], 6, 300_000),
        (vec![("retry-after-ms", "300001".into())], 1, 300_001),
        (
            vec![
                ("retry-after-ms", "0".into()),
                ("retry-after", "400".into()),
            ],
            6,
            0,
        ),
        (
            vec![("x-ratelimit-reset-ms", (NOW + 2_500).to_string())],
            6,
            2_500,
        ),
        (
            vec![("x-ratelimit-reset", (NOW / 1000 + 301).to_string())],
            1,
            301_000,
        ),
    ];
    for (headers, expected, expected_hint) in cases {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let _env = Env::new(dir.path(), &server.uri());
        login(&server, &access("retry-hints", "eu")).await;
        let provider = build();
        let count = Arc::new(AtomicUsize::new(0));
        let observed = count.clone();
        Mock::given(method("POST"))
            .and(path(RESPONSES))
            .respond_with(move |_: &Request| {
                observed.fetch_add(1, Ordering::SeqCst);
                let mut response = ResponseTemplate::new(429).set_body_string("rate limited");
                for (key, value) in &headers {
                    response = response.insert_header(*key, value.as_str());
                }
                response
            })
            .mount(&server)
            .await;
        let result = provider
            .stream_completion(vec![user()], options("hint"))
            .await;
        assert!(
            matches!(result, Err(ProviderError::RateLimited { retry_after_ms: Some(ms) }) if ms == expected_hint)
        );
        assert_eq!(count.load(Ordering::SeqCst), expected);
        let requests = server.received_requests().await.unwrap();
        assert_eq!(response_count(&requests), expected);
        assert_no_chat(&requests);
    }
}

#[tokio::test]
#[serial]
async fn redirect_status_never_follows_or_reposts_the_bearer() {
    for status in [301, 302, 303, 307, 308] {
        let server = MockServer::start().await;
        let target = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let _env = Env::new(dir.path(), &server.uri());
        login(&server, &access("redirect-account", "eu")).await;
        let provider = build();
        Mock::given(method("POST"))
            .and(path(RESPONSES))
            .respond_with(
                ResponseTemplate::new(status)
                    .insert_header(
                        "location",
                        format!("{}/bearer-must-not-arrive", target.uri()).as_str(),
                    )
                    .set_body_string("redirect refused"),
            )
            .mount(&server)
            .await;
        let error = provider
            .stream_completion(vec![user()], options("redirect"))
            .await
            .err()
            .unwrap();
        assert!(
            matches!(&error, ProviderError::Other(message) if message == &format!("Codex HTTP {status}: redirect refused"))
        );
        assert_eq!(
            response_count(&server.received_requests().await.unwrap()),
            1
        );
        assert!(
            target.received_requests().await.unwrap().is_empty(),
            "no redirected GET or POST may leak the bearer"
        );
        assert_no_chat(&server.received_requests().await.unwrap());
    }
}

#[tokio::test]
#[serial]
async fn no_terminal_before_output_reposts_but_committed_text_makes_eof_final() {
    for committed in [false, true] {
        for done_marker in [false, true] {
            let server = MockServer::start().await;
            let dir = tempfile::tempdir().unwrap();
            let _env = Env::new(dir.path(), &server.uri());
            login(&server, &access("terminal-account", "eu")).await;
            let provider = build();
            let count = Arc::new(AtomicUsize::new(0));
            let observed = count.clone();
            Mock::given(method("POST"))
                .and(path(RESPONSES))
                .respond_with(move |_: &Request| {
                    let body = if observed.fetch_add(1, Ordering::SeqCst) > 0 {
                        COMPLETE.to_owned()
                    } else {
                        let mut body = if committed {
                            frames(&[
                                json!({"type":"response.output_text.delta","delta":"committed"}),
                            ])
                        } else {
                            frames(&[
                                json!({"type":"response.created","response":{"id":"not-terminal"}}),
                            ])
                        };
                        if done_marker {
                            body.push_str("data: [DONE]\n\n");
                        }
                        body
                    };
                    ResponseTemplate::new(200)
                        .insert_header("content-type", "text/event-stream")
                        .set_body_string(body)
                })
                .mount(&server)
                .await;
            let output = collect(&provider, "no-terminal").await;
            assert_eq!(count.load(Ordering::SeqCst), if committed { 1 } else { 2 });
            if committed {
                assert!(
                    matches!(output.first(), Some(StreamChunk::Text { content, .. }) if content == "committed")
                );
                assert!(
                    matches!(output.last(), Some(StreamChunk::Error { content }) if content == "Codex stream ended before terminal completion event")
                );
                assert!(
                    !output
                        .iter()
                        .any(|chunk| matches!(chunk, StreamChunk::TurnComplete { .. }))
                );
            } else {
                assert!(matches!(
                    output.last(),
                    Some(StreamChunk::TurnComplete { .. })
                ));
                assert!(
                    !output
                        .iter()
                        .any(|chunk| matches!(chunk, StreamChunk::Error { .. }))
                );
            }
            let requests = server.received_requests().await.unwrap();
            assert_eq!(response_count(&requests), if committed { 1 } else { 2 });
            assert_no_chat(&requests);
        }
    }
}

#[tokio::test]
#[serial]
async fn whitespace_argument_event_and_byte_guards_replay_twice_then_fail_without_tools() {
    for byte_guard in [false, true] {
        for committed in [false, true] {
            let server = MockServer::start().await;
            let dir = tempfile::tempdir().unwrap();
            let _env = Env::new(dir.path(), &server.uri());
            login(&server, &access("loop-account", "eu")).await;
            let provider = build();
            let mut events = Vec::new();
            if committed {
                events.push(json!({"type":"response.output_text.delta","delta":"visible"}));
            }
            events.push(json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"loop-item","call_id":"loop-call","name":"must_not_execute"}}));
            let delta = if byte_guard {
                " ".repeat(16 * 1024)
            } else {
                " ".into()
            };
            for _ in 0..if byte_guard { 1 } else { 256 } {
                events.push(json!({"type":"response.function_call_arguments.delta","item_id":"loop-item","output_index":0,"delta":delta}));
            }
            mount_stream(&server, frames(&events)).await;
            let output = collect(&provider, "whitespace-loop").await;
            assert!(
                !output
                    .iter()
                    .any(|chunk| matches!(chunk, StreamChunk::ToolUse { .. })),
                "partial whitespace arguments must never execute a tool"
            );
            assert!(
                !output
                    .iter()
                    .any(|chunk| matches!(chunk, StreamChunk::TurnComplete { .. }))
            );
            assert_eq!(
                output
                    .iter()
                    .filter(|chunk| matches!(chunk, StreamChunk::Error { .. }))
                    .count(),
                1
            );
            let expected_error = if byte_guard {
                "Interrupted OpenAI Codex response after 1 consecutive whitespace-only tool-call argument delta events (16384 chars) for item loop-item."
            } else {
                "Interrupted OpenAI Codex response after 256 consecutive whitespace-only tool-call argument delta events (256 chars) for item loop-item."
            };
            assert!(
                matches!(output.last(), Some(StreamChunk::Error { content }) if content == expected_error)
            );
            assert_eq!(
                output
                    .iter()
                    .filter(|chunk| matches!(chunk, StreamChunk::Text { .. }))
                    .count(),
                usize::from(committed)
            );
            let requests = server.received_requests().await.unwrap();
            assert_eq!(
                response_count(&requests),
                if committed { 1 } else { 3 },
                "two loop-guard replays only, and none after committed text"
            );
            assert_no_chat(&requests);
        }
    }
}

#[tokio::test]
#[serial]
async fn stream_first_event_and_idle_timeouts_use_the_scaled_clock_and_commit_boundary() {
    for committed in [false, true] {
        let auth = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let _env = Env::new(dir.path(), &auth.uri());
        login(&auth, &access("timeout-account", "eu")).await;
        let (listener, origin) = raw_listener().await;
        Env::set("RUSTAIN_TEST_AUTH_ORIGIN", &origin);
        Env::set("RUSTAIN_TEST_RESPONSES_RETRY_SCALE", "0.001");
        let provider = build();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            assert_eq!(read_request(&mut socket).await.target, RESPONSES);
            stream_headers(&mut socket).await;
            if committed {
                write_chunk(
                    &mut socket,
                    frames(&[json!({"type":"response.output_text.delta","delta":"before idle"})])
                        .as_bytes(),
                )
                .await;
            }
            assert_peer_eof(&mut socket).await;
            if !committed {
                let (mut second, _) = listener.accept().await.unwrap();
                assert_eq!(read_request(&mut second).await.target, RESPONSES);
                stream_headers(&mut second).await;
                finish_stream(&mut second, COMPLETE).await;
            }
            listener
        });
        let mut stream = provider
            .stream_completion(vec![user()], options("timeout"))
            .await
            .unwrap();
        if committed {
            assert!(
                matches!(stream.next().await, Some(StreamChunk::Text { content, .. }) if content == "before idle")
            );
        }
        tokio::time::pause();
        assert!(stream.next().now_or_never().is_none());
        // Tokio's timer wheel rounds deadlines to millisecond resolution.
        tokio::time::advance(Duration::from_millis(301)).await;
        tokio::task::yield_now().await;
        if committed {
            let chunk = stream
                .next()
                .now_or_never()
                .expect("idle timeout must be ready after its deadline");
            assert!(
                matches!(chunk, Some(StreamChunk::Error { content }) if content == "Codex stream timed out waiting for an event")
            );
            drop(stream);
            tokio::time::resume();
            assert_no_connection(&server.await.unwrap());
        } else {
            // Poll the elapsed timeout to schedule its scaled in-band retry delay.
            assert!(stream.next().now_or_never().is_none());
            tokio::time::advance(Duration::from_millis(1)).await;
            tokio::time::resume();
            let output: Vec<_> = stream.collect().await;
            assert!(matches!(
                output.last(),
                Some(StreamChunk::TurnComplete { .. })
            ));
            assert!(
                !output
                    .iter()
                    .any(|chunk| matches!(chunk, StreamChunk::Error { .. }))
            );
            assert_no_connection(&server.await.unwrap());
        }
        assert_no_chat(&auth.received_requests().await.unwrap());
    }
}

#[derive(Clone, Default)]
struct LogBuffer(Arc<std::sync::Mutex<Vec<u8>>>);
impl std::io::Write for LogBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[tokio::test]
#[serial]
async fn reflected_bearer_account_and_residency_are_absent_from_errors_and_captured_tracing() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let _env = Env::new(dir.path(), &server.uri());
    let token = access("ACCOUNT-SECRET-BOUNDARY", "RESIDENCY-SECRET-BOUNDARY");
    login(&server, &token).await;
    let provider = build();
    let logs = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(logs.clone())
        .finish();
    let _capture = tracing::subscriber::set_default(subscriber);
    tracing::info!("boundary-log-capture-positive-control");
    for in_band in [false, true] {
        server.reset().await;
        let reflection = format!(
            "public diagnostic; bearer={token}; account=ACCOUNT-SECRET-BOUNDARY; residency=RESIDENCY-SECRET-BOUNDARY"
        );
        let error = if in_band {
            mount_stream(&server, frames(&[
                json!({"type":"response.output_text.delta","delta":"already committed"}),
                json!({"type":"response.failed","response":{"error":{"code":"invalid_request_error","message":reflection}}}),
            ])).await;
            let output = collect(&provider, "reflected-inband").await;
            assert_eq!(
                response_count(&server.received_requests().await.unwrap()),
                1
            );
            output
                .into_iter()
                .find_map(|chunk| match chunk {
                    StreamChunk::Error { content } => Some(content),
                    _ => None,
                })
                .unwrap()
        } else {
            Mock::given(method("POST")).and(path(RESPONSES))
                .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error":{"message":reflection},"ignored_body_echo":"RAW-BODY-SECRET-MUST-NOT-ECHO"})))
                .mount(&server).await;
            let error = provider
                .stream_completion(vec![user()], options("reflected-http"))
                .await
                .err()
                .unwrap();
            assert!(
                matches!(&error, ProviderError::Other(message) if message.starts_with("Codex HTTP 400: public diagnostic"))
            );
            error.to_string()
        };
        assert!(
            error.contains("public diagnostic"),
            "redaction must retain the useful provider message"
        );
        let captured = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        assert!(
            captured.contains("boundary-log-capture-positive-control"),
            "capture must have a positive control"
        );
        for secret in [
            token.as_str(),
            "ACCOUNT-SECRET-BOUNDARY",
            "RESIDENCY-SECRET-BOUNDARY",
            "RAW-BODY-SECRET-MUST-NOT-ECHO",
        ] {
            assert!(
                !error.contains(secret),
                "secret reflected in consumer error"
            );
            assert!(!captured.contains(secret), "secret reflected in tracing");
        }
        assert_no_chat(&server.received_requests().await.unwrap());
    }
}

#[cfg(unix)]
fn daemon_runtime(
    provider: Arc<dyn StreamingProvider>,
    workspace: &std::path::Path,
) -> rustain::adapters::daemon::runtime::DaemonTurnRuntime {
    use arc_swap::ArcSwap;
    use rustain::{
        adapters::{
            daemon::{recipient_context::RecipientContextState, runtime::DaemonTurnRuntime},
            filesystem::FileSystemStorage,
            noop::{
                NoOpApprovalPersistence, NoOpContext, NoOpPersona, NoOpSecurity, NoOpStorage,
                NoOpToolSet, NoOpUsageLedger,
            },
        },
        domain::{
            models::AppConfig,
            ports::{ContextPort, SecurityPort, ToolSetPort},
            services::{
                approval_runtime::ApprovalRuntime, plan_mode_injector::DefaultPlanInjector,
                tool_scheduler::ToolScheduler,
            },
        },
        infrastructure::telemetry::ActiveRatioWindow,
    };
    let security: Arc<dyn SecurityPort> = Arc::new(NoOpSecurity);
    let tools: Arc<dyn ToolSetPort> = Arc::new(NoOpToolSet);
    let approval = ApprovalRuntime::new(16, Arc::new(NoOpApprovalPersistence));
    let tool_scheduler = ToolScheduler::new(security.clone(), tools.clone(), approval.clone(), 16);
    let app = AppConfig {
        model: "gpt-5.6-luna".into(),
        ..Default::default()
    };
    DaemonTurnRuntime {
        recipient_context: Arc::new(RecipientContextState::default()),
        provider,
        app_config: Arc::new(ArcSwap::from_pointee(app)),
        security,
        tools,
        tool_scheduler,
        persona: Arc::new(NoOpPersona),
        context_assembler: Arc::new(ArcSwap::from_pointee(None)),
        context: Arc::new(ArcSwap::from_pointee(
            Arc::new(NoOpContext) as Arc<dyn ContextPort>
        )),
        storage: Arc::new(NoOpStorage),
        fs_storage: Arc::new(FileSystemStorage::with_workspace_root(
            rustain::infrastructure::paths::sessions_dir(workspace),
            workspace.to_path_buf(),
        )),
        usage_ledger: Arc::new(NoOpUsageLedger),
        telemetry: ActiveRatioWindow::new_in_memory(),
        plan_injector: Arc::new(DefaultPlanInjector::new()),
        approval,
        workspace: workspace.to_path_buf(),
        #[cfg(feature = "mcp")]
        mcp_task_runtimes: Vec::new(),
    }
}

#[cfg(unix)]
#[tokio::test]
#[serial]
async fn concurrent_daemon_frontdoor_turns_keep_stable_conversation_ids_and_own_turn_state_on_wire()
{
    use rustain::domain::models::{ChannelKind, ChatMessage, Conversation, TurnOrigin};
    use tokio_util::sync::CancellationToken;
    let auth = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let _env = Env::new(dir.path(), &auth.uri());
    login(&auth, &access("daemon-account", "eu")).await;
    let (listener, origin) = raw_listener().await;
    Env::set("RUSTAIN_TEST_AUTH_ORIGIN", &origin);
    let runtime = daemon_runtime(build(), dir.path());
    let server = tokio::spawn(async move {
        let mut total = 0;
        for continuation in [false, true] {
            // Do not answer the first request until both conversations are in
            // flight. This proves concurrency without sleeps or timing guesses.
            let (mut first, _) = listener.accept().await.unwrap();
            let first_request = read_request(&mut first).await;
            let (mut second, _) = listener.accept().await.unwrap();
            let second_request = read_request(&mut second).await;
            let mut ids = Vec::new();
            for (socket, request) in [(&mut first, first_request), (&mut second, second_request)] {
                total += 1;
                assert_eq!(request.method, "POST");
                assert_eq!(request.target, RESPONSES);
                let id = request.headers["session_id"].clone();
                assert!(matches!(
                    id.as_str(),
                    "daemon-conversation-a" | "daemon-conversation-b"
                ));
                assert_eq!(request.headers["conversation_id"], id);
                assert_eq!(request.headers["x-client-request-id"], id);
                let body: Value = serde_json::from_slice(&request.body).unwrap();
                assert_eq!(body["prompt_cache_key"], id);
                assert_eq!(body["client_metadata"]["session_id"], id);
                let state = format!("state-{id}");
                if continuation {
                    assert_eq!(
                        request.headers.get("x-codex-turn-state"),
                        Some(&state),
                        "each daemon conversation must replay only its own state"
                    );
                } else {
                    assert!(!request.headers.contains_key("x-codex-turn-state"));
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nx-codex-turn-state: {state}\r\nConnection: close\r\n\r\n{COMPLETE}",
                    COMPLETE.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
                ids.push(id);
            }
            ids.sort();
            assert_eq!(ids, ["daemon-conversation-a", "daemon-conversation-b"]);
        }
        assert_eq!(
            total, 4,
            "two concurrent initial turns and two continuations"
        );
        listener
    });
    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut a = Conversation {
        id: "daemon-conversation-a".into(),
        session_id: Some("ephemeral-a".into()),
        ..Default::default()
    };
    let mut b = Conversation {
        id: "daemon-conversation-b".into(),
        session_id: Some("ephemeral-b".into()),
        ..Default::default()
    };
    let first = runtime
        .drive_turn(
            "A".into(),
            ChannelKind::Terminal,
            &mut a,
            &events,
            TurnOrigin::Interactive,
            CancellationToken::new(),
        )
        .await;
    let second = runtime
        .drive_turn(
            "B".into(),
            ChannelKind::Terminal,
            &mut b,
            &events,
            TurnOrigin::Interactive,
            CancellationToken::new(),
        )
        .await;
    let (first, second) = tokio::join!(first, second);
    first.unwrap();
    second.unwrap();
    for conversation in [&mut a, &mut b] {
        conversation.messages.push(ChatMessage {
            role: MessageRole::Assistant,
            content: "previous assistant output".into(),
            ..Default::default()
        });
        // Daemon runtime session ids can rotate independently of conversations.
        conversation.session_id = Some(format!("resumed-{}", conversation.id));
    }
    let first = runtime.drive_preloaded_turn(
        &mut a,
        &events,
        TurnOrigin::Interactive,
        CancellationToken::new(),
    );
    let second = runtime.drive_preloaded_turn(
        &mut b,
        &events,
        TurnOrigin::Interactive,
        CancellationToken::new(),
    );
    let (first, second) = tokio::join!(first, second);
    first.unwrap();
    second.unwrap();
    assert_no_connection(&server.await.unwrap());
    assert_no_chat(&auth.received_requests().await.unwrap());
}

#[tokio::test]
#[serial]
async fn refusal_and_incomplete_terminal_statuses_reach_the_actual_stream_consumer() {
    use rustain::domain::models::StopReason;
    for (reason, expected) in [
        ("max_output_tokens", StopReason::MaxTokens),
        ("steered", StopReason::EndTurn),
    ] {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let _env = Env::new(dir.path(), &server.uri());
        login(&server, &access("incomplete-account", "eu")).await;
        let provider = build();
        mount_stream(&server, frames(&[
            json!({"type":"response.refusal.delta","delta":"I cannot comply."}),
            json!({"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":reason}}}),
        ])).await;
        let output = collect(&provider, "incomplete").await;
        assert!(
            matches!(output.first(), Some(StreamChunk::Text { content, .. }) if content == "I cannot comply.")
        );
        assert!(
            matches!(output.last(), Some(StreamChunk::TurnComplete { stop_reason }) if *stop_reason == expected)
        );
        assert!(
            !output
                .iter()
                .any(|chunk| matches!(chunk, StreamChunk::Error { .. }))
        );
        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            response_count(&requests),
            1,
            "an incomplete terminal is not a retryable EOF"
        );
        assert_no_chat(&requests);
    }
}

#[tokio::test]
#[serial]
async fn models_health_and_probe_return_from_headers_without_waiting_for_the_body() {
    let auth = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let _env = Env::new(dir.path(), &auth.uri());
    login(&auth, &access("status-only-account", "eu")).await;
    let (listener, origin) = raw_listener().await;
    Env::set("RUSTAIN_TEST_AUTH_ORIGIN", &origin);
    let provider = build();
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_request(&mut socket).await;
            assert_eq!(request.method, "GET");
            assert_eq!(request.target, format!("{MODELS}?client_version=0.159.0"));
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").await.unwrap();
            // There is intentionally no body and no terminating chunk. Both
            // consumers must finish and close this response from status alone.
            assert_peer_eof(&mut socket).await;
        }
        listener
    });
    assert!(provider.health_check().await.is_ok());
    assert!(provider.connectivity_probe().await.is_ok());
    assert_no_connection(&server.await.unwrap());
    assert_no_chat(&auth.received_requests().await.unwrap());
}
