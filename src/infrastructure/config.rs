use std::path::Path;

use crate::domain::models::AppConfig;

fn try_load_from(path: &Path) -> Option<AppConfig> {
    if !path.exists() {
        return None;
    }
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("Config file unreadable at {}: {}", path.display(), e);
            return None;
        }
    };
    match toml::from_str::<AppConfig>(&content) {
        Ok(config) => Some(config),
        Err(e) => {
            tracing::warn!("Config file malformed at {}: {}", path.display(), e);
            None
        }
    }
}

/// Load application configuration.
/// Searches workspace `.rustain/config.toml`, then `~/.config/rustain/config.toml`.
/// Falls back to defaults if no config file exists.
///
/// INVARIANT: Missing config file must return defaults, never error on absence.
pub fn load() -> AppConfig {
    let workspace = std::env::current_dir().ok();
    if let Some(ref ws) = workspace {
        let ws_config = ws.join(".rustain").join("config.toml");
        if let Some(config) = try_load_from(&ws_config) {
            tracing::info!("Loaded config from {}", ws_config.display());
            return config;
        }
    }

    if let Some(home) = dirs::home_dir() {
        let home_config = home.join(".config").join("rustain").join("config.toml");
        if let Some(config) = try_load_from(&home_config) {
            tracing::info!("Loaded config from {}", home_config.display());
            return config;
        }
    }

    AppConfig::default()
}
