//! Active-profile resolution helper — single-writer rule for deriving the
//! effective profile name from CLI flags, environment variables, and config.
//!
//! Extracted from `startup.rs:99-104` per Decision Gate 1.4 (Story 8.6a).
//!
//! Resolution order: `--profile` CLI flag → `RUSTAIN_PROFILE` env var →
//! `AppConfig.active_profile` field.

use crate::adapters::cli::commands::Cli;
use crate::domain::models::AppConfig;
use crate::infrastructure::utils::env_var_trimmed;

/// Derive the effective profile name from CLI flags, env vars, and config.
///
/// Resolution precedence:
/// 1. `cli.profile` (if non-empty)
/// 2. `RUSTAIN_PROFILE` environment variable
/// 3. `bootstrap_config.active_profile` (defaults to "coding")
pub fn effective_profile_name(cli: &Cli, bootstrap_config: &AppConfig) -> String {
    cli.profile
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| env_var_trimmed("RUSTAIN_PROFILE"))
        .unwrap_or_else(|| bootstrap_config.active_profile.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_cli(profile: Option<&str>) -> Cli {
        Cli {
            log_level: None,
            command: None,
            new: false,
            session: None,
            snapshot_retention: None,
            config_file: None,
            model: None,
            profile: profile.map(|s| s.to_string()),
            persona: None,
            memory: None,
            session_adapter: None,
            tools: None,
            channels: None,
            scheduler: None,
            context: None,
            tool_exposure: None,
            skill_exposure: None,
        }
    }

    fn make_config(active_profile: &str) -> AppConfig {
        let mut config = AppConfig::default();
        config.active_profile = active_profile.to_string();
        config
    }

    #[test]
    fn test_cli_profile_flag_wins() {
        let cli = make_cli(Some("custom-profile"));
        let config = make_config("coding");
        assert_eq!(effective_profile_name(&cli, &config), "custom-profile");
    }

    #[test]
    fn test_env_var_used_when_cli_absent() {
        let cli = make_cli(None);
        let config = make_config("coding");
        // Save original
        let orig = std::env::var("RUSTAIN_PROFILE").ok();
        // CONFORMANCE_EXCEPTION: test env manipulation
        unsafe {
            std::env::set_var("RUSTAIN_PROFILE", "env-profile");
        }
        let result = effective_profile_name(&cli, &config);
        match orig {
            Some(v) => unsafe { std::env::set_var("RUSTAIN_PROFILE", v) },
            None => unsafe { std::env::remove_var("RUSTAIN_PROFILE") },
        }
        assert_eq!(result, "env-profile");
    }

    #[test]
    fn test_config_active_profile_used_when_cli_and_env_absent() {
        let cli = make_cli(None);
        let config = make_config("coding");
        // Remove env var if set
        let orig = std::env::var("RUSTAIN_PROFILE").ok(); // CONFORMANCE_EXCEPTION: test env save/restore
        unsafe { std::env::remove_var("RUSTAIN_PROFILE") };
        let result = effective_profile_name(&cli, &config);
        match orig {
            Some(v) => unsafe { std::env::set_var("RUSTAIN_PROFILE", v) },
            None => {}
        }
        assert_eq!(result, "coding");
    }

    #[test]
    fn test_empty_cli_string_falls_through_to_env() {
        let cli = make_cli(Some(""));
        let config = make_config("coding");
        // Remove env var if set
        let orig = std::env::var("RUSTAIN_PROFILE").ok(); // CONFORMANCE_EXCEPTION: test env save/restore
        unsafe { std::env::remove_var("RUSTAIN_PROFILE") };
        let result = effective_profile_name(&cli, &config);
        match orig {
            Some(v) => unsafe { std::env::set_var("RUSTAIN_PROFILE", v) },
            None => {}
        }
        // Empty CLI string → falls through → config
        assert_eq!(result, "coding");
    }
}
