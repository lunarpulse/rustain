//! Provider factory — constructs `Arc<dyn StreamingProvider>` from `ProviderConfig`.
//!
//! Used by `startup.rs` to build adapters from the `[provider]` config map.

use std::sync::Arc;

use crate::domain::errors::ProviderError;
use crate::domain::models::ProviderConfig;
use crate::domain::ports::StreamingProvider;
#[cfg(any(test, feature = "test-instrumentation"))]
use std::sync::atomic::{AtomicUsize, Ordering};

/// Resolve ready material through the shared credential cascade.
pub fn resolve_auth(
    api_key_env: &str,
    provider_id: &str,
) -> Option<crate::domain::models::credential::ResolvedAuth> {
    resolve_auth_with_runtime(api_key_env, provider_id, None)
}

/// Convenience wrapper that unwraps `ResolvedAuth` to a plain `String` key.
/// Used by the OpenAI-compatible builders that take a bare `String`.
pub fn resolve_api_key(api_key_env: &str, provider_id: &str) -> Option<String> {
    resolved_key(resolve_auth(api_key_env, provider_id))
}

fn resolved_key(auth: Option<crate::domain::models::ResolvedAuth>) -> Option<String> {
    auth.map(|a| a.bearer_secret().expose_secret().to_owned())
}

fn resolve_auth_with_runtime(
    api_key_env: &str,
    provider_id: &str,
    runtime: Option<&crate::domain::models::SecretString>,
) -> Option<crate::domain::models::ResolvedAuth> {
    use crate::domain::services::key_cascade::{CascadeInputs, SelectionMaterial, select_ready};
    let snapshot = crate::adapters::auth_store::FileAuthStore::snapshot_sync();
    let env_vars = if api_key_env.is_empty() {
        &[][..]
    } else {
        std::slice::from_ref(&api_key_env)
    };
    let selection = select_ready(&CascadeInputs {
        runtime,
        rows: snapshot.rows(provider_id),
        env_vars,
        env_lookup: &crate::infrastructure::utils::env_var_trimmed,
        now_ms: chrono::Utc::now().timestamp_millis(),
        oauth_allowed: true,
    })?;
    match selection.material {
        SelectionMaterial::Ready(auth) => Some(auth),
        SelectionMaterial::NeedsRefresh { .. } => {
            unreachable!("select_ready only returns ready material")
        }
    }
}

/// Complete endpoint identity, using the same URL resolution as construction.
fn discovery_base_url(provider_id: &str, cfg: &ProviderConfig) -> String {
    let kind = cfg.kind.as_deref().unwrap_or(provider_id);
    if kind == "anthropic" {
        return crate::infrastructure::utils::normalize_base_url(&anthropic_base_url(
            Some(cfg),
            &crate::infrastructure::utils::env_var_trimmed,
        ));
    }
    let base = cfg
        .base_url
        .as_ref()
        .map(|url| url.expose_url().to_owned())
        .unwrap_or_else(|| {
            #[cfg(feature = "openai")]
            {
                use crate::adapters::openai::OpenAiCompatibleVariant as V;
                V::builtin(kind)
                    .unwrap_or(V::OpenAI)
                    .default_base_url()
                    .to_owned()
            }
            #[cfg(not(feature = "openai"))]
            {
                String::new()
            }
        });
    crate::infrastructure::utils::normalize_base_url(&base)
}

/// Endpoint part of every discovery cache scope: provider id + effective base URL.
pub fn discovery_scope_context(
    rows_key: &str,
    cfg: &ProviderConfig,
    provider: &dyn StreamingProvider,
) -> String {
    format!(
        "rustain-model-cache-v1\0{}\0{}",
        provider.provider_id(),
        discovery_base_url(rows_key, cfg)
    )
}

