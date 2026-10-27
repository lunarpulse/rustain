use futures::future::BoxFuture;
use rustain::adapters::auth::{
    flows::{self, FlowCtx, LoginUi},
    policies::ANTHROPIC_OAUTH,
};
use rustain::adapters::auth_store::FileAuthStore;
use rustain::domain::models::{
    AuthMethod, CasOutcome, Credential, CredentialGeneration, CredentialOrigin, CredentialStamp,
    SecretString,
};
const OAUTH: AuthMethod = AuthMethod::OAuthCode(&ANTHROPIC_OAUTH);
use rustain::domain::ports::AuthStorePort;
use serial_test::serial;
use std::ffi::OsString;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_partial_json, method, path},
};

struct Env(Vec<(&'static str, Option<OsString>)>);
impl Env {
    fn set(values: &[(&'static str, &str)]) -> Self {
        let old = values
            .iter()
            .map(|(k, v)| {
                let old = std::env::var_os(k);
                unsafe {
                    std::env::set_var(k, v);
                }
                (*k, old)
            })
            .collect();
        Self(old)
    }
}
impl Drop for Env {
    fn drop(&mut self) {
        for (k, v) in self.0.drain(..) {
            unsafe {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }
}

struct Ui {
    urls: mpsc::UnboundedSender<(String, String)>,
    paste: Option<Arc<Mutex<mpsc::UnboundedReceiver<String>>>>,
    secret: Option<String>,
    cancel: CancellationToken,
    paste_prompts: std::sync::atomic::AtomicUsize,
    reprompt: tokio::sync::Semaphore,
}
impl LoginUi for Ui {
    fn on_auth_url(&self, url: &str, launch: &str, _: &str) {
        self.urls.send((url.into(), launch.into())).unwrap();
    }
    fn on_device_code(&self, _: &str, _: &str, _: &str) {
        panic!("browser login cannot show a device code");
    }
    fn on_progress(&self, _: &str) {}
    fn prompt_secret(&self, _: &str) -> BoxFuture<'_, Option<SecretString>> {
        Box::pin(async { self.secret.clone().map(SecretString::new) })
    }
    fn manual_input(&self, _: &str) -> Option<BoxFuture<'static, Option<String>>> {
        if self.paste.is_some()
            && self
                .paste_prompts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                > 0
        {
            self.reprompt.add_permits(1);
        }
        self.paste.clone().map(|p| {
            Box::pin(async move { p.lock().await.recv().await })
                as BoxFuture<'static, Option<String>>
        })
    }
    fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }
}
fn recording_ui(
    paste: bool,
) -> (
    Arc<Ui>,
    mpsc::UnboundedReceiver<(String, String)>,
    mpsc::UnboundedSender<String>,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    let (ptx, prx) = mpsc::unbounded_channel();
    (
        Arc::new(Ui {
            urls: tx,
            paste: paste.then(|| Arc::new(Mutex::new(prx))),
            secret: Some("sk-test-key".into()),
            cancel: CancellationToken::new(),
            paste_prompts: std::sync::atomic::AtomicUsize::new(0),
            reprompt: tokio::sync::Semaphore::new(0),
        }),
        rx,
        ptx,
    )
}
fn ctx(ui: Arc<Ui>, now: i64) -> FlowCtx {
    FlowCtx::new(ui, Arc::new(move || now))
}
fn callback(url: &str, code: &str, state: Option<&str>) -> String {
    let auth = url::Url::parse(url).unwrap();
    let query: std::collections::HashMap<_, _> = auth.query_pairs().into_owned().collect();
    let mut cb = url::Url::parse(&query["redirect_uri"]).unwrap();
    {
        let mut q = cb.query_pairs_mut();
        q.append_pair("code", code);
        if let Some(s) = state {
            q.append_pair("state", s);
        }
    }
    cb.into()
}
fn state(url: &str) -> String {
    url::Url::parse(url)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == "state")
        .unwrap()
        .1
        .into_owned()
}
async fn token_mock(server: &MockServer, expires_in: i64, identity: serde_json::Value) {
    let mut response = serde_json::json!({"access_token":"LOGIN-ACCESS-CANARY","refresh_token":"LOGIN-REFRESH-CANARY","expires_in":expires_in});
    if let Some(identity) = identity.as_object() {
        for (k, v) in identity {
            response[k] = v.clone();
        }
    }
    Mock::given(method("POST"))
        .and(path("/v1/oauth/token"))
        .and(body_partial_json(
            serde_json::json!({"grant_type":"authorization_code"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(response))
        .mount(server)
        .await;
}
pub(super) async fn login_oauth(
    server: &MockServer,
    store: &Arc<dyn AuthStorePort>,
    now: i64,
    expires_in: i64,
    identity: serde_json::Value,
) {
    token_mock(server, expires_in, identity).await;
    let (ui, mut urls, _) = recording_ui(false);
    let context = ctx(ui, now);
    let login = flows::login("anthropic", &OAUTH, &context, store);
    let browser = async {
        let (url, _) = urls.recv().await.unwrap();
        let authorize = url::Url::parse(&url).unwrap();
        assert_eq!(authorize.host_str(), Some("127.0.0.1"));
        assert_eq!(
            authorize.origin(),
            url::Url::parse(&server.uri()).unwrap().origin()
        );
        reqwest::get(callback(&url, "test-code", Some(&state(&url))))
            .await
            .unwrap();
    };
    let (result, ()) = tokio::join!(login, browser);
    result.unwrap();
}
fn store(temp: &tempfile::TempDir) -> Arc<dyn AuthStorePort> {
    Arc::new(FileAuthStore::at(temp.path().join("auth.json")))
}

#[tokio::test]
#[serial]
async fn k01_k02_k03_pkce_csrf_launch_and_release() {
    use base64::Engine;
    use sha2::{Digest, Sha256};
    let server = MockServer::start().await;
    let _env = Env::set(&[("RUSTAIN_TEST_AUTH_ORIGIN", &server.uri())]);
    token_mock(&server,3600,serde_json::json!({"account":{"uuid":"acct","email_address":"a@b"},"organization":{"uuid":"org"}})).await;
    let temp = tempfile::tempdir().unwrap();
    let port = store(&temp);
    let (ui, mut urls, _) = recording_ui(false);
    let context = ctx(ui, 1000);
    let browser = async {
        let (auth, launch) = urls.recv().await.unwrap();
        let s = state(&auth);
        assert_eq!(s.len(), 32);
        assert!(
            s.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let response = client.get(&launch).send().await.unwrap();
        assert_eq!(response.status(), 302);
        assert_eq!(response.headers()["location"], auth);
        assert_eq!(
            client
                .get(launch.replace("/launch", "/other"))
                .send()
                .await
                .unwrap()
                .status(),
            404
        );
        assert!(
            client
                .get(callback(&auth, "forged", Some("wrong")))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
                .contains("State mismatch - possible CSRF attack")
        );
        client
            .get(callback(&auth, "genuine", Some(&s)))
            .send()
            .await
            .unwrap();
        auth
    };
    let (result, auth) = tokio::join!(flows::login("anthropic", &OAUTH, &context, &port), browser);
    result.unwrap();
    let requests = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    let verifier = body["code_verifier"].as_str().unwrap();
    assert_eq!(verifier.len(), 128);
    assert_ne!(verifier, state(&auth));
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    let params: Vec<_> = url::Url::parse(&auth)
        .unwrap()
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    assert_eq!(
        params.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
        [
            "client_id",
            "response_type",
            "redirect_uri",
            "scope",
            "code_challenge",
            "code_challenge_method",
            "state",
            "code"
        ]
    );
    assert_eq!(params[4].1, challenge);
    let s = state(&auth);
    let authorize = url::Url::parse(&auth).unwrap();
    assert_eq!(authorize.path(), "/oauth/authorize");
    let redirect_uri = params[2].1.as_str();
    assert!(redirect_uri.starts_with("http://localhost:") && redirect_uri.ends_with("/callback"));
    for (index, value) in [
        (0, "9d1c250a-e61b-44d9-88ed-5944d1962f5e"),
        (1, "code"),
        (
            3,
            "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload",
        ),
        (5, "S256"),
        (6, s.as_str()),
        (7, "true"),
    ] {
        assert_eq!(params[index].1, value, "authorize {}", params[index].0);
    }
    // Exactly one exchange, bound to this authorization's redirect, verifier and state.
    assert_eq!(requests.len(), 1);
    assert_eq!(
        body,
        serde_json::json!({
            "grant_type": "authorization_code",
            "client_id": "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
            "code": "genuine",
            "redirect_uri": redirect_uri,
            "code_verifier": verifier,
            "state": s,
        })
    );
    let redirect = url::Url::parse(body["redirect_uri"].as_str().unwrap()).unwrap();
    assert!(
        tokio::net::TcpListener::bind(("127.0.0.1", redirect.port().unwrap()))
            .await
            .is_ok()
    );
}

#[tokio::test]
#[serial]
async fn token_endpoint_redirect_never_resends_code_or_verifier() {
    let server = MockServer::start().await;
    let sink = MockServer::start().await;
    let _env = Env::set(&[("RUSTAIN_TEST_AUTH_ORIGIN", &server.uri())]);
    // A 307 from the policy origin to another origin would replay the exchange body there.
    Mock::given(method("POST"))
        .and(path("/v1/oauth/token"))
        .respond_with(
            ResponseTemplate::new(307)
                .insert_header("location", format!("{}/v1/oauth/token", sink.uri())),
        )
        .expect(1)
        .mount(&server)
        .await;
    token_mock(&sink, 3600, serde_json::json!({"account":{"uuid":"sink","email_address":"sink@example.test"},"organization":{"uuid":"sink-org"}})).await;
    let temp = tempfile::tempdir().unwrap();
    let port = store(&temp);
    let (ui, mut urls, _) = recording_ui(false);
    let context = ctx(ui, 0);
    let browser = async {
        let (url, _) = urls.recv().await.unwrap();
        reqwest::get(callback(&url, "REDIRECT-CODE-CANARY", Some(&state(&url))))
            .await
            .unwrap();
    };
    let (result, ()) = tokio::join!(flows::login("anthropic", &OAUTH, &context, &port), browser);
    assert_eq!(
        result.unwrap_err().to_string(),
        "OAuth token exchange failed (HTTP 307)"
    );
    assert!(sink.received_requests().await.unwrap().is_empty());
    assert!(port.snapshot().await.unwrap().rows("anthropic").is_empty());
}

#[tokio::test]
#[serial]
async fn k04_all_paste_shapes_and_wrong_state() {
    for shape in 0..5 {
        let server = MockServer::start().await;
        let _env = Env::set(&[("RUSTAIN_TEST_AUTH_ORIGIN", &server.uri())]);
        token_mock(&server, 3600, serde_json::json!({})).await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let temp = tempfile::tempdir().unwrap();
        let port = store(&temp);
        let (ui, mut urls, paste) = recording_ui(true);
        let context = ctx(ui.clone(), 0);
        let mut login = Box::pin(flows::login("anthropic", &OAUTH, &context, &port));
        let (auth, _) = tokio::select! {
            result = &mut login => panic!("login finished before authorization: {result:?}"),
            auth = urls.recv() => auth.unwrap(),
        };
        paste.send("forged-code#WRONG".into()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(20), async {
            tokio::select! {
                result = &mut login => panic!("wrong-state paste completed login: {result:?}"),
                permit = ui.reprompt.acquire() => permit.unwrap().forget(),
            }
        })
        .await
        .expect("invalid paste must structurally re-prompt");
        assert!(
            !server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .any(|request| request.method.as_str() == "POST"),
            "wrong-state paste must not send an exchange"
        );
        let s = state(&auth);
        let value = match shape {
            0 => callback(&auth, "genuine-code", Some(&s)),
            1 => format!("code=genuine-code&state={s}"),
            2 => format!("genuine-code#{s}"),
            3 => format!("?state={s}&code=genuine-code"),
            _ => "genuine-code".into(),
        };
        paste.send(value).unwrap();
        login.await.unwrap();
        let requests = server.received_requests().await.unwrap();
        let posts: Vec<_> = requests
            .iter()
            .filter(|r| r.method.as_str() == "POST")
            .collect();
        assert_eq!(posts.len(), 1);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&posts[0].body).unwrap()["code"],
            "genuine-code"
        );
    }
}

#[tokio::test]
#[serial]
async fn k05_projection_origin_and_org_scoped_relogin() {
    let server = MockServer::start().await;
    let _env = Env::set(&[("RUSTAIN_TEST_AUTH_ORIGIN", &server.uri())]);
    let temp = tempfile::tempdir().unwrap();
    let port = store(&temp);
    login_oauth(&server,&port,123456,3600,serde_json::json!({"account":{"uuid":"acct","email_address":"Me@Example.com"},"organization":{"uuid":"org-1","name":"One"}})).await;
    let snap = port.snapshot().await.unwrap();
    let row = &snap.rows("anthropic")[0];
    assert_eq!(row.origin, CredentialOrigin::Login);
    let Credential::OAuth(o) = &row.credential else {
        panic!()
    };
    assert_eq!(o.expires_at_ms, 3423456);
    assert_eq!(o.authorized_at_ms, Some(123456));
    assert_eq!(o.identity.org_name.as_deref(), Some("One"));
    server.reset().await;
    login_oauth(&server,&port,123457,3600,serde_json::json!({"account":{"uuid":"acct","email_address":"Me@Example.com"},"organization":{"uuid":"org-2","name":"Two"}})).await;
    assert_eq!(port.snapshot().await.unwrap().rows("anthropic").len(), 2);
    let before_relogin = port.snapshot().await.unwrap().rows("anthropic")[1].clone();
    assert_ne!(before_relogin.generation, CredentialGeneration::default());
    server.reset().await;
    login_oauth(&server,&port,123458,3600,serde_json::json!({"account":{"uuid":"acct","email_address":"Me@Example.com"},"organization":{"uuid":"org-2","name":"Two"}})).await;
    assert_eq!(port.snapshot().await.unwrap().rows("anthropic").len(), 2);
    let after_relogin = port.snapshot().await.unwrap().rows("anthropic")[1].clone();
    assert_eq!(after_relogin.id, before_relogin.id);
    assert_eq!(after_relogin.rev, before_relogin.rev + 1);
    assert_ne!(after_relogin.generation, CredentialGeneration::default());
    assert_ne!(after_relogin.generation, before_relogin.generation);
    let stale_generation = CredentialStamp {
        generation: before_relogin.generation,
        ..after_relogin.stamp()
    };
    let path = temp.path().join("auth.json");
    let before = std::fs::read(&path).unwrap();
    assert_eq!(
        port.replace_if_matches("anthropic", stale_generation, before_relogin.credential)
            .await
            .unwrap(),
        CasOutcome::Stale
    );
    assert_eq!(
        port.disable_if_matches("anthropic", stale_generation, "old grant revoked")
            .await
            .unwrap(),
        CasOutcome::Stale
    );
    assert_eq!(std::fs::read(path).unwrap(), before);
}

#[tokio::test]
#[serial]
async fn k06_bootstrap_headers_and_swallowed_failure() {
    let server = MockServer::start().await;
    let _env = Env::set(&[
        ("RUSTAIN_TEST_AUTH_ORIGIN", &server.uri()),
        ("RUSTAIN_CLAUDE_CODE_VERSION", "9.8.7"),
    ]);
    let temp = tempfile::tempdir().unwrap();
    let port = store(&temp);
    Mock::given(method("GET")).and(path("/api/claude_cli/bootstrap")).respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"oauth_account":{"account_uuid":"bootstrap-account","account_email":"b@c","organization_uuid":"boot-org","organization_name":"Boot"}}))).mount(&server).await;
    login_oauth(&server, &port, 1, 3600, serde_json::json!({})).await;
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(
        reqs.iter().filter(|r| r.method.as_str() == "GET").count(),
        1
    );
    let bootstrap = reqs.iter().find(|r| r.method.as_str() == "GET").unwrap();
    assert_eq!(bootstrap.url.path(), "/api/claude_cli/bootstrap");
    assert_eq!(bootstrap.headers["user-agent"], "claude-code/9.8.7");
    assert_eq!(
        bootstrap.headers["authorization"],
        "Bearer LOGIN-ACCESS-CANARY"
    );
    assert_eq!(bootstrap.headers["anthropic-beta"], "oauth-2025-04-20");
    assert_eq!(
        bootstrap.headers["accept"],
        "application/json, text/plain, */*"
    );
    assert_eq!(bootstrap.headers["content-type"], "application/json");
    assert_eq!(
        bootstrap.url.query(),
        Some("entrypoint=cli&model=claude-opus-4-8")
    );
    let snap = port.snapshot().await.unwrap();
    let Credential::OAuth(o) = &snap.rows("anthropic")[0].credential else {
        panic!()
    };
    assert_eq!(o.identity.account_id.as_deref(), Some("bootstrap-account"));
    assert_eq!(o.identity.email.as_deref(), Some("b@c"));
    assert_eq!(o.identity.org_id.as_deref(), Some("boot-org"));
    assert_eq!(o.identity.org_name.as_deref(), Some("Boot"));
    server.reset().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    login_oauth(&server, &port, 2, 3600, serde_json::json!({})).await;
}

