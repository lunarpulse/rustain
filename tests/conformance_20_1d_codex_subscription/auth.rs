//! Browser subscription authentication through the real flow and persistence boundary.
#![cfg(feature = "test-instrumentation")]

use base64::Engine;
use futures::future::BoxFuture;
use rustain::adapters::auth::{
    flows::{self, FlowCtx, LoginResult, LoginUi},
    policies::CODEX_OAUTH,
};
use rustain::adapters::auth_store::FileAuthStore;
use rustain::domain::models::{AuthMethod, Credential, CredentialOrigin, SecretString};
use rustain::domain::ports::AuthStorePort;
use serial_test::serial;
use std::{ffi::OsString, sync::Arc};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const OAUTH: AuthMethod = AuthMethod::OAuthCode(&CODEX_OAUTH);
const NOW: i64 = 1_800_000_000_000;

pub(crate) fn jwt(value: serde_json::Value) -> String {
    format!(
        "e30.{}.signature",
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&value).unwrap())
    )
}

/// Restores every variable it set; reused by later subscription suites.
pub(crate) struct Env(Vec<(&'static str, Option<OsString>)>);
impl Env {
    pub(crate) fn set(values: &[(&'static str, Option<&str>)]) -> Self {
        Self(
            values
                .iter()
                .map(|(key, value)| {
                    let old = std::env::var_os(key);
                    // SAFETY: callers hold the unkeyed serial environment lock.
                    unsafe {
                        match value {
                            Some(value) => std::env::set_var(key, value),
                            None => std::env::remove_var(key),
                        }
                    }
                    (*key, old)
                })
                .collect(),
        )
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

struct Ui {
    urls: mpsc::UnboundedSender<(String, String)>,
    cancel: CancellationToken,
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
        Box::pin(async { None })
    }
    fn manual_input(&self, _: &str) -> Option<BoxFuture<'static, Option<String>>> {
        None
    }
    fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }
}
fn ui() -> (Arc<Ui>, mpsc::UnboundedReceiver<(String, String)>) {
    let (urls, receiver) = mpsc::unbounded_channel();
    (
        Arc::new(Ui {
            urls,
            cancel: CancellationToken::new(),
        }),
        receiver,
    )
}
fn store(temp: &tempfile::TempDir) -> Arc<dyn AuthStorePort> {
    Arc::new(FileAuthStore::at(temp.path().join("auth.json")))
}
fn callback(auth: &url::Url) -> url::Url {
    let pairs: std::collections::HashMap<_, _> = auth.query_pairs().into_owned().collect();
    let mut callback = url::Url::parse(&pairs["redirect_uri"]).unwrap();
    assert_eq!(callback.scheme(), "http");
    assert_eq!(callback.host_str(), Some("localhost"));
    assert_eq!(callback.path(), "/auth/callback");
    callback
        .query_pairs_mut()
        .append_pair("code", "fixture-code")
        .append_pair("state", &pairs["state"]);
    callback
}
async fn browser(
    urls: &mut mpsc::UnboundedReceiver<(String, String)>,
    server: &MockServer,
) -> url::Url {
    let (auth, launch) = urls.recv().await.unwrap();
    let auth = url::Url::parse(&auth).unwrap();
    // Check the rebased authorization origin BEFORE sending any dummy authorization code.
    assert_eq!(
        auth.origin(),
        url::Url::parse(&server.uri()).unwrap().origin()
    );
    assert!(matches!(
        auth.host_str(),
        Some("127.0.0.1" | "localhost" | "[::1]" | "::1")
    ));
    assert_eq!(auth.path(), "/oauth/authorize");
    let launch = url::Url::parse(&launch).unwrap();
    assert_eq!(launch.host_str(), Some("localhost"));
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let response = client.get(launch).send().await.unwrap();
    assert_eq!(response.status(), 302);
    assert_eq!(response.headers()["location"], auth.as_str());
    assert!(
        client
            .get(callback(&auth))
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    auth
}
async fn token_mock(server: &MockServer, response: serde_json::Value) {
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(response))
        .mount(server)
        .await;
}
/// Callers set instrumented loopback origin/port under the serial environment lock.
pub(crate) async fn login_oauth(
    server: &MockServer,
    store: &Arc<dyn AuthStorePort>,
    now: i64,
    response: serde_json::Value,
) -> LoginResult {
    token_mock(server, response).await;
    let (ui, mut urls) = ui();
    let ctx = FlowCtx::new(ui, Arc::new(move || now));
    let (result, _) = tokio::join!(
        flows::login("openai-codex", &OAUTH, &ctx, store),
        browser(&mut urls, server)
    );
    result.unwrap()
}
fn token_response(access: serde_json::Value, id: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"access_token":jwt(access),"id_token":jwt(id),"refresh_token":"FIXTURE-REFRESH","expires_in":3600})
}

#[tokio::test]
#[serial]
async fn k01_real_loopback_policy_form_pkce_and_storage() {
    use sha2::{Digest, Sha256};
    let server = MockServer::start().await;
    let _env = Env::set(&[
        ("RUSTAIN_TEST_AUTH_ORIGIN", Some(&server.uri())),
        ("RUSTAIN_TEST_AUTH_CALLBACK_PORT", None),
    ]);
    token_mock(&server, token_response(
        serde_json::json!({"https://api.openai.com/auth":{"chatgpt_account_id":"access-account","chatgpt_plan_type":"Plus "},"https://api.openai.com/profile":{"email":" User@EXAMPLE.com "}}),
        serde_json::json!({"https://api.openai.com/auth":{"chatgpt_account_id":"wrong-account","chatgpt_plan_type":"wrong"},"https://api.openai.com/profile":{"email":"wrong@example.com"}}),
    )).await;
    let temp = tempfile::tempdir().unwrap();
    let store = store(&temp);
    let (ui, mut urls) = ui();
    let ctx = FlowCtx::new(ui, Arc::new(|| NOW));
    let (result, auth) = tokio::join!(
        flows::login("openai-codex", &OAUTH, &ctx, &store),
        browser(&mut urls, &server)
    );
    let result = result.unwrap();
    assert_eq!(result.method, "oauth");
    let query: Vec<_> = auth.query_pairs().into_owned().collect();
    assert_eq!(
        query
            .iter()
            .map(|(key, _)| key.as_str())
            .collect::<Vec<_>>(),
        [
            "client_id",
            "response_type",
            "redirect_uri",
            "scope",
            "code_challenge",
            "code_challenge_method",
            "state",
            "id_token_add_organizations",
            "codex_cli_simplified_flow",
            "originator"
        ]
    );
    assert_eq!(query[0].1, "app_EMoamEEZ73f0CkXaXp7hrann");
    assert_eq!(query[1].1, "code");
    assert_eq!(query[2].1, "http://localhost:1455/auth/callback");
    assert_eq!(
        query[3].1,
        "openid profile email offline_access api.connectors.read api.connectors.invoke"
    );
    assert_eq!(query[5].1, "S256");
    assert_eq!(query[7].1, "true");
    assert_eq!(query[8].1, "true");
    assert_eq!(query[9].1, "omp");
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests.len(),
        1,
        "Codex must never call the Anthropic bootstrap"
    );
    let request = &requests[0];
    assert_eq!(request.url.path(), "/oauth/token");
    assert_eq!(
        request.headers["content-type"],
        "application/x-www-form-urlencoded"
    );
    assert!(!request.headers.contains_key("anthropic-beta"));
    let fields: Vec<_> = url::form_urlencoded::parse(&request.body)
        .into_owned()
        .collect();
    assert_eq!(
        fields
            .iter()
            .map(|(key, _)| key.as_str())
            .collect::<Vec<_>>(),
        [
            "grant_type",
            "client_id",
            "code",
            "redirect_uri",
            "code_verifier"
        ]
    );
    assert_eq!(fields[0].1, "authorization_code");
    assert_eq!(fields[1].1, query[0].1);
    assert_eq!(fields[2].1, "fixture-code");
    assert_eq!(fields[3].1, query[2].1);
    assert_eq!(
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(fields[4].1.as_bytes())),
        query[4].1
    );
    let snapshot = store.snapshot().await.unwrap();
    let rows = snapshot.rows("openai-codex");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].origin, CredentialOrigin::Login);
    assert!(rows[0].last_validated.is_some());
    let Credential::OAuth(oauth) = &rows[0].credential else {
        panic!("OAuth row required")
    };
    assert_eq!(oauth.expires_at_ms, NOW + 3_600_000);
    assert_eq!(oauth.authorized_at_ms, Some(NOW));
    assert_eq!(oauth.identity.account_id.as_deref(), Some("access-account"));
    assert_eq!(oauth.identity.email.as_deref(), Some("user@example.com"));
    assert_eq!(oauth.identity.org_id.as_deref(), Some("access-account"));
    assert_eq!(oauth.identity.org_name.as_deref(), Some("plus"));
    let disk = std::fs::read_to_string(temp.path().join("auth.json")).unwrap();
    assert!(!disk.contains("id_token"));
    assert!(!disk.contains(&fields[4].1));
    std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 1455)).unwrap();
}

