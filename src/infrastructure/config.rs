//! Layered configuration loader.
//!
//! Resolves `AppConfig` from a stack of layered providers using `figment`
//! (Epic 7 retro AI-7.2 / Path A). Field-level merge is the binding behavior
//! per Epic 8 Story 8.1 AC — adding `[pricing."my-model"]` in a user file
//! ADDS to the default pricing catalog instead of REPLACING it. The same
//! holds for `[provider.X]` and `[router.step_tiers]`.
//!
//! Layer order (later layers override earlier layers at the key level):
//!
//! 1. **Built-in defaults** — `AppConfig::default()` serialized into the merge
//!    chain via `Serialized::defaults(...)`. Provides the curated pricing
//!    catalog, default router step→tier mapping, and empty provider map.
//! 2. **User-global config** — `~/.config/rustain/config.toml` if present.
//! 3. **Workspace config** — `<cwd>/.rustain/config.toml` if present.
//!
//! Epic 8 Story 8.1 will extend the chain to 7 layers (CLI > env > local
//! override > workspace > user-global > profile defaults > built-ins). The
//! merge engine is already in place; Story 8.1 only adds providers.

use std::path::Path;

use figment::Figment;
use figment::providers::{Format, Serialized, Toml};

use crate::domain::models::AppConfig;

/// Load application configuration via the layered figment merge chain.
///
/// Returns `AppConfig::default()` if no config files are present and no
/// figment errors occur. Malformed files trigger a `tracing::error!` and
/// fall through to the next layer (file is skipped, not fatal).
///
/// INVARIANT: Missing config file must return defaults, never error on absence.
/// INVARIANT: A malformed config file must NOT panic — it warns and falls through.
pub fn load() -> AppConfig {
    let mut figment = Figment::from(Serialized::defaults(AppConfig::default()));

    // Layer 2: User-global config (~/.config/rustain/config.toml)
    if let Some(home) = dirs::home_dir() {
        let home_config = home.join(".config").join("rustain").join("config.toml");
        figment = merge_if_valid(figment, &home_config, "user-global");
    }

    // Layer 3: Workspace config (<cwd>/.rustain/config.toml)
    if let Ok(cwd) = std::env::current_dir() {
        let ws_config = cwd.join(".rustain").join("config.toml");
        figment = merge_if_valid(figment, &ws_config, "workspace");
    }

    match figment.extract::<AppConfig>() {
        Ok(mut config) => {
            // Post-deserialization validation: layout.auto_panels has a small
            // enum allow-list; bad values fall back to defaults for that key.
            if let Err(e) = config.layout.auto_panels.validate() {
                tracing::warn!(
                    "Config layout.auto_panels has invalid value: {} — \
                     falling back to default for that key.",
                    e
                );
                config.layout.auto_panels = Default::default();
            }
            config
        }
        Err(e) => {
            tracing::error!(
                "Layered config extraction failed: {}. Falling back to defaults. \
                 Run `rustain doctor` to diagnose.",
                e
            );
            AppConfig::default()
        }
    }
}

