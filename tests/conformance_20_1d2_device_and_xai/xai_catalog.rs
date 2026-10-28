//! xAI live discovery and curation (K10) and the credential gate (K11), through
//! `init_provider_layer` + `refresh_target` / `rediscover_after_auth_change`. Stored
//! `xai-oauth` rows come from a real `flows::login` against the loopback stub;
//! the official API origin is the same stub (`RUSTAIN_TEST_AUTH_ORIGIN`), so
//! discovery and inference reach `<stub>/v1/...`.

use super::device::{
    OIDC_PATH, PROVIDER as OAUTH, Recorder, TOKEN_ENDPOINT, TOKEN_PATH, USERINFO_PATH, json_reply,
    login_xai_oauth, received, unreachable_origin, xai_access,
};
use crate::conformance_20_1d_codex_subscription::auth::jwt;
use crate::conformance_20_1d1b_codex_replay_discovery::common::{
    GUARD, World, completed, sse, text,
};
use futures::StreamExt;
use rustain::adapters::auth_store::FileAuthStore;
use rustain::adapters::model_catalog_cache::{
    CachedDiscoveryEntry, DiscoveryTarget, ModelCatalogCache,
};
use rustain::domain::errors::ProviderError;
use rustain::domain::models::provider::{DiscoveryStatus, LockedProvider};
use rustain::domain::models::{
    AppConfig, AuthSource, CompletionOptions, Message, MessageRole, ModelCapability,
    ModelDescriptor, ProviderConfig, StreamChunk,
};
use rustain::domain::ports::AuthStorePort;
use rustain::infrastructure::model_discovery::{
    RefreshReport, rediscover_after_auth_change, refresh_target,
};
use rustain::infrastructure::provider_availability::{
    Availability, availability, effective_providers,
};
use rustain::infrastructure::provider_factory::build_provider_for_config;
use rustain::infrastructure::startup::{ProviderLayer, init_provider_layer};
use rustain::infrastructure::utils::env_var_trimmed;
use serde_json::{Value, json};
use serial_test::serial;
use std::collections::HashSet;
use std::ffi::OsString;
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{header, method, path},
};

const KEY: &str = "xai";
const MODELS: &str = "/v1/models";
const RESPONSES: &str = "/v1/responses";
const DEFAULT_MODEL: &str = "grok-4.6";
/// Image, video, speech and voice ids xAI lists next to its chat models.
const MEDIA: [&str; 6] = [
    "grok-imagine-image",
    "grok-imagine-image-quality",
    "grok-imagine-video-1",
    "grok-stt-1",
    "grok-tts",
    "grok-voice-1",
];
const OAUTH_NAME: &str = "xAI Grok OAuth (SuperGrok or X Premium+)";
const KEY_HINT: &str = "Set XAI_API_KEY or run `rustain auth login xai`";
const OAUTH_HINT: &str = "Set XAI_OAUTH_TOKEN or run `rustain auth login xai-oauth`";
const XAI_ENV: [&str; 3] = ["XAI_API_KEY", "XAI_OAUTH_TOKEN", "XAI_BASE_URL"];

// ── fixtures ────────────────────────────────────────────────────────────────

