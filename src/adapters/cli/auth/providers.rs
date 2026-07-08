//! Static provider metadata table (Story 13.4a Task 4).
//!
//! Canonical source of: display names, signup URLs, key requirements,
//! and auth methods for each supported provider. Reused by 13.4b/13.4c
//! and Epic 19's login UX.

use crate::domain::models::credential::AuthMethod;

/// Metadata for a supported AI provider.
#[derive(Debug, Clone)]
pub struct ProviderMeta {
    /// Canonical provider id (matches provider_factory.rs kind strings).
    pub id: &'static str,
    /// Human-readable display name.
    pub display_name: &'static str,
    /// URL where users can create/manage API keys.
    pub signup_url: &'static str,
    /// Whether this provider requires an API key.
    pub requires_key: bool,
    /// Name of the env var checked for this provider's API key.
    pub api_key_env: &'static str,
    /// Supported authentication methods (forward-compat scaffold for Epic 19).
    pub auth_methods: &'static [AuthMethod],
}

/// All known providers. Order is display order for `auth list`.
static PROVIDERS: &[ProviderMeta] = &[
    ProviderMeta {
        id: "anthropic",
        display_name: "Anthropic",
        signup_url: "https://console.anthropic.com/",
        requires_key: true,
        api_key_env: "ANTHROPIC_API_KEY",
        auth_methods: &[AuthMethod::ApiKey],
    },
    ProviderMeta {
        id: "openai",
        display_name: "OpenAI",
        signup_url: "https://platform.openai.com/api-keys",
        requires_key: true,
        api_key_env: "OPENAI_API_KEY",
        auth_methods: &[AuthMethod::ApiKey],
    },
    ProviderMeta {
        id: "openrouter",
        display_name: "OpenRouter",
        signup_url: "https://openrouter.ai/keys",
        requires_key: true,
        api_key_env: "OPENROUTER_API_KEY",
        auth_methods: &[AuthMethod::ApiKey],
    },
    ProviderMeta {
        id: "google",
        display_name: "Google AI",
        signup_url: "https://aistudio.google.com/apikey",
        requires_key: true,
        api_key_env: "GOOGLE_API_KEY",
        auth_methods: &[AuthMethod::ApiKey],
    },
    ProviderMeta {
        id: "deepseek",
        display_name: "DeepSeek",
        signup_url: "https://platform.deepseek.com/api_keys",
        requires_key: true,
        api_key_env: "DEEPSEEK_API_KEY",
        auth_methods: &[AuthMethod::ApiKey],
    },
    ProviderMeta {
        id: "moonshot",
        display_name: "Moonshot AI",
        signup_url: "https://platform.moonshot.cn/console/api-keys",
        requires_key: true,
        api_key_env: "MOONSHOT_API_KEY",
        auth_methods: &[AuthMethod::ApiKey],
    },
    ProviderMeta {
        id: "ollama",
        display_name: "Ollama",
        signup_url: "https://ollama.com/",
        requires_key: false,
        api_key_env: "",
        auth_methods: &[AuthMethod::ApiKey], // placeholder — ollama has no auth but ApiKey is the shape
    },
];

/// Look up a provider by id. Returns `None` for unknown providers.
pub fn lookup(id: &str) -> Option<&'static ProviderMeta> {
    PROVIDERS.iter().find(|p| p.id == id)
}

/// Return all known provider ids (for error messages listing valid providers).
pub fn all_provider_ids() -> impl Iterator<Item = &'static str> {
    PROVIDERS.iter().map(|p| p.id)
}

/// Return all known providers.
pub fn all_providers() -> &'static [ProviderMeta] {
    PROVIDERS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_known_provider() {
        let meta = lookup("anthropic").unwrap();
        assert_eq!(meta.display_name, "Anthropic");
        assert!(meta.requires_key);
        assert!(!meta.signup_url.is_empty());
    }

    #[test]
    fn lookup_keyless_provider() {
        let meta = lookup("ollama").unwrap();
        assert!(!meta.requires_key);
    }

    #[test]
    fn lookup_unknown_returns_none() {
        assert!(lookup("nonexistent").is_none());
    }

    #[test]
    fn all_provider_ids_includes_all_seven() {
        let ids: Vec<_> = all_provider_ids().collect();
        assert_eq!(ids.len(), 7);
        assert!(ids.contains(&"anthropic"));
        assert!(ids.contains(&"ollama"));
    }
}
