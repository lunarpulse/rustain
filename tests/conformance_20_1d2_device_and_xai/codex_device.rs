//! Codex device login through `flows::login` with the production `CODEX_DEVICE`
//! policy (K04): OpenAI's custom device protocol, its paced polls, the exchange with
//! the server-issued verifier, and the stored row served and refreshed by the existing
//! Codex consumer. Plus the interval, 120-poll exhaustion, start/poll/exchange
//! failure and cancellation plain tests.

use super::device::{
    Event, Recorder, assert_nothing_stored, body, display, failure, json_reply, oauth, post,
    progress, received, rows, run_login, unreachable_origin,
};
use crate::conformance_20_1d_codex_subscription::auth::jwt;
use crate::conformance_20_1d1b_codex_replay_discovery::common::{
    MODEL, NOW, RESPONSE_PATH, World, accept, codex, completed, mount_script, sse, text,
};
use futures::{FutureExt, StreamExt};
use rustain::adapters::auth::flows::{FlowError, LoginResult};
use rustain::adapters::auth::policies::CODEX_DEVICE;
use rustain::domain::models::{
    AuthMethod, CompletionOptions, CredentialOrigin, Message, MessageRole, StreamChunk,
};
use serde_json::{Value, json};
use serial_test::serial;
use wiremock::{MockServer, ResponseTemplate};

const PROVIDER: &str = "openai-codex";
const CODEX: AuthMethod = AuthMethod::Device(&CODEX_DEVICE);
const USERCODE_PATH: &str = "/api/accounts/deviceauth/usercode";
const POLL_PATH: &str = "/api/accounts/deviceauth/token";
const EXCHANGE_PATH: &str = "/oauth/token";
/// Displayed verbatim (never rebased): the operator's browser visits OpenAI itself.
const VERIFY: &str = "https://auth.openai.com/codex/device";
const DEVICE_AUTH_ID: &str = "deviceauth-1";
const USER_CODE: &str = "CODX-12345";
const START_BODY: &str = r#"{"client_id":"app_EMoamEEZ73f0CkXaXp7hrann"}"#;
const POLL_BODY: &str = r#"{"device_auth_id":"deviceauth-1","user_code":"CODX-12345"}"#;
const EXCHANGE_FORM: &str = "grant_type=authorization_code&client_id=app_EMoamEEZ73f0CkXaXp7hrann&code=AC&code_verifier=SV&redirect_uri=https%3A%2F%2Fauth.openai.com%2Fdeviceauth%2Fcallback";
const REFRESH_FORM: &str =
    "grant_type=refresh_token&client_id=app_EMoamEEZ73f0CkXaXp7hrann&refresh_token=DEVICE-REFRESH";
const POLLS_EXHAUSTED: &str =
    "Device authorization timed out — user did not complete login in time";