/// `World` does not own the xAI variables: cleared for the test, restored on drop.
struct XaiEnv(Vec<(&'static str, Option<OsString>)>);

impl XaiEnv {
    fn new() -> Self {
        let saved = XAI_ENV.iter().map(|k| (*k, std::env::var_os(k))).collect();
        for k in XAI_ENV {
            // SAFETY: every environment-mutating test holds the shared serial lock.
            unsafe { std::env::remove_var(k) }
        }
        Self(saved)
    }
}

impl Drop for XaiEnv {
    fn drop(&mut self) {
        for (k, v) in self.0.drain(..) {
            // SAFETY: the serial guard outlives this environment guard.
            unsafe {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }
}

/// No configured providers: both xAI kinds are bundled entries.
fn app_config() -> AppConfig {
    AppConfig {
        model: DEFAULT_MODEL.into(),
        ..Default::default()
    }
}

/// A `[provider.<kind>]` entry with `provider_id = kind = <kind>`.
fn provider_config(kind: &str, base_url: Option<String>) -> ProviderConfig {
    ProviderConfig {
        provider_id: kind.into(),
        kind: Some(kind.into()),
        model_id: DEFAULT_MODEL.into(),
        api_key_env: if kind == KEY {
            "XAI_API_KEY"
        } else {
            "XAI_OAUTH_TOKEN"
        }
        .into(),
        auth_header: None,
        enabled: true,
        base_url: base_url.map(Into::into),
        context_window: None,
        supports_tools: None,
        discover_models: true,
        model_filter: vec!["*".into()],
        cache_ttl_seconds: 3600,
    }
}

fn configured(kind: &str, base_url: String) -> AppConfig {
    let mut cfg = app_config();
    cfg.provider
        .insert(kind.into(), provider_config(kind, Some(base_url)));
    cfg
}

/// Userinfo naming `account`, at login and at every refresh.
fn identity(account: &str) -> ResponseTemplate {
    json_reply(
        200,
        json!({"sub": account, "email": format!("{account}@example.test")}),
    )
}

/// A real device login for `account`; the stub's login history is cleared after it.
async fn login(server: &MockServer, account: &str) -> String {
    let access = xai_access(account);
    login_xai_oauth(
        server,
        &access,
        &format!("REFRESH-{account}"),
        identity(account),
    )
    .await;
    server.reset().await;
    access
}

/// The refresh a stored row's `401` triggers: OIDC discovery, the token endpoint
/// minting a new access token for the same account, then userinfo.
async fn mount_refresh(server: &MockServer, account: &str) -> String {
    let rotated = jwt(json!({"sub": account, "iss": "https://auth.x.ai", "rotation": 1}));
    reply(
        server,
        "GET",
        OIDC_PATH,
        json_reply(
            200,
            json!({"issuer": "https://auth.x.ai", "token_endpoint": TOKEN_ENDPOINT}),
        ),
    )
    .await;
    reply(
        server,
        "POST",
        TOKEN_PATH,
        json_reply(
            200,
            json!({"access_token": rotated, "refresh_token": format!("REFRESH-{account}-next"), "expires_in": 3600, "token_type": "Bearer"}),
        ),
    )
    .await;
    reply(server, "GET", USERINFO_PATH, identity(account)).await;
    rotated
}

async fn reply(server: &MockServer, verb: &str, route: &str, template: ResponseTemplate) {
    Mock::given(method(verb))
        .and(path(route))
        .respond_with(template)
        .mount(server)
        .await;
}

async fn roster(server: &MockServer, status: u16, body: Value) {
    reply(server, "GET", MODELS, json_reply(status, body)).await;
}

fn target<'a>(layer: &'a ProviderLayer, id: &str) -> &'a DiscoveryTarget {
    layer
        .discovery_targets
        .iter()
        .find(|t| t.provider_id == id)
        .unwrap_or_else(|| panic!("{id} is not a live discovery target"))
}

fn locked(layer: &ProviderLayer, id: &str) -> LockedProvider {
    layer
        .registry
        .list_locked()
        .into_iter()
        .find(|p| p.provider_id == id)
        .unwrap_or_else(|| panic!("{id} is not locked"))
}

fn served(layer: &ProviderLayer, id: &str) -> Vec<ModelDescriptor> {
    layer.registry.list_models_by_provider(id)
}

fn ids(models: &[ModelDescriptor]) -> Vec<String> {
    models.iter().map(|m| m.model_id.clone()).collect()
}

fn assert_no_media(ids: &[String]) {
    for id in ids {
        assert!(!MEDIA.contains(&id.as_str()), "media id {id} served");
    }
}

async fn refresh(layer: &ProviderLayer, id: &str) -> RefreshReport {
    refresh_target(
        target(layer, id),
        &ModelCatalogCache::new(),
        Some(&layer.registry),
        true,
    )
    .await
}

async fn disk(id: &str) -> CachedDiscoveryEntry {
    ModelCatalogCache::new()
        .load()
        .await
        .providers
        .remove(id)
        .unwrap_or_else(|| panic!("no cache entry for {id}"))
}

fn bearer(request: &Request) -> &str {
    request.headers["authorization"].to_str().unwrap()
}

fn rejected(display: &str, hint: &str) -> String {
    format!(
        "{display} rejected its credential (HTTP 401/403) — models could not be discovered. {hint}"
    )
}

fn user(content: &str) -> Message {
    Message {
        role: MessageRole::User,
        content: content.into(),
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
        session_id: None,
        model: DEFAULT_MODEL.into(),
        max_tokens: 1024,
        system_prompt: String::new(),
        temperature: None,
        tools: vec![],
    }
}

/// `Ok` as `"ok"`, an error by its variant, so one table row states both ports.
fn port(result: Result<(), ProviderError>) -> String {
    match result {
        Ok(()) => "ok".into(),
        Err(error) => format!("{error:?}"),
    }
}

// ── K10: live discovery ─────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn k10_login_rediscovery_serves_seed_order_then_new_live_ids_sorted_never_media() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let _xai = XaiEnv::new();
    let cfg = app_config();
    let before = FileAuthStore::new().snapshot().await.unwrap();
    let access = login(&server, "k10-account").await;
    // The bundled catalogue the signed entry serves before any live roster.
    let seeded = served(&init_provider_layer(&cfg), OAUTH);
    let seeded_ids = ids(&seeded);
    assert!(seeded_ids.iter().any(|id| id == DEFAULT_MODEL));
    roster(
        &server,
        200,
        json!({"data": [
            {"id": "grok-9", "name": "Grok 9"},
            {"id": "grok-imagine-video-1", "name": "Video"},
            {"id": DEFAULT_MODEL, "name": "Live Grok 4.6", "context_length": 1},
            {"id": "grok-stt-1"},
            {"id": "grok-10-preview", "context_length": "256000"},
            {"id": "grok-tts"},
            {"id": "grok-imagine-image"},
            {"id": "grok-imagine-image-quality"},
            {"id": "grok-voice-1"},
        ]}),
    )
    .await;

    let report = rediscover_after_auth_change(OAUTH, &cfg, &before)
        .await
        .expect("a new xai-oauth row is rediscovered after login");
    assert_eq!(report.status, DiscoveryStatus::Ok);
    assert_eq!(report.notice, None);
    let gets = received(&server, MODELS).await;
    assert_eq!(gets.len(), 1);
    assert_eq!(bearer(&gets[0]), format!("Bearer {access}"));
    assert_eq!(
        gets[0].headers["accept"].to_str().unwrap(),
        "application/json"
    );

    // A restart serves the stored roster under the startup scope, without a request.
    let layer = init_provider_layer(&cfg);
    let target = target(&layer, OAUTH);
    assert_eq!(target.display_name, OAUTH_NAME);
    let models = served(&layer, OAUTH);
    let served_ids = ids(&models);
    let mut expected = seeded_ids.clone();
    expected.extend(["grok-10-preview".to_owned(), "grok-9".to_owned()]);
    assert_eq!(
        served_ids, expected,
        "seed order first, then the new live ids sorted by id"
    );
    assert_eq!(report.model_count, served_ids.len());
    assert_no_media(&served_ids);
    let by_id = |id: &str| models.iter().find(|m| m.model_id == id).unwrap();
    let seeded_default = seeded.iter().find(|m| m.model_id == DEFAULT_MODEL).unwrap();
    assert_eq!(
        by_id(DEFAULT_MODEL).display_name,
        seeded_default.display_name
    );
    assert_eq!(
        by_id(DEFAULT_MODEL).context_window,
        seeded_default.context_window
    );
    assert_eq!(
        by_id(DEFAULT_MODEL).capabilities,
        seeded_default.capabilities
    );
    assert_eq!(by_id("grok-9").display_name, "Grok 9");
    assert_eq!(by_id("grok-9").context_window, 128_000);
    assert_eq!(
        by_id("grok-9").capabilities,
        HashSet::from([ModelCapability::ToolUse])
    );
    assert_eq!(by_id("grok-10-preview").display_name, "grok-10-preview");
    assert_eq!(by_id("grok-10-preview").context_window, 256_000);

    let cached = disk(OAUTH).await;
    assert_eq!(cached.credential_fingerprint, target.credential_fingerprint);
    assert_no_media(
        &cached
            .models
            .iter()
            .map(|m| m.model_id.clone())
            .collect::<Vec<_>>(),
    );
    assert_eq!(received(&server, MODELS).await.len(), 1);
}

#[tokio::test]
#[serial]
async fn k10_stored_row_401_is_recovered_once_and_the_rotated_roster_keeps_the_account_scope() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let _xai = XaiEnv::new();
    let first = login(&server, "k10-rotation").await;
    let rotated = mount_refresh(&server, "k10-rotation").await;
    Recorder::new()
        .mount(
            &server,
            "GET",
            MODELS,
            vec![
                json_reply(401, json!({"error": "unauthorized"})),
                json_reply(200, json!({"data": [{"id": "grok-after-refresh"}]})),
            ],
        )
        .await;
    let layer = init_provider_layer(&app_config());
    let report = refresh(&layer, OAUTH).await;
    assert_eq!(report.status, DiscoveryStatus::Ok);
    assert!(ids(&served(&layer, OAUTH)).contains(&"grok-after-refresh".to_owned()));
    let gets = received(&server, MODELS).await;
    assert_eq!(gets.len(), 2, "one recovery resend");
    assert_eq!(bearer(&gets[0]), format!("Bearer {first}"));
    assert_eq!(bearer(&gets[1]), format!("Bearer {rotated}"));
    assert_eq!(received(&server, TOKEN_PATH).await.len(), 1);
    // The roster is stamped with the account the resend used, which is the
    // account the startup scope predicted.
    assert_eq!(
        disk(OAUTH).await.credential_fingerprint,
        target(&layer, OAUTH).credential_fingerprint
    );
}