#[tokio::test]
#[serial]
async fn cancel_and_drop_release_listener() {
    let server = MockServer::start().await;
    let _env = Env::set(&[("RUSTAIN_TEST_AUTH_ORIGIN", &server.uri())]);
    let temp = tempfile::tempdir().unwrap();
    let port = store(&temp);
    let (ui, mut urls, _) = recording_ui(false);
    let context = ctx(ui.clone(), 0);
    let cancel = async {
        let (auth, _) = urls.recv().await.unwrap();
        ui.cancel.cancel();
        auth
    };
    let (result, auth) = tokio::join!(flows::login("anthropic", &OAUTH, &context, &port), cancel);
    assert!(result.is_err());
    let redirect = url::Url::parse(&callback(&auth, "x", None)).unwrap();
    assert!(
        tokio::net::TcpListener::bind(("127.0.0.1", redirect.port().unwrap()))
            .await
            .is_ok()
    );
}

fn validation_config(server: &MockServer) -> rustain::domain::models::ProviderConfig {
    rustain::domain::models::ProviderConfig {
        provider_id: "anthropic".into(),
        model_id: "claude-opus-4-8".into(),
        api_key_env: "FLOW_ENV_KEY".into(),
        enabled: true,
        kind: Some("anthropic".into()),
        base_url: Some(server.uri().into()),
        auth_header: None,
        context_window: None,
        supports_tools: None,
        discover_models: false,
        model_filter: vec![],
        cache_ttl_seconds: 0,
    }
}

