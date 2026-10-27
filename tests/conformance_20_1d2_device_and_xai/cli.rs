//! CLI front door (K05) and debug-binary device login → `ask` → logout flows (K12).
//!
//! Real-binary proof on the 20-1d CLI `Fixture`: build the binary with
//! `--features test-instrumentation` first; `RUSTAIN_TEST_CODEX_BINARY` selects it,
//! else the cargo-bin fallback (same instrumentation requirement). Each test keeps
//! its command/output/request evidence under
//! `<RUSTAIN_TEST_CODEX_EVIDENCE_DIR>/20-1d2-<test>/` (a kept temp dir when unset).
//!
//! A device flow sends its first request before printing anything, so a binary
//! built without instrumentation would reach the real auth host. `preflight` first
//! proves that the running binary rebases auth endpoints to the loopback stub, via
//! the browser flow, which prints its authorize URL before any HTTP and is killed
//! right there. No test follows a printed verification URL.

use super::device::{
    DEVICE_PATH, OIDC_PATH, PROVIDER as XAI_OAUTH, TOKEN_ENDPOINT, TOKEN_PATH, USERINFO_PATH,
};
use crate::conformance_20_1d_codex_subscription::auth::jwt;
use crate::conformance_20_1d_codex_subscription::cli::Fixture;
use rustain::adapters::auth_store::FileAuthStore;
use rustain::domain::models::{Credential, CredentialOrigin, OAuthCredential, StoredCredential};
use rustain::domain::ports::AuthStorePort;
use serde_json::{Value, json};
use serial_test::serial;
use std::{process::Output, time::Duration};
use tokio::io::{AsyncBufReadExt, BufReader};
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{method, path},
};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const MODELS_DEV_PATH: &str = "/api.json";

const CODEX: &str = "openai-codex";
const CODEX_CLIENT: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const USERCODE_PATH: &str = "/api/accounts/deviceauth/usercode";
const POLL_PATH: &str = "/api/accounts/deviceauth/token";
const EXCHANGE_PATH: &str = "/oauth/token";
const CODEX_MODELS_PATH: &str = "/backend-api/codex/models";
const CODEX_RESPONSES_PATH: &str = "/backend-api/codex/responses";
/// Displayed verbatim by the binary; never requested by this test.
const CODEX_VERIFICATION_URL: &str = "https://auth.openai.com/codex/device";
const CODEX_DEVICE_AUTH_ID: &str = "K05-DEVICE-AUTH-ID";
const CODEX_USER_CODE: &str = "K05-USER-CODE";
const CODEX_AUTH_CODE: &str = "K05-AUTHORIZATION-CODE";
const CODEX_SERVER_VERIFIER: &str = "K05-SERVER-ISSUED-VERIFIER";
const CODEX_REFRESH: &str = "K05-CODEX-REFRESH";
const CODEX_TEXT: &str = "hello Codex device fixture";
const CODEX_LOGIN_NAME: &str = "ChatGPT Plus/Pro (Codex Subscription)";

const XAI_CLIENT: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const XAI_SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
const XAI_MODELS_PATH: &str = "/v1/models";
const XAI_RESPONSES_PATH: &str = "/v1/responses";
const XAI_DEVICE_CODE: &str = "K12-XAI-DEVICE-CODE";
const XAI_USER_CODE: &str = "K12-GROK";
const XAI_REFRESH: &str = "K12-XAI-REFRESH";
const XAI_EMAIL: &str = "grok@example.test";
const XAI_USERINFO_SUB: &str = "k12-userinfo-sub";
const XAI_TEXT: &str = "hello xAI device fixture";
const XAI_LOGIN_NAME: &str = "xAI Grok OAuth (SuperGrok or X Premium+)";
/// The configured entry K12 activates (story K12, verbatim keys).
const XAI_CONFIG: &str = "model = \"grok-4.6\"\n\n[provider.grok]\nprovider_id = \"xai-oauth\"\nkind = \"xai-oauth\"\napi_key_env = \"XAI_OAUTH_TOKEN\"\nmodel_id = \"grok-4.6\"\nenabled = true\ndiscover_models = true\n";