#[tokio::test]
#[serial]
async fn k10_remaining_401_after_recovery_is_unauthenticated_with_the_rejected_notice() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let _xai = XaiEnv::new();
    let first = login(&server, "k10-rejected").await;
    let rotated = mount_refresh(&server, "k10-rejected").await;
    roster(&server, 401, json!({"error": "unauthorized"})).await;
    let layer = init_provider_layer(&app_config());
    let seeded = ids(&served(&layer, OAUTH));
    let report = refresh(&layer, OAUTH).await;
    assert_eq!(report.status, DiscoveryStatus::Unauthenticated);
    assert_eq!(
        layer.registry.discovery_status(OAUTH),
        Some(DiscoveryStatus::Unauthenticated)
    );
    assert_eq!(
        report.notice.as_deref(),
        Some(rejected(OAUTH_NAME, OAUTH_HINT).as_str())
    );
    let gets = received(&server, MODELS).await;
    assert_eq!(gets.len(), 2, "exactly one recovery");
    assert_eq!(bearer(&gets[0]), format!("Bearer {first}"));
    assert_eq!(bearer(&gets[1]), format!("Bearer {rotated}"));
    assert_eq!(received(&server, TOKEN_PATH).await.len(), 1);
    let models = served(&layer, OAUTH);
    assert_eq!(ids(&models), seeded, "the bundled catalogue stays");
    assert!(models.iter().all(|m| m.stale), "marked as rejected");
}

