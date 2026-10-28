//! Configured-entry identity and Codex catalogue gates through runtime consumers.
#![cfg(feature = "test-instrumentation")]
use super::auth::{jwt, login_oauth};
use rustain::adapters::{auth_store::FileAuthStore, model_catalog_cache::ModelCatalogCache};
use rustain::domain::models::provider::{DiscoveryStatus, ModelCapability};
use rustain::domain::models::{AppConfig, Credential, CredentialOrigin, ProviderConfig};
use rustain::domain::ports::{AuthStorePort, StreamingProvider};
use rustain::infrastructure::{
    model_discovery::{rediscover_after_auth_change, refresh_target, remerge_all},
    provider_factory,
    startup::init_provider_layer,
};
use serde_json::json;
use serial_test::serial;
use std::ffi::OsString;
use std::sync::Arc;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path},
};

const ENV: &[&str] = &[
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_BASE_URL",
    "OPENAI_API_KEY",
    "OPENAI_CODEX_OAUTH_TOKEN",
    "OPENROUTER_API_KEY",
    "GOOGLE_API_KEY",
    "DEEPSEEK_API_KEY",
    "MOONSHOT_API_KEY",
    "XAI_API_KEY",
    "XAI_OAUTH_TOKEN",
    "XAI_BASE_URL",
    "COPILOT_GITHUB_TOKEN",
    "COPILOT_INTEGRATION_ID",
    "RUSTAIN_DATA_DIR",
    "RUSTAIN_CONFIG_DIR",
    "RUSTAIN_MODELS_DEV_URL",
    "RUSTAIN_TEST_AUTH_ORIGIN",
    "RUSTAIN_TEST_AUTH_CALLBACK_PORT",
];
struct World {
    dir: tempfile::TempDir,
    saved: Vec<(&'static str, Option<OsString>)>,
}
impl World {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let saved = ENV
            .iter()
            .map(|name| (*name, std::env::var_os(name)))
            .collect();
        // SAFETY: all environment-mutating tests hold the shared serial lock.
        unsafe {
            for name in ENV {
                std::env::remove_var(name);
            }
            std::env::set_var("RUSTAIN_DATA_DIR", dir.path());
            std::env::set_var("RUSTAIN_CONFIG_DIR", dir.path());
            std::env::set_var("RUSTAIN_MODELS_DEV_URL", "http://127.0.0.1:1");
        }
        Self { dir, saved }
    }
    fn set(&self, name: &str, value: &str) {
        // SAFETY: caller holds the shared serial lock.
        unsafe {
            std::env::set_var(name, value);
        }
    }
    async fn key(&self, rows: &str, key: &str) {
        FileAuthStore::new()
            .upsert(
                rows,
                Credential::new_api_key(key.into()),
                CredentialOrigin::Login,
                false,
            )
            .await
            .unwrap();
    }
}
impl Drop for World {
    fn drop(&mut self) {
        for (name, value) in &self.saved {
            // SAFETY: the serial guard outlives this environment guard.
            unsafe {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }
}
fn auth_status(world: &World, cfg: &AppConfig) -> serde_json::Value {
    std::fs::write(
        world.dir.path().join("config.toml"),
        toml::to_string(cfg).unwrap(),
    )
    .unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_rustain"))
        .current_dir(world.dir.path())
        .args(["auth", "status", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}
fn entry(rows: &str, kind: &str, base: &str) -> ProviderConfig {
    ProviderConfig {
        provider_id: rows.into(),
        kind: Some(kind.into()),
        model_id: "configured".into(),
        api_key_env: String::new(),
        auth_header: None,
        enabled: true,
        base_url: Some(base.into()),
        context_window: None,
        supports_tools: None,
        discover_models: true,
        model_filter: vec!["*".into()],
        cache_ttl_seconds: 3600,
    }
}
async fn roster(server: &MockServer, credential: &str, id: &str) {
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", format!("Bearer {credential}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":[{"id":id}]})))
        .mount(server)
        .await;
}

#[tokio::test]
#[serial]
async fn same_kind_entries_keep_routes_credentials_cache_and_status_separate() {
    let world = World::new();
    let server = MockServer::start().await;
    let mut cfg = AppConfig::default();
    for (id, key) in [("alpha", "key-a"), ("beta", "key-b")] {
        world.key(id, key).await;
        // For non-OAuth kinds, rows are selected by map key, not provider_id.
        cfg.provider.insert(
            id.into(),
            entry(
                "deliberately-not-the-map-key",
                "openai",
                &format!("{}/v1", server.uri()),
            ),
        );
        roster(&server, key, &format!("{id}-only")).await;
    }
    let layer = init_provider_layer(&cfg);
    assert_eq!(
        layer.registry.provider_ids(),
        ["alpha".to_owned(), "beta".to_owned()].into()
    );
    assert_eq!(layer.discovery_targets.len(), 2);
    assert_ne!(
        layer.discovery_targets[0].credential_fingerprint,
        layer.discovery_targets[1].credential_fingerprint
    );
    let cache = ModelCatalogCache::new();
    for id in ["alpha", "beta"] {
        layer.router.set_active(id).unwrap();
        assert_eq!(layer.router.active_delegate_id(), id);
        layer.router.connectivity_probe().await.unwrap();
        let provider = layer.router.get_provider(id).unwrap();
        assert_eq!(provider.provider_descriptor().provider_id, id);
        let target = layer
            .discovery_targets
            .iter()
            .find(|t| t.provider_id == id)
            .unwrap();
        assert_eq!(
            refresh_target(target, &cache, Some(&layer.registry), true)
                .await
                .status,
            DiscoveryStatus::Ok
        );
        let served = layer.registry.list_models_by_provider(id);
        assert_eq!(served.len(), 1);
        assert_eq!(served[0].model_id, format!("{id}-only"));
        assert_eq!(served[0].provider_id, id);
    }
    let disk = cache.load().await;
    assert!(disk.providers.contains_key("alpha") && disk.providers.contains_key("beta"));
    assert_eq!(disk.providers["alpha"].models[0].model_id, "alpha-only");
    assert_eq!(disk.providers["beta"].models[0].model_id, "beta-only");
    layer
        .registry
        .set_discovery_status("alpha", DiscoveryStatus::Unauthenticated);
    assert_eq!(
        layer.registry.discovery_status("beta"),
        Some(DiscoveryStatus::Ok)
    );
    assert!(
        layer
            .registry
            .sign_in_hint("alpha")
            .unwrap()
            .contains("alpha")
    );
    assert!(
        layer
            .registry
            .sign_in_hint("beta")
            .unwrap()
            .contains("beta")
    );
    let restarted = init_provider_layer(&cfg);
    assert_eq!(
        restarted.registry.list_models_by_provider("alpha")[0].model_id,
        "alpha-only"
    );
    assert_eq!(
        restarted.registry.list_models_by_provider("beta")[0].model_id,
        "beta-only"
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 4);
    let status = auth_status(&world, &cfg);
    for id in ["alpha", "beta"] {
        assert_eq!(
            status["providers"]
                .as_array()
                .unwrap()
                .iter()
                .find(|p| p["provider"] == id)
                .unwrap()["source"],
            "login_key"
        );
    }
}

#[tokio::test]
#[serial]
async fn identical_endpoint_and_credential_still_have_independent_cache_slots() {
    let world = World::new();
    let server = MockServer::start().await;
    let mut cfg = AppConfig::default();
    for id in ["one", "two"] {
        world.key(id, "same-key").await;
        cfg.provider.insert(
            id.into(),
            entry(
                "same-internal-id",
                "openai-compatible",
                &format!("{}/v1", server.uri()),
            ),
        );
    }
    cfg.provider.get_mut("one").unwrap().supports_tools = Some(false);
    roster(&server, "same-key", "first-roster").await;
    let layer = init_provider_layer(&cfg);
    assert_ne!(
        layer.discovery_targets[0].credential_fingerprint,
        layer.discovery_targets[1].credential_fingerprint
    );
    let cache = ModelCatalogCache::new();
    refresh_target(
        &layer.discovery_targets[0],
        &cache,
        Some(&layer.registry),
        true,
    )
    .await;
    server.reset().await;
    roster(&server, "same-key", "second-roster").await;
    refresh_target(
        &layer.discovery_targets[1],
        &cache,
        Some(&layer.registry),
        true,
    )
    .await;
    assert_eq!(
        layer.registry.list_models_by_provider("one")[0].model_id,
        "first-roster"
    );
    assert_eq!(
        layer.registry.list_models_by_provider("two")[0].model_id,
        "second-roster"
    );
    assert!(
        !layer.registry.list_models_by_provider("one")[0]
            .capabilities
            .contains(&ModelCapability::ToolUse)
    );
    assert!(
        layer.registry.list_models_by_provider("two")[0]
            .capabilities
            .contains(&ModelCapability::ToolUse)
    );
    let restarted = init_provider_layer(&cfg);
    assert!(
        !restarted.registry.list_models_by_provider("one")[0]
            .capabilities
            .contains(&ModelCapability::ToolUse)
    );
    assert!(
        restarted.registry.list_models_by_provider("two")[0]
            .capabilities
            .contains(&ModelCapability::ToolUse)
    );
    let disk = cache.load().await;
    assert_eq!(disk.providers["one"].models[0].model_id, "first-roster");
    assert_eq!(disk.providers["two"].models[0].model_id, "second-roster");
}

#[tokio::test]
#[serial]
async fn tool_restriction_survives_live_cache_modelsdev_and_remerge() {
    let world = World::new();
    let server = MockServer::start().await;
    world.set("OPENAI_API_KEY", "tools-key");
    let mut cfg = AppConfig::default();
    let mut provider = entry("unused", "openai", &format!("{}/v1", server.uri()));
    provider.api_key_env = "OPENAI_API_KEY".into();
    provider.supports_tools = Some(false);
    cfg.provider.insert("no-tools".into(), provider);
    roster(&server, "tools-key", "extra-live").await;
    let layer = init_provider_layer(&cfg);
    let cache = ModelCatalogCache::new();
    refresh_target(
        &layer.discovery_targets[0],
        &cache,
        Some(&layer.registry),
        true,
    )
    .await;
    let assert_restricted = |registry: &rustain::adapters::provider::ProviderRegistry| {
        let served = registry.list_models_by_provider("no-tools");
        assert_eq!(served.len(), 1);
        assert_eq!(served[0].model_id, "extra-live");
        assert!(!served[0].capabilities.contains(&ModelCapability::ToolUse));
        assert!(
            !registry
                .list_models_by_capability(&ModelCapability::ToolUse)
                .iter()
                .any(|m| m.provider_id == "no-tools")
        );
    };
    assert_restricted(&layer.registry);
    world.set("RUSTAIN_MODELS_DEV_URL", &server.uri());
    Mock::given(path("/api.json")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"openai":{"models":{"extra-live":{"name":"Enriched","tool_call":true,"reasoning":true,"limit":{"context":123456}}}}}))).mount(&server).await;
    rustain::adapters::models_dev::refresh().await.unwrap();
    let (bus, _) = rustain::infrastructure::runtime::event_bus::EventBus::new(64);
    remerge_all(
        &layer.registry,
        &layer.catalogue_targets,
        &Arc::new(bus),
        &cfg,
    )
    .await;
    assert_restricted(&layer.registry);
    let enriched = layer.registry.list_models_by_provider("no-tools");
    assert_eq!(enriched[0].context_window, 123456);
    assert!(
        enriched[0]
            .capabilities
            .contains(&ModelCapability::Thinking)
    );
    assert_restricted(&init_provider_layer(&cfg).registry);
    // Restriction is entry-specific: an otherwise identical entry keeps ToolUse.
    cfg.provider.get_mut("no-tools").unwrap().supports_tools = None;
    assert!(
        init_provider_layer(&cfg)
            .registry
            .list_models_by_provider("no-tools")[0]
            .capabilities
            .contains(&ModelCapability::ToolUse)
    );
}

#[tokio::test]
#[serial]
async fn codex_seed_is_locked_then_signed_with_live_discovery_and_no_api_key_construction() {
    let world = World::new();
    let server = MockServer::start().await;
    world.set("RUSTAIN_TEST_AUTH_ORIGIN", &server.uri());
    let mut cfg = AppConfig::default();
    let mut provider = entry(
        "openai-codex",
        "openai-codex",
        &format!("{}/backend-api", server.uri()),
    );
    provider.api_key_env = "OPENAI_CODEX_OAUTH_TOKEN".into();
    provider.model_id = "gpt-5.6-luna".into();
    cfg.provider.insert("work".into(), provider.clone());
    let layer = init_provider_layer(&cfg);
    let locked = layer
        .registry
        .list_locked()
        .into_iter()
        .find(|p| p.provider_id == "work")
        .unwrap();
    let expected = [
        "gpt-5.6-luna",
        "gpt-5.6-sol",
        "gpt-5.6-terra",
        "gpt-6-astra",
        "gpt-6-luna",
        "gpt-6-sol",
        "gpt-6.1-sol",
        "gpt-daybreak-blue-latest",
    ];
    assert_eq!(
        locked
            .models
            .iter()
            .map(|m| m.model_id.as_str())
            .collect::<Vec<_>>(),
        expected
    );
    assert_eq!(
        locked.sign_in_hint,
        "Set OPENAI_CODEX_OAUTH_TOKEN or run `rustain auth login openai-codex`"
    );
    assert_eq!(
        locked
            .models
            .iter()
            .map(|m| m.context_window)
            .collect::<Vec<_>>(),
        [
            1000000, 1000000, 1000000, 272000, 272000, 272000, 500000, 272000
        ]
    );
    assert!(locked.models.iter().all(|m| {
        m.provider_id == "work"
            && [
                ModelCapability::Vision,
                ModelCapability::Thinking,
                ModelCapability::ToolUse,
            ]
            .iter()
            .all(|c| m.capabilities.contains(c))
    }));
    let before = FileAuthStore::new().snapshot().await.unwrap();
    // The configured `work` entry is signed by the row `auth login openai-codex` writes.
    world.set("RUSTAIN_TEST_AUTH_CALLBACK_PORT", "0");
    let store: Arc<dyn AuthStorePort> = Arc::new(FileAuthStore::new());
    login_oauth(
        &server,
        &store,
        1_800_000_000_000,
        json!({
            "access_token": jwt(json!({"https://api.openai.com/auth":{"chatgpt_account_id":"gate-account"}})),
            "refresh_token": "GATE-REFRESH",
            "expires_in": 3600,
        }),
    )
    .await;
    // Observe startup and rediscovery separately from the completed browser exchange.
    server.reset().await;
    let layer = init_provider_layer(&cfg);
    assert!(layer.registry.provider_ids().contains("work"));
    assert_eq!(
        layer
            .registry
            .list_models_by_provider("work")
            .iter()
            .map(|m| m.model_id.as_str())
            .collect::<Vec<_>>(),
        expected
    );
    assert_eq!(
        layer.registry.sign_in_hint("work").as_deref(),
        Some(locked.sign_in_hint.as_str())
    );
    // Story 20.1d1b: the signed Codex entry is now a live discovery target.
    assert!(
        layer
            .discovery_targets
            .iter()
            .any(|t| t.provider_id == "work")
    );
    assert!(
        !layer
            .unsupported_discovery
            .iter()
            .any(|(id, _)| id == "work")
    );
    assert!(
        layer
            .catalogue_targets
            .iter()
            .any(|t| t.provider_id == "work")
    );
    assert!(
        server.received_requests().await.unwrap().is_empty(),
        "startup seeds without a live request"
    );
    Mock::given(method("GET"))
        .and(path("/backend-api/codex/models"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"models":[{"slug":"gpt-5.6-luna"},{"slug":"gpt-6.1-sol"}]})),
        )
        .mount(&server)
        .await;
    // The login changed the row versions, so the live target is refreshed.
    let report = rediscover_after_auth_change("openai-codex", &cfg, &before)
        .await
        .expect("signed Codex entry is rediscovered after login");
    assert_eq!(report.status, DiscoveryStatus::Ok);
    assert_eq!(report.model_count, 2);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url.path(), "/backend-api/codex/models");
    assert_eq!(requests[0].url.query(), Some("client_version=0.159.0"));
    server.reset().await;
    assert_eq!(
        provider_factory::build_provider_for_config_with_key("work", &provider, "candidate")
            .err()
            .unwrap()
            .to_string(),
        "openai-codex has no API-key login"
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
#[serial]
async fn codex_catalogue_remerge_never_promotes_unrelated_openai_api_models() {
    let world = World::new();
    let server = MockServer::start().await;
    world.set("OPENAI_CODEX_OAUTH_TOKEN", "env-oauth-bearer");
    world.set("RUSTAIN_MODELS_DEV_URL", &server.uri());
    let mut cfg = AppConfig::default();
    let mut codex = entry(
        "openai-codex",
        "openai-codex",
        "https://chatgpt.com/backend-api",
    );
    codex.api_key_env = "OPENAI_CODEX_OAUTH_TOKEN".into();
    codex.supports_tools = Some(false);
    cfg.provider.insert("subscription".into(), codex);
    let layer = init_provider_layer(&cfg);
    Mock::given(path("/api.json"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"openai":{"models":{"api-only-model":{"tool_call":true}}}})),
        )
        .mount(&server)
        .await;
    rustain::adapters::models_dev::refresh().await.unwrap();
    let (bus, mut events) = rustain::infrastructure::runtime::event_bus::EventBus::new(64);
    remerge_all(
        &layer.registry,
        &layer.catalogue_targets,
        &Arc::new(bus),
        &cfg,
    )
    .await;
    let served = layer.registry.list_models_by_provider("subscription");
    assert_eq!(served.len(), 8);
    assert!(
        served.iter().all(|m| m.model_id != "api-only-model"
            && !m.capabilities.contains(&ModelCapability::ToolUse))
    );
    let mut republished = false;
    while let Ok(event) = events.try_recv() {
        if matches!(event, rustain::domain::events::AppEvent::ProviderCatalogRefreshed { provider_id } if provider_id == "subscription")
        {
            republished = true;
        }
    }
    assert!(republished);
}

