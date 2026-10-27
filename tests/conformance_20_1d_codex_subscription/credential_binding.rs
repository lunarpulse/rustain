//! Bind a private request to the grant that actually authorized it, even across store recreation.
use super::auth::{jwt, login_oauth};
use futures::{FutureExt, StreamExt};
use rustain::{
    adapters::{
        auth::{flows::refresh::HttpOAuthRefresher, policies::org_scoped_identity},
        auth_store::FileAuthStore,
    },
    domain::{
        errors::ProviderError,
        models::{
            CasOutcome, CompletionOptions, Credential, Message, MessageRole, ProviderConfig,
            ResolvedAuth, StoredCredential, StreamChunk,
        },
        ports::{AuthStorePort, OAuthRefreshPort, StreamingProvider},
    },
    infrastructure::{
        key_resolver::{KeyResolver, Resolved},
        provider_factory::build_provider_for_config,
    },
};
use serde_json::{Value, json};
use serial_test::serial;
use std::{collections::HashMap, ffi::OsString, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};
use wiremock::MockServer;

const PROVIDER: &str = "openai-codex";
const NOW: i64 = 1_800_000_000_000;
const RESPONSES: &str = "/backend-api/codex/responses";
const PRIVATE: &str = "ACCOUNT-A-PRIVATE: never send this conversation to account B";
const COMPLETE: &str =
    "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n";

