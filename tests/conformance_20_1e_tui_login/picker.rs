//! K07 (AC5): the `/login` provider picker's provenance labels, async
//! validation through the adapter's credential handle, search over name, id,
//! source and env variable, and the generation guard.

use super::common::{Tui, World};
use rustain::adapters::auth_store::FileAuthStore;
use rustain::adapters::tui::state::Validation;
use rustain::domain::events::{DomainKey, LoginEvent};
use rustain::domain::models::{AppConfig, Credential, CredentialOrigin};
use rustain::domain::ports::AuthStorePort;
use serde_json::json;
use serial_test::serial;
use std::sync::Arc;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_partial_json, method, path},
};

/// A TUI key login of `provider`: method `method_downs` steps down the
/// picker (0 when the provider has a single method), then a pasted key.
async fn key_login(tui: &mut Tui, provider: &str, method_downs: usize, key: &str) {
    tui.submit(&format!("/login {provider}"));
    for _ in 0..method_downs {
        tui.key(DomainKey::Down);
    }
    if method_downs > 0 {
        tui.key(DomainKey::Enter);
    }
    tui.until("masked key field", Tui::field_pending).await;
    tui.paste(key);
    tui.key(DomainKey::Enter);
    tui.until_login("login finished", |event| {
        matches!(event, LoginEvent::Finished { .. })
    })
    .await;
    tui.drain();
}

/// An Anthropic browser login through the overlay whose token is already due.
async fn due_anthropic_login(tui: &mut Tui) {
    tui.submit("/login anthropic");
    tui.key(DomainKey::Enter);
    tui.until("paste field", Tui::field_pending).await;
    let running = tui.state.login.running.as_ref().unwrap();
    let rustain::adapters::tui::state::LoginSection::AuthUrl { url, .. } = &running.sections[0]
    else {
        panic!("browser link first");
    };
    let auth = url::Url::parse(url).unwrap();
    let query: std::collections::HashMap<_, _> = auth.query_pairs().into_owned().collect();
    let mut cb = url::Url::parse(&query["redirect_uri"]).unwrap();
    cb.query_pairs_mut()
        .append_pair("code", "k07-code")
        .append_pair("state", &query["state"]);
    tui.paste(cb.as_str());
    tui.key(DomainKey::Enter);
    tui.until_login("login finished", |event| {
        matches!(event, LoginEvent::Finished { .. })
    })
    .await;
    tui.drain();
    assert!(
        tui.feedback()
            .starts_with("Logged in to Anthropic (Claude Pro/Max)"),
        "{}",
        tui.feedback()
    );
}

fn row_line(frame: &str, display: &str) -> String {
    frame
        .lines()
        .find(|line| line.contains(&format!("{display} · ")))
        .unwrap_or_else(|| panic!("{display} row\n{frame}"))
        .to_owned()
}