#[tokio::test]
#[serial]
async fn login_refresh_selects_all_entries_by_credential_rows_not_first_kind() {
    let world = World::new();
    let server = MockServer::start().await;
    let mut cfg = AppConfig::default();
    for (id, rows) in [
        ("first", "anthropic"),
        ("second", "anthropic"),
        ("unrelated", "separate-account"),
    ] {
        cfg.provider.insert(
            id.into(),
            entry(rows, "anthropic", &format!("{}/v1", server.uri())),
        );
    }
    world.key("separate-account", "other-key").await;
    let before = FileAuthStore::new().snapshot().await.unwrap();
    world.key("anthropic", "login-key").await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("x-api-key", "login-key"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data":[{"id":"claude-shared"}],"has_more":false})),
        )
        .mount(&server)
        .await;
    let layer = init_provider_layer(&cfg);
    for id in ["first", "second", "unrelated"] {
        assert!(layer.registry.provider_ids().contains(id));
        assert_eq!(layer.router.get_provider(id).unwrap().provider_id(), id);
    }
    for id in ["first", "second"] {
        layer.router.set_active(id).unwrap();
        layer.router.connectivity_probe().await.unwrap();
    }
    assert_eq!(
        rediscover_after_auth_change("anthropic", &cfg, &before)
            .await
            .unwrap()
            .status,
        DiscoveryStatus::Ok
    );
    let disk = ModelCatalogCache::new().load().await;
    assert!(disk.providers.contains_key("first") && disk.providers.contains_key("second"));
    assert!(!disk.providers.contains_key("unrelated"));
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 4);
    assert!(
        requests
            .iter()
            .all(|r| r.headers["x-api-key"] == "login-key")
    );
    assert!(
        rediscover_after_auth_change(
            "anthropic",
            &cfg,
            &FileAuthStore::new().snapshot().await.unwrap()
        )
        .await
        .is_none()
    );
}

