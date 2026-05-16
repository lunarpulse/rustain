//! Per-variant curated model allowlists and filter helpers.
//!
//! Story 7.6 AC2 — bundled allowlists are the first line of defense against
//! provider catalog noise. Each non-Custom variant ships a curated list of
//! model_ids that rustain knows are good defaults. The live `/v1/models`
//! response is intersected with this allowlist (AND semantics with the user
//! `model_filter` globs).

use std::collections::HashSet;

use once_cell::sync::Lazy;
use regex::Regex;

use super::variant::OpenAiCompatibleVariant;

/// Curated allowlist per variant. Custom returns `&[]` — the user is
/// signalling "trust the server's catalog" when they set `discover_models = true`
/// on a Custom provider (single-model local servers; their `list_models()` already
/// returns the right single descriptor).
pub fn allowlist_for(variant: &OpenAiCompatibleVariant) -> &'static [&'static str] {
    match variant {
        OpenAiCompatibleVariant::OpenRouter => &[
            "anthropic/claude-3.5-sonnet",
            "anthropic/claude-3-haiku",
            "openai/gpt-4o",
            "openai/gpt-4o-mini",
            "google/gemini-2.5-pro-preview",
            "deepseek/deepseek-chat",
            "deepseek/deepseek-reasoner",
        ],
        OpenAiCompatibleVariant::OpenAI => &[
            "gpt-4o",
            "gpt-4o-mini",
            "o1",
            "o3-mini",
            "gpt-4.1",
            "gpt-4.1-mini",
        ],
        OpenAiCompatibleVariant::Google => &["gemini-2.0-flash", "gemini-2.5-pro-preview-03-25"],
        OpenAiCompatibleVariant::DeepSeek => &["deepseek-chat", "deepseek-reasoner"],
        OpenAiCompatibleVariant::Moonshot => &["moonshot-v1-auto", "kimi-k2-instruct"],
        OpenAiCompatibleVariant::Custom { .. } => &[],
    }
}

/// Noise regex — drop any model id matching these patterns.
/// Verbatim from hermes-agent `_NOISE_PATTERNS` at `agent/models_dev.py:436-441`.
static NOISE_PATTERN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)-tts\b|embedding|live-|-(preview|exp)-\d{2,4}[-_]|-image\b|-image-preview\b|-customtools\b")
        .expect("hard-coded noise regex is valid")
});

/// Returns true when the model id matches a noise pattern (should be dropped).
pub fn is_noise_model_id(id: &str) -> bool {
    NOISE_PATTERN.is_match(id)
}

/// Compile user `model_filter` globs into a `globset::GlobSet`.
/// Invalid patterns are warn-logged and skipped (graceful — never fail startup).
pub fn compile_filter_patterns(patterns: &[String]) -> globset::GlobSet {
    let mut builder = globset::GlobSetBuilder::new();
    for p in patterns {
        match globset::Glob::new(p) {
            Ok(glob) => {
                builder.add(glob);
            }
            Err(e) => {
                tracing::warn!("Invalid model_filter glob '{}': {}; skipping", p, e);
            }
        }
    }
    builder
        .build()
        .unwrap_or_else(|_| globset::GlobSet::empty())
}

/// Returns true when `s` matches any compiled pattern.
/// An empty compiled set is treated as match-everything (accept all).
pub fn matches_any(compiled: &globset::GlobSet, s: &str) -> bool {
    compiled.is_empty() || compiled.is_match(s)
}