#[tokio::test]
#[serial]
async fn k01_busy_real_1455_fails_before_url_or_token_exchange() {
    let server = MockServer::start().await;
    let _env = Env::set(&[
        ("RUSTAIN_TEST_AUTH_ORIGIN", Some(&server.uri())),
        ("RUSTAIN_TEST_AUTH_CALLBACK_PORT", None),
    ]);
    let _held = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 1455)).unwrap();
    let temp = tempfile::tempdir().unwrap();
    let store = store(&temp);
    let (ui, mut urls) = ui();
    let ctx = FlowCtx::new(ui, Arc::new(|| NOW));
    let error = flows::login("openai-codex", &OAUTH, &ctx, &store)
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "Cannot bind: OAuth callback port 1455 is in use and the provider accepts only this callback port. Free port 1455 and retry."
    );
    assert!(
        urls.try_recv().is_err(),
        "no fallback listener may publish a URL"
    );
    assert!(server.received_requests().await.unwrap().is_empty());
    assert!(
        store
            .snapshot()
            .await
            .unwrap()
            .rows("openai-codex")
            .is_empty()
    );
}

#[tokio::test]
#[serial]
async fn k02_missing_malformed_and_empty_access_identity_are_atomic() {
    let server = MockServer::start().await;
    let _env = Env::set(&[
        ("RUSTAIN_TEST_AUTH_ORIGIN", Some(&server.uri())),
        ("RUSTAIN_TEST_AUTH_CALLBACK_PORT", Some("0")),
    ]);
    let temp = tempfile::tempdir().unwrap();
    let store = store(&temp);
    let empty = serde_json::to_value(store.snapshot().await.unwrap()).unwrap();
    token_mock(
        &server,
        token_response(serde_json::json!({}), serde_json::json!({})),
    )
    .await;
    let (recording, mut urls) = ui();
    let ctx = FlowCtx::new(recording, Arc::new(|| NOW));
    let (result, _) = tokio::join!(
        flows::login("openai-codex", &OAUTH, &ctx, &store),
        browser(&mut urls, &server)
    );
    assert_eq!(
        result.unwrap_err().to_string(),
        "Failed to extract account identity from token"
    );
    assert_eq!(
        serde_json::to_value(store.snapshot().await.unwrap()).unwrap(),
        empty
    );
    assert!(!temp.path().join("auth.json").exists());
    server.reset().await;
    // Seed through the real login engine, then prove every failure leaves its bytes intact.
    login_oauth(
        &server,
        &store,
        NOW,
        token_response(
            serde_json::json!({"https://api.openai.com/profile":{"email":"keep@example.com"}}),
            serde_json::json!({}),
        ),
    )
    .await;
    let before = std::fs::read(temp.path().join("auth.json")).unwrap();
    for response in [
        token_response(serde_json::json!({}), serde_json::json!({})),
        serde_json::json!({"access_token":"malformed.jwt","id_token":"garbage","refresh_token":"new-refresh","expires_in":3600}),
        token_response(
            serde_json::json!({"https://api.openai.com/auth":{"chatgpt_account_id":""},"https://api.openai.com/profile":{"email":"  "}}),
            serde_json::json!({"https://api.openai.com/auth":{"chatgpt_account_id":"must-not-fallback"},"https://api.openai.com/profile":{"email":"must-not-fallback@example.com"}}),
        ),
    ] {
        server.reset().await;
        token_mock(&server, response).await;
        let (ui, mut urls) = ui();
        let ctx = FlowCtx::new(ui, Arc::new(|| NOW));
        let (result, _) = tokio::join!(
            flows::login("openai-codex", &OAUTH, &ctx, &store),
            browser(&mut urls, &server)
        );
        assert_eq!(
            result.unwrap_err().to_string(),
            "Failed to extract account identity from token"
        );
        assert_eq!(
            std::fs::read(temp.path().join("auth.json")).unwrap(),
            before
        );
        assert_eq!(
            store.snapshot().await.unwrap().rows("openai-codex").len(),
            1
        );
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "no bootstrap request"
        );
    }
}