#[tokio::test]
#[serial]
async fn ollama_aliases_and_explicit_candidate_builders_keep_configured_identity() {
    let _world = World::new();
    let mut cfg = AppConfig::default();
    let a = MockServer::start().await;
    let b = MockServer::start().await;
    for (id, server) in [("local-a", &a), ("local-b", &b)] {
        cfg.provider.insert(
            id.into(),
            entry("same-internal-name", "ollama", &server.uri()),
        );
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"models":[{"name":format!("{id}-model"),"details":{}}]})),
            )
            .mount(server)
            .await;
        Mock::given(method("POST")).and(path("/api/show")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"capabilities":["tools","vision"],"model_info":{"llama.context_length":32768}}))).mount(server).await;
    }
    cfg.provider.get_mut("local-a").unwrap().supports_tools = Some(false);
    let layer = init_provider_layer(&cfg);
    for id in ["local-a", "local-b"] {
        assert!(layer.registry.provider_ids().contains(id));
        assert_eq!(layer.router.get_provider(id).unwrap().provider_id(), id);
        layer.router.set_active(id).unwrap();
        layer.router.health_check().await.unwrap();
        let served = layer.registry.list_models_by_provider(id);
        assert_eq!(served.len(), 1);
        assert_eq!(served[0].provider_id, id);
        assert_eq!(served[0].model_id, format!("{id}-model"));
        assert_eq!(served[0].context_window, 32768);
        assert_eq!(
            served[0].capabilities.contains(&ModelCapability::ToolUse),
            id == "local-b"
        );
    }
    assert_eq!(a.received_requests().await.unwrap().len(), 2);
    assert_eq!(b.received_requests().await.unwrap().len(), 2);
    let candidate = provider_factory::build_provider_for_config_with_key(
        "candidate-alias",
        &entry("ignored", "openai", "http://127.0.0.1:1/v1"),
        "key",
    )
    .unwrap();
    assert_eq!(
        candidate.provider_descriptor().provider_id,
        "candidate-alias"
    );
    assert!(
        candidate
            .list_models()
            .iter()
            .all(|m| m.provider_id == "candidate-alias")
    );
}