#[tokio::test]
#[serial]
async fn k07_api_key_frontdoor_validation_precedence_subscription_and_decline() {
    let server = MockServer::start().await;
    let _env = Env::set(&[
        ("FLOW_ENV_KEY", "ENV-MUST-LOSE"),
        ("ANTHROPIC_BASE_URL", &server.uri()),
    ]);
    let temp = tempfile::tempdir().unwrap();
    let port = store(&temp);
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let (ui, _, _) = recording_ui(false);
    let context = ctx(ui, 0).with_validation_config(validation_config(&server));
    let result = flows::login("anthropic", &AuthMethod::ApiKey, &context, &port)
        .await
        .unwrap();
    assert!(result.validated);
    let snapshot = port.snapshot().await.unwrap();
    assert_eq!(
        snapshot.rows("anthropic")[0].origin,
        CredentialOrigin::Login
    );
    let resolver = rustain::infrastructure::key_resolver::compose(
        port.clone(),
        Arc::new(flows::refresh::HttpOAuthRefresher::new()),
        Arc::new(|| 0),
    );
    let resolved = resolver
        .resolve(
            "anthropic",
            None,
            &["FLOW_ENV_KEY"],
            &rustain::infrastructure::utils::env_var_trimmed,
        )
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(&resolved.1,rustain::domain::models::ResolvedAuth::ApiKey(key) if key.expose_secret()=="sk-test-key")
    );
    // The same UI's non-yes answer declines the overwrite before probing or writing.
    assert!(matches!(
        flows::login("anthropic", &AuthMethod::ApiKey, &context, &port).await,
        Err(flows::FlowError::Declined)
    ));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    port.remove("anthropic").await.unwrap();
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let (mut subscription, _, _) = recording_ui(false);
    Arc::get_mut(&mut subscription).unwrap().secret = Some("sk-ant-oat-subscription-canary".into());
    let context = ctx(subscription, 0).with_validation_config(validation_config(&server));
    let result = flows::login("anthropic", &AuthMethod::ApiKey, &context, &port)
        .await
        .unwrap();
    assert!(!result.validated);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests[0].headers["authorization"],
        "Bearer sk-ant-oat-subscription-canary"
    );
    assert_eq!(requests[0].headers["x-app"], "cli");
    assert!(requests[0].headers.get("x-api-key").is_none());
    assert!(
        port.snapshot().await.unwrap().rows("anthropic")[0]
            .last_validated
            .is_none()
    );
}

