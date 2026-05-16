use serde::{Deserialize, Serialize};

use crate::domain::models::budget::BudgetConfig;
use crate::domain::models::pricing::PricingConfig;

/// Configuration for a single LLM provider.
///
/// `api_key_env` names an environment variable — the adapter reads
/// the actual key at startup; the domain config never stores secrets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Unique provider identifier (e.g., `"anthropic"`).
    pub provider_id: String,
    /// The model to use (e.g., `"claude-sonnet-4-6"`).
    pub model_id: String,
    /// Environment variable name that holds the API key or bearer token.
    pub api_key_env: String,
    /// Whether this provider is enabled.
    #[serde(default = "ProviderConfig::default_enabled")]
    pub enabled: bool,
    /// Adapter selector — `"anthropic"`, `"openai"`, `"openrouter"`, `"google"`,
    /// `"deepseek"`, `"moonshot"`, `"ollama"`, or `"openai-compatible"`.
    /// When absent, `provider_id` is used as the kind (back-compat).
    #[serde(default)]
    pub kind: Option<String>,
    /// Overrides the adapter's default endpoint URL.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Context window for single-model servers that cannot self-describe.
    #[serde(default)]
    pub context_window: Option<u32>,
    /// Whether the model supports tool use (for single-model servers).
    #[serde(default)]
    pub supports_tools: Option<bool>,
    /// Enable dynamic model discovery from /v1/models endpoint.
    #[serde(default)]
    pub discover_models: bool,
    /// Glob patterns to filter discovered models. AND-intersected with the bundled allowlist.
    #[serde(default = "ProviderConfig::default_model_filter")]
    pub model_filter: Vec<String>,
    /// Seconds before cached catalog is considered stale.
    #[serde(default = "ProviderConfig::default_cache_ttl_seconds")]
    pub cache_ttl_seconds: u64,
}

impl ProviderConfig {
    fn default_enabled() -> bool {
        true
    }

    fn default_model_filter() -> Vec<String> {
        vec!["*".to_string()]
    }

