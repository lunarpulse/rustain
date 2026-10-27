//! K04-K06: live Codex discovery through 20-1c's `refresh_target`, request
//! headers, credential-bound cache scope and overlapping-refresh ordering.
use super::common::*;
use crate::conformance_20_1d_codex_subscription::auth::jwt;
use rustain::adapters::auth_store::FileAuthStore;
use rustain::adapters::model_catalog_cache::{DiscoveryTarget, ModelCatalogCache};
use rustain::domain::events::AppEvent;
use rustain::domain::models::AppConfig;
use rustain::domain::models::provider::DiscoveryStatus;
use rustain::domain::ports::AuthStorePort;
use rustain::infrastructure::model_discovery::{refresh_and_announce, refresh_target, remerge_all};
use rustain::infrastructure::runtime::event_bus::EventBus;
use rustain::infrastructure::startup::{ProviderLayer, init_provider_layer};
use serde_json::{Value, json};
use serial_test::serial;
use std::sync::Arc;
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{method, path},
};

const CODEX_MODELS: &str = "/backend-api/codex/models";
const ROOT_MODELS: &str = "/backend-api/models";
const TOKEN: &str = "/oauth/token";

fn target<'a>(layer: &'a ProviderLayer, id: &str) -> &'a DiscoveryTarget {
    layer
        .discovery_targets
        .iter()
        .find(|t| t.provider_id == id)
        .unwrap_or_else(|| panic!("{id} is not a live discovery target"))
}

fn served(layer: &ProviderLayer, id: &str) -> Vec<String> {
    layer
        .registry
        .list_models_by_provider(id)
        .into_iter()
        .map(|m| m.model_id)
        .collect()
}

async fn roster(server: &MockServer, route: &str, status: u16, body: Value) {
    Mock::given(method("GET"))
        .and(path(route))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(server)
        .await;
}

async fn requests(server: &MockServer, route: &str) -> Vec<Request> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path() == route)
        .collect()
}

async fn disk(id: &str) -> rustain::adapters::model_catalog_cache::CachedDiscoveryEntry {
    ModelCatalogCache::new()
        .load()
        .await
        .providers
        .remove(id)
        .unwrap_or_else(|| panic!("no cache entry for {id}"))
}

