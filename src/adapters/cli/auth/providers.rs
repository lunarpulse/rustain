//! Static provider metadata table (Story 13.4a Task 4).
//!
//! Canonical source of display names, signup URLs, key requirements and login methods.
//!
//! The xAI rows are adapted from oh-my-pi's catalog auth rules.
//! Portions adapted from NousResearch/hermes-agent, Copyright (c) 2025 Nous Research (MIT)
//!
//! MIT License
//!
//! Copyright (c) 2025-2026 Can Bölük
//! Copyright (c) 2026 Stencil Labs, Inc.
//!
//! Permission is hereby granted, free of charge, to any person obtaining a copy
//! of this software and associated documentation files (the "Software"), to deal
//! in the Software without restriction, including without limitation the rights
//! to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
//! copies of the Software, and to permit persons to whom the Software is
//! furnished to do so, subject to the following conditions:
//!
//! The above copyright notice and this permission notice shall be included in all
//! copies or substantial portions of the Software.
//!
//! THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
//! IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
//! FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
//! AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
//! LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
//! OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
//! SOFTWARE.

use crate::adapters::auth::policies;
use crate::domain::models::AuthMethod;
use crate::domain::models::auth_policy::OAuthApiPolicy;

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
    /// Supported authentication methods, in picker order.
    pub auth_methods: &'static [AuthMethod],
    pub oauth_api: Option<&'static OAuthApiPolicy>,
    pub org_scoped_identity: bool,
    /// Bundled default model when it differs from the provider's first seed row.
    pub default_model: Option<&'static str>,
}

