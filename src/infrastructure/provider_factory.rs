//! Provider factory — constructs `Arc<dyn StreamingProvider>` from `ProviderConfig`.
//!
//! Used by `startup.rs` to build adapters from the `[provider]` config map.

use std::sync::{Arc, Mutex};

use crate::domain::errors::ProviderError;
use crate::domain::models::ProviderConfig;
use crate::domain::ports::StreamingProvider;

/// Shared cache of built OpenAI-compatible adapters keyed by config key (provider_id).
/// Used so that `build_openai_for_discovery` can return the same `Arc`
/// that was already created during provider layer init, ensuring discovery
/// updates are visible to the ProviderRegistry.
#[cfg(feature = "openai")]
static OPENAI_ADAPTERS: std::sync::LazyLock<Mutex<std::collections::HashMap<String, Arc<crate::adapters::openai::OpenAiAdapter>>>> =
    std::sync::LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

#[cfg(feature = "openai")]
pub fn get_openai_adapter(provider_id: &str) -> Option<Arc<crate::adapters::openai::OpenAiAdapter>> {
    OPENAI_ADAPTERS.lock().unwrap().get(provider_id).cloned()
}

#[cfg(feature = "openai")]
pub fn clear_openai_adapters() {
    OPENAI_ADAPTERS.lock().unwrap().clear();
}

/// Build a provider adapter from a `ProviderConfig` entry.
///
/// Matches on `provider_id` to decide which concrete adapter to construct.
/// Reads the API key from the env var named by `cfg.api_key_env`.
pub fn build_provider_for_config(
    provider_id: &str,
    cfg: &ProviderConfig,
) -> Result<Arc<dyn StreamingProvider>, ProviderError> {
    let kind = cfg.kind.as_deref().unwrap_or(provider_id);
    match kind {
        "anthropic" => build_anthropic_from_config(cfg),
        "openai" | "openrouter" | "google" | "deepseek" | "moonshot" => {
            #[cfg(feature = "openai")]
            {
                build_openai_from_config(provider_id, cfg)
            }
            #[cfg(not(feature = "openai"))]
            {
                Err(ProviderError::Other(
                    "openai feature not enabled — rebuild with --features openai".to_string(),
                ))
            }
        }
        "ollama" => {
            #[cfg(feature = "ollama")]
            {
                build_ollama_from_config(cfg)
            }
            #[cfg(not(feature = "ollama"))]
            {
                Err(ProviderError::Other(
                    "ollama feature not enabled — rebuild with --features ollama".to_string(),
                ))
            }
        }
        "openai-compatible" => {
            #[cfg(feature = "openai")]
            {
                build_openai_compatible_from_config(provider_id, cfg)
            }
            #[cfg(not(feature = "openai"))]
            {
                Err(ProviderError::Other(
                    "openai feature not enabled — rebuild with --features openai".to_string(),
                ))
            }
        }
        _ => Err(ProviderError::Other(format!(
            "unknown provider kind '{}' for '{}'",
            kind, provider_id
        ))),
    }
}

fn build_anthropic_from_config(
    cfg: &ProviderConfig,
) -> Result<Arc<dyn StreamingProvider>, ProviderError> {
    #[cfg(feature = "anthropic")]
    {
        use crate::adapters::anthropic::{AnthropicAdapter, AuthMode};

        let api_key = if cfg.api_key_env.is_empty() {
            None
        } else {
            crate::infrastructure::utils::env_var_trimmed(&cfg.api_key_env)
        };

        let auth_mode = match api_key {
            Some(key) if cfg.api_key_env.contains("API_KEY") => AuthMode::ApiKey(key),
            Some(token) => AuthMode::BearerToken(token),
            None => {
                return Err(ProviderError::AuthenticationFailed);
            }
        };

        let adapter =
            AnthropicAdapter::new(auth_mode, cfg.model_id.clone(), None).map_err(|e| {
                ProviderError::Other(format!("Failed to create Anthropic adapter: {}", e))
            })?;
        Ok(Arc::new(adapter))
    }
    #[cfg(not(feature = "anthropic"))]
    {
        Err(ProviderError::Other(
            "anthropic feature not enabled".to_string(),
        ))
    }
}

