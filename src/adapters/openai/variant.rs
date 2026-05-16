//! OpenAI-compatible provider variants.
//!
//! The OpenAI-compatible adapter supports multiple providers that implement
//! the OpenAI Chat Completions API with different base URLs and model catalogs.
//!
//! Model catalogs are sourced from the embedded `models_variants.json` file
//! (Story 7.7). The shipped JSON is the Tier-0 seed — providers read their
//! catalog from it rather than hard-coded Rust `vec![]` literals.
//!
//! | Variant | provider_id | Default base URL |
//! |---|---|---|
//! | OpenAI | `openai` | `https://api.openai.com/v1` |
//! | OpenRouter | `openrouter` | `https://openrouter.ai/api/v1` |
//! | Google AI | `google` | `https://generativelanguage.googleapis.com/v1beta/openai` |
//! | DeepSeek | `deepseek` | `https://api.deepseek.com/v1` |
//! | Moonshot/Kimi | `moonshot` | `https://api.moonshot.cn/v1` |

use super::allowlists;
use crate::domain::models::provider::{ModelCapability, ModelDescriptor};

/// Public accessor for the raw embedded JSON text (used by `update-catalog` CLI, startup).
/// Delegates to the shared catalog infrastructure (Story 7.7).
pub fn embedded_models_json() -> &'static str {
    crate::adapters::model_catalog_cache::embedded_seed_json()
}

/// Identifies which OpenAI-compatible provider an adapter instance targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenAiCompatibleVariant {
    OpenAI,
    OpenRouter,
    Google,
    DeepSeek,
    Moonshot,
    Custom {
        provider_id: String,
        display_name: String,
        context_window: Option<u32>,
        supports_tools: Option<bool>,
    },
}

impl OpenAiCompatibleVariant {
    /// Provider identifier used for registry keys and UI display.
    pub fn provider_id(&self) -> &str {
        match self {
            Self::OpenAI => "openai",
            Self::OpenRouter => "openrouter",
            Self::Google => "google",
            Self::DeepSeek => "deepseek",
            Self::Moonshot => "moonshot",
            Self::Custom { provider_id, .. } => provider_id.as_str(),
        }
    }