#[tokio::test]
#[serial]
async fn k04_k05_fallback_names_contexts_hidden_worker_and_discovery_headers() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    let token = login(&server, "k04-account").await;
    roster(&server, CODEX_MODELS, 404, json!({})).await;
    roster(
        &server,
        ROOT_MODELS,
        200,
        json!({"models":[
            {"slug":"gpt-5.6-mini","priority":5},
            {"slug":"gpt-5.6-luna","display_name":"GPT-5.6-Luna","context_window":272000,"priority":1},
            {"slug":"hidden-model","visibility":"hidden","priority":0},
            {"slug":"gpt-6.1-sol-wm","display_name":"GPT-6.1-Sol Worker","priority":3},
            {"slug":"brand-new","display_name":"Brand New","priority":2}
        ]}),
    )
    .await;
    let cfg = app_config();
    let layer = init_provider_layer(&cfg);
    assert!(
        layer
            .catalogue_targets
            .iter()
            .any(|t| t.provider_id == "openai-codex")
    );
    let seeded = layer.registry.list_models_by_provider("openai-codex");
    let report = refresh_target(
        target(&layer, "openai-codex"),
        &ModelCatalogCache::new(),
        Some(&layer.registry),
        true,
    )
    .await;
    assert_eq!(report.status, DiscoveryStatus::Ok);
    assert_eq!(report.model_count, 5);
    let models = layer.registry.list_models_by_provider("openai-codex");
    let ids: Vec<_> = models.iter().map(|m| m.model_id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "gpt-5.6-luna",
            "brand-new",
            "gpt-6.1-sol",
            "gpt-6.1-sol-wm",
            "gpt-5.6-mini"
        ],
        "live priority order, hidden excluded, safe -wm plain id synthesised"
    );
    let by_id = |id: &str| models.iter().find(|m| m.model_id == id).unwrap();
    let seeded_name = |id: &str| {
        seeded
            .iter()
            .find(|m| m.model_id == id)
            .map(|m| m.display_name.clone())
            .unwrap()
    };
    assert_eq!(
        by_id("gpt-5.6-luna").display_name,
        seeded_name("gpt-5.6-luna")
    );
    assert_ne!(by_id("gpt-5.6-luna").display_name, "GPT-5.6-Luna");
    assert_eq!(
        by_id("gpt-6.1-sol").display_name,
        seeded_name("gpt-6.1-sol")
    );
    assert_eq!(by_id("brand-new").display_name, "Brand New");
    assert_eq!(by_id("gpt-6.1-sol-wm").display_name, "GPT-6.1-Sol Worker");
    assert_eq!(by_id("gpt-5.6-luna").context_window, 1_000_000);
    assert_eq!(by_id("gpt-5.6-mini").context_window, 372_000);
    assert_eq!(by_id("brand-new").context_window, 272_000);

    // K05: both paths carry omp's discovery header set, bound to the bearer.
    let gets: Vec<_> = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.method == "GET" && r.url.path().ends_with("/models"))
        .collect();
    assert_eq!(
        gets.iter().map(|r| r.url.path()).collect::<Vec<_>>(),
        [CODEX_MODELS, ROOT_MODELS]
    );
    for request in &gets {
        assert_eq!(request.url.query(), Some("client_version=0.159.0"));
        for (key, value) in [
            ("authorization", format!("Bearer {token}")),
            ("chatgpt-account-id", "k04-account".into()),
            ("openai-beta", "responses=experimental".into()),
            ("originator", "omp".into()),
            ("version", "0.159.0".into()),
            ("accept", "application/json".into()),
        ] {
            assert_eq!(
                request.headers.get(key).unwrap().to_str().unwrap(),
                value,
                "{key}"
            );
        }
        assert!(
            !request
                .headers
                .contains_key("x-openai-internal-codex-residency")
        );
        assert!(
            request
                .headers
                .get("user-agent")
                .is_none_or(|ua| !ua.to_str().unwrap().starts_with("omp/"))
        );
    }

    // Plain test: a models.dev remerge keeps the scoped live roster.
    world.set("RUSTAIN_MODELS_DEV_URL", &server.uri());
    Mock::given(method("GET"))
        .and(path("/api.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"openai-codex":{"models":{"models-dev-only":{"tool_call":true}}}}),
        ))
        .mount(&server)
        .await;
    rustain::adapters::models_dev::refresh().await.unwrap();
    let (bus, _rx) = EventBus::new(64);
    remerge_all(
        &layer.registry,
        &layer.catalogue_targets,
        &Arc::new(bus),
        &cfg,
    )
    .await;
    assert_eq!(served(&layer, "openai-codex"), ids);
}