#[cfg(feature = "openai")]
fn build_openai_from_config(
    provider_id: &str,
    cfg: &ProviderConfig,
) -> Result<Arc<dyn StreamingProvider>, ProviderError> {
    use crate::adapters::openai::{OpenAiAdapter, OpenAiCompatibleVariant};

    let kind = cfg.kind.as_deref().unwrap_or(provider_id);
    let api_key = if cfg.api_key_env.is_empty() {
        String::new()
    } else {
        crate::infrastructure::utils::env_var_trimmed(&cfg.api_key_env).unwrap_or_default()
    };

    let variant = match kind {
        "openai" => OpenAiCompatibleVariant::OpenAI,
        "openrouter" => OpenAiCompatibleVariant::OpenRouter,
        "google" => OpenAiCompatibleVariant::Google,
        "deepseek" => OpenAiCompatibleVariant::DeepSeek,
        "moonshot" => OpenAiCompatibleVariant::Moonshot,
        _ => OpenAiCompatibleVariant::Custom {
            provider_id: provider_id.to_string(),
            display_name: provider_id.to_string(),
            context_window: None,
            supports_tools: None,
        },
    };

    let adapter = Arc::new(
        OpenAiAdapter::new(variant, api_key, cfg.model_id.clone(), cfg.base_url.clone())
            .map_err(|e| ProviderError::Other(format!("Failed to create OpenAI adapter: {}", e)))?,
    );
    OPENAI_ADAPTERS
        .lock()
        .unwrap()
        .insert(provider_id.to_string(), Arc::clone(&adapter));
    Ok(adapter)
}

#[cfg(feature = "openai")]
fn build_openai_compatible_from_config(
    provider_id: &str,
    cfg: &ProviderConfig,
) -> Result<Arc<dyn StreamingProvider>, ProviderError> {
    use crate::adapters::openai::{OpenAiAdapter, OpenAiCompatibleVariant};

    let base_url = cfg.base_url.clone().ok_or_else(|| {
        ProviderError::Other(format!(
            "openai-compatible provider '{}' requires a base_url",
            cfg.provider_id
        ))
    })?;

    let api_key = if cfg.api_key_env.is_empty() {
        String::new()
    } else {
        crate::infrastructure::utils::env_var_trimmed(&cfg.api_key_env).unwrap_or_default()
    };

    let variant = OpenAiCompatibleVariant::Custom {
        provider_id: cfg.provider_id.clone(),
        display_name: cfg.provider_id.clone(),
        context_window: cfg.context_window,
        supports_tools: cfg.supports_tools,
    };

    let adapter = Arc::new(
        OpenAiAdapter::new(variant, api_key, cfg.model_id.clone(), Some(base_url))
            .map_err(|e| ProviderError::Other(format!("Failed to create OpenAI adapter: {}", e)))?,
    );
    OPENAI_ADAPTERS
        .lock()
        .unwrap()
        .insert(provider_id.to_string(), Arc::clone(&adapter));
    Ok(adapter)
}

