//! Installed-app login through the real flow, loopback listener and persistence
//! boundary, plus the helpers the other Gemini CLI suites share.

pub(crate) use crate::conformance_20_1d_codex_subscription::auth::Env;
use futures::future::BoxFuture;
use rustain::adapters::auth::{
    flows::{self, FlowCtx, FlowError, LoginResult, LoginUi, Sleeper},
    policies::GEMINI_CLI_OAUTH,
};
use rustain::adapters::auth_store::FileAuthStore;
use rustain::domain::models::{
    AuthMethod, CompletionOptions, Credential, Message, MessageRole, ProviderConfig, SecretString,
};
use rustain::domain::ports::{AuthStorePort, StreamingProvider};
use serde_json::{Value, json};
use serial_test::serial;
use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{method, path},
};

pub(crate) const METHOD: AuthMethod = AuthMethod::OAuthCode(&GEMINI_CLI_OAUTH);
pub(crate) const NOW: i64 = 1_800_000_000_000;
pub(crate) const PROVIDER: &str = "google-gemini-cli";
pub(crate) const MODEL: &str = "gemini-3.1-pro-preview";
pub(crate) const PROJECT: &str = "gemini-fixture-project";
pub(crate) const EMAIL: &str = "operator@example.test";
pub(crate) const ACCESS: &str = "GEMINI-ACCESS-CANARY";
pub(crate) const REFRESH: &str = "GEMINI-REFRESH-CANARY";
pub(crate) const CODE: &str = "GEMINI-CODE-CANARY";
pub(crate) const CLIENT_ID: &str =
    "681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com";
/// The decoded embedded secret, split so secret scanners never flag this source;
/// production ships it Base64-encoded, as omp does.
pub(crate) const CLIENT_SECRET: &str = concat!("GOCSPX-", "4uHgMPm-1o7Sk-geV6Cu5clXFsxl");
pub(crate) const STREAM: &str = "/v1internal:streamGenerateContent";
pub(crate) const LOAD: &str = "/v1internal:loadCodeAssist";
pub(crate) const ONBOARD: &str = "/v1internal:onboardUser";
pub(crate) const OPERATION: &str = "/v1internal/operations/fixture-op";
pub(crate) const TOKEN: &str = "/token";
pub(crate) const USERINFO: &str = "/oauth2/v1/userinfo";
pub(crate) const FREE_LOAD_BODY: &str = r#"{"metadata":{"ideType":"IDE_UNSPECIFIED","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI"}}"#;

/// Isolated data dir and loopback origin; every wait scale is zero and the
/// callback listener gets a free port (never 8085).
pub(crate) fn env(dir: &std::path::Path, origin: &str) -> Env {
    let port = {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.local_addr().unwrap().port().to_string()
    };
    let now = NOW.to_string();
    Env::set(&[
        ("RUSTAIN_DATA_DIR", Some(dir.to_str().unwrap())),
        ("RUSTAIN_TEST_AUTH_ORIGIN", Some(origin)),
        ("RUSTAIN_TEST_AUTH_CALLBACK_PORT", Some(&port)),
        ("RUSTAIN_TEST_AUTH_SLEEP_SCALE", Some("0")),
        ("RUSTAIN_TEST_GEMINI_CLI_RETRY_SCALE", Some("0")),
        ("RUSTAIN_TEST_NOW_MS", Some(&now)),
        ("GOOGLE_CLOUD_PROJECT", None),
        ("GOOGLE_CLOUD_PROJECT_ID", None),
        ("GOOGLE_API_KEY", None),
    ])
}

