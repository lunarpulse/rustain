use async_trait::async_trait;
use futures::{Future, StreamExt};
use rustain::adapters::{auth::flows::refresh::HttpOAuthRefresher, auth_store::FileAuthStore};
use rustain::domain::{
    errors::{AuthError, ProviderError},
    models::{
        AuthSnapshot, CasOutcome, CompletionOptions, Credential, CredentialId, CredentialOrigin,
        CredentialStamp, Message, MessageRole, ProviderConfig,
    },
    ports::{AuthStorePort, RefreshLease},
};
use rustain::infrastructure::{key_resolver::compose, provider_factory::build_provider_for_config};
use serde_json::{Value, json};
use serial_test::serial;
use std::ffi::OsString;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_partial_json, method, path},
};

struct Environment(Vec<(&'static str, Option<OsString>)>);
impl Environment {
    fn isolate(data: &std::path::Path, origin: &str) -> Self {
        let names = [
            "RUSTAIN_DATA_DIR",
            "RUSTAIN_TEST_AUTH_ORIGIN",
            "ANTHROPIC_BASE_URL",
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "RUSTAIN_CLAUDE_CODE_VERSION",
        ];
        let guard = Self(
            names
                .into_iter()
                .map(|key| (key, std::env::var_os(key)))
                .collect(),
        );
        // SAFETY: every caller holds the unkeyed serial lock until this guard drops.
        unsafe {
            for key in names {
                std::env::remove_var(key);
            }
            std::env::set_var("RUSTAIN_DATA_DIR", data);
            std::env::set_var("RUSTAIN_TEST_AUTH_ORIGIN", origin);
            std::env::set_var("ANTHROPIC_BASE_URL", origin);
        }
        guard
    }
}
impl Drop for Environment {
    fn drop(&mut self) {
        for (key, value) in &self.0 {
            // SAFETY: the enclosing serial guard outlives this restoration.
            unsafe {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}
fn config(origin: &str) -> ProviderConfig {
    serde_json::from_value(json!({"provider_id":"anthropic", "kind":"anthropic", "model_id":"claude-test", "api_key_env":"ANTHROPIC_API_KEY", "base_url":origin})).unwrap()
}
fn identity() -> Value {
    json!({"account":{"uuid":"account","email_address":"operator@example.test"},"organization":{"uuid":"org","name":"Organization"}})
}
fn options() -> CompletionOptions {
    CompletionOptions {
        session_id: None,
        model: "claude-test".into(),
        max_tokens: 8,
        system_prompt: String::new(),
        temperature: None,
        tools: vec![],
    }
}
fn messages() -> Vec<Message> {
    vec![Message {
        role: MessageRole::User,
        content: "hi".into(),
        images: vec![],
        tool_results: vec![],
        tool_uses: vec![],
        context_prefix: None,
        reasoning_content: None,
        provider_replay: vec![],
    }]
}
async fn complete(origin: &str) -> Result<(), ProviderError> {
    let provider = build_provider_for_config("anthropic", &config(origin))?;
    let mut stream = provider.stream_completion(messages(), options()).await?;
    while stream.next().await.is_some() {}
    Ok(())
}
async fn messages_stub(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
            "text/event-stream",
        ))
        .mount(server)
        .await;
}

#[tokio::test]
#[serial]
async fn k08_refresh_request_rotates_bearer_and_keeps_original_grant_and_org() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let _environment = Environment::isolate(dir.path(), &server.uri());
    let store: Arc<dyn AuthStorePort> = Arc::new(FileAuthStore::new());
    super::flows::login_oauth(&server, &store, 1000, 3600, identity()).await;
    Mock::given(method("POST")).and(path("/v1/oauth/token"))
        .and(body_partial_json(json!({"grant_type":"refresh_token"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token":"ROTATED-ACCESS", "refresh_token":"ROTATED-REFRESH", "expires_in":3600,"account":{"uuid":"account-new","email_address":"new@example.test"}})))
        .expect(1).mount(&server).await;
    messages_stub(&server).await;
    complete(&server.uri()).await.unwrap();
    let requests = server.received_requests().await.unwrap();
    let refresh = requests
        .iter()
        .find(|request| {
            request
                .body_json::<Value>()
                .is_ok_and(|v| v["grant_type"] == "refresh_token")
        })
        .unwrap();
    assert_eq!(
        refresh.body_json::<Value>().unwrap(),
        json!({"grant_type":"refresh_token","client_id":"9d1c250a-e61b-44d9-88ed-5944d1962f5e","refresh_token":"LOGIN-REFRESH-CANARY"})
    );
    assert_eq!(
        refresh.headers.get("content-type").unwrap(),
        "application/json"
    );
    assert_eq!(
        refresh.headers.get("anthropic-beta").unwrap(),
        "oauth-2025-04-20"
    );
    assert_eq!(
        refresh.headers.get("user-agent").unwrap(),
        "anthropic-sdk-typescript/0.112.1 userOAuthProvider"
    );
    let messages = requests
        .iter()
        .find(|request| request.url.path() == "/v1/messages")
        .unwrap();
    assert_eq!(
        messages.headers.get("authorization").unwrap(),
        "Bearer ROTATED-ACCESS"
    );
    let snapshot = store.snapshot().await.unwrap();
    let Credential::OAuth(saved) = &snapshot.rows("anthropic")[0].credential else {
        panic!("OAuth")
    };
    assert_eq!(saved.authorized_at_ms, Some(1000));
    assert_eq!(saved.identity.org_id.as_deref(), Some("org"));
    assert_eq!(saved.identity.org_name.as_deref(), Some("Organization"));
    assert_eq!(saved.identity.account_id.as_deref(), Some("account-new"));
    assert_eq!(saved.refresh.expose_secret(), "ROTATED-REFRESH");
}

#[tokio::test]
#[serial]
async fn k08_refresh_failures_disable_only_definitive_and_never_echo_private_body() {
    for (status, error, disabled) in [
        (400, "invalid_grant", true),
        (503, "temporarily_unavailable", false),
        (403, "access_denied", false),
    ] {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let _environment = Environment::isolate(dir.path(), &server.uri());
        let store: Arc<dyn AuthStorePort> = Arc::new(FileAuthStore::new());
        super::flows::login_oauth(&server, &store, 1000, 3600, identity()).await;
        Mock::given(method("POST")).and(path("/v1/oauth/token"))
            .and(body_partial_json(json!({"grant_type":"refresh_token"})))
            .respond_with(ResponseTemplate::new(status).set_body_json(json!({"error":error,"error_description":"LOGIN-ACCESS-CANARY LOGIN-REFRESH-CANARY CODE-CANARY STATE-CANARY"})))
            .expect(1).mount(&server).await;
        let failure = complete(&server.uri()).await.unwrap_err();
        assert_eq!(
            matches!(failure, ProviderError::AuthenticationFailed),
            disabled
        );
        assert_eq!(
            matches!(failure, ProviderError::ConnectionFailed(_)),
            !disabled
        );
        let output = format!("{failure:?} {failure}");
        for secret in [
            "LOGIN-ACCESS-CANARY",
            "LOGIN-REFRESH-CANARY",
            "CODE-CANARY",
            "STATE-CANARY",
        ] {
            assert!(!output.contains(secret));
        }
        let snapshot = store.snapshot().await.unwrap();
        let row = &snapshot.rows("anthropic")[0];
        assert_eq!(row.disabled_cause.is_some(), disabled);
        if disabled {
            assert_eq!(
                row.disabled_cause.as_deref(),
                Some("oauth refresh failed: invalid_grant")
            );
        }
    }
}

#[tokio::test]
#[serial]
async fn k13_corrupt_and_future_stores_fall_back_to_env_without_leaking_store_errors() {
    for document in ["not JSON", "{\"version\":3,\"providers\":{}}"] {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let _environment = Environment::isolate(dir.path(), &server.uri());
        std::fs::write(dir.path().join("auth.json"), document).unwrap();
        // SAFETY: the enclosing unkeyed serial lock isolates all environment readers.
        unsafe {
            std::env::set_var("ANTHROPIC_API_KEY", "ENV-FALLBACK");
        }
        messages_stub(&server).await;
        let adapter = build_provider_for_config("anthropic", &config(&server.uri())).unwrap();
        let mut stream = adapter
            .stream_completion(messages(), options())
            .await
            .unwrap();
        while stream.next().await.is_some() {}
        let received = server.received_requests().await.unwrap();
        assert_eq!(
            received[0].headers.get("x-api-key").unwrap(),
            "ENV-FALLBACK"
        );
        // Use the already-constructed adapter to prove request-time fallback too.
        unsafe {
            std::env::remove_var("ANTHROPIC_API_KEY");
        }
        assert!(matches!(
            adapter.stream_completion(messages(), options()).await,
            Err(ProviderError::AuthenticationFailed)
        ));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("auth.json")).unwrap(),
            document
        );
    }
}

struct FailingWrites {
    inner: FileAuthStore,
    failures: AtomicUsize,
    calls: AtomicUsize,
}
#[async_trait]
impl AuthStorePort for FailingWrites {
    async fn snapshot(&self) -> Result<AuthSnapshot, AuthError> {
        self.inner.snapshot().await
    }
    async fn upsert(
        &self,
        p: &str,
        c: Credential,
        o: CredentialOrigin,
        v: bool,
    ) -> Result<CredentialId, AuthError> {
        self.inner.upsert(p, c, o, v).await
    }
    async fn replace_if_matches(
        &self,
        p: &str,
        expected: CredentialStamp,
        c: Credential,
    ) -> Result<CasOutcome, AuthError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self
            .failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| v.checked_sub(1))
            .is_ok()
        {
            return Err(AuthError::Io("injected disk outage".into()));
        }
        self.inner.replace_if_matches(p, expected, c).await
    }
    async fn disable_if_matches(
        &self,
        p: &str,
        expected: CredentialStamp,
        cause: &str,
    ) -> Result<CasOutcome, AuthError> {
        self.inner.disable_if_matches(p, expected, cause).await
    }
    async fn remove(&self, p: &str) -> Result<(), AuthError> {
        self.inner.remove(p).await
    }
    async fn select(
        &self,
        p: &str,
        id: CredentialId,
    ) -> Result<rustain::domain::models::SelectOutcome, AuthError> {
        self.inner.select(p, id).await
    }
    async fn remove_row(
        &self,
        p: &str,
        id: CredentialId,
    ) -> Result<rustain::domain::models::RowRemoval, AuthError> {
        self.inner.remove_row(p, id).await
    }
    async fn refresh_lease(&self) -> Result<RefreshLease, AuthError> {
        self.inner.refresh_lease().await
    }
}

#[tokio::test]
#[serial]
async fn rotated_token_retries_one_failed_write_and_serves_after_two_failures() {
    for failures in [1, 2] {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let _environment = Environment::isolate(dir.path(), &server.uri());
        let port = Arc::new(FailingWrites {
            inner: FileAuthStore::at(dir.path().join("auth.json")),
            failures: AtomicUsize::new(failures),
            calls: AtomicUsize::new(0),
        });
        let store: Arc<dyn AuthStorePort> = port.clone();
        super::flows::login_oauth(&server, &store, 0, 3600, identity()).await;
        Mock::given(method("POST")).and(path("/v1/oauth/token")).and(body_partial_json(json!({"grant_type":"refresh_token"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token":"WRITE-RETRY-ACCESS","refresh_token":"WRITE-RETRY-REFRESH","expires_in":3600,"account":{"uuid":"account","email_address":"operator@example.test"}})))
            .expect(1).mount(&server).await;
        let resolver = compose(
            store.clone(),
            Arc::new(HttpOAuthRefresher::new()),
            Arc::new(|| 3_600_000),
        );
        let resolved = resolver
            .resolve("anthropic", None, &[], &|_| None)
            .await
            .unwrap()
            .unwrap()
            .1;
        assert_eq!(
            resolved.bearer_secret().expose_secret(),
            "WRITE-RETRY-ACCESS"
        );
        assert_eq!(port.calls.load(Ordering::SeqCst), 2);
        let snapshot = store.snapshot().await.unwrap();
        let Credential::OAuth(saved) = &snapshot.rows("anthropic")[0].credential else {
            panic!("OAuth")
        };
        assert_eq!(
            saved.access.expose_secret(),
            if failures == 1 {
                "WRITE-RETRY-ACCESS"
            } else {
                "LOGIN-ACCESS-CANARY"
            }
        );
    }
}

struct GatedRefresh {
    arrived: Arc<tokio::sync::Semaphore>,
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    calls: Arc<AtomicUsize>,
}
impl wiremock::Respond for GatedRefresh {
    fn respond(&self, _: &wiremock::Request) -> ResponseTemplate {
        let first = self.calls.fetch_add(1, Ordering::SeqCst) == 0;
        self.arrived.add_permits(1);
        if first {
            self.release.lock().unwrap().recv().unwrap();
        }
        ResponseTemplate::new(200).set_body_json(json!({
            "access_token":"LEASE-ROTATED-ACCESS","refresh_token":"LEASE-ROTATED-REFRESH",
            "expires_in":3600,"account":{"uuid":"account","email_address":"operator@example.test"}
        }))
    }
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn k14_two_store_resolvers_send_one_http_refresh_and_adopt_rotation() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let _environment = Environment::isolate(dir.path(), &server.uri());
    let auth_path = dir.path().join("auth.json");
    let store: Arc<dyn AuthStorePort> = Arc::new(FileAuthStore::at(&auth_path));
    super::flows::login_oauth(&server, &store, 0, 3600, identity()).await;
    let arrived = Arc::new(tokio::sync::Semaphore::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let (release, released) = std::sync::mpsc::channel();
    Mock::given(method("POST"))
        .and(path("/v1/oauth/token"))
        .and(body_partial_json(json!({"grant_type":"refresh_token"})))
        .respond_with(GatedRefresh {
            arrived: arrived.clone(),
            release: std::sync::Mutex::new(released),
            calls: calls.clone(),
        })
        .mount(&server)
        .await;
    let first = compose(
        store,
        Arc::new(HttpOAuthRefresher::new()),
        Arc::new(|| 3_600_000),
    );
    let second = compose(
        Arc::new(FileAuthStore::at(&auth_path)),
        Arc::new(HttpOAuthRefresher::new()),
        Arc::new(|| 3_600_000),
    );
    let first = tokio::spawn(async move { first.resolve("anthropic", None, &[], &|_| None).await });
    arrived.acquire().await.unwrap().forget();
    let mut second = Box::pin(second.resolve("anthropic", None, &[], &|_| None));
    futures::future::poll_fn(|cx| {
        assert!(second.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    release.send(()).unwrap();
    for result in [first.await.unwrap(), second.await] {
        let (_, auth) = result.unwrap().unwrap();
        assert_eq!(auth.bearer_secret().expose_secret(), "LEASE-ROTATED-ACCESS");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
#[serial]
async fn refresh_redirect_never_resends_the_refresh_token() {
    let server = MockServer::start().await;
    let sink = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let _environment = Environment::isolate(dir.path(), &server.uri());
    let store: Arc<dyn AuthStorePort> = Arc::new(FileAuthStore::new());
    super::flows::login_oauth(&server, &store, 0, 3600, identity()).await;
    // A 308 from the policy origin would replay the refresh grant to another origin.
    Mock::given(method("POST"))
        .and(path("/v1/oauth/token"))
        .and(body_partial_json(json!({"grant_type":"refresh_token"})))
        .respond_with(
            ResponseTemplate::new(308)
                .insert_header("location", format!("{}/v1/oauth/token", sink.uri())),
        )
        .expect(1)
        .mount(&server)
        .await;
    let sink_tokens = json!({"access_token":"SINK-A","refresh_token":"SINK-R","expires_in":3600});
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(sink_tokens))
        .mount(&sink)
        .await;
    messages_stub(&server).await;
    assert!(matches!(
        complete(&server.uri()).await,
        Err(ProviderError::ConnectionFailed(_))
    ));
    assert!(sink.received_requests().await.unwrap().is_empty());
    let snapshot = store.snapshot().await.unwrap();
    let row = &snapshot.rows("anthropic")[0];
    assert!(row.disabled_cause.is_none());
    let Credential::OAuth(saved) = &row.credential else {
        panic!("OAuth")
    };
    assert_eq!(saved.refresh.expose_secret(), "LOGIN-REFRESH-CANARY");
}

#[tokio::test]
#[serial]
async fn abandoned_request_still_persists_the_rotated_refresh_token() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let _environment = Environment::isolate(dir.path(), &server.uri());
    let store: Arc<dyn AuthStorePort> = Arc::new(FileAuthStore::new());
    super::flows::login_oauth(&server, &store, 0, 3600, identity()).await;
    let arrived = Arc::new(tokio::sync::Semaphore::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let (release, released) = std::sync::mpsc::channel();
    Mock::given(method("POST"))
        .and(path("/v1/oauth/token"))
        .and(body_partial_json(json!({"grant_type":"refresh_token"})))
        .respond_with(GatedRefresh {
            arrived: arrived.clone(),
            release: std::sync::Mutex::new(released),
            calls: calls.clone(),
        })
        .mount(&server)
        .await;
    messages_stub(&server).await;
    let origin = server.uri();
    // The turn is abandoned (Esc, /new) while the provider is issuing rotated tokens.
    let abandoned = tokio::spawn(async move { complete(&origin).await });
    arrived.acquire().await.unwrap().forget();
    abandoned.abort();
    assert!(abandoned.await.unwrap_err().is_cancelled());
    release.send(()).unwrap();
    // The next turn adopts the persisted rotation instead of spending the old grant again.
    complete(&server.uri()).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let requests = server.received_requests().await.unwrap();
    let messages = requests
        .iter()
        .find(|request| request.url.path() == "/v1/messages")
        .unwrap();
    assert_eq!(
        messages.headers.get("authorization").unwrap(),
        "Bearer LEASE-ROTATED-ACCESS"
    );
    let snapshot = store.snapshot().await.unwrap();
    let Credential::OAuth(saved) = &snapshot.rows("anthropic")[0].credential else {
        panic!("OAuth")
    };
    assert_eq!(saved.refresh.expose_secret(), "LEASE-ROTATED-REFRESH");
}

#[tokio::test]
#[serial]
async fn k08_refused_refresh_connection_preserves_the_enabled_row() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let _environment = Environment::isolate(dir.path(), &server.uri());
    let store: Arc<dyn AuthStorePort> = Arc::new(FileAuthStore::new());
    super::flows::login_oauth(&server, &store, 0, 3600, identity()).await;
    let unavailable = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let origin = format!("http://{}", unavailable.local_addr().unwrap());
    // SAFETY: the serial and Environment guards span the request.
    unsafe {
        std::env::set_var("RUSTAIN_TEST_AUTH_ORIGIN", &origin);
        std::env::set_var("ANTHROPIC_BASE_URL", &origin);
    }
    drop(unavailable);
    assert!(matches!(
        complete(&origin).await,
        Err(ProviderError::ConnectionFailed(_))
    ));
    assert!(
        store.snapshot().await.unwrap().rows("anthropic")[0]
            .disabled_cause
            .is_none()
    );
}