/// The 20-1d fixture with this story's knobs and a per-test evidence directory.
/// Device waits are scaled to zero; the child environment is exactly `env`, so no
/// provider credential variable reaches it.
fn fixture(server: &MockServer, test: &str, config: Option<&str>) -> Fixture {
    let mut fixture = Fixture::new(server);
    let shared = fixture.evidence.clone();
    fixture.evidence = shared.join(format!("20-1d2-{test}"));
    std::fs::create_dir_all(&fixture.evidence).unwrap();
    // A reused evidence destination must not make a failed rerun look passed.
    let passed = fixture.evidence.join("passed.json");
    if passed.exists() {
        std::fs::remove_file(passed).unwrap();
    }
    std::fs::copy(
        shared.join("binary.json"),
        fixture.evidence.join("binary.json"),
    )
    .unwrap();
    fixture
        .env
        .insert("RUSTAIN_TEST_AUTH_SLEEP_SCALE", "0".into());
    let config_path = fixture.home.path().join("config/config.toml");
    if let Some(config) = config {
        std::fs::write(&config_path, config).unwrap();
    }
    std::fs::copy(&config_path, fixture.evidence.join("config.toml")).unwrap();
    for key in fixture.env.keys() {
        assert!(
            !["XAI_", "OPENAI", "ANTHROPIC", "OPENROUTER"]
                .iter()
                .any(|prefix| key.starts_with(prefix)),
            "provider credential variable {key} must be stripped"
        );
    }
    fixture
}

/// Loopback preflight. The auth origin must be the loopback stub, and the binary
/// must actually rebase to it (only instrumented builds do): the browser flow
/// prints its authorize URL before any HTTP, and is killed as soon as it does.
async fn preflight(fixture: &Fixture, server: &MockServer) {
    let origin = url::Url::parse(&server.uri()).unwrap();
    assert_eq!(origin.scheme(), "http");
    assert_eq!(origin.host_str(), Some("127.0.0.1"));
    assert_eq!(
        fixture.env["RUSTAIN_TEST_AUTH_ORIGIN"].to_str(),
        Some(server.uri().as_str())
    );
    let mut child = fixture
        .command("preflight", &["auth", "login", CODEX, "--method", "oauth"])
        .spawn()
        .unwrap();
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut transcript = String::new();
    let printed = tokio::time::timeout(COMMAND_TIMEOUT, async {
        loop {
            let mut line = String::new();
            assert_ne!(
                stderr.read_line(&mut line).await.unwrap(),
                0,
                "preflight exited before printing its authorize URL: {transcript}"
            );
            transcript.push_str(&line);
            assert!(transcript.len() <= 65_536, "unbounded preflight stderr");
            if line.trim().starts_with("http") {
                break url::Url::parse(line.trim()).unwrap();
            }
        }
    })
    .await
    .expect("preflight did not print its authorize URL within its deadline");
    child.kill().await.unwrap();
    std::fs::write(fixture.evidence.join("preflight.stderr"), &transcript).unwrap();
    assert_eq!(
        printed.origin(),
        origin.origin(),
        "binary MUST be built with test-instrumentation: {printed}"
    );
    assert!(
        server.received_requests().await.unwrap().is_empty(),
        "the preflight sends no HTTP"
    );
    fixture.json(
        "preflight.json",
        &json!({
            "auth_origin": server.uri(),
            "authorize_url_from_child_stderr": printed.as_str(),
            "loopback_origin_asserted_before_any_device_request": true,
            "child_killed_before_any_http": true,
        }),
    );
}

/// Binds the browser method's callback port for the guard's lifetime, so any
/// browser-flow attempt fails with the busy-port configuration error.
fn hold_callback_port(fixture: &Fixture) -> std::net::TcpListener {
    let port: u16 = fixture.env["RUSTAIN_TEST_AUTH_CALLBACK_PORT"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
        .expect("the test holds the browser callback port")
}

/// Runs one command to completion without asserting its exit status.
async fn attempt(fixture: &Fixture, label: &str, args: &[&str]) -> Output {
    let output = tokio::time::timeout(COMMAND_TIMEOUT, fixture.command(label, args).output())
        .await
        .unwrap_or_else(|_| panic!("{label} exceeded {COMMAND_TIMEOUT:?}"))
        .unwrap_or_else(|error| panic!("cannot execute {label}: {error}"));
    fixture.capture(label, &output, &output.stderr);
    output
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// `METHOD path` of every provider/auth request, in arrival order (the public
/// catalogue fetch is not part of either flow).
fn routes(requests: &[Request]) -> Vec<String> {
    requests
        .iter()
        .filter(|request| request.url.path() != MODELS_DEV_PATH)
        .map(|request| format!("{} {}", request.method.as_str(), request.url.path()))
        .collect()
}

fn find<'a>(requests: &'a [Request], verb: &str, route: &str) -> Vec<&'a Request> {
    requests
        .iter()
        .filter(|request| request.method.as_str() == verb && request.url.path() == route)
        .collect()
}

