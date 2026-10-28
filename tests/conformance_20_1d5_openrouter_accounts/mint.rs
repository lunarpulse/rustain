//! K01–K03 (AC1–AC3): OpenRouter browser key minting and key validation through
//! the real `flows::login` with the production `OPENROUTER_KEY_MINT` / `ApiKey`
//! policies, the real loopback listener and the file store. OpenRouter is a
//! wiremock (or raw TCP) stub reached through `RUSTAIN_TEST_AUTH_ORIGIN`.

use crate::conformance_20_1d_codex_subscription::boundaries::{
    assert_no_connection, raw_listener, read_request,
};
use crate::conformance_20_1d1b_codex_replay_discovery::common::{NOW, World};
use crate::conformance_20_1d3_copilot::chat_d4::KeyUi;
use crate::conformance_20_1d3_copilot::common::requests;
use crate::conformance_20_1d4_gemini_cli::auth::{
    Seen, no_redirect_client, query, ui as recording_ui,
};
use base64::Engine;
use parking_lot::Mutex;
use rustain::adapters::auth::flows::{self, FlowCtx, FlowError, LoginResult, LoginUi};
use rustain::adapters::auth::policies::OPENROUTER_KEY_MINT;
use rustain::adapters::auth_store::FileAuthStore;
use rustain::domain::models::{AuthMethod, CredentialOrigin, ProviderConfig, StoredCredential};
use rustain::domain::ports::AuthStorePort;
use serde_json::json;
use serial_test::serial;
use sha2::Digest;
use std::sync::Arc;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

pub(crate) const PROVIDER: &str = "openrouter";
pub(crate) const MINT: AuthMethod = AuthMethod::PkceKey(&OPENROUTER_KEY_MINT);
pub(crate) const KEYS: &str = "/api/v1/auth/keys";
pub(crate) const AUTH_KEY: &str = "/api/v1/auth/key";
pub(crate) const MODELS: &str = "/api/v1/models";
pub(crate) const CODE: &str = "OPENROUTER-CODE-CANARY";
const MINTED: &str = "sk-or-v1-minted-canary";
const PASTED: &str = "sk-or-v1-pasted-canary";
const INVALID: &str = "Invalid API key for OpenRouter";

pub(crate) fn store() -> Arc<dyn AuthStorePort> {
    Arc::new(FileAuthStore::new())
}

pub(crate) async fn rows() -> Vec<StoredCredential> {
    store().snapshot().await.unwrap().rows(PROVIDER).to_vec()
}

/// The bytes of `auth.json`, absent when no write happened.
fn snapshot(world: &World) -> Option<Vec<u8>> {
    std::fs::read(world.dir.path().join("auth.json")).ok()
}

/// The exchange answers `body`; `/auth/key` and `/models` answer 200 to any
/// bearer, so only the request counts tell which endpoint was consulted.
pub(crate) async fn mount_openrouter(server: &MockServer, exchange: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path(KEYS))
        .respond_with(exchange)
        .mount(server)
        .await;
    for route in [AUTH_KEY, MODELS] {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
            .mount(server)
            .await;
    }
}

pub(crate) fn minted(key: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({"key": key, "user_id": "user-1"}))
}

fn mint_ctx(ui: Arc<dyn LoginUi>) -> FlowCtx {
    FlowCtx::new(ui, Arc::new(|| NOW))
}

/// The authorization URL the flow published.
async fn authorize(seen: &mut Seen) -> url::Url {
    let (authorize, _) = seen.urls.recv().await.unwrap();
    url::Url::parse(&authorize).unwrap()
}

fn param(url: &url::Url, key: &str) -> String {
    query(url)
        .into_iter()
        .find(|(k, _)| k == key)
        .unwrap_or_else(|| panic!("no {key} in {url}"))
        .1
}

/// Completes the browser step: `GET <callback_url>?<query>` on the advertised
/// redirect, whatever port the flow bound.
pub(crate) async fn complete_callback(authorize: &url::Url, query: &str) -> u16 {
    let callback = format!("{}?{query}", param(authorize, "callback_url"));
    no_redirect_client()
        .get(callback)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

/// A full browser mint against the stub: the published URL, then the stateless
/// callback carrying `CODE`.
pub(crate) async fn mint_login() -> Result<LoginResult, FlowError> {
    let (ui, mut seen) = recording_ui(&[]);
    let ctx = mint_ctx(ui);
    let store = store();
    let browser = async {
        let authorize = authorize(&mut seen).await;
        assert_eq!(
            complete_callback(&authorize, &format!("code={CODE}")).await,
            200
        );
    };
    tokio::join!(flows::login(PROVIDER, &MINT, &ctx, &store), browser).0
}

fn sha256_challenge(verifier: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier))
}

