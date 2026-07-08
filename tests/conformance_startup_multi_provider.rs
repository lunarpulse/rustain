#![allow(clippy::field_reassign_with_default, dead_code)] // AI-12.1: test setup + scaffold
//! Conformance tests for multi-provider startup wiring.
//!
//! Tests the `init_provider_layer` extraction from `startup.rs`.

use std::collections::BTreeMap;

use async_trait::async_trait;
use futures::stream::BoxStream;

use rustain::domain::errors::ProviderError;
use rustain::domain::models::provider::{ModelCapability, ModelDescriptor};
use rustain::domain::models::{AppConfig, CompletionOptions, Message, ProviderConfig, StreamChunk};
use rustain::domain::ports::StreamingProvider;
use rustain::infrastructure::startup::{ProviderLayer, init_provider_layer};

#[test]
#[cfg(feature = "ollama")]
fn test_startup_registers_configured_providers() {
    let mut config = AppConfig::default();
    config.provider = BTreeMap::from([
        (
            "anthropic".to_string(),
            ProviderConfig {
                provider_id: "anthropic".to_string(),
                model_id: "claude-sonnet-4-20250514".to_string(),
                api_key_env: "ANTHROPIC_API_KEY".to_string(),
                enabled: true,
                kind: None,
                base_url: None,
                context_window: None,
                supports_tools: None,
                discover_models: false,
                model_filter: vec!["*".to_string()],
                cache_ttl_seconds: 3600,
            },
        ),
        (
            "ollama".to_string(),
            ProviderConfig {
                provider_id: "ollama".to_string(),
                model_id: "llama3.3:70b".to_string(),
                api_key_env: "".to_string(),
                enabled: true,
                kind: None,
                base_url: None,
                context_window: None,
                supports_tools: None,
                discover_models: false,
                model_filter: vec!["*".to_string()],
                cache_ttl_seconds: 3600,
            },
        ),
        (
            "deepseek".to_string(),
            ProviderConfig {
                provider_id: "deepseek".to_string(),
                model_id: "deepseek-chat".to_string(),
                api_key_env: "DEEPSEEK_API_KEY".to_string(),
                enabled: false,
                kind: None,
                base_url: None,
                context_window: None,
                supports_tools: None,
                discover_models: false,
                model_filter: vec!["*".to_string()],
                cache_ttl_seconds: 3600,
            },
        ),
    ]);

    let ProviderLayer {
        router,
        registry,
        deferred_notices: deferred,
        active_id,
        ..
    } = init_provider_layer(&config);

    // Only enabled providers that successfully construct are registered
    let ids = registry.provider_ids();
    assert!(
        ids.contains("ollama"),
        "ollama should be registered (no auth required)"
    );
    assert!(
        !ids.contains("deepseek"),
        "deepseek is disabled and should NOT be registered"
    );

    // Anthropic registration depends on env key presence
    let has_anthropic_key =
        std::env::var("ANTHROPIC_API_KEY").is_ok() || std::env::var("ANTHROPIC_AUTH_TOKEN").is_ok();

    if has_anthropic_key {
        assert!(
            ids.contains("anthropic"),
            "anthropic should be registered when env key present"
        );
        assert_eq!(active_id, "anthropic");
        assert_eq!(router.active_delegate_id(), "anthropic");
        assert!(deferred.is_empty());
    } else {
        // Anthropic fails construction — deferred captures it, ollama becomes active
        assert!(
            !ids.contains("anthropic"),
            "anthropic should NOT be registered when no env key"
        );
        assert_eq!(active_id, "ollama");
        assert_eq!(router.active_delegate_id(), "ollama");
        assert!(
            deferred.iter().any(|(id, _)| id == "anthropic"),
            "anthropic construction failure should be deferred"
        );
    }
}

#[test]
fn test_startup_legacy_fallback_when_provider_map_empty() {
    let config = AppConfig::default();
    assert!(config.provider.is_empty());

    let ProviderLayer {
        router: _,
        registry,
        deferred_notices: deferred,
        active_id,
        ..
    } = init_provider_layer(&config);

    // Should fall back to legacy Anthropic path
    let ids = registry.provider_ids();

    // If ANTHROPIC_API_KEY or ANTHROPIC_AUTH_TOKEN is set, anthropic is registered.
    // Otherwise, the fallback fails and deferred contains the failure.
    let has_anthropic_key =
        std::env::var("ANTHROPIC_API_KEY").is_ok() || std::env::var("ANTHROPIC_AUTH_TOKEN").is_ok();

    if has_anthropic_key {
        assert!(
            ids.contains("anthropic"),
            "anthropic should be registered when env key present"
        );
        assert_eq!(active_id, "anthropic");
        assert!(deferred.is_empty(), "no failures when key is present");
    } else {
        // Fallback fails — deferred should contain the failure
        assert!(
            !deferred.is_empty(),
            "fallback should fail when no anthropic key set"
        );
        assert_eq!(deferred[0].0, "anthropic");
    }
}