#[tokio::test]
#[serial]
async fn k10_env_token_401_is_final_without_refresh() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    let _xai = XaiEnv::new();
    world.set("XAI_OAUTH_TOKEN", &xai_access("k10-env"));
    roster(&server, 401, json!({"error": "unauthorized"})).await;
    let layer = init_provider_layer(&app_config());
    let report = refresh(&layer, OAUTH).await;
    assert_eq!(report.status, DiscoveryStatus::Unauthenticated);
    assert_eq!(
        report.notice.as_deref(),
        Some(rejected(OAUTH_NAME, OAUTH_HINT).as_str())
    );
    assert_eq!(received(&server, MODELS).await.len(), 1);
    assert!(received(&server, OIDC_PATH).await.is_empty());
    assert!(received(&server, TOKEN_PATH).await.is_empty());
}

#[tokio::test]
#[serial]
async fn k10_discovery_scope_is_the_key_each_request_sent() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    let _xai = XaiEnv::new();
    for (key, live, media) in [
        ("xai-key-a", "grok-scope-a", "grok-imagine-image"),
        ("xai-key-b", "grok-scope-b", "grok-voice-1"),
    ] {
        Mock::given(method("GET"))
            .and(path(MODELS))
            .and(header("authorization", format!("Bearer {key}").as_str()))
            .respond_with(json_reply(
                200,
                json!({"data": [{"id": media}, {"id": live}]}),
            ))
            .mount(&server)
            .await;
    }
    let cfg = app_config();
    let has = |layer: &ProviderLayer, id: &str| ids(&served(layer, KEY)).contains(&id.to_owned());

    world.set("XAI_API_KEY", "xai-key-a");
    let first = init_provider_layer(&cfg);
    assert_eq!(refresh(&first, KEY).await.status, DiscoveryStatus::Ok);
    assert!(has(&first, "grok-scope-a"));
    assert_no_media(&ids(&served(&first, KEY)));
    let scope_a = disk(KEY).await.credential_fingerprint;
    assert_eq!(scope_a, target(&first, KEY).credential_fingerprint);
    // A restart with the same key serves that roster without a request.
    assert!(has(&init_provider_layer(&cfg), "grok-scope-a"));
    assert_eq!(received(&server, MODELS).await.len(), 1);

    // The key changes under a running target: the refresh sends the new key,
    // serves its roster only and stamps the new key's scope.
    world.set("XAI_API_KEY", "xai-key-b");
    assert_eq!(refresh(&first, KEY).await.status, DiscoveryStatus::Ok);
    let gets = received(&server, MODELS).await;
    assert_eq!(gets.len(), 2);
    assert_eq!(bearer(&gets[1]), "Bearer xai-key-b");
    assert!(has(&first, "grok-scope-b"));
    assert!(!has(&first, "grok-scope-a"));
    assert_no_media(&ids(&served(&first, KEY)));
    let scope_b = disk(KEY).await.credential_fingerprint;
    assert_ne!(scope_b, scope_a);
    let restarted = init_provider_layer(&cfg);
    assert_eq!(target(&restarted, KEY).credential_fingerprint, scope_b);
    assert!(has(&restarted, "grok-scope-b"));
    assert!(!has(&restarted, "grok-scope-a"));
}