#[tokio::test]
#[serial]
async fn k01_browser_mint_sends_omps_bytes_and_stores_one_validated_key_row() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    // The requested port is held, so the flow must fall back and advertise
    // the port it actually bound.
    let held = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let held_port = held.local_addr().unwrap().port();
    world.set("RUSTAIN_TEST_AUTH_CALLBACK_PORT", &held_port.to_string());
    mount_openrouter(&server, minted(MINTED)).await;

    let (ui, mut seen) = recording_ui(&[]);
    let ctx = mint_ctx(ui);
    let store = store();
    let browser = async {
        let authorize = authorize(&mut seen).await;
        // Exactly omp's three query parameters, in order: no client, scope or state.
        assert_eq!(
            authorize.origin(),
            url::Url::parse(&server.uri()).unwrap().origin()
        );
        assert_eq!(authorize.path(), "/auth");
        let pairs = query(&authorize);
        let keys: Vec<&str> = pairs.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            ["callback_url", "code_challenge", "code_challenge_method"]
        );
        assert_eq!(param(&authorize, "code_challenge_method"), "S256");
        let callback = url::Url::parse(&param(&authorize, "callback_url")).unwrap();
        assert_eq!(callback.host_str(), Some("localhost"));
        assert_eq!(callback.path(), "/callback");
        let bound = callback.port().unwrap();
        assert_ne!(bound, held_port, "the held port is never advertised");
        // A stateless callback is accepted.
        assert_eq!(
            complete_callback(&authorize, &format!("code={CODE}")).await,
            200
        );
        (param(&authorize, "code_challenge"), bound)
    };
    let (result, (challenge, bound)) =
        tokio::join!(flows::login(PROVIDER, &MINT, &ctx, &store), browser);
    let result = result.expect("the mint succeeds");
    assert_eq!(result.method, "browser_key");
    assert!(result.validated);
    assert!(seen.progress().contains(&format!(
        "Preferred port {held_port} unavailable, using port {bound}"
    )));

    // One exchange with exactly the three JSON fields; the verifier matches the
    // challenge; the engine sets no credential or attribution header.
    let exchanges = requests(&server, "POST", KEYS).await;
    assert_eq!(exchanges.len(), 1);
    let exchange = &exchanges[0];
    let body = std::str::from_utf8(&exchange.body).unwrap();
    let verifier: serde_json::Value = serde_json::from_str(body).unwrap();
    let verifier = verifier["code_verifier"].as_str().unwrap().to_owned();
    assert_eq!(
        body,
        format!(
            r#"{{"code":"{CODE}","code_verifier":"{verifier}","code_challenge_method":"S256"}}"#
        )
    );
    assert_eq!(verifier.len(), 128);
    assert_eq!(sha256_challenge(&verifier), challenge);
    assert_eq!(
        exchange.headers.get("content-type").unwrap(),
        "application/json"
    );
    for header in [
        "authorization",
        "user-agent",
        "http-referer",
        "x-openrouter-title",
    ] {
        assert!(exchange.headers.get(header).is_none(), "{header}");
    }
    // The exchange answer is the key: no `/auth/key` (omp parity) or `/models` call.
    assert!(requests(&server, "GET", AUTH_KEY).await.is_empty());
    assert!(requests(&server, "GET", MODELS).await.is_empty());

    // One API-key row, origin login, validated, active.
    let rows = rows().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].credential.expose_api_key(), Some(MINTED));
    assert_eq!(rows[0].origin, CredentialOrigin::Login);
    assert!(rows[0].last_validated.is_some());
    assert_eq!(rows[0].id, result.credential_id);
    assert_eq!(
        store.snapshot().await.unwrap().selected(PROVIDER),
        Some(result.credential_id)
    );
    drop(held);
}

/// A login that must fail with `expected` and write nothing: each case starts
/// from an empty data dir, so the store file must still be absent.
async fn assert_refused(world: &World, result: Result<LoginResult, FlowError>, expected: &str) {
    let error = result.expect_err("the login fails");
    assert_eq!(error.to_string(), expected);
    assert_eq!(snapshot(world), None, "{expected}: nothing is written");
    assert!(rows().await.is_empty());
}