/// K07 (M07): each served credential is labelled by its source (OAuth row,
/// login key, env over a lower-ranked stored key), validated, counted, and
/// searchable; a stale picker generation never changes the reopened one.
#[tokio::test]
#[serial]
async fn k07_provider_picker_provenance_validation_search_and_generation() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    Mock::given(method("POST"))
        .and(path("/v1/oauth/token"))
        .and(body_partial_json(
            json!({"grant_type": "authorization_code"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "K07-ACCESS", "refresh_token": "K07-REFRESH", "expires_in": 3600,
            "account": {"uuid": "acct-7", "email_address": "k07@example.test"},
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/oauth/token"))
        .and(body_partial_json(json!({"grant_type": "refresh_token"})))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": "invalid_grant"})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
        .mount(&server)
        .await;
    let app = AppConfig::default();
    let mut tui = Tui::new(app.clone());

    due_anthropic_login(&mut tui).await;
    key_login(&mut tui, "openrouter", 1, "sk-or-k07-first").await;
    key_login(&mut tui, "openrouter", 1, "sk-or-k07-second").await;
    let store: Arc<dyn AuthStorePort> = Arc::new(FileAuthStore::new());
    let first = store.snapshot().await.unwrap().rows("openrouter")[0].id;
    rustain::infrastructure::auth_accounts::select_account(&store, &app, "openrouter", first)
        .await
        .unwrap();
    store
        .upsert(
            "openai",
            Credential::new_api_key("sk-legacy-stored".into()),
            CredentialOrigin::Stored,
            false,
        )
        .await
        .unwrap();
    world.set("OPENAI_API_KEY", "sk-env-openai");

    // The Anthropic grant is due by now; its refresh is rejected (400).
    world.set(
        "RUSTAIN_TEST_NOW_MS",
        &(super::common::NOW + 7_200_000).to_string(),
    );
    tui.submit("/login");
    tui.until("validated rows", |tui| {
        !tui.state.login.loading
            && tui
                .state
                .login
                .rows
                .iter()
                .all(|row| row.row.source.is_none() || row.validation != Validation::Checking)
    })
    .await;
    let frame = tui.frame();
    assert!(
        row_line(&frame, "Anthropic").contains("Anthropic · ✘ invalid · oauth"),
        "{frame}"
    );
    assert!(
        row_line(&frame, "OpenRouter").contains("OpenRouter · ● logged in · login · 2 accounts"),
        "{frame}"
    );
    assert!(
        row_line(&frame, "OpenAI").contains("OpenAI · ● logged in · env: OPENAI_API_KEY"),
        "{frame}"
    );
    assert!(
        row_line(&frame, "DeepSeek").contains("DeepSeek · not signed in"),
        "{frame}"
    );

    // Search matches the env variable in the description.
    tui.type_text("OPENAI_API");
    assert_eq!(
        tui.state
            .login
            .selected_row()
            .map(|row| row.row.id.as_str()),
        Some("openai")
    );

    // A picker closed before its data arrived: its rows never fill the
    // reopened one.
    tui.key(DomainKey::Esc);
    tui.key(DomainKey::Esc);
    tui.submit("/login");
    tui.key(DomainKey::Esc);
    tui.submit("/login");
    tui.until_login("stale rows", |event| {
        matches!(event, LoginEvent::Targets { .. })
    })
    .await;
    assert!(tui.state.login.loading, "a stale generation is ignored");
    tui.until("current rows", |tui| !tui.state.login.loading)
        .await;
    assert!(!tui.state.login.rows.is_empty());
}

/// The picker dispatches independent validation children after publishing rows.
#[tokio::test]
#[serial]
async fn k3_picker_validations_run_independently_after_targets() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    Mock::given(method("POST"))
        .and(path("/v1/oauth/token"))
        .and(body_partial_json(
            json!({"grant_type": "authorization_code"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "K3-ACCESS", "refresh_token": "K3-REFRESH", "expires_in": 3600,
            "account": {"uuid": "acct-k3", "email_address": "k3@example.test"},
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
        .mount(&server)
        .await;
    let mut tui = Tui::new(AppConfig::default());
    due_anthropic_login(&mut tui).await;
    world.set(
        "RUSTAIN_TEST_NOW_MS",
        &(super::common::NOW + 7_200_000).to_string(),
    );
    world.set("OPENAI_API_KEY", "sk-k3-openai");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    world.set(
        "RUSTAIN_TEST_AUTH_ORIGIN",
        &format!("http://{}", listener.local_addr().unwrap()),
    );
    tui.event_log.clear();
    tui.submit("/login");
    let held = super::common::accept(&listener).await;
    assert_eq!(held.path(), "/v1/oauth/token");
    tui.until_login(
        "openai validation while anthropic is held",
        |event| matches!(event, LoginEvent::Validated { provider, .. } if provider == "openai"),
    )
    .await;
    assert!(
        tui.event_log
            .iter()
            .any(|event| event.contains("Targets {"))
    );
    assert!(
        !tui.event_log
            .iter()
            .any(|event| event.contains("Validated {") && event.contains("anthropic"))
    );
    held.json(
        &json!({"access_token": "K3-NEW", "refresh_token": "K3-NEW-REFRESH", "expires_in": 3600}),
    )
    .await;
    tui.until_login(
        "anthropic validation after release",
        |event| matches!(event, LoginEvent::Validated { provider, .. } if provider == "anthropic"),
    )
    .await;
    let targets = tui
        .event_log
        .iter()
        .position(|event| event.contains("Targets {"))
        .unwrap();
    let openai = tui
        .event_log
        .iter()
        .position(|event| event.contains("Validated {") && event.contains("openai"))
        .unwrap();
    let anthropic = tui
        .event_log
        .iter()
        .position(|event| event.contains("Validated {") && event.contains("anthropic"))
        .unwrap();
    assert!(targets < openai && targets < anthropic);
}