/// Discovery, `health_check` and `connectivity_probe` against one `/v1/models`
/// status each; an environment key is never refreshed, so every port sends once.
#[tokio::test]
#[serial]
async fn k10_discovery_health_and_probe_map_models_statuses() {
    let refresh_failed = |detail: &str| {
        format!("Model catalog refresh for 'xai' failed: {detail} — showing bundled/cached models")
    };
    let rows: [(u16, Value, DiscoveryStatus, Option<String>, &str); 7] = [
        (
            200,
            json!({"data": [{"id": DEFAULT_MODEL}]}),
            DiscoveryStatus::Ok,
            None,
            "ok",
        ),
        (
            200,
            json!({"object": "list"}),
            DiscoveryStatus::Unavailable,
            Some(refresh_failed("Could not parse xAI models response")),
            "ok",
        ),
        (
            401,
            json!({}),
            DiscoveryStatus::Unauthenticated,
            Some(rejected("xAI", KEY_HINT)),
            "AuthenticationFailed",
        ),
        (
            403,
            json!({}),
            DiscoveryStatus::Unauthenticated,
            Some(rejected("xAI", KEY_HINT)),
            "AuthenticationFailed",
        ),
        (
            404,
            json!({}),
            DiscoveryStatus::Unavailable,
            None,
            "EndpointUnsupported(404)",
        ),
        (
            405,
            json!({}),
            DiscoveryStatus::Unavailable,
            None,
            "EndpointUnsupported(405)",
        ),
        (
            500,
            json!({}),
            DiscoveryStatus::Unavailable,
            Some(refresh_failed("xAI models HTTP 500")),
            "Other(\"xAI models HTTP 500\")",
        ),
    ];
    for (status, body, discovery, notice, ports) in rows {
        // A fresh store per row: no earlier roster turns a failure into `Cached`.
        let server = MockServer::start().await;
        let world = World::new(&server);
        let _xai = XaiEnv::new();
        world.set("XAI_API_KEY", "xai-status-key");
        roster(&server, status, body).await;
        let layer = init_provider_layer(&app_config());
        let report = refresh(&layer, KEY).await;
        assert_eq!(report.status, discovery, "{status}");
        assert_eq!(report.notice, notice, "{status}");
        let provider = layer.router.get_provider(KEY).expect("signed by the key");
        assert_eq!(
            port(provider.health_check().await),
            ports,
            "health {status}"
        );
        assert_eq!(
            port(provider.connectivity_probe().await.map(|_| ())),
            ports,
            "probe {status}"
        );
        let gets = received(&server, MODELS).await;
        assert_eq!(
            gets.len(),
            3,
            "discovery, health and probe once each: {status}"
        );
        for get in &gets {
            assert_eq!(bearer(get), "Bearer xai-status-key");
            assert_eq!(get.headers["accept"].to_str().unwrap(), "application/json");
        }
    }
}

