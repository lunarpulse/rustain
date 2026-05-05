use serde::{Deserialize, Serialize};

/// Configuration for a single LLM provider.
///
/// `api_key_env` names an environment variable — the adapter reads
/// the actual key at startup; the domain config never stores secrets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Unique provider identifier (e.g., `"anthropic"`).
    pub provider_id: String,
    /// The model to use (e.g., `"claude-sonnet-4-20250514"`).
    pub model_id: String,
    /// Environment variable name that holds the API key or bearer token.
    pub api_key_env: String,
    /// Whether this provider is enabled.
    #[serde(default = "ProviderConfig::default_enabled")]
    pub enabled: bool,
}

impl ProviderConfig {
    fn default_enabled() -> bool {
        true
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
        assert!(serialized.contains("live_tail = true"), "serialized TOML must contain live_tail");
        assert!(serialized.contains("tail_lines = 8"), "serialized TOML must contain tail_lines");
        assert!(serialized.contains("threshold_ms = 5000"), "serialized TOML must contain threshold_ms");
    }

    #[test]
    fn tool_progress_tail_lines_clamps_out_of_range() {
        // Helper to construct a config with a specific tail_lines value
        // via serde deserialize (simulates user TOML input)
        fn parse_tail_lines(val: u8) -> ToolProgressConfig {
            let toml = format!(
                "[tool_progress]\nlive_tail = false\ntail_lines = {}\n",
                val
            );
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
}
