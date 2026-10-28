//! Credential-gated discovery through production composition and refresh boundaries.
use rustain::adapters::auth_store::FileAuthStore;
use rustain::domain::models::{AppConfig, Credential, CredentialOrigin, ProviderConfig};
use rustain::domain::ports::AuthStorePort;
use rustain::infrastructure::startup::init_provider_layer;
use serial_test::serial;
use std::ffi::{OsStr, OsString};

const KEYS: &[&str] = &[
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_API_KEY",
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
    "ANTHROPIC_BASE_URL",
    "GW_TOKEN",
    "RUSTAIN_DATA_DIR",
    "RUSTAIN_CONFIG_DIR",
    "RUSTAIN_MODELS_DEV_URL",
    "RUSTAIN_TEST_AUTH_ORIGIN",
];
struct World {
    dir: tempfile::TempDir,
    saved: Vec<(&'static str, Option<OsString>)>,
}
impl World {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let saved = KEYS
            .iter()
            .map(|name| (*name, std::env::var_os(name)))
            .collect();
        // SAFETY: every parent-environment test holds the shared unkeyed serial lock.
        unsafe {
            for name in KEYS {
                std::env::remove_var(name);
            }
            std::env::set_var("RUSTAIN_DATA_DIR", dir.path());
            std::env::set_var("RUSTAIN_CONFIG_DIR", dir.path());
            std::env::set_var("RUSTAIN_MODELS_DEV_URL", "http://127.0.0.1:1");
        }
        Self { dir, saved }
    }
    fn set(&self, name: &str, value: impl AsRef<OsStr>) {
        // SAFETY: caller holds #[serial].
        unsafe {
            std::env::set_var(name, value);
        }
    }
    async fn key(&self, provider: &str, key: &str) {
        FileAuthStore::new()
            .upsert(
                provider,
                Credential::new_api_key(key.into()),
                CredentialOrigin::Stored,
                false,
            )
            .await
            .unwrap();
    }
}
impl Drop for World {
    fn drop(&mut self) {
        for (name, value) in &self.saved {
            // SAFETY: the shared serial guard outlives this guard.
            unsafe {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }
}
fn config(id: &str, kind: &str, key: &str, url: Option<String>) -> ProviderConfig {
    ProviderConfig {
        provider_id: id.into(),
        kind: Some(kind.into()),
        model_id: "configured-model".into(),
        api_key_env: key.into(),
        auth_header: None,
        enabled: true,
        base_url: url.map(Into::into),
        context_window: None,
        supports_tools: None,
        discover_models: true,
        model_filter: vec!["*".into()],
        cache_ttl_seconds: 3600,
    }
}

#[tokio::test]
#[serial]
async fn k01_home_key_provisions_without_config_and_env_unlocks() {
    let world = World::new();
    world.key("openrouter", "home-key").await;
    let layer = init_provider_layer(&AppConfig::default());
    assert_eq!(
        layer.registry.provider_ids(),
        ["openrouter".to_owned()].into()
    );
    let locked: std::collections::BTreeSet<_> = layer
        .registry
        .list_locked()
        .iter()
        .map(|p| p.provider_id.clone())
        .collect();
    assert_eq!(
        locked,
        [
            "anthropic",
            "deepseek",
            "github-copilot",
            "google",
            "google-gemini-cli",
            "moonshot",
            "openai",
            "openai-codex",
            "xai",
            "xai-oauth"
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    );
    for (provider, hint) in [
        ("xai", "Set XAI_API_KEY or run `rustain auth login xai`"),
        (
            "xai-oauth",
            "Set XAI_OAUTH_TOKEN or run `rustain auth login xai-oauth`",
        ),
    ] {
        assert_eq!(
            layer
                .registry
                .list_locked()
                .iter()
                .find(|p| p.provider_id == provider)
                .unwrap()
                .sign_in_hint,
            hint
        );
    }
    assert_eq!(
        layer
            .registry
            .list_locked()
            .iter()
            .find(|p| p.provider_id == "openai")
            .unwrap()
            .sign_in_hint,
        "Set OPENAI_API_KEY or run `rustain auth login openai`"
    );
    assert_eq!(
        layer
            .registry
            .list_models_by_provider("openrouter")
            .iter()
            .map(|m| m.model_id.as_str())
            .collect::<Vec<_>>(),
        [
            "anthropic/claude-opus-4.7",
            "anthropic/claude-sonnet-4.6",
            "anthropic/claude-haiku-4.5",
            "openai/gpt-5.5",
            "openai/gpt-5.4",
            "google/gemini-3.1-pro-preview",
            "deepseek/deepseek-v4-pro",
            "deepseek/deepseek-v4-flash"
        ]
    );
    assert_eq!(
        layer
            .discovery_targets
            .iter()
            .map(|t| t.provider_id.as_str())
            .collect::<Vec<_>>(),
        ["openrouter"]
    );
    assert!(layer.deferred_notices.is_empty());
    world.set("OPENAI_API_KEY", "env-key");
    assert!(
        init_provider_layer(&AppConfig::default())
            .registry
            .provider_ids()
            .contains("openai")
    );
}

#[test]
#[serial]
fn k02_keyless_custom_is_registered_but_named_missing_key_is_locked() {
    let _world = World::new();
    let mut cfg = AppConfig::default();
    cfg.provider.insert(
        "local".into(),
        config(
            "local",
            "openai-compatible",
            "",
            Some("http://127.0.0.1:1/v1".into()),
        ),
    );
    let layer = init_provider_layer(&cfg);
    assert!(layer.registry.provider_ids().contains("local"));
    assert!(
        layer
            .discovery_targets
            .iter()
            .any(|t| t.provider_id == "local")
    );
    cfg.provider.get_mut("local").unwrap().api_key_env = "GW_TOKEN".into();
    let layer = init_provider_layer(&cfg);
    assert!(!layer.registry.provider_ids().contains("local"));
    assert!(
        layer
            .registry
            .list_locked()
            .iter()
            .any(|p| p.provider_id == "local")
    );
}

#[test]
#[serial]
fn k03_config_anthropic_uses_its_named_environment_only() {
    let world = World::new();
    world.set("ANTHROPIC_API_KEY", "not-for-gateway");
    let mut cfg = AppConfig::default();
    cfg.provider.insert(
        "anthropic".into(),
        config("anthropic", "anthropic", "GW_TOKEN", None),
    );
    let layer = init_provider_layer(&cfg);
    assert!(!layer.registry.provider_ids().contains("anthropic"));
    assert!(
        layer
            .registry
            .list_locked()
            .iter()
            .find(|p| p.provider_id == "anthropic")
            .unwrap()
            .sign_in_hint
            .contains("GW_TOKEN")
    );
    assert!(layer.deferred_notices.is_empty());
    world.set("GW_TOKEN", "gateway-key");
    assert!(
        init_provider_layer(&cfg)
            .registry
            .provider_ids()
            .contains("anthropic")
    );
}

use rustain::adapters::model_catalog_cache::ModelCatalogCache;
use rustain::domain::models::provider::DiscoveryStatus;
use rustain::infrastructure::model_discovery::refresh_target;
use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

fn remote_config(provider: &str, url: String) -> AppConfig {
    let mut cfg = AppConfig::default();
    let key = match provider {
        "anthropic" => "ANTHROPIC_API_KEY",
        _ => "OPENAI_API_KEY",
    };
    cfg.provider
        .insert(provider.into(), config(provider, provider, key, Some(url)));
    cfg
}

async fn models(server: &MockServer, code: u16, data: serde_json::Value) {
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(code).set_body_json(json!({"data": data})))
        .mount(server)
        .await;
}

#[tokio::test]
#[serial]
async fn k05_rejected_credentials_are_not_offline_and_never_write_cache() {
    let world = World::new();
    world.set("OPENAI_API_KEY", "rejected-key");
    let server = MockServer::start().await;
    let cfg = remote_config("openai", format!("{}/v1", server.uri()));
    let layer = init_provider_layer(&cfg);
    let target = &layer.discovery_targets[0];
    let cache = ModelCatalogCache::new();
    let cache_path = world.dir.path().join("models_cache.json");
    std::fs::write(&cache_path, r#"{"version":3,"providers":{}}"#).unwrap();
    let before = std::fs::read(&cache_path).unwrap();
    for (code, status) in [
        (401, DiscoveryStatus::Unauthenticated),
        (403, DiscoveryStatus::Unauthenticated),
        (500, DiscoveryStatus::Unavailable),
        (404, DiscoveryStatus::Unavailable),
        (405, DiscoveryStatus::Unavailable),
    ] {
        models(&server, code, json!([])).await;
        let report = refresh_target(target, &cache, Some(&layer.registry), true).await;
        assert_eq!(report.status, status);
        assert_eq!(layer.registry.discovery_status("openai"), Some(status));
        assert_eq!(std::fs::read(&cache_path).unwrap(), before);
        if code == 401 {
            assert!(
                report
                    .notice
                    .as_ref()
                    .unwrap()
                    .contains("rustain auth login openai")
            );
            assert!(
                layer
                    .registry
                    .list_models_by_provider("openai")
                    .iter()
                    .all(|m| m.stale)
            );
            assert!(
                refresh_target(target, &cache, Some(&layer.registry), true)
                    .await
                    .notice
                    .is_none()
            );
        } else if code == 500 {
            assert!(!report.notice.unwrap().contains("auth login"));
        } else if code == 404 || code == 405 {
            assert!(report.notice.is_none());
        }
    }
    let refused = init_provider_layer(&remote_config("openai", "http://127.0.0.1:1/v1".into()));
    assert_eq!(
        refresh_target(
            &refused.discovery_targets[0],
            &cache,
            Some(&refused.registry),
            true
        )
        .await
        .status,
        DiscoveryStatus::Unavailable
    );
    assert_eq!(std::fs::read(&cache_path).unwrap(), before);
}

#[tokio::test]
#[serial]
async fn k06_authoritative_live_prunes_empty_is_not_cached_and_failure_restores_cache_ids() {
    let world = World::new();
    world.set("OPENAI_API_KEY", "account-key");
    let server = MockServer::start().await;
    let layer = init_provider_layer(&remote_config("openai", format!("{}/v1", server.uri())));
    let target = &layer.discovery_targets[0];
    let cache = ModelCatalogCache::new();
    models(&server, 200, json!([{"id":"gpt-old"}, {"id":"gpt-x"}])).await;
    refresh_target(target, &cache, Some(&layer.registry), true).await;
    models(&server, 200, json!([{"id":"gpt-x"}, {"id":"gpt-z"}])).await;
    let report = refresh_target(target, &cache, Some(&layer.registry), true).await;
    assert_eq!(report.status, DiscoveryStatus::Ok);
    assert_eq!(
        layer
            .registry
            .list_models_by_provider("openai")
            .iter()
            .map(|m| m.model_id.as_str())
            .collect::<Vec<_>>(),
        ["gpt-x", "gpt-z"]
    );
    let saved = std::fs::read(world.dir.path().join("models_cache.json")).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&saved).unwrap();
    assert_eq!(
        value["providers"]["openai"]["models"][0]["modelId"],
        "gpt-x"
    );
    assert_eq!(
        value["providers"]["openai"]["models"][1]["modelId"],
        "gpt-z"
    );
    assert_eq!(
        value["providers"]["openai"]["credentialFingerprint"],
        target.credential_fingerprint
    );
    models(&server, 200, json!([])).await;
    assert_eq!(
        refresh_target(target, &cache, Some(&layer.registry), true)
            .await
            .status,
        DiscoveryStatus::Empty
    );
    assert!(layer.registry.list_models_by_provider("openai").is_empty());
    assert_eq!(
        std::fs::read(world.dir.path().join("models_cache.json")).unwrap(),
        saved
    );
    models(&server, 500, json!([])).await;
    assert_eq!(
        refresh_target(target, &cache, Some(&layer.registry), true)
            .await
            .status,
        DiscoveryStatus::Cached
    );
    assert_eq!(
        layer
            .registry
            .list_models_by_provider("openai")
            .iter()
            .map(|m| m.model_id.as_str())
            .collect::<Vec<_>>(),
        ["gpt-x", "gpt-z"]
    );
}

#[cfg(feature = "models-dev")]
#[tokio::test]
#[serial]
async fn k09_provider_metadata_fills_sparse_live_fields_without_overwriting_with_defaults() {
    let world = World::new();
    world.set("OPENAI_API_KEY", "metadata-key");
    let server = MockServer::start().await;
    world.set("RUSTAIN_MODELS_DEV_URL", server.uri());
    let raw = json!({"openai":{"models":{"gpt-x":{"name":"GPT X","limit":{"context":400000},"tool_call":true,"reasoning":true},"text-embedding-77":{"name":"Noise"}}},"moonshotai":{"models":{"kimi-extra":{"name":"Kimi Extra"}}}});
    Mock::given(path("/api.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&raw))
        .mount(&server)
        .await;
    let pricing = rustain::adapters::models_dev::refresh().await.unwrap();
    assert_eq!(
        serde_json::to_value(&pricing.pricing).unwrap(),
        serde_json::to_value(
            rustain::adapters::models_dev::reduce_catalog(&raw.to_string()).unwrap()
        )
        .unwrap()
    );
    models(
        &server,
        200,
        json!([{"id":"gpt-x","context_length":0,"name":"gpt-x"},{"id":"gpt-z"}]),
    )
    .await;
    let layer = init_provider_layer(&remote_config("openai", format!("{}/v1", server.uri())));
    let cache = ModelCatalogCache::new();
    refresh_target(
        &layer.discovery_targets[0],
        &cache,
        Some(&layer.registry),
        true,
    )
    .await;
    let served = layer.registry.list_models_by_provider("openai");
    assert_eq!(
        (&*served[0].display_name, served[0].context_window),
        ("GPT X", 400000)
    );
    assert_eq!(served[1].context_window, 128000);
    assert!(
        served[0]
            .capabilities
            .contains(&rustain::domain::models::provider::ModelCapability::Thinking)
    );
    models(
        &server,
        200,
        json!([{"id":"gpt-x","context_length":900000,"name":"Live X"}]),
    )
    .await;
    refresh_target(
        &layer.discovery_targets[0],
        &cache,
        Some(&layer.registry),
        true,
    )
    .await;
    let served = layer.registry.list_models_by_provider("openai");
    assert_eq!(
        (&*served[0].display_name, served[0].context_window),
        ("Live X", 900000)
    );
    // A locked column sees public metadata, never account-specific discovery.
    unsafe {
        std::env::remove_var("OPENAI_API_KEY");
    }
    let locked = init_provider_layer(&AppConfig::default())
        .registry
        .list_locked();
    let ids: Vec<_> = locked
        .iter()
        .find(|p| p.provider_id == "openai")
        .unwrap()
        .models
        .iter()
        .map(|m| m.model_id.as_str())
        .collect();
    assert!(ids.contains(&"gpt-x"));
    assert!(!ids.contains(&"text-embedding-77"));
    assert!(
        locked
            .iter()
            .find(|p| p.provider_id == "moonshot")
            .unwrap()
            .models
            .iter()
            .any(|m| m.model_id == "kimi-extra")
    );
}

#[tokio::test]
#[serial]
async fn k11_cache_scope_changes_refetch_and_removal_locks_without_old_roster() {
    let world = World::new();
    let server = MockServer::start().await;
    let cfg = remote_config("openai", format!("{}/v1", server.uri()));
    let cache = ModelCatalogCache::new();
    world.set("OPENAI_API_KEY", "key-A");
    models(&server, 200, json!([{"id":"account-A-only"}])).await;
    let a = init_provider_layer(&cfg);
    refresh_target(&a.discovery_targets[0], &cache, Some(&a.registry), true).await;
    server.reset().await;
    refresh_target(&a.discovery_targets[0], &cache, Some(&a.registry), false).await;
    assert!(server.received_requests().await.unwrap().is_empty());
    world.set("OPENAI_API_KEY", "key-B");
    models(&server, 200, json!([{"id":"account-B-only"}])).await;
    let b = init_provider_layer(&cfg);
    refresh_target(&b.discovery_targets[0], &cache, Some(&b.registry), false).await;
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].headers["authorization"], "Bearer key-B");
    assert_eq!(
        b.registry.list_models_by_provider("openai")[0].model_id,
        "account-B-only"
    );
    unsafe {
        std::env::remove_var("OPENAI_API_KEY");
    }
    world.key("openai", "stored-C").await;
    models(&server, 200, json!([{"id":"account-C-only"}])).await;
    let c = init_provider_layer(&cfg);
    refresh_target(&c.discovery_targets[0], &cache, Some(&c.registry), false).await;
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].headers["authorization"], "Bearer stored-C");
    FileAuthStore::new().remove("openai").await.unwrap();
    let locked = init_provider_layer(&cfg);
    assert!(!locked.registry.provider_ids().contains("openai"));
    assert!(
        locked
            .registry
            .list_locked()
            .iter()
            .find(|p| p.provider_id == "openai")
            .unwrap()
            .models
            .iter()
            .all(|m| !m.model_id.starts_with("account-"))
    );
}

