//! Pure parse + filter pipeline for OpenAI-compatible `/v1/models` responses.
//!
//! Story 7.6 AC3 — testable without HTTP; no I/O, no clock, minimal logging.
//! Story 7.7 AC2 — allowlist intersection removed; noise regex + user model_filter only.
//!
//! The OpenRouter roster rule (tool-capable rows only; image input and reasoning
//! mapped) is adapted from oh-my-pi's `catalog/src/provider-models/openai-compat.ts`.
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

use std::collections::HashSet;

use crate::adapters::model_filter::{compile_filter_patterns, is_noise_model_id, matches_any};
use crate::adapters::openai::types::{ModelsListItem, ModelsListResponse};
use crate::adapters::openai::variant::OpenAiCompatibleVariant;
use crate::domain::errors::ProviderError;
use crate::domain::models::provider::{ModelCapability, ModelRecord};

/// Parse a JSON payload and filter it through the noise regex and user `model_filter` globs.
///
/// Story 7.7: the allowlist AND-intersection gate is removed. For built-in
/// variants the JSON seed provides the initial catalog; the live fetch
/// replaces it entirely. For Custom, noise regex alone handles filtering.
pub fn parse_and_filter_models(
    payload: &str,
    variant: &OpenAiCompatibleVariant,
    model_filter: &[String],
) -> Result<Vec<ModelRecord>, ProviderError> {
    let response: ModelsListResponse = serde_json::from_str(payload)
        .map_err(|e| ProviderError::Other(format!("Failed to parse models response: {}", e)))?;

    let compiled_filter = &compile_filter_patterns(model_filter);
    let is_custom = matches!(variant, OpenAiCompatibleVariant::Custom { .. });
    let openrouter = matches!(variant, OpenAiCompatibleVariant::OpenRouter);
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

        // OpenRouter serves only rows that can call tools; a row without
        // `supported_parameters` is dropped too.
        if openrouter && !has_parameter(item, "tools") {
            tracing::debug!("Skipping OpenRouter model without tools: {}", item.id);
            continue;
        }

        // Preserve absent metadata so lower catalog layers can fill it.
        let capabilities = if openrouter {
            Some(openrouter_capabilities(item))
        } else {
            item.supported_parameters.as_ref().map(|params| {
                if params.iter().any(|p| p == "tools") {
                    HashSet::from([ModelCapability::ToolUse])
                } else {
                    HashSet::new()
                }
            })
        };

        result.push(ModelRecord {
            model_id: item.id.clone(),
            display_name: item.name.clone(),
            context_window: item.context_length,
            capabilities,
            pricing_tier: None,
            wire_model_id: None,
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

fn has_parameter(item: &ModelsListItem, name: &str) -> bool {
    item.supported_parameters
        .as_ref()
        .is_some_and(|params| params.iter().any(|p| p == name))
}

/// A kept OpenRouter row calls tools; it sees images when its input modalities
/// (else its older `modality` string) name `image`, and reasons when
/// `supported_parameters` lists `reasoning`.
fn openrouter_capabilities(item: &ModelsListItem) -> HashSet<ModelCapability> {
    let mut capabilities = HashSet::from([ModelCapability::ToolUse]);
    let vision = item.architecture.as_ref().is_some_and(|architecture| {
        match &architecture.input_modalities {
            Some(modalities) => modalities.iter().any(|m| m == "image"),
            None => architecture
                .modality
                .as_deref()
                .is_some_and(|modality| modality.contains("image")),
        }
    });
    if vision {
        capabilities.insert(ModelCapability::Vision);
    }
    if has_parameter(item, "reasoning") {
        capabilities.insert(ModelCapability::Thinking);
    }
    capabilities
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(result[0].display_name, None);
        assert_eq!(result[0].context_window, None);
        assert_eq!(result[0].capabilities, None);
    }

    #[test]
    fn parse_openrouter_keeps_only_tool_rows() {
        // Deliberately supersedes Story 7.7's keep-all assertion for OpenRouter:
        // omp serves only rows that list `tools`.
        let payload = r#"{"data":[
            {"id":"anthropic/claude-3.5-sonnet","supported_parameters":["tools"]},
            {"id":"anthropic/claude-3-haiku","supported_parameters":["top_p"]},
            {"id":"meta/no-parameters"}
        ]}"#;
        let result = parse_and_filter_models(
            payload,
            &OpenAiCompatibleVariant::OpenRouter,
            &["*".to_string()],
        )
        .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].model_id, "anthropic/claude-3.5-sonnet");
        assert_eq!(
            result[0].capabilities,
            Some(HashSet::from([ModelCapability::ToolUse]))
        );
    }

    #[test]
    fn parse_openrouter_maps_image_input_and_reasoning() {
        let payload = r#"{"data":[
            {"id":"a/vision","supported_parameters":["tools"],"architecture":{"input_modalities":["text","image"]}},
            {"id":"a/text","supported_parameters":["tools","reasoning"],"architecture":{"input_modalities":["text"],"modality":"text+image->text"}},
            {"id":"a/legacy","supported_parameters":["tools"],"architecture":{"modality":"text+image->text"}}
        ]}"#;
        let result = parse_and_filter_models(
            payload,
            &OpenAiCompatibleVariant::OpenRouter,
            &["*".to_string()],
        )
        .unwrap();
        let caps = |i: usize| result[i].capabilities.clone().unwrap();
        assert_eq!(
            caps(0),
            HashSet::from([ModelCapability::ToolUse, ModelCapability::Vision])
        );
        // `input_modalities` wins over the older `modality` string.
        assert_eq!(
            caps(1),
            HashSet::from([ModelCapability::ToolUse, ModelCapability::Thinking])
        );
        assert_eq!(
            caps(2),
            HashSet::from([ModelCapability::ToolUse, ModelCapability::Vision])
        );
    }

    #[test]
    fn other_variants_keep_rows_without_tools() {
        let payload = r#"{"data":[
            {"id":"gpt-chat","supported_parameters":["top_p"],"architecture":{"input_modalities":["image"]}},
            {"id":"gpt-other"}
        ]}"#;
        let result = parse_and_filter_models(
            payload,
            &OpenAiCompatibleVariant::OpenAI,
            &["*".to_string()],
        )
        .unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].capabilities, Some(HashSet::new()));
        assert_eq!(result[1].capabilities, None);
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
        assert_eq!(result[0].display_name.as_deref(), Some("GPT-4 Omni"));
    }

    #[test]
    fn parse_preserves_absent_display_name() {
        let payload = r#"{"data":[{"id":"gpt-4o"}]}"#;
        let result = parse_and_filter_models(
            payload,
            &OpenAiCompatibleVariant::OpenAI,
            &["*".to_string()],
        )
        .unwrap();
        assert_eq!(result[0].display_name, None);
    }

    #[test]
    fn parse_preserves_explicit_zero_and_id_name_for_merge() {
        let result = parse_and_filter_models(
            r#"{"data":[{"id":"gpt-chat","name":"gpt-chat","context_length":0,"supported_parameters":[]}]}"#,
            &OpenAiCompatibleVariant::OpenAI,
            &["*".to_string()],
        ).unwrap();
        assert_eq!(result[0].display_name.as_deref(), Some("gpt-chat"));
        assert_eq!(result[0].context_window, Some(0));
        assert_eq!(result[0].capabilities, Some(HashSet::new()));
        assert_eq!(result[0].pricing_tier, None);
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
                architecture: None,
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
                    name: m.display_name.clone(),
                    context_length: m.context_window,
                    supported_parameters: m.capabilities.as_ref().map(|capabilities| {
                        if capabilities.contains(&ModelCapability::ToolUse) {
                            vec!["tools".to_string()]
                        } else {
                            vec![]
                        }
                    }),
                    object: None,
                    architecture: None,
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