struct Env(Vec<(&'static str, Option<OsString>)>);
impl Env {
    fn new(dir: &std::path::Path, origin: &str) -> Self {
        let keys = [
            "RUSTAIN_DATA_DIR",
            "RUSTAIN_TEST_AUTH_ORIGIN",
            "RUSTAIN_TEST_AUTH_CALLBACK_PORT",
            "RUSTAIN_TEST_CODEX_RETRY_SCALE",
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
        // SAFETY: all callers hold the unkeyed serial environment lock.
        unsafe {
            for key in keys {
                std::env::remove_var(key);
            }
            std::env::set_var("RUSTAIN_DATA_DIR", dir);
            std::env::set_var("RUSTAIN_TEST_AUTH_ORIGIN", origin);
            std::env::set_var("RUSTAIN_TEST_CODEX_RETRY_SCALE", "0");
            std::env::set_var("RUSTAIN_TEST_NOW_MS", NOW.to_string());
            let callback = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            std::env::set_var(
                "RUSTAIN_TEST_AUTH_CALLBACK_PORT",
                callback.local_addr().unwrap().port().to_string(),
            );
        }
        guard
    }
    fn origin(origin: &str) {
        // SAFETY: the same serial guard spans the request tasks and browser callbacks.
        unsafe {
            std::env::set_var("RUSTAIN_TEST_AUTH_ORIGIN", origin);
        }
    }
}
impl Drop for Env {
    fn drop(&mut self) {
        for (key, value) in self.0.drain(..) {
            // SAFETY: the serial guard outlives environment restoration.
            unsafe {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

fn access(account: &str) -> String {
    grant_access(account, "login")
}
fn grant_access(account: &str, grant: &str) -> String {
    jwt(json!({
        "jti": grant,
        "https://api.openai.com/auth": {
            "chatgpt_account_id": account,
            "chatgpt_data_residency": "eu",
            "chatgpt_plan_type": "plus"
        },
        "https://api.openai.com/profile": {"email": format!("{account}@example.test")}
    }))
}
fn tokens(account: &str, refresh: &str) -> Value {
    json!({"access_token": access(account), "refresh_token": refresh, "expires_in": 3600})
}
async fn login(server: &MockServer, store: &Arc<dyn AuthStorePort>, account: &str, refresh: &str) {
    Env::origin(&server.uri());
    // login_oauth checks the printed authorization origin is loopback before callback delivery.
    login_oauth(server, store, NOW, tokens(account, refresh)).await;
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url.path(), "/oauth/token");
    let form: HashMap<_, _> = url::form_urlencoded::parse(&requests[0].body)
        .into_owned()
        .collect();
    assert_eq!(form["grant_type"], "authorization_code");
    assert!(!form.contains_key("refresh_token"));
}
fn build() -> Arc<dyn StreamingProvider> {
    let config = ProviderConfig {
        provider_id: PROVIDER.into(),
        kind: Some(PROVIDER.into()),
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
    };
    build_provider_for_config(PROVIDER, &config).unwrap()
}
fn private_request(
    provider: Arc<dyn StreamingProvider>,
) -> JoinHandle<Result<Vec<StreamChunk>, ProviderError>> {
    tokio::spawn(async move {
        let message = Message {
            role: MessageRole::User,
            content: PRIVATE.into(),
            images: vec![],
            tool_results: vec![],
            tool_uses: vec![],
            context_prefix: None,
            reasoning_content: None,
        };
        let options = CompletionOptions {
            model: "gpt-5.6-luna".into(),
            max_tokens: 4096,
            system_prompt: String::new(),
            temperature: None,
            tools: vec![],
            session_id: Some("credential-binding-private-A".into()),
        };
        Ok(provider
            .stream_completion(vec![message], options)
            .await?
            .collect()
            .await)
    })
}

struct Request {
    method: String,
    target: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}
async fn listener() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    (listener, origin)
}
async fn accept(listener: &TcpListener) -> (TcpStream, Request) {
    tokio::time::timeout(Duration::from_secs(10), async {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_request(&mut socket).await;
        (socket, request)
    })
    .await
    .expect("expected request must reach its structural socket gate")
}
async fn read_request(socket: &mut TcpStream) -> Request {
    let mut bytes = Vec::new();
    let header_end = loop {
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
        let mut buffer = [0; 4096];
        let count = socket.read(&mut buffer).await.unwrap();
        assert_ne!(count, 0, "request closed before headers");
        bytes.extend_from_slice(&buffer[..count]);
    };
    let head = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
    let mut lines = head.split("\r\n");
    let mut start = lines.next().unwrap().split_whitespace();
    let method = start.next().unwrap().to_owned();
    let target = start.next().unwrap().to_owned();
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
        assert_ne!(count, 0, "request closed before body");
        bytes.extend_from_slice(&buffer[..count]);
    }
    Request {
        method,
        target,
        headers,
        body: bytes[header_end..header_end + length].to_vec(),
    }
}
async fn respond(socket: &mut TcpStream, status: u16, content_type: &str, body: &str) {
    socket
        .write_all(
            format!(
                "HTTP/1.1 {status} Fixture\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    socket.shutdown().await.unwrap();
}
fn assert_private_post(request: &Request, account: &str) {
    assert_eq!(request.method, "POST");
    assert_eq!(request.target, RESPONSES);
    assert_eq!(
        request.headers["authorization"],
        format!("Bearer {}", access(account))
    );
    assert_eq!(request.headers["chatgpt-account-id"], account);
    let body: Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(body["model"], "gpt-5.6-luna");
    assert_eq!(
        body["input"],
        json!([{
            "role": "user", "content": [{"type": "input_text", "text": PRIVATE}]
        }])
    );
    assert_eq!(body["store"], false);
}
fn assert_refresh(request: &Request, refresh: &str) {
    assert_eq!(request.method, "POST");
    assert_eq!(request.target, "/oauth/token");
    assert_eq!(
        request.headers["content-type"],
        "application/x-www-form-urlencoded"
    );
    assert!(!request.headers.contains_key("authorization"));
    let form: HashMap<_, _> = url::form_urlencoded::parse(&request.body)
        .into_owned()
        .collect();
    assert_eq!(form.len(), 3);
    assert_eq!(form["grant_type"], "refresh_token");
    assert_eq!(form["client_id"], "app_EMoamEEZ73f0CkXaXp7hrann");
    assert_eq!(form["refresh_token"], refresh);
    assert!(!String::from_utf8_lossy(&request.body).contains(PRIVATE));
}

/// Drain erroneous extra traffic as well as the correct refusal. Returning success to an
/// erroneous refresh makes an account-B private replay observable instead of masking it
/// behind a fixture 404. Completion, not a timer or an idle interval, closes observation.
async fn finish_private(
    listener: &TcpListener,
    mut task: JoinHandle<Result<Vec<StreamChunk>, ProviderError>>,
) -> (Result<Vec<StreamChunk>, ProviderError>, Vec<Request>) {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut extra = Vec::new();
        loop {
            tokio::select! {
                result = &mut task => {
                    assert!(
                        listener.accept().now_or_never().is_none(),
                        "no detached private replay"
                    );
                    return (result.unwrap(), extra);
                }
                accepted = listener.accept() => {
                    let (mut socket, _) = accepted.unwrap();
                    let request = read_request(&mut socket).await;
                    match request.target.as_str() {
                        "/oauth/token" => respond(
                            &mut socket,
                            200,
                            "application/json",
                            &tokens("account-B", "B-ROTATED").to_string(),
                        ).await,
                        RESPONSES => respond(&mut socket, 200, "text/event-stream", COMPLETE).await,
                        target => panic!("unexpected credential-binding HTTP target {target}"),
                    }
                    extra.push(request);
                    assert!(extra.len() <= 4, "unbounded credential recovery traffic");
                }
            }
        }
    })
    .await
    .expect("credential-bound refusal must finish without another grant")
}
fn assert_refused(result: Result<Vec<StreamChunk>, ProviderError>) {
    assert!(
        matches!(result, Err(ProviderError::AuthenticationFailed)),
        "a changed grant must fail fatally, never invite an outer turn retry: {result:?}"
    );
}
async fn assert_b_untouched(store: &Arc<dyn AuthStorePort>, path: &std::path::Path, bytes: &[u8]) {
    assert_eq!(
        std::fs::read(path).unwrap(),
        bytes,
        "account B must remain byte-for-byte untouched"
    );
    let snapshot = store.snapshot().await.unwrap();
    let rows = snapshot.rows(PROVIDER);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].rev, 1);
    assert!(
        rows[0].disabled_cause.is_none(),
        "A's refusal must not disable B"
    );
    let Credential::OAuth(oauth) = &rows[0].credential else {
        panic!("browser login must persist OAuth")
    };
    assert_eq!(oauth.access.expose_secret(), access("account-B"));
    assert_eq!(oauth.refresh.expose_secret(), "B-REFRESH");
    assert_eq!(oauth.identity.account_id.as_deref(), Some("account-B"));
}

#[tokio::test]
#[serial]
async fn aba_recreated_login_before_401_never_refreshes_or_posts_private_text_under_b() {
    let auth_a = MockServer::start().await;
    let auth_b = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let _env = Env::new(dir.path(), &auth_a.uri());
    let path = dir.path().join("auth.json");
    let store: Arc<dyn AuthStorePort> = Arc::new(FileAuthStore::new());
    login(&auth_a, &store, "account-A", "A-REFRESH").await;
    let before = store.snapshot().await.unwrap();
    let old = &before.rows(PROVIDER)[0];
    assert_eq!(old.id.0, 1);
    assert_eq!(old.rev, 1);
    let (listener, origin) = listener().await;
    Env::origin(&origin);
    let task = private_request(build());
    let (mut first, request) = accept(&listener).await;
    assert_private_post(&request, "account-A");

    // The complete private POST has arrived, but its 401 cannot be seen until B is durable.
    std::fs::remove_file(&path).unwrap();
    login(&auth_b, &store, "account-B", "B-REFRESH").await;
    let replacement = store.snapshot().await.unwrap();
    assert_eq!(
        replacement.rows(PROVIDER)[0].id,
        old.id,
        "exercise actual id reuse"
    );
    assert_eq!(
        replacement.rows(PROVIDER)[0].rev,
        old.rev,
        "exercise actual revision reuse"
    );
    let b_bytes = std::fs::read(&path).unwrap();
    Env::origin(&origin);
    respond(
        &mut first,
        401,
        "application/json",
        r#"{"error":{"code":"invalid_token"}}"#,
    )
    .await;
    let (result, extra) = finish_private(&listener, task).await;

    assert_b_untouched(&store, &path, &b_bytes).await;
    assert!(
        extra.iter().all(|request| request.target != "/oauth/token"),
        "A's 401 must not spend B's refresh grant"
    );
    assert!(
        extra.iter().all(|request| request.target != RESPONSES),
        "A's private body must not be retried using B"
    );
    assert!(extra.is_empty());
    assert_refused(result);
}

#[tokio::test]
#[serial]
async fn aba_recreated_login_during_real_refresh_never_overwrites_or_adopts_b() {
    let auth_a = MockServer::start().await;
    let auth_b = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let _env = Env::new(dir.path(), &auth_a.uri());
    let path = dir.path().join("auth.json");
    let store: Arc<dyn AuthStorePort> = Arc::new(FileAuthStore::new());
    login(&auth_a, &store, "account-A", "A-REFRESH").await;
    let before = store.snapshot().await.unwrap();
    let old = &before.rows(PROVIDER)[0];
    assert_eq!(old.id.0, 1);
    assert_eq!(old.rev, 1);
    let (listener, origin) = listener().await;
    Env::origin(&origin);
    let task = private_request(build());
    let (mut first, request) = accept(&listener).await;
    assert_private_post(&request, "account-A");
    respond(
        &mut first,
        401,
        "application/json",
        r#"{"error":{"code":"invalid_token"}}"#,
    )
    .await;
    let (mut refresh, request) = accept(&listener).await;
    assert_refresh(&request, "A-REFRESH");

    // Gate the actual refresh RESPONSE, not an emulated resolver or a handwritten row.
    std::fs::remove_file(&path).unwrap();
    login(&auth_b, &store, "account-B", "B-REFRESH").await;
    let replacement = store.snapshot().await.unwrap();
    assert_eq!(replacement.rows(PROVIDER)[0].id, old.id);
    assert_eq!(replacement.rows(PROVIDER)[0].rev, old.rev);
    let b_bytes = std::fs::read(&path).unwrap();
    Env::origin(&origin);
    let mut rotated = tokens("account-A", "A-ROTATED");
    rotated["access_token"] = grant_access("account-A", "rotated-A").into();
    respond(&mut refresh, 200, "application/json", &rotated.to_string()).await;
    let (result, extra) = finish_private(&listener, task).await;

    assert_b_untouched(&store, &path, &b_bytes).await;
    assert!(
        extra.iter().all(|request| request.target != "/oauth/token"),
        "CAS loss cannot spend another login's grant"
    );
    assert!(
        extra.iter().all(|request| request.target != RESPONSES),
        "CAS loss cannot adopt B or replay A's conversation"
    );
    assert!(extra.is_empty());
    assert_refused(result);
}

fn resolver(store: Arc<dyn AuthStorePort>) -> Arc<KeyResolver> {
    Arc::new(KeyResolver::new(
        store,
        Arc::new(HttpOAuthRefresher::new()),
        Arc::new(|| NOW),
    ))
}
fn assert_actual_grant(resolved: &Resolved, row: &StoredCredential) {
    assert_eq!(
        resolved.row,
        Some(row.stamp()),
        "served stamp belongs to the actual returned grant"
    );
    let Credential::OAuth(oauth) = &row.credential else {
        panic!("expected persisted OAuth grant")
    };
    let ResolvedAuth::OAuth { access, account_id } = &resolved.auth else {
        panic!("expected resolved OAuth grant")
    };
    assert_eq!(access.expose_secret(), oauth.access.expose_secret());
    assert_eq!(account_id, &oauth.identity.account_id);
    assert_eq!(
        resolved.identity,
        oauth.identity.key(org_scoped_identity(PROVIDER))
    );
}

#[tokio::test]
#[serial]
async fn passive_real_refresh_returns_rotated_identity_and_actual_stamp() {
    let auth = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let _env = Env::new(dir.path(), &auth.uri());
    let store: Arc<dyn AuthStorePort> = Arc::new(FileAuthStore::new());
    // A real login at an earlier clock instant makes the stored grant stale without editing it.
    login_oauth(
        &auth,
        &store,
        NOW - 7_200_000,
        tokens("account-A", "A-REFRESH"),
    )
    .await;
    let before = store.snapshot().await.unwrap();
    let Credential::OAuth(old) = &before.rows(PROVIDER)[0].credential else {
        panic!("expected OAuth login")
    };
    let old_identity = old.identity.key(org_scoped_identity(PROVIDER));
    let (listener, origin) = listener().await;
    Env::origin(&origin);
    let resolver = resolver(store.clone());
    let task = tokio::spawn(async move {
        resolver
            .resolve_tracked(PROVIDER, None, &[], &|_| None)
            .await
    });
    let (mut refresh, request) = accept(&listener).await;
    assert_refresh(&request, "A-REFRESH");
    respond(
        &mut refresh,
        200,
        "application/json",
        &tokens("account-A-rotated", "A-ROTATED").to_string(),
    )
    .await;
    let resolved = task.await.unwrap().unwrap().unwrap();
    let after = store.snapshot().await.unwrap();
    let row = &after.rows(PROVIDER)[0];
    assert_eq!(row.rev, before.rows(PROVIDER)[0].rev + 1);
    assert_actual_grant(&resolved, row);
    assert_ne!(
        resolved.identity, old_identity,
        "passive refresh must not retain pre-refresh scope metadata"
    );
    assert!(listener.accept().now_or_never().is_none());
}

#[tokio::test]
#[serial]
async fn real_refresh_losing_cas_returns_peer_grant_identity_and_stamp() {
    for definitive in [false, true] {
        let auth = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let _env = Env::new(dir.path(), &auth.uri());
        let path = dir.path().join("auth.json");
        let store: Arc<dyn AuthStorePort> = Arc::new(FileAuthStore::new());
        login_oauth(
            &auth,
            &store,
            NOW - 7_200_000,
            tokens("account-A", "A-REFRESH"),
        )
        .await;
        let before = store.snapshot().await.unwrap();
        let row = &before.rows(PROVIDER)[0];
        let Credential::OAuth(old) = &row.credential else {
            panic!("expected OAuth login")
        };
        let (listener, origin) = listener().await;
        Env::origin(&origin);
        let resolver = resolver(store.clone());
        let task = tokio::spawn(async move {
            resolver
                .resolve_tracked(PROVIDER, None, &[], &|_| None)
                .await
        });
        let (mut losing_refresh, request) = accept(&listener).await;
        assert_refresh(&request, "A-REFRESH");

        // An independent file-store writer publishes an actual HTTP-projected refresh grant
        // while the resolver's response is gated. This exercises peer-CAS adoption without
        // substituting an OAuthRefreshPort mock or manually seeding an OAuth credential.
        let previous = old.clone();
        let peer_refresh = tokio::spawn(async move {
            HttpOAuthRefresher::new()
                .refresh(PROVIDER, &previous)
                .await
                .unwrap()
        });
        let (mut peer_socket, request) = accept(&listener).await;
        assert_refresh(&request, "A-REFRESH");
        respond(
            &mut peer_socket,
            200,
            "application/json",
            &tokens("account-A-peer", "A-PEER-REFRESH").to_string(),
        )
        .await;
        let peer_tokens = peer_refresh.await.unwrap();
        let mut peer_grant = old.clone();
        peer_grant.access = peer_tokens.access;
        peer_grant.refresh = peer_tokens.refresh.unwrap();
        peer_grant.expires_at_ms = peer_tokens.expires_at_ms;
        peer_grant.identity = peer_tokens.identity;
        let peer = FileAuthStore::at(&path);
        assert_eq!(
            peer.replace_if_matches(PROVIDER, row.stamp(), Credential::OAuth(peer_grant))
                .await
                .unwrap(),
            CasOutcome::Applied
        );
        let peer_bytes = std::fs::read(&path).unwrap();
        if definitive {
            respond(
                &mut losing_refresh,
                400,
                "application/json",
                r#"{"error":"invalid_grant"}"#,
            )
            .await;
        } else {
            respond(
                &mut losing_refresh,
                200,
                "application/json",
                &tokens("account-A-loser", "A-LOSER-REFRESH").to_string(),
            )
            .await;
        }
        let resolved = task.await.unwrap().unwrap().unwrap();
        let after = store.snapshot().await.unwrap();
        let actual = &after.rows(PROVIDER)[0];
        assert_actual_grant(&resolved, actual);
        let ResolvedAuth::OAuth {
            access: returned_access,
            ..
        } = &resolved.auth
        else {
            panic!("expected adopted OAuth grant")
        };
        assert_eq!(returned_access.expose_secret(), access("account-A-peer"));
        assert_eq!(
            std::fs::read(&path).unwrap(),
            peer_bytes,
            "losing refresh must not rewrite or disable peer rotation"
        );
        assert!(actual.disabled_cause.is_none());
        assert!(listener.accept().now_or_never().is_none());
    }
}
