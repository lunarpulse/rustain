use serde::{Deserialize, Serialize};

/// Application configuration loaded from file + env.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub model: String,
    pub log_level: String,
    pub log_max_size_mb: u64,
    pub log_retain_count: usize,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            model: "claude-sonnet-4-6".to_string(),
            log_level: "info".to_string(),
            log_max_size_mb: 10,
            log_retain_count: 3,
        }
    }
}