/// Startup scope: a static adapter's captured credential, otherwise the stored
/// login's account identity or the key a request resolves first. Refreshes
/// re-read the scope through the adapter (`StreamingProvider::discovery_scope`)
/// and stamp the scope a completed request actually sent.
pub fn discovery_fingerprint(
    rows_key: &str,
    env_vars: &[&str],
    cfg: &ProviderConfig,
    provider: &dyn StreamingProvider,
) -> String {
    use crate::adapters::model_catalog_cache::{ScopeCredential, credential_scope};
    use crate::domain::services::key_cascade::SelectionMaterial;
    let context = discovery_scope_context(rows_key, cfg, provider);
    if let Some(scope) = provider.discovery_credential_fingerprint(&context) {
        return scope;
    }
    let kind = cfg.kind.as_deref().unwrap_or(rows_key);
    let oauth_allowed =
        kind != "anthropic" || anthropic_oauth_allowed(&discovery_base_url(rows_key, cfg));
    let snapshot = crate::adapters::auth_store::FileAuthStore::snapshot_sync();
    let selection = crate::adapters::cli::auth::source_for(
        rows_key,
        env_vars,
        &snapshot,
        &crate::infrastructure::utils::env_var_trimmed,
        oauth_allowed,
    );
    let credential = match &selection {
        Some(selection) => match (
            &selection.source,
            selection.identity.as_deref(),
            &selection.material,
        ) {
            (crate::domain::models::AuthSource::StoredOAuth, Some(identity), _) => {
                ScopeCredential::OAuthIdentity(identity)
            }
            (_, _, SelectionMaterial::Ready(auth)) => ScopeCredential::Secret(auth.bearer_secret()),
            // An identity-less OAuth row awaiting refresh: the bearer a request
            // will send is unknown, so no cached roster can match at startup.
            _ => return String::new(),
        },
        None => ScopeCredential::Keyless,
    };
    credential_scope(&context, credential)
}

