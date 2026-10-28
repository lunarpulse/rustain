use super::{FlowCtx, FlowError};
use crate::adapters::cli::auth::providers;
use crate::domain::{
    errors::ProviderError,
    models::{Credential, ProviderConfig},
};

/// A distinct key is another account (a new active row); the same key again
/// updates its row in place. Nothing is asked before the key itself.
pub(super) async fn run(provider: &str, ctx: &FlowCtx) -> Result<(Credential, bool), FlowError> {
    let display = providers::lookup(provider).map_or(provider, |m| m.display_name);
    let key = ctx
        .ui
        .prompt_secret(&format!("Enter {display} API key: "))
        .await
        .ok_or(FlowError::Cancelled)?;
    let key = expose(&key).trim();
    if key.is_empty() {
        return Err(FlowError::configuration("API key cannot be empty."));
    }
    let validated = verify(provider, ctx, key).await?;
    Ok((Credential::new_api_key(key.to_owned()), validated))
}
/// The one key check every key-producing engine runs before storing a key: the
/// validation entry, the progress line, the provider's probe and its decision.
pub(super) async fn verify(provider: &str, ctx: &FlowCtx, key: &str) -> Result<bool, FlowError> {
    let meta = providers::lookup(provider);
    let display = meta.map_or(provider, |m| m.display_name);
    let cfg = match &ctx.validation_config {
        Some(cfg) => cfg.clone(),
        None => {
            let meta = meta.ok_or_else(|| FlowError::configuration("Unknown provider"))?;
            ProviderConfig {
                provider_id: provider.into(),
                model_id: String::new(),
                api_key_env: meta.api_key_env.into(),
                enabled: true,
                kind: Some(provider.into()),
                base_url: None,
                auth_header: None,
                context_window: None,
                supports_tools: None,
                discover_models: false,
                model_filter: vec![],
                cache_ttl_seconds: 0,
            }
        }
    };
    ctx.ui
        .on_progress(&format!("Validating key against {display}…"));
    let validation = validate_key(provider, &cfg, key).await;
    decision(display, meta.map_or("", |m| m.signup_url), validation)
}
fn expose(value: &crate::domain::models::SecretString) -> &str {
    value.expose_secret()
}
#[derive(Debug)]
enum ValidationOutcome {
    Invalid,
    Offline,
    Inconclusive,
    Error,
}
fn decision(
    display: &str,
    signup: &str,
    result: Result<(), ValidationOutcome>,
) -> Result<bool, FlowError> {
    match result {
        Ok(()) => Ok(true),
        Err(ValidationOutcome::Inconclusive) => Ok(false),
        Err(ValidationOutcome::Invalid) => Err(FlowError::configuration(format!(
            "✗ Invalid API key for {display}.\n  Get one at: {signup}"
        ))),
        Err(ValidationOutcome::Offline) => Err(FlowError::configuration(
            "✗ Cannot validate — offline. Key was NOT stored.",
        )),
        Err(ValidationOutcome::Error) => Err(FlowError::configuration(
            "✗ Validation error. Key was NOT stored.",
        )),
    }
}
async fn validate_key(
    provider: &str,
    cfg: &ProviderConfig,
    key: &str,
) -> Result<(), ValidationOutcome> {
    let provider = match crate::infrastructure::provider_factory::build_provider_for_config_with_key(
        provider, cfg, key,
    ) {
        Ok(provider) => provider,
        Err(ProviderError::Other(message)) if message.contains("feature not enabled") => {
            return Err(ValidationOutcome::Inconclusive);
        }
        Err(_) => return Err(ValidationOutcome::Error),
    };
    match provider.connectivity_probe().await {
        Ok(_) => Ok(()),
        Err(ProviderError::AuthenticationFailed) => Err(ValidationOutcome::Invalid),
        Err(ProviderError::Offline(_)) => Err(ValidationOutcome::Offline),
        Err(ProviderError::EndpointUnsupported(_)) => Err(ValidationOutcome::Inconclusive),
        Err(_) => Err(ValidationOutcome::Error),
    }
}
#[cfg(all(
    test,
    any(feature = "anthropic", feature = "openai", feature = "ollama")
))]
mod tests;
