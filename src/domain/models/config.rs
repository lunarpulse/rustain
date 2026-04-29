use serde::{Deserialize, Serialize};

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
        }
    }
}
