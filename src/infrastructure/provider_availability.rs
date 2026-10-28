//! Credential availability mirrors each factory's selection inputs, before construction.
use crate::adapters::cli::auth::{env_vars_for, providers, source_for};
use crate::domain::models::{AppConfig, AuthSnapshot, AuthSource, ProviderConfig};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderOrigin {
    Bundled,
    Config,
}
#[derive(Debug, Clone)]
pub struct EffectiveProvider {
    pub id: String,
    pub cfg: ProviderConfig,
    pub origin: ProviderOrigin,
}
#[derive(Debug)]
pub enum Availability {
    Keyless,
    Signed(AuthSource),
    Locked { hint: String },
}

pub fn effective_providers(config: &AppConfig) -> Vec<EffectiveProvider> {
    let mut entries: Vec<_> = config
        .provider
        .iter()
        .map(|(id, cfg)| EffectiveProvider {
            id: id.clone(),
            cfg: cfg.clone(),
            origin: ProviderOrigin::Config,
        })
        .collect();
    for meta in providers::all_providers()
        .iter()
        .filter(|meta| meta.requires_key)
    {
        if config.provider.contains_key(meta.id)
            || config
                .provider
                .iter()
                .any(|(id, cfg)| cfg.enabled && cfg.kind.as_deref().unwrap_or(id) == meta.id)
        {
            continue;
        }
        let seed = crate::adapters::model_catalog_cache::load_embedded_seed()
            .and_then(|seed| seed.providers.get(meta.id))
            .map(|p| p.models.as_slice())
            .unwrap_or_default();
        // The provider's own default when the seed carries it, else the first row.
        let model_id = meta
            .default_model
            .and_then(|id| seed.iter().find(|m| m.descriptor.model_id == id))
            .or_else(|| seed.first())
            .map(|m| m.descriptor.model_id.clone())
            .unwrap_or_default();
        entries.push(EffectiveProvider {
            id: meta.id.into(),
            origin: ProviderOrigin::Bundled,
            cfg: ProviderConfig {
                provider_id: meta.id.into(),
                kind: Some(meta.id.into()),
                api_key_env: meta.api_key_env.into(),
                model_id,
                enabled: true,
                base_url: None,
                auth_header: None,
                context_window: None,
                supports_tools: None,
                discover_models: true,
                model_filter: vec!["*".into()],
                cache_ttl_seconds: 3600,
            },
        });
    }
    entries
}

impl EffectiveProvider {
    pub fn kind(&self) -> &str {
        self.cfg.kind.as_deref().unwrap_or(&self.id)
    }
    pub fn rows_key(&self) -> &str {
        if crate::adapters::auth::policies::oauth_api(self.kind()).is_some() {
            &self.cfg.provider_id
        } else {
            &self.id
        }
    }
    pub fn env_vars(&self) -> Vec<&str> {
        if self.origin == ProviderOrigin::Bundled && self.kind() == "anthropic" {
            env_vars_for("anthropic", &self.cfg.api_key_env)
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect()
        // A login-only credential (Gemini CLI token + project) has no env form.
        } else if self.cfg.api_key_env.is_empty() || providers::login_only(self.kind()) {
            vec![]
        } else {
            vec![&self.cfg.api_key_env]
        }
    }
    /// The endpoint every stored row of this entry would be sent to.
    fn base_url(&self, env_lookup: &dyn Fn(&str) -> Option<String>) -> String {
        if self.kind() == "anthropic" {
            super::provider_factory::anthropic_base_url(
                if self.origin == ProviderOrigin::Config {
                    Some(&self.cfg)
                } else {
                    None
                },
                env_lookup,
            )
        } else {
            super::provider_factory::discovery_base_url(&self.id, &self.cfg)
        }
    }
    /// The factory's stored-row rule for this entry (`provider_factory::row_policy`).
    pub fn row_policy(
        &self,
        env_lookup: &dyn Fn(&str) -> Option<String>,
    ) -> crate::domain::services::key_cascade::RowPolicy {
        super::provider_factory::row_policy(
            self.kind(),
            self.rows_key(),
            &self.base_url(env_lookup),
        )
    }
    pub fn display_name(&self) -> String {
        providers::lookup(self.kind())
            .map(|p| p.display_name)
            .unwrap_or(&self.id)
            .into()
    }
    /// The id `auth login` writes rows for that availability actually reads.
    fn login_id(&self) -> Option<&str> {
        let rows = self.rows_key();
        (rows == self.id || providers::lookup(rows).is_some()).then_some(rows)
    }
    pub fn sign_in_hint(&self) -> String {
        let env = self.env_vars();
        match (env.is_empty(), self.login_id()) {
            (true, Some(id)) => format!("Run `rustain auth login {id}`"),
            (false, Some(id)) => {
                format!("Set {} or run `rustain auth login {id}`", env.join(" or "))
            }
            (false, None) => format!("Set {}", env.join(" or ")),
            (true, None) => format!("Set `api_key_env` in `[provider.{}]`", self.id),
        }
    }
}

pub fn availability(
    entry: &EffectiveProvider,
    snapshot: &AuthSnapshot,
    env_lookup: &dyn Fn(&str) -> Option<String>,
) -> Availability {
    let kind = entry.kind();
    if providers::lookup(kind).is_some_and(|meta| !meta.requires_key)
        || (kind == "openai-compatible" && entry.cfg.api_key_env.is_empty())
        || (providers::lookup(kind).is_none() && kind != "openai-compatible")
    {
        return Availability::Keyless;
    }
    match source_for(
        entry.rows_key(),
        &entry.env_vars(),
        snapshot,
        env_lookup,
        entry.row_policy(env_lookup),
    ) {
        Some(selected) => Availability::Signed(selected.source),
        None => Availability::Locked {
            hint: entry.sign_in_hint(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundled(id: &str) -> EffectiveProvider {
        effective_providers(&AppConfig::default())
            .into_iter()
            .find(|entry| entry.id == id)
            .unwrap_or_else(|| panic!("{id} is bundled"))
    }

    #[test]
    fn bundled_default_model_prefers_the_provider_default_in_its_seed() {
        assert_eq!(bundled("xai").cfg.model_id, "grok-4.6");
        assert_eq!(bundled("xai-oauth").cfg.model_id, "grok-4.6");
        // Not the seed's first row (`anthropic/claude-opus-4.7`): omp's default.
        assert_eq!(bundled("openrouter").cfg.model_id, "openai/gpt-5.5");
        // Providers without a declared default keep their first seed row.
        assert_eq!(bundled("anthropic").cfg.model_id, "claude-opus-4-7");
        assert_eq!(bundled("openai-codex").cfg.model_id, "gpt-5.6-luna");
    }

    #[test]
    fn xai_kinds_read_their_own_env_and_login_ids() {
        let key = bundled("xai");
        assert_eq!(key.env_vars(), ["XAI_API_KEY"]);
        assert_eq!(
            key.sign_in_hint(),
            "Set XAI_API_KEY or run `rustain auth login xai`"
        );
        let oauth = bundled("xai-oauth");
        assert_eq!(oauth.env_vars(), ["XAI_OAUTH_TOKEN"]);
        assert_eq!(oauth.rows_key(), "xai-oauth");
        assert_eq!(
            oauth.sign_in_hint(),
            "Set XAI_OAUTH_TOKEN or run `rustain auth login xai-oauth`"
        );
    }
}