#[tokio::test]
#[serial]
async fn k04_stored_401_recovers_once_then_unauthenticated_without_fallback() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    let first = login(&server, "k04-401").await;
    // The resolver never resends a rejected bearer, so the refresh must mint a
    // new access token for the same account; only then is the recovery observable.
    server.reset().await;
    let refreshed = jwt(json!({
        "https://api.openai.com/auth": {"chatgpt_account_id": "k04-401", "chatgpt_plan_type": "plus", "rotation": 1},
        "https://api.openai.com/profile": {"email": "operator@example.test"},
    }));
    Mock::given(method("POST"))
        .and(path(TOKEN))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"access_token": refreshed, "refresh_token": "REFRESH-k04-401-next", "expires_in": 3600}),
        ))
        .mount(&server)
        .await;
    roster(&server, CODEX_MODELS, 401, json!({})).await;
    roster(
        &server,
        ROOT_MODELS,
        200,
        json!({"models":[{"slug":"never"}]}),
    )
    .await;
    let layer = init_provider_layer(&app_config());
    let target = target(&layer, "openai-codex");
    let report = refresh_target(
        target,
        &ModelCatalogCache::new(),
        Some(&layer.registry),
        true,
    )
    .await;
    assert_eq!(report.status, DiscoveryStatus::Unauthenticated);
    assert_eq!(
        layer.registry.discovery_status("openai-codex"),
        Some(DiscoveryStatus::Unauthenticated)
    );
    assert_eq!(
        report.notice.as_deref(),
        Some(
            format!(
                "{} rejected its credential (HTTP 401/403) — models could not be discovered. {}",
                target.display_name, target.sign_in_hint
            )
            .as_str()
        )
    );
    assert!(
        target
            .sign_in_hint
            .contains("rustain auth login openai-codex")
    );
    let gets = requests(&server, CODEX_MODELS).await;
    assert_eq!(gets.len(), 2, "one recovery resend");
    assert_eq!(
        gets[0].headers["authorization"].to_str().unwrap(),
        format!("Bearer {first}")
    );
    assert_eq!(
        gets[1].headers["authorization"].to_str().unwrap(),
        format!("Bearer {refreshed}")
    );
    assert_eq!(
        requests(&server, ROOT_MODELS).await.len(),
        0,
        "no fallback after auth failure"
    );
    assert_eq!(
        requests(&server, TOKEN).await.len(),
        1,
        "exactly one refresh"
    );
    assert!(!served(&layer, "openai-codex").contains(&"never".to_owned()));
}

#[tokio::test]
#[serial]
async fn k04_env_token_401_is_final_without_refresh() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    world.set("OPENAI_CODEX_OAUTH_TOKEN", &access("env-account"));
    roster(&server, CODEX_MODELS, 401, json!({})).await;
    let layer = init_provider_layer(&app_config());
    let report = refresh_target(
        target(&layer, "openai-codex"),
        &ModelCatalogCache::new(),
        Some(&layer.registry),
        true,
    )
    .await;
    assert_eq!(report.status, DiscoveryStatus::Unauthenticated);
    assert_eq!(requests(&server, CODEX_MODELS).await.len(), 1);
    assert_eq!(requests(&server, ROOT_MODELS).await.len(), 0);
    assert_eq!(requests(&server, TOKEN).await.len(), 0);
}

/// Only 404/405 on both paths is "unsupported"; a primary-path failure followed
/// by a fallback 404 keeps its own report and the refresh-failed notice.
#[tokio::test]
#[serial]
async fn k04_unsupported_only_when_both_paths_say_so() {
    for (codex_status, unsupported) in [(404, true), (503, false)] {
        let server = MockServer::start().await;
        let _world = World::new(&server);
        login(&server, "k04-fallback-failure").await;
        roster(&server, CODEX_MODELS, codex_status, json!({})).await;
        roster(&server, ROOT_MODELS, 404, json!({})).await;
        let layer = init_provider_layer(&app_config());
        let report = refresh_target(
            target(&layer, "openai-codex"),
            &ModelCatalogCache::new(),
            Some(&layer.registry),
            true,
        )
        .await;
        assert_eq!(report.status, DiscoveryStatus::Unavailable);
        assert_eq!(requests(&server, CODEX_MODELS).await.len(), 1);
        assert_eq!(requests(&server, ROOT_MODELS).await.len(), 1);
        if unsupported {
            assert_eq!(report.notice, None, "{codex_status}");
        } else {
            let notice = report.notice.expect("a failed primary path is reported");
            assert!(notice.contains("Codex models HTTP 503"), "{notice}");
        }
    }
}

