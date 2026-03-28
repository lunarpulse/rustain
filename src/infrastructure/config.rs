use crate::domain::models::AppConfig;

/// Load application configuration.
/// For now returns defaults; TOML file loading added in later stories.
pub fn load() -> AppConfig {
    AppConfig::default()
}
