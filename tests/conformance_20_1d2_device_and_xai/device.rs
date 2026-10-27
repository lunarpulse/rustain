//! xAI device login through `flows::login` with the production `XAI_DEVICE` policy:
//! happy path (K01), failure semantics (K02), token-endpoint trust (K03) and the
//! device-response, interval, OIDC and access-JWT `sub` plain tests. Also hosts the
//! recording harness (UI, sleeper, fake clock, stub responders) and the real
//! `xai-oauth` login the other submodules use to create rows.

use crate::conformance_20_1d_codex_subscription::auth::jwt;
use crate::conformance_20_1d1b_codex_replay_discovery::common::{GUARD, NOW, World, accept};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use futures::{FutureExt, future::BoxFuture};
use parking_lot::Mutex;
use rustain::adapters::auth::flows::{self, FlowCtx, FlowError, LoginResult, LoginUi, Sleeper};
use rustain::adapters::auth::policies::XAI_DEVICE;
use rustain::adapters::auth_store::FileAuthStore;
use rustain::domain::models::{
    AuthMethod, Credential, CredentialOrigin, OAuthCredential, SecretString, StoredCredential,
};
use rustain::domain::ports::AuthStorePort;
use serde_json::{Value, json};
use serial_test::serial;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;
use wiremock::{
    Mock, MockGuard, MockServer, Request, Respond, ResponseTemplate,
    matchers::{method, path},
};

pub(crate) const PROVIDER: &str = "xai-oauth";
const XAI: AuthMethod = AuthMethod::Device(&XAI_DEVICE);
pub(crate) const DEVICE_PATH: &str = "/oauth2/device/code";
pub(crate) const OIDC_PATH: &str = "/.well-known/openid-configuration";
pub(crate) const TOKEN_PATH: &str = "/oauth2/token";
pub(crate) const USERINFO_PATH: &str = "/oauth2/userinfo";
/// What discovery normally returns; rebased to the stub's `TOKEN_PATH` after validation.
pub(crate) const TOKEN_ENDPOINT: &str = "https://auth.x.ai/oauth2/token";
const CLIENT: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const USER_CODE: &str = "WDJB-MJHT";
const DEVICE_CODE: &str = "DEVICE-CODE-1";
const VERIFY: &str = "https://accounts.x.ai/device";
const VERIFY_COMPLETE: &str = "https://accounts.x.ai/device?user_code=WDJB-MJHT";
const DEVICE_FORM: &str = "client_id=b1a00492-073a-47ea-816f-4c329264a828&scope=openid+profile+email+offline_access+grok-cli%3Aaccess+api%3Aaccess";

// ── recording harness ───────────────────────────────────────────────────────

/// One observable step, in causal order: UI calls, waits and stub-received requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Event {
    Display {
        url: String,
        code: String,
        instructions: String,
    },
    Progress(String),
    Sleep(u64),
    Request {
        method: String,
        path: String,
        at: i64,
    },
}

pub(crate) fn post(path: &str, at: i64) -> Event {
    Event::Request {
        method: "POST".into(),
        path: path.into(),
        at,
    }
}
pub(crate) fn get(path: &str, at: i64) -> Event {
    Event::Request {
        method: "GET".into(),
        path: path.into(),
        at,
    }
}
pub(crate) fn progress(message: &str) -> Event {
    Event::Progress(message.into())
}
pub(crate) fn display(url: &str, code: &str) -> Event {
    Event::Display {
        url: url.into(),
        code: code.into(),
        instructions: format!("Enter code: {code}"),
    }
}

/// Shared fake clock (`NOW` + every recorded sleep), recording sleeper and timeline.
#[derive(Clone)]
pub(crate) struct Recorder {
    elapsed: Arc<AtomicI64>,
    timeline: Arc<Mutex<Vec<Event>>>,
    pub(crate) cancel: CancellationToken,
    cancel_at_sleep: Option<usize>,
}