#[tokio::test]
#[serial]
async fn jwt_identity_is_fieldwise_nullish_and_account_is_not_trimmed() {
    let server = MockServer::start().await;
    let _env = Env::set(&[
        ("RUSTAIN_TEST_AUTH_ORIGIN", Some(&server.uri())),
        ("RUSTAIN_TEST_AUTH_CALLBACK_PORT", Some("0")),
    ]);
    let temp = tempfile::tempdir().unwrap();
    let store = store(&temp);
    let result = login_oauth(&server, &store, NOW, token_response(
        serde_json::json!({"https://api.openai.com/auth":{"chatgpt_account_id":null,"chatgpt_plan_type":""},"https://api.openai.com/profile":{"email":" Access@EXAMPLE.com "}}),
        serde_json::json!({"https://api.openai.com/auth":{"chatgpt_account_id":" account with spaces ","chatgpt_plan_type":"Pro"},"https://api.openai.com/profile":{"email":"id@example.com"}}),
    )).await;
    assert_eq!(
        result.identity.account_id.as_deref(),
        Some(" account with spaces ")
    );
    assert_eq!(result.identity.email.as_deref(), Some("access@example.com"));
    assert_eq!(
        result.identity.org_id.as_deref(),
        Some(" account with spaces ")
    );
    assert_eq!(
        result.identity.org_name, None,
        "present empty plan must not fall back"
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    server.reset().await;
    let result = login_oauth(&server, &store, NOW, token_response(
        serde_json::json!({"https://api.openai.com/auth":{"chatgpt_account_id":"access-only-account"}}),
        serde_json::json!({
            "https://api.openai.com/auth": {"chatgpt_account_id":"id-account","chatgpt_plan_type":" PRO "},
            "https://api.openai.com/profile": {"email":" ID@EXAMPLE.COM "}
        }),
    )).await;
    assert_eq!(
        result.identity.account_id.as_deref(),
        Some("access-only-account")
    );
    assert_eq!(result.identity.email.as_deref(), Some("id@example.com"));
    assert_eq!(result.identity.org_name.as_deref(), Some("pro"));
}

#[tokio::test]
#[serial]
async fn refresh_form_preserves_stored_identity_and_org_through_resolver() {
    use rustain::adapters::auth::flows::refresh::HttpOAuthRefresher;
    use rustain::infrastructure::key_resolver::compose;
    for claims in [
        serde_json::json!({}),
        serde_json::json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "rotated-account",
                "chatgpt_plan_type": "Enterprise"
            },
            "https://api.openai.com/profile": {"email": " ROTATED@EXAMPLE.COM "}
        }),
    ] {
        let server = MockServer::start().await;
        let _env = Env::set(&[
            ("RUSTAIN_TEST_AUTH_ORIGIN", Some(&server.uri())),
            ("RUSTAIN_TEST_AUTH_CALLBACK_PORT", Some("0")),
            ("OPENAI_CODEX_OAUTH_TOKEN", None),
            ("RUSTAIN_TEST_NOW_MS", Some("1800000000000")),
        ]);
        let temp = tempfile::tempdir().unwrap();
        let store = store(&temp);
        login_oauth(
            &server,
            &store,
            1000,
            token_response(
                serde_json::json!({
                    "https://api.openai.com/auth": {
                        "chatgpt_account_id": "original-account",
                        "chatgpt_plan_type": "Plus"
                    },
                    "https://api.openai.com/profile": {"email": "original@example.com"}
                }),
                serde_json::json!({}),
            ),
        )
        .await;
        let before = store.snapshot().await.unwrap();
        let old_id = before.rows("openai-codex")[0].id;
        server.reset().await;
        let changed = claims.get("https://api.openai.com/auth").is_some();
        let bearer = jwt(claims);
        token_mock(
            &server,
            serde_json::json!({
                "access_token": bearer,
                "refresh_token": "ROTATED-REFRESH",
                "expires_in": 3600
            }),
        )
        .await;
        let resolver = compose(
            store.clone(),
            Arc::new(HttpOAuthRefresher::new()),
            Arc::new(|| NOW),
        );
        resolver
            .resolve("openai-codex", None, &[], &|_| None)
            .await
            .unwrap()
            .unwrap();
        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests.len(),
            1,
            "refresh must never call Anthropic bootstrap"
        );
        assert_eq!(requests[0].url.path(), "/oauth/token");
        assert_eq!(
            requests[0].headers["content-type"],
            "application/x-www-form-urlencoded"
        );
        assert!(!requests[0].headers.contains_key("anthropic-beta"));
        let form: Vec<_> = url::form_urlencoded::parse(&requests[0].body)
            .into_owned()
            .collect();
        assert_eq!(
            form,
            [
                ("grant_type".into(), "refresh_token".into()),
                ("client_id".into(), "app_EMoamEEZ73f0CkXaXp7hrann".into()),
                ("refresh_token".into(), "FIXTURE-REFRESH".into()),
            ]
        );
        let snapshot = store.snapshot().await.unwrap();
        let rows = snapshot.rows("openai-codex");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, old_id);
        let Credential::OAuth(saved) = &rows[0].credential else {
            panic!("OAuth required")
        };
        assert_eq!(saved.refresh.expose_secret(), "ROTATED-REFRESH");
        assert_eq!(saved.authorized_at_ms, Some(1000));
        assert_eq!(
            saved.identity.account_id.as_deref(),
            Some(if changed {
                "rotated-account"
            } else {
                "original-account"
            })
        );
        assert_eq!(
            saved.identity.email.as_deref(),
            Some(if changed {
                "rotated@example.com"
            } else {
                "original@example.com"
            })
        );
        assert_eq!(saved.identity.org_id.as_deref(), Some("original-account"));
        assert_eq!(saved.identity.org_name.as_deref(), Some("plus"));
        assert_eq!(saved.expires_at_ms, NOW + 3_600_000);
    }
}