#[tokio::test]
#[serial]
async fn k10_unreachable_xai_base_is_offline_for_health_probe_and_discovery() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    let _xai = XaiEnv::new();
    world.set("XAI_API_KEY", "xai-offline-key");
    let layer = init_provider_layer(&configured(KEY, format!("{}/v1", unreachable_origin())));
    let provider = layer.router.get_provider(KEY).expect("signed by the key");
    let health = provider.health_check().await.unwrap_err();
    assert!(matches!(health, ProviderError::Offline(_)), "{health:?}");
    assert_eq!(
        provider.connectivity_probe().await.unwrap_err().to_string(),
        "Offline: xAI probe transport failed"
    );
    let report = refresh(&layer, KEY).await;
    assert_eq!(report.status, DiscoveryStatus::Unavailable);
    let notice = report.notice.expect("a transport failure is reported");
    assert!(
        notice.starts_with("Model catalog refresh for 'xai' failed: Offline: "),
        "{notice}"
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
#[serial]
async fn k10_probe_recovers_a_stored_row_401_once_and_health_uses_the_rotated_row() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let _xai = XaiEnv::new();
    let first = login(&server, "k10-probe").await;
    let rotated = mount_refresh(&server, "k10-probe").await;
    Recorder::new()
        .mount(
            &server,
            "GET",
            MODELS,
            vec![
                json_reply(401, json!({"error": "unauthorized"})),
                json_reply(200, json!({"data": []})),
            ],
        )
        .await;
    let layer = init_provider_layer(&app_config());
    let provider = layer.router.get_provider(OAUTH).expect("signed by the row");
    assert_eq!(port(provider.connectivity_probe().await.map(|_| ())), "ok");
    assert_eq!(port(provider.health_check().await), "ok");
    let gets = received(&server, MODELS).await;
    assert_eq!(
        gets.iter().map(bearer).collect::<Vec<_>>(),
        [
            format!("Bearer {first}"),
            format!("Bearer {rotated}"),
            format!("Bearer {rotated}")
        ]
    );
    assert_eq!(received(&server, TOKEN_PATH).await.len(), 1);
}

#[cfg(feature = "models-dev")]
#[tokio::test]
#[serial]
async fn xai_catalogues_are_never_extended_from_models_dev() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    let _xai = XaiEnv::new();
    world.set("RUSTAIN_MODELS_DEV_URL", &server.uri());
    let row = json!({"models": {"models-dev-only": {"tool_call": true}}});
    reply(
        &server,
        "GET",
        "/api.json",
        json_reply(200, json!({"xai": row, "xai-oauth": row, "openai": row})),
    )
    .await;
    rustain::adapters::models_dev::refresh().await.unwrap();
    let layer = init_provider_layer(&app_config());
    let has = |id: &str| ids(&locked(&layer, id).models).contains(&"models-dev-only".to_owned());
    assert!(has("openai"), "control: a mapped provider takes the row");
    assert!(!has(KEY));
    assert!(!has(OAUTH));
}