impl Recorder {
    pub(crate) fn new() -> Self {
        Self {
            elapsed: Arc::new(AtomicI64::new(0)),
            timeline: Arc::new(Mutex::new(Vec::new())),
            cancel: CancellationToken::new(),
            cancel_at_sleep: None,
        }
    }
    /// The operator cancels while the flow waits in sleep number `index` (0-based);
    /// that sleep never finishes and the clock does not advance.
    pub(crate) fn cancelling_at_sleep(index: usize) -> Self {
        Self {
            cancel_at_sleep: Some(index),
            ..Self::new()
        }
    }
    pub(crate) fn now(&self) -> i64 {
        NOW + self.elapsed.load(Ordering::SeqCst)
    }
    fn push(&self, event: Event) {
        self.timeline.lock().push(event);
    }
    pub(crate) fn events(&self) -> Vec<Event> {
        self.timeline.lock().clone()
    }
    pub(crate) fn sleeps(&self) -> Vec<u64> {
        self.events()
            .into_iter()
            .filter_map(|event| match event {
                Event::Sleep(ms) => Some(ms),
                _ => None,
            })
            .collect()
    }
    /// Request paths in arrival order.
    pub(crate) fn paths(&self) -> Vec<String> {
        self.events()
            .into_iter()
            .filter_map(|event| match event {
                Event::Request { path, .. } => Some(path),
                _ => None,
            })
            .collect()
    }
    /// Fake-clock time of every request to `route`.
    pub(crate) fn times(&self, route: &str) -> Vec<i64> {
        self.events()
            .into_iter()
            .filter_map(|event| match event {
                Event::Request { path, at, .. } if path == route => Some(at),
                _ => None,
            })
            .collect()
    }
    fn sleeper(&self) -> Sleeper {
        let recorder = self.clone();
        Arc::new(move |duration: Duration| -> BoxFuture<'static, ()> {
            let ms = u64::try_from(duration.as_millis()).expect("finite recorded wait");
            let index = recorder.sleeps().len();
            recorder.push(Event::Sleep(ms));
            if recorder.cancel_at_sleep == Some(index) {
                recorder.cancel.cancel();
                return Box::pin(futures::future::pending());
            }
            recorder
                .elapsed
                .fetch_add(i64::try_from(ms).unwrap(), Ordering::SeqCst);
            Box::pin(async {})
        })
    }
    /// The production flow context with this recorder's UI, clock and sleeper.
    pub(crate) fn ctx(&self) -> FlowCtx {
        let clock = self.clone();
        FlowCtx::new(
            Arc::new(RecordingUi(self.clone())),
            Arc::new(move || clock.now()),
        )
        .with_sleeper(self.sleeper())
    }
    fn mock(&self, verb: &str, route: &str, replies: Vec<ResponseTemplate>) -> Mock {
        assert!(!replies.is_empty());
        Mock::given(method(verb))
            .and(path(route))
            .respond_with(Replies {
                recorder: self.clone(),
                replies,
                next: AtomicUsize::new(0),
            })
    }
    /// Answers `verb route` with `replies` in order (the last repeats), recording each
    /// request with the fake-clock time at which it arrived.
    pub(crate) async fn mount(
        &self,
        server: &MockServer,
        verb: &str,
        route: &str,
        replies: Vec<ResponseTemplate>,
    ) {
        self.mock(verb, route, replies).mount(server).await;
    }
    pub(crate) async fn mount_scoped(
        &self,
        server: &MockServer,
        verb: &str,
        route: &str,
        replies: Vec<ResponseTemplate>,
    ) -> MockGuard {
        self.mock(verb, route, replies)
            .mount_as_scoped(server)
            .await
    }
}

struct Replies {
    recorder: Recorder,
    replies: Vec<ResponseTemplate>,
    next: AtomicUsize,
}
impl Respond for Replies {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        self.recorder.push(Event::Request {
            method: request.method.to_string(),
            path: request.url.path().to_owned(),
            at: self.recorder.now(),
        });
        let index = self.next.fetch_add(1, Ordering::SeqCst);
        self.replies[index.min(self.replies.len() - 1)].clone()
    }
}

struct RecordingUi(Recorder);
impl LoginUi for RecordingUi {
    fn on_auth_url(&self, _: &str, _: &str, _: &str) {
        panic!("a device login never publishes a browser authorization URL");
    }
    fn on_device_code(&self, verification_url: &str, user_code: &str, instructions: &str) {
        self.0.push(Event::Display {
            url: verification_url.into(),
            code: user_code.into(),
            instructions: instructions.into(),
        });
    }
    fn on_progress(&self, message: &str) {
        self.0.push(Event::Progress(message.into()));
    }
    fn prompt_secret(&self, _: &str) -> BoxFuture<'_, Option<SecretString>> {
        panic!("a device login never prompts for a secret");
    }
    fn manual_input(&self, _: &str) -> Option<BoxFuture<'static, Option<String>>> {
        None
    }
    fn cancel_token(&self) -> CancellationToken {
        self.0.cancel.clone()
    }
}

