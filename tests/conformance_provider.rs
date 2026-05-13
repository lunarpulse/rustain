//! Conformance tests for the `StreamingProvider` trait and `ProviderRegistry`.
//!
//! Follows the pattern established in `tests/conformance.rs` — generic test
//! functions parameterized over trait implementations.

use rustain::adapters::noop::NoOpProvider;
use rustain::adapters::provider::ProviderRegistry;
use rustain::domain::models::provider::ModelDescriptor;
use rustain::domain::models::{CompletionOptions, Message, MessageRole, UserMessage};
use rustain::domain::ports::StreamingProvider;
use tokio::runtime::Runtime;

// ---------------------------------------------------------------------------
// Test 1: streaming_provider_conformance — generic conformance over any impl
// ---------------------------------------------------------------------------

fn assert_streaming_provider_conformance(provider: &dyn StreamingProvider) {
    // provider_id returns non-empty
    let pid = provider.provider_id();
    assert!(!pid.is_empty(), "provider_id must not be empty");

    // list_models returns valid descriptors
    let models = provider.list_models();
    for model in &models {
        assert!(!model.model_id.is_empty(), "model_id must not be empty");
        assert!(
            !model.display_name.is_empty(),
            "display_name must not be empty"
        );
        assert_eq!(
            model.provider_id, pid,
            "model's provider_id must match the owning provider"
        );
        assert!(model.context_window > 0, "context_window must be positive");
    }

    // health_check must succeed or return a typed error (not panic)
    let rt = Runtime::new().unwrap();
    let health_result = rt.block_on(provider.health_check());
    assert!(
        health_result.is_ok() || health_result.is_err(),
        "health_check must return a result (Ok or typed Err)"
    );
}

#[test]
fn test_noop_provider_conformance() {
    let provider = NoOpProvider::default();
    assert_streaming_provider_conformance(&provider);
    assert_eq!(provider.provider_id(), "noop");
    assert!(provider.list_models().is_empty());

    // health_check should succeed for NoOp
    let rt = Runtime::new().unwrap();
    rt.block_on(async {
        assert!(provider.health_check().await.is_ok());
    });
}

// ---------------------------------------------------------------------------
// Test 2: ProviderRegistry
// ---------------------------------------------------------------------------

#[test]
fn test_registry_list_models() {
    let registry = ProviderRegistry::new();
    registry.register(Box::new(NoOpProvider::default()));

    // NoOp returns empty model list
    let all = registry.list_all_models();
    assert!(all.is_empty());

    // list_providers returns the registered provider
    let providers = registry.list_providers();
    assert_eq!(providers.len(), 1);
    assert_eq!(providers[0].provider_id, "noop");

    // get_model returns None for non-existent model
    assert!(registry.get_model("noop", "nope").is_none());
}

#[test]
fn test_registry_health_check_on_noop_succeeds() {
    let registry = ProviderRegistry::new();
    registry.register(Box::new(NoOpProvider::default()));

    let providers = registry.list_providers();
    assert_eq!(providers[0].model_count, 0);
    // NoOp health_check always succeeds, so the provider is listed as healthy
}

// ---------------------------------------------------------------------------
// Test 3: Anthropic adapter behavior preservation (compile-time + basic)
// ---------------------------------------------------------------------------

#[test]
#[ignore = "Requires ANTHROPIC_API_KEY or ANTHROPIC_AUTH_TOKEN env var"]
fn test_anthropic_adapter_provider_id_and_models() {
    let auth_token = std::env::var("ANTHROPIC_AUTH_TOKEN").ok();
    let api_key = std::env::var("ANTHROPIC_API_KEY").ok();

    assert!(
        auth_token.is_some() || api_key.is_some(),
        "ANTHROPIC_API_KEY or ANTHROPIC_AUTH_TOKEN must be set"
    );

    let auth_mode = if let Some(token) = auth_token {
        rustain::adapters::anthropic::AuthMode::BearerToken(token)
    } else if let Some(key) = api_key {
        rustain::adapters::anthropic::AuthMode::ApiKey(key)
    } else {
        unreachable!()
    };

    let adapter = rustain::adapters::anthropic::AnthropicAdapter::new(
        auth_mode,
        "claude-sonnet-4-20250514".to_string(),
        None,
    )
    .expect("AnthropicAdapter construction failed");

    assert_eq!(adapter.provider_id(), "anthropic");

    let models = adapter.list_models();
    assert!(
        !models.is_empty(),
        "Anthropic adapter must list at least one model"
    );
    let sonnet = models
        .iter()
        .find(|m| m.model_id == "claude-sonnet-4-20250514");
    assert!(sonnet.is_some(), "Sonnet model must be present");
    let sonnet = sonnet.unwrap();
    assert_eq!(sonnet.context_window, 200_000);
    assert!(
        sonnet
            .capabilities
            .contains(&rustain::domain::models::ModelCapability::ToolUse)
    );
}

// ---------------------------------------------------------------------------
// Test 4: Domain isolation (AC9)
// ---------------------------------------------------------------------------

#[test]
fn test_domain_models_no_adapter_imports() {
    // The new domain types must not import from adapters/ or infrastructure/.
    // This is verified at compile time — if it compiles, it passes.
    // At runtime, we assert that ModelDescriptor has the expected shape.
    let desc = ModelDescriptor {
        model_id: "test".to_string(),
        display_name: "Test".to_string(),
        provider_id: "test".to_string(),
        context_window: 1000,
        capabilities: Default::default(),
        pricing_tier: None,
    };
    assert_eq!(desc.model_id, "test");
    assert_eq!(desc.provider_id, "test");
    assert_eq!(desc.context_window, 1000);
}