// ── K11: credential gate ────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn k11_without_credentials_both_kinds_are_locked_with_login_hints_and_default_to_grok_4_6() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    let _xai = XaiEnv::new();
    let cfg = app_config();
    for id in [KEY, OAUTH] {
        let entry = effective_providers(&cfg)
            .into_iter()
            .find(|entry| entry.id == id)
            .unwrap_or_else(|| panic!("{id} is a bundled provider"));
        assert_eq!(entry.cfg.model_id, DEFAULT_MODEL, "{id} default model");
    }
    let layer = init_provider_layer(&cfg);
    for (id, name, hint) in [(KEY, "xAI", KEY_HINT), (OAUTH, OAUTH_NAME, OAUTH_HINT)] {
        let locked = locked(&layer, id);
        assert_eq!(locked.display_name, name);
        assert_eq!(locked.sign_in_hint, hint);
        let models = ids(&locked.models);
        assert!(
            models.iter().any(|m| m == DEFAULT_MODEL),
            "{id}: {models:?}"
        );
        assert_no_media(&models);
        assert!(layer.router.get_provider(id).is_none());
        assert!(layer.discovery_targets.iter().all(|t| t.provider_id != id));
    }

    // A key signs `xai`, which serves the catalogue it advertised while locked.
    let advertised = ids(&locked(&layer, KEY).models);
    world.set("XAI_API_KEY", "xai-gate-key");
    let signed = init_provider_layer(&cfg);
    assert!(
        signed
            .registry
            .list_locked()
            .iter()
            .all(|p| p.provider_id != KEY)
    );
    assert_eq!(ids(&served(&signed, KEY)), advertised);
    assert_eq!(locked(&signed, OAUTH).sign_in_hint, OAUTH_HINT);
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
#[serial]
async fn k11_stored_xai_oauth_row_is_locked_for_a_configured_foreign_origin_and_never_sent() {
    let server = MockServer::start().await;
    let foreign = MockServer::start().await;
    let _world = World::new(&server);
    let _xai = XaiEnv::new();
    login(&server, "k11-account").await;
    // Control: on the official origin the same row signs the bundled entry.
    let official = init_provider_layer(&app_config());
    target(&official, OAUTH);

    let cfg = configured(OAUTH, format!("{}/v1", foreign.uri()));
    let layer = init_provider_layer(&cfg);
    assert_eq!(locked(&layer, OAUTH).sign_in_hint, OAUTH_HINT);
    assert!(layer.router.get_provider(OAUTH).is_none());
    assert!(
        layer
            .discovery_targets
            .iter()
            .all(|t| t.provider_id != OAUTH)
    );
    assert!(
        layer
            .catalogue_targets
            .iter()
            .all(|t| t.provider_id != OAUTH)
    );
    let entry = effective_providers(&cfg)
        .into_iter()
        .find(|entry| entry.id == OAUTH)
        .unwrap();
    let snapshot = FileAuthStore::new().snapshot().await.unwrap();
    assert!(matches!(
        availability(&entry, &snapshot, &env_var_trimmed),
        Availability::Locked { .. }
    ));
    assert!(matches!(
        build_provider_for_config(OAUTH, &cfg.provider[OAUTH]).err(),
        Some(ProviderError::AuthenticationFailed)
    ));
    assert!(foreign.received_requests().await.unwrap().is_empty());
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
#[serial]
async fn k11_configured_xai_base_with_an_env_key_is_signed_and_receives_that_key() {
    let server = MockServer::start().await;
    let foreign = MockServer::start().await;
    let world = World::new(&server);
    let _xai = XaiEnv::new();
    world.set("XAI_API_KEY", "xai-k11-key");
    reply(
        &foreign,
        "POST",
        RESPONSES,
        sse(&[text("k11 signed"), completed()]),
    )
    .await;
    let cfg = configured(KEY, format!("{}/v1", foreign.uri()));
    let entry = effective_providers(&cfg)
        .into_iter()
        .find(|entry| entry.id == KEY)
        .unwrap();
    let snapshot = FileAuthStore::new().snapshot().await.unwrap();
    assert!(matches!(
        availability(&entry, &snapshot, &env_var_trimmed),
        Availability::Signed(AuthSource::Env { var }) if var == "XAI_API_KEY"
    ));
    let layer = init_provider_layer(&cfg);
    assert!(
        layer
            .registry
            .list_locked()
            .iter()
            .all(|p| p.provider_id != KEY)
    );
    let provider = layer.router.get_provider(KEY).expect("signed by the key");
    let chunks: Vec<StreamChunk> = tokio::time::timeout(GUARD, async {
        provider
            .stream_completion(vec![user("hi")], options())
            .await
            .expect("the configured base answers")
            .collect::<Vec<_>>()
            .await
    })
    .await
    .expect("stream finished");
    assert!(
        chunks
            .iter()
            .any(|c| matches!(c, StreamChunk::Text { content, .. } if content == "k11 signed")),
        "{chunks:?}"
    );
    let posts = received(&foreign, RESPONSES).await;
    assert_eq!(posts.len(), 1);
    assert_eq!(bearer(&posts[0]), "Bearer xai-k11-key");
    assert!(server.received_requests().await.unwrap().is_empty());
}