/// `flows::login` into the default store (`RUSTAIN_DATA_DIR`, set by `World`).
pub(crate) async fn run_login(
    recorder: &Recorder,
    provider: &str,
    method: &AuthMethod,
) -> Result<LoginResult, FlowError> {
    let store: Arc<dyn AuthStorePort> = Arc::new(FileAuthStore::new());
    flows::login(provider, method, &recorder.ctx(), &store).await
}
async fn login(recorder: &Recorder) -> Result<LoginResult, FlowError> {
    run_login(recorder, PROVIDER, &XAI).await
}

pub(crate) fn failure(result: Result<LoginResult, FlowError>) -> String {
    result.expect_err("the login must fail").to_string()
}

pub(crate) async fn rows(provider: &str) -> Vec<StoredCredential> {
    FileAuthStore::new()
        .snapshot()
        .await
        .unwrap()
        .rows(provider)
        .to_vec()
}
pub(crate) fn oauth(row: &StoredCredential) -> &OAuthCredential {
    match &row.credential {
        Credential::OAuth(oauth) => oauth,
        _ => panic!("a device login stores an OAuth row"),
    }
}
/// A failed login writes nothing: the store file is never created.
pub(crate) fn assert_nothing_stored(world: &World) {
    assert!(
        !world.dir.path().join("auth.json").exists(),
        "a failed or cancelled login must write nothing"
    );
}

pub(crate) async fn received(server: &MockServer, route: &str) -> Vec<Request> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path() == route)
        .collect()
}
pub(crate) fn body(request: &Request) -> String {
    String::from_utf8(request.body.clone()).unwrap()
}
pub(crate) fn json_reply(status: u16, body: Value) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(body)
}
/// A loopback origin nothing listens on: every request fails at connect.
pub(crate) fn unreachable_origin() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    format!("http://{}", listener.local_addr().unwrap())
}

// ── xAI fixtures ────────────────────────────────────────────────────────────

fn device_body(extra: Value) -> Value {
    let mut body = json!({
        "device_code": DEVICE_CODE,
        "user_code": USER_CODE,
        "verification_uri": VERIFY,
        "verification_uri_complete": VERIFY_COMPLETE,
    });
    let fields = body.as_object_mut().unwrap();
    for (key, value) in extra.as_object().unwrap() {
        if value.is_null() {
            fields.remove(key);
        } else {
            fields.insert(key.clone(), value.clone());
        }
    }
    body
}
fn device(extra: Value) -> ResponseTemplate {
    json_reply(200, device_body(extra))
}
fn discovery(endpoint: &str) -> ResponseTemplate {
    json_reply(
        200,
        json!({"issuer": "https://auth.x.ai", "token_endpoint": endpoint}),
    )
}
fn poll_error(error: &str) -> ResponseTemplate {
    json_reply(400, json!({"error": error}))
}
fn pending() -> ResponseTemplate {
    poll_error("authorization_pending")
}
fn slow_down() -> ResponseTemplate {
    poll_error("slow_down")
}
/// An xAI access JWT whose `sub` is the projected account id.
pub(crate) fn xai_access(sub: &str) -> String {
    jwt(json!({"sub": sub, "iss": "https://auth.x.ai", "aud": CLIENT}))
}
fn tokens(access: &str, refresh: &str) -> ResponseTemplate {
    json_reply(
        200,
        json!({"access_token": access, "refresh_token": refresh, "expires_in": 3600, "token_type": "Bearer"}),
    )
}
fn userinfo() -> ResponseTemplate {
    json_reply(
        200,
        json!({"sub": "userinfo-sub", "email": "grok@example.test"}),
    )
}
fn poll_form() -> String {
    format!(
        "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code&client_id={CLIENT}&device_code={DEVICE_CODE}"
    )
}

/// Mounts the four xAI auth endpoints; polls are answered at `poll_path`.
async fn mount_xai(
    server: &MockServer,
    recorder: &Recorder,
    device: ResponseTemplate,
    oidc: ResponseTemplate,
    poll_path: &str,
    polls: Vec<ResponseTemplate>,
) {
    recorder
        .mount(server, "POST", DEVICE_PATH, vec![device])
        .await;
    recorder.mount(server, "GET", OIDC_PATH, vec![oidc]).await;
    recorder.mount(server, "POST", poll_path, polls).await;
    recorder
        .mount(server, "GET", USERINFO_PATH, vec![userinfo()])
        .await;
}