#[tokio::test]
#[serial]
async fn api_key_invalid_offline_and_custom_provider_preserve_store() {
    let server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();
    let port = store(&temp);
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(401).set_body_string("sk-test-key"))
        .mount(&server)
        .await;
    let (ui, _, _) = recording_ui(false);
    let context = ctx(ui, 0).with_validation_config(validation_config(&server));
    let error = flows::login("anthropic", &AuthMethod::ApiKey, &context, &port)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("Invalid API key"));
    assert!(error.contains("https://"));
    assert!(!error.contains("sk-test-key"));
    assert!(port.snapshot().await.unwrap().rows("anthropic").is_empty());
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let mut config = validation_config(&server);
    config.kind = Some("openai-compatible".into());
    config.provider_id = "custom".into();
    config.base_url = Some(format!("{}/v1", server.uri()).into());
    let (ui, _, _) = recording_ui(false);
    let context = ctx(ui, 0).with_validation_config(config);
    assert!(
        flows::login("custom", &AuthMethod::ApiKey, &context, &port)
            .await
            .unwrap()
            .validated
    );
    assert_eq!(
        port.snapshot().await.unwrap().rows("custom")[0].origin,
        CredentialOrigin::Login
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let mut config = validation_config(&server);
    config.base_url = Some(format!("http://{address}").into());
    let (ui, _, _) = recording_ui(false);
    let context = ctx(ui, 0).with_validation_config(config);
    assert!(
        flows::login("anthropic", &AuthMethod::ApiKey, &context, &port)
            .await
            .is_err()
    );
    assert!(port.snapshot().await.unwrap().rows("anthropic").is_empty());
}

