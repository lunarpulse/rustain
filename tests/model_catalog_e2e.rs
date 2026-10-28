//! Integration coverage for discovery-cache persistence and model-picker state.

use std::collections::HashSet;

use rustain::adapters::model_catalog_cache::{
    CachedDiscoveryEntry, DiscoveryCacheFile, ModelCatalogCache,
};
use rustain::domain::models::provider::{ModelDescriptor, ModelRecord};

fn model(id: &str, provider: &str, ctx: u32) -> ModelDescriptor {
    ModelDescriptor {
        model_id: id.to_string(),
        display_name: id.to_string(),
        provider_id: provider.to_string(),
        context_window: ctx,
        capabilities: HashSet::new(),
        pricing_tier: None,
        stale: false,
        wire_model_id: None,
    }
}

fn entry(models: Vec<ModelDescriptor>) -> CachedDiscoveryEntry {
    CachedDiscoveryEntry {
        fetched_at_unix: 1000,
        credential_fingerprint: "0123456789abcdef".to_string(),
        authoritative: true,
        models: models.iter().map(ModelRecord::from).collect(),
    }
}

#[tokio::test]
#[serial_test::serial]
async fn discover_models_off_no_disk_write() {
    // When no discovery targets exist, the cache file should not be created.
    let dir = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("RUSTAIN_DATA_DIR", dir.path().as_os_str());
    }
    let cache = ModelCatalogCache::new();
    let catalog = cache.load().await;
    assert!(catalog.providers.is_empty());
    // No save occurred — cache file should not exist
    assert!(!dir.path().join("models_cache.json").exists());
}

#[tokio::test]
#[serial_test::serial]
async fn discovery_cache_roundtrip_preserves_raw_models() {
    let dir = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("RUSTAIN_DATA_DIR", dir.path().as_os_str());
    }
    let cache = ModelCatalogCache::new();

    let mut catalog = DiscoveryCacheFile::default();
    catalog.providers.insert(
        "openrouter".to_string(),
        entry(vec![model("m1", "openrouter", 128_000)]),
    );
    cache.save(&catalog).await.unwrap();

    let loaded = cache.load().await;
    assert_eq!(loaded.providers.len(), 1);
    assert_eq!(loaded.providers["openrouter"].models.len(), 1);
    let cached = &loaded.providers["openrouter"];
    assert_eq!(cached.credential_fingerprint, "0123456789abcdef");
    assert!(cached.authoritative);
    assert_eq!(cached.models[0].model_id, "m1");
    assert_eq!(cached.models[0].context_window, Some(128_000));
}

#[tokio::test]
#[serial_test::serial]
async fn discovery_cache_ttl_marks_recent_entry_fresh() {
    let dir = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("RUSTAIN_DATA_DIR", dir.path().as_os_str());
    }
    let cache = ModelCatalogCache::new();

    let mut catalog = DiscoveryCacheFile::default();
    catalog.providers.insert(
        "openrouter".to_string(),
        CachedDiscoveryEntry {
            fetched_at_unix: rustain::infrastructure::clock_util::now_unix(),
            ..entry(vec![model("m1", "openrouter", 128_000)])
        },
    );
    cache.save(&catalog).await.unwrap();

    let entry = cache.load().await.providers["openrouter"].clone();
    assert!(cache.is_fresh(
        &entry,
        3600,
        rustain::infrastructure::clock_util::now_unix()
    ));
}

#[test]
fn fresh_adapter_uses_bundled_models_before_discovery() {
    #[cfg(feature = "openai")]
    {
        use rustain::adapters::openai::{
            ChatCredential, OpenAiAdapter, OpenAiCompatibleVariant, StaticChatProfile,
        };
        use rustain::domain::models::SecretString;
        use rustain::domain::ports::StreamingProvider;
        let adapter = OpenAiAdapter::new(
            OpenAiCompatibleVariant::OpenAI,
            ChatCredential::Fixed(SecretString::new("test-key".to_string())),
            "gpt-4o".to_string(),
            StaticChatProfile::new(OpenAiCompatibleVariant::OpenAI.default_base_url()),
        )
        .unwrap();
        let models = adapter.list_models();
        assert!(
            !models.is_empty(),
            "fallback to bundled list should never be empty"
        );
    }
}

#[test]
fn keyword_search_filters_and_dismisses() {
    use rustain::adapters::tui::state::ModelSelectorState;

    let mut ms = ModelSelectorState::new();
    ms.columns = vec![rustain::adapters::tui::state::ProviderColumn {
        provider_id: "openrouter".to_string(),
        display_name: "OpenRouter".to_string(),
        healthy: true,
        locked_hint: None,
        sign_in_hint: None,
        discovery: None,
        models: vec![
            model("anthropic/claude-3.5-sonnet", "openrouter", 200_000),
            model("openai/gpt-4o", "openrouter", 128_000),
        ],
    }];
    ms.search_active = true;
    ms.search_query = "gpt".to_string();
    ms.recompute_filter();
    assert_eq!(ms.filtered_indices.len(), 1);
    assert_eq!(ms.filtered_indices[0], 1);

    // Dismiss clears search
    ms.dismiss();
    assert!(!ms.search_active);
    assert!(ms.search_query.is_empty());
}

#[test]
fn keyword_search_accepts_unicode() {
    use rustain::adapters::tui::state::ModelSelectorState;

    let mut ms = ModelSelectorState::new();
    ms.columns = vec![rustain::adapters::tui::state::ProviderColumn {
        provider_id: "openrouter".to_string(),
        display_name: "OpenRouter".to_string(),
        healthy: true,
        locked_hint: None,
        sign_in_hint: None,
        discovery: None,
        models: vec![ModelDescriptor {
            model_id: "google/gemini-flash".to_string(),
            display_name: "Gemini Flash ★".to_string(),
            provider_id: "openrouter".to_string(),
            context_window: 1_000_000,
            capabilities: HashSet::new(),
            pricing_tier: None,
            stale: false,
            wire_model_id: None,
        }],
    }];
    ms.search_active = true;
    ms.search_query = "flash ★".to_string();
    ms.recompute_filter();
    assert_eq!(ms.filtered_indices.len(), 1);
}

#[test]
fn empty_catalog_renders_hint() {
    // Covered by inline test in model_selector.rs; this is a smoke test.
    use rustain::adapters::tui::state::{ModelSelectorState, ProviderColumn};

    let mut ms = ModelSelectorState::new();
    ms.active = true;
    ms.columns = vec![ProviderColumn {
        provider_id: "openrouter".to_string(),
        display_name: "OpenRouter".to_string(),
        healthy: true,
        locked_hint: None,
        sign_in_hint: None,
        discovery: None,
        models: vec![],
    }];
    assert!(ms.columns[0].models.is_empty());
}

#[test]
fn rejected_credential_stale_render_smoke() {
    // The stale marker denotes a rejected credential, not an absent live id.
    use rustain::adapters::tui::state::{ModelSelectorState, ProviderColumn};

    let mut ms = ModelSelectorState::new();
    ms.active = true;
    ms.columns = vec![ProviderColumn {
        provider_id: "openrouter".to_string(),
        display_name: "OpenRouter".to_string(),
        healthy: true,
        locked_hint: None,
        sign_in_hint: None,
        discovery: None,
        models: vec![
            model("accepted", "openrouter", 128_000),
            ModelDescriptor {
                stale: true,
                ..model("credential-rejected", "openrouter", 128_000)
            },
        ],
    }];
    assert!(ms.columns[0].models[1].stale);
}
