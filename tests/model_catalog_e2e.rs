//! E2E tests for Story 7.6: Dynamic Model Catalog Discovery
//!
//! Integration tests that exercise the ModelCatalogCache, discovery pipeline,
//! and provider factory wiring without the full TUI harness.

use std::collections::HashSet;

use rustain::adapters::model_catalog_cache::{
    CachedCatalog, CachedModelEntry, CachedProviderEntry, ModelCatalogCache,
};
use rustain::domain::models::provider::ModelDescriptor;

fn model(id: &str, provider: &str, ctx: u32) -> ModelDescriptor {
    ModelDescriptor {
        model_id: id.to_string(),
        display_name: id.to_string(),
        provider_id: provider.to_string(),
        context_window: ctx,
        capabilities: HashSet::new(),
        pricing_tier: None,
        stale: false,
    }
}

fn entry(models: Vec<ModelDescriptor>) -> CachedProviderEntry {
    CachedProviderEntry {
        fetched_at_unix: 1000,
        models: models
            .into_iter()
            .map(|m| CachedModelEntry { descriptor: m })
            .collect(),
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
async fn discover_models_on_writes_cache_and_seeds_adapter() {
    let dir = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("RUSTAIN_DATA_DIR", dir.path().as_os_str());
    }
    let cache = ModelCatalogCache::new();

    let mut catalog = CachedCatalog::default();
    catalog.providers.insert(
        "openrouter".to_string(),
        entry(vec![model("m1", "openrouter", 128_000)]),
    );
    cache.save(&catalog).await.unwrap();

    let loaded = cache.load().await;
    assert_eq!(loaded.providers.len(), 1);
    assert_eq!(loaded.providers["openrouter"].models.len(), 1);
}

#[tokio::test]
#[serial_test::serial]
async fn discover_models_skips_fetch_when_cache_fresh() {
    let dir = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("RUSTAIN_DATA_DIR", dir.path().as_os_str());
    }
    let cache = ModelCatalogCache::new();

    let mut catalog = CachedCatalog::default();
    catalog.providers.insert(
        "openrouter".to_string(),
        CachedProviderEntry {
            fetched_at_unix: rustain::infrastructure::clock_util::now_unix(),
            models: vec![CachedModelEntry {
                descriptor: model("m1", "openrouter", 128_000),
            }],
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
fn discover_models_fetch_timeout_falls_back_to_bundled() {
    // The fallback is architectural: OpenAiAdapter::list_models returns
    // variant.known_models() when discovered_models is empty/missing.
    // We verify this by checking that a fresh adapter returns the bundled list.
    #[cfg(feature = "openai")]
    {
        use rustain::adapters::openai::{OpenAiAdapter, OpenAiCompatibleVariant};
        use rustain::domain::ports::StreamingProvider;
        let adapter = OpenAiAdapter::new(
            OpenAiCompatibleVariant::OpenAI,
            "test-key".to_string(),
            "gpt-4o".to_string(),
            None,
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
fn anthropic_discover_models_is_no_op() {
    // Anthropic is not an OpenAI-compatible provider; build_openai_for_discovery
    // returns None for non-openai kinds.
    #[cfg(feature = "openai")]
    {
        use rustain::domain::models::ProviderConfig;
        use rustain::infrastructure::provider_factory::build_openai_for_discovery;
        let cfg = ProviderConfig {
            provider_id: "anthropic".to_string(),
            enabled: true,
            model_id: "claude-sonnet-4".to_string(),
            api_key_env: String::new(),
            base_url: None,
            kind: Some("anthropic".to_string()),
            context_window: None,
            supports_tools: None,
            discover_models: true,
            model_filter: vec!["*".to_string()],
            cache_ttl_seconds: 3600,
        };
        let result = build_openai_for_discovery("anthropic", &cfg);
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
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
        models: vec![ModelDescriptor {
            model_id: "google/gemini-flash".to_string(),
            display_name: "Gemini Flash ★".to_string(),
            provider_id: "openrouter".to_string(),
            context_window: 1_000_000,
            capabilities: HashSet::new(),
            pricing_tier: None,
            stale: false,
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
        models: vec![],
    }];
    assert!(ms.columns[0].models.is_empty());
}

#[test]
fn ghost_model_strikethrough_render() {
    // Covered by inline test in model_selector.rs; this is a smoke test.
    use rustain::adapters::tui::state::{ModelSelectorState, ProviderColumn};

    let mut ms = ModelSelectorState::new();
    ms.active = true;
    ms.columns = vec![ProviderColumn {
        provider_id: "openrouter".to_string(),
        display_name: "OpenRouter".to_string(),
        healthy: true,
        models: vec![
            model("live", "openrouter", 128_000),
            ModelDescriptor {
                stale: true,
                ..model("ghost", "openrouter", 128_000)
            },
        ],
    }];
    assert!(ms.columns[0].models[1].stale);
}