#[tokio::test]
#[serial]
async fn callback_errors_require_state_and_large_heads_are_closed() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let server = MockServer::start().await;
    let _env = Env::set(&[("RUSTAIN_TEST_AUTH_ORIGIN", &server.uri())]);
    let temp = tempfile::tempdir().unwrap();
    let port = store(&temp);
    let (ui, mut urls, _) = recording_ui(false);
    let context = ctx(ui, 0);
    let browser = async {
        let (auth, launch) = urls.recv().await.unwrap();
        let redirect = url::Url::parse(&callback(&auth, "x", None)).unwrap();
        let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", redirect.port().unwrap()))
            .await
            .unwrap();
        socket
            .write_all(
                format!(
                    "GET /callback HTTP/1.1\r\nX-Large: {}\r\n\r\n",
                    "x".repeat(8192)
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut response = Vec::new();
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            socket.read_to_end(&mut response),
        )
        .await
        .unwrap();
        assert!(response.is_empty());
        let mut error = redirect.clone();
        error.set_query(Some("error=access_denied"));
        assert_eq!(reqwest::get(error.as_str()).await.unwrap().status(), 400);
        // The missing state must not terminate the flow.
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        assert_eq!(client.get(launch).send().await.unwrap().status(), 302);
        error.query_pairs_mut().append_pair("state", &state(&auth));
        reqwest::get(error).await.unwrap();
    };
    let (result, ()) = tokio::join!(flows::login("anthropic", &OAUTH, &context, &port), browser);
    assert!(result.unwrap_err().to_string().contains("denied"));
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
#[serial]
async fn pinned_port_busy_is_configuration_error() {
    static PINNED: rustain::domain::models::auth_policy::OAuthCodePolicy =
        rustain::domain::models::auth_policy::OAuthCodePolicy {
            port: 54546,
            port_fallback: false,
            ..ANTHROPIC_OAUTH
        };
    let _listener = tokio::net::TcpListener::bind(("127.0.0.1", 54546))
        .await
        .unwrap();
    let temp = tempfile::tempdir().unwrap();
    let port = store(&temp);
    let (ui, _, _) = recording_ui(false);
    let context = ctx(ui, 0);
    assert!(
        flows::login(
            "anthropic",
            &AuthMethod::OAuthCode(&PINNED),
            &context,
            &port
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("bind")
    );
}

#[tokio::test]
#[serial]
async fn ipv6_companion_busy_redraws_both_listeners() {
    let held = match tokio::net::TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, 54545)).await {
        Ok(v) => v,
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::AddrNotAvailable | std::io::ErrorKind::Unsupported
            ) =>
        {
            return;
        }
        Err(e) => panic!("{e}"),
    };
    let server = MockServer::start().await;
    let _env = Env::set(&[("RUSTAIN_TEST_AUTH_ORIGIN", &server.uri())]);
    token_mock(&server,3600,serde_json::json!({"account":{"uuid":"a","email_address":"a@b"},"organization":{"uuid":"o"}})).await;
    let temp = tempfile::tempdir().unwrap();
    let port = store(&temp);
    let (ui, mut urls, _) = recording_ui(false);
    let context = ctx(ui, 0);
    let browser = async {
        let (auth, _) = urls.recv().await.unwrap();
        let callback = callback(&auth, "x", Some(&state(&auth)));
        let parsed = url::Url::parse(&callback).unwrap();
        assert_ne!(parsed.port(), Some(54545));
        let mut v6 = parsed.clone();
        v6.set_host(Some("[::1]")).unwrap();
        reqwest::get(v6).await.unwrap();
    };
    let (result, ()) = tokio::join!(flows::login("anthropic", &OAUTH, &context, &port), browser);
    result.unwrap();
    drop(held);
    assert!(
        tokio::net::TcpListener::bind(("127.0.0.1", 54545))
            .await
            .is_ok()
    );
}