struct FailingHealthCheckProvider {
    id: String,
}

#[async_trait]
impl StreamingProvider for FailingHealthCheckProvider {
    fn provider_id(&self) -> String {
        self.id.clone()
    }

    fn list_models(&self) -> Vec<ModelDescriptor> {
        vec![ModelDescriptor {
            model_id: "test-model".to_string(),
            display_name: "Test Model".to_string(),
            provider_id: self.id.clone(),
            context_window: 4096,
            capabilities: std::collections::HashSet::from([ModelCapability::ToolUse]),
            pricing_tier: None,
            stale: false,
        }]
    }

    async fn health_check(&self) -> Result<(), ProviderError> {
        Err(ProviderError::ConnectionFailed(
            "intentionally unreachable".to_string(),
        ))
    }

    async fn connectivity_probe(
        &self,
    ) -> Result<rustain::domain::ports::ProbeOutcome, rustain::domain::errors::ProviderError> {
        Ok(rustain::domain::ports::ProbeOutcome {
            latency: std::time::Duration::ZERO,
        })
    }

    async fn stream_completion(
        &self,
        _messages: Vec<Message>,
        _options: CompletionOptions,
    ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
        unimplemented!()
    }

    async fn abort(&self) -> Result<(), ProviderError> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Story 7.3: kind routing + openai-compatible builder tests
// ---------------------------------------------------------------------------

#[test]
#[cfg(feature = "openai")]
fn test_factory_routes_kind_openai_compatible() {
    use rustain::infrastructure::provider_factory::build_provider_for_config;

    let cfg = ProviderConfig {
        provider_id: "my-llamacpp".to_string(),
        model_id: "qwen2.5-coder".to_string(),
        api_key_env: "".to_string(),
        enabled: true,
        kind: Some("openai-compatible".to_string()),
        base_url: Some("http://localhost:8080/v1".to_string().into()),
        context_window: Some(32_768),
        supports_tools: Some(true),
        discover_models: false,
        model_filter: vec!["*".to_string()],
        cache_ttl_seconds: 3600,
    };

    let provider = build_provider_for_config("my-llamacpp", &cfg).unwrap();
    assert_eq!(provider.provider_id(), "my-llamacpp");

    let models = provider.list_models();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].context_window, 32_768);
}

#[test]
#[cfg(feature = "openai")]
fn test_factory_openai_compatible_requires_base_url() {
    use rustain::domain::errors::ProviderError;
    use rustain::infrastructure::provider_factory::build_provider_for_config;

    let cfg = ProviderConfig {
        provider_id: "my-llamacpp".to_string(),
        model_id: "qwen2.5-coder".to_string(),
        api_key_env: "".to_string(),
        enabled: true,
        kind: Some("openai-compatible".to_string()),
        base_url: None,
        context_window: Some(32_768),
        supports_tools: Some(true),
        discover_models: false,
        model_filter: vec!["*".to_string()],
        cache_ttl_seconds: 3600,
    };

    let result = build_provider_for_config("my-llamacpp", &cfg);
    assert!(result.is_err());
    let err = match result {
        Err(e) => e,
        Ok(_) => panic!("Expected error"),
    };
    match err {
        ProviderError::Other(msg) => {
            assert!(msg.contains("requires a base_url"));
        }
        other => panic!("Expected ProviderError::Other, got {:?}", other),
    }
}

#[test]
#[cfg(feature = "ollama")]
fn test_factory_kind_absent_uses_provider_id() {
    use rustain::infrastructure::provider_factory::build_provider_for_config;

    let cfg = ProviderConfig {
        provider_id: "ollama".to_string(),
        model_id: "llama3.3:70b".to_string(),
        api_key_env: "".to_string(),
        enabled: true,
        kind: None,
        base_url: Some("http://localhost:11434".to_string().into()),
        context_window: None,
        supports_tools: None,
        discover_models: false,
        model_filter: vec!["*".to_string()],
        cache_ttl_seconds: 3600,
    };

    let provider = build_provider_for_config("ollama", &cfg).unwrap();
    assert_eq!(provider.provider_id(), "ollama");
}