#[tokio::test]
#[serial]
async fn k12_login_rediscovery_bypasses_a_matching_fresh_cache_only_after_rows_change() {
    use rustain::infrastructure::model_discovery::rediscover_after_login;
    let world = World::new();
    let server = MockServer::start().await;
    let cfg = remote_config("openai", format!("{}/v1", server.uri()));
    let before = FileAuthStore::new().snapshot().await.unwrap();
    world.key("openai", "login-B").await;
    let layer = init_provider_layer(&cfg);
    let cache = ModelCatalogCache::new();
    models(&server, 200, json!([{"id":"old"}])).await;
    refresh_target(
        &layer.discovery_targets[0],
        &cache,
        Some(&layer.registry),
        true,
    )
    .await;
    models(&server, 200, json!([{"id":"new1"},{"id":"new2"}])).await;
    let report = rediscover_after_login("openai", &cfg, &before)
        .await
        .unwrap();
    assert_eq!(report.model_count, 2);
    assert_eq!(report.status, DiscoveryStatus::Ok);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    let cached = cache.load().await;
    assert_eq!(
        cached.providers["openai"]
            .models
            .iter()
            .map(|m| m.model_id.as_str())
            .collect::<Vec<_>>(),
        ["new1", "new2"]
    );
    assert_eq!(
        cached.providers["openai"].credential_fingerprint,
        layer.discovery_targets[0].credential_fingerprint
    );
    let unchanged = FileAuthStore::new().snapshot().await.unwrap();
    server.reset().await;
    assert!(
        rediscover_after_login("openai", &cfg, &unchanged)
            .await
            .is_none()
    );
    assert!(
        rediscover_after_login("ollama", &cfg, &before)
            .await
            .is_none()
    );
    // Keyless control past the unchanged-rows guard: its stored rows change,
    // but a keyless endpoint has no credential to rediscover, so nothing is sent.
    let mut keyless = cfg.clone();
    keyless.provider.insert(
        "local".into(),
        config(
            "local",
            "openai-compatible",
            "",
            Some(format!("{}/v1", server.uri())),
        ),
    );
    let before_local = FileAuthStore::new().snapshot().await.unwrap();
    world.key("local", "unused-local-key").await;
    assert!(
        rediscover_after_login("local", &keyless, &before_local)
            .await
            .is_none()
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
#[serial]
async fn recreated_store_real_login_rediscovery_detects_reused_id_and_revision() {
    use futures::future::BoxFuture;
    use rustain::adapters::auth::flows::{self, FlowCtx, LoginUi};
    use rustain::domain::models::{AuthMethod, SecretString};
    use rustain::infrastructure::model_discovery::rediscover_after_login;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    struct KeyUi(&'static str);
    impl LoginUi for KeyUi {
        fn on_auth_url(&self, _: &str, _: &str, _: &str) {
            panic!("API-key login must not open a browser");
        }
        fn on_device_code(&self, _: &str, _: &str, _: &str) {
            panic!("API-key login must not show a device code");
        }
        fn on_progress(&self, _: &str) {}
        fn prompt_secret(&self, _: &str) -> BoxFuture<'_, Option<SecretString>> {
            Box::pin(async { Some(SecretString::new(self.0.into())) })
        }
        fn manual_input(&self, _: &str) -> Option<BoxFuture<'static, Option<String>>> {
            None
        }
        fn prompt_line(&self, _: &str) -> BoxFuture<'_, Option<String>> {
            Box::pin(async { None })
        }
        fn cancel_token(&self) -> CancellationToken {
            CancellationToken::new()
        }
    }
    async fn login_key(key: &'static str, cfg: &AppConfig) {
        let store: Arc<dyn AuthStorePort> = Arc::new(FileAuthStore::new());
        let context = FlowCtx::new(Arc::new(KeyUi(key)), Arc::new(|| 0))
            .with_validation_config(cfg.provider["openai"].clone());
        assert!(
            flows::login("openai", &AuthMethod::ApiKey, &context, &store)
                .await
                .unwrap()
                .validated
        );
    }

    let world = World::new();
    let server = MockServer::start().await;
    let cfg = remote_config("openai", format!("{}/v1", server.uri()));
    models(&server, 200, json!([{"id":"account-A-model"}])).await;
    login_key("login-A", &cfg).await;
    let before = FileAuthStore::new().snapshot().await.unwrap();
    let original = &before.rows("openai")[0];
    let layer = init_provider_layer(&cfg);
    let cache = ModelCatalogCache::new();
    assert_eq!(
        refresh_target(
            &layer.discovery_targets[0],
            &cache,
            Some(&layer.registry),
            true
        )
        .await
        .status,
        DiscoveryStatus::Ok
    );

    std::fs::remove_file(world.dir.path().join("auth.json")).unwrap();
    models(&server, 200, json!([{"id":"account-B-model"}])).await;
    login_key("login-B", &cfg).await;
    let after = FileAuthStore::new().snapshot().await.unwrap();
    let replacement = &after.rows("openai")[0];
    assert_eq!(replacement.id, original.id);
    assert_eq!(replacement.rev, original.rev);

    // Exclude B's completed validation probe; observe only the post-login refresh.
    models(&server, 200, json!([{"id":"account-B-model"}])).await;
    let report = rediscover_after_login("openai", &cfg, &before)
        .await
        .expect("a recreated store's new grant must rediscover despite repeated id/rev");
    assert_eq!(report.status, DiscoveryStatus::Ok);
    assert_eq!(report.model_count, 1);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url.path(), "/v1/models");
    assert_eq!(requests[0].headers["authorization"], "Bearer login-B");
    assert_eq!(
        cache.load().await.providers["openai"]
            .models
            .iter()
            .map(|model| model.model_id.as_str())
            .collect::<Vec<_>>(),
        ["account-B-model"]
    );

    server.reset().await;
    assert!(
        rediscover_after_login("openai", &cfg, &after)
            .await
            .is_none()
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[cfg(all(feature = "anthropic", feature = "models-dev"))]
#[tokio::test]
#[serial]
async fn k10_anthropic_pages_use_shared_auth_and_provider_metadata() {
    let world = World::new();
    world.set("ANTHROPIC_API_KEY", "anthropic-discovery-key");
    let server = MockServer::start().await;
    world.set("RUSTAIN_MODELS_DEV_URL", server.uri());
    let raw = json!({"anthropic":{"models":{"claude-new":{"name":"Claude New","limit":{"context":500000},"cost":{"input":3,"output":15}}}},"openai":{"models":{"gpt-public":{"name":"Public GPT"}}}});
    Mock::given(path("/api.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&raw))
        .mount(&server)
        .await;
    rustain::adapters::models_dev::refresh().await.unwrap();
    // Pricing positive control: the persisted pricing from this same download
    // is exactly the unchanged pricing reduction, with real costs present.
    let pricing = rustain::adapters::models_dev::load_cache().unwrap().pricing;
    assert_eq!(
        serde_json::to_value(&pricing).unwrap(),
        serde_json::to_value(
            rustain::adapters::models_dev::reduce_catalog(&raw.to_string()).unwrap()
        )
        .unwrap()
    );
    assert_eq!(
        (
            pricing["claude-new"].input_per_million,
            pricing["claude-new"].output_per_million
        ),
        (3.0, 15.0)
    );
    Mock::given(path("/v1/models")).and(wiremock::matchers::query_param("limit","1000")).and(wiremock::matchers::query_param_is_missing("after_id")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":[{"id":"claude-sonnet-4-6","display_name":"Claude Sonnet 4.6"},{"id":"claude-new","display_name":"Claude New"}],"has_more":true,"last_id":"claude-new"}))).mount(&server).await;
    Mock::given(path("/v1/models"))
        .and(wiremock::matchers::query_param("after_id", "claude-new"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data":[{"id":"claude-unknown"}],"has_more":false})),
        )
        .mount(&server)
        .await;
    let cfg = remote_config("anthropic", format!("{}/v1", server.uri()));
    let layer = init_provider_layer(&cfg);
    let target = layer
        .discovery_targets
        .iter()
        .find(|t| t.provider_id == "anthropic")
        .unwrap();
    assert_eq!(
        refresh_target(
            target,
            &ModelCatalogCache::new(),
            Some(&layer.registry),
            true
        )
        .await
        .status,
        DiscoveryStatus::Ok
    );
    let served = layer.registry.list_models_by_provider("anthropic");
    assert_eq!(
        served
            .iter()
            .map(|m| m.model_id.as_str())
            .collect::<Vec<_>>(),
        ["claude-sonnet-4-6", "claude-new", "claude-unknown"]
    );
    assert_eq!(
        served.iter().map(|m| m.context_window).collect::<Vec<_>>(),
        [1000000, 500000, 200000]
    );
    let requests = server.received_requests().await.unwrap();
    let discovery: Vec<_> = requests
        .iter()
        .filter(|r| r.url.path() == "/v1/models")
        .collect();
    assert_eq!(discovery.len(), 2);
    for request in discovery {
        assert_eq!(request.headers["x-api-key"], "anthropic-discovery-key");
        assert_eq!(request.headers["anthropic-version"], "2023-06-01");
        assert!(request.body.is_empty());
    }
    assert!(
        layer
            .registry
            .list_locked()
            .iter()
            .find(|p| p.provider_id == "openai")
            .unwrap()
            .models
            .iter()
            .any(|m| m.model_id == "gpt-public")
    );
}

fn auth_binary(world: &World, cwd: &std::path::Path, action: &str) -> std::process::Output {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_rustain"));
    command
        .current_dir(cwd)
        .args(["auth", action, "--json"])
        .env("RUSTAIN_DATA_DIR", world.dir.path())
        .env("RUSTAIN_CONFIG_DIR", world.dir.path())
        .env("RUSTAIN_MODELS_DEV_URL", "http://127.0.0.1:1");
    for name in KEYS.iter().filter(|name| !name.starts_with("RUSTAIN_")) {
        command.env_remove(name);
    }
    command.env("OPENAI_API_KEY", "binary-env-key");
    command.output().unwrap()
}

#[test]
#[serial]
fn k04_k08_any_cwd_and_corrupt_store_keep_env_status_without_leaking_file_bytes() {
    let world = World::new();
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    std::fs::create_dir(first.path().join(".rustain")).unwrap();
    std::fs::write(
        first.path().join(".rustain/config.toml"),
        "[permissions]\ndefault_mode = \"normal\"\n",
    )
    .unwrap();
    let a = auth_binary(&world, first.path(), "list");
    let b = auth_binary(&world, second.path(), "list");
    assert!(a.status.success(), "{}", String::from_utf8_lossy(&a.stderr));
    assert!(b.status.success(), "{}", String::from_utf8_lossy(&b.stderr));
    let a: serde_json::Value = serde_json::from_slice(&a.stdout).unwrap();
    let b: serde_json::Value = serde_json::from_slice(&b.stdout).unwrap();
    assert_eq!(a, b);
    assert!(a.get("store_error").is_none());
    std::fs::write(
        world.dir.path().join("auth.json"),
        r#"{"providers": "CORRUPT-SECRET-CANARY""#,
    )
    .unwrap();
    for action in ["status", "list"] {
        let output = auth_binary(&world, second.path(), action);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(value["store_error"].as_str().is_some());
        let openai = value["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["provider"] == "openai")
            .unwrap();
        if action == "status" {
            assert_eq!(openai["source"], "env");
        } else {
            assert_eq!(openai["configured"], true);
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("warning: auth.json unreadable; stored credentials ignored:"));
        assert!(!stderr.contains("CORRUPT-SECRET-CANARY"));
        assert!(!String::from_utf8_lossy(&output.stdout).contains("CORRUPT-SECRET-CANARY"));
    }
}

#[tokio::test]
#[serial]
async fn explicit_overrides_disabled_rows_unknown_kinds_and_locked_router_are_distinct() {
    use rustain::domain::ports::StreamingProvider;
    let world = World::new();
    let mut cfg = AppConfig::default();
    cfg.provider.insert(
        "zai".into(),
        config(
            "zai",
            "anthropic",
            "GW_TOKEN",
            Some("http://127.0.0.1:1".into()),
        ),
    );
    world.set("GW_TOKEN", "gateway-key");
    let layer = init_provider_layer(&cfg);
    assert!(layer.registry.provider_ids().contains("zai"));
    assert!(
        !layer
            .registry
            .list_locked()
            .iter()
            .any(|p| p.provider_id == "anthropic")
    );
    assert!(
        layer
            .discovery_targets
            .iter()
            .any(|t| t.provider_id == "zai")
    );
    cfg.provider.clear();
    let mut hidden = config("openai", "openai", "OPENAI_API_KEY", None);
    hidden.enabled = false;
    cfg.provider.insert("openai".into(), hidden);
    cfg.provider.insert(
        "unknown".into(),
        config("unknown", "not-a-provider", "", None),
    );
    let layer = init_provider_layer(&cfg);
    assert!(!layer.registry.provider_ids().contains("openai"));
    assert!(
        !layer
            .registry
            .list_locked()
            .iter()
            .any(|p| p.provider_id == "openai" || p.provider_id == "unknown")
    );
    assert!(layer.deferred_notices.iter().any(|(id, _)| id == "unknown"));
    let error = layer.router.abort().await.unwrap_err().to_string();
    assert!(error.starts_with("router: no active provider 'anthropic'"));
    assert!(error.contains("rustain auth login anthropic"));
    let result = layer
        .router
        .stream_completion(
            vec![],
            rustain::domain::models::CompletionOptions {
                session_id: None,
                model: "claude-sonnet-4-6".into(),
                max_tokens: 1,
                system_prompt: String::new(),
                temperature: None,
                tools: vec![],
            },
        )
        .await;
    assert!(
        result
            .err()
            .unwrap()
            .to_string()
            .contains("has no credentials configured")
    );
    // A locked `kind = "anthropic"` alias reads `provider_id` rows: its hint
    // names that login, and the headless default route keeps it actionable.
    cfg.provider.clear();
    cfg.provider.insert(
        "zai".into(),
        config(
            "anthropic",
            "anthropic",
            "",
            Some("http://127.0.0.1:1".into()),
        ),
    );
    let layer = init_provider_layer(&cfg);
    let locked = layer.registry.list_locked();
    assert_eq!(
        locked
            .iter()
            .find(|p| p.provider_id == "zai")
            .unwrap()
            .sign_in_hint,
        "Run `rustain auth login anthropic`"
    );
    assert_eq!(
        layer.router.abort().await.unwrap_err().to_string(),
        "router: no active provider 'zai' — Anthropic has no credentials configured. \
         Run `rustain auth login anthropic`"
    );
}

#[test]
#[serial]
fn bundled_cc_token_future_store_warning_and_locked_filter_use_no_account_cache() {
    let world = World::new();
    world.set("ANTHROPIC_AUTH_TOKEN", "cc-env-bearer");
    assert!(
        init_provider_layer(&AppConfig::default())
            .registry
            .provider_ids()
            .contains("anthropic")
    );
    world.set("OPENAI_API_KEY", "env-works-with-unreadable-store");
    let future = r#"{"version":999,"providers":{"openai":"FUTURE-SECRET-CANARY"}}"#;
    std::fs::write(world.dir.path().join("auth.json"), future).unwrap();
    let layer = init_provider_layer(&AppConfig::default());
    assert!(layer.store_warning.is_some());
    assert!(
        !layer
            .store_warning
            .as_ref()
            .unwrap()
            .contains("FUTURE-SECRET-CANARY")
    );
    assert!(layer.registry.provider_ids().contains("openai"));
    assert_eq!(
        std::fs::read_to_string(world.dir.path().join("auth.json")).unwrap(),
        future
    );
    unsafe {
        std::env::remove_var("OPENAI_API_KEY");
    }
    let mut cfg = AppConfig::default();
    let mut openai = config("openai", "openai", "OPENAI_API_KEY", None);
    openai.model_filter = vec!["gpt-5.5".into()];
    cfg.provider.insert("openai".into(), openai);
    let locked = init_provider_layer(&cfg).registry.list_locked();
    assert_eq!(
        locked
            .iter()
            .find(|p| p.provider_id == "openai")
            .unwrap()
            .models
            .iter()
            .map(|m| m.model_id.as_str())
            .collect::<Vec<_>>(),
        ["gpt-5.5"]
    );
}

#[tokio::test]
#[serial]
async fn v2_cache_refetches_once_and_the_key_a_request_sends_defines_scope() {
    let world = World::new();
    let server = MockServer::start().await;
    world.set("OPENAI_API_KEY", "captured-key-A");
    std::fs::write(
        world.dir.path().join("models_cache.json"),
        r#"{"version":2,"providers":{"openai":{"fetchedAtUnix":9999999999,"models":[]}}}"#,
    )
    .unwrap();
    models(&server, 200, json!([{"id":"v3-live"}])).await;
    let cfg = remote_config("openai", format!("{}/v1", server.uri()));
    let layer = init_provider_layer(&cfg);
    let target = &layer.discovery_targets[0];
    let original = target.credential_fingerprint.clone();
    world.set("OPENAI_API_KEY", "different-current-key-B");
    // OpenAI-family adapters resolve their key per request (14-0 D-4): the scope
    // follows the key the next request sends, never one captured at startup.
    let current = rustain::infrastructure::provider_factory::discovery_fingerprint(
        "openai",
        &["OPENAI_API_KEY"],
        &cfg.provider["openai"],
        target.provider.as_ref(),
    );
    assert_ne!(current, original);
    refresh_target(
        target,
        &ModelCatalogCache::new(),
        Some(&layer.registry),
        false,
    )
    .await;
    let received = server.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    assert_eq!(
        received[0].headers["authorization"],
        "Bearer different-current-key-B"
    );
    let cached = ModelCatalogCache::new().load().await;
    assert_eq!(cached.providers["openai"].models[0].model_id, "v3-live");
    assert_eq!(cached.providers["openai"].credential_fingerprint, current);
    let file = std::fs::read_to_string(world.dir.path().join("models_cache.json")).unwrap();
    for secret in ["captured-key-A", "different-current-key-B", &server.uri()] {
        assert!(!file.contains(secret));
    }
    // A restart with key B scopes exactly as the refresh that sent key B stamped.
    assert_eq!(
        init_provider_layer(&cfg).discovery_targets[0].credential_fingerprint,
        current
    );
    // Same key B, another endpoint: the endpoint alone changes the scope.
    let mut other = cfg;
    other.provider.get_mut("openai").unwrap().base_url =
        Some(format!("{}/another/v1", server.uri()).into());
    assert_ne!(
        init_provider_layer(&other).discovery_targets[0].credential_fingerprint,
        current
    );
}

#[tokio::test]
#[serial]
async fn anthropic_stale_oauth_scope_survives_access_rotation_and_gateway_never_counts_it() {
    use rustain::domain::models::{CredentialIdentity, OAuthCredential};
    let world = World::new();
    world.set("ANTHROPIC_API_KEY", "lower-priority-env");
    let identity = CredentialIdentity {
        account_id: Some("account-scope-1".into()),
        email: Some("safe@example.test".into()),
        ..Default::default()
    };
    let oauth = |access: &str| {
        Credential::OAuth(OAuthCredential {
            access: access.into(),
            refresh: "refresh-scope-secret".into(),
            expires_at_ms: 0,
            authorized_at_ms: Some(1),
            identity: identity.clone(),
            routing: Default::default(),
        })
    };
    let store = FileAuthStore::new();
    store
        .upsert(
            "anthropic",
            oauth("old-access-secret"),
            CredentialOrigin::Login,
            true,
        )
        .await
        .unwrap();
    let a = init_provider_layer(&AppConfig::default());
    let a = &a
        .discovery_targets
        .iter()
        .find(|t| t.provider_id == "anthropic")
        .unwrap()
        .credential_fingerprint;
    store
        .upsert(
            "anthropic",
            oauth("rotated-access-secret"),
            CredentialOrigin::Login,
            true,
        )
        .await
        .unwrap();
    let b = init_provider_layer(&AppConfig::default());
    let b = &b
        .discovery_targets
        .iter()
        .find(|t| t.provider_id == "anthropic")
        .unwrap()
        .credential_fingerprint;
    assert_eq!(a, b);
    let mut cfg = AppConfig::default();
    cfg.provider.insert(
        "anthropic".into(),
        config(
            "anthropic",
            "anthropic",
            "GW_TOKEN",
            Some("http://127.0.0.1:1".into()),
        ),
    );
    assert!(
        !init_provider_layer(&cfg)
            .registry
            .provider_ids()
            .contains("anthropic")
    );
    std::fs::write(
        world.dir.path().join("config.toml"),
        toml::to_string(&cfg).unwrap(),
    )
    .unwrap();
    let out = auth_binary(&world, world.dir.path(), "list");
    assert!(out.status.success());
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        value["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["provider"] == "anthropic")
            .unwrap()["configured"],
        false
    );
    let out = auth_binary(&world, world.dir.path(), "status");
    assert!(out.status.success());
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let row = value["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["provider"] == "anthropic")
        .unwrap();
    assert_ne!(row["source"], "oauth");
    assert_eq!(row["oauth_grants"][0]["who"], "safe@example.test");
}

#[cfg(feature = "models-dev")]
#[tokio::test]
#[serial]
async fn models_dev_completion_remerges_sparse_live_and_locked_catalog_but_preserves_empty() {
    use rustain::infrastructure::{model_discovery::remerge_all, runtime::event_bus::EventBus};
    let world = World::new();
    world.set("OPENAI_API_KEY", "remerge-key");
    let server = MockServer::start().await;
    let cfg = remote_config("openai", format!("{}/v1", server.uri()));
    let layer = init_provider_layer(&cfg);
    let cache = ModelCatalogCache::new();
    models(&server, 200, json!([{"id":"gpt-new"}])).await;
    refresh_target(
        &layer.discovery_targets[0],
        &cache,
        Some(&layer.registry),
        true,
    )
    .await;
    world.set("RUSTAIN_MODELS_DEV_URL", server.uri());
    Mock::given(path("/api.json")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"openai":{"models":{"gpt-new":{"name":"Enriched New","limit":{"context":700000}}}},"anthropic":{"models":{"claude-public-new":{"limit":{"context":500000}}}}}))).mount(&server).await;
    rustain::adapters::models_dev::refresh().await.unwrap();
    let (bus, mut events) = EventBus::new(64);
    let bus = std::sync::Arc::new(bus);
    remerge_all(&layer.registry, &layer.discovery_targets, &bus, &cfg).await;
    let served = layer.registry.list_models_by_provider("openai");
    assert_eq!(
        served
            .iter()
            .map(|m| m.model_id.as_str())
            .collect::<Vec<_>>(),
        ["gpt-new"]
    );
    assert_eq!(
        (&*served[0].display_name, served[0].context_window),
        ("Enriched New", 700000)
    );
    assert!(
        layer
            .registry
            .list_locked()
            .iter()
            .find(|p| p.provider_id == "anthropic")
            .unwrap()
            .models
            .iter()
            .any(|m| m.model_id == "claude-public-new")
    );
    assert!(
        matches!(events.try_recv().unwrap(),rustain::domain::events::AppEvent::ProviderCatalogRefreshed{provider_id} if provider_id=="openai")
    );
    models(&server, 200, json!([])).await;
    refresh_target(
        &layer.discovery_targets[0],
        &cache,
        Some(&layer.registry),
        true,
    )
    .await;
    remerge_all(&layer.registry, &layer.discovery_targets, &bus, &cfg).await;
    assert!(layer.registry.list_models_by_provider("openai").is_empty());
}

#[tokio::test]
#[serial]
async fn acp_composition_fetches_live_while_one_shot_cli_only_seeds() {
    let world = World::new();
    world.set("OPENAI_API_KEY", "headless-key");
    let server = MockServer::start().await;
    models(&server, 200, json!([{"id":"headless-account-model"}])).await;
    let cfg = remote_config("openai", format!("{}/v1", server.uri()));
    let workspace = tempfile::tempdir().unwrap();
    let core =
        rustain::infrastructure::composition::build_acp_core(&cfg, workspace.path(), false, &[])
            .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !core
            .registry
            .list_models_by_provider("openai")
            .iter()
            .any(|m| m.model_id == "headless-account-model")
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    server.reset().await;
    let cli = rustain::infrastructure::composition::build_cli_core(&cfg, workspace.path(), false)
        .unwrap();
    // One-shot startup serves bundled ⊕ the matching account cache synchronously:
    // the authoritative cached roster, before and without any request.
    assert_eq!(
        cli.provider
            .list_models()
            .iter()
            .map(|m| m.model_id.as_str())
            .collect::<Vec<_>>(),
        ["headless-account-model"]
    );
    tokio::task::yield_now().await;
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn live_publication_waits_for_remerge_boundary_and_persists_before_overlay() {
    let world = World::new();
    world.set("OPENAI_API_KEY", "publication-key");
    let server = MockServer::start().await;
    models(&server, 200, json!([{"id":"publication-live"}])).await;
    let layer = init_provider_layer(&remote_config("openai", format!("{}/v1", server.uri())));
    let registry = layer.registry.clone();
    let held = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
    let signal = held.clone();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let thread = std::thread::spawn(move || {
        registry.with_catalog_update(|| {
            signal.add_permits(1);
            let _ = release_rx.recv();
        })
    });
    // Release and join even when a consumer assertion unwinds.
    let release = scopeguard::guard((release_tx, thread), |(tx, thread)| {
        let _ = tx.send(());
        let _ = thread.join();
    });
    held.acquire().await.unwrap().forget();
    let target = layer.discovery_targets[0].clone();
    let registry = layer.registry.clone();
    let refresh = tokio::spawn(async move {
        refresh_target(&target, &ModelCatalogCache::new(), Some(&registry), true).await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let cache = ModelCatalogCache::new().load().await;
            if cache
                .providers
                .get("openai")
                .is_some_and(|p| p.models.iter().any(|m| m.model_id == "publication-live"))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        !layer
            .registry
            .list_models_by_provider("openai")
            .iter()
            .any(|m| m.model_id == "publication-live")
    );
    let (tx, thread) = scopeguard::ScopeGuard::into_inner(release);
    tx.send(()).unwrap();
    thread.join().unwrap();
    assert_eq!(refresh.await.unwrap().status, DiscoveryStatus::Ok);
    assert_eq!(
        layer.registry.list_models_by_provider("openai")[0].model_id,
        "publication-live"
    );
}

fn ids(models: Vec<rustain::domain::models::ModelDescriptor>) -> Vec<String> {
    models.into_iter().map(|model| model.model_id).collect()
}

/// The cache is stamped with the credential a discovery request actually sent.
/// A stale OAuth row whose refresh fails falls through to the env key; that
/// key's roster is never served for the OAuth account once it refreshes again.
#[cfg(all(feature = "anthropic", feature = "test-instrumentation"))]
#[tokio::test]
#[serial]
async fn anthropic_fallthrough_roster_is_never_served_for_the_oauth_account() {
    use rustain::domain::models::{CredentialIdentity, OAuthCredential};
    use wiremock::matchers::header;
    let world = World::new();
    let server = MockServer::start().await;
    // Instrumented builds treat this loopback origin as the official API origin.
    world.set("RUSTAIN_TEST_AUTH_ORIGIN", server.uri());
    world.set("ANTHROPIC_BASE_URL", server.uri());
    world.set("ANTHROPIC_API_KEY", "secondary-key-B");
    FileAuthStore::new()
        .upsert(
            "anthropic",
            Credential::OAuth(OAuthCredential {
                access: "expired-access-A".into(),
                refresh: "refresh-A".into(),
                expires_at_ms: 0,
                authorized_at_ms: Some(1),
                identity: CredentialIdentity {
                    account_id: Some("account-A".into()),
                    ..Default::default()
                },
                routing: Default::default(),
            }),
            CredentialOrigin::Login,
            true,
        )
        .await
        .unwrap();
    let roster = |id: &str| {
        ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": id}], "has_more": false}))
    };
    // The OAuth refresh fails transiently, so requests fall through to key B.
    Mock::given(method("POST"))
        .and(path("/v1/oauth/token"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("x-api-key", "secondary-key-B"))
        .respond_with(roster("claude-b-only"))
        .mount(&server)
        .await;
    let cache = ModelCatalogCache::new();
    let first = init_provider_layer(&AppConfig::default());
    let target = first
        .discovery_targets
        .iter()
        .find(|t| t.provider_id == "anthropic")
        .unwrap();
    refresh_target(target, &cache, Some(&first.registry), true).await;
    assert_eq!(
        ids(first.registry.list_models_by_provider("anthropic")),
        ["claude-b-only"]
    );

    // Within the TTL, the OAuth account refreshes again.
    server.reset().await;
    Mock::given(method("POST"))
        .and(path("/v1/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "access-A2",
            "refresh_token": "refresh-A2",
            "expires_in": 3600,
            "account": {"uuid": "account-A"}
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", "Bearer access-A2"))
        .respond_with(roster("claude-a-only"))
        .mount(&server)
        .await;
    let second = init_provider_layer(&AppConfig::default());
    assert!(
        !ids(second.registry.list_models_by_provider("anthropic"))
            .contains(&"claude-b-only".to_owned()),
        "startup must not seed key B's roster for the OAuth row"
    );
    let target = second
        .discovery_targets
        .iter()
        .find(|t| t.provider_id == "anthropic")
        .unwrap();
    refresh_target(target, &cache, Some(&second.registry), false).await;
    assert_eq!(
        ids(second.registry.list_models_by_provider("anthropic")),
        ["claude-a-only"]
    );
    let discovery: Vec<_> = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path() == "/v1/models")
        .collect();
    assert_eq!(discovery.len(), 1);
    assert_eq!(discovery[0].headers["authorization"], "Bearer access-A2");
}

/// Row ids restart when `auth.json` is recreated while `models_cache.json`
/// survives: account B's row may reuse account A's id, and must never seed or
/// serve A's roster.
#[cfg(all(feature = "anthropic", feature = "test-instrumentation"))]
#[tokio::test]
#[serial]
async fn recreated_store_reusing_a_row_id_never_serves_the_previous_account() {
    use rustain::domain::models::{CredentialIdentity, OAuthCredential};
    use wiremock::matchers::header;
    let world = World::new();
    let server = MockServer::start().await;
    world.set("RUSTAIN_TEST_AUTH_ORIGIN", server.uri());
    world.set("ANTHROPIC_BASE_URL", server.uri());
    let login = |account: &str, access: &str| {
        Credential::OAuth(OAuthCredential {
            access: access.into(),
            refresh: format!("refresh-{account}").into(),
            expires_at_ms: chrono::Utc::now().timestamp_millis() + 3_600_000,
            authorized_at_ms: Some(1),
            identity: CredentialIdentity {
                account_id: Some(account.into()),
                ..Default::default()
            },
            routing: Default::default(),
        })
    };
    let store = FileAuthStore::new();
    store
        .upsert(
            "anthropic",
            login("account-A", "access-A"),
            CredentialOrigin::Login,
            true,
        )
        .await
        .unwrap();
    let first_id = store.snapshot().await.unwrap().rows("anthropic")[0].id;
    for (access, model) in [
        ("access-A", "claude-account-a"),
        ("access-B", "claude-account-b"),
    ] {
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", format!("Bearer {access}").as_str()))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"data": [{"id": model}], "has_more": false})),
            )
            .mount(&server)
            .await;
    }
    let cache = ModelCatalogCache::new();
    let first = init_provider_layer(&AppConfig::default());
    let target = first
        .discovery_targets
        .iter()
        .find(|t| t.provider_id == "anthropic")
        .unwrap();
    refresh_target(target, &cache, Some(&first.registry), true).await;
    assert_eq!(
        ids(first.registry.list_models_by_provider("anthropic")),
        ["claude-account-a"]
    );

    // The store is recreated and another account logs in; the cache survives.
    std::fs::remove_file(world.dir.path().join("auth.json")).unwrap();
    store
        .upsert(
            "anthropic",
            login("account-B", "access-B"),
            CredentialOrigin::Login,
            true,
        )
        .await
        .unwrap();
    assert_eq!(
        store.snapshot().await.unwrap().rows("anthropic")[0].id,
        first_id,
        "precondition: the new account reuses the old row id"
    );
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", "Bearer access-B"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data": [{"id": "claude-account-b"}], "has_more": false})),
        )
        .mount(&server)
        .await;
    let second = init_provider_layer(&AppConfig::default());
    assert!(
        !ids(second.registry.list_models_by_provider("anthropic"))
            .contains(&"claude-account-a".to_owned()),
        "startup must not seed account A's roster for account B"
    );
    let target = second
        .discovery_targets
        .iter()
        .find(|t| t.provider_id == "anthropic")
        .unwrap();
    refresh_target(target, &cache, Some(&second.registry), false).await;
    assert_eq!(
        ids(second.registry.list_models_by_provider("anthropic")),
        ["claude-account-b"]
    );
    let discovery: Vec<_> = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path() == "/v1/models")
        .collect();
    assert_eq!(discovery.len(), 1);
    assert_eq!(discovery[0].headers["authorization"], "Bearer access-B");
}

/// A live roster whose cache write fails stays authoritative in this process:
/// a later models.dev remerge never resurrects the older disk roster.
#[tokio::test]
#[serial]
async fn failed_cache_write_never_lets_remerge_resurrect_an_older_roster() {
    use rustain::infrastructure::{model_discovery::remerge_all, runtime::event_bus::EventBus};
    let world = World::new();
    world.set("OPENAI_API_KEY", "cache-io-key");
    let server = MockServer::start().await;
    let cfg = remote_config("openai", format!("{}/v1", server.uri()));
    let layer = init_provider_layer(&cfg);
    let target = &layer.discovery_targets[0];
    let cache = ModelCatalogCache::new();
    models(&server, 200, json!([{"id":"gpt-old"}])).await;
    refresh_target(target, &cache, Some(&layer.registry), true).await;
    // Every later write fails (the sidecar lock cannot be opened); the old
    // authoritative entry stays readable on disk.
    let lock = world.dir.path().join("models_cache.json.lock");
    std::fs::remove_file(&lock).unwrap();
    std::fs::create_dir(&lock).unwrap();
    models(&server, 200, json!([{"id":"gpt-new"}])).await;
    assert_eq!(
        refresh_target(target, &cache, Some(&layer.registry), true)
            .await
            .status,
        DiscoveryStatus::Ok
    );
    assert_eq!(
        cache.load().await.providers["openai"].models[0].model_id,
        "gpt-old",
        "the write must have failed for this regression to mean anything"
    );
    let (bus, _events) = EventBus::new(64);
    remerge_all(
        &layer.registry,
        &layer.discovery_targets,
        &std::sync::Arc::new(bus),
        &cfg,
    )
    .await;
    assert_eq!(
        ids(layer.registry.list_models_by_provider("openai")),
        ["gpt-new"]
    );
}