/// A real `xai-oauth` device login against `server` (the only way rows are written):
/// the first poll succeeds and userinfo answers `userinfo`. The login mocks are
/// unmounted on return, so callers mount their own refresh, discovery or inference
/// answers afterwards. Callers hold `World` with the origin set to `server`.
pub(crate) async fn login_xai_oauth(
    server: &MockServer,
    access: &str,
    refresh: &str,
    userinfo: ResponseTemplate,
) -> LoginResult {
    let recorder = Recorder::new();
    let _mocks = [
        recorder
            .mount_scoped(
                server,
                "POST",
                DEVICE_PATH,
                vec![device(json!({"interval": 5, "expires_in": 900}))],
            )
            .await,
        recorder
            .mount_scoped(server, "GET", OIDC_PATH, vec![discovery(TOKEN_ENDPOINT)])
            .await,
        recorder
            .mount_scoped(server, "POST", TOKEN_PATH, vec![tokens(access, refresh)])
            .await,
        recorder
            .mount_scoped(server, "GET", USERINFO_PATH, vec![userinfo])
            .await,
    ];
    let result = login(&recorder).await.expect("xAI device login");
    assert!(recorder.sleeps().is_empty(), "the first poll succeeded");
    result
}

// ── K01: happy path ─────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn k01_xai_device_login_paces_pending_and_slow_down_and_stores_userinfo_identity() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let recorder = Recorder::new();
    let access = xai_access("jwt-sub");
    mount_xai(
        &server,
        &recorder,
        device(json!({"interval": 5, "expires_in": 900})),
        discovery(TOKEN_ENDPOINT),
        TOKEN_PATH,
        vec![pending(), slow_down(), tokens(&access, "XAI-REFRESH")],
    )
    .await;

    let result = login(&recorder).await.unwrap();
    assert_eq!(result.provider, PROVIDER);
    assert_eq!(result.method, "oauth");
    assert_eq!(result.identity.email.as_deref(), Some("grok@example.test"));

    // Display before discovery; discovery once; the first poll at t=0, then the
    // pending interval and the slow_down-extended interval.
    assert_eq!(
        recorder.events(),
        [
            progress("Requesting device authorization..."),
            post(DEVICE_PATH, NOW),
            display(VERIFY_COMPLETE, USER_CODE),
            progress("Waiting for device authorization..."),
            get(OIDC_PATH, NOW),
            post(TOKEN_PATH, NOW),
            Event::Sleep(5_000),
            post(TOKEN_PATH, NOW + 5_000),
            Event::Sleep(10_000),
            post(TOKEN_PATH, NOW + 15_000),
            get(USERINFO_PATH, NOW + 15_000),
        ]
    );

    let devices = received(&server, DEVICE_PATH).await;
    let device = &devices[0];
    assert_eq!(
        device.headers["content-type"],
        "application/x-www-form-urlencoded"
    );
    assert_eq!(device.headers["accept"], "application/json");
    assert_eq!(body(device), DEVICE_FORM);
    let discovery = received(&server, OIDC_PATH).await;
    assert_eq!(discovery[0].headers["accept"], "application/json");
    let polls = received(&server, TOKEN_PATH).await;
    assert_eq!(polls.len(), 3);
    for poll in &polls {
        assert_eq!(
            poll.headers["content-type"],
            "application/x-www-form-urlencoded"
        );
        assert_eq!(poll.headers["accept"], "application/json");
        assert_eq!(body(poll), poll_form());
    }
    let infos = received(&server, USERINFO_PATH).await;
    let info = &infos[0];
    assert_eq!(info.headers["authorization"], format!("Bearer {access}"));
    assert!(info.body.is_empty());

    let rows = rows(PROVIDER).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].origin, CredentialOrigin::Login);
    let oauth = oauth(&rows[0]);
    let done = NOW + 15_000;
    assert_eq!(oauth.access.expose_secret(), access);
    assert_eq!(oauth.refresh.expose_secret(), "XAI-REFRESH");
    assert_eq!(oauth.expires_at_ms, done + 3_600_000 - 300_000);
    assert_eq!(oauth.authorized_at_ms, Some(done));
    assert_eq!(oauth.identity.account_id.as_deref(), Some("userinfo-sub"));
    assert_eq!(oauth.identity.email.as_deref(), Some("grok@example.test"));
    assert_eq!(oauth.identity.org_id, None);
    assert_eq!(oauth.identity.org_name, None);
}

#[tokio::test]
#[serial]
async fn k01_userinfo_failure_stores_the_access_jwt_sub() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let access = xai_access("jwt-sub");
    let result = login_xai_oauth(&server, &access, "XAI-REFRESH", ResponseTemplate::new(500)).await;
    assert_eq!(result.identity.account_id.as_deref(), Some("jwt-sub"));
    assert_eq!(result.identity.email, None);
    assert_eq!(received(&server, USERINFO_PATH).await.len(), 1);
    let rows = rows(PROVIDER).await;
    assert_eq!(rows.len(), 1);
    let identity = &oauth(&rows[0]).identity;
    assert_eq!(identity.account_id.as_deref(), Some("jwt-sub"));
    assert_eq!(identity.email, None);
}

