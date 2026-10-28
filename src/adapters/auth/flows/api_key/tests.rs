//! Existing login decision oracles, migrated to the shared production front door.
use crate::adapters::{
    auth::flows::{self, FlowCtx, FlowError, LoginUi},
    auth_store::FileAuthStore,
};
use crate::domain::{
    models::{AuthMethod, Credential, CredentialOrigin, ProviderConfig, SecretString},
    ports::AuthStorePort,
};
use futures::future::BoxFuture;
use std::{collections::VecDeque, sync::Arc};
use tokio::sync::Mutex;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

struct Ui(Mutex<VecDeque<String>>);
impl LoginUi for Ui {
    fn on_auth_url(&self, _: &str, _: &str, _: &str) {
        panic!("API-key login cannot launch OAuth")
    }
    fn on_device_code(&self, _: &str, _: &str, _: &str) {
        panic!("API-key login cannot show a device code")
    }
    fn on_progress(&self, _: &str) {}
    fn prompt_secret(&self, _: &str) -> BoxFuture<'_, Option<SecretString>> {
        Box::pin(async move { self.0.lock().await.pop_front().map(SecretString::new) })
    }
    fn manual_input(&self, _: &str) -> Option<BoxFuture<'static, Option<String>>> {
        None
    }
    fn prompt_line(&self, _: &str) -> BoxFuture<'_, Option<String>> {
        panic!("API-key login never prompts for a plain line")
    }
    fn cancel_token(&self) -> tokio_util::sync::CancellationToken {
        tokio_util::sync::CancellationToken::new()
    }
}
fn context(origin: &str, input: &[&str]) -> FlowCtx {
    let config:ProviderConfig=serde_json::from_value(serde_json::json!({"provider_id":"anthropic","kind":"anthropic","model_id":"test","api_key_env":"unused","base_url":origin})).unwrap();
    FlowCtx::new(
        Arc::new(Ui(Mutex::new(input.iter().map(|s| (*s).into()).collect()))),
        Arc::new(|| 0),
    )
    .with_validation_config(config)
}
async fn run(
    status: u16,
    input: &[&str],
    existing: bool,
) -> (
    Result<flows::LoginResult, FlowError>,
    crate::domain::models::AuthSnapshot,
) {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(status))
        .mount(&server)
        .await;
    let directory = tempfile::tempdir().unwrap();
    let store: Arc<dyn AuthStorePort> =
        Arc::new(FileAuthStore::at(directory.path().join("auth.json")));
    if existing {
        store
            .upsert(
                "anthropic",
                Credential::new_api_key("ORIGINAL-KEY".into()),
                CredentialOrigin::Stored,
                true,
            )
            .await
            .unwrap();
    }
    let result = flows::login(
        "anthropic",
        &AuthMethod::ApiKey,
        &context(&server.uri(), input),
        &store,
    )
    .await;
    (result, store.snapshot().await.unwrap())
}
#[tokio::test]
async fn p10_valid_key_stored_invalid_key_not() {
    let (result, snapshot) = run(200, &["VALID-CANDIDATE"], false).await;
    assert!(result.unwrap().validated);
    assert_eq!(snapshot.rows("anthropic").len(), 1);
    assert_eq!(
        snapshot.rows("anthropic")[0].credential.expose_api_key(),
        Some("VALID-CANDIDATE")
    );
    let (result, snapshot) = run(401, &["INVALID-CANDIDATE"], false).await;
    assert!(result.is_err());
    assert!(snapshot.rows("anthropic").is_empty());
}
#[tokio::test]
async fn p12_invalid_rejection_includes_signup_url() {
    let (result, snapshot) = run(401, &["INVALID-CANDIDATE"], false).await;
    let message = result.unwrap_err().to_string();
    assert!(message.contains("https://console.anthropic.com/"));
    assert!(!message.contains("INVALID-CANDIDATE"));
    assert!(snapshot.rows("anthropic").is_empty());
}
#[tokio::test]
async fn p13_offline_stores_nothing() {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let directory = tempfile::tempdir().unwrap();
    let store: Arc<dyn AuthStorePort> =
        Arc::new(FileAuthStore::at(directory.path().join("auth.json")));
    let result = flows::login(
        "anthropic",
        &AuthMethod::ApiKey,
        &context(&origin, &["OFFLINE-CANDIDATE"]),
        &store,
    )
    .await;
    assert!(result.is_err());
    assert!(store.snapshot().await.unwrap().rows("anthropic").is_empty());
}
#[tokio::test]
async fn p3_inconclusive_stored_unvalidated() {
    let (result, snapshot) = run(404, &["UNCONFIRMED-CANDIDATE"], false).await;
    assert!(!result.unwrap().validated);
    assert_eq!(snapshot.rows("anthropic").len(), 1);
    assert!(snapshot.rows("anthropic")[0].last_validated.is_none());
}
#[tokio::test]
async fn p14_decline_stores_nothing_accept_stores() {
    let (result, snapshot) = run(200, &["n"], true).await;
    assert!(matches!(result, Err(FlowError::Declined)));
    assert_eq!(
        snapshot.rows("anthropic")[0].credential.expose_api_key(),
        Some("ORIGINAL-KEY")
    );
    assert_eq!(snapshot.rows("anthropic")[0].rev, 1);
    let (result, snapshot) = run(200, &["yes", "REPLACEMENT-CANDIDATE"], true).await;
    assert!(result.unwrap().validated);
    assert_eq!(
        snapshot.rows("anthropic")[0].credential.expose_api_key(),
        Some("REPLACEMENT-CANDIDATE")
    );
    assert_eq!(snapshot.rows("anthropic")[0].rev, 2);
}
#[tokio::test]
async fn p11_success_output_never_echoes_key() {
    let (result, _) = run(200, &["SECRET-CANARY-DEADBEEF"], false).await;
    let result = result.unwrap();
    assert!(!format!("{result:?}").contains("SECRET-CANARY-DEADBEEF"));
    let json = serde_json::json!({"provider":result.provider,"method":result.method,"validated":result.validated,"identity":result.identity});
    assert!(!json.to_string().contains("SECRET-CANARY-DEADBEEF"));
}
struct PatientUi(Mutex<Option<tokio::sync::oneshot::Receiver<String>>>);
impl LoginUi for PatientUi {
    fn on_auth_url(&self, _: &str, _: &str, _: &str) {
        panic!("API-key login cannot launch OAuth")
    }
    fn on_device_code(&self, _: &str, _: &str, _: &str) {
        panic!("API-key login cannot show a device code")
    }
    fn on_progress(&self, _: &str) {}
    fn prompt_secret(&self, _: &str) -> BoxFuture<'_, Option<SecretString>> {
        Box::pin(async move {
            let key = self.0.lock().await.take()?;
            key.await.ok().map(SecretString::new)
        })
    }
    fn manual_input(&self, _: &str) -> Option<BoxFuture<'static, Option<String>>> {
        None
    }
    fn prompt_line(&self, _: &str) -> BoxFuture<'_, Option<String>> {
        panic!("API-key login never prompts for a plain line")
    }
    fn cancel_token(&self) -> tokio_util::sync::CancellationToken {
        tokio_util::sync::CancellationToken::new()
    }
}
#[tokio::test]
async fn masked_key_entry_has_no_login_deadline() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let directory = tempfile::tempdir().unwrap();
    let store: Arc<dyn AuthStorePort> =
        Arc::new(FileAuthStore::at(directory.path().join("auth.json")));
    let (send, receive) = tokio::sync::oneshot::channel();
    let config = context(&server.uri(), &[]).validation_config.unwrap();
    let context = FlowCtx::new(
        Arc::new(PatientUi(Mutex::new(Some(receive)))),
        Arc::new(|| 0),
    )
    .with_validation_config(config);
    let mut login = Box::pin(flows::login(
        "anthropic",
        &AuthMethod::ApiKey,
        &context,
        &store,
    ));
    tokio::select! {
        biased;
        result = &mut login => panic!("login finished before key entry: {result:?}"),
        () = tokio::task::yield_now() => {}
    }
    // The operator takes longer than the OAuth deadline at the masked prompt.
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(301)).await;
    tokio::time::resume();
    send.send("PATIENT-KEY".into()).unwrap();
    assert!(login.await.unwrap().validated);
}