/// Default context window for a variant, used when the live response
/// does not include a `context_length` field.
pub fn variant_default_context(variant: &OpenAiCompatibleVariant) -> u32 {
    match variant {
        OpenAiCompatibleVariant::OpenAI => 128_000,
        OpenAiCompatibleVariant::OpenRouter => 128_000,
        OpenAiCompatibleVariant::Google => 1_048_576,
        OpenAiCompatibleVariant::DeepSeek => 64_000,
        OpenAiCompatibleVariant::Moonshot => 128_000,
        OpenAiCompatibleVariant::Custom { context_window, .. } => context_window.unwrap_or(8_192),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_openrouter_verbatim() {
        let list = allowlist_for(&OpenAiCompatibleVariant::OpenRouter);
        assert_eq!(
            list,
            &[
                "anthropic/claude-3.5-sonnet",
                "anthropic/claude-3-haiku",
                "openai/gpt-4o",
                "openai/gpt-4o-mini",
                "google/gemini-2.5-pro-preview",
                "deepseek/deepseek-chat",
                "deepseek/deepseek-reasoner",
            ]
        );
    }

    #[test]
    fn allowlist_openai_verbatim() {
        let list = allowlist_for(&OpenAiCompatibleVariant::OpenAI);
        assert_eq!(
            list,
            &[
                "gpt-4o",
                "gpt-4o-mini",
                "o1",
                "o3-mini",
                "gpt-4.1",
                "gpt-4.1-mini",
            ]
        );
    }

    #[test]
    fn allowlist_google_verbatim() {
        let list = allowlist_for(&OpenAiCompatibleVariant::Google);
        assert_eq!(list, &["gemini-2.0-flash", "gemini-2.5-pro-preview-03-25"]);
    }

    #[test]
    fn allowlist_deepseek_verbatim() {
        let list = allowlist_for(&OpenAiCompatibleVariant::DeepSeek);
        assert_eq!(list, &["deepseek-chat", "deepseek-reasoner"]);
    }

    #[test]
    fn allowlist_moonshot_verbatim() {
        let list = allowlist_for(&OpenAiCompatibleVariant::Moonshot);
        assert_eq!(list, &["moonshot-v1-auto", "kimi-k2-instruct"]);
    }

    #[test]
    fn allowlist_custom_is_empty() {
        let list = allowlist_for(&OpenAiCompatibleVariant::Custom {
            provider_id: "local".to_string(),
            display_name: "Local".to_string(),
            context_window: None,
            supports_tools: None,
        });
        assert!(list.is_empty());
    }

    #[test]
    fn all_allowlists_unique() {
        let variants = [
            OpenAiCompatibleVariant::OpenRouter,
            OpenAiCompatibleVariant::OpenAI,
            OpenAiCompatibleVariant::Google,
            OpenAiCompatibleVariant::DeepSeek,
            OpenAiCompatibleVariant::Moonshot,
        ];
        for v in &variants {
            let list = allowlist_for(v);
            let set: HashSet<&str> = list.iter().copied().collect();
            assert_eq!(
                set.len(),
                list.len(),
                "allowlist for {:?} contains duplicates",
                v
            );
        }
    }

    #[test]
    fn noise_pattern_matches() {
        assert!(is_noise_model_id("openai/gpt-4o-tts"));
        assert!(is_noise_model_id("text-embedding-ada-002"));
        assert!(is_noise_model_id("gemini-2.0-live-001"));
        assert!(is_noise_model_id("gpt-4-preview-1106-vision"));
        assert!(is_noise_model_id("some-image-preview-model"));
        assert!(is_noise_model_id("my-customtools-model"));
    }

    #[test]
    fn noise_pattern_does_not_match_good_models() {
        assert!(!is_noise_model_id("gpt-3.5-turbo"));
        assert!(!is_noise_model_id("anthropic/claude-3.5-sonnet"));
        assert!(!is_noise_model_id("deepseek-chat"));
        assert!(!is_noise_model_id("gpt-4o"));
    }

    #[test]
    fn globset_star_matches_everything() {
        let compiled = compile_filter_patterns(&["*".to_string()]);
        assert!(matches_any(&compiled, "anything"));
        assert!(matches_any(&compiled, "anthropic/claude-3.5-sonnet"));
    }

    #[test]
    fn globset_prefix_match() {
        let compiled = compile_filter_patterns(&["openai/gpt-4*".to_string()]);
        assert!(matches_any(&compiled, "openai/gpt-4o"));
        assert!(matches_any(&compiled, "openai/gpt-4o-mini"));
        assert!(!matches_any(&compiled, "anthropic/claude-3.5-sonnet"));
    }

    #[test]
    fn globset_brace_expansion() {
        let compiled = compile_filter_patterns(&["{anthropic,openai}/*".to_string()]);
        assert!(matches_any(&compiled, "anthropic/claude-3-haiku"));
        assert!(matches_any(&compiled, "openai/gpt-4o"));
        assert!(!matches_any(&compiled, "deepseek/deepseek-chat"));
    }

    #[test]
    fn globset_invalid_pattern_skipped() {
        let compiled = compile_filter_patterns(&["[".to_string(), "openai/*".to_string()]);
        // One valid pattern survives
        assert!(matches_any(&compiled, "openai/gpt-4o"));
        assert!(!matches_any(&compiled, "deepseek/deepseek-chat"));
    }

    #[test]
    fn globset_empty_slice_matches_all() {
        let compiled = compile_filter_patterns(&[]);
        assert!(matches_any(&compiled, "anything"));
    }

    #[test]
    fn variant_default_context_values() {
        assert_eq!(
            variant_default_context(&OpenAiCompatibleVariant::OpenAI),
            128_000
        );
        assert_eq!(
            variant_default_context(&OpenAiCompatibleVariant::OpenRouter),
            128_000
        );
        assert_eq!(
            variant_default_context(&OpenAiCompatibleVariant::Google),
            1_048_576
        );
        assert_eq!(
            variant_default_context(&OpenAiCompatibleVariant::DeepSeek),
            64_000
        );
        assert_eq!(
            variant_default_context(&OpenAiCompatibleVariant::Moonshot),
            128_000
        );
        assert_eq!(
            variant_default_context(&OpenAiCompatibleVariant::Custom {
                provider_id: "x".to_string(),
                display_name: "X".to_string(),
                context_window: None,
                supports_tools: None,
            }),
            8_192
        );
    }
}