// ── K02: failures ───────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn k02_device_code_expiry_caps_the_last_wait_and_never_sends_the_late_poll() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    let recorder = Recorder::new();
    mount_xai(
        &server,
        &recorder,
        device(json!({"interval": 5, "expires_in": 7})),
        discovery(TOKEN_ENDPOINT),
        TOKEN_PATH,
        vec![pending(), pending(), tokens(&xai_access("late"), "LATE")],
    )
    .await;

    assert_eq!(failure(login(&recorder).await), "Device flow timed out");
    assert_eq!(recorder.times(TOKEN_PATH), [NOW, NOW + 5_000]);
    assert_eq!(recorder.sleeps(), [5_000, 2_000]);
    assert_eq!(
        recorder.paths(),
        [DEVICE_PATH, OIDC_PATH, TOKEN_PATH, TOKEN_PATH]
    );
    assert_nothing_stored(&world);
}

#[tokio::test]
#[serial]
async fn k02_expiry_after_slow_down_reports_clock_drift() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    let recorder = Recorder::new();
    mount_xai(
        &server,
        &recorder,
        device(json!({"interval": 5, "expires_in": 20})),
        discovery(TOKEN_ENDPOINT),
        TOKEN_PATH,
        vec![slow_down(), pending(), tokens(&xai_access("late"), "LATE")],
    )
    .await;

    assert_eq!(
        failure(login(&recorder).await),
        "Device flow timed out after one or more slow_down responses. This is often caused by clock drift in WSL or VM environments. Please sync or restart the VM clock and try again."
    );
    assert_eq!(recorder.times(TOKEN_PATH), [NOW, NOW + 10_000]);
    assert_eq!(recorder.sleeps(), [10_000, 10_000]);
    assert_nothing_stored(&world);
}

#[tokio::test]
#[serial]
async fn k02_terminal_poll_replies_fail_with_exact_text_and_stop_polling() {
    let cases = [
        (
            poll_error("expired_token"),
            "xai-oauth device code expired; restart the login",
        ),
        (
            poll_error("access_denied"),
            "xai-oauth device authorization was denied",
        ),
        (
            json_reply(
                400,
                json!({"error": "invalid_scope", "error_description": "SECRET-DETAIL"}),
            ),
            "xai-oauth device token request failed: 400",
        ),
        (
            json_reply(200, json!({"token_type": "Bearer", "expires_in": 3600})),
            "xai-oauth token response missing access token",
        ),
        (
            ResponseTemplate::new(200).set_body_string("SECRET-NOT-JSON"),
            "xai-oauth token response missing access token",
        ),
        (
            json_reply(
                200,
                json!({"access_token": xai_access("no-expiry"), "refresh_token": "R"}),
            ),
            "xai-oauth token response missing expires_in",
        ),
    ];
    for (reply, expected) in cases {
        let server = MockServer::start().await;
        let world = World::new(&server);
        let recorder = Recorder::new();
        mount_xai(
            &server,
            &recorder,
            device(json!({"interval": 5, "expires_in": 900})),
            discovery(TOKEN_ENDPOINT),
            TOKEN_PATH,
            vec![reply, pending()],
        )
        .await;
        assert_eq!(failure(login(&recorder).await), expected);
        assert_eq!(
            recorder.paths(),
            [DEVICE_PATH, OIDC_PATH, TOKEN_PATH],
            "{expected}: no further request"
        );
        assert!(recorder.sleeps().is_empty(), "{expected}");
        assert_nothing_stored(&world);
    }
}

#[tokio::test]
#[serial]
async fn k02_cancel_during_the_recorded_wait_freezes_polling() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    let recorder = Recorder::cancelling_at_sleep(0);
    mount_xai(
        &server,
        &recorder,
        device(json!({"interval": 5, "expires_in": 900})),
        discovery(TOKEN_ENDPOINT),
        TOKEN_PATH,
        vec![pending(), tokens(&xai_access("late"), "LATE")],
    )
    .await;

    let result = login(&recorder).await;
    assert!(matches!(result, Err(FlowError::Cancelled)), "{result:?}");
    assert_eq!(failure(result), "Login cancelled");
    assert_eq!(recorder.sleeps(), [5_000]);
    assert_eq!(recorder.times(TOKEN_PATH), [NOW]);
    assert_eq!(received(&server, TOKEN_PATH).await.len(), 1);
    assert_nothing_stored(&world);
}