fn start_body(extra: Value) -> Value {
    let mut body = json!({"device_auth_id": DEVICE_AUTH_ID, "user_code": USER_CODE});
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
fn usercode(extra: Value) -> ResponseTemplate {
    json_reply(200, start_body(extra))
}
fn authorized_body() -> Value {
    json!({"authorization_code": "AC", "code_verifier": "SV", "code_challenge": "CHALLENGE"})
}
fn authorized() -> ResponseTemplate {
    json_reply(200, authorized_body())
}
fn status(code: u16) -> ResponseTemplate {
    ResponseTemplate::new(code)
}
/// A Codex access JWT for one account; `grant` tells login and refresh apart.
fn access(grant: &str) -> String {
    jwt(json!({
        "jti": grant,
        "https://api.openai.com/auth": {"chatgpt_account_id": "acct-device", "chatgpt_plan_type": "Pro"},
        "https://api.openai.com/profile": {"email": "Device@Example.TEST"},
    }))
}
fn tokens(access: &str, refresh: &str) -> ResponseTemplate {
    json_reply(
        200,
        json!({"access_token": access, "refresh_token": refresh, "expires_in": 3600}),
    )
}

async fn mount_codex(
    server: &MockServer,
    recorder: &Recorder,
    start: ResponseTemplate,
    polls: Vec<ResponseTemplate>,
    exchange: Vec<ResponseTemplate>,
) {
    recorder
        .mount(server, "POST", USERCODE_PATH, vec![start])
        .await;
    recorder.mount(server, "POST", POLL_PATH, polls).await;
    recorder
        .mount(server, "POST", EXCHANGE_PATH, exchange)
        .await;
}
async fn login(recorder: &Recorder) -> Result<LoginResult, FlowError> {
    run_login(recorder, PROVIDER, &CODEX).await
}
fn never() -> Vec<ResponseTemplate> {
    vec![tokens(&access("never"), "NEVER")]
}

fn user(text: &str) -> Message {
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
fn options() -> CompletionOptions {
    CompletionOptions {
        model: MODEL.into(),
        max_tokens: 4096,
        system_prompt: String::new(),
        temperature: None,
        tools: vec![],
        session_id: Some("codex-device-session".into()),
    }
}

// ── K04 ─────────────────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn k04_codex_device_login_exchanges_the_server_verifier_and_its_row_serves_codex() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let recorder = Recorder::new();
    let login_access = access("device-login");
    let refreshed = access("device-refresh");
    // interval 7 → I = 7·1000 + 3000 = 10 000 ms; the first wait is capped at 5 000.
    mount_codex(
        &server,
        &recorder,
        usercode(json!({"interval": 7})),
        vec![status(403), status(404), authorized()],
        vec![
            tokens(&login_access, "DEVICE-REFRESH"),
            tokens(&refreshed, "DEVICE-REFRESH-2"),
        ],
    )
    .await;

    let result = login(&recorder).await.unwrap();
    assert_eq!(result.provider, PROVIDER);
    assert_eq!(result.method, "oauth");
    assert_eq!(
        recorder.events(),
        [
            progress("Initiating device authorization…"),
            post(USERCODE_PATH, NOW),
            display(VERIFY, USER_CODE),
            progress("Waiting for browser authorization (code: CODX-12345)…"),
            Event::Sleep(5_000),
            post(POLL_PATH, NOW + 5_000),
            Event::Sleep(10_000),
            post(POLL_PATH, NOW + 15_000),
            Event::Sleep(10_000),
            post(POLL_PATH, NOW + 25_000),
            progress("Exchanging authorization code for tokens…"),
            post(EXCHANGE_PATH, NOW + 25_000),
        ]
    );

    let starts = received(&server, USERCODE_PATH).await;
    assert_eq!(starts[0].headers["content-type"], "application/json");
    assert_eq!(body(&starts[0]), START_BODY);
    let polls = received(&server, POLL_PATH).await;
    assert_eq!(polls.len(), 3);
    for poll in &polls {
        assert_eq!(poll.headers["content-type"], "application/json");
        assert_eq!(body(poll), POLL_BODY);
    }
    let exchanges = received(&server, EXCHANGE_PATH).await;
    assert_eq!(exchanges.len(), 1);
    assert_eq!(
        exchanges[0].headers["content-type"],
        "application/x-www-form-urlencoded"
    );
    assert_eq!(body(&exchanges[0]), EXCHANGE_FORM);

    let stored = rows(PROVIDER).await;
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].origin, CredentialOrigin::Login);
    let row = oauth(&stored[0]);
    let done = NOW + 25_000;
    assert_eq!(row.access.expose_secret(), login_access);
    assert_eq!(row.refresh.expose_secret(), "DEVICE-REFRESH");
    assert_eq!(row.expires_at_ms, done + 3_600_000, "Codex skew is 0");
    assert_eq!(row.authorized_at_ms, Some(done));
    assert_eq!(row.identity.account_id.as_deref(), Some("acct-device"));
    assert_eq!(row.identity.email.as_deref(), Some("device@example.test"));
    assert_eq!(row.identity.org_id.as_deref(), Some("acct-device"));
    assert_eq!(row.identity.org_name.as_deref(), Some("pro"));

    // The existing Codex consumer serves the device row; a 401 refreshes it once
    // through the existing Codex refresh policy and the request is resent once.
    mount_script(
        &server,
        vec![
            json_reply(401, json!({"error": {"message": "token expired"}})),
            sse(&[text("served"), completed()]),
        ],
    )
    .await;
    let chunks: Vec<StreamChunk> = codex()
        .stream_completion(vec![user("hello")], options())
        .await
        .expect("the device row serves Codex")
        .collect()
        .await;
    assert!(
        chunks
            .iter()
            .any(|chunk| matches!(chunk, StreamChunk::Text { content, .. } if content == "served")),
        "{chunks:?}"
    );
    let sends = received(&server, RESPONSE_PATH).await;
    assert_eq!(sends.len(), 2, "one send, one resend");
    for (send, bearer) in sends.iter().zip([&login_access, &refreshed]) {
        assert_eq!(send.headers["authorization"], format!("Bearer {bearer}"));
        assert_eq!(send.headers["chatgpt-account-id"], "acct-device");
    }
    let token_posts = received(&server, EXCHANGE_PATH).await;
    assert_eq!(token_posts.len(), 2, "one exchange, one refresh");
    assert_eq!(body(&token_posts[1]), REFRESH_FORM);
    let rotated = rows(PROVIDER).await;
    assert_eq!(rotated.len(), 1);
    assert_eq!(oauth(&rotated[0]).access.expose_secret(), refreshed);
    assert_eq!(
        oauth(&rotated[0]).refresh.expose_secret(),
        "DEVICE-REFRESH-2"
    );
}

