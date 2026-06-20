//! Provider factory — constructs `Arc<dyn StreamingProvider>` from `ProviderConfig`.
//!
//! Used by `startup.rs` to build adapters from the `[provider]` config map.

use std::sync::{Arc, Mutex}; // CONFORMANCE_EXCEPTION_STD_SYNC_LOCK: Mutex guards static OPENAI_ADAPTERS cache, short critical sections, never across .await

use crate::domain::errors::ProviderError;
use crate::domain::models::ProviderConfig;
use crate::domain::ports::StreamingProvider;
#[cfg(any(test, feature = "test-instrumentation"))]
use std::sync::atomic::{AtomicUsize, Ordering};

/// Resolve auth for a provider: env var → auth.json fallback (Story 13.4a AC7).
///
/// Returns `Some(ResolvedAuth)` if the env var is set (trimmed, non-empty), OR if
/// `auth.json` has a stored credential for `provider_id`.  Env var always wins
/// (backward compatible).  Returns `None` only when neither source has a key.
///
/// Forward-compat scaffold: returns `ResolvedAuth` (today only `ApiKey`; Epic 19
/// adds `OAuth` which selects `Authorization: Bearer` + provider betas).
pub fn resolve_auth(
    api_key_env: &str,
    provider_id: &str,
) -> Option<crate::domain::models::credential::ResolvedAuth> {
    use crate::domain::models::SecretString;
    use crate::domain::models::credential::ResolvedAuth;
    if !api_key_env.is_empty() {
        if let Some(key) = crate::infrastructure::utils::env_var_trimmed(api_key_env) {
            return Some(ResolvedAuth::ApiKey(SecretString::new(key)));
        }
    }
    // Fallback: auth.json (Story 13.4a read-path integration).
    crate::adapters::auth_store::FileAuthStore::get_sync(provider_id)
        .map(|s| ResolvedAuth::ApiKey(SecretString::new(s)))
}

/// Convenience wrapper that unwraps `ResolvedAuth` to a plain `String` key.
/// Used by the OpenAI-compatible builders that take a bare `String`.
pub fn resolve_api_key(api_key_env: &str, provider_id: &str) -> Option<String> {
    resolve_auth(api_key_env, provider_id)
        .and_then(|a| a.to_api_key())
        .map(|s| s.expose_secret().to_owned())
}

/// Runtime call counter for provider construction — Story 13.2a P0-3 sentinel.
///
/// Incremented in `build_provider_for_config`. Integration tests assert this
/// is 0 after `config validate` (no provider constructed) and >0 after a path
/// that builds a provider (positive control proving the sentinel is armed).
#[cfg(any(test, feature = "test-instrumentation"))]
pub static PROVIDER_CTOR_COUNT: AtomicUsize = AtomicUsize::new(0);
/// Shared cache of built OpenAI-compatible adapters keyed by config key (provider_id).
/// Used so that `build_openai_for_discovery` can return the same `Arc`
/// that was already created during provider layer init, ensuring discovery
/// updates are visible to the ProviderRegistry.
#[cfg(feature = "openai")]
static OPENAI_ADAPTERS: std::sync::LazyLock<
    Mutex<std::collections::HashMap<String, Arc<crate::adapters::openai::OpenAiAdapter>>>,
> = std::sync::LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

