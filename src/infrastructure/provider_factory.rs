//! Provider factory — constructs `Arc<dyn StreamingProvider>` from `ProviderConfig`.
//!
//! Used by `startup.rs` to build adapters from the `[provider]` config map.

use std::sync::Arc;

use crate::domain::errors::ProviderError;
use crate::domain::models::ProviderConfig;
use crate::domain::ports::StreamingProvider;

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
                build_openai_from_config(kind, cfg)
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
                build_openai_compatible_from_config(cfg)
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

    let api_key = if cfg.api_key_env.is_empty() {
        String::new()
    } else {
        crate::infrastructure::utils::env_var_trimmed(&cfg.api_key_env).unwrap_or_default()
    };

    let variant = match provider_id {
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

    let adapter = OpenAiAdapter::new(variant, api_key, cfg.model_id.clone(), None)
        .map_err(|e| ProviderError::Other(format!("Failed to create OpenAI adapter: {}", e)))?;
    Ok(Arc::new(adapter))
}

#[cfg(feature = "openai")]
fn build_openai_compatible_from_config(
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

    let adapter = OpenAiAdapter::new(variant, api_key, cfg.model_id.clone(), Some(base_url))
        .map_err(|e| ProviderError::Other(format!("Failed to create OpenAI adapter: {}", e)))?;
    Ok(Arc::new(adapter))
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