/// Runtime call counter for provider construction — Story 13.2a P0-3 sentinel.
///
/// Incremented in `build_provider_for_config`. Integration tests assert this
/// is 0 after `config validate` (no provider constructed) and >0 after a path
/// that builds a provider (positive control proving the sentinel is armed).
#[cfg(any(test, feature = "test-instrumentation"))]
pub static PROVIDER_CTOR_COUNT: AtomicUsize = AtomicUsize::new(0);

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
        "anthropic" => build_anthropic_from_config(cfg, None),
        "openai" | "openrouter" | "google" | "deepseek" | "moonshot" => {
            #[cfg(feature = "openai")]
            {
                // Story 13.4a AC7: env var → auth.json → empty.
                let api_key = resolve_api_key(&cfg.api_key_env, provider_id).unwrap_or_default();
                let adapter = build_openai_from_config(provider_id, cfg, api_key)?;
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
/// builder instead of being resolved from env / `auth.json`.
pub fn build_provider_for_config_with_key(
    provider_id: &str,
    cfg: &ProviderConfig,
    key: &str,
) -> Result<Arc<dyn StreamingProvider>, ProviderError> {
    let kind = cfg.kind.as_deref().unwrap_or(provider_id);
    let runtime = crate::domain::models::SecretString::new(key.to_owned());
    let auth = Some(crate::domain::models::ResolvedAuth::ApiKey(runtime));
    match kind {
        "anthropic" => build_anthropic_from_config(cfg, auth),
        "openai" | "openrouter" | "google" | "deepseek" | "moonshot" => {
            #[cfg(feature = "openai")]
            {
                let adapter = build_openai_from_config(
                    provider_id,
                    cfg,
                    resolved_key(auth).unwrap_or_default(),
                )?;
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
                let adapter = build_openai_compatible_from_config(
                    provider_id,
                    cfg,
                    resolved_key(auth).unwrap_or_default(),
                )?;
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
    auth: Option<crate::domain::models::ResolvedAuth>,
) -> Result<Arc<dyn StreamingProvider>, ProviderError> {
    #[cfg(feature = "anthropic")]
    {
        use crate::adapters::anthropic::{AnthropicAdapter, AnthropicCredential, AuthMode};
        let base_url =
            anthropic_base_url(Some(cfg), &crate::infrastructure::utils::env_var_trimmed);
        static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if let Some(warning) = cfg.auth_header_migration_warning()
            && !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            tracing::warn!("{warning}");
        }
        let credential = if let Some(auth) = auth {
            let scheme = cfg
                .auth_header
                .unwrap_or(crate::domain::models::AuthHeaderScheme::ApiKey);
            let key = auth.bearer_secret().clone();
            AnthropicCredential::Fixed(match scheme {
                crate::domain::models::AuthHeaderScheme::ApiKey => AuthMode::ApiKey(key),
                crate::domain::models::AuthHeaderScheme::Bearer => AuthMode::BearerToken(key),
            })
        } else {
            let env_vars = if cfg.api_key_env.is_empty() {
                vec![]
            } else {
                vec![cfg.api_key_env.clone()]
            };
            AnthropicCredential::Resolved(anthropic_handle(
                &cfg.provider_id,
                env_vars,
                &base_url,
                cfg.auth_header,
            )?)
        };
        let adapter =
            AnthropicAdapter::with_credential(credential, cfg.model_id.clone(), Some(base_url))?;
        Ok(Arc::new(adapter))
    }
    #[cfg(not(feature = "anthropic"))]
    {
        Err(ProviderError::Other(
            "anthropic feature not enabled".to_string(),
        ))
    }
}

/// The same effective endpoint for construction, availability and cache identity.
pub(crate) fn anthropic_base_url(
    cfg: Option<&ProviderConfig>,
    env_lookup: &dyn Fn(&str) -> Option<String>,
) -> String {
    cfg.and_then(|cfg| cfg.base_url.as_ref())
        .map(|url| url.expose_url().to_owned())
        .or_else(|| env_lookup("ANTHROPIC_BASE_URL"))
        .unwrap_or_else(|| crate::adapters::auth::policies::endpoint("https://api.anthropic.com"))
}

/// Stored subscription credentials may only leave for their policy origin.
pub fn anthropic_oauth_allowed(base_url: &str) -> bool {
    let Some(policy) = crate::adapters::auth::policies::oauth_api("anthropic") else {
        return false;
    };
    let official = crate::adapters::auth::policies::endpoint(policy.api_origin);
    match (url::Url::parse(base_url), url::Url::parse(&official)) {
        (Ok(base), Ok(official)) => base.origin() == official.origin(),
        _ => false,
    }
}

#[cfg(feature = "anthropic")]
pub(crate) fn anthropic_handle(
    provider: &str,
    env_vars: Vec<String>,
    base_url: &str,
    auth_header: Option<crate::domain::models::AuthHeaderScheme>,
) -> Result<crate::infrastructure::key_resolver::CredentialHandle, ProviderError> {
    let oauth_allowed = anthropic_oauth_allowed(base_url);
    let snapshot = crate::adapters::auth_store::FileAuthStore::snapshot_sync();
    let names: Vec<&str> = env_vars.iter().map(String::as_str).collect();
    if crate::adapters::cli::auth::source_for(
        provider,
        &names,
        &snapshot,
        &crate::infrastructure::utils::env_var_trimmed,
        oauth_allowed,
    )
    .is_none()
    {
        return Err(ProviderError::AuthenticationFailed);
    }
    Ok(crate::infrastructure::key_resolver::CredentialHandle::new(
        crate::infrastructure::key_resolver::shared(),
        provider.to_owned(),
        env_vars,
        oauth_allowed,
        auth_header,
    ))
}

#[cfg(feature = "openai")]
fn build_openai_from_config(
    provider_id: &str,
    cfg: &ProviderConfig,
    api_key: String,
) -> Result<Arc<crate::adapters::openai::OpenAiAdapter>, ProviderError> {
    use crate::adapters::openai::{OpenAiAdapter, OpenAiCompatibleVariant};

    let kind = cfg.kind.as_deref().unwrap_or(provider_id);

    let variant =
        OpenAiCompatibleVariant::builtin(kind).unwrap_or_else(|| OpenAiCompatibleVariant::Custom {
            provider_id: provider_id.to_string(),
            display_name: provider_id.to_string(),
            context_window: None,
            supports_tools: None,
        });

    let adapter = Arc::new(
        OpenAiAdapter::new(
            variant,
            api_key,
            cfg.model_id.clone(),
            Some(discovery_base_url(provider_id, cfg)),
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

    if cfg.base_url.is_none() {
        return Err(ProviderError::Other(format!(
            "openai-compatible provider '{}' requires a base_url",
            cfg.provider_id
        )));
    }
    let base_url = discovery_base_url(_provider_id, cfg);

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