#[tokio::test]
#[serial]
async fn launch_stays_alive_through_gated_exchange() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let token_server = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let origin = format!("http://{}", token_server.local_addr().unwrap());
    let _env = Env::set(&[("RUSTAIN_TEST_AUTH_ORIGIN", &origin)]);
    let (arrived_tx, arrived_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let exchange = async move {
        let (mut socket, _) = token_server.accept().await.unwrap();
        let mut request = Vec::new();
        let mut end = None;
        let mut length = 0;
        loop {
            let mut bytes = [0; 1024];
            let count = socket.read(&mut bytes).await.unwrap();
            assert_ne!(count, 0);
            request.extend_from_slice(&bytes[..count]);
            if end.is_none() {
                if let Some(i) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    end = Some(i + 4);
                    length = std::str::from_utf8(&request[..i])
                        .unwrap()
                        .lines()
                        .find_map(|line| {
                            line.split_once(':')
                                .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                                .map(|(_, v)| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap();
                }
            }
            if end.is_some_and(|end| request.len() >= end + length) {
                break;
            }
        }
        arrived_tx.send(()).unwrap();
        release_rx.await.unwrap();
        let body = r#"{"access_token":"gated-access","refresh_token":"gated-refresh","expires_in":3600,"account":{"uuid":"a","email_address":"a@b"},"organization":{"uuid":"o"}}"#;
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
    };
    let temp = tempfile::tempdir().unwrap();
    let port = store(&temp);
    let (ui, mut urls, _) = recording_ui(false);
    let context = ctx(ui, 0);
    let browser = async {
        let (auth, launch) = urls.recv().await.unwrap();
        reqwest::get(callback(&auth, "gated-code", Some(&state(&auth))))
            .await
            .unwrap();
        arrived_rx.await.unwrap();
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let response = client.get(&launch).send().await.unwrap();
        assert_eq!(response.status(), 302);
        assert_eq!(response.headers()["location"], auth);
        release_tx.send(()).unwrap();
    };
    let (result, (), ()) = tokio::join!(
        flows::login("anthropic", &OAUTH, &context, &port),
        browser,
        exchange
    );
    result.unwrap();
}

#[tokio::test]
#[serial]
async fn dropping_pending_login_and_whole_flow_deadline_release_port() {
    let server = MockServer::start().await;
    let _env = Env::set(&[("RUSTAIN_TEST_AUTH_ORIGIN", &server.uri())]);
    let temp = tempfile::tempdir().unwrap();
    let port = store(&temp);
    let (ui, mut urls, _) = recording_ui(false);
    let context = ctx(ui, 0);
    let mut login = Box::pin(flows::login("anthropic", &OAUTH, &context, &port));
    let auth = tokio::select! {result=&mut login=>panic!("unexpected result {result:?}"),url=urls.recv()=>url.unwrap().0};
    drop(login);
    let address = url::Url::parse(&callback(&auth, "x", None)).unwrap();
    assert!(
        tokio::net::TcpListener::bind(("127.0.0.1", address.port().unwrap()))
            .await
            .is_ok()
    );
    let (ui, mut urls, _) = recording_ui(false);
    let context = ctx(ui, 0);
    let mut login = Box::pin(flows::login("anthropic", &OAUTH, &context, &port));
    let auth = tokio::select! {result=&mut login=>panic!("unexpected result {result:?}"),url=urls.recv()=>url.unwrap().0};
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(301)).await;
    assert!(login.await.unwrap_err().to_string().contains("300"));
    tokio::time::resume();
    let address = url::Url::parse(&callback(&auth, "x", None)).unwrap();
    assert!(
        tokio::net::TcpListener::bind(("127.0.0.1", address.port().unwrap()))
            .await
            .is_ok()
    );
}

#[tokio::test]
#[serial]
async fn token_projection_and_provider_errors_are_fixed_and_redacted() {
    for body in [
        serde_json::json!({"refresh_token":"TOKEN-CANARY","expires_in":3600}),
        serde_json::json!({"access_token":"TOKEN-CANARY","expires_in":"3600"}),
        serde_json::json!({"error":"invalid_grant","error_description":"TOKEN-CANARY CODE-CANARY STATE-CANARY"}),
    ] {
        let server = MockServer::start().await;
        let _env = Env::set(&[("RUSTAIN_TEST_AUTH_ORIGIN", &server.uri())]);
        let status = if body.get("error").is_some() {
            400
        } else {
            200
        };
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .mount(&server)
            .await;
        let temp = tempfile::tempdir().unwrap();
        let port = store(&temp);
        let (ui, mut urls, _) = recording_ui(false);
        let context = ctx(ui, 0);
        let browser = async {
            let (auth, _) = urls.recv().await.unwrap();
            reqwest::get(callback(&auth, "CODE-CANARY", Some(&state(&auth))))
                .await
                .unwrap();
        };
        let (result, ()) =
            tokio::join!(flows::login("anthropic", &OAUTH, &context, &port), browser);
        let error = result.unwrap_err().to_string();
        for secret in ["TOKEN-CANARY", "CODE-CANARY", "STATE-CANARY"] {
            assert!(!error.contains(secret));
        }
        assert!(port.snapshot().await.unwrap().rows("anthropic").is_empty());
    }
}

#[tokio::test]
#[serial]
async fn k15_logout_removes_every_row_and_is_idempotent_without_changing_env() {
    let server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();
    let _env = Env::set(&[
        ("RUSTAIN_TEST_AUTH_ORIGIN", &server.uri()),
        ("ANTHROPIC_API_KEY", "ENV-SURVIVES"),
    ]);
    let port = store(&temp);
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let (ui, _, _) = recording_ui(false);
    let context = ctx(ui, 0).with_validation_config(validation_config(&server));
    flows::login("anthropic", &AuthMethod::ApiKey, &context, &port)
        .await
        .unwrap();
    login_oauth(&server,&port,0,3600,serde_json::json!({"account":{"uuid":"a","email_address":"a@b"},"organization":{"uuid":"org-1"}})).await;
    let first = port
        .snapshot()
        .await
        .unwrap()
        .rows("anthropic")
        .iter()
        .find(|row| matches!(row.credential, Credential::OAuth(_)))
        .unwrap()
        .clone();
    port.disable_if_matches(
        "anthropic",
        first.stamp(),
        "oauth refresh failed: invalid_grant",
    )
    .await
    .unwrap();
    server.reset().await;
    login_oauth(&server,&port,0,3600,serde_json::json!({"account":{"uuid":"a","email_address":"a@b"},"organization":{"uuid":"org-2"}})).await;
    assert_eq!(port.snapshot().await.unwrap().rows("anthropic").len(), 3);
    rustain::adapters::cli::auth::logout::run_auth_logout("anthropic", false, &port)
        .await
        .unwrap();
    let raw: serde_json::Value =
        serde_json::from_slice(&std::fs::read(temp.path().join("auth.json")).unwrap()).unwrap();
    assert!(raw["providers"].get("anthropic").is_none());
    assert_eq!(
        std::env::var_os("ANTHROPIC_API_KEY"),
        Some(OsString::from("ENV-SURVIVES"))
    );
    rustain::adapters::cli::auth::logout::run_auth_logout("anthropic", true, &port)
        .await
        .unwrap();
}

fn binary_command(temp: &tempfile::TempDir, server: &MockServer) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(assert_cmd::cargo::cargo_bin("rustain"));
    command
        .env_clear()
        .env("HOME", temp.path())
        .env("RUSTAIN_DATA_DIR", temp.path().join("data"))
        .env("XDG_CONFIG_HOME", temp.path().join("config"))
        .env("XDG_DATA_HOME", temp.path().join("xdg-data"))
        .env("RUSTAIN_TEST_AUTH_ORIGIN", server.uri())
        .env("ANTHROPIC_BASE_URL", server.uri())
        .env("RUSTAIN_MODELS_DEV_URL", server.uri())
        .current_dir(temp.path())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    command
}

