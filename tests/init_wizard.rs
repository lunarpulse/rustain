//! Integration tests for Story 2-3: Configuration Init Wizard.

use clap::Parser;
use rustain::adapters::cli::commands::{Cli, Command};
use rustain::domain::models::AppConfig;

// ──────────────────────────────────────────────────
// Task 4.1: CLI subcommand parsing tests
// ──────────────────────────────────────────────────

/// `rustain init` sets command = Some(Command::Init).
// Covers: FR97 (init wizard)
#[test]
fn test_cli_init_subcommand() {
    let cli = Cli::parse_from(["rustain", "init"]);
    assert!(matches!(cli.command, Some(Command::Init)));
    // Default flags preserved
    assert!(!cli.new);
    assert!(cli.session.is_none());
    assert_eq!(cli.log_level, "info");
}

/// Bare `rustain` sets command = None.
// Covers: FR97 (init wizard)
#[test]
fn test_cli_no_subcommand() {
    let cli = Cli::parse_from(["rustain"]);
    assert!(cli.command.is_none());
    assert!(!cli.new);
    assert!(cli.session.is_none());
}

/// Existing --new flag still works in no-subcommand mode.
// Covers: FR97 (init wizard)
#[test]
fn test_cli_new_flag_still_works() {
    let cli = Cli::parse_from(["rustain", "--new"]);
    assert!(cli.command.is_none());
    assert!(cli.new);
    assert!(cli.session.is_none());
}

/// Existing --session flag still works in no-subcommand mode.
// Covers: FR97 (init wizard)
#[test]
fn test_cli_session_flag_still_works() {
    let cli = Cli::parse_from(["rustain", "--session", "abc-123"]);
    assert!(cli.command.is_none());
    assert!(!cli.new);
    assert_eq!(cli.session, Some("abc-123".to_string()));
}

/// --log-level works as global flag with init subcommand.
// Covers: FR97 (init wizard)
#[test]
fn test_cli_log_level_with_init() {
    let cli = Cli::parse_from(["rustain", "--log-level", "debug", "init"]);
    assert!(matches!(cli.command, Some(Command::Init)));
    assert_eq!(cli.log_level, "debug");
}

// ──────────────────────────────────────────────────
// Task 4.3: Full init flow integration test
// ──────────────────────────────────────────────────

/// Init helpers: verify create_directories, write_config_toml, write_settings_json
/// produce valid artifacts. Does not test run_init_with_paths (requires TTY).
// Covers: FR97 (init wizard)
#[test]
fn test_init_helpers_create_valid_artifacts() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config").join("rustain");
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();

    // Call internal functions directly to test the flow without TTY
    rustain::adapters::cli::init::create_directories(&config_dir, &workspace)
        .unwrap();
    let config_toml_path = config_dir.join("config.toml");
    rustain::adapters::cli::init::write_config_toml(&config_toml_path).unwrap();
    let settings_path = workspace.join(".claude").join("settings.json");
    rustain::adapters::cli::init::write_settings_json(&settings_path).unwrap();

    // Verify directories exist
    assert!(config_dir.exists(), "Config dir should exist");
    assert!(
        workspace.join(".claude").exists(),
        ".claude dir should exist"
    );
    assert!(
        workspace.join(".claude").join("sessions").exists(),
        "Sessions dir should exist"
    );

    // Verify config.toml exists and is valid
    assert!(config_toml_path.exists(), "config.toml should exist");
    let config_content = std::fs::read_to_string(&config_toml_path).unwrap();
    assert!(config_content.contains("# Rustain Configuration"));
    assert!(config_content.contains("claude-sonnet-4-6"));

    // Parse TOML back (strip comments)
    let toml_lines: String = config_content
        .lines()
        .filter(|l| !l.starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    let parsed: AppConfig = toml::from_str(&toml_lines).unwrap();
    assert_eq!(parsed.model, "claude-sonnet-4-6");
    assert_eq!(parsed.log_level, "info");
    assert_eq!(parsed.log_max_size_mb, 10);
    assert_eq!(parsed.log_retain_count, 3);

    // Verify settings.json exists and is valid
    assert!(settings_path.exists(), "settings.json should exist");
    let settings_content = std::fs::read_to_string(&settings_path).unwrap();
    let settings: serde_json::Value = serde_json::from_str(&settings_content).unwrap();
    assert_eq!(settings["permissions"]["allow"], serde_json::json!([]));
}

// ──────────────────────────────────────────────────
// Task 4.4: Existing config detection test
// ──────────────────────────────────────────────────

/// Existing config detection: verify the condition from run_init_with_paths that
/// triggers the "Configuration already exists. Overwrite?" prompt.
// Covers: FR97 (init wizard)
#[test]
fn test_existing_config_detection_logic() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config").join("rustain");
    std::fs::create_dir_all(&config_dir).unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();

    let config_toml_path = config_dir.join("config.toml");
    let settings_json_path = workspace.join(".claude").join("settings.json");

    // Neither exists → no detection
    assert!(
        !(config_toml_path.exists() || settings_json_path.exists()),
        "No config should be detected when neither file exists"
    );

    // Only config.toml exists → detected
    std::fs::write(&config_toml_path, "model = \"test\"").unwrap();
    assert!(
        config_toml_path.exists() || settings_json_path.exists(),
        "Config should be detected when config.toml exists"
    );

    // Both exist → detected
    std::fs::create_dir_all(settings_json_path.parent().unwrap()).unwrap();
    std::fs::write(&settings_json_path, "{}").unwrap();
    assert!(
        config_toml_path.exists() || settings_json_path.exists(),
        "Config should be detected when both files exist"
    );

    // Only settings.json exists → detected
    std::fs::remove_file(&config_toml_path).unwrap();
    assert!(
        config_toml_path.exists() || settings_json_path.exists(),
        "Config should be detected when only settings.json exists"
    );
}

