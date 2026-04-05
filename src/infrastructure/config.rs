use crate::domain::models::AppConfig;

/// Load application configuration.
/// For now returns defaults; TOML file loading added in later stories.
///
/// INVARIANT: Missing config file must return defaults, never error on absence.
/// The init wizard (`rustain init`) depends on this — it runs after config load
/// in the startup sequence and must work for users with no config file yet.
pub fn load() -> AppConfig {
    AppConfig::default()
}