#[tokio::test]
#[serial]
async fn k04_foreign_base_with_only_a_stored_row_is_locked_and_never_sent() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    login(&server, "k04-foreign").await;
    server.reset().await;
    let foreign = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut cfg = app_config();
    cfg.provider.get_mut("openai-codex").unwrap().base_url =
        Some(format!("http://{}/backend-api", foreign.local_addr().unwrap()).into());
    let layer = init_provider_layer(&cfg);
    assert!(
        layer
            .discovery_targets
            .iter()
            .all(|t| t.provider_id != "openai-codex")
    );
    assert!(
        layer
            .registry
            .list_locked()
            .iter()
            .any(|p| p.provider_id == "openai-codex")
    );
    assert!(server.received_requests().await.unwrap().is_empty());
    drop(foreign);
}

#[tokio::test]
#[serial]
async fn k04_every_live_consumer_includes_codex() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    let token = login(&server, "k04-consumers").await;
    roster(
        &server,
        CODEX_MODELS,
        200,
        json!({"models":[{"slug":"gpt-5.6-luna"},{"slug":"announced"}]}),
    )
    .await;
    let layer = init_provider_layer(&app_config());
    // Startup and periodic refresh both iterate `discovery_targets`.
    let target = target(&layer, "openai-codex").clone();
    let (bus, mut domain_rx) = EventBus::new(64);
    let bus = Arc::new(bus);
    let tracker = rustain::adapters::tui::refresh_tracker::RefreshTracker::new();
    refresh_and_announce(
        &target,
        &ModelCatalogCache::new(),
        &layer.registry,
        true,
        &bus,
        &tracker,
    )
    .await;
    assert!(served(&layer, "openai-codex").contains(&"announced".to_owned()));
    let mut announced = false;
    while let Ok(event) = domain_rx.try_recv() {
        announced |= matches!(event, AppEvent::ProviderCatalogRefreshed { provider_id } if provider_id == "openai-codex");
    }
    assert!(announced);

    // The headless rail: its request reaches the (held) Codex endpoint.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    world.set(
        "RUSTAIN_TEST_AUTH_ORIGIN",
        &format!("http://{}", listener.local_addr().unwrap()),
    );
    let headless = init_provider_layer(&app_config());
    rustain::infrastructure::model_discovery::spawn_headless(
        headless.discovery_targets.clone(),
        headless.registry.clone(),
    );
    let held = accept(&listener).await;
    assert_eq!(
        held.path(),
        "/backend-api/codex/models?client_version=0.159.0"
    );
    assert_eq!(
        held.header("authorization"),
        Some(format!("Bearer {token}"))
    );
    held.json(&json!({"models":[{"slug":"gpt-5.6-luna"}]}))
        .await;
}

