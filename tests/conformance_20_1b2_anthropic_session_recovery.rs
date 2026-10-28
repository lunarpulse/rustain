//! Local-only subscription recovery through the production handle and adapter.
#![cfg(feature = "test-instrumentation")]

use futures::{StreamExt, future::BoxFuture};
use rustain::{
    adapters::{
        anthropic::claude_code::{self, ClaudeCodeIdentity},
        auth::{
            flows::{self, FlowCtx, LoginUi, refresh::HttpOAuthRefresher},
            policies::ANTHROPIC_OAUTH,
        },
        auth_store::FileAuthStore,
    },
    domain::{
        errors::ProviderError,
        models::{
            AuthMethod, CompletionOptions, Credential, Message, MessageRole, ProviderConfig,
            SecretString,
        },
        ports::AuthStorePort,
    },
    infrastructure::{
        key_resolver::{CredentialHandle, compose},
        provider_factory::build_provider_for_config,
    },
};
use serde_json::{Value, json};
use serial_test::serial;
use std::{
    ffi::OsString,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_partial_json, method, path},
};

const NOW: i64 = 1_800_000_000_000;
struct Env(Vec<(&'static str, Option<OsString>)>);
impl Env {
    fn new(dir: &std::path::Path, origin: &str) -> Self {
        let keys = [
            "RUSTAIN_DATA_DIR",
            "RUSTAIN_TEST_AUTH_ORIGIN",
            "ANTHROPIC_BASE_URL",
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "RUSTAIN_CLAUDE_CODE_VERSION",
            "RUSTAIN_TEST_NOW_MS",
        ];
        let guard = Self(keys.iter().map(|k| (*k, std::env::var_os(k))).collect());
        // SAFETY: each environment-mutating test holds the unkeyed serial lock.
        unsafe {
            for key in keys {
                std::env::remove_var(key);
            }
            std::env::set_var("RUSTAIN_DATA_DIR", dir);
            std::env::set_var("RUSTAIN_TEST_AUTH_ORIGIN", origin);
            std::env::set_var("ANTHROPIC_BASE_URL", origin);
        }
        Self::set("RUSTAIN_TEST_NOW_MS", &NOW.to_string());
        claude_code::reset_for_test();
        claude_code::pin_identity_for_test(ClaudeCodeIdentity {
            version: "2.1.280".into(),
            install_id: "install-test".into(),
            session: "session-test".into(),
        });
        guard
    }
    fn set(key: &str, value: &str) {
        // SAFETY: callers hold the unkeyed serial lock and the restoring guard.
        unsafe {
            std::env::set_var(key, value);
        }
    }
}
impl Drop for Env {
    fn drop(&mut self) {
        claude_code::reset_for_test();
        for (key, value) in self.0.drain(..) {
            // SAFETY: the serial guard outlives this restoration.
            unsafe {
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}
struct Ui(mpsc::UnboundedSender<String>);
impl LoginUi for Ui {
    fn on_auth_url(&self, url: &str, _: &str, _: &str) {
        self.0.send(url.into()).unwrap();
    }
    fn on_device_code(&self, _: &str, _: &str, _: &str) {
        panic!("browser login cannot show a device code");
    }
    fn on_progress(&self, _: &str) {}
    fn prompt_secret(&self, _: &str) -> BoxFuture<'_, Option<SecretString>> {
        Box::pin(async { None })
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
async fn login(server: &MockServer, store: &Arc<dyn AuthStorePort>, at: i64, expiry: i64) {
    Mock::given(method("POST")).and(path("/v1/oauth/token")).and(body_partial_json(json!({"grant_type":"authorization_code"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token":"LOGIN-ACCESS", "refresh_token":"LOGIN-REFRESH", "expires_in":expiry,"account":{"uuid":"account","email_address":"operator@example.test"},"organization":{"uuid":"org","name":"Organization"}})))
        .mount(server).await;
    let (send, mut urls) = mpsc::unbounded_channel();
    let context = FlowCtx::new(Arc::new(Ui(send)), Arc::new(move || at));
    let browser = async {
        let auth = url::Url::parse(&urls.recv().await.unwrap()).unwrap();
        assert_eq!(auth.host_str(), Some("127.0.0.1"));
        let query: std::collections::HashMap<_, _> = auth.query_pairs().into_owned().collect();
        let mut callback = url::Url::parse(&query["redirect_uri"]).unwrap();
        assert_eq!(callback.host_str(), Some("localhost"));
        callback
            .query_pairs_mut()
            .append_pair("code", "fixture-code")
            .append_pair("state", &query["state"]);
        reqwest::get(callback).await.unwrap();
    };
    let method = AuthMethod::OAuthCode(&ANTHROPIC_OAUTH);
    let (result, ()) = tokio::join!(flows::login("anthropic", &method, &context, store), browser);
    result.unwrap();
}
fn handle(store: Arc<dyn AuthStorePort>) -> CredentialHandle {
    CredentialHandle::new(
        compose(store, Arc::new(HttpOAuthRefresher::new()), Arc::new(|| NOW)),
        "anthropic".into(),
        vec![],
        rustain::domain::services::key_cascade::RowPolicy::ANY,
        None,
    )
}
async fn refresh(server: &MockServer, token: &str, status: u16) {
    Mock::given(method("POST")).and(path("/v1/oauth/token")).and(body_partial_json(json!({"grant_type":"refresh_token"})))
        .respond_with(ResponseTemplate::new(status).set_body_json(json!({"access_token":token,"refresh_token":"ROTATED-REFRESH","expires_in":3600,"account":{"uuid":"account"}})))
        .mount(server).await;
}
fn count(requests: &[wiremock::Request], endpoint: &str) -> usize {
    requests
        .iter()
        .filter(|r| {
            r.url.path() == endpoint
                && (endpoint != "/v1/oauth/token"
                    || r.body_json::<Value>().unwrap()["grant_type"] == "refresh_token")
        })
        .count()
}

#[tokio::test]
#[serial]
async fn tracked_handle_forces_fresh_row_and_declines_same_bearer_or_disabled_row() {
    for (token, retried) in [("ROTATED-ACCESS", true), ("LOGIN-ACCESS", false)] {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let _env = Env::new(dir.path(), &server.uri());
        let store: Arc<dyn AuthStorePort> = Arc::new(FileAuthStore::new());
        login(&server, &store, NOW, 3600).await;
        refresh(&server, token, 200).await;
        let handle = handle(store.clone());
        let rejected = handle.current().await.unwrap();
        let recovered = handle.recover(&rejected).await.unwrap();
        assert_eq!(recovered.is_some(), retried);
        if let Some(auth) = recovered {
            assert_eq!(auth.auth.bearer_secret().expose_secret(), "ROTATED-ACCESS");
        }
        assert_eq!(store.snapshot().await.unwrap().rows("anthropic")[0].rev, 2);
        assert!(
            handle
                .recover(&handle.current().await.unwrap())
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            count(
                &server.received_requests().await.unwrap(),
                "/v1/oauth/token"
            ),
            1
        );
        store
            .disable_if_matches(
                "anthropic",
                rustain::domain::models::CredentialStamp {
                    rev: 2,
                    ..rejected.row.unwrap()
                },
                "operator disabled",
            )
            .await
            .unwrap();
        assert!(handle.recover(&rejected).await.unwrap().is_none());
        assert_eq!(
            count(
                &server.received_requests().await.unwrap(),
                "/v1/oauth/token"
            ),
            1
        );
    }
}

struct GatedRefresh {
    arrived: Arc<Semaphore>,
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    calls: Arc<AtomicUsize>,
}
impl wiremock::Respond for GatedRefresh {
    fn respond(&self, _: &wiremock::Request) -> ResponseTemplate {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.arrived.add_permits(1);
            self.release.lock().unwrap().recv().unwrap();
        }
        ResponseTemplate::new(200).set_body_json(json!({"access_token":"PEER-ACCESS", "refresh_token":"PEER-REFRESH", "expires_in":3600,"account":{"uuid":"account"}}))
    }
}
#[tokio::test]
#[serial]
async fn k03_concurrent_recovery_adopts_one_rotation_in_process_and_across_store_leases() {
    for separate_resolvers in [false, true] {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let _env = Env::new(dir.path(), &server.uri());
        let store: Arc<dyn AuthStorePort> = Arc::new(FileAuthStore::new());
        login(&server, &store, NOW, 3600).await;
        let first = handle(store.clone());
        let second = if separate_resolvers {
            handle(Arc::new(FileAuthStore::at(dir.path().join("auth.json"))))
        } else {
            first.clone()
        };
        let a = first.current().await.unwrap();
        let b = second.current().await.unwrap();
        let arrived = Arc::new(Semaphore::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let (send, recv) = std::sync::mpsc::channel();
        Mock::given(method("POST"))
            .and(path("/v1/oauth/token"))
            .and(body_partial_json(json!({"grant_type":"refresh_token"})))
            .respond_with(GatedRefresh {
                arrived: arrived.clone(),
                release: std::sync::Mutex::new(recv),
                calls: calls.clone(),
            })
            .mount(&server)
            .await;
        let one = tokio::spawn(async move { first.recover(&a).await });
        arrived.acquire().await.unwrap().forget();
        let mut two = Box::pin(second.recover(&b));
        futures::future::poll_fn(|cx| {
            assert!(Future::poll(two.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        send.send(()).unwrap();
        for result in [one.await.unwrap(), two.await] {
            assert_eq!(
                result
                    .unwrap()
                    .unwrap()
                    .auth
                    .bearer_secret()
                    .expose_secret(),
                "PEER-ACCESS"
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(store.snapshot().await.unwrap().rows("anthropic")[0].rev, 2);
    }
}

#[tokio::test]
#[serial]
async fn abandoned_forced_recovery_persists_rotation_before_releasing_lease() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let _env = Env::new(dir.path(), &server.uri());
    let store: Arc<dyn AuthStorePort> = Arc::new(FileAuthStore::new());
    login(&server, &store, NOW, 3600).await;
    let handle = handle(store.clone());
    let rejected = handle.current().await.unwrap();
    let arrived = Arc::new(Semaphore::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let (send, recv) = std::sync::mpsc::channel();
    Mock::given(method("POST"))
        .and(path("/v1/oauth/token"))
        .and(body_partial_json(json!({"grant_type":"refresh_token"})))
        .respond_with(GatedRefresh {
            arrived: arrived.clone(),
            release: std::sync::Mutex::new(recv),
            calls: calls.clone(),
        })
        .mount(&server)
        .await;
    let request = tokio::spawn(async move { handle.recover(&rejected).await });
    arrived.acquire().await.unwrap().forget();
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    send.send(()).unwrap();
    // Waiting on the real store lease observes completion through persistence, not time.
    let _lease = store.refresh_lease().await.unwrap();
    let snapshot = store.snapshot().await.unwrap();
    assert_eq!(snapshot.rows("anthropic")[0].rev, 2);
    let Credential::OAuth(saved) = &snapshot.rows("anthropic")[0].credential else {
        panic!("OAuth");
    };
    assert_eq!(saved.refresh.expose_secret(), "PEER-REFRESH");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

fn config(origin: &str) -> ProviderConfig {
    serde_json::from_value(json!({"provider_id":"anthropic","kind":"anthropic","model_id":"test-model","api_key_env":"ANTHROPIC_API_KEY","base_url":origin})).unwrap()
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
fn options() -> CompletionOptions {
    CompletionOptions {
        session_id: None,
        model: "test-model".into(),
        max_tokens: 8,
        system_prompt: String::new(),
        temperature: None,
        tools: vec![],
    }
}
async fn complete(origin: &str) -> Result<(), ProviderError> {
    let provider = build_provider_for_config("anthropic", &config(origin))?;
    let mut stream = provider.stream_completion(messages(), options()).await?;
    while stream.next().await.is_some() {}
    Ok(())
}
fn success() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        "text/event-stream",
    )
}
async fn reject_once(server: &MockServer, endpoint: &str, verb: &str, status: u16) {
    Mock::given(method(verb))
        .and(path(endpoint))
        .respond_with(success())
        .with_priority(10)
        .mount(server)
        .await;
    Mock::given(method(verb))
        .and(path(endpoint))
        .respond_with(ResponseTemplate::new(status))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(server)
        .await;
}

#[tokio::test]
#[serial]
async fn k01_stream_health_probe_refresh_once_with_exact_refresh_request_and_new_bearer() {
    for entry in ["stream", "health", "probe"] {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let _env = Env::new(dir.path(), &server.uri());
        let store: Arc<dyn AuthStorePort> = Arc::new(FileAuthStore::new());
        login(&server, &store, NOW, 3600).await;
        refresh(&server, "ROTATED-ACCESS", 200).await;
        let (endpoint, verb) = if entry == "probe" {
            ("/v1/models", "GET")
        } else {
            ("/v1/messages", "POST")
        };
        reject_once(&server, endpoint, verb, 401).await;
        let provider = build_provider_for_config("anthropic", &config(&server.uri())).unwrap();
        match entry {
            "stream" => {
                let mut stream = provider
                    .stream_completion(messages(), options())
                    .await
                    .unwrap();
                while stream.next().await.is_some() {}
            }
            "health" => provider.health_check().await.unwrap(),
            _ => {
                provider.connectivity_probe().await.unwrap();
            }
        }
        let requests = server.received_requests().await.unwrap();
        assert_eq!(count(&requests, endpoint), 2, "{entry}");
        assert_eq!(count(&requests, "/v1/oauth/token"), 1, "{entry}");
        let posts: Vec<_> = requests
            .iter()
            .filter(|r| r.url.path() == endpoint)
            .collect();
        assert_eq!(posts[0].headers["authorization"], "Bearer LOGIN-ACCESS");
        assert_eq!(posts[1].headers["authorization"], "Bearer ROTATED-ACCESS");
        let refresh = requests
            .iter()
            .find(|r| {
                r.body_json::<Value>()
                    .is_ok_and(|v| v["grant_type"] == "refresh_token")
            })
            .unwrap();
        assert_eq!(
            refresh.body_json::<Value>().unwrap(),
            json!({"grant_type":"refresh_token","client_id":"9d1c250a-e61b-44d9-88ed-5944d1962f5e","refresh_token":"LOGIN-REFRESH"})
        );
        assert_eq!(refresh.headers["anthropic-beta"], "oauth-2025-04-20");
        assert_eq!(refresh.headers["content-type"], "application/json");
        assert_eq!(
            refresh.headers["user-agent"],
            "anthropic-sdk-typescript/0.112.1 userOAuthProvider"
        );
        assert_eq!(store.snapshot().await.unwrap().rows("anthropic")[0].rev, 2);
    }
}

#[tokio::test]
#[serial]
async fn k01_auth_retry_budget_static_keys_forbidden_and_refresh_errors() {
    for (stored, status, token, refresh_status, expected_posts, refresh_posts, connection) in [
        (true, 401, "ROTATED-ACCESS", 200, 2, 1, false),
        (true, 401, "LOGIN-ACCESS", 200, 1, 1, false),
        (true, 403, "ROTATED-ACCESS", 200, 1, 0, false),
        (false, 401, "ROTATED-ACCESS", 200, 1, 0, false),
        (true, 401, "ROTATED-ACCESS", 503, 1, 1, true),
    ] {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let _env = Env::new(dir.path(), &server.uri());
        let store: Arc<dyn AuthStorePort> = Arc::new(FileAuthStore::new());
        if stored {
            login(&server, &store, NOW, 3600).await;
        } else {
            Env::set("ANTHROPIC_API_KEY", "sk-ant-oat01-STATIC");
        }
        refresh(&server, token, refresh_status).await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(status))
            .mount(&server)
            .await;
        let error = complete(&server.uri()).await.unwrap_err();
        if connection {
            assert!(matches!(error, ProviderError::ConnectionFailed(_)));
        } else if status == 401 {
            assert!(matches!(error, ProviderError::AuthenticationFailed));
        } else {
            assert!(matches!(error, ProviderError::Other(_)));
        }
        let requests = server.received_requests().await.unwrap();
        assert_eq!(count(&requests, "/v1/messages"), expected_posts);
        assert_eq!(count(&requests, "/v1/oauth/token"), refresh_posts);
    }
}

#[tokio::test]
#[serial]
async fn k02_lazy_mint_cooldown_expires_at_five_minutes() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let _env = Env::new(dir.path(), &server.uri());
    let store: Arc<dyn AuthStorePort> = Arc::new(FileAuthStore::new());
    login(&server, &store, NOW - 3_600_000, 3600).await;
    refresh(&server, "LAZY-ACCESS", 200).await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(success())
        .mount(&server)
        .await;
    complete(&server.uri()).await.unwrap();
    server.reset().await;
    refresh(&server, "FORCED-ACCESS", 200).await;
    reject_once(&server, "/v1/messages", "POST", 401).await;
    Env::set("RUSTAIN_TEST_NOW_MS", &(NOW + 299_999).to_string());
    assert!(matches!(
        complete(&server.uri()).await,
        Err(ProviderError::AuthenticationFailed)
    ));
    assert_eq!(
        count(
            &server.received_requests().await.unwrap(),
            "/v1/oauth/token"
        ),
        0
    );
    server.reset().await;
    refresh(&server, "FORCED-ACCESS", 200).await;
    reject_once(&server, "/v1/messages", "POST", 401).await;
    Env::set("RUSTAIN_TEST_NOW_MS", &(NOW + 300_001).to_string());
    complete(&server.uri()).await.unwrap();
    let requests = server.received_requests().await.unwrap();
    assert_eq!(count(&requests, "/v1/oauth/token"), 1);
    assert_eq!(count(&requests, "/v1/messages"), 2);
    assert_eq!(
        requests
            .iter()
            .rfind(|r| r.url.path() == "/v1/messages")
            .unwrap()
            .headers["authorization"],
        "Bearer FORCED-ACCESS"
    );
}

fn version_error(version: &str) -> ResponseTemplate {
    ResponseTemplate::new(400).set_body_json(json!({"type":"error","error":{"type":"invalid_request_error","message":format!("claude_code_version_too_old: version {version} or newer is required")}}))
}
#[tokio::test]
#[serial]
async fn k04_version_adoption_rebuilds_profile_for_stored_and_static_subscription_tokens() {
    for stored in [true, false] {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let _env = Env::new(dir.path(), &server.uri());
        let store: Arc<dyn AuthStorePort> = Arc::new(FileAuthStore::new());
        if stored {
            login(&server, &store, NOW, 3600).await;
        } else {
            Env::set("ANTHROPIC_API_KEY", "sk-ant-oat01-STATIC");
        }
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(success())
            .with_priority(10)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(version_error("2.1.999"))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        complete(&server.uri()).await.unwrap();
        complete(&server.uri()).await.unwrap();
        let requests = server.received_requests().await.unwrap();
        let posts: Vec<_> = requests
            .iter()
            .filter(|r| r.url.path() == "/v1/messages")
            .collect();
        assert_eq!(posts.len(), 3);
        assert_eq!(
            posts[0].headers["user-agent"],
            "claude-cli/2.1.280 (external, cli)"
        );
        for request in &posts[1..] {
            assert_eq!(
                request.headers["user-agent"],
                "claude-cli/2.1.999 (external, cli)"
            );
            let body = request.body_json::<Value>().unwrap();
            // Independent Python JSON + system libxxhash oracle, not the production hasher.
            assert_eq!(
                body["system"][0]["text"],
                if stored {
                    "x-anthropic-billing-header: cc_version=2.1.999.fbb; cc_entrypoint=cli; cch=a2a17;"
                } else {
                    "x-anthropic-billing-header: cc_version=2.1.999.fbb; cc_entrypoint=cli; cch=1e79c;"
                }
            );
            assert_eq!(
                body["messages"],
                json!([{"role":"user","content":[{"type":"text","text":"hi"}]}])
            );
        }
        let old = posts[0].body_json::<Value>().unwrap();
        let new = posts[1].body_json::<Value>().unwrap();
        let checksum = |v: &Value| {
            v["system"][0]["text"]
                .as_str()
                .unwrap()
                .split("cch=")
                .nth(1)
                .unwrap()
                .to_owned()
        };
        assert_ne!(checksum(&old), checksum(&new));
        assert!(!checksum(&new).contains("00000"));
        assert_eq!(posts[1].body, posts[2].body);
        assert_eq!(count(&requests, "/v1/oauth/token"), 0);
    }
}
#[tokio::test]
#[serial]
async fn k04_nonnewer_malformed_pinned_or_plain_keys_never_adopt() {
    for (required, pin, key) in [
        ("2.1.280", None, "sk-ant-oat01-STATIC"),
        ("2.0.5", None, "sk-ant-oat01-STATIC"),
        ("2.1", None, "sk-ant-oat01-STATIC"),
        ("2.1.999", Some("2.1.280"), "sk-ant-oat01-STATIC"),
        ("2.1.999", None, "plain-api-key"),
    ] {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let _env = Env::new(dir.path(), &server.uri());
        Env::set("ANTHROPIC_API_KEY", key);
        if let Some(pin) = pin {
            Env::set("RUSTAIN_CLAUDE_CODE_VERSION", pin);
        }
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(success())
            .with_priority(10)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(version_error(required))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        assert!(matches!(
            complete(&server.uri()).await,
            Err(ProviderError::Other(_))
        ));
        assert_eq!(
            count(&server.received_requests().await.unwrap(), "/v1/messages"),
            1
        );
    }
}
#[tokio::test]
#[serial]
async fn version_increases_can_repeat_but_same_version_stops_and_auth_budget_is_independent() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let _env = Env::new(dir.path(), &server.uri());
    let store: Arc<dyn AuthStorePort> = Arc::new(FileAuthStore::new());
    login(&server, &store, NOW, 3600).await;
    refresh(&server, "ROTATED-ACCESS", 200).await;
    let attempt = Arc::new(AtomicUsize::new(0));
    let seen = attempt.clone();
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            move |_: &wiremock::Request| match seen.fetch_add(1, Ordering::SeqCst) {
                0 => version_error("2.1.999"),
                1 => ResponseTemplate::new(401),
                2 => version_error("2.2.0"),
                _ => version_error("2.2.0"),
            },
        )
        .mount(&server)
        .await;
    assert!(matches!(
        complete(&server.uri()).await,
        Err(ProviderError::Other(_))
    ));
    let requests = server.received_requests().await.unwrap();
    assert_eq!(count(&requests, "/v1/messages"), 4);
    assert_eq!(count(&requests, "/v1/oauth/token"), 1);
    assert_eq!(
        requests
            .iter()
            .rfind(|r| r.url.path() == "/v1/messages")
            .unwrap()
            .headers["user-agent"],
        "claude-cli/2.2.0 (external, cli)"
    );
}

fn binary(dir: &std::path::Path, server: &MockServer, now: i64) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(assert_cmd::cargo::cargo_bin("rustain"));
    command
        .env_clear()
        .env("HOME", dir)
        .env("RUSTAIN_DATA_DIR", dir.join("data"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("XDG_DATA_HOME", dir.join("xdg-data"))
        .env("RUSTAIN_TEST_AUTH_ORIGIN", server.uri())
        .env("ANTHROPIC_BASE_URL", server.uri())
        .env("RUSTAIN_MODELS_DEV_URL", server.uri())
        .env("RUSTAIN_TEST_NOW_MS", now.to_string())
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    command
}
async fn cli_login(dir: &std::path::Path, server: &MockServer, at: i64, account: usize) {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
    server.reset().await;
    // Discovery after login can refresh an older account. This response belongs
    // only to the new authorization-code exchange, never that older refresh.
    Mock::given(method("POST")).and(path("/v1/oauth/token")).and(body_partial_json(json!({"grant_type":"authorization_code"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token":"CLI-ACCESS","refresh_token":"CLI-REFRESH","expires_in":3600,"account":{"uuid":format!("account-{account}"),"email_address":format!("account{account}@example.test")},"organization":{"uuid":"org","name":"Smoke Org"}}))).mount(server).await;
    let mut child = binary(dir, server, at)
        .args(["auth", "login", "anthropic", "--method", "oauth"])
        .spawn()
        .unwrap();
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut transcript = String::new();
    let auth = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            let mut line = String::new();
            assert_ne!(
                stderr.read_line(&mut line).await.unwrap(),
                0,
                "login exited: {transcript}"
            );
            transcript.push_str(&line);
            if line.trim().starts_with("http") {
                break url::Url::parse(line.trim()).unwrap();
            }
        }
    })
    .await
    .unwrap();
    // Preflight the built binary's override BEFORE sending any fixture callback.
    assert_eq!(auth.scheme(), "http", "{auth}");
    assert_eq!(
        auth.host_str(),
        Some("127.0.0.1"),
        "binary lacks test-instrumentation: {auth}"
    );
    assert_eq!(auth.port(), url::Url::parse(&server.uri()).unwrap().port());
    let query: std::collections::HashMap<_, _> = auth.query_pairs().into_owned().collect();
    let mut callback = url::Url::parse(&query["redirect_uri"]).unwrap();
    assert_eq!(callback.host_str(), Some("localhost"));
    callback
        .query_pairs_mut()
        .append_pair("code", "fixture-code")
        .append_pair("state", &query["state"]);
    reqwest::get(callback).await.unwrap();
    let output = tokio::time::timeout(std::time::Duration::from_secs(20), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    stderr.read_to_string(&mut transcript).await.unwrap();
    assert!(output.status.success(), "{transcript}");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        format!(
            "Logged in to Anthropic (Claude Pro/Max) as account{account}@example.test (Smoke Org)"
        )
    );
}
async fn cli_status(dir: &std::path::Path, server: &MockServer, now: i64, json: bool) -> String {
    let mut command = binary(dir, server, now);
    command.args(["auth", "status"]);
    if json {
        command.arg("--json");
    }
    let output = command.output().await.unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}
#[tokio::test]
#[serial]
async fn k05_real_debug_cli_login_produces_grant_warnings_and_json_deadlines() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    for (i, at) in [
        1_797_883_200_000,
        1_797_453_000_000,
        1_797_321_600_000,
        1_799_136_000_000,
    ]
    .into_iter()
    .enumerate()
    {
        cli_login(dir.path(), &server, at, i + 1).await;
    }
    server.reset().await;
    let human = cli_status(dir.path(), &server, NOW, false).await;
    let warnings: Vec<_> = human
        .lines()
        .filter(|line| line.starts_with("  ⚠"))
        .collect();
    assert_eq!(
        warnings,
        [
            "  ⚠ account1@example.test (Smoke Org) — re-login within 5d12h (Anthropic expires OAuth grants ~30d after login)",
            "  ⚠ account2@example.test (Smoke Org) — re-login within 12h30m (Anthropic expires OAuth grants ~30d after login)",
            "  ⚠ account3@example.test (Smoke Org) — grant is past Anthropic's ~30d lifetime; re-login now",
        ]
    );
    let json = cli_status(dir.path(), &server, NOW, true).await;
    let value: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["schema_version"], "2.0");
    assert_eq!(
        value["providers"][0]["oauth_grants"],
        json!([
            {"who":"account1@example.test (Smoke Org)","grant_expires_at":"2027-01-20T20:00:00Z"},
            {"who":"account2@example.test (Smoke Org)","grant_expires_at":"2027-01-15T20:30:00Z"},
            {"who":"account3@example.test (Smoke Org)","grant_expires_at":"2027-01-14T08:00:00Z"},
            {"who":"account4@example.test (Smoke Org)","grant_expires_at":"2027-02-04T08:00:00Z"}
        ])
    );
    assert!(!human.contains('\u{1b}'));
    assert!(server.received_requests().await.unwrap().is_empty());
    println!("REAL DEBUG CLI HUMAN:\n{human}REAL DEBUG CLI JSON:\n{json}");
}

#[tokio::test]
#[serial]
async fn grant_warning_window_duration_branches_disabled_rows_and_empty_json() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    cli_login(dir.path(), &server, 1_797_883_200_000, 1).await;
    // The deadline was produced by the real login, not a handwritten auth row.
    let deadline = 1_800_475_200_000;
    for (remaining, duration) in [
        (604_800_001, None),
        (604_800_000, Some("7d")),
        (90_061_999, Some("1d1h")),
        (3_660_999, Some("1h1m")),
        (3_600_000, Some("1h")),
        (61_999, Some("1m1s")),
        (60_000, Some("1m")),
        (1_500, Some("1.5s")),
        (59_950, Some("1m")),
        (999, Some("999ms")),
    ] {
        let human = cli_status(dir.path(), &server, deadline - remaining, false).await;
        let warning = human.lines().find(|line| line.starts_with("  ⚠"));
        match duration {
            Some(duration) => assert_eq!(warning, Some(format!("  ⚠ account1@example.test (Smoke Org) — re-login within {duration} (Anthropic expires OAuth grants ~30d after login)").as_str())),
            None => assert!(warning.is_none()),
        }
    }
    for now in [deadline, deadline + 1] {
        assert!(cli_status(dir.path(), &server, now, false).await.contains("  ⚠ account1@example.test (Smoke Org) — grant is past Anthropic's ~30d lifetime; re-login now"));
    }
    let store = FileAuthStore::at(dir.path().join("data/auth.json"));
    let snapshot = store.snapshot().await.unwrap();
    let row = &snapshot.rows("anthropic")[0];
    store
        .disable_if_matches("anthropic", row.stamp(), "operator disabled")
        .await
        .unwrap();
    let human = cli_status(dir.path(), &server, deadline, false).await;
    assert!(!human.lines().any(|line| line.starts_with("  ⚠")));
    let json: Value =
        serde_json::from_str(&cli_status(dir.path(), &server, deadline, true).await).unwrap();
    assert!(json["providers"][0].get("oauth_grants").is_none());
}

#[tokio::test]
#[serial]
async fn removed_row_and_new_stale_revision_are_rechecked_under_the_lease() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let _env = Env::new(dir.path(), &server.uri());
    let store: Arc<dyn AuthStorePort> = Arc::new(FileAuthStore::new());
    login(&server, &store, NOW, 3600).await;
    let handle = handle(store.clone());
    let rejected = handle.current().await.unwrap();
    // A peer refresh writes a newer but stale revision in the same login epoch.
    let snapshot = store.snapshot().await.unwrap();
    let row = &snapshot.rows("anthropic")[0];
    let Credential::OAuth(mut stale) = row.credential.clone() else {
        panic!("real login must produce OAuth");
    };
    stale.expires_at_ms = NOW;
    assert_eq!(
        store
            .replace_if_matches("anthropic", row.stamp(), Credential::OAuth(stale))
            .await
            .unwrap(),
        rustain::domain::models::CasOutcome::Applied
    );
    refresh(&server, "NEW-REV-ACCESS", 200).await;
    assert_eq!(
        handle
            .recover(&rejected)
            .await
            .unwrap()
            .unwrap()
            .auth
            .bearer_secret()
            .expose_secret(),
        "NEW-REV-ACCESS"
    );
    assert_eq!(store.snapshot().await.unwrap().rows("anthropic")[0].rev, 3);
    store.remove("anthropic").await.unwrap();
    assert!(handle.recover(&rejected).await.unwrap().is_none());
    assert_eq!(
        count(
            &server.received_requests().await.unwrap(),
            "/v1/oauth/token"
        ),
        1
    );
}

#[tokio::test]
#[serial]
async fn version_errors_do_not_replay_health_probe_or_postcontent_sse() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let _env = Env::new(dir.path(), &server.uri());
    Env::set("ANTHROPIC_API_KEY", "sk-ant-oat01-STATIC");
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(version_error("2.1.999"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(version_error("2.1.999"))
        .mount(&server)
        .await;
    let provider = build_provider_for_config("anthropic", &config(&server.uri())).unwrap();
    assert!(matches!(
        provider.health_check().await,
        Err(ProviderError::Other(_))
    ));
    assert!(matches!(
        provider.connectivity_probe().await,
        Err(ProviderError::Other(_))
    ));
    let requests = server.received_requests().await.unwrap();
    assert_eq!(count(&requests, "/v1/messages"), 1);
    assert_eq!(count(&requests, "/v1/models"), 1);
    server.reset().await;
    Mock::given(method("POST")).and(path("/v1/messages")).respond_with(ResponseTemplate::new(200).set_body_raw(
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\nevent: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"claude_code_version_too_old: version 2.1.999 or newer is required\"}}\n\n", "text/event-stream"
    )).mount(&server).await;
    let chunks = provider
        .stream_completion(messages(), options())
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    assert!(chunks.iter().any(|c| matches!(c, rustain::domain::models::StreamChunk::Text { content, .. } if content == "partial")));
    assert_eq!(
        count(&server.received_requests().await.unwrap(), "/v1/messages"),
        1
    );
    assert_eq!(
        server.received_requests().await.unwrap()[0].headers["user-agent"],
        "claude-cli/2.1.280 (external, cli)"
    );
}

/// Delegates to the real file store; CAS replacements fail while `fail` is set.
struct FailingWrites {
    inner: FileAuthStore,
    fail: std::sync::atomic::AtomicBool,
}
#[async_trait::async_trait]
impl AuthStorePort for FailingWrites {
    async fn snapshot(
        &self,
    ) -> Result<rustain::domain::models::AuthSnapshot, rustain::domain::errors::AuthError> {
        self.inner.snapshot().await
    }
    async fn upsert(
        &self,
        provider: &str,
        cred: Credential,
        origin: rustain::domain::models::CredentialOrigin,
        validated: bool,
    ) -> Result<rustain::domain::models::CredentialId, rustain::domain::errors::AuthError> {
        self.inner.upsert(provider, cred, origin, validated).await
    }
    async fn replace_if_matches(
        &self,
        provider: &str,
        expected: rustain::domain::models::CredentialStamp,
        cred: Credential,
    ) -> Result<rustain::domain::models::CasOutcome, rustain::domain::errors::AuthError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(rustain::domain::errors::AuthError::Io("injected".into()));
        }
        self.inner
            .replace_if_matches(provider, expected, cred)
            .await
    }
    async fn disable_if_matches(
        &self,
        provider: &str,
        expected: rustain::domain::models::CredentialStamp,
        cause: &str,
    ) -> Result<rustain::domain::models::CasOutcome, rustain::domain::errors::AuthError> {
        self.inner
            .disable_if_matches(provider, expected, cause)
            .await
    }
    async fn remove(&self, provider: &str) -> Result<(), rustain::domain::errors::AuthError> {
        self.inner.remove(provider).await
    }
    async fn select(
        &self,
        provider: &str,
        id: rustain::domain::models::CredentialId,
    ) -> Result<rustain::domain::models::SelectOutcome, rustain::domain::errors::AuthError> {
        self.inner.select(provider, id).await
    }
    async fn remove_row(
        &self,
        provider: &str,
        id: rustain::domain::models::CredentialId,
    ) -> Result<rustain::domain::models::RowRemoval, rustain::domain::errors::AuthError> {
        self.inner.remove_row(provider, id).await
    }
    async fn refresh_lease(
        &self,
    ) -> Result<rustain::domain::ports::RefreshLease, rustain::domain::errors::AuthError> {
        self.inner.refresh_lease().await
    }
}

#[tokio::test]
#[serial]
async fn unpersisted_mint_leaves_the_unchanged_stored_bearer_recoverable() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let _env = Env::new(dir.path(), &server.uri());
    let file: Arc<dyn AuthStorePort> = Arc::new(FileAuthStore::new());
    login(&server, &file, NOW, 3600).await;
    refresh(&server, "ROTATED-ACCESS", 200).await;
    let store = Arc::new(FailingWrites {
        inner: FileAuthStore::new(),
        fail: std::sync::atomic::AtomicBool::new(true),
    });
    let handle = handle(store.clone());
    let rejected = handle.current().await.unwrap();
    // Both writes fail: this request still gets the minted bearer; the disk keeps rev 1.
    assert_eq!(
        handle
            .recover(&rejected)
            .await
            .unwrap()
            .unwrap()
            .auth
            .bearer_secret()
            .expose_secret(),
        "ROTATED-ACCESS"
    );
    store.fail.store(false, Ordering::SeqCst);
    let stored = handle.current().await.unwrap();
    assert_eq!(stored.auth.bearer_secret().expose_secret(), "LOGIN-ACCESS");
    // The stored bearer was never minted by this process, so its 401 refreshes again.
    assert_eq!(
        handle
            .recover(&stored)
            .await
            .unwrap()
            .unwrap()
            .auth
            .bearer_secret()
            .expose_secret(),
        "ROTATED-ACCESS"
    );
    assert_eq!(
        count(
            &server.received_requests().await.unwrap(),
            "/v1/oauth/token"
        ),
        2
    );
    assert_eq!(file.snapshot().await.unwrap().rows("anthropic")[0].rev, 2);
}