#[tokio::test]
#[serial]
async fn callback_override_applies_to_every_policy_and_never_changes_fallback() {
    let server = MockServer::start().await;
    let held = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = held.local_addr().unwrap().port().to_string();
    let _env = Env::set(&[
        ("RUSTAIN_TEST_AUTH_ORIGIN", Some(&server.uri())),
        ("RUSTAIN_TEST_AUTH_CALLBACK_PORT", Some(&port)),
    ]);
    // Fallback policies take the override too, so tests never bind their fixed port;
    // each policy keeps its own fallback rule.
    for policy in [
        &rustain::adapters::auth::policies::ANTHROPIC_OAUTH,
        &rustain::adapters::auth::policies::GEMINI_CLI_OAUTH,
    ] {
        assert!(policy.port_fallback);
        assert_eq!(
            rustain::adapters::auth::policies::callback_port(policy).to_string(),
            port
        );
    }
    assert!(!CODEX_OAUTH.port_fallback);
    let temp = tempfile::tempdir().unwrap();
    let store = store(&temp);
    let (ui, mut urls) = ui();
    let ctx = FlowCtx::new(ui, Arc::new(|| NOW));
    let error = flows::login("openai-codex", &OAUTH, &ctx, &store)
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        format!(
            "Cannot bind: OAuth callback port {port} is in use and the provider accepts only this callback port. Free port {port} and retry."
        )
    );
    assert!(urls.try_recv().is_err());
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
#[serial]
async fn login_storage_scopes_same_email_by_codex_account() {
    let server = MockServer::start().await;
    let _env = Env::set(&[
        ("RUSTAIN_TEST_AUTH_ORIGIN", Some(&server.uri())),
        ("RUSTAIN_TEST_AUTH_CALLBACK_PORT", Some("0")),
    ]);
    let temp = tempfile::tempdir().unwrap();
    let store = store(&temp);
    for account in ["personal-account", "team-account"] {
        server.reset().await;
        login_oauth(
            &server,
            &store,
            NOW,
            token_response(
                serde_json::json!({
                    "https://api.openai.com/auth": {"chatgpt_account_id": account},
                    "https://api.openai.com/profile": {"email": "same@example.com"}
                }),
                serde_json::json!({}),
            ),
        )
        .await;
    }
    let snapshot = store.snapshot().await.unwrap();
    let rows = snapshot.rows("openai-codex");
    assert_eq!(rows.len(), 2);
    assert_ne!(rows[0].id, rows[1].id);
    let accounts: std::collections::BTreeSet<_> = rows
        .iter()
        .map(|row| {
            let Credential::OAuth(oauth) = &row.credential else {
                panic!("OAuth required")
            };
            oauth.identity.org_id.as_deref().unwrap()
        })
        .collect();
    assert_eq!(
        accounts,
        std::collections::BTreeSet::from(["personal-account", "team-account"])
    );
}
