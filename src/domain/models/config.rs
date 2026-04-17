use serde::{Deserialize, Serialize};

/// Application configuration loaded from file + env.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub model: String,
    pub log_level: String,
    pub log_max_size_mb: u64,
    pub log_retain_count: usize,
    /// Maximum number of checkpoints to retain per conversation.
    /// Older checkpoints (and their file snapshots) are pruned when a new
    /// checkpoint is created and the count exceeds this threshold.
    /// `None` means unlimited retention (not recommended for long sessions).
    /// Config key: `[storage] snapshot_retention_count`. Default: 100.
    #[serde(default = "AppConfig::default_snapshot_retention_count")]
    pub snapshot_retention_count: Option<usize>,
}

impl AppConfig {
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
        }
    }
}