    /// Default base URL for the provider's OpenAI-compatible endpoint.
    pub fn default_base_url(&self) -> &'static str {
        match self {
            Self::OpenAI => "https://api.openai.com/v1",
            Self::OpenRouter => "https://openrouter.ai/api/v1",
            Self::Google => "https://generativelanguage.googleapis.com/v1beta/openai",
            Self::DeepSeek => "https://api.deepseek.com/v1",
            Self::Moonshot => "https://api.moonshot.cn/v1",
            Self::Custom { .. } => "https://api.openai.com/v1",
        }
    }

    /// Known model catalog for this variant.
    ///
    /// Built-in variants read from the embedded `models_variants.json` (Story 7.7 AC2).
    /// `Custom` returns a single descriptor for the configured model (no seed catalog).
    pub fn known_models(&self, configured_model: &str) -> Vec<ModelDescriptor> {
        let provider_id = self.provider_id().to_string();
        match self {
            Self::Custom {
                context_window,
                supports_tools,
                ..
            } => {
                let mut caps = std::collections::HashSet::new();
                if supports_tools.unwrap_or(true) {
                    caps.insert(ModelCapability::ToolUse);
                }
                vec![ModelDescriptor {
                    model_id: configured_model.to_string(),
                    display_name: configured_model.to_string(),
                    provider_id: provider_id.clone(),
                    context_window: context_window.unwrap_or(8_192),
                    capabilities: caps,
                    pricing_tier: Some("local".to_string()),
                    stale: false,
                }]
            }
            _ => {
                if let Some(catalog) = crate::adapters::model_catalog_cache::load_embedded_seed() {
                    if let Some(entry) = catalog.providers.get(self.provider_id()) {
                        return entry.models.iter().map(|m| m.descriptor.clone()).collect();
                    }
                }
                // Fallback: empty catalog (no crash — AC1)
                tracing::warn!(
                    "No embedded catalog entry for provider '{}'; returning empty",
                    self.provider_id()
                );
                Vec::new()
            }
        }
    }

    /// Per-model context window lookup.
    ///
    /// For built-in variants, searches `known_models()`. For `Custom` variants
    /// whose `provider_id` matches a built-in (e.g. "openai", "deepseek"),
    /// cross-references that built-in's catalog so models from `/v1/models`
    /// that don't self-report `context_length` still get accurate values.
    ///
    /// Matching uses exact match first, then prefix-of-known (so "gpt-4o"
    /// matches "gpt-4o-2024-11-20" and vice versa).
    pub fn context_window_for(&self, model_id: &str) -> Option<u32> {
        let candidates = match self {
            Self::Custom { provider_id, .. } => match provider_id.as_str() {
                "openai" => Self::OpenAI.known_models(""),
                "openrouter" => Self::OpenRouter.known_models(""),
                "deepseek" => Self::DeepSeek.known_models(""),
                "moonshot" => Self::Moonshot.known_models(""),
                "google" => Self::Google.known_models(""),
                _ => self.known_models(model_id),
            },
            _ => self.known_models(model_id),
        };
        candidates
            .iter()
            .find(|m| {
                m.model_id == model_id
                    || m.model_id.starts_with(model_id)
                    || model_id.starts_with(&m.model_id)
            })
            .map(|m| m.context_window)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_variant_provider_ids() {
        assert_eq!(OpenAiCompatibleVariant::OpenAI.provider_id(), "openai");
        assert_eq!(
            OpenAiCompatibleVariant::OpenRouter.provider_id(),
            "openrouter"
        );
        assert_eq!(OpenAiCompatibleVariant::Google.provider_id(), "google");
        assert_eq!(OpenAiCompatibleVariant::DeepSeek.provider_id(), "deepseek");
        assert_eq!(OpenAiCompatibleVariant::Moonshot.provider_id(), "moonshot");
        assert_eq!(
            OpenAiCompatibleVariant::Custom {
                provider_id: "my-proxy".to_string(),
                display_name: "My Proxy".to_string(),
                context_window: None,
                supports_tools: None,
            }
            .provider_id(),
            "my-proxy"
        );
    }

    #[test]
    fn test_variant_default_base_urls() {
        assert_eq!(
            OpenAiCompatibleVariant::OpenAI.default_base_url(),
            "https://api.openai.com/v1"
        );
        assert_eq!(
            OpenAiCompatibleVariant::OpenRouter.default_base_url(),
            "https://openrouter.ai/api/v1"
        );
        assert_eq!(
            OpenAiCompatibleVariant::Google.default_base_url(),
            "https://generativelanguage.googleapis.com/v1beta/openai"
        );
        assert_eq!(
            OpenAiCompatibleVariant::DeepSeek.default_base_url(),
            "https://api.deepseek.com/v1"
        );
        assert_eq!(
            OpenAiCompatibleVariant::Moonshot.default_base_url(),
            "https://api.moonshot.cn/v1"
        );
    }

    #[test]
    fn test_openai_known_catalog() {
        let models = OpenAiCompatibleVariant::OpenAI.known_models("gpt-5.5");
        assert_eq!(models.len(), 4, "expected 4 models, got {}", models.len());
        assert!(models.iter().any(|m| m.model_id == "gpt-5.5"));
        assert!(models.iter().any(|m| m.model_id == "gpt-5.4"));
        assert!(models.iter().any(|m| m.model_id == "gpt-5.4-mini"));
        assert!(models.iter().any(|m| m.model_id == "gpt-5.4-nano"));
        for m in &models {
            assert_eq!(m.provider_id, "openai");
        }
    }

    #[test]
    fn test_openrouter_returns_curated_allowlist() {
        let models = OpenAiCompatibleVariant::OpenRouter.known_models("anthropic/claude-opus-4.7");
        // Should return the full curated seed catalog from embedded JSON.
        assert_eq!(models.len(), 8);
        assert!(
            models
                .iter()
                .any(|m| m.model_id == "anthropic/claude-opus-4.7")
        );
        assert!(models.iter().any(|m| m.model_id == "openai/gpt-5.5"));
        assert!(models.iter().all(|m| m.provider_id == "openrouter"));
    }

    #[test]
    fn test_google_known_catalog() {
        let models = OpenAiCompatibleVariant::Google.known_models("gemini-3.1-pro-preview");
        assert_eq!(models.len(), 2);
        assert!(
            models
                .iter()
                .any(|m| m.model_id == "gemini-3.1-pro-preview")
        );
        assert!(
            models
                .iter()
                .any(|m| m.model_id == "gemini-3.1-flash-lite-preview")
        );
    }

    #[test]
    fn test_deepseek_known_catalog() {
        let models = OpenAiCompatibleVariant::DeepSeek.known_models("deepseek-chat");
        assert_eq!(models.len(), 2);
        assert!(models.iter().any(|m| m.model_id == "deepseek-v4-flash"));
        assert!(models.iter().any(|m| m.model_id == "deepseek-v4-pro"));
    }

    #[test]
    fn test_moonshot_known_catalog() {
        let models = OpenAiCompatibleVariant::Moonshot.known_models("kimi-k2.6");
        assert_eq!(models.len(), 2);
        assert!(models.iter().any(|m| m.model_id == "kimi-k2.6"));
        assert!(models.iter().any(|m| m.model_id == "moonshot-v1-128k"));
    }

    #[test]
    fn test_custom_single_entry() {
        let models = OpenAiCompatibleVariant::Custom {
            provider_id: "my-proxy".to_string(),
            display_name: "My Proxy".to_string(),
            context_window: None,
            supports_tools: None,
        }
        .known_models("custom-model");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].model_id, "custom-model");
        assert_eq!(models[0].provider_id, "my-proxy");
    }

    #[test]
    fn test_custom_metadata_overrides() {
        let models = OpenAiCompatibleVariant::Custom {
            provider_id: "local".to_string(),
            display_name: "Local".to_string(),
            context_window: Some(32_768),
            supports_tools: Some(false),
        }
        .known_models("custom-model");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].context_window, 32_768);
        assert!(models[0].capabilities.is_empty());
        assert_eq!(models[0].pricing_tier, Some("local".to_string()));
    }
}
