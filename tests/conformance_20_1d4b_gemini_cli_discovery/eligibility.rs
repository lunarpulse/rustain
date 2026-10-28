//! K5: the signed entry is a live discovery target reached by every live
//! consumer, the post-login rediscovery, the generic rejected notice, and
//! locked entries that never discover.
use super::common::*;
use crate::conformance_20_1d1b_codex_replay_discovery::common::accept;
use rustain::adapters::model_catalog_cache::ModelCatalogCache;
use rustain::adapters::tui::refresh_tracker::RefreshTracker;
use rustain::domain::events::AppEvent;
use rustain::domain::models::ProviderConfig;
use rustain::domain::models::provider::DiscoveryStatus;
use rustain::infrastructure::model_discovery::{
    rediscover_after_login, refresh_and_announce, rejected_notice, spawn_headless,
};
use rustain::infrastructure::runtime::event_bus::EventBus;
use rustain::infrastructure::startup::init_provider_layer;
use serde_json::json;
use serial_test::serial;
use std::sync::Arc;
use wiremock::Request;

fn discovery_paths(requests: &[Request]) -> Vec<&str> {
    requests
        .iter()
        .map(|r| r.url.path())
        .filter(|p| [FETCH, QUOTA, LOAD].contains(p))
        .collect()
}

fn refreshed(rx: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>) -> Vec<String> {
    let mut ids = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let AppEvent::ProviderCatalogRefreshed { provider_id } = event {
            ids.push(provider_id);
        }
    }
    ids
}

#[tokio::test]
#[serial]
async fn k5_the_signed_entry_is_live_and_every_consumer_discovers() {
    let world = World::signed_out().await;
    let cfg = app_config();
    let before = store().snapshot().await.unwrap();
    login(&world.server, ACCESS, PROJECT, EMAIL).await;
    let baseline = world.requests().await.len();

    // `auth login`'s post-login refresh discovers through the new login.
    mount_quota_path(&world.server, &["gemini-9-pro"]).await;
    let report = rediscover_after_login(PROVIDER, &cfg, &before)
        .await
        .expect("a new Gemini CLI login is rediscovered");
    assert_eq!(report.status, DiscoveryStatus::Ok);
    assert_eq!(report.model_count, SEED.len() + 1);
    assert_eq!(report.notice, None);
    assert_eq!(
        discovery_paths(&world.since(baseline).await),
        [FETCH, FETCH, QUOTA]
    );

    // Both target lists; no "doesn't support model discovery" toast.
    let layer = init_provider_layer(&cfg);
    let live = target(&layer, PROVIDER);
    assert_eq!(live.kind, PROVIDER);
    assert_eq!(live.display_name, NAME);
    assert_eq!(live.sign_in_hint, HINT);
    assert!(
        layer
            .catalogue_targets
            .iter()
            .any(|t| t.provider_id == PROVIDER)
    );
    assert!(
        layer
            .unsupported_discovery
            .iter()
            .all(|(id, kind)| id != PROVIDER && kind != PROVIDER)
    );
    // "models could not be discovered" is now true for the kind.
    assert_eq!(
        rejected_notice(live),
        "Google Cloud Code Assist (Gemini CLI) rejected its credential (HTTP 401/403) — models could not be discovered. Run `rustain auth login google-gemini-cli`"
    );
    // The roster discovered at login is served at startup without a request.
    assert!(ids(&served(&layer)).contains(&"gemini-9-pro".to_owned()));

    // Startup `refresh_and_announce` (unforced) and the periodic tick (forced)
    // over the same target list: each reaches the stub and announces.
    let _ = std::fs::remove_file(world.temp.path().join("models_cache.json"));
    let fresh = init_provider_layer(&cfg);
    let (bus, mut rx) = EventBus::new(64);
    let bus = Arc::new(bus);
    let tracker = RefreshTracker::new();
    let cache = ModelCatalogCache::new();
    for (force, live_id) in [(false, "gemini-startup"), (true, "gemini-periodic")] {
        world.server.reset().await;
        mount_quota_path(&world.server, &[live_id]).await;
        for target in &fresh.discovery_targets {
            refresh_and_announce(target, &cache, &fresh.registry, force, &bus, &tracker).await;
        }
        assert_eq!(
            discovery_paths(&world.requests().await),
            [FETCH, FETCH, QUOTA],
            "force {force}"
        );
        assert!(ids(&served(&fresh)).contains(&live_id.to_owned()));
        assert_eq!(refreshed(&mut rx), [PROVIDER]);
    }

    // The headless rail: its first request reaches a held Antigravity endpoint.
    let _ = std::fs::remove_file(world.temp.path().join("models_cache.json"));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let held_origin = format!("http://{}", listener.local_addr().unwrap());
    let _route = Env::set(&[("RUSTAIN_TEST_AUTH_ORIGIN", Some(held_origin.as_str()))]);
    let headless = init_provider_layer(&cfg);
    spawn_headless(
        headless.discovery_targets.clone(),
        headless.registry.clone(),
    );
    let held = accept(&listener).await;
    assert_eq!(held.path(), FETCH);
    assert_eq!(
        held.header("authorization"),
        Some(format!("Bearer {ACCESS}"))
    );
    assert_eq!(held.header("user-agent").as_deref(), Some(ANTIGRAVITY_UA));
    held.json(&json!({"models": {}})).await;
}

#[tokio::test]
#[serial]
async fn k5_locked_entries_never_discover() {
    let world = World::signed_out().await;
    let cfg = app_config();
    let before = store().snapshot().await.unwrap();

    // No login: locked, not a target, nothing sent.
    let layer = init_provider_layer(&cfg);
    assert!(
        layer
            .discovery_targets
            .iter()
            .all(|t| t.provider_id != PROVIDER)
    );
    assert!(
        layer
            .registry
            .list_locked()
            .iter()
            .any(|p| p.provider_id == PROVIDER)
    );
    spawn_headless(layer.discovery_targets.clone(), layer.registry.clone());
    assert!(
        rediscover_after_login(PROVIDER, &cfg, &before)
            .await
            .is_none()
    );
    assert!(world.requests().await.is_empty());

    // A stored row with a foreign-origin `base_url`: locked, never sent.
    login(&world.server, ACCESS, PROJECT, EMAIL).await;
    world.server.reset().await;
    let foreign = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut cfg = app_config();
    cfg.provider.insert(
        PROVIDER.into(),
        ProviderConfig {
            provider_id: PROVIDER.into(),
            kind: Some(PROVIDER.into()),
            model_id: MODEL.into(),
            api_key_env: String::new(),
            auth_header: None,
            enabled: true,
            base_url: Some(format!("http://{}", foreign.local_addr().unwrap()).into()),
            context_window: None,
            supports_tools: None,
            discover_models: true,
            model_filter: vec![],
            cache_ttl_seconds: 3600,
        },
    );
    let layer = init_provider_layer(&cfg);
    assert!(
        layer
            .discovery_targets
            .iter()
            .all(|t| t.provider_id != PROVIDER)
    );
    assert!(
        layer
            .registry
            .list_locked()
            .iter()
            .any(|p| p.provider_id == PROVIDER)
    );
    spawn_headless(layer.discovery_targets.clone(), layer.registry.clone());
    let after = store().snapshot().await.unwrap();
    store().remove(PROVIDER).await.unwrap();
    login(&world.server, ACCESS, PROJECT, EMAIL).await;
    world.server.reset().await;
    assert!(
        rediscover_after_login(PROVIDER, &cfg, &after)
            .await
            .is_none()
    );
    assert!(world.requests().await.is_empty());
    drop(foreign);
}
