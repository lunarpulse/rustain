//! Provider-specific defaults used when all catalog layers omit context.

use super::variant::OpenAiCompatibleVariant;

/// Default context window for a variant, used when the live response
/// does not include a `context_length` field.
pub fn variant_default_context(variant: &OpenAiCompatibleVariant) -> u32 {
    match variant {
        OpenAiCompatibleVariant::OpenAI => 128_000,
        OpenAiCompatibleVariant::OpenRouter => 128_000,
        OpenAiCompatibleVariant::Google => 1_048_576,
        OpenAiCompatibleVariant::DeepSeek => 1_048_576,
        OpenAiCompatibleVariant::Moonshot => 128_000,
        OpenAiCompatibleVariant::Custom { context_window, .. } => context_window.unwrap_or(8_192),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            1_048_576
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