#[tokio::test]
#[serial]
async fn codex_rows_origin_gate_current_handle_and_exact_endpoint_spellings_agree() {
    use rustain::domain::models::{CredentialIdentity, OAuthCredential};
    let world = World::new();
    let official = MockServer::start().await;
    let gateway = MockServer::start().await;
    world.set("RUSTAIN_TEST_AUTH_ORIGIN", &official.uri());
    let store = FileAuthStore::new();
    let credential = |access: &str| {
        Credential::OAuth(OAuthCredential {
            access: access.into(),
            refresh: "not-used".into(),
            expires_at_ms: i64::MAX,
            authorized_at_ms: Some(1),
            identity: CredentialIdentity {
                account_id: Some("gate-account".into()),
                email: Some("gate@example.test".into()),
                ..Default::default()
            },
            routing: Default::default(),
        })
    };
    // Gate fixtures do not claim to exercise the login engine or JWT profile.
    store
        .upsert(
            "openai-codex",
            credential("row-first"),
            CredentialOrigin::Login,
            true,
        )
        .await
        .unwrap();
    let mut cfg = AppConfig::default();
    let signed = entry(
        "openai-codex",
        "openai-codex",
        &format!("{}/backend-api/codex", official.uri()),
    );
    cfg.provider.insert("work".into(), signed.clone());
    cfg.provider.insert(
        "proxy".into(),
        entry(
            "openai-codex",
            "openai-codex",
            &format!("{}/backend-api", gateway.uri()),
        ),
    );
    let layer = init_provider_layer(&cfg);
    assert!(layer.registry.provider_ids().contains("work"));
    assert!(
        layer
            .registry
            .list_locked()
            .iter()
            .any(|p| p.provider_id == "proxy")
    );
    assert!(matches!(
        provider_factory::build_provider_for_config("proxy", &cfg.provider["proxy"]),
        Err(rustain::domain::errors::ProviderError::AuthenticationFailed)
    ));
    let live = layer.router.get_provider("work").unwrap();
    let gateway_scope = provider_factory::discovery_fingerprint(
        "openai-codex",
        &[],
        &cfg.provider["proxy"],
        live.as_ref(),
    );
    let status = auth_status(&world, &cfg);
    assert_eq!(
        status["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["provider"] == "work")
            .unwrap()["source"],
        "oauth"
    );
    assert!(
        !status["providers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["provider"] == "proxy")
    );
    Mock::given(method("GET"))
        .and(path("/backend-api/codex/models"))
        .and(header("authorization", "Bearer row-first"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&official)
        .await;
    live.connectivity_probe().await.unwrap();
    store
        .upsert(
            "openai-codex",
            credential("row-second"),
            CredentialOrigin::Login,
            true,
        )
        .await
        .unwrap();
    Mock::given(method("GET"))
        .and(path("/backend-api/codex/models"))
        .and(header("authorization", "Bearer row-second"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&official)
        .await;
    // The original adapter reads current rows, not a construction-time bearer.
    live.connectivity_probe().await.unwrap();
    assert!(gateway.received_requests().await.unwrap().is_empty());
    assert_eq!(official.received_requests().await.unwrap().len(), 2);
    for suffix in [
        "/backend-api",
        "/backend-api/",
        "/backend-api/codex",
        "/backend-api/codex/responses/",
    ] {
        let same = entry(
            "openai-codex",
            "openai-codex",
            &format!("{}{suffix}", official.uri()),
        );
        assert_eq!(
            provider_factory::discovery_base_url("work", &same),
            format!("{}/backend-api/codex/responses", official.uri())
        );
        let adapter = provider_factory::build_provider_for_config("work", &same).unwrap();
        adapter.connectivity_probe().await.unwrap();
    }
    assert_eq!(official.received_requests().await.unwrap().len(), 6);
    store.remove("openai-codex").await.unwrap();
    assert_eq!(
        provider_factory::discovery_fingerprint(
            "openai-codex",
            &[],
            &cfg.provider["proxy"],
            live.as_ref()
        ),
        gateway_scope
    );
    assert!(matches!(
        provider_factory::build_provider_for_config("work", &signed),
        Err(rustain::domain::errors::ProviderError::AuthenticationFailed)
    ));
}

#[tokio::test]
#[serial]
async fn locked_tool_restriction_is_applied_after_public_catalogue_remerge() {
    let world = World::new();
    let server = MockServer::start().await;
    world.set("RUSTAIN_MODELS_DEV_URL", &server.uri());
    let mut cfg = AppConfig::default();
    let mut restricted = entry("irrelevant", "openai", "https://api.openai.com/v1");
    restricted.supports_tools = Some(false);
    cfg.provider.insert("restricted".into(), restricted);
    cfg.provider.insert(
        "normal".into(),
        entry("irrelevant", "openai", "https://api.openai.com/v1"),
    );
    let layer = init_provider_layer(&cfg);
    let before = layer.registry.list_locked();
    assert!(
        before
            .iter()
            .find(|p| p.provider_id == "restricted")
            .unwrap()
            .models
            .iter()
            .all(|m| !m.capabilities.contains(&ModelCapability::ToolUse))
    );
    Mock::given(path("/api.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"openai":{"models":{"public-extra":{"tool_call":true,"reasoning":true}}}}),
        ))
        .mount(&server)
        .await;
    rustain::adapters::models_dev::refresh().await.unwrap();
    let (bus, _) = rustain::infrastructure::runtime::event_bus::EventBus::new(64);
    remerge_all(
        &layer.registry,
        &layer.catalogue_targets,
        &Arc::new(bus),
        &cfg,
    )
    .await;
    let locked = layer.registry.list_locked();
    let extra = |id: &str| {
        locked
            .iter()
            .find(|p| p.provider_id == id)
            .unwrap()
            .models
            .iter()
            .find(|m| m.model_id == "public-extra")
            .unwrap()
    };
    assert!(
        !extra("restricted")
            .capabilities
            .contains(&ModelCapability::ToolUse)
    );
    assert!(
        extra("restricted")
            .capabilities
            .contains(&ModelCapability::Thinking)
    );
    assert!(
        extra("normal")
            .capabilities
            .contains(&ModelCapability::ToolUse)
    );
}

#[tokio::test]
#[serial]
async fn locked_codex_entry_keeps_a_routable_bundled_default_active() {
    let world = World::new();
    world.set("ANTHROPIC_API_KEY", "sk-ant-api03-gate-fixture");
    let mut cfg = AppConfig::default();
    cfg.provider.insert(
        "openai-codex".into(),
        entry(
            "openai-codex",
            "openai-codex",
            "https://chatgpt.com/backend-api",
        ),
    );
    let layer = init_provider_layer(&cfg);
    assert!(
        layer
            .registry
            .list_locked()
            .iter()
            .any(|p| p.provider_id == "openai-codex")
    );
    // Configuring Codex before `auth login` must not strand headless turns.
    assert_eq!(layer.active_id, "anthropic");
    layer.router.abort().await.unwrap();
}