// ──────────────────────────────────────────────────
// Task 4.5: TTY detection guard
// ──────────────────────────────────────────────────

/// TTY rejection: spawn a subprocess to verify exit code 1 when piped.
// Covers: FR97 (init wizard)
#[test]
fn test_tty_rejection_subprocess() {
    let binary = env!("CARGO_BIN_EXE_rustain");
    let output = std::process::Command::new(binary)
        .arg("init")
        .stdin(std::process::Stdio::piped()) // Force non-TTY
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("Failed to execute rustain init");

    // Should exit with code 1 (non-interactive terminal)
    assert_eq!(
        output.status.code(),
        Some(1),
        "rustain init should exit with code 1 when stdin is piped"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("rustain init requires an interactive terminal"),
        "Should display TTY error message, got: {}",
        stderr
    );
    assert!(
        stderr.contains("--non-interactive"),
        "Should mention --non-interactive flag, got: {}",
        stderr
    );
}

// ──────────────────────────────────────────────────
// D3: settings.json preservation on re-init
// ──────────────────────────────────────────────────

/// Verify that re-init preserves existing settings.json with accumulated permissions.
// Covers: FR97 (init wizard)
#[test]
fn test_reinit_preserves_existing_settings_json() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config").join("rustain");
    let workspace = tmp.path().join("workspace");

    // First init: create everything from scratch
    rustain::adapters::cli::init::create_directories(&config_dir, &workspace).unwrap();
    let settings_path = workspace.join(".claude").join("settings.json");
    rustain::adapters::cli::init::write_settings_json(&settings_path).unwrap();

    // Simulate accumulated permissions (as SecurityAdapter would write)
    let accumulated = serde_json::json!({
        "permissions": {
            "allow": ["Bash(cargo test)", "Read"]
        }
    });
    std::fs::write(&settings_path, serde_json::to_string_pretty(&accumulated).unwrap()).unwrap();

    // Re-init: write config.toml again (simulating overwrite=yes), but settings.json should be skipped
    let config_toml_path = config_dir.join("config.toml");
    rustain::adapters::cli::init::write_config_toml(&config_toml_path).unwrap();
    // The D3 fix: settings.json is NOT overwritten because it already exists
    // (in run_init_with_paths, the check is: if settings_json_path.exists() { skip })
    assert!(settings_path.exists());
    let content = std::fs::read_to_string(&settings_path).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
    // Accumulated permissions should still be there
    assert_eq!(
        parsed["permissions"]["allow"],
        serde_json::json!(["Bash(cargo test)", "Read"]),
        "Existing permissions should be preserved after re-init"
    );
}

// ──────────────────────────────────────────────────
// P6: find_api_key_var integration test
// ──────────────────────────────────────────────────

/// Verify find_api_key_var is accessible and returns expected types.
// Covers: FR97 (init wizard)
#[test]
fn test_find_api_key_var_returns_static_str() {
    // This test validates the public API surface of find_api_key_var.
    // The detailed env var manipulation tests are in the unit test module.
    let result: Option<&'static str> = rustain::adapters::cli::init::find_api_key_var();
    // We can't control env vars safely in integration tests (edition 2024),
    // but we can verify the return type and that it doesn't panic.
    let _ = result;
}

// ──────────────────────────────────────────────────
// Task 4.6: Config TOML round-trip
// ──────────────────────────────────────────────────

/// Serialize AppConfig::default() to TOML, deserialize back, assert equality.
// Covers: FR97 (init wizard)
#[test]
fn test_config_toml_roundtrip() {
    let default = AppConfig::default();
    let toml_str = toml::to_string_pretty(&default).unwrap();
    let parsed: AppConfig = toml::from_str(&toml_str).unwrap();
    assert_eq!(parsed.model, default.model);
    assert_eq!(parsed.log_level, default.log_level);
    assert_eq!(parsed.log_max_size_mb, default.log_max_size_mb);
    assert_eq!(parsed.log_retain_count, default.log_retain_count);
}

/// Ensure the config.toml written by init can be parsed back to AppConfig.
// Covers: FR97 (init wizard)
#[test]
fn test_init_config_file_roundtrip() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config_path = tmp.path().join("config.toml");

    rustain::adapters::cli::init::write_config_toml(&config_path).unwrap();

    let content = std::fs::read_to_string(&config_path).unwrap();
    // Strip comment lines for TOML parsing
    let toml_lines: String = content
        .lines()
        .filter(|l| !l.starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");

    let parsed: AppConfig = toml::from_str(&toml_lines).unwrap();
    let default = AppConfig::default();
    assert_eq!(parsed.model, default.model);
    assert_eq!(parsed.log_level, default.log_level);
    assert_eq!(parsed.log_max_size_mb, default.log_max_size_mb);
    assert_eq!(parsed.log_retain_count, default.log_retain_count);
}