#[tokio::test]
#[serial]
async fn k06_scope_survives_rotation_and_restart_but_never_another_account() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    login(&server, "k06-account-a").await;
    roster(
        &server,
        CODEX_MODELS,
        200,
        json!({"models":[{"slug":"gpt-5.6-luna"},{"slug":"live-a"}]}),
    )
    .await;
    let cfg = app_config();
    let first = init_provider_layer(&cfg);
    refresh_target(
        target(&first, "openai-codex"),
        &ModelCatalogCache::new(),
        Some(&first.registry),
        true,
    )
    .await;
    assert!(served(&first, "openai-codex").contains(&"live-a".to_owned()));
    let scope_a = disk("openai-codex").await.credential_fingerprint;

    // Restart: the startup scope of the same row matches and serves without a GET.
    let restarted = init_provider_layer(&cfg);
    assert_eq!(
        target(&restarted, "openai-codex").credential_fingerprint,
        scope_a
    );
    assert!(served(&restarted, "openai-codex").contains(&"live-a".to_owned()));
    let before = requests(&server, CODEX_MODELS).await.len();
    let report = refresh_target(
        target(&restarted, "openai-codex"),
        &ModelCatalogCache::new(),
        Some(&restarted.registry),
        false,
    )
    .await;
    assert_eq!(report.status, DiscoveryStatus::Ok);
    assert_eq!(
        requests(&server, CODEX_MODELS).await.len(),
        before,
        "fresh scoped cache"
    );

    // Access-token rotation keeps the account scope.
    server.reset().await;
    let rotated = jwt(json!({
        "https://api.openai.com/auth": {"chatgpt_account_id": "k06-account-a", "chatgpt_plan_type": "plus", "rotation": 2},
        "https://api.openai.com/profile": {"email": "operator@example.test"},
    }));
    Mock::given(method("POST"))
        .and(path(TOKEN))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token": rotated, "refresh_token": "REFRESH-rotated", "expires_in": 3600})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(CODEX_MODELS))
        .respond_with(Script401Then200(std::sync::atomic::AtomicUsize::new(0)))
        .mount(&server)
        .await;
    let rotating = init_provider_layer(&cfg);
    let report = refresh_target(
        target(&rotating, "openai-codex"),
        &ModelCatalogCache::new(),
        Some(&rotating.registry),
        true,
    )
    .await;
    assert_eq!(report.status, DiscoveryStatus::Ok);
    let gets = requests(&server, CODEX_MODELS).await;
    assert_eq!(
        gets[1].headers["authorization"].to_str().unwrap(),
        format!("Bearer {rotated}")
    );
    assert_eq!(disk("openai-codex").await.credential_fingerprint, scope_a);
    let after_rotation = init_provider_layer(&cfg);
    assert!(served(&after_rotation, "openai-codex").contains(&"live-a".to_owned()));

    // Logout, recreate the store and log in to another account: row ids may
    // repeat, the old roster is never served and the first refresh fetches.
    FileAuthStore::new().remove("openai-codex").await.unwrap();
    let _ = std::fs::remove_file(world.dir.path().join("auth.json"));
    server.reset().await;
    login(&server, "k06-account-b").await;
    roster(
        &server,
        CODEX_MODELS,
        200,
        json!({"models":[{"slug":"live-b"}]}),
    )
    .await;
    let other = init_provider_layer(&cfg);
    let scope_b = target(&other, "openai-codex")
        .credential_fingerprint
        .clone();
    assert_ne!(scope_b, scope_a);
    assert!(!served(&other, "openai-codex").contains(&"live-a".to_owned()));
    refresh_target(
        target(&other, "openai-codex"),
        &ModelCatalogCache::new(),
        Some(&other.registry),
        false,
    )
    .await;
    assert_eq!(
        requests(&server, CODEX_MODELS).await.len(),
        1,
        "a new account fetches"
    );
    assert_eq!(served(&other, "openai-codex"), ["live-b"]);

    // An environment bearer has its own scope.
    FileAuthStore::new().remove("openai-codex").await.unwrap();
    world.set("OPENAI_CODEX_OAUTH_TOKEN", &access("k06-account-a"));
    let env = init_provider_layer(&cfg);
    let scope_env = target(&env, "openai-codex").credential_fingerprint.clone();
    assert_ne!(scope_env, scope_a);
    assert_ne!(scope_env, scope_b);
    assert!(!served(&env, "openai-codex").contains(&"live-a".to_owned()));
}

/// First discovery request is rejected, the recovered resend is served.
struct Script401Then200(std::sync::atomic::AtomicUsize);
impl wiremock::Respond for Script401Then200 {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        if self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            ResponseTemplate::new(401)
        } else {
            ResponseTemplate::new(200)
                .set_body_json(json!({"models":[{"slug":"gpt-5.6-luna"},{"slug":"live-a"}]}))
        }
    }
}