fn header<'a>(request: &'a Request, name: &str) -> &'a str {
    request
        .headers
        .get(name)
        .unwrap_or_else(|| panic!("missing {name}"))
        .to_str()
        .unwrap()
}

fn form(request: &Request) -> Vec<(String, String)> {
    url::form_urlencoded::parse(&request.body)
        .into_owned()
        .collect()
}

fn pairs(fields: &[(&str, &str)]) -> Vec<(String, String)> {
    fields
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

fn assert_no_chat_completions(requests: &[Request]) {
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.path().ends_with("/chat/completions"))
            .count(),
        0,
        "Chat Completions fallback is forbidden"
    );
}

/// Rows the login wrote; an absent `auth.json` holds none.
async fn rows(fixture: &Fixture, provider: &str) -> Vec<StoredCredential> {
    let file = fixture.home.path().join("data/auth.json");
    if !file.exists() {
        return Vec::new();
    }
    FileAuthStore::at(file)
        .snapshot()
        .await
        .unwrap()
        .rows(provider)
        .to_vec()
}

fn oauth(row: &StoredCredential) -> &OAuthCredential {
    match &row.credential {
        Credential::OAuth(oauth) => oauth,
        _ => panic!("the device login must store an OAuth row"),
    }
}

fn provider_row<'a>(value: &'a Value, provider: &str) -> &'a Value {
    value["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["provider"] == provider)
        .unwrap_or_else(|| panic!("{provider} row"))
}

fn now_ms(fixture: &Fixture) -> i64 {
    fixture.env["RUSTAIN_TEST_NOW_MS"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap()
}

fn sse(text: &str) -> String {
    format!(
        "data: {}\n\ndata: {}\n\n",
        json!({"type":"response.output_text.delta","delta":text}),
        json!({"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":1,"output_tokens":2}}}),
    )
}

fn assert_no_leak(outputs: &[&Output], secrets: &[&str]) {
    let all: String = outputs
        .iter()
        .map(|output| format!("{}{}", text(&output.stdout), text(&output.stderr)))
        .collect();
    for secret in secrets {
        assert!(
            !all.contains(secret),
            "fixture credential leaked into CLI output"
        );
    }
}

fn codex_access() -> String {
    jwt(json!({
        "https://api.openai.com/auth": {"chatgpt_account_id":"cli-access-account", "chatgpt_plan_type":"Plus "},
        "https://api.openai.com/profile": {"email":" Operator@EXAMPLE.test "},
    }))
}

/// OpenAI's custom device protocol: usercode, one pending (403) poll, an
/// authorization-code poll, the exchange; then live discovery and one turn.
async fn mount_codex_device(server: &MockServer, access: &str) {
    Mock::given(method("POST"))
        .and(path(USERCODE_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_auth_id": CODEX_DEVICE_AUTH_ID,
            "user_code": CODEX_USER_CODE,
            "interval": "5",
        })))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path(POLL_PATH))
        .respond_with(ResponseTemplate::new(403))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path(POLL_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "authorization_code": CODEX_AUTH_CODE,
            "code_verifier": CODEX_SERVER_VERIFIER,
        })))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path(EXCHANGE_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": access,
            "refresh_token": CODEX_REFRESH,
            "expires_in": 3600,
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(MODELS_DEV_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(CODEX_MODELS_PATH))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"models":[{"slug":"gpt-5.6-luna"}]})),
        )
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path(CODEX_RESPONSES_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse(CODEX_TEXT), "text/event-stream"))
        .mount(server)
        .await;
}