    fn default_cache_ttl_seconds() -> u64 {
        3600
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SkillsConfig {
    #[serde(default)]
    pub disabled: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventBusConfig {
    #[serde(default = "EventBusConfig::default_raw_capacity")]
    pub raw_capacity: usize,
}

impl EventBusConfig {
    fn default_raw_capacity() -> usize {
        1024
    }
}

impl Default for EventBusConfig {
    fn default() -> Self {
        Self { raw_capacity: 1024 }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuntimeConfig {
    #[serde(default)]
    pub event_bus: EventBusConfig,
}

/// Auto-open behavior for sidebar panels triggered by domain events.
///
/// Story 6.3 (PD4): `on_task_plan` controls whether the Tasks panel auto-opens
/// when a plan begins executing. Accepted values: `"tasks"` (default — open
/// the Tasks panel) or `"none"` (no auto-open; user must use Ctrl+X, T).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoPanelsConfig {
    #[serde(default = "AutoPanelsConfig::default_on_task_plan")]
    pub on_task_plan: String,
}

impl AutoPanelsConfig {
    fn default_on_task_plan() -> String {
        "tasks".to_string()
    }

    /// Returns `Ok(())` if the config holds only recognized values, else an
    /// error describing the offending key. Called by the TOML loader.
    pub fn validate(&self) -> Result<(), String> {
        match self.on_task_plan.as_str() {
            "tasks" | "none" => Ok(()),
            other => Err(format!(
                "[layout.auto_panels] on_task_plan = {:?} is invalid; expected \"tasks\" or \"none\"",
                other
            )),
        }
    }
}

impl Default for AutoPanelsConfig {
    fn default() -> Self {
        Self {
            on_task_plan: Self::default_on_task_plan(),
        }
    }
}

/// Mouse configuration. Story 16.8, AC6 + AC14.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MouseConfig {
    /// Lines per wheel tick. Default 3 per UX-DR-MOUSE.
    /// Clamped to [1, u16::MAX] on deserialize (0 → 1 with warn).
    #[serde(default = "MouseConfig::default_wheel_lines")]
    pub wheel_lines: u16,
    /// Enable terminal mouse capture. Default true.
    /// Set to false in config or via RUSTAIN_NO_MOUSE=1 env to opt out.
    #[serde(default = "MouseConfig::default_capture")]
    pub capture: bool,
}

impl MouseConfig {
    fn default_wheel_lines() -> u16 {
        3
    }
    fn default_capture() -> bool {
        true
    }
}

impl Default for MouseConfig {
    fn default() -> Self {
        Self {
            wheel_lines: 3,
            capture: true,
        }
    }
}

/// Tool progress configuration. Story 16.9, AC4.
///
/// `live_tail` is the kill-switch — default OFF so the bash-adapter refactor
/// can prove stable before the visible surface is turned on.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolProgressConfig {
    /// Enable streaming stdout tail for long-running tools. Default false.
    #[serde(default = "ToolProgressConfig::default_live_tail")]
    pub live_tail: bool,
    /// Consumer ring-buffer line cap (for the producer). Clamped to [1, 16].
    #[serde(default = "ToolProgressConfig::default_tail_lines")]
    pub tail_lines: u8,
    /// Minimum elapsed ms before the producer emits its first event. Default 3000.
    #[serde(default = "ToolProgressConfig::default_threshold_ms")]
    pub threshold_ms: u64,
}

impl ToolProgressConfig {
    fn default_live_tail() -> bool {
        false
    }
    fn default_tail_lines() -> u8 {
        4
    }
    fn default_threshold_ms() -> u64 {
        3000
    }

    pub fn tail_lines_clamped(&self) -> usize {
        self.tail_lines.clamp(1, 16) as usize
    }

    /// Validates the config, emitting `tracing::warn!` when clamping is applied.
    /// Returns the clamped `tail_lines` value.
    pub fn validate(&self) -> usize {
        let clamped = self.tail_lines_clamped();
        if self.tail_lines < 1 {
            tracing::warn!(
                tail_lines = self.tail_lines,
                clamped,
                "tool_progress.tail_lines below minimum (1); clamped up"
            );
        } else if self.tail_lines > 16 {
            tracing::warn!(
                tail_lines = self.tail_lines,
                clamped,
                "tool_progress.tail_lines above maximum (16); clamped down"
            );
        }
        clamped
    }
}

impl Default for ToolProgressConfig {
    fn default() -> Self {
        Self {
            live_tail: false,
            tail_lines: 4,
            threshold_ms: 3000,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LayoutConfig {
    #[serde(default)]
    pub auto_panels: AutoPanelsConfig,
}

/// Application configuration loaded from file + env.
///
/// NOTE (Story 5-1 Task 3.5): we intentionally do NOT use
/// `#[serde(deny_unknown_fields)]`. Skills (Story 5-1), agents (Story 5-4),
/// and future profile/provider config blocks will be added incrementally;
/// rejecting unknown top-level keys would force users to upgrade rustain in
/// lockstep with any shared team config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default = "AppConfig::default_model")]
    pub model: String,
    #[serde(default = "AppConfig::default_log_level")]
    pub log_level: String,
    #[serde(default = "AppConfig::default_log_max_size_mb")]
    pub log_max_size_mb: u64,
    #[serde(default = "AppConfig::default_log_retain_count")]
    pub log_retain_count: usize,
    /// Maximum number of checkpoints to retain per conversation.
    /// Older checkpoints (and their file snapshots) are pruned when a new
    /// checkpoint is created and the count exceeds this threshold.
    /// `None` means unlimited retention (not recommended for long sessions).
    /// Config key: `[storage] snapshot_retention_count`. Default: 100.
    #[serde(default = "AppConfig::default_snapshot_retention_count")]
    pub snapshot_retention_count: Option<usize>,
    #[serde(default)]
    pub skills: SkillsConfig,
    #[serde(default)]
    pub runtime: RuntimeConfig,
    #[serde(default)]
    pub layout: LayoutConfig,
    /// Start new sessions in Plan mode by default.
    #[serde(default)]
    pub default_plan_mode: bool,
    /// Provider configurations keyed by provider_id.
    /// If no `[provider]` section exists, the default Anthropic config is used.
    #[serde(default)]
    pub provider: std::collections::HashMap<String, ProviderConfig>,
    /// Mouse configuration (scroll lines, capture on/off). Story 16.8, AC6 + AC14.
    #[serde(default)]
    pub mouse: MouseConfig,
    /// Tool progress configuration (stdout tail, counter). Story 16.9, AC4.
    #[serde(default)]
    pub tool_progress: ToolProgressConfig,
    /// Tiered model router configuration. Story 7.1c, AC2.
    #[serde(default)]
    pub router: crate::domain::models::router::RouterConfig,
    /// Per-model pricing rates keyed by `model_id`. Story 7.5 AC1.
    /// Missing entries fall back to `n/a` cost display (AC6).
    #[serde(default = "AppConfig::default_pricing_catalog")]
    pub pricing: std::collections::HashMap<String, PricingConfig>,
    /// Daily budget configuration. Story 7.5 AC5.
    #[serde(default)]
    pub budget: BudgetConfig,
}

impl AppConfig {
    fn default_model() -> String {
        "claude-sonnet-4-6".to_string()
    }
    fn default_log_level() -> String {
        "info".to_string()
    }
    fn default_log_max_size_mb() -> u64 {
        10
    }
    fn default_log_retain_count() -> usize {
        3
    }
    fn default_snapshot_retention_count() -> Option<usize> {
        Some(100)
    }

    /// Curated default pricing catalog (Story 7.5 AC1; Dev Notes §"Default
    /// pricing catalog"). USD per 1,000,000 tokens, verified May 2026.
    ///
    /// Local LLMs (Ollama, llama.cpp via `OpenAiCompatibleVariant::Custom`)
    /// ship NO catalog entry — `cost_for_entry` returns `None` for them and
    /// the panel renders `n/a` per AC6.
    pub fn default_pricing_catalog() -> std::collections::HashMap<String, PricingConfig> {
        use std::collections::HashMap;
        let mut m = HashMap::new();
        // Anthropic
        // `claude-sonnet-4-6` is the load-bearing key: returned by `default_model()`,
        // used 7x in `src/adapters/anthropic/mod.rs` as the model ID sent to the API,
        // and what the ledger records. There is exactly ONE Sonnet-4 catalog entry
        // (per Epic 7 retro AI-7.3 canonicalization, resolving DF-S76-2).
        m.insert(
            "claude-sonnet-4-6".to_string(),
            PricingConfig {
                input_per_million: 3.00,
                output_per_million: 15.00,
                cache_creation_per_million: Some(3.75),
                cache_read_per_million: Some(0.30),
                reasoning_per_million: None,
            },
        );
        m.insert(
            "claude-haiku-4-5-20251001".to_string(),
            PricingConfig {
                input_per_million: 0.80,
                output_per_million: 4.00,
                cache_creation_per_million: Some(1.00),
                cache_read_per_million: Some(0.08),
                reasoning_per_million: None,
            },
        );
        m.insert(
            "claude-opus-4-7".to_string(),
            PricingConfig {
                input_per_million: 15.00,
                output_per_million: 75.00,
                cache_creation_per_million: Some(18.75),
                cache_read_per_million: Some(1.50),
                reasoning_per_million: None,
            },
        );
        // OpenAI
        m.insert(
            "gpt-4o".to_string(),
            PricingConfig {
                input_per_million: 2.50,
                output_per_million: 10.00,
                cache_creation_per_million: None,
                cache_read_per_million: None,
                reasoning_per_million: None,
            },
        );
        m.insert(
            "gpt-4o-mini".to_string(),
            PricingConfig {
                input_per_million: 0.15,
                output_per_million: 0.60,
                cache_creation_per_million: None,
                cache_read_per_million: None,
                reasoning_per_million: None,
            },
        );
        m.insert(
            "o1".to_string(),
            PricingConfig {
                input_per_million: 15.00,
                output_per_million: 60.00,
                cache_creation_per_million: None,
                cache_read_per_million: None,
                reasoning_per_million: None,
            },
        );
        m.insert(
            "o3-mini".to_string(),
            PricingConfig {
                input_per_million: 1.10,
                output_per_million: 4.40,
                cache_creation_per_million: None,
                cache_read_per_million: None,
                reasoning_per_million: None,
            },
        );
        // Google
        m.insert(
            "gemini-2.0-flash".to_string(),
            PricingConfig {
                input_per_million: 0.10,
                output_per_million: 0.40,
                cache_creation_per_million: None,
                cache_read_per_million: None,
                reasoning_per_million: None,
            },
        );
        m.insert(
            "gemini-2.5-pro-preview-03-25".to_string(),
            PricingConfig {
                input_per_million: 1.25,
                output_per_million: 5.00,
                cache_creation_per_million: None,
                cache_read_per_million: None,
                reasoning_per_million: None,
            },
        );
        // DeepSeek
        m.insert(
            "deepseek-chat".to_string(),
            PricingConfig {
                input_per_million: 0.27,
                output_per_million: 1.10,
                cache_creation_per_million: None,
                cache_read_per_million: None,
                reasoning_per_million: None,
            },
        );
        m.insert(
            "deepseek-reasoner".to_string(),
            PricingConfig {
                input_per_million: 0.55,
                output_per_million: 2.19,
                cache_creation_per_million: None,
                cache_read_per_million: None,
                reasoning_per_million: None,
            },
        );
        m
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            model: "claude-sonnet-4-6".to_string(),
            log_level: "info".to_string(),
            log_max_size_mb: 10,
            log_retain_count: 3,
            snapshot_retention_count: Some(100),
            skills: SkillsConfig::default(),
            runtime: RuntimeConfig::default(),
            layout: LayoutConfig::default(),
            default_plan_mode: false,
            provider: std::collections::HashMap::new(),
            mouse: MouseConfig::default(),
            tool_progress: ToolProgressConfig::default(),
            router: crate::domain::models::router::RouterConfig::default(),
            pricing: Self::default_pricing_catalog(),
            budget: BudgetConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_config_tool_progress_roundtrip() {
        let toml = r#"
model = "test-model"
[tool_progress]
live_tail = true
tail_lines = 8
threshold_ms = 5000
"#;
        let config: AppConfig = toml::from_str(toml).expect("deserialize");
        assert!(config.tool_progress.live_tail);
        assert_eq!(config.tool_progress.tail_lines, 8);
        assert_eq!(config.tool_progress.threshold_ms, 5000);

        // True round-trip: serialize back to TOML and verify key values preserved
        let serialized = toml::to_string(&config).expect("serialize");
        assert!(
            serialized.contains("live_tail = true"),
            "serialized TOML must contain live_tail"
        );
        assert!(
            serialized.contains("tail_lines = 8"),
            "serialized TOML must contain tail_lines"
        );
        assert!(
            serialized.contains("threshold_ms = 5000"),
            "serialized TOML must contain threshold_ms"
        );
    }

    #[test]
    fn tool_progress_tail_lines_clamps_out_of_range() {
        // Helper to construct a config with a specific tail_lines value
        // via serde deserialize (simulates user TOML input)
        fn parse_tail_lines(val: u8) -> ToolProgressConfig {
            let toml = format!("[tool_progress]\nlive_tail = false\ntail_lines = {}\n", val);
            let config: AppConfig = toml::from_str(&toml).expect("deserialize");
            config.tool_progress
        }

        let cfg = parse_tail_lines(4);
        assert_eq!(cfg.tail_lines_clamped(), 4);

        let cfg = parse_tail_lines(0);
        assert_eq!(cfg.tail_lines_clamped(), 1); // clamped up

        let cfg = parse_tail_lines(100);
        assert_eq!(cfg.tail_lines_clamped(), 16); // clamped down

        let cfg = parse_tail_lines(1);
        assert_eq!(cfg.tail_lines_clamped(), 1);

        let cfg = parse_tail_lines(16);
        assert_eq!(cfg.tail_lines_clamped(), 16);
    }

    #[test]
    fn app_config_router_roundtrip() {
        let toml = r#"
model = "test-model"
[router]
default_tier = "flagship"
threshold_tokens = 50000
max_retries = 3

[router.tier_models]
cheap_agentic = "cheap-model"
flagship = "flagship-model"

[router.step_tiers]
codegen = "flagship"
edit = "cheap_agentic"
test = "cheap_agentic"
plan = "flagship"
review = "flagship"
"#;
        let config: AppConfig = toml::from_str(toml).expect("deserialize");
        assert_eq!(
            config.router.default_tier,
            crate::domain::models::router::ModelTier::Flagship
        );
        assert_eq!(config.router.threshold_tokens, 50000);
        assert_eq!(config.router.max_retries, 3);
        assert_eq!(
            config
                .router
                .tier_models
                .get(&crate::domain::models::router::ModelTier::CheapAgentic),
            Some(&"cheap-model".to_string())
        );
        assert_eq!(
            config
                .router
                .tier_models
                .get(&crate::domain::models::router::ModelTier::Flagship),
            Some(&"flagship-model".to_string())
        );
        assert_eq!(
            config
                .router
                .step_tiers
                .get(&crate::domain::models::router::StepKind::Codegen),
            Some(&crate::domain::models::router::ModelTier::Flagship)
        );
        assert_eq!(
            config
                .router
                .step_tiers
                .get(&crate::domain::models::router::StepKind::Edit),
            Some(&crate::domain::models::router::ModelTier::CheapAgentic)
        );

        // Serialize back and assert key strings survive
        let serialized = toml::to_string(&config).expect("serialize");
        assert!(
            serialized.contains("default_tier = \"flagship\""),
            "serialized must contain default_tier"
        );
        assert!(
            serialized.contains("threshold_tokens = 50000"),
            "serialized must contain threshold_tokens"
        );
        assert!(
            serialized.contains("max_retries = 3"),
            "serialized must contain max_retries"
        );
        assert!(
            serialized.contains("cheap_agentic = \"cheap-model\""),
            "serialized must contain cheap_agentic tier model"
        );
        assert!(
            serialized.contains("codegen = \"flagship\""),
            "serialized must contain codegen step tier"
        );
    }

    #[test]
    fn app_config_router_defaults_when_missing() {
        let toml = r#"model = "test-model""#;
        let config: AppConfig = toml::from_str(toml).expect("deserialize");
        assert_eq!(
            config.router.default_tier,
            crate::domain::models::router::ModelTier::CheapAgentic
        );
        assert_eq!(config.router.threshold_tokens, 100_000);
        assert_eq!(config.router.max_retries, 2);
        assert!(
            config.router.tier_models.is_empty(),
            "tier_models should be empty by default (no hardcoded models)"
        );
        assert_eq!(
            config
                .router
                .step_tiers
                .get(&crate::domain::models::router::StepKind::Plan),
            Some(&crate::domain::models::router::ModelTier::Flagship)
        );
    }

    #[test]
    fn app_config_pricing_roundtrip() {
        // Canonical form is snake_case (post-Epic-7 AI-7.2 figment fix).
        // camelCase remains accepted as an alias for back-compat — see the
        // separate `app_config_pricing_camelcase_alias` test below.
        let toml_input = r#"
model = "test-model"

[pricing."claude-sonnet-4-6"]
input_per_million = 3.0
output_per_million = 15.0
"#;
        let config: AppConfig = toml::from_str(toml_input).expect("deserialize pricing");
        let p = config
            .pricing
            .get("claude-sonnet-4-6")
            .expect("sonnet pricing");
        assert_eq!(p.input_per_million, 3.0);
        assert_eq!(p.output_per_million, 15.0);
        assert_eq!(p.cache_creation_per_million, None);
        assert_eq!(p.cache_read_per_million, None);
        assert_eq!(p.reasoning_per_million, None);

        let serialized = toml::to_string(&config).expect("serialize");
        assert!(
            serialized.contains("input_per_million = 3.0"),
            "serialized must use snake_case canonical: {serialized}"
        );
        assert!(
            serialized.contains("output_per_million = 15.0"),
            "serialized must use snake_case canonical: {serialized}"
        );
    }

    /// camelCase alias accepted on deserialization (back-compat for any
    /// JSON-format configs migrated from Claude Code conventions).
    #[test]
    fn app_config_pricing_camelcase_alias() {
        let toml_input = r#"
model = "test-model"

[pricing."claude-sonnet-4-6"]
inputPerMillion = 3.0
outputPerMillion = 15.0
"#;
        let config: AppConfig =
            toml::from_str(toml_input).expect("deserialize pricing via camelCase alias");
        let p = config.pricing.get("claude-sonnet-4-6").unwrap();
        assert_eq!(p.input_per_million, 3.0);
        assert_eq!(p.output_per_million, 15.0);
    }

    #[test]
    fn app_config_pricing_snake_case_alias() {
        let toml_input = r#"
model = "test-model"

[pricing."deepseek/deepseek-v4-flash"]
input_per_million = 0.1122
output_per_million = 0.244
"#;
        let config: AppConfig = toml::from_str(toml_input).expect("deserialize pricing snake_case");
        let p = config
            .pricing
            .get("deepseek/deepseek-v4-flash")
            .expect("deepseek pricing");
        assert_eq!(p.input_per_million, 0.1122);
        assert_eq!(p.output_per_million, 0.244);
    }

    #[test]
    fn app_config_budget_snake_case_alias() {
        let toml_input = r#"
model = "test-model"

[budget]
daily_limit_usd = 10.0
"#;
        let config: AppConfig = toml::from_str(toml_input).expect("deserialize budget snake_case");
        assert_eq!(config.budget.daily_limit_usd, Some(10.0));
    }

    #[test]
    fn app_config_budget_roundtrip() {
        // Canonical form is snake_case (post-Epic-7 AI-7.2 figment fix).
        let toml_input = r#"
model = "test-model"

[budget]
daily_limit_usd = 5.0
"#;
        let config: AppConfig = toml::from_str(toml_input).expect("deserialize budget");
        assert_eq!(config.budget.daily_limit_usd, Some(5.0));

        let serialized = toml::to_string(&config).expect("serialize");
        assert!(
            serialized.contains("daily_limit_usd = 5.0"),
            "serialized must use snake_case canonical: {serialized}"
        );
    }

    /// camelCase alias still accepted on deserialization (back-compat).
    #[test]
    fn app_config_budget_camelcase_alias() {
        let toml_input = r#"
model = "test-model"

[budget]
dailyLimitUsd = 7.50
"#;
        let config: AppConfig =
            toml::from_str(toml_input).expect("deserialize budget via camelCase alias");
        assert_eq!(config.budget.daily_limit_usd, Some(7.50));
    }

    #[test]
    fn app_config_pricing_and_budget_defaults_when_missing() {
        let toml_input = r#"model = "test-model""#;
        let config: AppConfig = toml::from_str(toml_input).expect("deserialize");
        // Pricing defaults to the curated catalog
        assert!(
            config.pricing.contains_key("claude-sonnet-4-6"),
            "default pricing catalog must include claude-sonnet-4-6 \
             (the load-bearing key returned by default_model())"
        );
        assert!(
            config.pricing.contains_key("gpt-4o"),
            "default pricing catalog must include gpt-4o"
        );
        // Budget defaults to no daily_limit_usd
        assert_eq!(config.budget.daily_limit_usd, None);
        assert_eq!(config.budget, BudgetConfig::default());
    }

    /// Regression test for Epic 7 retro AI-7.3 (DF-S76-2): the default pricing
    /// catalog must NOT contain duplicate semantic entries for the same model
    /// family. Specifically, `claude-sonnet-4-6` (load-bearing) and
    /// `claude-sonnet-4-20250514` (dated-format alias, removed) must not coexist.
    #[test]
    fn app_config_default_pricing_catalog_has_no_sonnet4_duplicate() {
        let catalog = AppConfig::default_pricing_catalog();
        assert!(
            catalog.contains_key("claude-sonnet-4-6"),
            "canonical Sonnet-4 key claude-sonnet-4-6 missing from catalog"
        );
        assert!(
            !catalog.contains_key("claude-sonnet-4-20250514"),
            "dated-format Sonnet-4 alias claude-sonnet-4-20250514 must not be \
             present alongside the canonical key (DF-S76-2 / Epic 7 retro AI-7.3)"
        );
    }

    /// Regression test for AI-7.3 + Epic 8 future-proofing: the model returned by
    /// `default_model()` must have a matching entry in `default_pricing_catalog()`
    /// so an out-of-the-box config shows cost as a number, not `n/a`.
    #[test]
    fn app_config_default_model_has_pricing_entry() {
        let default_model = AppConfig::default_model();
        let catalog = AppConfig::default_pricing_catalog();
        assert!(
            catalog.contains_key(&default_model),
            "default_model() returns '{default_model}' but default_pricing_catalog() \
             has no entry for it — out-of-the-box config would show cost: n/a"
        );
    }

    #[test]
    fn app_config_provider_local_fields_roundtrip() {
        let toml = r#"
model = "test-model"

[provider.ollama]
provider_id = "ollama"
model_id = "llama3.3:70b"
api_key_env = ""
enabled = true
base_url = "http://192.168.1.50:11434"

[provider.local]
provider_id = "local"
model_id = "qwen2.5-coder"
api_key_env = ""
enabled = true
kind = "openai-compatible"
base_url = "http://localhost:8080/v1"
context_window = 32768
supports_tools = true
"#;
        let config: AppConfig = toml::from_str(toml).expect("deserialize");

        let ollama = config.provider.get("ollama").expect("ollama provider");
        assert_eq!(
            ollama.base_url.as_deref(),
            Some("http://192.168.1.50:11434")
        );
        assert_eq!(ollama.kind, None);
        assert_eq!(ollama.context_window, None);
        assert_eq!(ollama.supports_tools, None);

        let local = config.provider.get("local").expect("local provider");
        assert_eq!(local.kind.as_deref(), Some("openai-compatible"));
        assert_eq!(local.base_url.as_deref(), Some("http://localhost:8080/v1"));
        assert_eq!(local.context_window, Some(32_768));
        assert_eq!(local.supports_tools, Some(true));

        // Serialize back and assert key strings survive
        let serialized = toml::to_string(&config).expect("serialize");
        assert!(
            serialized.contains("base_url = \"http://192.168.1.50:11434\""),
            "serialized must contain ollama base_url"
        );
        assert!(
            serialized.contains("kind = \"openai-compatible\""),
            "serialized must contain local kind"
        );
        assert!(
            serialized.contains("context_window = 32768"),
            "serialized must contain local context_window"
        );
        assert!(
            serialized.contains("supports_tools = true"),
            "serialized must contain local supports_tools"
        );
    }

    #[test]
    fn provider_discover_models_roundtrip() {
        let toml = r#"
model = "test-model"

[provider.or]
provider_id = "openrouter"
model_id = "anthropic/claude-3.5-sonnet"
api_key_env = "OPENROUTER_API_KEY"
discover_models = true
model_filter = ["anthropic/*"]
cache_ttl_seconds = 1800
"#;
        let config: AppConfig = toml::from_str(toml).expect("deserialize");
        let or = config.provider.get("or").expect("openrouter provider");
        assert!(or.discover_models);
        assert_eq!(or.model_filter, vec!["anthropic/*"]);
        assert_eq!(or.cache_ttl_seconds, 1800);

        let serialized = toml::to_string(&config).expect("serialize");
        assert!(
            serialized.contains("discover_models = true"),
            "serialized must contain discover_models"
        );
        assert!(
            serialized.contains("cache_ttl_seconds = 1800"),
            "serialized must contain cache_ttl_seconds"
        );
        assert!(
            serialized.contains(r#"model_filter = ["anthropic/*"]"#),
            "serialized must contain model_filter: {serialized}"
        );
    }

    #[test]
    fn provider_discover_models_defaults() {
        let toml = r#"
model = "test-model"

[provider.an]
provider_id = "anthropic"
model_id = "claude-sonnet-4-6"
api_key_env = "ANTHROPIC_API_KEY"
"#;
        let config: AppConfig = toml::from_str(toml).expect("deserialize");
        let an = config.provider.get("an").expect("anthropic provider");
        assert!(!an.discover_models);
        assert_eq!(an.model_filter, vec!["*"]);
        assert_eq!(an.cache_ttl_seconds, 3600);
    }
}