#[tokio::test]
#[serial]
async fn k16_debug_binary_login_status_ask_smoke() {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
    let server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();
    token_mock(&server,3600,serde_json::json!({"account":{"uuid":"binary-account","email_address":"operator@example.test"},"organization":{"uuid":"binary-org","name":"Smoke Org"}})).await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&server)
        .await;
    let sse = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello smoke\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
        .mount(&server)
        .await;
    let mut command = binary_command(&temp, &server);
    command.args(["auth", "login", "anthropic", "--method", "oauth"]);
    let mut child = command.spawn().unwrap();
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut transcript = String::new();
    let auth = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            let mut line = String::new();
            assert_ne!(
                stderr.read_line(&mut line).await.unwrap(),
                0,
                "login exited before URL: {transcript}"
            );
            transcript.push_str(&line);
            if line.trim().starts_with("http") {
                break line.trim().to_owned();
            }
        }
    })
    .await
    .unwrap();
    let redirect =
        url::Url::parse(&callback(&auth, "BINARY-CODE-CANARY", Some(&state(&auth)))).unwrap();
    let launch = format!("http://localhost:{}/launch", redirect.port().unwrap());
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let response = client.get(launch).send().await.unwrap();
    assert_eq!(response.status(), 302);
    assert_eq!(response.headers()["location"], auth);
    client.get(redirect).send().await.unwrap();
    let output = tokio::time::timeout(std::time::Duration::from_secs(20), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    stderr.read_to_string(&mut transcript).await.unwrap();
    assert!(output.status.success(), "{transcript}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(
        stdout.trim(),
        "Logged in to Anthropic (Claude Pro/Max) as operator@example.test (Smoke Org)"
    );
    let mut all_output = format!("{transcript}{stdout}");
    let status = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        binary_command(&temp, &server)
            .args(["auth", "status", "--json"])
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(status.status.success());
    let value: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(
        value["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["provider"] == "anthropic")
            .unwrap()["source"],
        "oauth"
    );
    all_output.push_str(&String::from_utf8_lossy(&status.stdout));
    all_output.push_str(&String::from_utf8_lossy(&status.stderr));
    let ask = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        binary_command(&temp, &server)
            .args([
                "--model",
                "claude-opus-4-8",
                "ask",
                "hi",
                "--output-format",
                "json",
            ])
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        ask.status.success(),
        "{}",
        String::from_utf8_lossy(&ask.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&ask.stdout).unwrap();
    assert_eq!(value["response"], "hello smoke");
    all_output.push_str(&String::from_utf8_lossy(&ask.stdout));
    all_output.push_str(&String::from_utf8_lossy(&ask.stderr));
    let requests = server.received_requests().await.unwrap();
    let messages = requests
        .iter()
        .find(|r| r.url.path() == "/v1/messages")
        .unwrap();
    assert_eq!(messages.url.query(), Some("beta=true"));
    assert_eq!(
        messages.headers["authorization"],
        "Bearer LOGIN-ACCESS-CANARY"
    );
    assert_eq!(messages.headers["x-app"], "cli");
    assert!(messages.headers.get("x-api-key").is_none());
    let token = requests
        .iter()
        .find(|r| r.url.path() == "/v1/oauth/token")
        .unwrap();
    let token: serde_json::Value = serde_json::from_slice(&token.body).unwrap();
    for secret in [
        "LOGIN-ACCESS-CANARY",
        "LOGIN-REFRESH-CANARY",
        "BINARY-CODE-CANARY",
        token["code_verifier"].as_str().unwrap(),
    ] {
        assert!(
            !all_output.contains(secret),
            "credential leaked in binary output"
        );
    }
    let logout = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        binary_command(&temp, &server)
            .args(["auth", "logout", "anthropic"])
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(logout.status.success());
    assert_eq!(
        String::from_utf8_lossy(&logout.stdout).trim(),
        "Logged out of anthropic."
    );
    assert!(
        String::from_utf8_lossy(&logout.stderr).contains("Environment variables are unchanged")
    );
    // Idempotent: nothing left to remove, and a still-set provider variable is named.
    let again = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        binary_command(&temp, &server)
            .env("ANTHROPIC_API_KEY", "ENV-SURVIVES")
            .args(["auth", "logout", "anthropic", "--json"])
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(again.status.success());
    let value: serde_json::Value = serde_json::from_slice(&again.stdout).unwrap();
    assert_eq!(value["status"], "no_stored_credentials");
    assert_eq!(
        value["env_still_set"],
        serde_json::json!(["ANTHROPIC_API_KEY"])
    );
    assert!(
        String::from_utf8_lossy(&again.stderr)
            .contains("ANTHROPIC_API_KEY is still set and will still be used")
    );
}

#[tokio::test]
#[serial]
async fn binary_non_tty_method_errors_and_oauth_json_success() {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
    let server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();
    for (args, needle) in [
        (vec!["auth", "login", "anthropic"], "--method"),
        (
            vec!["auth", "login", "anthropic", "--method", "api-key"],
            "interactive terminal",
        ),
    ] {
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            binary_command(&temp, &server).args(args).output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains(needle));
        assert!(result.stdout.is_empty());
    }
    token_mock(&server,3600,serde_json::json!({"account":{"uuid":"json-account","email_address":"json@example.test"},"organization":{"uuid":"json-org","name":"Json Org"}})).await;
    let mut child = binary_command(&temp, &server)
        .args(["auth", "login", "anthropic", "--method", "oauth", "--json"])
        .spawn()
        .unwrap();
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut guidance = String::new();
    let auth = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            let mut line = String::new();
            assert_ne!(stderr.read_line(&mut line).await.unwrap(), 0);
            guidance.push_str(&line);
            if line.trim().starts_with("http") {
                break line.trim().to_owned();
            }
        }
    })
    .await
    .unwrap();
    reqwest::get(callback(&auth, "JSON-CODE", Some(&state(&auth))))
        .await
        .unwrap();
    let output = tokio::time::timeout(std::time::Duration::from_secs(20), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    stderr.read_to_string(&mut guidance).await.unwrap();
    assert!(output.status.success(), "{guidance}");
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        result,
        serde_json::json!({"provider":"anthropic","status":"authenticated","method":"oauth","validated":true,"identity":{"account_id":"json-account","email":"json@example.test","org_id":"json-org","org_name":"Json Org"}})
    );
    for secret in ["LOGIN-ACCESS-CANARY", "LOGIN-REFRESH-CANARY", "JSON-CODE"] {
        assert!(!guidance.contains(secret));
        assert!(!String::from_utf8_lossy(&output.stdout).contains(secret));
    }
}