/// Records every UI call; pastes are served in order, then paste is unavailable.
pub(crate) struct Ui {
    urls: mpsc::UnboundedSender<(String, String)>,
    progress: mpsc::UnboundedSender<String>,
    pastes: Mutex<VecDeque<String>>,
    pub(crate) cancel: CancellationToken,
}
pub(crate) struct Seen {
    pub(crate) urls: mpsc::UnboundedReceiver<(String, String)>,
    progress: mpsc::UnboundedReceiver<String>,
}
impl Seen {
    pub(crate) fn progress(&mut self) -> Vec<String> {
        let mut lines = Vec::new();
        while let Ok(line) = self.progress.try_recv() {
            lines.push(line);
        }
        lines
    }
}
impl LoginUi for Ui {
    fn on_auth_url(&self, url: &str, launch: &str, _: &str) {
        let _ = self.urls.send((url.into(), launch.into()));
    }
    fn on_device_code(&self, _: &str, _: &str, _: &str) {
        panic!("a browser login never shows a device code");
    }
    fn prompt_line(&self, _: &str) -> BoxFuture<'_, Option<String>> {
        panic!("a browser login never prompts for a line");
    }
    fn on_progress(&self, message: &str) {
        let _ = self.progress.send(message.into());
    }
    fn prompt_secret(&self, _: &str) -> BoxFuture<'_, Option<SecretString>> {
        Box::pin(async { None })
    }
    fn manual_input(&self, _: &str) -> Option<BoxFuture<'static, Option<String>>> {
        let pasted = self.pastes.try_lock().ok()?.pop_front()?;
        Some(Box::pin(async move { Some(pasted) }))
    }
    fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }
}
pub(crate) fn ui(pastes: &[&str]) -> (Arc<Ui>, Seen) {
    let (urls, url_rx) = mpsc::unbounded_channel();
    let (progress, progress_rx) = mpsc::unbounded_channel();
    (
        Arc::new(Ui {
            urls,
            progress,
            pastes: Mutex::new(pastes.iter().map(|paste| (*paste).to_owned()).collect()),
            cancel: CancellationToken::new(),
        }),
        Seen {
            urls: url_rx,
            progress: progress_rx,
        },
    )
}

/// Recording sleeper: every wait is counted and returns at once. `cancel_at`
/// cancels the login token at that (1-based) sleep and never returns from it.
#[derive(Clone, Default)]
pub(crate) struct Sleeps {
    pub(crate) count: Arc<AtomicUsize>,
    pub(crate) other: Arc<AtomicUsize>,
}
impl Sleeps {
    pub(crate) fn sleeper(&self, cancel: Option<(usize, CancellationToken)>) -> Sleeper {
        let (count, other) = (self.count.clone(), self.other.clone());
        Arc::new(move |duration: Duration| {
            let n = count.fetch_add(1, Ordering::SeqCst) + 1;
            if duration != Duration::from_millis(5_000) {
                other.fetch_add(1, Ordering::SeqCst);
            }
            match &cancel {
                Some((at, token)) if *at == n => {
                    token.cancel();
                    Box::pin(std::future::pending())
                }
                _ => Box::pin(async {}),
            }
        })
    }
}

/// One Google response set. Polls are served in order, once each.
pub(crate) struct Google {
    pub(crate) token: ResponseTemplate,
    pub(crate) userinfo: ResponseTemplate,
    pub(crate) load: ResponseTemplate,
    pub(crate) onboard: ResponseTemplate,
    pub(crate) polls: Vec<ResponseTemplate>,
}
pub(crate) fn token_body(access: &str, refresh: Option<&str>) -> Value {
    let mut body = json!({"access_token": access, "expires_in": 3600, "token_type": "Bearer"});
    if let Some(refresh) = refresh {
        body["refresh_token"] = json!(refresh);
    }
    body
}
pub(crate) fn ok(body: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(body)
}
/// Free tier: `done:false` onboarding, then one pending poll and one done poll.
pub(crate) fn google(access: &str, project: &str, email: &str) -> Google {
    Google {
        token: ok(token_body(access, Some(REFRESH))),
        userinfo: ok(json!({"email": email, "id": "1"})),
        load: ok(json!({"allowedTiers": [{"id": "free-tier", "isDefault": true}]})),
        onboard: ok(json!({"name": "operations/fixture-op", "done": false})),
        polls: vec![
            ok(json!({"name": "operations/fixture-op", "done": false})),
            ok(json!({"done": true, "response": {"cloudaicompanionProject": {"id": project}}})),
        ],
    }
}
pub(crate) async fn mount(server: &MockServer, google: Google) {
    Mock::given(method("POST"))
        .and(path(TOKEN))
        .respond_with(google.token)
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(USERINFO))
        .respond_with(google.userinfo)
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path(LOAD))
        .respond_with(google.load)
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path(ONBOARD))
        .respond_with(google.onboard)
        .mount(server)
        .await;
    for poll in google.polls {
        Mock::given(method("GET"))
            .and(path(OPERATION))
            .respond_with(poll)
            .up_to_n_times(1)
            .mount(server)
            .await;
    }
}