#[cfg(feature = "openai")]
pub fn get_openai_adapter(
    provider_id: &str,
) -> Option<Arc<crate::adapters::openai::OpenAiAdapter>> {
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
    #[cfg(any(test, feature = "test-instrumentation"))]
    PROVIDER_CTOR_COUNT.fetch_add(1, Ordering::Relaxed);
    let kind = cfg.kind.as_deref().unwrap_or(provider_id);
    match kind {
        "anthropic" => {
            // Story 13.4a AC7: env var → auth.json → None.
            let api_key = if cfg.api_key_env.is_empty() {
                None
            } else {
                resolve_api_key(&cfg.api_key_env, &cfg.provider_id)
            };
            build_anthropic_from_config(cfg, api_key)
        }
        "openai" | "openrouter" | "google" | "deepseek" | "moonshot" => {
            #[cfg(feature = "openai")]
            {
                // Story 13.4a AC7: env var → auth.json → empty.
                let api_key = resolve_api_key(&cfg.api_key_env, provider_id).unwrap_or_default();
                let adapter = build_openai_from_config(provider_id, cfg, api_key)?;
                OPENAI_ADAPTERS
                    .lock()
                    .unwrap()
                    .insert(provider_id.to_string(), Arc::clone(&adapter));
                Ok(adapter)
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
                // Story 13.4a AC7: env var → auth.json → empty.
                let api_key = resolve_api_key(&cfg.api_key_env, provider_id).unwrap_or_default();
                let adapter = build_openai_compatible_from_config(provider_id, cfg, api_key)?;
                OPENAI_ADAPTERS
                    .lock()
                    .unwrap()
                    .insert(provider_id.to_string(), Arc::clone(&adapter));
                Ok(adapter)
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

/// Build a provider adapter with an **explicit** candidate key (Story 13.4a DN-1).
///
/// Used by `auth login`'s validation step to probe a candidate key WITHOUT
/// injecting it into the process environment (the old path did
/// `std::env::set_var`, which leaked the key via `/proc/self/environ` and was
/// unsafe under the multi-threaded tokio runtime). Same kind dispatch as
/// [`build_provider_for_config`]; the key is passed straight to the per-kind
/// builder instead of being resolved from env / `auth.json`. The built adapter
/// is NOT registered in the `OPENAI_ADAPTERS` cache (it is a throwaway probe).
pub fn build_provider_for_config_with_key(
    provider_id: &str,
    cfg: &ProviderConfig,
    key: &str,
) -> Result<Arc<dyn StreamingProvider>, ProviderError> {
    let kind = cfg.kind.as_deref().unwrap_or(provider_id);
    match kind {
        "anthropic" => build_anthropic_from_config(cfg, Some(key.to_string())),
        "openai" | "openrouter" | "google" | "deepseek" | "moonshot" => {
            #[cfg(feature = "openai")]
            {
                let adapter = build_openai_from_config(provider_id, cfg, key.to_string())?;
                Ok(adapter)
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
                let adapter =
                    build_openai_compatible_from_config(provider_id, cfg, key.to_string())?;
                Ok(adapter)
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
    api_key: Option<String>,
) -> Result<Arc<dyn StreamingProvider>, ProviderError> {
    #[cfg(feature = "anthropic")]
    {
        use crate::adapters::anthropic::{AnthropicAdapter, AuthMode};
        use crate::domain::models::SecretString;

        let auth_mode = match api_key {
            Some(key) if cfg.api_key_env.contains("API_KEY") => {
                AuthMode::ApiKey(SecretString::new(key))
            }
            Some(token) => AuthMode::BearerToken(SecretString::new(token)),
            None => {
                return Err(ProviderError::AuthenticationFailed);
            }
        };

        // Resolve base_url precedence: explicit config field > ANTHROPIC_BASE_URL
        // env var. The env var is the Claude-Code-compatible mechanism for
        // pointing at gateways/proxies (z.ai, LiteLLM, Helicone, etc.);
        // without this fallback, `[provider.anthropic]` config silently hits
        // the default api.anthropic.com endpoint and auth fails for gateway
        // tokens. The legacy env-var-only construction path
        // (`build_anthropic_provider_from_env`) does this correctly; this
        // brings the config-driven path to parity.
        let base_url = cfg
            .base_url
            .as_ref()
            .map(|u| u.expose_url().to_owned())
            .or_else(|| crate::infrastructure::utils::env_var_trimmed("ANTHROPIC_BASE_URL"));

        let adapter =
            AnthropicAdapter::new(auth_mode, cfg.model_id.clone(), base_url).map_err(|e| {
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
    api_key: String,
) -> Result<Arc<crate::adapters::openai::OpenAiAdapter>, ProviderError> {
    use crate::adapters::openai::{OpenAiAdapter, OpenAiCompatibleVariant};

    let kind = cfg.kind.as_deref().unwrap_or(provider_id);

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
        OpenAiAdapter::new(
            variant,
            api_key,
            cfg.model_id.clone(),
            cfg.base_url.as_ref().map(|u| u.expose_url().to_owned()),
        )
        .map_err(|e| ProviderError::Other(format!("Failed to create OpenAI adapter: {}", e)))?,
    );
    Ok(adapter)
}

#[cfg(feature = "openai")]
fn build_openai_compatible_from_config(
    _provider_id: &str,
    cfg: &ProviderConfig,
    api_key: String,
) -> Result<Arc<crate::adapters::openai::OpenAiAdapter>, ProviderError> {
    use crate::adapters::openai::{OpenAiAdapter, OpenAiCompatibleVariant};

    let base_url = cfg
        .base_url
        .as_ref()
        .map(|u| u.expose_url().to_owned())
        .ok_or_else(|| {
            ProviderError::Other(format!(
                "openai-compatible provider '{}' requires a base_url",
                cfg.provider_id
            ))
        })?;

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
            // Story 13.4a AC7/P-5: env var → auth.json → empty (was env-only).
            let api_key = if cfg.api_key_env.is_empty() {
                String::new()
            } else {
                resolve_api_key(&cfg.api_key_env, provider_id).unwrap_or_default()
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
            let base_url = cfg.base_url.as_ref().map(|u| u.expose_url().to_owned());
            let adapter = Arc::new(
                OpenAiAdapter::new(variant, api_key, cfg.model_id.clone(), base_url).map_err(
                    |e| ProviderError::Other(format!("Failed to create OpenAI adapter: {}", e)),
                )?,
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

    let adapter = OllamaAdapter::new(
        cfg.model_id.clone(),
        cfg.base_url.as_ref().map(|u| u.expose_url().to_owned()),
    )
    .map_err(|e| ProviderError::Other(format!("Failed to create Ollama adapter: {}", e)))?;
    Ok(Arc::new(adapter))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(feature = "openai")]
    fn build_openai_for_discovery_kind_list_matches() {
        unsafe {
            std::env::set_var("RUSTAIN_TEST_KEY", "sk-test-dummy-key");
        }
        let known_kinds = [
            "openai",
            "openrouter",
            "google",
            "deepseek",
            "moonshot",
            "openai-compatible",
        ];
        for kind in known_kinds {
            let cfg = ProviderConfig {
                provider_id: kind.to_string(),
                enabled: true,
                model_id: "test".to_string(),
                api_key_env: "RUSTAIN_TEST_KEY".to_string(),
                base_url: Some("http://localhost".to_string().into()),
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
                base_url: Some("http://localhost".to_string().into()),
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