#[tokio::test]
#[serial]
async fn k02_cancel_during_an_in_flight_poll_drops_its_connection() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    world.set(
        "RUSTAIN_TEST_AUTH_ORIGIN",
        &format!("http://{}", listener.local_addr().unwrap()),
    );
    let recorder = Recorder::new();
    let stub = async {
        let held = accept(&listener).await;
        assert_eq!(held.path(), DEVICE_PATH);
        held.json(&device_body(json!({"interval": 5, "expires_in": 900})))
            .await;
        let held = accept(&listener).await;
        assert_eq!(held.path(), OIDC_PATH);
        held.json(&json!({"token_endpoint": TOKEN_ENDPOINT})).await;
        let held = accept(&listener).await;
        assert_eq!(held.path(), TOKEN_PATH);
        // The poll is in flight and unanswered: the operator cancels now.
        recorder.cancel.cancel();
        held
    };
    let (result, mut poll) = tokio::join!(login(&recorder), stub);
    assert!(matches!(result, Err(FlowError::Cancelled)), "{result:?}");
    let mut byte = [0_u8; 1];
    let read = tokio::time::timeout(GUARD, poll.stream.read(&mut byte))
        .await
        .expect("the cancelled poll's connection must close promptly")
        .unwrap();
    assert_eq!(read, 0, "cancellation closes the in-flight poll");
    assert!(
        listener.accept().now_or_never().is_none(),
        "no request after cancellation"
    );
    assert!(recorder.sleeps().is_empty());
    assert_nothing_stored(&world);
}

#[tokio::test]
#[serial]
async fn k02_poll_transport_failure_before_the_deadline_is_exact_and_final() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    world.set(
        "RUSTAIN_TEST_AUTH_ORIGIN",
        &format!("http://{}", listener.local_addr().unwrap()),
    );
    let recorder = Recorder::new();
    let stub = async {
        let held = accept(&listener).await;
        assert_eq!(held.path(), DEVICE_PATH);
        held.json(&device_body(json!({"interval": 5, "expires_in": 900})))
            .await;
        let held = accept(&listener).await;
        assert_eq!(held.path(), OIDC_PATH);
        held.json(&json!({"token_endpoint": TOKEN_ENDPOINT})).await;
        let held = accept(&listener).await;
        assert_eq!(held.path(), TOKEN_PATH);
        // The poll's connection closes before any response.
        drop(held);
    };
    let (result, ()) = tokio::join!(login(&recorder), stub);
    assert_eq!(
        failure(result),
        "xai-oauth device token request failed: transport unavailable"
    );
    assert!(
        listener.accept().now_or_never().is_none(),
        "a failed poll is final"
    );
    assert!(recorder.sleeps().is_empty());
    assert_nothing_stored(&world);
}

#[tokio::test]
#[serial]
async fn device_authorization_failures_are_exact_and_never_display_or_discover() {
    let cases = [
        (
            device(json!({"verification_uri": null, "verification_uri_complete": null})),
            "xai-oauth device authorization response missing required fields",
        ),
        (
            device(json!({"user_code": null})),
            "xai-oauth device authorization response missing required fields",
        ),
        (
            device(json!({"device_code": ""})),
            "xai-oauth device authorization response missing required fields",
        ),
        (
            ResponseTemplate::new(200).set_body_string("SECRET-NOT-JSON"),
            "xai-oauth device authorization response missing required fields",
        ),
        (
            json_reply(
                400,
                json!({"error": "invalid_request", "error_description": "SECRET-DETAIL"}),
            ),
            "xai-oauth device authorization failed: 400 invalid_request",
        ),
        (
            json_reply(400, json!({"error": "invalid_scope"})),
            "xai-oauth device authorization failed: 400",
        ),
        (
            ResponseTemplate::new(503).set_body_string("SECRET-UPSTREAM"),
            "xai-oauth device authorization failed: 503",
        ),
    ];
    for (reply, expected) in cases {
        let server = MockServer::start().await;
        let world = World::new(&server);
        let recorder = Recorder::new();
        mount_xai(
            &server,
            &recorder,
            reply,
            discovery(TOKEN_ENDPOINT),
            TOKEN_PATH,
            vec![tokens(&xai_access("never"), "NEVER")],
        )
        .await;
        assert_eq!(failure(login(&recorder).await), expected);
        assert_eq!(recorder.paths(), [DEVICE_PATH], "{expected}");
        assert!(
            !recorder
                .events()
                .iter()
                .any(|event| matches!(event, Event::Display { .. })),
            "{expected}"
        );
        assert_nothing_stored(&world);
    }

    let server = MockServer::start().await;
    let world = World::new(&server);
    world.set("RUSTAIN_TEST_AUTH_ORIGIN", &unreachable_origin());
    let recorder = Recorder::new();
    assert_eq!(
        failure(login(&recorder).await),
        "xai-oauth device authorization failed: transport unavailable"
    );
    assert_nothing_stored(&world);
}