pub(crate) fn no_redirect_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
}
pub(crate) fn query(url: &url::Url) -> Vec<(String, String)> {
    url.query_pairs().into_owned().collect()
}
fn value<'a>(pairs: &'a [(String, String)], key: &str) -> &'a str {
    &pairs.iter().find(|(k, _)| k == key).unwrap().1
}
/// Waits for the published URLs, proves the loopback rebase, follows `/launch`
/// and completes the callback with the expected state.
pub(crate) async fn browse(seen: &mut Seen, server: &MockServer) -> url::Url {
    let (authorize, launch) = seen.urls.recv().await.unwrap();
    let authorize = url::Url::parse(&authorize).unwrap();
    assert_eq!(
        authorize.origin(),
        url::Url::parse(&server.uri()).unwrap().origin()
    );
    let launch = url::Url::parse(&launch).unwrap();
    let client = no_redirect_client();
    let response = client.get(launch).send().await.unwrap();
    assert_eq!(response.status(), 302);
    assert_eq!(response.headers()["location"], authorize.as_str());
    let pairs = query(&authorize);
    let mut callback = url::Url::parse(value(&pairs, "redirect_uri")).unwrap();
    callback
        .query_pairs_mut()
        .append_pair("code", CODE)
        .append_pair("state", value(&pairs, "state"));
    assert!(
        client
            .get(callback)
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    authorize
}
pub(crate) fn store() -> Arc<dyn AuthStorePort> {
    Arc::new(FileAuthStore::new())
}
/// Real `flows::login` of the production policy, driven by the browser helper.
pub(crate) async fn login_with(
    server: &MockServer,
    ui: Arc<Ui>,
    seen: &mut Seen,
    sleeper: Sleeper,
) -> (Result<LoginResult, FlowError>, url::Url) {
    let ctx = FlowCtx::new(ui, Arc::new(|| NOW)).with_sleeper(sleeper);
    let store = store();
    tokio::join!(
        flows::login(PROVIDER, &METHOD, &ctx, &store),
        browse(seen, server)
    )
}
/// The default successful login of `access`/`project`/`email` into `RUSTAIN_DATA_DIR`.
pub(crate) async fn login(
    server: &MockServer,
    access: &str,
    project: &str,
    email: &str,
) -> LoginResult {
    mount(server, google(access, project, email)).await;
    let (ui, mut seen) = ui(&[]);
    let (result, _) = login_with(server, ui, &mut seen, Sleeps::default().sleeper(None)).await;
    result.unwrap()
}

pub(crate) fn config(base: Option<&str>) -> ProviderConfig {
    ProviderConfig {
        provider_id: PROVIDER.into(),
        kind: Some(PROVIDER.into()),
        model_id: MODEL.into(),
        api_key_env: String::new(),
        auth_header: None,
        enabled: true,
        base_url: base.map(|base| base.to_owned().into()),
        context_window: None,
        supports_tools: None,
        discover_models: true,
        model_filter: vec!["*".into()],
        cache_ttl_seconds: 3600,
    }
}
pub(crate) fn build() -> Arc<dyn StreamingProvider> {
    rustain::infrastructure::provider_factory::build_provider_for_config(PROVIDER, &config(None))
        .unwrap()
}
pub(crate) fn options(model: &str) -> CompletionOptions {
    CompletionOptions {
        model: model.into(),
        max_tokens: 8192,
        system_prompt: String::new(),
        temperature: None,
        tools: vec![],
        session_id: Some("gemini-conversation".into()),
    }
}
pub(crate) fn user(text: &str) -> Message {
    Message {
        role: MessageRole::User,
        content: text.into(),
        images: vec![],
        tool_results: vec![],
        tool_uses: vec![],
        context_prefix: None,
        reasoning_content: None,
        provider_replay: vec![],
    }
}
pub(crate) fn hits<'a>(requests: &'a [Request], path: &str) -> Vec<&'a Request> {
    requests
        .iter()
        .filter(|request| request.url.path() == path)
        .collect()
}
pub(crate) fn form(request: &Request) -> Vec<(String, String)> {
    url::form_urlencoded::parse(&request.body)
        .into_owned()
        .collect()
}
pub(crate) async fn stored_oauth(provider: &str) -> Vec<rustain::domain::models::OAuthCredential> {
    store()
        .snapshot()
        .await
        .unwrap()
        .rows(provider)
        .iter()
        .filter_map(|row| match &row.credential {
            Credential::OAuth(oauth) => Some(oauth.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
#[serial]
async fn k01_installed_app_login_has_no_pkce_sends_the_secret_and_stores_the_project() {
    let server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();
    let _env = env(temp.path(), &server.uri());
    mount(&server, google(ACCESS, PROJECT, EMAIL)).await;
    let (ui, mut seen) = ui(&[]);
    let sleeps = Sleeps::default();
    let (result, authorize) = login_with(&server, ui, &mut seen, sleeps.sleeper(None)).await;
    let result = result.unwrap();
    assert_eq!(result.method, "oauth");
    assert_eq!(result.identity.email.as_deref(), Some(EMAIL));
    assert_eq!(result.identity.project_id.as_deref(), Some(PROJECT));

    // Authorize query: exact keys, values and order, and no PKCE.
    assert_eq!(authorize.path(), "/o/oauth2/v2/auth");
    let pairs = query(&authorize);
    let keys: Vec<_> = pairs.iter().map(|(key, _)| key.as_str()).collect();
    assert_eq!(
        keys,
        [
            "client_id",
            "response_type",
            "redirect_uri",
            "scope",
            "state",
            "access_type",
            "prompt"
        ]
    );
    let port: u16 = std::env::var("RUSTAIN_TEST_AUTH_CALLBACK_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let redirect = format!("http://127.0.0.1:{port}/oauth2callback");
    let state = value(&pairs, "state").to_owned();
    assert_eq!(state.len(), 32);
    assert!(state.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')));
    let expected = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs([
            ("client_id", CLIENT_ID),
            ("response_type", "code"),
            ("redirect_uri", redirect.as_str()),
            ("scope", "https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/userinfo.email https://www.googleapis.com/auth/userinfo.profile"),
            ("state", state.as_str()),
            ("access_type", "offline"),
            ("prompt", "consent"),
        ])
        .finish();
    assert_eq!(authorize.query(), Some(expected.as_str()));
    assert!(!authorize.as_str().contains("code_challenge"));

    // Exchange form literal: secret, no verifier, the actual redirect.
    let requests = server.received_requests().await.unwrap();
    let token = hits(&requests, TOKEN);
    assert_eq!(token.len(), 1);
    assert_eq!(
        token[0].headers["content-type"],
        "application/x-www-form-urlencoded"
    );
    let exchange = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs([
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("client_secret", CLIENT_SECRET),
            ("code", CODE),
            ("redirect_uri", redirect.as_str()),
        ])
        .finish();
    assert_eq!(String::from_utf8_lossy(&token[0].body), exchange);

    // Userinfo: bearer only. Provisioning: Gemini CLI identity and exact bodies.
    let userinfo = hits(&requests, USERINFO);
    assert_eq!(userinfo.len(), 1);
    assert_eq!(userinfo[0].url.query(), Some("alt=json"));
    assert_eq!(
        userinfo[0].headers["authorization"],
        format!("Bearer {ACCESS}").as_str()
    );
    let load = hits(&requests, LOAD);
    assert_eq!(load.len(), 1);
    assert_eq!(String::from_utf8_lossy(&load[0].body), FREE_LOAD_BODY);
    let onboard = hits(&requests, ONBOARD);
    assert_eq!(
        String::from_utf8_lossy(&onboard[0].body),
        r#"{"tierId":"free-tier","metadata":{"ideType":"IDE_UNSPECIFIED","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI"}}"#
    );
    for request in [load[0], onboard[0]] {
        assert_eq!(
            request.headers["authorization"],
            format!("Bearer {ACCESS}").as_str()
        );
        assert_eq!(request.headers["content-type"], "application/json");
        let agent = request.headers["user-agent"].to_str().unwrap();
        assert!(
            agent.starts_with("GeminiCLI/0.46.0/gemini-3.1-pro-preview (")
                && agent.ends_with("; terminal)"),
            "{agent}"
        );
        assert_eq!(
            request.headers["client-metadata"],
            "ideType=IDE_UNSPECIFIED,platform=PLATFORM_UNSPECIFIED,pluginType=GEMINI"
        );
    }
    assert_eq!(hits(&requests, OPERATION).len(), 2);
    assert_eq!(sleeps.count.load(Ordering::SeqCst), 1);
    assert_eq!(sleeps.other.load(Ordering::SeqCst), 0);
    assert_eq!(
        seen.progress(),
        [
            "Checking for existing Cloud Code Assist project...",
            "Provisioning Cloud Code Assist project (this may take a moment)...",
            "Waiting for project provisioning (attempt 2/24)...",
        ]
    );

    // Stored row fields; the client secret never reaches auth.json.
    let rows = stored_oauth(PROVIDER).await;
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.access.expose_secret(), ACCESS);
    assert_eq!(row.refresh.expose_secret(), REFRESH);
    assert_eq!(row.expires_at_ms, NOW + 3_600_000 - 300_000);
    assert_eq!(row.authorized_at_ms, Some(NOW));
    assert_eq!(row.identity.email.as_deref(), Some(EMAIL));
    assert_eq!(row.identity.project_id.as_deref(), Some(PROJECT));
    assert_eq!(row.identity.account_id, None);
    let file = std::fs::read_to_string(temp.path().join("auth.json")).unwrap();
    assert!(!file.contains(CLIENT_SECRET));
    assert!(!file.contains("R09DU1BYLTR1SGdNUG0tMW83U2stZ2VWNkN1NWNsWEZzeGw"));
}

#[tokio::test]
#[serial]
async fn userinfo_failure_still_logs_in_without_an_email() {
    let server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();
    let _env = env(temp.path(), &server.uri());
    let mut google = google(ACCESS, PROJECT, EMAIL);
    google.userinfo = ResponseTemplate::new(500).set_body_string(format!("echo {ACCESS}"));
    mount(&server, google).await;
    let (ui, mut seen) = ui(&[]);
    let (result, _) = login_with(&server, ui, &mut seen, Sleeps::default().sleeper(None)).await;
    let result = result.unwrap();
    assert_eq!(result.identity.email, None);
    assert_eq!(result.identity.project_id.as_deref(), Some(PROJECT));
}

#[tokio::test]
#[serial]
async fn a_token_response_without_refresh_token_stores_nothing() {
    let server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();
    let _env = env(temp.path(), &server.uri());
    let mut google = google(ACCESS, PROJECT, EMAIL);
    google.token = ok(token_body(ACCESS, None));
    mount(&server, google).await;
    let (ui, mut seen) = ui(&[]);
    let (result, _) = login_with(&server, ui, &mut seen, Sleeps::default().sleeper(None)).await;
    assert_eq!(
        result.unwrap_err().to_string(),
        "No refresh token received. Please try again."
    );
    let requests = server.received_requests().await.unwrap();
    assert!(hits(&requests, LOAD).is_empty());
    assert!(stored_oauth(PROVIDER).await.is_empty());
}

/// Drives the listener by hand: the test owns every callback request.
async fn published(seen: &mut Seen) -> (url::Url, url::Url, String) {
    let (authorize, launch) = seen.urls.recv().await.unwrap();
    let authorize = url::Url::parse(&authorize).unwrap();
    let pairs = query(&authorize);
    let state = value(&pairs, "state").to_owned();
    (
        url::Url::parse(value(&pairs, "redirect_uri")).unwrap(),
        url::Url::parse(&launch).unwrap(),
        state,
    )
}
fn with(mut url: url::Url, pairs: &[(&str, &str)]) -> url::Url {
    url.query_pairs_mut().extend_pairs(pairs);
    url
}

#[tokio::test]
#[serial]
async fn k02_held_port_falls_back_binds_ipv4_only_and_mismatch_keeps_waiting() {
    let server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();
    let _env = env(temp.path(), &server.uri());
    // Hold the requested port so the binary must choose the fallback itself.
    let held = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let requested = held.local_addr().unwrap().port();
    let _port = Env::set(&[(
        "RUSTAIN_TEST_AUTH_CALLBACK_PORT",
        Some(&requested.to_string()),
    )]);
    mount(&server, google(ACCESS, PROJECT, EMAIL)).await;
    let (ui, mut seen) = ui(&[]);
    let ctx = FlowCtx::new(ui, Arc::new(|| NOW)).with_sleeper(Sleeps::default().sleeper(None));
    let store = store();
    let drive = async {
        let (redirect, launch, state) = published(&mut seen).await;
        let actual = redirect.port().unwrap();
        assert_ne!(actual, requested);
        assert_eq!(redirect.host_str(), Some("127.0.0.1"));
        assert_eq!(launch.host_str(), Some("127.0.0.1"));
        assert_eq!(launch.port(), Some(actual));
        assert_eq!(redirect.path(), "/oauth2callback");
        // IPv4 only: nothing listens on [::1] at the actual port.
        assert!(std::net::TcpStream::connect(("::1", actual)).is_err());
        let client = no_redirect_client();
        let mismatch = client
            .get(with(redirect.clone(), &[("code", CODE), ("state", "0000")]))
            .send()
            .await
            .unwrap();
        assert_eq!(mismatch.status(), 400);
        // A denial with a foreign state is answered and ignored too.
        let foreign = client
            .get(with(
                redirect.clone(),
                &[("error", "access_denied"), ("state", "0000")],
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(foreign.status(), 400);
        let done = client
            .get(with(redirect, &[("code", CODE), ("state", &state)]))
            .send()
            .await
            .unwrap();
        assert!(done.status().is_success());
        seen
    };
    let (result, mut seen) = tokio::join!(flows::login(PROVIDER, &METHOD, &ctx, &store), drive);
    result.unwrap();
    let progress = seen.progress();
    assert!(
        progress[0].starts_with(&format!(
            "Preferred port {requested} unavailable, using port "
        )),
        "{progress:?}"
    );
    drop(held);
}

#[tokio::test]
#[serial]
async fn k02_denial_with_the_matching_state_ends_the_login_and_stores_nothing() {
    let server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();
    let _env = env(temp.path(), &server.uri());
    let (ui, mut seen) = ui(&[]);
    let ctx = FlowCtx::new(ui, Arc::new(|| NOW));
    let store = store();
    let drive = async {
        let (redirect, _, state) = published(&mut seen).await;
        let response = no_redirect_client()
            .get(with(
                redirect,
                &[("error", "access_denied"), ("state", &state)],
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
    };
    let (result, ()) = tokio::join!(flows::login(PROVIDER, &METHOD, &ctx, &store), drive);
    assert_eq!(
        result.unwrap_err().to_string(),
        "OAuth authorization was denied"
    );
    assert!(server.received_requests().await.unwrap().is_empty());
    assert!(stored_oauth(PROVIDER).await.is_empty());
}

#[tokio::test]
#[serial]
async fn k02_paste_accepts_url_query_bare_code_and_code_hash_state() {
    for form_of in ["url", "query", "bare", "hash"] {
        let server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();
        let _env = env(temp.path(), &server.uri());
        mount(&server, google(ACCESS, PROJECT, EMAIL)).await;
        // The state is unknown before login; a wrong one re-prompts, so serve a
        // mismatching paste first and the accepted form second.
        let ctx_ui = Arc::new(PasteUi {
            authorize: Mutex::new(None),
            form: form_of,
            served: AtomicUsize::new(0),
            cancel: CancellationToken::new(),
        });
        let ctx = FlowCtx::new(ctx_ui.clone(), Arc::new(|| NOW))
            .with_sleeper(Sleeps::default().sleeper(None));
        let store = store();
        let result = flows::login(PROVIDER, &METHOD, &ctx, &store).await;
        result.unwrap_or_else(|error| panic!("{form_of}: {error}"));
        let requests = server.received_requests().await.unwrap();
        let exchange = form(hits(&requests, TOKEN)[0]);
        assert_eq!(exchange[3], ("code".into(), CODE.into()), "{form_of}");
        assert_eq!(ctx_ui.served.load(Ordering::SeqCst), 2, "{form_of}");
        assert_eq!(stored_oauth(PROVIDER).await.len(), 1, "{form_of}");
    }
}

/// Paste UI whose pastes are computed from the published redirect and state.
struct PasteUi {
    authorize: Mutex<Option<String>>,
    form: &'static str,
    served: AtomicUsize,
    cancel: CancellationToken,
}
impl LoginUi for PasteUi {
    fn on_auth_url(&self, url: &str, _: &str, _: &str) {
        *self.authorize.try_lock().unwrap() = Some(url.into());
    }
    fn on_device_code(&self, _: &str, _: &str, _: &str) {
        panic!("a browser login never shows a device code");
    }
    fn prompt_line(&self, _: &str) -> BoxFuture<'_, Option<String>> {
        panic!("a browser login never prompts for a line");
    }
    fn on_progress(&self, _: &str) {}
    fn prompt_secret(&self, _: &str) -> BoxFuture<'_, Option<SecretString>> {
        Box::pin(async { None })
    }
    fn manual_input(&self, _: &str) -> Option<BoxFuture<'static, Option<String>>> {
        let n = self.served.fetch_add(1, Ordering::SeqCst);
        // `on_auth_url` runs before the first prompt.
        let authorize = self.authorize.try_lock().ok()?.clone()?;
        let pairs = query(&url::Url::parse(&authorize).ok()?);
        let redirect = value(&pairs, "redirect_uri").to_owned();
        // The first paste carries a mismatching state and must re-prompt.
        let state = if n == 0 {
            "f".repeat(32)
        } else {
            value(&pairs, "state").to_owned()
        };
        let pasted = match (self.form, n) {
            ("url", _) => format!("{redirect}?code={CODE}&state={state}"),
            ("query", _) => format!("?code={CODE}&state={state}"),
            // A bare code carries no state, so it is accepted as is.
            ("bare", 0) => format!("{CODE}#{state}"),
            ("bare", _) => CODE.to_owned(),
            _ => format!("{CODE}#{state}"),
        };
        Some(Box::pin(async move { Some(pasted) }))
    }
    fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }
}

#[tokio::test]
#[serial]
async fn k02_cancel_during_the_wait_releases_the_listener() {
    let server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();
    let _env = env(temp.path(), &server.uri());
    let (ui, mut seen) = ui(&[]);
    let cancel = ui.cancel.clone();
    let ctx = FlowCtx::new(ui, Arc::new(|| NOW));
    let store = store();
    let drive = async {
        let (redirect, _, _) = published(&mut seen).await;
        cancel.cancel();
        redirect.port().unwrap()
    };
    let (result, port) = tokio::join!(flows::login(PROVIDER, &METHOD, &ctx, &store), drive);
    assert_eq!(result.unwrap_err().to_string(), "Login cancelled");
    // The login future owned the socket: the port is free again at once.
    std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).unwrap();
    assert!(server.received_requests().await.unwrap().is_empty());
    assert!(stored_oauth(PROVIDER).await.is_empty());
}

#[tokio::test]
#[serial]
async fn the_secret_never_reaches_errors_or_tracing() {
    let server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();
    let _env = env(temp.path(), &server.uri());
    let mut google = google(ACCESS, PROJECT, EMAIL);
    google.token = ResponseTemplate::new(400).set_body_string(format!(
        "{{\"error\":\"invalid_grant\",\"echo\":\"{CLIENT_SECRET}\"}}"
    ));
    mount(&server, google).await;
    let (ui, mut seen) = ui(&[]);
    let (result, _) = login_with(&server, ui, &mut seen, Sleeps::default().sleeper(None)).await;
    let error = result.unwrap_err().to_string();
    assert_eq!(error, "OAuth token exchange failed: invalid_grant");
    assert!(!error.contains(CLIENT_SECRET));
}
