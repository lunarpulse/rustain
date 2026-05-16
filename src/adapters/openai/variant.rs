//! OpenAI-compatible provider variants.
//!
//! The OpenAI-compatible adapter supports multiple providers that implement
//! the OpenAI Chat Completions API with different base URLs and model catalogs.
//!
//! | Variant | provider_id | Default base URL | Hard-coded catalog |
//! |---|---|---|---|
//! | OpenAI | `openai` | `https://api.openai.com/v1` | gpt-4o-2024-11-20, gpt-4o-mini-2024-07-18, o1-2024-12-17, o3-mini-2025-01-31 |
//! | OpenRouter | `openrouter` | `https://openrouter.ai/api/v1` | curated 7-model allowlist |
//! | Google AI | `google` | `https://generativelanguage.googleapis.com/v1beta/openai` | gemini-2.0-flash, gemini-2.5-pro-preview-03-25 |
//! | DeepSeek | `deepseek` | `https://api.deepseek.com/v1` | deepseek-chat, deepseek-reasoner |
//! | Moonshot/Kimi | `moonshot` | `https://api.moonshot.cn/v1` | moonshot-v1-auto, kimi-k2-instruct |

use crate::domain::models::provider::{ModelCapability, ModelDescriptor};
use super::allowlists;

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
    /// For built-in variants this is the curated allowlist (Tier-0 fallback).
    /// For `Custom`, returns a single descriptor for the configured model.
    pub fn known_models(&self, configured_model: &str) -> Vec<ModelDescriptor> {
        let provider_id = self.provider_id().to_string();
        match self {
            Self::OpenAI => vec![
                ModelDescriptor {
                    model_id: "gpt-4o-2024-11-20".to_string(),
                    display_name: "GPT-4o".to_string(),
                    provider_id: provider_id.clone(),
                    context_window: 128_000,
                    capabilities: std::collections::HashSet::from([
                        ModelCapability::ToolUse,
                        ModelCapability::Vision,
                    ]),
                    pricing_tier: Some("flagship".to_string()),
                stale: false,
                },
                ModelDescriptor {
                    model_id: "gpt-4o-mini-2024-07-18".to_string(),
                    display_name: "GPT-4o Mini".to_string(),
                    provider_id: provider_id.clone(),
                    context_window: 128_000,
                    capabilities: std::collections::HashSet::from([
                        ModelCapability::ToolUse,
                        ModelCapability::Vision,
                    ]),
                    pricing_tier: Some("cheap".to_string()),
                stale: false,
                },
                ModelDescriptor {
                    model_id: "o1-2024-12-17".to_string(),
                    display_name: "O1".to_string(),
                    provider_id: provider_id.clone(),
                    context_window: 200_000,
                    capabilities: std::collections::HashSet::from([ModelCapability::ToolUse]),
                    pricing_tier: Some("flagship".to_string()),
                stale: false,
                },
                ModelDescriptor {
                    model_id: "o3-mini-2025-01-31".to_string(),
                    display_name: "O3 Mini".to_string(),
                    provider_id: provider_id.clone(),
                    context_window: 200_000,
                    capabilities: std::collections::HashSet::from([ModelCapability::ToolUse]),
                    pricing_tier: Some("cheap".to_string()),
                stale: false,
                },
            ],
            Self::OpenRouter => {
                allowlists::allowlist_for(self)
                    .iter()
                    .map(|&id| ModelDescriptor {
                        model_id: id.to_string(),
                        display_name: id.to_string(),
                        provider_id: provider_id.clone(),
                        context_window: allowlists::variant_default_context(self),
                        capabilities: std::collections::HashSet::from([ModelCapability::ToolUse]),
                        pricing_tier: None,
                        stale: false,
                    })
                    .collect()
            }
            Self::Custom {
                provider_id: _,
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
                    provider_id,
                    context_window: context_window.unwrap_or(8_192),
                    capabilities: caps,
                    pricing_tier: Some("local".to_string()),
                stale: false,
                }]
            }
            Self::Google => vec![
                ModelDescriptor {
                    model_id: "gemini-2.0-flash".to_string(),
                    display_name: "Gemini 2.0 Flash".to_string(),
                    provider_id: provider_id.clone(),
                    context_window: 1_048_576,
                    capabilities: std::collections::HashSet::from([
                        ModelCapability::ToolUse,
                        ModelCapability::Vision,
                    ]),
                    pricing_tier: Some("flagship".to_string()),
                stale: false,
                },
                ModelDescriptor {
                    model_id: "gemini-2.5-pro-preview-03-25".to_string(),
                    display_name: "Gemini 2.5 Pro Preview".to_string(),
                    provider_id,
                    context_window: 2_097_152,
                    capabilities: std::collections::HashSet::from([
                        ModelCapability::ToolUse,
                        ModelCapability::Vision,
                    ]),
                    pricing_tier: Some("flagship".to_string()),
                stale: false,
                },
            ],
            Self::DeepSeek => vec![
                ModelDescriptor {
                    model_id: "deepseek-chat".to_string(),
                    display_name: "DeepSeek Chat".to_string(),
                    provider_id: provider_id.clone(),
                    context_window: 64_000,
                    capabilities: std::collections::HashSet::from([ModelCapability::ToolUse]),
                    pricing_tier: Some("cheap".to_string()),
                stale: false,
                },
                ModelDescriptor {
                    model_id: "deepseek-reasoner".to_string(),
                    display_name: "DeepSeek Reasoner".to_string(),
                    provider_id,
                    context_window: 64_000,
                    capabilities: std::collections::HashSet::from([ModelCapability::ToolUse]),
                    pricing_tier: Some("flagship".to_string()),
                stale: false,
                },
            ],
            Self::Moonshot => vec![
                ModelDescriptor {
                    model_id: "moonshot-v1-auto".to_string(),
                    display_name: "Moonshot V1 Auto".to_string(),
                    provider_id: provider_id.clone(),
                    context_window: 128_000,
                    capabilities: std::collections::HashSet::from([ModelCapability::ToolUse]),
                    pricing_tier: Some("flagship".to_string()),
                stale: false,
                },
                ModelDescriptor {
                    model_id: "kimi-k2-instruct".to_string(),
                    display_name: "Kimi K2 Instruct".to_string(),
                    provider_id,
                    context_window: 128_000,
                    capabilities: std::collections::HashSet::from([ModelCapability::ToolUse]),
                    pricing_tier: Some("flagship".to_string()),
                stale: false,
                },
            ],
        }
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
        let models = OpenAiCompatibleVariant::OpenAI.known_models("gpt-4o");
        assert_eq!(models.len(), 4);
        assert!(models.iter().any(|m| m.model_id == "gpt-4o-2024-11-20"));
        assert!(models.iter().any(|m| m.model_id == "o3-mini-2025-01-31"));
        for m in &models {
            assert_eq!(m.provider_id, "openai");
            assert!(m.capabilities.contains(&ModelCapability::ToolUse));
        }
    }

    #[test]
    fn test_openrouter_returns_curated_allowlist() {
        let models =
            OpenAiCompatibleVariant::OpenRouter.known_models("anthropic/claude-3.5-sonnet");
        // Should return the full 7-model curated allowlist, not just the configured model.
        assert_eq!(models.len(), 7);
        assert!(models.iter().any(|m| m.model_id == "anthropic/claude-3.5-sonnet"));
        assert!(models.iter().any(|m| m.model_id == "openai/gpt-4o"));
        assert!(models.iter().all(|m| m.provider_id == "openrouter"));
    }

    #[test]
    fn test_google_known_catalog() {
        let models = OpenAiCompatibleVariant::Google.known_models("gemini-2.0-flash");
        assert_eq!(models.len(), 2);
        assert!(models.iter().any(|m| m.model_id == "gemini-2.0-flash"));
        assert!(
            models
                .iter()
                .any(|m| m.model_id == "gemini-2.5-pro-preview-03-25")
        );
    }

    #[test]
    fn test_deepseek_known_catalog() {
        let models = OpenAiCompatibleVariant::DeepSeek.known_models("deepseek-chat");
        assert_eq!(models.len(), 2);
        assert!(models.iter().any(|m| m.model_id == "deepseek-chat"));
        assert!(models.iter().any(|m| m.model_id == "deepseek-reasoner"));
    }

    #[test]
    fn test_moonshot_known_catalog() {
        let models = OpenAiCompatibleVariant::Moonshot.known_models("moonshot-v1-auto");
        assert_eq!(models.len(), 2);
        assert!(models.iter().any(|m| m.model_id == "moonshot-v1-auto"));
        assert!(models.iter().any(|m| m.model_id == "kimi-k2-instruct"));
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