// ── plain tests ─────────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn codex_interval_follows_javascript_parsing_and_caps_only_the_first_wait() {
    // Numbers are taken as-is; strings go through parseInt, where 0 and NaN mean 5.
    let cases: [(Value, [u64; 2]); 6] = [
        (json!({"interval": null}), [5_000, 8_000]),
        (json!({"interval": "0"}), [5_000, 8_000]),
        (json!({"interval": 0}), [3_000, 3_000]),
        (json!({"interval": " 12abc"}), [5_000, 15_000]),
        (json!({"interval": 2.5}), [5_000, 5_500]),
        (json!({"interval": -10}), [0, 0]),
    ];
    for (extra, expected) in cases {
        let server = MockServer::start().await;
        let world = World::new(&server);
        let recorder = Recorder::new();
        mount_codex(
            &server,
            &recorder,
            usercode(extra.clone()),
            vec![status(403), status(500)],
            never(),
        )
        .await;
        assert_eq!(
            failure(login(&recorder).await),
            "Device token polling failed: 500"
        );
        assert_eq!(recorder.sleeps(), expected, "{extra}");
        assert_eq!(
            recorder.paths(),
            [USERCODE_PATH, POLL_PATH, POLL_PATH],
            "{extra}"
        );
        assert_nothing_stored(&world);
    }
}

#[tokio::test]
#[serial]
async fn codex_device_gives_up_after_120_pending_polls() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    let recorder = Recorder::new();
    mount_codex(
        &server,
        &recorder,
        usercode(json!({"interval": 0})),
        vec![status(404)],
        never(),
    )
    .await;
    assert_eq!(failure(login(&recorder).await), POLLS_EXHAUSTED);
    assert_eq!(received(&server, POLL_PATH).await.len(), 120);
    assert_eq!(recorder.sleeps(), vec![3_000; 120]);
    assert!(received(&server, EXCHANGE_PATH).await.is_empty());
    assert_nothing_stored(&world);
}

#[tokio::test]
#[serial]
async fn codex_device_start_failures_are_exact() {
    let missing = "Device authorization response missing required fields";
    let cases = [
        (
            ResponseTemplate::new(500).set_body_string("SECRET-UPSTREAM"),
            "Device authorization initiation failed: 500",
        ),
        (
            json_reply(429, json!({"error": "rate_limited"})),
            "Device authorization initiation failed: 429",
        ),
        (usercode(json!({"user_code": null})), missing),
        (usercode(json!({"device_auth_id": ""})), missing),
        (usercode(json!({"user_code": 12345})), missing),
        (
            ResponseTemplate::new(200).set_body_string("SECRET-NOT-JSON"),
            missing,
        ),
    ];
    for (start, expected) in cases {
        let server = MockServer::start().await;
        let world = World::new(&server);
        let recorder = Recorder::new();
        mount_codex(&server, &recorder, start, vec![authorized()], never()).await;
        assert_eq!(failure(login(&recorder).await), expected);
        assert_eq!(recorder.paths(), [USERCODE_PATH], "{expected}");
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
        "Device authorization initiation failed: transport unavailable"
    );
    assert_nothing_stored(&world);
}