/// Build a typed `OpenAiAdapter` for discovery purposes.
/// Returns `Some(Arc<OpenAiAdapter>)` for OpenAI-compatible kinds,
/// `None` for non-OpenAI kinds (Anthropic, Ollama).
///
/// MUST stay kind-list-synced with `build_provider_for_config` — see Story 7.6 AC5.
#[cfg(feature = "openai")]
pub fn build_openai_for_discovery(
    provider_id: &str,
    cfg: &ProviderConfig,
) -> Result<Option<Arc<crate::adapters::openai::OpenAiAdapter>>, ProviderError> {
    let kind = cfg.kind.as_deref().unwrap_or(provider_id);
    match kind {
        "openai" | "openrouter" | "google" | "deepseek" | "moonshot" | "openai-compatible" => {
            // Reuse the production adapter if one was already built for this provider_id.
            if let Some(adapter) = get_openai_adapter(provider_id) {
                return Ok(Some(adapter));
            }
            // Fallback: build a fresh adapter (e.g., when called from tests).
            use crate::adapters::openai::{OpenAiAdapter, OpenAiCompatibleVariant};
            let api_key = if cfg.api_key_env.is_empty() {
                String::new()
            } else {
                crate::infrastructure::utils::env_var_trimmed(&cfg.api_key_env).unwrap_or_default()
            };
            let variant = match kind {
                "openai" => OpenAiCompatibleVariant::OpenAI,
                "openrouter" => OpenAiCompatibleVariant::OpenRouter,
                "google" => OpenAiCompatibleVariant::Google,
                "deepseek" => OpenAiCompatibleVariant::DeepSeek,
                "moonshot" => OpenAiCompatibleVariant::Moonshot,
                "openai-compatible" => OpenAiCompatibleVariant::Custom {
                    provider_id: cfg.provider_id.clone(),
                    display_name: cfg.provider_id.clone(),
                    context_window: cfg.context_window,
                    supports_tools: cfg.supports_tools,
                },
                _ => unreachable!(),
            };
            let base_url = cfg.base_url.clone();
            let adapter = Arc::new(
                OpenAiAdapter::new(variant, api_key, cfg.model_id.clone(), base_url)
                    .map_err(|e| ProviderError::Other(format!("Failed to create OpenAI adapter: {}", e)))?,
            );
            OPENAI_ADAPTERS
                .lock()
                .unwrap()
                .insert(provider_id.to_string(), Arc::clone(&adapter));
            Ok(Some(adapter))
        }
        _ => Ok(None),
    }
}

#[cfg(not(feature = "openai"))]
pub fn build_openai_for_discovery(
    _provider_id: &str,
    _cfg: &ProviderConfig,
) -> Result<Option<()>, ProviderError> {
    Ok(None)
}

#[cfg(feature = "ollama")]
fn build_ollama_from_config(
    cfg: &ProviderConfig,
) -> Result<Arc<dyn StreamingProvider>, ProviderError> {
    use crate::adapters::ollama::OllamaAdapter;

    if !cfg.api_key_env.is_empty() {
        tracing::debug!(
            "Ollama provider ignores api_key_env '{}' — no authentication required",
            cfg.api_key_env
        );
    }

    let adapter = OllamaAdapter::new(cfg.model_id.clone(), cfg.base_url.clone())
        .map_err(|e| ProviderError::Other(format!("Failed to create Ollama adapter: {}", e)))?;
    Ok(Arc::new(adapter))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(feature = "openai")]
    fn build_openai_for_discovery_kind_list_matches() {
        unsafe { std::env::set_var("RUSTAIN_TEST_KEY", "sk-test-dummy-key"); }
        let known_kinds = ["openai", "openrouter", "google", "deepseek", "moonshot", "openai-compatible"];
        for kind in known_kinds {
            let cfg = ProviderConfig {
                provider_id: kind.to_string(),
                enabled: true,
                model_id: "test".to_string(),
                api_key_env: "RUSTAIN_TEST_KEY".to_string(),
                base_url: Some("http://localhost".to_string()),
                kind: Some(kind.to_string()),
                context_window: None,
                supports_tools: None,
                discover_models: false,
                model_filter: vec!["*".to_string()],
                cache_ttl_seconds: 3600,

            };
            let result = build_openai_for_discovery(kind, &cfg);
            assert!(
                result.is_ok() && result.unwrap().is_some(),
                "kind='{}' should produce a discovery adapter",
                kind
            );
        }

        // Unknown kinds return None (not an error)
        let unknown_kinds = ["anthropic", "ollama", "foo-bar"];
        for kind in unknown_kinds {
            let cfg = ProviderConfig {
                provider_id: kind.to_string(),
                enabled: true,
                model_id: "test".to_string(),
                api_key_env: "RUSTAIN_TEST_KEY".to_string(),
                base_url: Some("http://localhost".to_string()),
                kind: Some(kind.to_string()),
                context_window: None,
                supports_tools: None,
                discover_models: false,
                model_filter: vec!["*".to_string()],
                cache_ttl_seconds: 3600,

            };
            let result = build_openai_for_discovery(kind, &cfg);
            assert!(
                result.is_ok() && result.unwrap().is_none(),
                "kind='{}' should return None for unsupported discovery",
                kind
            );
        }
    }
}