/// RFC 8628 against xAI: device code, OIDC discovery, one pending poll, the token,
/// userinfo; then live discovery (with media ids it must exclude) and one turn.
async fn mount_xai_device(server: &MockServer, access: &str) {
    Mock::given(method("POST"))
        .and(path(DEVICE_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_code": XAI_DEVICE_CODE,
            "user_code": XAI_USER_CODE,
            "verification_uri": format!("{}/activate", server.uri()),
            "verification_uri_complete": format!("{}/activate?user_code={XAI_USER_CODE}", server.uri()),
            "expires_in": 600,
            "interval": 5,
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(OIDC_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issuer": "https://auth.x.ai",
            "token_endpoint": TOKEN_ENDPOINT,
        })))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(
            ResponseTemplate::new(400).set_body_json(json!({"error":"authorization_pending"})),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": access,
            "refresh_token": XAI_REFRESH,
            "expires_in": 3600,
            "token_type": "Bearer",
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(USERINFO_PATH))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"sub": XAI_USERINFO_SUB, "email": XAI_EMAIL})),
        )
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(MODELS_DEV_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(XAI_MODELS_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":[
            {"id":"grok-4.6"},{"id":"grok-9"},{"id":"grok-imagine-video-1"},{"id":"grok-stt-1"},{"id":"grok-tts"}
        ]})))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path(XAI_RESPONSES_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse(XAI_TEXT), "text/event-stream"))
        .mount(server)
        .await;
}