#[tokio::test]
#[serial]
async fn codex_device_poll_failures_are_exact() {
    let missing = "Device token response missing authorization_code or code_verifier";
    let cases = [
        (
            json_reply(200, json!({"authorization_code": "AC"})),
            missing,
        ),
        (
            json_reply(
                200,
                json!({"authorization_code": "", "code_verifier": "SV"}),
            ),
            missing,
        ),
        (
            ResponseTemplate::new(200).set_body_string("SECRET-NOT-JSON"),
            missing,
        ),
        (status(429), "Device token polling failed: 429"),
        (
            json_reply(401, json!({"error": "invalid_grant"})),
            "Device token polling failed: 401",
        ),
    ];
    for (poll, expected) in cases {
        let server = MockServer::start().await;
        let world = World::new(&server);
        let recorder = Recorder::new();
        mount_codex(
            &server,
            &recorder,
            usercode(json!({"interval": 5})),
            vec![poll],
            never(),
        )
        .await;
        assert_eq!(failure(login(&recorder).await), expected);
        assert_eq!(recorder.paths(), [USERCODE_PATH, POLL_PATH], "{expected}");
        assert_nothing_stored(&world);
    }
}

#[tokio::test]
#[serial]
async fn codex_device_exchange_failures_are_exact_and_redacted() {
    let missing = "Token response missing required fields";
    let cases = [
        (
            json_reply(
                400,
                json!({"error": "invalid_grant", "error_description": "SECRET-DETAIL"}),
            ),
            "Token exchange failed: invalid_grant",
        ),
        (
            json_reply(500, json!({"error": "server_error"})),
            "Token exchange failed (HTTP 500)",
        ),
        (
            json_reply(
                200,
                json!({"access_token": access("x"), "expires_in": 3600}),
            ),
            missing,
        ),
        (
            json_reply(
                200,
                json!({"access_token": access("x"), "refresh_token": "R", "expires_in": "3600"}),
            ),
            missing,
        ),
        (
            json_reply(
                200,
                json!({"access_token": jwt(json!({"sub": "no-codex-claims"})), "refresh_token": "R", "expires_in": 3600}),
            ),
            "Failed to extract account identity from token",
        ),
    ];
    for (exchange, expected) in cases {
        let server = MockServer::start().await;
        let world = World::new(&server);
        let recorder = Recorder::new();
        mount_codex(
            &server,
            &recorder,
            usercode(json!({"interval": 5})),
            vec![authorized()],
            vec![exchange],
        )
        .await;
        assert_eq!(failure(login(&recorder).await), expected);
        assert_eq!(
            recorder.paths(),
            [USERCODE_PATH, POLL_PATH, EXCHANGE_PATH],
            "{expected}"
        );
        assert_nothing_stored(&world);
    }
}

#[tokio::test]
#[serial]
async fn codex_device_transport_failures_are_redacted() {
    // A poll, then an exchange, whose connection closes without a response.
    for (last_path, expected) in [
        (
            POLL_PATH,
            "Device token polling failed: transport unavailable",
        ),
        (
            EXCHANGE_PATH,
            "Token exchange failed: transport unavailable",
        ),
    ] {
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
            assert_eq!(held.path(), USERCODE_PATH);
            held.json(&start_body(json!({"interval": 5}))).await;
            let held = accept(&listener).await;
            assert_eq!(held.path(), POLL_PATH);
            if last_path == POLL_PATH {
                drop(held);
                return;
            }
            held.json(&authorized_body()).await;
            let held = accept(&listener).await;
            assert_eq!(held.path(), EXCHANGE_PATH);
            drop(held);
        };
        let (result, ()) = tokio::join!(login(&recorder), stub);
        assert_eq!(failure(result), expected);
        assert!(
            listener.accept().now_or_never().is_none(),
            "{expected}: no further request"
        );
        assert_nothing_stored(&world);
    }
}

#[tokio::test]
#[serial]
async fn codex_device_cancel_during_the_first_wait_sends_no_poll() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    let recorder = Recorder::cancelling_at_sleep(0);
    mount_codex(
        &server,
        &recorder,
        usercode(json!({"interval": 7})),
        vec![authorized()],
        never(),
    )
    .await;
    let result = login(&recorder).await;
    assert!(matches!(result, Err(FlowError::Cancelled)), "{result:?}");
    assert_eq!(failure(result), "Login cancelled");
    assert_eq!(recorder.sleeps(), [5_000]);
    assert_eq!(recorder.paths(), [USERCODE_PATH]);
    assert_nothing_stored(&world);
}