#[cfg(unix)]
#[tokio::test]
#[serial]
async fn binary_ctrl_c_cancels_and_releases_port() {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
    let server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();
    let mut child = binary_command(&temp, &server)
        .args(["auth", "login", "anthropic", "--method", "oauth"])
        .spawn()
        .unwrap();
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut guidance = String::new();
    let auth = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            let mut line = String::new();
            assert_ne!(stderr.read_line(&mut line).await.unwrap(), 0);
            guidance.push_str(&line);
            if line.trim().starts_with("http") {
                break line.trim().to_owned();
            }
        }
    })
    .await
    .unwrap();
    // SAFETY: this pid is our still-owned child; send only the interrupt signal.
    assert_eq!(
        unsafe { libc::kill(child.id().unwrap() as libc::pid_t, libc::SIGINT) },
        0
    );
    let output = tokio::time::timeout(std::time::Duration::from_secs(20), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    stderr.read_to_string(&mut guidance).await.unwrap();
    assert!(!output.status.success());
    assert!(guidance.contains("Login cancelled"), "{guidance}");
    let redirect = url::Url::parse(&callback(&auth, "x", None)).unwrap();
    assert!(
        tokio::net::TcpListener::bind(("127.0.0.1", redirect.port().unwrap()))
            .await
            .is_ok()
    );
}