#[tokio::test]
#[serial]
async fn k02_every_mint_failure_has_its_exact_text_and_stores_nothing() {
    for (answer, expected) in [
        (
            ResponseTemplate::new(400).set_body_string("echo sk-or-v1-leaked"),
            "OpenRouter key exchange failed (HTTP 400)",
        ),
        (
            ResponseTemplate::new(500).set_body_string("upstream down"),
            "OpenRouter key exchange failed (HTTP 500)",
        ),
        (
            ResponseTemplate::new(200).set_body_string("not json"),
            "OpenRouter key exchange returned no key",
        ),
        (
            ResponseTemplate::new(200).set_body_json(json!({"user_id": "u"})),
            "OpenRouter key exchange returned no key",
        ),
        (
            ResponseTemplate::new(200).set_body_json(json!({"key": ""})),
            "OpenRouter key exchange returned no key",
        ),
    ] {
        let server = MockServer::start().await;
        let world = World::new(&server);
        mount_openrouter(&server, answer).await;
        let result = mint_login().await;
        assert_refused(&world, result, expected).await;
        assert_eq!(requests(&server, "POST", KEYS).await.len(), 1, "{expected}");
        assert!(requests(&server, "GET", AUTH_KEY).await.is_empty());
    }
}

#[tokio::test]
#[serial]
async fn k02_a_denied_authorization_ends_the_login_without_an_exchange() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    mount_openrouter(&server, minted(MINTED)).await;
    let (ui, mut seen) = recording_ui(&[]);
    let ctx = mint_ctx(ui);
    let store = store();
    let browser = async {
        let authorize = authorize(&mut seen).await;
        // No state was sent, so an `error` without one is a denial.
        assert_eq!(
            complete_callback(&authorize, "error=access_denied").await,
            400
        );
    };
    let (result, ()) = tokio::join!(flows::login(PROVIDER, &MINT, &ctx, &store), browser);
    assert_refused(&world, result, "OAuth authorization was denied").await;
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
#[serial]
async fn k02_an_exchange_transport_failure_is_reported_without_details() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    let (listener, origin) = raw_listener().await;
    world.set("RUSTAIN_TEST_AUTH_ORIGIN", &origin);
    let (ui, mut seen) = recording_ui(&[]);
    let ctx = mint_ctx(ui);
    let store = store();
    let browser = async {
        let authorize = authorize(&mut seen).await;
        assert_eq!(
            complete_callback(&authorize, &format!("code={CODE}")).await,
            200
        );
        // The exchange arrives and the connection closes without an answer.
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_request(&mut socket).await;
        assert_eq!(
            (request.method.as_str(), request.target.as_str()),
            ("POST", KEYS)
        );
        drop(socket);
    };
    let (result, ()) = tokio::join!(flows::login(PROVIDER, &MINT, &ctx, &store), browser);
    assert_refused(
        &world,
        result,
        "OpenRouter key exchange failed: transport unavailable",
    )
    .await;
}

#[tokio::test]
#[serial]
async fn k02_cancellation_during_the_wait_or_the_exchange_stops_every_request() {
    // During the browser wait: nothing is ever sent.
    let server = MockServer::start().await;
    let world = World::new(&server);
    mount_openrouter(&server, minted(MINTED)).await;
    let (ui, mut seen) = recording_ui(&[]);
    let cancel = ui.cancel.clone();
    let ctx = mint_ctx(ui);
    let store = store();
    let browser = async {
        authorize(&mut seen).await;
        cancel.cancel();
    };
    let (result, ()) = tokio::join!(flows::login(PROVIDER, &MINT, &ctx, &store), browser);
    assert_refused(&world, result, "Login cancelled").await;
    assert!(server.received_requests().await.unwrap().is_empty());
    drop(world);

    // During the exchange: the held request is abandoned and no further one follows.
    let world = World::new(&server);
    let (listener, origin) = raw_listener().await;
    world.set("RUSTAIN_TEST_AUTH_ORIGIN", &origin);
    let (ui, mut seen) = recording_ui(&[]);
    let cancel = ui.cancel.clone();
    let ctx = mint_ctx(ui);
    let browser = async {
        let authorize = authorize(&mut seen).await;
        assert_eq!(
            complete_callback(&authorize, &format!("code={CODE}")).await,
            200
        );
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_request(&mut socket).await;
        assert_eq!(request.target, KEYS);
        cancel.cancel();
        socket
    };
    let (result, _held) = tokio::join!(flows::login(PROVIDER, &MINT, &ctx, &store), browser);
    assert_refused(&world, result, "Login cancelled").await;
    assert_no_connection(&listener);
}

