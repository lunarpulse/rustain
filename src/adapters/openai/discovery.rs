//! Pure parse + filter pipeline for OpenAI-compatible `/v1/models` responses.
//!
//! Story 7.6 AC3 — testable without HTTP; no I/O, no clock, minimal logging.
//! Story 7.7 AC2 — allowlist intersection removed; noise regex + user model_filter only.

use std::collections::HashSet;

use crate::adapters::openai::allowlists::{
    compile_filter_patterns, is_noise_model_id, matches_any, variant_default_context,
};
use crate::adapters::openai::types::{ModelsListItem, ModelsListResponse};
use crate::adapters::openai::variant::OpenAiCompatibleVariant;
use crate::domain::errors::ProviderError;
use crate::domain::models::provider::{ModelCapability, ModelDescriptor};

/// Parse a JSON payload and filter it through the noise regex and user `model_filter` globs.
///
/// Story 7.7: the allowlist AND-intersection gate is removed. For built-in
/// variants the JSON seed provides the initial catalog; the live fetch
/// replaces it entirely. For Custom, noise regex alone handles filtering.
pub fn parse_and_filter_models(
    payload: &str,
    variant: &OpenAiCompatibleVariant,
    model_filter: &[String],
) -> Result<Vec<ModelDescriptor>, ProviderError> {
    let response: ModelsListResponse = serde_json::from_str(payload)
        .map_err(|e| ProviderError::Other(format!("Failed to parse models response: {}", e)))?;

    let compiled_filter = &compile_filter_patterns(model_filter);
    let is_custom = matches!(variant, OpenAiCompatibleVariant::Custom { .. });
    let data_len = response.data.len();

    let mut result = Vec::new();
    for item in &response.data {
        // 1. Noise regex — applied to all variants; strip embedding/tts/image/customtools/preview noise
        if is_noise_model_id(&item.id) {
            tracing::debug!("Skipping noisy model id: {}", item.id);
            continue;
        }

        // 2. User model_filter AND-intersection
        if !matches_any(compiled_filter, &item.id) {
            tracing::debug!("Model {} does not match user model_filter", item.id);
            continue;
        }

        // 3. Map to ModelDescriptor
        let capabilities = match &item.supported_parameters {
            None => HashSet::from([ModelCapability::ToolUse]),
            Some(params) => {
                if params.iter().any(|p| p == "tools") {
                    HashSet::from([ModelCapability::ToolUse])
                } else {
                    HashSet::new()
                }
            }
        };

        result.push(ModelDescriptor {
            model_id: item.id.clone(),
            display_name: item.name.clone().unwrap_or_else(|| item.id.clone()),
            provider_id: variant.provider_id().to_string(),
            context_window: item
                .context_length
                .or_else(|| variant.context_window_for(&item.id))
                .unwrap_or_else(|| variant_default_context(variant)),
            capabilities,
            pricing_tier: None,
            stale: false,
        });
    }

    // For Custom variants, if filtering produced nothing, warn but return empty (caller decides)
    if is_custom && result.is_empty() && data_len > 0 {
        tracing::warn!(
            "All {} models from Custom provider '{}' were filtered out by noise regex or model_filter",
            data_len,
            variant.provider_id()
        );
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_item(id: &str) -> ModelsListItem {
        ModelsListItem {
            id: id.to_string(),
            name: None,
            context_length: None,
            supported_parameters: None,
            object: None,
        }
    }

    fn mk_item_with_tools(id: &str) -> ModelsListItem {
        ModelsListItem {
            id: id.to_string(),
            name: None,
            context_length: None,
            supported_parameters: Some(vec!["tools".to_string()]),
            object: None,
        }
    }

    #[test]
    fn parse_minimal_openai_shape() {
        let payload = r#"{"data":[{"id":"gpt-4o"},{"id":"text-embedding-3"}]}"#;
        let result = parse_and_filter_models(
            payload,
            &OpenAiCompatibleVariant::OpenAI,
            &["*".to_string()],
        )
        .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].model_id, "gpt-4o");
    }

    #[test]
    fn parse_openrouter_with_tools() {
        let payload = r#"{"data":[
            {"id":"anthropic/claude-3.5-sonnet","supported_parameters":["tools"]},
            {"id":"anthropic/claude-3-haiku","supported_parameters":["top_p"]}
        ]}"#;
        let result = parse_and_filter_models(
            payload,
            &OpenAiCompatibleVariant::OpenRouter,
            &["*".to_string()],
        )
        .unwrap();
        // claude-3.5-sonnet has tools => ToolUse capability
        // claude-3-haiku does NOT have tools => empty capabilities but still kept
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].model_id, "anthropic/claude-3.5-sonnet");
        assert!(result[0].capabilities.contains(&ModelCapability::ToolUse));
        assert_eq!(result[1].model_id, "anthropic/claude-3-haiku");
        assert!(result[1].capabilities.is_empty());
    }

    #[test]
    fn parse_uses_name_field() {
        let payload = r#"{"data":[{"id":"gpt-4o","name":"GPT-4 Omni"}]}"#;
        let result = parse_and_filter_models(
            payload,
            &OpenAiCompatibleVariant::OpenAI,
            &["*".to_string()],
        )
        .unwrap();
        assert_eq!(result[0].display_name, "GPT-4 Omni");
    }

    #[test]
    fn parse_fallback_display_name() {
        let payload = r#"{"data":[{"id":"gpt-4o"}]}"#;
        let result = parse_and_filter_models(
            payload,
            &OpenAiCompatibleVariant::OpenAI,
            &["*".to_string()],
        )
        .unwrap();
        assert_eq!(result[0].display_name, "gpt-4o");
    }

    #[test]
    fn parse_malformed_json() {
        let payload = "{garbage}";
        let result = parse_and_filter_models(
            payload,
            &OpenAiCompatibleVariant::OpenAI,
            &["*".to_string()],
        );
        assert!(result.is_err());
    }

    #[test]
    fn parse_empty_data() {
        let payload = r#"{"data":[]}"#;
        let result = parse_and_filter_models(
            payload,
            &OpenAiCompatibleVariant::OpenAI,
            &["*".to_string()],
        )
        .unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn noise_filter_removes_embedding() {
        let payload = r#"{"data":[
            {"id":"gpt-4o"},
            {"id":"text-embedding-3"}
        ]}"#;
        let result = parse_and_filter_models(
            payload,
            &OpenAiCompatibleVariant::OpenAI,
            &["*".to_string()],
        )
        .unwrap();
        // text-embedding-3 is stripped by noise regex
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].model_id, "gpt-4o");
    }

    #[test]
    fn noise_filter_lets_all_good_models_through() {
        // Known model ids from the embedded JSON — none should be caught by noise regex.
        let good_ids = [
            "gpt-5.5",
            "gpt-5.4",
            "gpt-5.4-mini",
            "deepseek-v4-pro",
            "deepseek-v4-flash",
            "gemini-3.1-pro-preview",
            "kimi-k2.6",
            "moonshot-v1-128k",
        ];
        for id in &good_ids {
            let item = ModelsListItem {
                id: id.to_string(),
                name: None,
                context_length: None,
                supported_parameters: Some(vec!["tools".to_string()]),
                object: None,
            };
            let payload = serde_json::to_string(&ModelsListResponse { data: vec![item] }).unwrap();
            let result = parse_and_filter_models(
                &payload,
                &OpenAiCompatibleVariant::OpenAI,
                &["*".to_string()],
            )
            .unwrap();
            assert_eq!(
                result.len(),
                1,
                "good model id '{}' should survive filter",
                id
            );
        }
    }

    #[test]
    fn parse_idempotent_property() {
        // Fuzz: filter(filter(x)) == filter(x)
        let payload = r#"{"data":[
            {"id":"gpt-4o"},
            {"id":"gpt-4o-mini"},
            {"id":"text-embedding-3"},
            {"id":"o3-mini"},
            {"id":"unknown-model"}
        ]}"#;
        let first = parse_and_filter_models(
            payload,
            &OpenAiCompatibleVariant::OpenAI,
            &["*".to_string()],
        )
        .unwrap();
        // Serialize back and re-filter
        let repayload = serde_json::to_string(&ModelsListResponse {
            data: first
                .iter()
                .map(|m| ModelsListItem {
                    id: m.model_id.clone(),
                    name: Some(m.display_name.clone()),
                    context_length: Some(m.context_window),
                    supported_parameters: if m.capabilities.contains(&ModelCapability::ToolUse) {
                        Some(vec!["tools".to_string()])
                    } else {
                        Some(vec![])
                    },
                    object: None,
                })
                .collect(),
        })
        .unwrap();
        let second = parse_and_filter_models(
            &repayload,
            &OpenAiCompatibleVariant::OpenAI,
            &["*".to_string()],
        )
        .unwrap();
        assert_eq!(first.len(), second.len());
        for (a, b) in first.iter().zip(second.iter()) {
            assert_eq!(a.model_id, b.model_id);
        }
    }
}