#[tokio::test]
#[serial]
async fn rfc8628_interval_defaults_to_five_seconds_and_never_drops_below_one() {
    // `interval` must be a JSON number; anything else is the RFC default.
    let cases = [
        (json!({"interval": null}), 5_000),
        (json!({"interval": 2.5}), 2_500),
        (json!({"interval": 0.5}), 1_000),
        (json!({"interval": 0}), 1_000),
        (json!({"interval": 1.0009}), 1_000),
        (json!({"interval": "7"}), 5_000),
    ];
    for (mut extra, expected) in cases {
        let server = MockServer::start().await;
        let world = World::new(&server);
        let recorder = Recorder::new();
        extra["expires_in"] = json!(900);
        mount_xai(
            &server,
            &recorder,
            device(extra.clone()),
            discovery(TOKEN_ENDPOINT),
            TOKEN_PATH,
            vec![pending(), poll_error("expired_token")],
        )
        .await;
        assert_eq!(
            failure(login(&recorder).await),
            "xai-oauth device code expired; restart the login"
        );
        assert_eq!(recorder.sleeps(), [expected], "{extra}");
        assert_eq!(recorder.times(TOKEN_PATH), [NOW, NOW + expected as i64]);
        assert_nothing_stored(&world);
    }
}

// ── K03: token-endpoint trust ───────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn k03_refused_discovery_sends_no_poll_and_accepted_endpoints_are_rebased() {
    // Every refused endpoint shares the stub-mounted path `/t`: a validator that let
    // one through would rebase it to the stub, where the poll would be counted.
    let invalid = |url: &str| (discovery(url), format!("Invalid xAI token_endpoint: {url}"));
    let not_object = "xAI OIDC discovery response was not a JSON object.".to_owned();
    let missing = "xAI OIDC discovery response was missing token_endpoint.".to_owned();
    let refused = [
        invalid("http://auth.x.ai/t"),
        invalid("https://evilx.ai/t"),
        invalid("https://x.ai.evil.com/t"),
        invalid("https://127.0.0.1/t"),
        (
            json_reply(201, json!({"token_endpoint": "https://auth.x.ai/t"})),
            "xAI OIDC discovery returned status 201.".to_owned(),
        ),
        (
            ResponseTemplate::new(200).set_body_string("{\"token_endpoint\": "),
            "xAI OIDC discovery returned invalid JSON".to_owned(),
        ),
        (
            ResponseTemplate::new(200).set_body_string("null"),
            not_object.clone(),
        ),
        (json_reply(200, json!("https://auth.x.ai/t")), not_object),
        (
            json_reply(200, json!([{"token_endpoint": "https://auth.x.ai/t"}])),
            missing.clone(),
        ),
        (json_reply(200, json!({"token_endpoint": "   "})), missing),
    ];
    for (oidc, expected) in refused {
        let server = MockServer::start().await;
        let world = World::new(&server);
        let recorder = Recorder::new();
        mount_xai(
            &server,
            &recorder,
            device(json!({"interval": 5, "expires_in": 900})),
            oidc,
            "/t",
            vec![tokens(&xai_access("refused"), "REFUSED")],
        )
        .await;
        assert_eq!(failure(login(&recorder).await), expected);
        assert_eq!(recorder.paths(), [DEVICE_PATH, OIDC_PATH], "{expected}");
        assert!(received(&server, "/t").await.is_empty(), "{expected}");
        assert_nothing_stored(&world);
    }

    for accepted in ["https://x.ai/t", "https://AUTH.X.AI:8443/t"] {
        let server = MockServer::start().await;
        let _world = World::new(&server);
        let recorder = Recorder::new();
        let access = xai_access("accepted");
        mount_xai(
            &server,
            &recorder,
            device(json!({"interval": 5, "expires_in": 900})),
            discovery(accepted),
            "/t",
            vec![tokens(&access, "ACCEPTED")],
        )
        .await;
        login(&recorder).await.expect(accepted);
        let polls = received(&server, "/t").await;
        assert_eq!(polls.len(), 1, "{accepted}");
        assert_eq!(body(&polls[0]), poll_form(), "{accepted}");
        assert_eq!(
            oauth(&rows(PROVIDER).await[0]).access.expose_secret(),
            access
        );
    }
}