/// The real API-key engine with `key` and `cfg` as its validation entry.
async fn api_key_login(cfg: Option<&ProviderConfig>, key: &str) -> Result<LoginResult, FlowError> {
    let ui = Arc::new(KeyUi {
        key: key.into(),
        prompts: Mutex::new(Vec::new()),
    });
    let mut ctx = FlowCtx::new(ui, Arc::new(|| NOW));
    if let Some(cfg) = cfg {
        ctx = ctx.with_validation_config(cfg.clone());
    }
    flows::login(PROVIDER, &AuthMethod::ApiKey, &ctx, &store()).await
}

/// `key` pasted into the browser flow's prompt instead of finishing the callback.
async fn pasted_login(key: &str) -> Result<LoginResult, FlowError> {
    let (ui, _seen) = recording_ui(&[key]);
    flows::login(PROVIDER, &MINT, &mint_ctx(ui), &store()).await
}

async fn mount_auth_key(server: &MockServer, status: u16) {
    Mock::given(method("GET"))
        .and(path(AUTH_KEY))
        .respond_with(ResponseTemplate::new(status).set_body_json(json!({"data": {}})))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(MODELS))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
        .mount(server)
        .await;
}

#[tokio::test]
#[serial]
async fn k03_both_paste_paths_validate_at_auth_key_and_never_at_models() {
    for paste in [true, false] {
        for (status, outcome) in [
            (200, Ok(())),
            (
                401,
                Err(format!(
                    "✗ {INVALID}.\n  Get one at: https://openrouter.ai/keys"
                )),
            ),
            (
                404,
                Err("✗ Validation error. Key was NOT stored.".to_owned()),
            ),
        ] {
            let server = MockServer::start().await;
            let world = World::new(&server);
            mount_auth_key(&server, status).await;
            let key = if paste {
                PASTED
            } else {
                "sk-or-v1-typed-canary"
            };
            let result = if paste {
                pasted_login(key).await
            } else {
                api_key_login(None, key).await
            };
            let checks = requests(&server, "GET", AUTH_KEY).await;
            assert_eq!(checks.len(), 1, "paste={paste} status={status}");
            assert_eq!(
                checks[0].headers.get("authorization").unwrap(),
                &format!("Bearer {key}")
            );
            assert!(requests(&server, "GET", MODELS).await.is_empty());
            assert!(requests(&server, "POST", KEYS).await.is_empty());
            match outcome {
                Ok(()) => {
                    let result = result.expect("a valid key is stored");
                    assert!(result.validated);
                    assert_eq!(result.method, if paste { "browser_key" } else { "api_key" });
                    let rows = rows().await;
                    assert_eq!(rows.len(), 1);
                    assert_eq!(rows[0].credential.expose_api_key(), Some(key));
                    assert!(rows[0].last_validated.is_some());
                }
                Err(expected) => assert_refused(&world, result, &expected).await,
            }
        }
    }
}

#[tokio::test]
#[serial]
async fn k03_an_unreachable_auth_key_endpoint_stores_nothing() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    let closed = {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.local_addr().unwrap().port()
    };
    let cfg: ProviderConfig = serde_json::from_value(json!({
        "provider_id": PROVIDER, "kind": PROVIDER, "model_id": "openai/gpt-5.5",
        "api_key_env": "OPENROUTER_API_KEY",
        "base_url": format!("http://127.0.0.1:{closed}/api/v1"),
    }))
    .unwrap();
    let result = api_key_login(Some(&cfg), PASTED).await;
    assert_refused(
        &world,
        result,
        "✗ Cannot validate — offline. Key was NOT stored.",
    )
    .await;
}

#[tokio::test]
#[serial]
async fn k01_the_launch_page_redirects_to_the_pending_authorization_url() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    mount_openrouter(&server, minted(MINTED)).await;
    let (ui, mut seen) = recording_ui(&[]);
    let ctx = mint_ctx(ui);
    let store = store();
    let browser = async {
        let (authorize, launch) = seen.urls.recv().await.unwrap();
        let response = no_redirect_client().get(&launch).send().await.unwrap();
        assert_eq!(response.status(), 302);
        assert_eq!(response.headers()["location"], authorize.as_str());
        let authorize = url::Url::parse(&authorize).unwrap();
        assert_eq!(
            complete_callback(&authorize, &format!("code={CODE}")).await,
            200
        );
    };
    let (result, ()) = tokio::join!(flows::login(PROVIDER, &MINT, &ctx, &store), browser);
    assert!(result.unwrap().validated);
}