#[tokio::test]
#[serial]
async fn k05_k12_debug_binary_codex_device_login_status_ask_logout() {
    // No sleeps: device waits are scaled to zero and every command is bounded.
    tokio::time::timeout(Duration::from_secs(180), async {
        let server = MockServer::start().await;
        let fixture = fixture(&server, "k05-k12-codex-device", None);
        preflight(&fixture, &server).await;
        let _held = hold_callback_port(&fixture);
        let access = codex_access();
        mount_codex_device(&server, &access).await;

        // Non-TTY without --method: the provider's own choices; nothing sent or stored.
        let ambiguous = attempt(&fixture, "no-method", &["auth", "login", CODEX]).await;
        assert!(!ambiguous.status.success());
        let ambiguous_err = text(&ambiguous.stderr);
        assert!(
            ambiguous_err.lines().any(|line| line
                == "Multiple login methods are available; choose --method oauth or --method device."),
            "{ambiguous_err}"
        );
        assert!(server.received_requests().await.unwrap().is_empty());
        assert!(rows(&fixture, CODEX).await.is_empty());

        let login = fixture
            .run("login", &["auth", "login", CODEX, "--method", "device"])
            .await;
        assert_eq!(
            text(&login.stdout).trim(),
            format!("Logged in to {CODEX_LOGIN_NAME} as operator@example.test (plus)")
        );
        let login_err = text(&login.stderr);
        assert!(
            login_err.contains(&format!(
                "Enter code: {CODEX_USER_CODE}\n{CODEX_VERIFICATION_URL}\n"
            )),
            "{login_err}"
        );
        assert!(!login_err.contains("Local browser launch"), "{login_err}");
        assert!(!login_err.contains("Cannot bind"), "{login_err}");
        assert!(
            login_err.contains(&format!("Discovered 1 models for {CODEX_LOGIN_NAME}.")),
            "{login_err}"
        );
        let login_requests = fixture.requests(&server, "login").await;
        assert_eq!(
            routes(&login_requests),
            [
                format!("POST {USERCODE_PATH}"),
                format!("POST {POLL_PATH}"),
                format!("POST {POLL_PATH}"),
                format!("POST {EXCHANGE_PATH}"),
                format!("GET {CODEX_MODELS_PATH}"),
            ]
        );
        let usercode = find(&login_requests, "POST", USERCODE_PATH)[0];
        assert_eq!(header(usercode, "content-type"), "application/json");
        assert_eq!(
            text(&usercode.body),
            format!(r#"{{"client_id":"{CODEX_CLIENT}"}}"#)
        );
        for poll in find(&login_requests, "POST", POLL_PATH) {
            assert_eq!(
                text(&poll.body),
                format!(
                    r#"{{"device_auth_id":"{CODEX_DEVICE_AUTH_ID}","user_code":"{CODEX_USER_CODE}"}}"#
                )
            );
        }
        let exchange = find(&login_requests, "POST", EXCHANGE_PATH)[0];
        assert_eq!(
            header(exchange, "content-type"),
            "application/x-www-form-urlencoded"
        );
        assert_eq!(
            form(exchange),
            pairs(&[
                ("grant_type", "authorization_code"),
                ("client_id", CODEX_CLIENT),
                ("code", CODEX_AUTH_CODE),
                ("code_verifier", CODEX_SERVER_VERIFIER),
                ("redirect_uri", "https://auth.openai.com/deviceauth/callback"),
            ])
        );
        let stored = rows(&fixture, CODEX).await;
        assert_eq!(stored.len(), 1);
        assert!(matches!(stored[0].origin, CredentialOrigin::Login));
        let row = oauth(&stored[0]);
        assert_eq!(row.access.expose_secret(), access);
        assert_eq!(row.refresh.expose_secret(), CODEX_REFRESH);
        assert_eq!(row.expires_at_ms, now_ms(&fixture) + 3_600_000);
        assert_eq!(row.identity.org_name.as_deref(), Some("plus"));
        std::fs::copy(
            fixture.home.path().join("data/auth.json"),
            fixture.evidence.join("auth-after-login.json"),
        )
        .unwrap();

        let methods = fixture.run("list-after-login", &["auth", "list", "--json"]).await;
        let methods_json: Value = serde_json::from_slice(&methods.stdout).unwrap();
        assert_eq!(
            provider_row(&methods_json, CODEX)["auth_methods"],
            json!(["oauth", "device_code"])
        );
        assert_eq!(provider_row(&methods_json, CODEX)["configured"], true);
        assert_eq!(
            provider_row(&methods_json, XAI_OAUTH)["auth_methods"],
            json!(["device_code"])
        );
        assert_eq!(
            provider_row(&methods_json, "xai")["auth_methods"],
            json!(["api_key"])
        );
        let status = fixture.run("status", &["auth", "status", "--json"]).await;
        let status_json: Value = serde_json::from_slice(&status.stdout).unwrap();
        assert_eq!(provider_row(&status_json, CODEX)["source"], "oauth");
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            login_requests.len(),
            "list/status never touch the network"
        );

        let ask = fixture
            .run("ask", &["ask", "hi", "--output-format", "json"])
            .await;
        let ask_json: Value = serde_json::from_slice(&ask.stdout).unwrap();
        assert_eq!(ask_json["response"], CODEX_TEXT);
        let all = fixture.requests(&server, "ask").await;
        let turn = &all[login_requests.len()..];
        let responses = find(turn, "POST", CODEX_RESPONSES_PATH);
        assert_eq!(responses.len(), 1, "one Codex request serves the device row");
        assert_no_chat_completions(&all);
        for route in [USERCODE_PATH, POLL_PATH, EXCHANGE_PATH] {
            assert!(find(turn, "POST", route).is_empty(), "fresh row: no {route}");
        }
        let request = responses[0];
        for (key, value) in [
            ("authorization", format!("Bearer {access}")),
            ("chatgpt-account-id", "cli-access-account".into()),
            ("originator", "omp".into()),
            ("user-agent", "omp/18.12.6".into()),
            ("accept", "text/event-stream".into()),
        ] {
            assert_eq!(header(request, key), value, "{key}");
        }
        let body: Value = request.body_json().unwrap();
        assert_eq!(body["model"], "gpt-5.6-luna");
        assert!(body.get("messages").is_none());
        fixture.json("codex-request-body.json", &body);

        let logout = fixture.run("logout", &["auth", "logout", CODEX]).await;
        assert_eq!(text(&logout.stdout).trim(), "Logged out of openai-codex.");
        assert!(rows(&fixture, CODEX).await.is_empty());
        let list = fixture.run("list", &["auth", "list", "--json"]).await;
        let list_json: Value = serde_json::from_slice(&list.stdout).unwrap();
        assert_eq!(provider_row(&list_json, CODEX)["configured"], false);
        let finals = fixture.requests(&server, "final").await;
        assert_eq!(finals.len(), all.len(), "logout/list make no requests");
        assert_no_leak(
            &[&ambiguous, &login, &methods, &status, &ask, &logout, &list],
            &[
                &access,
                CODEX_REFRESH,
                CODEX_AUTH_CODE,
                CODEX_SERVER_VERIFIER,
                CODEX_DEVICE_AUTH_ID,
            ],
        );
        // Written only after every runtime assertion above succeeded.
        fixture.json("passed.json", &json!({
            "test": "conformance_20_1d2_device_and_xai::cli::k05_k12_debug_binary_codex_device_login_status_ask_logout",
            "browser_callback_port_held": true,
            "no_method_exit": ambiguous.status.code(),
            "login_routes": routes(&login_requests),
            "auth_list_methods": provider_row(&methods_json, CODEX)["auth_methods"],
            "status_source": provider_row(&status_json, CODEX)["source"],
            "response": ask_json["response"], "codex_response_posts": responses.len(),
            "chat_completions_requests": 0,
            "configured_after_logout": provider_row(&list_json, CODEX)["configured"],
        }));
    })
    .await
    .expect("K05/K12 Codex device flow exceeded its structural deadline");
}