#[tokio::test]
#[serial]
async fn oidc_discovery_message_table() {
    let missing = "xAI OIDC discovery response was missing token_endpoint.";
    let not_object = "xAI OIDC discovery response was not a JSON object.";
    let cases = [
        (
            ResponseTemplate::new(500).set_body_string("SECRET-UPSTREAM"),
            "xAI OIDC discovery returned status 500.",
        ),
        // The credential client never follows a redirect, so a 302 is a status.
        (
            ResponseTemplate::new(302).insert_header("location", "https://auth.x.ai/elsewhere"),
            "xAI OIDC discovery returned status 302.",
        ),
        (json_reply(200, json!(7)), not_object),
        (json_reply(200, json!(true)), not_object),
        (json_reply(200, json!({})), missing),
        (json_reply(200, json!({"token_endpoint": 7})), missing),
        (json_reply(200, json!({"token_endpoint": null})), missing),
    ];
    for (oidc, expected) in cases {
        let server = MockServer::start().await;
        let world = World::new(&server);
        let recorder = Recorder::new();
        mount_xai(
            &server,
            &recorder,
            device(json!({"interval": 5, "expires_in": 900})),
            oidc,
            TOKEN_PATH,
            vec![tokens(&xai_access("never"), "NEVER")],
        )
        .await;
        assert_eq!(failure(login(&recorder).await), expected);
        assert_eq!(recorder.paths(), [DEVICE_PATH, OIDC_PATH], "{expected}");
        assert_nothing_stored(&world);
    }

    // A discovery connection that closes without a response is a transport failure.
    let server = MockServer::start().await;
    let world = World::new(&server);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    world.set(
        "RUSTAIN_TEST_AUTH_ORIGIN",
        &format!("http://{}", listener.local_addr().unwrap()),
    );
    let recorder = Recorder::new();
    let stub = async {
        let held = accept(&listener).await;
        held.json(&device_body(json!({"interval": 5, "expires_in": 900})))
            .await;
        let held = accept(&listener).await;
        assert_eq!(held.path(), OIDC_PATH);
        drop(held);
    };
    let (result, ()) = tokio::join!(login(&recorder), stub);
    assert_eq!(
        failure(result),
        "xAI OIDC discovery failed: transport unavailable"
    );
    assert!(listener.accept().now_or_never().is_none(), "no poll");
    assert_nothing_stored(&world);
}

// ── access-JWT `sub` (userinfo unavailable) ─────────────────────────────────

/// `header.<payload>.signature` with the standard alphabet and `=` padding.
fn padded_standard_token(sub: &str) -> String {
    let encode = |claims: Value| STANDARD.encode(serde_json::to_vec(&claims).unwrap());
    let payload = (0..3)
        .map(|filler| {
            // U+FFFF runs encode to `+`/`/` symbols at every alignment.
            encode(json!({"sub": sub, "f": "x".repeat(filler), "u": "\u{ffff}\u{ffff}\u{ffff}"}))
        })
        .find(|encoded| encoded.ends_with('='))
        .unwrap();
    assert!(
        payload.contains('+') || payload.contains('/'),
        "vector must exercise the standard alphabet"
    );
    format!("e30.{payload}.signature")
}

#[tokio::test]
#[serial]
async fn access_jwt_sub_decoder_accepts_both_alphabets_and_rejects_malformed_tokens() {
    let url_safe = |claims: Value| URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
    let cases = [
        (xai_access("url-safe-sub"), Some("url-safe-sub")),
        (padded_standard_token("standard-sub"), Some("standard-sub")),
        (jwt(json!({"sub": 42})), Some("42")),
        (jwt(json!({"sub": ""})), None),
        (jwt(json!({"sub": true})), None),
        (
            format!("e30.{}", url_safe(json!({"sub": "two-part"}))),
            None,
        ),
        (
            format!(
                "e30.{}.signature.extra",
                url_safe(json!({"sub": "four-part"}))
            ),
            None,
        ),
        (jwt(json!(["sub", "array"])), None),
        (jwt(json!("sub")), None),
        ("e30.%%%.signature".to_owned(), None),
    ];
    for (access, expected) in cases {
        let server = MockServer::start().await;
        let _world = World::new(&server);
        let result = login_xai_oauth(&server, &access, "REFRESH", ResponseTemplate::new(500)).await;
        assert_eq!(
            result.identity.account_id.as_deref(),
            expected,
            "{expected:?}"
        );
        let rows = rows(PROVIDER).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(oauth(&rows[0]).identity.account_id.as_deref(), expected);
        assert_eq!(oauth(&rows[0]).identity.email, None);
    }
}