#[tokio::test]
#[serial]
async fn k06_aliases_with_one_account_keep_separate_cache_keys() {
    let server = MockServer::start().await;
    let _world = World::new(&server);
    login(&server, "k06-alias").await;
    roster(
        &server,
        CODEX_MODELS,
        200,
        json!({"models":[{"slug":"gpt-5.6-luna"}]}),
    )
    .await;
    let mut cfg = AppConfig {
        model: MODEL.into(),
        ..Default::default()
    };
    cfg.provider.insert("work".into(), config());
    cfg.provider.insert("home".into(), config());
    let layer = init_provider_layer(&cfg);
    let (work, home) = (target(&layer, "work"), target(&layer, "home"));
    assert_eq!(work.kind, "openai-codex");
    assert_ne!(work.scope_context, home.scope_context);
    assert_ne!(work.credential_fingerprint, home.credential_fingerprint);
    for t in [work, home] {
        refresh_target(t, &ModelCatalogCache::new(), Some(&layer.registry), true).await;
    }
    assert_ne!(
        disk("work").await.credential_fingerprint,
        disk("home").await.credential_fingerprint
    );
    assert_eq!(served(&layer, "work"), ["gpt-5.6-luna"]);
    assert_eq!(served(&layer, "home"), ["gpt-5.6-luna"]);
}

#[tokio::test]
#[serial]
async fn k06_delayed_old_account_response_never_overwrites_the_newer_account() {
    let server = MockServer::start().await;
    let world = World::new(&server);
    let token_a = login(&server, "k06-order-a").await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let held_origin = format!("http://{}", listener.local_addr().unwrap());
    world.set("RUSTAIN_TEST_AUTH_ORIGIN", &held_origin);
    let layer = init_provider_layer(&app_config());
    let target = target(&layer, "openai-codex").clone();

    // Account A's refresh is in flight and its response is held.
    let (a_target, a_registry) = (target.clone(), layer.registry.clone());
    let refresh_a = tokio::spawn(async move {
        refresh_target(
            &a_target,
            &ModelCatalogCache::new(),
            Some(&a_registry),
            true,
        )
        .await
    });
    let held_a = accept(&listener).await;
    assert_eq!(
        held_a.header("authorization"),
        Some(format!("Bearer {token_a}"))
    );

    // Account switch, then B's refresh completes first.
    FileAuthStore::new().remove("openai-codex").await.unwrap();
    world.set("RUSTAIN_TEST_AUTH_ORIGIN", &server.uri());
    server.reset().await;
    let token_b = login(&server, "k06-order-b").await;
    world.set("RUSTAIN_TEST_AUTH_ORIGIN", &held_origin);
    let cache = ModelCatalogCache::new();
    let (report_b, ()) = tokio::join!(
        refresh_target(&target, &cache, Some(&layer.registry), true),
        async {
            let held_b = accept(&listener).await;
            assert_eq!(
                held_b.header("authorization"),
                Some(format!("Bearer {token_b}"))
            );
            held_b.json(&json!({"models":[{"slug":"b-only"}]})).await;
        }
    );
    assert_eq!(report_b.status, DiscoveryStatus::Ok);
    assert_eq!(served(&layer, "openai-codex"), ["b-only"]);
    let scope_b = disk("openai-codex").await.credential_fingerprint;

    // A's delayed answer arrives last and changes nothing.
    held_a.json(&json!({"models":[{"slug":"a-only"}]})).await;
    let report_a = refresh_a.await.unwrap();
    assert_eq!(report_a.notice, None);
    assert_eq!(served(&layer, "openai-codex"), ["b-only"]);
    assert_eq!(
        layer.registry.discovery_status("openai-codex"),
        Some(DiscoveryStatus::Ok)
    );
    let entry = disk("openai-codex").await;
    assert_eq!(entry.credential_fingerprint, scope_b);
    assert_eq!(
        entry
            .models
            .iter()
            .map(|m| m.model_id.as_str())
            .collect::<Vec<_>>(),
        ["b-only"]
    );
    let live = target.last_live.load_full().unwrap();
    assert_eq!(live.credential_fingerprint, scope_b);
    assert_eq!(
        live.models
            .iter()
            .map(|m| m.model_id.as_str())
            .collect::<Vec<_>>(),
        ["b-only"]
    );
}