/// All known providers. Order is display order for `auth list`.
static PROVIDERS: &[ProviderMeta] = &[
    ProviderMeta {
        id: "anthropic",
        display_name: "Anthropic",
        signup_url: "https://console.anthropic.com/",
        requires_key: true,
        api_key_env: "ANTHROPIC_API_KEY",
        auth_methods: &[
            AuthMethod::OAuthCode(&policies::ANTHROPIC_OAUTH),
            AuthMethod::ApiKey,
        ],
        oauth_api: Some(&policies::ANTHROPIC_API),
        org_scoped_identity: true,
        default_model: None,
    },
    ProviderMeta {
        id: "openai-codex",
        display_name: "ChatGPT Plus/Pro (Codex Subscription)",
        signup_url: "https://chatgpt.com/",
        requires_key: true,
        api_key_env: "OPENAI_CODEX_OAUTH_TOKEN",
        auth_methods: &[
            AuthMethod::OAuthCode(&policies::CODEX_OAUTH),
            AuthMethod::Device(&policies::CODEX_DEVICE),
        ],
        oauth_api: Some(&policies::CODEX_API),
        org_scoped_identity: true,
        default_model: None,
    },
    ProviderMeta {
        id: "openai",
        display_name: "OpenAI",
        signup_url: "https://platform.openai.com/api-keys",
        requires_key: true,
        api_key_env: "OPENAI_API_KEY",
        auth_methods: &[AuthMethod::ApiKey],
        oauth_api: None,
        org_scoped_identity: false,
        default_model: None,
    },
    ProviderMeta {
        id: "openrouter",
        display_name: "OpenRouter",
        signup_url: "https://openrouter.ai/keys",
        requires_key: true,
        api_key_env: "OPENROUTER_API_KEY",
        auth_methods: &[AuthMethod::ApiKey],
        oauth_api: None,
        org_scoped_identity: false,
        default_model: None,
    },
    ProviderMeta {
        id: "google",
        display_name: "Google AI",
        signup_url: "https://aistudio.google.com/apikey",
        requires_key: true,
        api_key_env: "GOOGLE_API_KEY",
        auth_methods: &[AuthMethod::ApiKey],
        oauth_api: None,
        org_scoped_identity: false,
        default_model: None,
    },
    // A structured subscription credential (token + project) has no env form.
    ProviderMeta {
        id: "google-gemini-cli",
        display_name: "Google Cloud Code Assist (Gemini CLI)",
        signup_url: "https://goo.gle/gemini-cli-auth-docs",
        requires_key: true,
        api_key_env: "",
        auth_methods: &[AuthMethod::OAuthCode(&policies::GEMINI_CLI_OAUTH)],
        oauth_api: Some(&policies::GEMINI_CLI_API),
        org_scoped_identity: false,
        default_model: Some("gemini-3.1-pro-preview"),
    },
    ProviderMeta {
        id: "deepseek",
        display_name: "DeepSeek",
        signup_url: "https://platform.deepseek.com/api_keys",
        requires_key: true,
        api_key_env: "DEEPSEEK_API_KEY",
        auth_methods: &[AuthMethod::ApiKey],
        oauth_api: None,
        org_scoped_identity: false,
        default_model: None,
    },
    ProviderMeta {
        id: "moonshot",
        display_name: "Moonshot AI",
        signup_url: "https://platform.moonshot.cn/console/api-keys",
        requires_key: true,
        api_key_env: "MOONSHOT_API_KEY",
        auth_methods: &[AuthMethod::ApiKey],
        oauth_api: None,
        org_scoped_identity: false,
        default_model: None,
    },
    ProviderMeta {
        id: "xai",
        display_name: "xAI",
        signup_url: "https://console.x.ai/team/default/api-keys",
        requires_key: true,
        api_key_env: "XAI_API_KEY",
        auth_methods: &[AuthMethod::ApiKey],
        oauth_api: None,
        org_scoped_identity: false,
        default_model: Some("grok-4.6"),
    },
    ProviderMeta {
        id: "xai-oauth",
        display_name: "xAI Grok OAuth (SuperGrok or X Premium+)",
        signup_url: "https://grok.com/",
        requires_key: true,
        api_key_env: "XAI_OAUTH_TOKEN",
        auth_methods: &[AuthMethod::Device(&policies::XAI_DEVICE)],
        oauth_api: Some(&policies::XAI_API),
        org_scoped_identity: false,
        default_model: Some("grok-4.6"),
    },
    ProviderMeta {
        id: "ollama",
        display_name: "Ollama",
        signup_url: "https://ollama.com/",
        requires_key: false,
        api_key_env: "",
        auth_methods: &[AuthMethod::ApiKey], // placeholder — ollama has no auth but ApiKey is the shape
        oauth_api: None,
        org_scoped_identity: false,
        default_model: None,
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
    fn all_provider_ids_include_browser_subscription() {
        let ids: Vec<_> = all_provider_ids().collect();
        assert!(ids.contains(&"openai-codex"));
        assert!(ids.contains(&"anthropic"));
        assert!(ids.contains(&"ollama"));
    }

    #[test]
    fn provider_ids_are_unique() {
        let ids: Vec<_> = all_provider_ids().collect();
        let unique: std::collections::BTreeSet<_> = ids.iter().copied().collect();
        assert_eq!(unique.len(), ids.len(), "{ids:?}");
    }

    #[test]
    fn codex_offers_browser_then_device_login() {
        let methods = lookup("openai-codex").unwrap().auth_methods;
        let labels: Vec<_> = methods.iter().map(AuthMethod::label).collect();
        assert_eq!(
            labels,
            [
                "ChatGPT Plus/Pro subscription (browser)",
                "ChatGPT Plus/Pro subscription (device code)"
            ]
        );
    }

    #[test]
    fn gemini_cli_is_a_login_only_subscription_with_its_own_default() {
        let meta = lookup("google-gemini-cli").unwrap();
        assert_eq!(meta.display_name, "Google Cloud Code Assist (Gemini CLI)");
        assert_eq!(meta.signup_url, "https://goo.gle/gemini-cli-auth-docs");
        assert!(meta.requires_key);
        assert_eq!(meta.api_key_env, "");
        assert!(matches!(meta.auth_methods, [AuthMethod::OAuthCode(_)]));
        assert_eq!(
            meta.auth_methods[0].login_name(),
            Some("Google Cloud Code Assist (Gemini CLI)")
        );
        assert_eq!(
            meta.oauth_api.unwrap().api_origin,
            "https://cloudcode-pa.googleapis.com"
        );
        assert!(!meta.org_scoped_identity);
        assert_eq!(meta.default_model, Some("gemini-3.1-pro-preview"));
        // The API-key Google provider is a different row.
        assert!(matches!(
            lookup("google").unwrap().auth_methods,
            [AuthMethod::ApiKey]
        ));
    }

    #[test]
    fn xai_rows_split_api_key_and_device_oauth() {
        let key = lookup("xai").unwrap();
        assert!(matches!(key.auth_methods, [AuthMethod::ApiKey]));
        assert!(key.oauth_api.is_none());
        assert_eq!(key.api_key_env, "XAI_API_KEY");
        assert_eq!(key.default_model, Some("grok-4.6"));
        let oauth = lookup("xai-oauth").unwrap();
        assert!(matches!(oauth.auth_methods, [AuthMethod::Device(_)]));
        assert_eq!(
            oauth.auth_methods[0].login_name(),
            Some("xAI Grok OAuth (SuperGrok or X Premium+)")
        );
        assert_eq!(oauth.api_key_env, "XAI_OAUTH_TOKEN");
        assert_eq!(oauth.oauth_api.unwrap().api_origin, "https://api.x.ai");
        assert!(!oauth.org_scoped_identity);
        assert_eq!(oauth.default_model, Some("grok-4.6"));
    }
}
