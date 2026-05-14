//! Conformance tests for multi-provider startup wiring.
//!
//! Tests the `init_provider_layer` extraction from `startup.rs`.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;

use rustain::domain::errors::ProviderError;
use rustain::domain::models::provider::{ModelCapability, ModelDescriptor};
use rustain::domain::models::{AppConfig, CompletionOptions, Message, ProviderConfig, StreamChunk};
use rustain::domain::ports::StreamingProvider;
use rustain::infrastructure::startup::init_provider_layer;

#[test]
#[cfg(feature = "ollama")]
fn test_startup_registers_configured_providers() {
    let mut config = AppConfig::default();
    config.provider = HashMap::from([
        (
            "anthropic".to_string(),
            ProviderConfig {
                provider_id: "anthropic".to_string(),
                model_id: "claude-sonnet-4-20250514".to_string(),
                api_key_env: "ANTHROPIC_API_KEY".to_string(),
                enabled: true,
            },
        ),
        (
            "ollama".to_string(),
            ProviderConfig {
                provider_id: "ollama".to_string(),
                model_id: "llama3.3:70b".to_string(),
                api_key_env: "".to_string(),
                enabled: true,
            },
        ),
        (
            "deepseek".to_string(),
            ProviderConfig {
                provider_id: "deepseek".to_string(),
                model_id: "deepseek-chat".to_string(),
                api_key_env: "DEEPSEEK_API_KEY".to_string(),
                enabled: false,
            },
        ),
    ]);

    let (router, registry, deferred, active_id) = init_provider_layer(&config);

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

    let (router, registry, deferred, active_id) = init_provider_layer(&config);

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
        }]
    }

    async fn health_check(&self) -> Result<(), ProviderError> {
        Err(ProviderError::ConnectionFailed(
            "intentionally unreachable".to_string(),
        ))
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
