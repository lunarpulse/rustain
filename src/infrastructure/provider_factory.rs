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
pub fn discovery_base_url(provider_id: &str, cfg: &ProviderConfig) -> String {
    let kind = cfg.kind.as_deref().unwrap_or(provider_id);
    if kind == "anthropic" {
        return crate::infrastructure::utils::normalize_base_url(&anthropic_base_url(
            Some(cfg),
            &crate::infrastructure::utils::env_var_trimmed,
        ));
    }
    if kind == "openai-codex" {
        return codex_base_url(cfg);
    }
    if kind == "google-gemini-cli" {
        return gemini_cli_base_url(cfg);
    }
    #[cfg(feature = "openai")]
    if let Some(xai) = crate::adapters::xai::XaiKind::from_id(kind) {
        return xai_base_url(xai, cfg);
    }
    #[cfg(feature = "openai")]
    if kind == "github-copilot" {
        // Origin guard and cache identity: the configured base, else the
        // canonical API root; requests derive their root per credential.
        return crate::infrastructure::utils::normalize_base_url(
            &configured_base(cfg).map(str::to_owned).unwrap_or_else(|| {
                crate::adapters::auth::policies::endpoint(
                    crate::adapters::github_copilot::DEFAULT_BASE_URL,
                )
            }),
        );
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
    let oauth_allowed = oauth_allowed(kind, &discovery_base_url(rows_key, cfg));
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
            (_, _, SelectionMaterial::Ready(auth)) => ScopeCredential::Grant(auth),
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
    let adapter: Result<Arc<dyn StreamingProvider>, ProviderError> = match kind {
        "anthropic" => build_anthropic_from_config(cfg, None),
        "openai-codex" => build_codex_from_config(cfg),
        "xai" | "xai-oauth" => build_xai_from_config(provider_id, cfg, kind),
        "google-gemini-cli" => build_gemini_cli_from_config(cfg),
        "github-copilot" => build_copilot_from_config(cfg),
        "openai" | "openrouter" | "google" | "deepseek" | "moonshot" => {
            #[cfg(feature = "openai")]
            {
                let adapter = build_openai_from_config(provider_id, cfg, None)?;
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
                let adapter = build_openai_compatible_from_config(provider_id, cfg, None)?;
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
    };
    Ok(configured_provider(provider_id, cfg, adapter?))
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
    let auth = (!matches!(
        kind,
        "openai-codex" | "xai" | "xai-oauth" | "google-gemini-cli" | "github-copilot"
    ))
    .then(|| {
        crate::domain::models::ResolvedAuth::ApiKey(crate::domain::models::SecretString::new(
            key.to_owned(),
        ))
    });
    let adapter: Result<Arc<dyn StreamingProvider>, ProviderError> = match kind {
        "anthropic" => build_anthropic_from_config(cfg, auth),
        "openai-codex" => Err(ProviderError::Other(
            "openai-codex has no API-key login".into(),
        )),
        "xai" => build_xai_with_key(provider_id, cfg, key),
        "xai-oauth" => Err(ProviderError::Other(
            "xai-oauth has no API-key login".into(),
        )),
        "google-gemini-cli" => Err(ProviderError::Other(
            "google-gemini-cli has no API-key login".into(),
        )),
        "github-copilot" => Err(ProviderError::Other(
            "github-copilot has no API-key login".into(),
        )),
        "openai" | "openrouter" | "google" | "deepseek" | "moonshot" => {
            #[cfg(feature = "openai")]
            {
                let adapter = build_openai_from_config(provider_id, cfg, Some(key))?;
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
                let adapter = build_openai_compatible_from_config(provider_id, cfg, Some(key))?;
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
    };
    Ok(configured_provider(provider_id, cfg, adapter?))
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

/// Stored subscription credentials may only leave for their policy origin, or an
/// `https` host under the policy's API host suffix.
pub fn oauth_allowed(kind: &str, base_url: &str) -> bool {
    let Some(policy) = crate::adapters::auth::policies::oauth_api(kind) else {
        return true;
    };
    let official = crate::adapters::auth::policies::endpoint(policy.api_origin);
    let Ok(base) = url::Url::parse(base_url) else {
        return false;
    };
    let suffixed = policy.api_host_suffix.is_some_and(|suffix| {
        base.scheme() == "https"
            && base.host_str().is_some_and(|host| {
                host == suffix
                    || host
                        .strip_suffix(suffix)
                        .is_some_and(|label| label.ends_with('.'))
            })
    });
    suffixed || url::Url::parse(&official).is_ok_and(|official| base.origin() == official.origin())
}

#[cfg(feature = "anthropic")]
pub(crate) fn anthropic_handle(
    provider: &str,
    env_vars: Vec<String>,
    base_url: &str,
    auth_header: Option<crate::domain::models::AuthHeaderScheme>,
) -> Result<crate::infrastructure::key_resolver::CredentialHandle, ProviderError> {
    let oauth_allowed = oauth_allowed("anthropic", base_url);
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

fn codex_base_url(cfg: &ProviderConfig) -> String {
    let base = cfg
        .base_url
        .as_ref()
        .map(|url| url.expose_url().to_owned())
        .unwrap_or_else(|| {
            crate::adapters::auth::policies::endpoint("https://chatgpt.com/backend-api")
        });
    #[cfg(feature = "openai")]
    {
        // All accepted endpoint spellings identify the same Responses endpoint.
        crate::adapters::openai_codex::responses_url(&base)
    }
    #[cfg(not(feature = "openai"))]
    {
        crate::infrastructure::utils::normalize_base_url(&base)
    }
}

fn build_codex_from_config(
    cfg: &ProviderConfig,
) -> Result<Arc<dyn StreamingProvider>, ProviderError> {
    #[cfg(feature = "openai")]
    {
        let base = codex_base_url(cfg);
        let allowed = oauth_allowed("openai-codex", &base);
        let env_vars = if cfg.api_key_env.is_empty() {
            vec![]
        } else {
            vec![cfg.api_key_env.clone()]
        };
        let snapshot = crate::adapters::auth_store::FileAuthStore::snapshot_sync();
        let names: Vec<&str> = env_vars.iter().map(String::as_str).collect();
        if crate::adapters::cli::auth::source_for(
            &cfg.provider_id,
            &names,
            &snapshot,
            &crate::infrastructure::utils::env_var_trimmed,
            allowed,
        )
        .is_none()
        {
            return Err(ProviderError::AuthenticationFailed);
        }
        let handle = crate::infrastructure::key_resolver::CredentialHandle::new(
            crate::infrastructure::key_resolver::shared(),
            cfg.provider_id.clone(),
            env_vars,
            allowed,
            None,
        );
        Ok(Arc::new(crate::adapters::openai_codex::CodexAdapter::new(
            handle,
            cfg.model_id.clone(),
            base,
        )?))
    }
    #[cfg(not(feature = "openai"))]
    {
        let _ = cfg;
        Err(ProviderError::Other(
            "openai feature not enabled — rebuild with --features openai".into(),
        ))
    }
}

/// The one exposure of an xAI or GitHub Copilot entry's configured base: its
/// actual HTTP endpoint, passed to the adapter verbatim.
#[cfg(feature = "openai")]
fn configured_base(cfg: &ProviderConfig) -> Option<&str> {
    cfg.base_url.as_ref().map(|url| url.expose_url())
}

/// One base for construction, availability, origin guard and cache identity.
#[cfg(feature = "openai")]
fn xai_base_url(kind: crate::adapters::xai::XaiKind, cfg: &ProviderConfig) -> String {
    crate::infrastructure::utils::normalize_base_url(&crate::adapters::xai::base_url(
        kind,
        configured_base(cfg),
    ))
}

fn build_xai_from_config(
    provider_id: &str,
    cfg: &ProviderConfig,
    kind: &str,
) -> Result<Arc<dyn StreamingProvider>, ProviderError> {
    #[cfg(feature = "openai")]
    {
        use crate::adapters::xai::{XaiAdapter, XaiKind};
        let xai = XaiKind::from_id(kind)
            .ok_or_else(|| ProviderError::Other(format!("unknown xAI kind '{kind}'")))?;
        // Rows are read under the same key availability reads.
        let rows_key = if crate::adapters::auth::policies::oauth_api(kind).is_some() {
            cfg.provider_id.as_str()
        } else {
            provider_id
        };
        // A subscription row keeps its own origin rule under any kind:
        // `[provider.xai-oauth] kind = "xai"` must not carry it to another origin.
        let base = xai_base_url(xai, cfg);
        let allowed = oauth_allowed(kind, &base) && oauth_allowed(rows_key, &base);
        let env_vars = if cfg.api_key_env.is_empty() {
            vec![]
        } else {
            vec![cfg.api_key_env.clone()]
        };
        let snapshot = crate::adapters::auth_store::FileAuthStore::snapshot_sync();
        let names: Vec<&str> = env_vars.iter().map(String::as_str).collect();
        if crate::adapters::cli::auth::source_for(
            rows_key,
            &names,
            &snapshot,
            &crate::infrastructure::utils::env_var_trimmed,
            allowed,
        )
        .is_none()
        {
            return Err(ProviderError::AuthenticationFailed);
        }
        let handle = crate::infrastructure::key_resolver::CredentialHandle::new(
            crate::infrastructure::key_resolver::shared(),
            rows_key.to_owned(),
            env_vars,
            allowed,
            None,
        );
        Ok(Arc::new(XaiAdapter::new(
            handle,
            xai,
            configured_base(cfg).map(str::to_owned),
        )?))
    }
    #[cfg(not(feature = "openai"))]
    {
        let _ = (provider_id, cfg, kind);
        Err(ProviderError::Other(
            "openai feature not enabled — rebuild with --features openai".into(),
        ))
    }
}

/// The API-key engine's probe: the candidate key alone, never env or stored rows.
fn build_xai_with_key(
    provider_id: &str,
    cfg: &ProviderConfig,
    key: &str,
) -> Result<Arc<dyn StreamingProvider>, ProviderError> {
    #[cfg(feature = "openai")]
    {
        let handle = crate::infrastructure::key_resolver::CredentialHandle::runtime(
            crate::infrastructure::key_resolver::shared(),
            provider_id.to_owned(),
            crate::domain::models::SecretString::new(key.to_owned()),
        );
        Ok(Arc::new(crate::adapters::xai::XaiAdapter::new(
            handle,
            crate::adapters::xai::XaiKind::Xai,
            configured_base(cfg).map(str::to_owned),
        )?))
    }
    #[cfg(not(feature = "openai"))]
    {
        let _ = (provider_id, cfg, key);
        Err(ProviderError::Other(
            "openai feature not enabled — rebuild with --features openai".into(),
        ))
    }
}

/// One base for construction, availability, origin guard and cache identity: the
/// configured `base_url` (its one exposure here), else the Cloud Code Assist origin.
fn gemini_cli_base_url(cfg: &ProviderConfig) -> String {
    let configured = cfg.base_url.as_ref().map(|url| url.expose_url());
    #[cfg(feature = "openai")]
    {
        crate::infrastructure::utils::normalize_base_url(&crate::adapters::gemini_cli::base_url(
            configured,
        ))
    }
    #[cfg(not(feature = "openai"))]
    {
        crate::infrastructure::utils::normalize_base_url(configured.unwrap_or_default())
    }
}

/// The stored login only: a structured token + project has no env or API-key form.
fn build_gemini_cli_from_config(
    cfg: &ProviderConfig,
) -> Result<Arc<dyn StreamingProvider>, ProviderError> {
    #[cfg(feature = "openai")]
    {
        let base = gemini_cli_base_url(cfg);
        // A configured base on another origin locks the stored row.
        let allowed =
            oauth_allowed("google-gemini-cli", &base) && oauth_allowed(&cfg.provider_id, &base);
        let snapshot = crate::adapters::auth_store::FileAuthStore::snapshot_sync();
        if crate::adapters::cli::auth::source_for(
            &cfg.provider_id,
            &[],
            &snapshot,
            &crate::infrastructure::utils::env_var_trimmed,
            allowed,
        )
        .is_none()
        {
            return Err(ProviderError::AuthenticationFailed);
        }
        let handle = crate::infrastructure::key_resolver::CredentialHandle::new(
            crate::infrastructure::key_resolver::shared(),
            cfg.provider_id.clone(),
            Vec::new(),
            allowed,
            None,
        );
        Ok(Arc::new(
            crate::adapters::gemini_cli::GeminiCliAdapter::new(handle, base)?,
        ))
    }
    #[cfg(not(feature = "openai"))]
    {
        let _ = cfg;
        Err(ProviderError::Other(
            "openai feature not enabled — rebuild with --features openai".into(),
        ))
    }
}

/// GitHub Copilot: the stored subscription row or `COPILOT_GITHUB_TOKEN`, with
/// the Codex builder shape; the configured base goes to the adapter verbatim.
fn build_copilot_from_config(
    cfg: &ProviderConfig,
) -> Result<Arc<dyn StreamingProvider>, ProviderError> {
    #[cfg(feature = "openai")]
    {
        let rows_key = cfg.provider_id.as_str();
        let base = discovery_base_url(rows_key, cfg);
        // A subscription row keeps its own origin rule under any entry name.
        let allowed = oauth_allowed("github-copilot", &base) && oauth_allowed(rows_key, &base);
        let env_vars = if cfg.api_key_env.is_empty() {
            vec![]
        } else {
            vec![cfg.api_key_env.clone()]
        };
        let snapshot = crate::adapters::auth_store::FileAuthStore::snapshot_sync();
        let names: Vec<&str> = env_vars.iter().map(String::as_str).collect();
        if crate::adapters::cli::auth::source_for(
            rows_key,
            &names,
            &snapshot,
            &crate::infrastructure::utils::env_var_trimmed,
            allowed,
        )
        .is_none()
        {
            return Err(ProviderError::AuthenticationFailed);
        }
        let handle = crate::infrastructure::key_resolver::CredentialHandle::new(
            crate::infrastructure::key_resolver::shared(),
            rows_key.to_owned(),
            env_vars,
            allowed,
            None,
        );
        Ok(Arc::new(
            crate::adapters::github_copilot::CopilotAdapter::new(
                handle,
                cfg.model_id.clone(),
                configured_base(cfg).map(str::to_owned),
            )?,
        ))
    }
    #[cfg(not(feature = "openai"))]
    {
        let _ = cfg;
        Err(ProviderError::Other(
            "openai feature not enabled — rebuild with --features openai".into(),
        ))
    }
}

/// Keep endpoint identity and endpoint restrictions outside transport-specific policy.
struct ConfiguredProvider {
    id: String,
    tools_disabled: bool,
    inner: Arc<dyn StreamingProvider>,
}

fn configured_provider(
    id: &str,
    cfg: &ProviderConfig,
    inner: Arc<dyn StreamingProvider>,
) -> Arc<dyn StreamingProvider> {
    if inner.provider_id() == id && cfg.supports_tools != Some(false) {
        return inner;
    }
    Arc::new(ConfiguredProvider {
        id: id.into(),
        tools_disabled: cfg.supports_tools == Some(false),
        inner,
    })
}

impl ConfiguredProvider {
    fn metadata(
        &self,
        mut models: Vec<crate::domain::models::provider::ModelDescriptor>,
    ) -> Vec<crate::domain::models::provider::ModelDescriptor> {
        for model in &mut models {
            model.provider_id.clone_from(&self.id);
        }
        restrict_tools(&mut models, self.tools_disabled);
        models
    }
}

pub(crate) fn restrict_tools(
    models: &mut [crate::domain::models::provider::ModelDescriptor],
    disabled: bool,
) {
    if disabled {
        for model in models {
            model
                .capabilities
                .remove(&crate::domain::models::provider::ModelCapability::ToolUse);
        }
    }
}

#[async_trait::async_trait]
impl StreamingProvider for ConfiguredProvider {
    async fn stream_completion(
        &self,
        messages: Vec<crate::domain::models::Message>,
        options: crate::domain::models::CompletionOptions,
    ) -> Result<
        futures::stream::BoxStream<'static, crate::domain::models::StreamChunk>,
        ProviderError,
    > {
        self.inner.stream_completion(messages, options).await
    }
    async fn abort(&self) -> Result<(), ProviderError> {
        self.inner.abort().await
    }
    fn provider_id(&self) -> String {
        self.id.clone()
    }
    fn list_models(&self) -> Vec<crate::domain::models::provider::ModelDescriptor> {
        self.metadata(self.inner.list_models())
    }
    async fn discover_models(
        &self,
        filter: &[String],
        context: &str,
    ) -> Option<Result<crate::domain::models::provider::DiscoveredModels, ProviderError>> {
        self.inner.discover_models(filter, context).await
    }
    fn set_discovered_models(&self, models: Vec<crate::domain::models::provider::ModelDescriptor>) {
        self.inner.set_discovered_models(self.metadata(models));
    }
    fn discovery_credential_fingerprint(&self, context: &str) -> Option<String> {
        self.inner.discovery_credential_fingerprint(context)
    }
    async fn discovery_scope(&self, context: &str) -> Option<String> {
        self.inner.discovery_scope(context).await
    }
    fn default_context_window(&self, model: &str) -> Option<u32> {
        self.inner.default_context_window(model)
    }
    async fn health_check(&self) -> Result<(), ProviderError> {
        self.inner.health_check().await
    }
    async fn connectivity_probe(
        &self,
    ) -> Result<crate::domain::ports::ProbeOutcome, ProviderError> {
        self.inner.connectivity_probe().await
    }
    fn provider_descriptor(&self) -> crate::domain::models::provider::ProviderDescriptor {
        let mut descriptor = self.inner.provider_descriptor();
        descriptor.provider_id.clone_from(&self.id);
        descriptor
    }
}

/// The credential of an OpenAI-family entry: an explicit candidate key, else a
/// per-request handle over env and stored rows (precheck: something resolves).
/// An `openai-compatible` entry without `api_key_env` uses a stored key when one
/// exists and is otherwise keyless.
#[cfg(feature = "openai")]
fn chat_credential(
    provider_id: &str,
    cfg: &ProviderConfig,
    key: Option<&str>,
) -> Result<crate::adapters::openai::ChatCredential, ProviderError> {
    use crate::adapters::openai::ChatCredential;
    if let Some(key) = key {
        return Ok(ChatCredential::Fixed(
            crate::domain::models::SecretString::new(key.to_owned()),
        ));
    }
    let kind = cfg.kind.as_deref().unwrap_or(provider_id);
    let snapshot = crate::adapters::auth_store::FileAuthStore::snapshot_sync();
    let handle = |env_vars: Vec<String>, allowed: bool| {
        crate::infrastructure::key_resolver::CredentialHandle::new(
            crate::infrastructure::key_resolver::shared(),
            provider_id.to_owned(),
            env_vars,
            allowed,
            None,
        )
    };
    // A subscription row keeps its own origin rule under any kind:
    // `[provider.github-copilot] kind = "openai"` must not carry it to another origin.
    let base = discovery_base_url(provider_id, cfg);
    let allowed = oauth_allowed(kind, &base) && oauth_allowed(provider_id, &base);
    if cfg.api_key_env.is_empty() && kind == "openai-compatible" {
        let stored = crate::adapters::cli::auth::source_for(
            provider_id,
            &[],
            &snapshot,
            &crate::infrastructure::utils::env_var_trimmed,
            allowed,
        );
        return Ok(match stored {
            Some(_) => ChatCredential::Resolved(handle(vec![], allowed)),
            None => ChatCredential::Fixed(crate::domain::models::SecretString::new(String::new())),
        });
    }
    let env_vars = if cfg.api_key_env.is_empty() {
        vec![]
    } else {
        vec![cfg.api_key_env.clone()]
    };
    let names: Vec<&str> = env_vars.iter().map(String::as_str).collect();
    if crate::adapters::cli::auth::source_for(
        provider_id,
        &names,
        &snapshot,
        &crate::infrastructure::utils::env_var_trimmed,
        allowed,
    )
    .is_none()
    {
        return Err(ProviderError::AuthenticationFailed);
    }
    Ok(ChatCredential::Resolved(handle(env_vars, allowed)))
}

#[cfg(feature = "openai")]
fn build_openai_from_config(
    provider_id: &str,
    cfg: &ProviderConfig,
    key: Option<&str>,
) -> Result<Arc<crate::adapters::openai::OpenAiAdapter>, ProviderError> {
    use crate::adapters::openai::{OpenAiAdapter, OpenAiCompatibleVariant, StaticChatProfile};

    let kind = cfg.kind.as_deref().unwrap_or(provider_id);

    let variant =
        OpenAiCompatibleVariant::builtin(kind).unwrap_or_else(|| OpenAiCompatibleVariant::Custom {
            provider_id: provider_id.to_string(),
            display_name: provider_id.to_string(),
            context_window: None,
            supports_tools: None,
        });
    let credential = chat_credential(provider_id, cfg, key)?;
    let profile = StaticChatProfile::new(&discovery_base_url(provider_id, cfg));

    let adapter = Arc::new(
        OpenAiAdapter::new(variant, credential, cfg.model_id.clone(), profile)
            .map_err(|e| ProviderError::Other(format!("Failed to create OpenAI adapter: {}", e)))?,
    );
    Ok(adapter)
}

#[cfg(feature = "openai")]
fn build_openai_compatible_from_config(
    provider_id: &str,
    cfg: &ProviderConfig,
    key: Option<&str>,
) -> Result<Arc<crate::adapters::openai::OpenAiAdapter>, ProviderError> {
    use crate::adapters::openai::{OpenAiAdapter, OpenAiCompatibleVariant, StaticChatProfile};

    if cfg.base_url.is_none() {
        return Err(ProviderError::Other(format!(
            "openai-compatible provider '{}' requires a base_url",
            cfg.provider_id
        )));
    }
    let base_url = discovery_base_url(provider_id, cfg);

    let variant = OpenAiCompatibleVariant::Custom {
        provider_id: cfg.provider_id.clone(),
        display_name: cfg.provider_id.clone(),
        context_window: cfg.context_window,
        supports_tools: cfg.supports_tools,
    };
    let credential = chat_credential(provider_id, cfg, key)?;

    let adapter = Arc::new(
        OpenAiAdapter::new(
            variant,
            credential,
            cfg.model_id.clone(),
            StaticChatProfile::new(&base_url),
        )
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

#[cfg(all(test, feature = "openai"))]
mod tests {
    use super::*;

    fn xai_config(kind: &str, base_url: Option<&str>) -> ProviderConfig {
        ProviderConfig {
            provider_id: kind.into(),
            kind: Some(kind.into()),
            model_id: "grok-4.6".into(),
            api_key_env: String::new(),
            auth_header: None,
            enabled: true,
            base_url: base_url.map(|url| url.to_owned().into()),
            context_window: None,
            supports_tools: None,
            discover_models: true,
            model_filter: vec!["*".into()],
            cache_ttl_seconds: 3600,
        }
    }

    /// Restores the variables the xAI base rules read.
    struct EnvGuard(Vec<(&'static str, Option<std::ffi::OsString>)>);
    impl EnvGuard {
        fn new(base: Option<&str>) -> Self {
            let names = ["XAI_BASE_URL", "RUSTAIN_TEST_AUTH_ORIGIN"];
            let saved = names.map(|name| (name, std::env::var_os(name))).to_vec();
            // SAFETY: the test holds `#[serial]`.
            unsafe {
                std::env::remove_var("RUSTAIN_TEST_AUTH_ORIGIN");
                match base {
                    Some(base) => std::env::set_var("XAI_BASE_URL", base),
                    None => std::env::remove_var("XAI_BASE_URL"),
                }
            }
            Self(saved)
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (name, value) in &self.0 {
                // SAFETY: the test holds `#[serial]`.
                unsafe {
                    match value {
                        Some(value) => std::env::set_var(name, value),
                        None => std::env::remove_var(name),
                    }
                }
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn gemini_cli_has_no_key_login_and_guards_its_origin() {
        let _env = EnvGuard::new(None);
        let cfg = |base: Option<&str>| ProviderConfig {
            model_id: "gemini-3.1-pro-preview".into(),
            ..xai_config("google-gemini-cli", base)
        };
        match build_provider_for_config_with_key("google-gemini-cli", &cfg(None), "key") {
            Err(ProviderError::Other(message)) => {
                assert_eq!(message, "google-gemini-cli has no API-key login");
            }
            Err(other) => panic!("unexpected error {other:?}"),
            Ok(_) => panic!("google-gemini-cli must refuse a static key"),
        }
        let base = discovery_base_url("google-gemini-cli", &cfg(None));
        assert_eq!(base, "https://cloudcode-pa.googleapis.com");
        assert!(oauth_allowed("google-gemini-cli", &base));
        let foreign = discovery_base_url("google-gemini-cli", &cfg(Some("https://proxy.example/")));
        assert_eq!(foreign, "https://proxy.example");
        assert!(!oauth_allowed("google-gemini-cli", &foreign));
    }

    #[test]
    fn xai_oauth_has_no_api_key_login() {
        let cfg = xai_config("xai-oauth", None);
        match build_provider_for_config_with_key("xai-oauth", &cfg, "key") {
            Err(ProviderError::Other(message)) => {
                assert_eq!(message, "xai-oauth has no API-key login");
            }
            Err(other) => panic!("unexpected error {other:?}"),
            Ok(_) => panic!("xai-oauth must refuse a static key"),
        }
    }

    #[test]
    #[serial_test::serial]
    fn xai_bases_follow_kind_rules_and_guard_the_oauth_origin() {
        let _env = EnvGuard::new(Some(" http://127.0.0.1:9/v1/ "));
        let base = |kind: &str, configured: Option<&str>| {
            discovery_base_url(kind, &xai_config(kind, configured))
        };
        // The key provider follows the env override, also over a default-valued config.
        assert_eq!(base("xai", None), "http://127.0.0.1:9/v1");
        assert_eq!(
            base("xai", Some("https://api.x.ai/v1/")),
            "http://127.0.0.1:9/v1"
        );
        assert_eq!(
            base("xai", Some("https://gateway.example/v1/")),
            "https://gateway.example/v1"
        );
        // The OAuth provider never follows it; a foreign configured origin locks the row.
        assert_eq!(base("xai-oauth", None), "https://api.x.ai/v1");
        assert!(oauth_allowed("xai-oauth", &base("xai-oauth", None)));
        let foreign = base("xai-oauth", Some("https://gateway.example/v1"));
        assert_eq!(foreign, "https://gateway.example/v1");
        assert!(!oauth_allowed("xai-oauth", &foreign));
        // API keys have no stored-OAuth origin rule.
        assert!(oauth_allowed("xai", &foreign));
    }
}