#[tokio::test]
#[serial]
async fn k12_debug_binary_xai_oauth_login_status_ask_logout() {
    // No sleeps: device waits are scaled to zero and every command is bounded.
    tokio::time::timeout(Duration::from_secs(180), async {
        let server = MockServer::start().await;
        let fixture = fixture(&server, "k12-xai-oauth", Some(XAI_CONFIG));
        preflight(&fixture, &server).await;
        let access = jwt(json!({"sub": "k12-jwt-sub"}));
        mount_xai_device(&server, &access).await;

        let login = fixture.run("login", &["auth", "login", XAI_OAUTH]).await;
        assert_eq!(
            text(&login.stdout).trim(),
            format!("Logged in to {XAI_LOGIN_NAME} as {XAI_EMAIL}")
        );
        let login_err = text(&login.stderr);
        // The verification URL is the stub's own and is shown verbatim, never fetched.
        assert!(
            login_err.contains(&format!(
                "Enter code: {XAI_USER_CODE}\n{}/activate?user_code={XAI_USER_CODE}\n",
                server.uri()
            )),
            "{login_err}"
        );
        assert!(!login_err.contains("Local browser launch"), "{login_err}");
        assert!(
            login_err.contains(&format!("Discovered 11 models for {XAI_LOGIN_NAME}.")),
            "{login_err}"
        );
        let login_requests = fixture.requests(&server, "login").await;
        assert_eq!(
            routes(&login_requests),
            [
                format!("POST {DEVICE_PATH}"),
                format!("GET {OIDC_PATH}"),
                format!("POST {TOKEN_PATH}"),
                format!("POST {TOKEN_PATH}"),
                format!("GET {USERINFO_PATH}"),
                format!("GET {XAI_MODELS_PATH}"),
            ]
        );
        let device = find(&login_requests, "POST", DEVICE_PATH)[0];
        assert_eq!(
            header(device, "content-type"),
            "application/x-www-form-urlencoded"
        );
        assert_eq!(header(device, "accept"), "application/json");
        assert_eq!(
            form(device),
            pairs(&[("client_id", XAI_CLIENT), ("scope", XAI_SCOPE)])
        );
        for poll in find(&login_requests, "POST", TOKEN_PATH) {
            assert_eq!(header(poll, "accept"), "application/json");
            assert_eq!(
                form(poll),
                pairs(&[
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ("client_id", XAI_CLIENT),
                    ("device_code", XAI_DEVICE_CODE),
                ])
            );
        }
        let bearer = format!("Bearer {access}");
        for route in [USERINFO_PATH, XAI_MODELS_PATH] {
            assert_eq!(
                header(find(&login_requests, "GET", route)[0], "authorization"),
                bearer,
                "{route}"
            );
        }
        let stored = rows(&fixture, XAI_OAUTH).await;
        assert_eq!(stored.len(), 1);
        assert!(matches!(stored[0].origin, CredentialOrigin::Login));
        let row = oauth(&stored[0]);
        let now = now_ms(&fixture);
        assert_eq!(row.access.expose_secret(), access);
        assert_eq!(row.refresh.expose_secret(), XAI_REFRESH);
        assert_eq!(row.expires_at_ms, now + 3_600_000 - 300_000);
        assert_eq!(row.authorized_at_ms, Some(now));
        assert_eq!(row.identity.email.as_deref(), Some(XAI_EMAIL));
        assert_eq!(row.identity.account_id.as_deref(), Some(XAI_USERINFO_SUB));
        std::fs::copy(
            fixture.home.path().join("data/auth.json"),
            fixture.evidence.join("auth-after-login.json"),
        )
        .unwrap();

        let status = fixture.run("status", &["auth", "status", "--json"]).await;
        let status_json: Value = serde_json::from_slice(&status.stdout).unwrap();
        assert_eq!(provider_row(&status_json, XAI_OAUTH)["source"], "oauth");

        let ask = fixture
            .run("ask", &["ask", "hi", "--output-format", "json"])
            .await;
        let ask_json: Value = serde_json::from_slice(&ask.stdout).unwrap();
        assert_eq!(ask_json["response"], XAI_TEXT);
        let all = fixture.requests(&server, "ask").await;
        let turn = &all[login_requests.len()..];
        let responses = find(turn, "POST", XAI_RESPONSES_PATH);
        assert_eq!(responses.len(), 1, "one xAI Responses request");
        assert_no_chat_completions(&all);
        assert!(find(turn, "POST", TOKEN_PATH).is_empty(), "fresh row: no refresh");
        assert!(find(turn, "GET", OIDC_PATH).is_empty(), "fresh row: no discovery");
        assert!(
            all.iter().all(|request| request.url.path() != "/activate"),
            "the verification URL is never fetched"
        );
        let request = responses[0];
        let body: Value = request.body_json().unwrap();
        let cache_key = body["prompt_cache_key"]
            .as_str()
            .expect("ask has a session, so a prompt-cache key");
        assert!(!cache_key.is_empty());
        for (key, value) in [
            ("content-type", "application/json"),
            ("accept", "text/event-stream"),
            ("x-grok-conv-id", cache_key),
            ("user-agent", "omp/18.12.6"),
            ("authorization", bearer.as_str()),
            ("x-stainless-timeout", "300"),
        ] {
            assert_eq!(header(request, key), value, "{key}");
        }
        for absent in [
            "originator",
            "openai-beta",
            "chatgpt-account-id",
            "session_id",
            "x-client-request-id",
            "x-xai-token-auth",
            "x-api-key",
        ] {
            assert!(!request.headers.contains_key(absent), "unexpected {absent}");
        }
        assert_eq!(body["model"], "grok-4.6");
        assert_eq!(body["stream"], true);
        assert_eq!(body["store"], false);
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert!(body["input"].as_array().unwrap().iter().any(|item| item["role"] == "user"
            && item["content"].as_array().is_some_and(|content| content
                .iter()
                .any(|part| part["type"] == "input_text" && part["text"] == "hi"))));
        for absent in ["messages", "client_metadata", "reasoning", "parallel_tool_calls", "tool_choice"] {
            assert!(body.get(absent).is_none(), "unexpected body field {absent}");
        }
        fixture.json("xai-request-body.json", &body);

        let logout = fixture.run("logout", &["auth", "logout", XAI_OAUTH]).await;
        assert_eq!(text(&logout.stdout).trim(), "Logged out of xai-oauth.");
        assert!(rows(&fixture, XAI_OAUTH).await.is_empty());
        let list = fixture.run("list", &["auth", "list", "--json"]).await;
        let list_json: Value = serde_json::from_slice(&list.stdout).unwrap();
        assert_eq!(provider_row(&list_json, XAI_OAUTH)["configured"], false);
        let finals = fixture.requests(&server, "final").await;
        assert_eq!(finals.len(), all.len(), "logout/list make no requests");
        assert_no_leak(
            &[&login, &status, &ask, &logout, &list],
            &[&access, XAI_REFRESH, XAI_DEVICE_CODE],
        );
        // Written only after every runtime assertion above succeeded.
        fixture.json("passed.json", &json!({
            "test": "conformance_20_1d2_device_and_xai::cli::k12_debug_binary_xai_oauth_login_status_ask_logout",
            "login_routes": routes(&login_requests),
            "discovered_line": format!("Discovered 11 models for {XAI_LOGIN_NAME}."),
            "status_source": provider_row(&status_json, XAI_OAUTH)["source"],
            "response": ask_json["response"], "xai_response_posts": responses.len(),
            "chat_completions_requests": 0,
            "configured_after_logout": provider_row(&list_json, XAI_OAUTH)["configured"],
        }));
    })
    .await
    .expect("K12 xAI device flow exceeded its structural deadline");
}