/// Merge a TOML file into the figment if it exists AND parses. If the file
/// is unreadable or malformed, log and skip — do not fail the load.
///
/// `label` is a short human-readable layer name used in tracing messages
/// (e.g., "user-global", "workspace").
fn merge_if_valid(figment: Figment, path: &Path, label: &str) -> Figment {
    if !path.exists() {
        return figment;
    }

    // Pre-parse the file to catch malformed TOML before merging. figment's
    // `Toml::file` is lazy and would surface parse errors at extract time,
    // bundled with potentially-unrelated extraction errors. The pre-parse
    // gives us a clean per-file error message and skip-the-file semantics
    // identical to the legacy loader.
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(
                "Config file unreadable at {} ({}): {}. Skipping {} layer.",
                path.display(),
                label,
                e,
                label
            );
            return figment;
        }
    };
    if let Err(e) = toml::from_str::<toml::Value>(&content) {
        tracing::error!(
            "Config file at {} ({}) is malformed: {}. Skipping {} layer. \
             Fix the file or run `rustain doctor` to diagnose.",
            path.display(),
            label,
            e,
            label
        );
        return figment;
    }

    tracing::info!("Merging {} config layer from {}", label, path.display());
    figment.merge(Toml::file(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use figment::providers::Toml;

    /// Helper: construct a figment from defaults + a TOML string layer.
    /// Mirrors what `load()` does internally, but with a synthetic string
    /// layer instead of a file — so tests don't touch the filesystem.
    fn figment_with_user_layer(user_toml: &str) -> Figment {
        Figment::from(Serialized::defaults(AppConfig::default()))
            .merge(Toml::string(user_toml))
    }

    /// AI-7.2 Path A canary: user-supplied `[pricing."my-model"]` MUST add to
    /// the default pricing catalog, NOT replace it. This is the binding
    /// behavior for Epic 8 Story 8.1's "field-level merge" AC and the bug
    /// DF-S75-3 documented.
    ///
    /// Uses snake_case canonical form (post-Epic-7 AI-7.2 figment fix). In a
    /// figment-merge context, fields with serde aliases (camelCase) for a key
    /// that is ALSO in the defaults layer will produce a "duplicate field"
    /// error — the alias only works when the same logical key isn't present
    /// in a prior layer. Users overriding any of the 11 default-catalog
    /// entries MUST use snake_case canonical form.
    #[test]
    fn pricing_user_entry_merges_into_default_catalog() {
        let user_toml = r#"
            model = "test-model"
            [pricing."my-custom-model"]
            input_per_million = 0.50
            output_per_million = 1.00
        "#;
        let config: AppConfig = figment_with_user_layer(user_toml)
            .extract()
            .expect("extract figment");

        // User's custom entry is present.
        let custom = config
            .pricing
            .get("my-custom-model")
            .expect("user-supplied my-custom-model present in merged catalog");
        assert_eq!(custom.input_per_million, 0.50);
        assert_eq!(custom.output_per_million, 1.00);

        // CRITICAL: default catalog entries are STILL present (merge, not replace).
        // Pre-AI-7.2 this would have been replaced — the entire 11-entry catalog
        // would be gone.
        assert!(
            config.pricing.contains_key("claude-sonnet-4-6"),
            "default Sonnet pricing must survive a user adding one custom entry \
             (DF-S75-3 / Epic 7 retro AI-7.2 / Path A figment merge)"
        );
        assert!(
            config.pricing.contains_key("gpt-4o"),
            "default GPT-4o pricing must survive a user adding one custom entry"
        );
        // Spot-check the default value came through correctly.
        let sonnet = config.pricing.get("claude-sonnet-4-6").unwrap();
        assert_eq!(sonnet.input_per_million, 3.00);
        assert_eq!(sonnet.output_per_million, 15.00);
    }

    /// Field-level merge within a struct: user-overriding ONE field of an
    /// existing pricing entry should preserve the other fields from the
    /// default. This is the "field-level (not whole-file replacement)"
    /// language from Story 8.1 AC at the struct level (not just at the
    /// HashMap-key level).
    #[test]
    fn pricing_partial_struct_override_preserves_other_fields() {
        // Canonical form is snake_case (post-Epic-7 AI-7.2 figment fix).
        let user_toml = r#"
            model = "test-model"
            [pricing."claude-sonnet-4-6"]
            input_per_million = 2.50
        "#;
        let config: AppConfig = figment_with_user_layer(user_toml)
            .extract()
            .expect("extract figment");

        let sonnet = config.pricing.get("claude-sonnet-4-6").unwrap();
        // User overrode input_per_million.
        assert_eq!(sonnet.input_per_million, 2.50);
        // Other fields fall through from the default catalog.
        assert_eq!(sonnet.output_per_million, 15.00);
        assert_eq!(sonnet.cache_creation_per_million, Some(3.75));
        assert_eq!(sonnet.cache_read_per_million, Some(0.30));
    }

    /// AI-7.2 Path A: user-supplied `[provider.X]` MUST merge into the
    /// default provider map (which is empty at present) rather than triggering
    /// any whole-map-replacement footgun. With `figment`, the user's entry
    /// is simply additive — there's nothing to clobber.
    #[test]
    fn provider_user_entry_merges_into_default_map() {
        let user_toml = r#"
            model = "test-model"
            [provider.openrouter]
            provider_id = "openrouter"
            model_id = "anthropic/claude-3.5-sonnet"
            api_key_env = "OPENROUTER_API_KEY"
        "#;
        let config: AppConfig = figment_with_user_layer(user_toml)
            .extract()
            .expect("extract figment");
        assert!(config.provider.contains_key("openrouter"));
        assert_eq!(
            config.provider["openrouter"].model_id,
            "anthropic/claude-3.5-sonnet"
        );
    }

    /// AI-7.2 Path A: user-supplied `[router.step_tiers] codegen = "..."` MUST
    /// merge into the 5-entry default step_tiers mapping, not replace it.
    /// Pre-AI-7.2 the 4 other default mappings would have been silently lost.
    #[test]
    fn router_step_tiers_user_override_merges_into_defaults() {
        use crate::domain::models::router::{ModelTier, StepKind};

        let user_toml = r#"
            model = "test-model"
            [router.step_tiers]
            codegen = "cheap_agentic"
        "#;
        let config: AppConfig = figment_with_user_layer(user_toml)
            .extract()
            .expect("extract figment");

        // User-overridden entry takes effect.
        assert_eq!(
            config.router.step_tiers.get(&StepKind::Codegen),
            Some(&ModelTier::CheapAgentic),
            "user override of codegen tier must take effect"
        );
        // CRITICAL: other default entries must STILL be present.
        // Pre-AI-7.2 this would have been replaced.
        assert_eq!(
            config.router.step_tiers.get(&StepKind::Plan),
            Some(&ModelTier::Flagship),
            "default plan→flagship mapping must survive a user override of codegen \
             (Epic 7 retro AI-7.2 / Path A figment merge)"
        );
        assert_eq!(
            config.router.step_tiers.get(&StepKind::Edit),
            Some(&ModelTier::CheapAgentic),
            "default edit→cheap_agentic mapping must survive"
        );
        assert_eq!(
            config.router.step_tiers.get(&StepKind::Review),
            Some(&ModelTier::Flagship),
            "default review→flagship mapping must survive"
        );
    }

    /// Layer ordering: later layers override earlier layers at the key level.
    /// Two layers both define the same model — later wins per-entry.
    #[test]
    fn pricing_later_layer_overrides_earlier_at_key_level() {
        let figment = Figment::from(Serialized::defaults(AppConfig::default()))
            .merge(Toml::string(
                r#"
                model = "test-model"
                [pricing."claude-sonnet-4-6"]
                input_per_million = 2.00
                output_per_million = 10.00
            "#,
            ))
            .merge(Toml::string(
                r#"
                [pricing."claude-sonnet-4-6"]
                input_per_million = 1.50
            "#,
            ));

        let config: AppConfig = figment.extract().expect("extract figment");
        let sonnet = config.pricing.get("claude-sonnet-4-6").unwrap();
        // Second layer wins for input_per_million.
        assert_eq!(sonnet.input_per_million, 1.50);
        // First layer's output_per_million flows through (second layer
        // didn't override it).
        assert_eq!(sonnet.output_per_million, 10.00);
    }

    /// Reproduces the user-reported regression: with figment loader, the
    /// `discover_models = true` flag on a real `[provider.X]` entry must
    /// survive the round-trip. If this test fails, dynamic model catalog
    /// discovery (Story 7-6) silently no-ops at startup.
    #[test]
    fn provider_discover_models_survives_figment_roundtrip() {
        let user_toml = r#"
            model = "deepseek/deepseek-v4-flash"

            [provider.openrouter]
            provider_id    = "openrouter"
            kind           = "openai-compatible"
            model_id       = "deepseek/deepseek-v4-flash"
            api_key_env    = "OPENROUTER_API_KEY"
            enabled        = true
            base_url       = "https://openrouter.ai/api/v1"
            discover_models = true
            context_window = 131072
            cache_ttl_seconds = 3600
        "#;
        let config: AppConfig = figment_with_user_layer(user_toml)
            .extract()
            .expect("extract figment");

        let or = config
            .provider
            .get("openrouter")
            .expect("openrouter provider must be present");
        assert!(
            or.discover_models,
            "discover_models = true MUST survive figment round-trip — \
             this is the Story 7-6 dynamic discovery trigger"
        );
        assert_eq!(or.kind.as_deref(), Some("openai-compatible"));
        assert_eq!(or.base_url.as_deref(), Some("https://openrouter.ai/api/v1"));
        assert_eq!(or.cache_ttl_seconds, 3600);
        assert_eq!(or.context_window, Some(131072));
    }

    /// Reproduces the user-reported multi-provider scenario: 5 openai-compatible
    /// providers, all with discover_models=true. All 5 must survive merge.
    #[test]
    fn five_openai_compatible_providers_all_survive_figment_merge() {
        let user_toml = r#"
            model = "deepseek/deepseek-v4-flash"

            [provider.openai]
            provider_id = "openai"
            kind = "openai-compatible"
            model_id = "gpt-5.5-pro"
            api_key_env = "OPENAI_API_KEY"
            enabled = true
            base_url = "https://api.openai.com/v1"
            discover_models = true

            [provider.openrouter]
            provider_id = "openrouter"
            kind = "openai-compatible"
            model_id = "deepseek/deepseek-v4-flash"
            api_key_env = "OPENROUTER_API_KEY"
            enabled = true
            base_url = "https://openrouter.ai/api/v1"
            discover_models = true

            [provider.deepseek]
            provider_id = "deepseek"
            kind = "openai-compatible"
            model_id = "deepseek-chat"
            api_key_env = "DEEPSEEK_API_KEY"
            base_url = "https://api.deepseek.com"
            enabled = true
            discover_models = true

            [provider.moonshot]
            provider_id = "moonshot"
            kind = "openai-compatible"
            model_id = "moonshot-v1-auto"
            api_key_env = "MOONSHOT_API_KEY"
            base_url = "https://api.kimi.com/coding/v1"
            enabled = true
            discover_models = true

            [provider.zhipu]
            provider_id = "zhipu"
            kind = "openai-compatible"
            model_id = "glm-4.7-flash"
            api_key_env = "ZHIPU_API_KEY"
            enabled = true
            base_url = "https://api.z.ai/api/coding/paas/v4"
            discover_models = true
            supports_tools = true
        "#;
        let config: AppConfig = figment_with_user_layer(user_toml)
            .extract()
            .expect("extract figment");

        for id in ["openai", "openrouter", "deepseek", "moonshot", "zhipu"] {
            let p = config
                .provider
                .get(id)
                .unwrap_or_else(|| panic!("provider '{id}' missing from merged config"));
            assert!(
                p.discover_models,
                "discover_models must survive for provider '{id}'"
            );
            assert_eq!(p.kind.as_deref(), Some("openai-compatible"), "kind for '{id}'");
            assert!(p.enabled, "enabled for '{id}'");
        }
        assert_eq!(config.provider.len(), 5, "exactly 5 providers expected");
    }

    /// Regression test for the bug that broke 7-6 dynamic discovery: when
    /// `BudgetConfig` had `rename_all = "camelCase"` + a snake_case alias,
    /// figment's merge produced a value tree with BOTH `dailyLimitUsd` (from
    /// the defaults layer) AND `daily_limit_usd` (from the user TOML layer),
    /// and serde rejected the resulting dict as "duplicate field." `load()`
    /// silently fell back to defaults, losing the user's entire config —
    /// providers, budget, tool_progress, model — everything.
    ///
    /// Fix: canonical form for BudgetConfig + PricingConfig is now snake_case
    /// (matches TOML idiom + user configs). camelCase remains as an alias for
    /// JSON-format imports but MUST NOT collide with a defaults-layer key.
    #[test]
    fn budget_snake_case_user_does_not_collide_with_defaults_layer() {
        let user_toml = r#"
            model = "test-model"
            [budget]
            daily_limit_usd = 10.00
        "#;
        // Full figment chain: defaults layer + user layer. If this errors,
        // the same regression that broke 7-6 has resurfaced.
        let config: AppConfig = figment_with_user_layer(user_toml)
            .extract()
            .expect(
                "merging defaults + user budget MUST NOT produce duplicate field — \
                 see Epic 7 retro AI-7.2 figment-fix root-cause analysis",
            );
        assert_eq!(config.budget.daily_limit_usd, Some(10.00));
    }

    /// Empty user file: extraction must produce the full default config.
    /// This is the "no config file" / "empty file" path.
    #[test]
    fn empty_user_layer_produces_default_config() {
        let config: AppConfig = figment_with_user_layer("")
            .extract()
            .expect("extract figment from empty user layer");
        // All 11 default pricing entries present.
        assert!(config.pricing.contains_key("claude-sonnet-4-6"));
        assert!(config.pricing.contains_key("gpt-4o"));
        assert!(config.pricing.contains_key("gemini-2.0-flash"));
        assert!(config.pricing.contains_key("deepseek-chat"));
        // Default model.
        assert_eq!(config.model, "claude-sonnet-4-6");
    }
}

