//! Integration tests for Story 2-4: Setup Health Check.

use clap::Parser;
use rustain::adapters::cli::commands::{Cli, Command};
use rustain::adapters::cli::doctor::{
    ApiKeyCheck, CheckResult, CheckStatus, CheckTier, GlobalConfigCheck, HealthCheck,
    SessionStorageCheck, WorkspaceConfigCheck, WorkspaceDirCheck, display_results,
};
use rustain::domain::models::AppConfig;

// ──────────────────────────────────────────────────
// Task 8.1: CLI subcommand parsing
// ──────────────────────────────────────────────────

/// `rustain doctor` sets command = Some(Command::Doctor { terminal: false }).
// Covers: FR98 (doctor health)
#[test]
fn test_cli_doctor_subcommand() {
    let cli = Cli::parse_from(["rustain", "doctor"]);
    assert!(matches!(
        cli.command,
        Some(Command::Doctor {
            terminal: false,
            adapters: false,
            json: false
        })
    ));
    assert!(!cli.new);
    assert!(cli.session.is_none());
    assert!(
        cli.log_level.is_none(),
        "no --log-level => None (Story 8.1 D5)"
    );
}

/// `rustain doctor --terminal` sets terminal = true.
// Covers: FR98 (doctor health)
#[test]
fn test_cli_doctor_terminal_flag() {
    let cli = Cli::parse_from(["rustain", "doctor", "--terminal"]);
    assert!(matches!(
        cli.command,
        Some(Command::Doctor {
            terminal: true,
            adapters: false,
            json: false
        })
    ));
}

/// Existing subcommands still work after adding Doctor.
// Covers: FR98 (doctor health), FR97 (init wizard)
#[test]
fn test_cli_init_still_works() {
    let cli = Cli::parse_from(["rustain", "init"]);
    assert!(matches!(cli.command, Some(Command::Init)));
}

/// Bare `rustain` still sets command = None.
// Covers: FR98 (doctor health)
#[test]
fn test_cli_no_subcommand_still_works() {
    let cli = Cli::parse_from(["rustain"]);
    assert!(cli.command.is_none());
}

/// Existing --new flag still works.
// Covers: FR98 (doctor health)
#[test]
fn test_cli_new_flag_unchanged() {
    let cli = Cli::parse_from(["rustain", "--new"]);
    assert!(cli.command.is_none());
    assert!(cli.new);
}

/// Existing --session flag still works.
// Covers: FR98 (doctor health)
#[test]
fn test_cli_session_flag_unchanged() {
    let cli = Cli::parse_from(["rustain", "--session", "abc-123"]);
    assert!(cli.command.is_none());
    assert_eq!(cli.session, Some("abc-123".to_string()));
}

/// --log-level works as global flag with doctor subcommand.
// Covers: FR98 (doctor health)
#[test]
fn test_cli_log_level_with_doctor() {
    let cli = Cli::parse_from(["rustain", "--log-level", "debug", "doctor"]);
    assert!(matches!(
        cli.command,
        Some(Command::Doctor {
            terminal: false,
            adapters: false,
            json: false
        })
    ));
    assert_eq!(cli.log_level.as_deref(), Some("debug"));
}

// ──────────────────────────────────────────────────
// Task 8.2: CheckResult formatting
// ──────────────────────────────────────────────────

// Covers: FR98 (doctor health)
#[test]
fn test_display_results_formats_all_statuses() {
    let results = vec![
        CheckResult {
            name: "Pass check".to_string(),
            category: "test".to_string(),
            status: CheckStatus::Pass,
            message: "all good".to_string(),
            fix: None,
            latency: None,
            tier: CheckTier::ExitAffecting,
        },
        CheckResult {
            name: "Warn check".to_string(),
            category: "test".to_string(),
            status: CheckStatus::Warning,
            message: "something off".to_string(),
            fix: Some("try this".to_string()),
            latency: None,
            tier: CheckTier::ExitAffecting,
        },
        CheckResult {
            name: "Fail check".to_string(),
            category: "test".to_string(),
            status: CheckStatus::Fail,
            message: "broken".to_string(),
            fix: Some("fix it".to_string()),
            latency: None,
            tier: CheckTier::ExitAffecting,
        },
    ];
    // Should not panic; output goes to stdout
    display_results(&results);
}

// ──────────────────────────────────────────────────
// Task 8.3: Summary counting
// ──────────────────────────────────────────────────

// Covers: FR98 (doctor health)
#[test]
fn test_summary_counts_various_combos() {
    let results = [
        CheckResult {
            name: "A".to_string(),
            category: "test".to_string(),
            status: CheckStatus::Pass,
            message: "".to_string(),
            fix: None,
            latency: None,
            tier: CheckTier::ExitAffecting,
        },
        CheckResult {
            name: "B".to_string(),
            category: "test".to_string(),
            status: CheckStatus::Pass,
            message: "".to_string(),
            fix: None,
            latency: None,
            tier: CheckTier::ExitAffecting,
        },
        CheckResult {
            name: "C".to_string(),
            category: "test".to_string(),
            status: CheckStatus::Pass,
            message: "".to_string(),
            fix: None,
            latency: None,
            tier: CheckTier::ExitAffecting,
        },
        CheckResult {
            name: "D".to_string(),
            category: "test".to_string(),
            status: CheckStatus::Warning,
            message: "".to_string(),
            fix: None,
            latency: None,
            tier: CheckTier::ExitAffecting,
        },
        CheckResult {
            name: "E".to_string(),
            category: "test".to_string(),
            status: CheckStatus::Fail,
            message: "".to_string(),
            fix: None,
            latency: None,
            tier: CheckTier::ExitAffecting,
        },
        CheckResult {
            name: "F".to_string(),
            category: "test".to_string(),
            status: CheckStatus::Fail,
            message: "".to_string(),
            fix: None,
            latency: None,
            tier: CheckTier::ExitAffecting,
        },
    ];
    let pass = results
        .iter()
        .filter(|r| r.status == CheckStatus::Pass)
        .count();
    let warn = results
        .iter()
        .filter(|r| r.status == CheckStatus::Warning)
        .count();
    let fail = results
        .iter()
        .filter(|r| r.status == CheckStatus::Fail)
        .count();
    assert_eq!(pass, 3);
    assert_eq!(warn, 1);
    assert_eq!(fail, 2);
}

// ──────────────────────────────────────────────────
// Task 8.4: GlobalConfigCheck
// ──────────────────────────────────────────────────

// Covers: FR98 (doctor health), FR97 (init wizard)
#[tokio::test]
async fn test_global_config_valid_toml() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().to_path_buf();
    let config = AppConfig::default();
    let toml_content = toml::to_string_pretty(&config).unwrap();
    std::fs::write(config_dir.join("config.toml"), &toml_content).unwrap();

    let check = GlobalConfigCheck {
        config_dir: Some(config_dir),
    };
    let result = check.run().await;
    assert_eq!(result.status, CheckStatus::Pass);
    assert!(result.message.contains("config.toml"));
}

// Covers: FR98 (doctor health), FR97 (init wizard)
#[tokio::test]
async fn test_global_config_invalid_toml_file() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().to_path_buf();
    std::fs::write(config_dir.join("config.toml"), "{{{{invalid toml").unwrap();

    let check = GlobalConfigCheck {
        config_dir: Some(config_dir),
    };
    let result = check.run().await;
    assert_eq!(result.status, CheckStatus::Fail);
    assert!(result.fix.unwrap().contains("rustain init"));
}

// Covers: FR98 (doctor health), FR97 (init wizard)
#[tokio::test]
async fn test_global_config_missing_file() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("nonexistent_dir");

    let check = GlobalConfigCheck {
        config_dir: Some(config_dir),
    };
    let result = check.run().await;
    assert_eq!(result.status, CheckStatus::Fail);
    assert!(result.fix.unwrap().contains("rustain init"));
}

// ──────────────────────────────────────────────────
// Task 8.5: WorkspaceConfigCheck
// ──────────────────────────────────────────────────

// Covers: FR98 (doctor health)
#[tokio::test]
async fn test_workspace_config_valid_json() {
    let tmp = tempfile::TempDir::new().unwrap();
    let workspace = tmp.path().to_path_buf();
    let claude_dir = workspace.join(".claude");
    std::fs::create_dir_all(&claude_dir).unwrap();
    std::fs::write(
        claude_dir.join("settings.json"),
        r#"{"permissions":{"allow":["Read"]}}"#,
    )
    .unwrap();

    let check = WorkspaceConfigCheck {
        workspace: Some(workspace),
    };
    let result = check.run().await;
    assert_eq!(result.status, CheckStatus::Pass);
}

// Covers: FR98 (doctor health)
#[tokio::test]
async fn test_workspace_config_invalid_json_file() {
    let tmp = tempfile::TempDir::new().unwrap();
    let workspace = tmp.path().to_path_buf();
    let claude_dir = workspace.join(".claude");
    std::fs::create_dir_all(&claude_dir).unwrap();
    std::fs::write(claude_dir.join("settings.json"), "not-json{{{").unwrap();

    let check = WorkspaceConfigCheck {
        workspace: Some(workspace),
    };
    let result = check.run().await;
    assert_eq!(result.status, CheckStatus::Fail);
}

// Covers: FR98 (doctor health)
#[tokio::test]
async fn test_workspace_config_missing_file() {
    let tmp = tempfile::TempDir::new().unwrap();
    let workspace = tmp.path().to_path_buf();
    // No .claude/ directory

    let check = WorkspaceConfigCheck {
        workspace: Some(workspace),
    };
    let result = check.run().await;
    assert_eq!(result.status, CheckStatus::Warning);
}

// ──────────────────────────────────────────────────
// Task 8.6: SessionStorageCheck
// ──────────────────────────────────────────────────

// Covers: FR98 (doctor health)
#[tokio::test]
async fn test_sessions_empty_directory() {
    let tmp = tempfile::TempDir::new().unwrap();
    let workspace = tmp.path().to_path_buf();
    let sessions_dir = workspace.join(".claude").join("sessions");
    std::fs::create_dir_all(&sessions_dir).unwrap();

    let check = SessionStorageCheck {
        workspace: Some(workspace),
        config_dir: Some(tmp.path().join("no_config")),
    };
    let result = check.run().await;
    assert_eq!(result.status, CheckStatus::Pass);
    assert!(result.message.contains("empty"));
}

// Covers: FR98 (doctor health)
#[tokio::test]
async fn test_sessions_with_valid_files() {
    let tmp = tempfile::TempDir::new().unwrap();
    let workspace = tmp.path().to_path_buf();
    let sessions_dir = workspace.join(".claude").join("sessions");
    std::fs::create_dir_all(&sessions_dir).unwrap();

    std::fs::write(
        sessions_dir.join("s1.meta.json"),
        r#"{"id":"s1","title":"Session 1"}"#,
    )
    .unwrap();
    std::fs::write(
        sessions_dir.join("s2.meta.json"),
        r#"{"id":"s2","title":"Session 2"}"#,
    )
    .unwrap();
    std::fs::write(
        sessions_dir.join("s3.meta.json"),
        r#"{"id":"s3","title":"Session 3"}"#,
    )
    .unwrap();

    let check = SessionStorageCheck {
        workspace: Some(workspace),
        config_dir: Some(tmp.path().join("no_config")),
    };
    let result = check.run().await;
    assert_eq!(result.status, CheckStatus::Pass);
    assert!(result.message.contains("3 saved"));
}

// Covers: FR98 (doctor health)
#[tokio::test]
async fn test_sessions_with_corrupted_file() {
    let tmp = tempfile::TempDir::new().unwrap();
    let workspace = tmp.path().to_path_buf();
    let sessions_dir = workspace.join(".claude").join("sessions");
    std::fs::create_dir_all(&sessions_dir).unwrap();

    std::fs::write(
        sessions_dir.join("ok.meta.json"),
        r#"{"id":"ok","title":"Good"}"#,
    )
    .unwrap();
    std::fs::write(sessions_dir.join("corrupt.meta.json"), "{{invalid").unwrap();

    let check = SessionStorageCheck {
        workspace: Some(workspace),
        config_dir: Some(tmp.path().join("no_config")),
    };
    let result = check.run().await;
    assert_eq!(result.status, CheckStatus::Warning);
    assert!(result.message.contains("corrupted"));
}

// Covers: FR98 (doctor health)
#[tokio::test]
async fn test_sessions_missing_dir_when_init_run() {
    let tmp = tempfile::TempDir::new().unwrap();
    let workspace = tmp.path().join("some_workspace");
    let config_dir = tmp.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(config_dir.join("config.toml"), "model = \"test\"").unwrap();

    let check = SessionStorageCheck {
        workspace: Some(workspace),
        config_dir: Some(config_dir),
    };
    let result = check.run().await;
    assert_eq!(result.status, CheckStatus::Fail);
    assert!(result.fix.unwrap().contains("rustain init"));
}

// Covers: FR98 (doctor health)
#[tokio::test]
async fn test_sessions_missing_dir_not_initialized() {
    let tmp = tempfile::TempDir::new().unwrap();
    let workspace = tmp.path().join("fresh_workspace");
    // No config.toml → not initialized

    let check = SessionStorageCheck {
        workspace: Some(workspace),
        config_dir: Some(tmp.path().join("empty_config")),
    };
    let result = check.run().await;
    assert_eq!(result.status, CheckStatus::Pass);
    assert!(result.message.contains("not initialized"));
}

// ──────────────────────────────────────────────────
// Task 8.7: TerminalCheck (env-based)
// ──────────────────────────────────────────────────

// Covers: FR98 (doctor health)
#[tokio::test]
async fn test_terminal_check_runs() {
    use rustain::infrastructure::terminal_info::detect_color_capability;

    // Just verify it doesn't panic and returns a Pass
    let color = detect_color_capability();
    // Color detection should return a valid variant
    let display = format!("{}", color);
    assert!(
        ["truecolor", "256color", "16color", "mono"].contains(&display.as_str()),
        "Unexpected color capability: {}",
        display
    );
}

// ──────────────────────────────────────────────────
// Task 8.8: ApiKeyCheck with mockito
// ──────────────────────────────────────────────────

// Covers: FR98 (doctor health), NFR11 (no API keys logged)
// Story 13.2 AC8b: DELIBERATE UPDATE — ApiKeyCheck is now key-presence only (no network).
// Network-based auth validation moved to ProviderConnectivityCheck (AC8).
#[tokio::test]
async fn test_api_key_not_set() {
    let check = ApiKeyCheck {
        key_var_override: Some(None), // simulate no key found
    };
    let result = check.run().await;
    assert_eq!(result.status, CheckStatus::Fail);
    assert!(result.message.contains("not set"));
    assert!(result.fix.is_some());
}

// Story 13.2 AC8b: ApiKeyCheck now returns Pass (key-presence) — no mock server needed.
#[tokio::test]
async fn test_api_key_valid_with_mock_400() {
    // De-billed: ApiKeyCheck no longer makes network calls.
    // Auth validation is now ProviderConnectivityCheck's job (AC8).
    let check = ApiKeyCheck {
        key_var_override: Some(Some("ANTHROPIC_API_KEY")),
    };
    let result = check.run().await;
    assert_eq!(result.status, CheckStatus::Pass);
    assert!(result.message.contains("set"));
    assert!(result.message.contains("ANTHROPIC_API_KEY"));
}

// Story 13.2 AC8b: ApiKeyCheck returns Pass for any set key — auth validation is separate.
#[tokio::test]
async fn test_api_key_invalid_401_with_mock() {
    // De-billed: key is set → Pass. Auth failure detection moved to ProviderConnectivityCheck.
    let check = ApiKeyCheck {
        key_var_override: Some(Some("ANTHROPIC_API_KEY")),
    };
    let result = check.run().await;
    assert_eq!(result.status, CheckStatus::Pass);
    assert!(result.message.contains("set"));
}

// Story 13.2 AC8b: Bearer token key presence check.
#[tokio::test]
async fn test_api_key_bearer_auth_token() {
    let check = ApiKeyCheck {
        key_var_override: Some(Some("ANTHROPIC_AUTH_TOKEN")),
    };
    let result = check.run().await;
    assert_eq!(result.status, CheckStatus::Pass);
    assert!(result.message.contains("ANTHROPIC_AUTH_TOKEN"));
}

// Story 13.2 AC8b: Custom URL key presence.
#[tokio::test]
async fn test_api_key_custom_url_fix_message() {
    // De-billed: key is set → Pass regardless of URL. Custom URL auth → ProviderConnectivityCheck.
    let check = ApiKeyCheck {
        key_var_override: Some(Some("ANTHROPIC_API_KEY")),
    };
    let result = check.run().await;
    assert_eq!(result.status, CheckStatus::Pass);
    assert!(result.message.contains("set"));
}

// ──────────────────────────────────────────────────
// Task 8.10: terminal_info relocation
// ──────────────────────────────────────────────────

// Covers: FR98 (doctor health)
#[test]
fn test_terminal_info_detect_color_returns_valid() {
    use rustain::infrastructure::terminal_info::{ColorCapability, detect_color_capability};

    let cap = detect_color_capability();
    // Should be one of the valid variants
    assert!(matches!(
        cap,
        ColorCapability::TrueColor
            | ColorCapability::Color256
            | ColorCapability::Color16
            | ColorCapability::Monochrome
    ));
}

// Covers: FR98 (doctor health)
#[test]
fn test_color_capability_display() {
    use rustain::infrastructure::terminal_info::ColorCapability;

    assert_eq!(format!("{}", ColorCapability::TrueColor), "truecolor");
    assert_eq!(format!("{}", ColorCapability::Color256), "256color");
    assert_eq!(format!("{}", ColorCapability::Color16), "16color");
    assert_eq!(format!("{}", ColorCapability::Monochrome), "mono");
}

// ──────────────────────────────────────────────────
// Task 8.11: Full doctor flow integration
// ──────────────────────────────────────────────────

// Covers: FR98 (doctor health)
#[tokio::test]
async fn test_full_doctor_flow_valid_workspace() {
    let tmp = tempfile::TempDir::new().unwrap();
    let workspace = tmp.path().to_path_buf();
    let config_dir = tmp.path().join("config");

    // Create valid config.toml
    std::fs::create_dir_all(&config_dir).unwrap();
    let config = AppConfig::default();
    let toml_content = toml::to_string_pretty(&config).unwrap();
    std::fs::write(config_dir.join("config.toml"), &toml_content).unwrap();

    // Create valid workspace
    let claude_dir = workspace.join(".claude");
    std::fs::create_dir_all(&claude_dir).unwrap();
    std::fs::write(
        claude_dir.join("settings.json"),
        r#"{"permissions":{"allow":[]}}"#,
    )
    .unwrap();

    // Create sessions dir with one session
    let sessions_dir = claude_dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir).unwrap();
    std::fs::write(
        sessions_dir.join("test.meta.json"),
        r#"{"id":"test","title":"Test Session"}"#,
    )
    .unwrap();

    // Run individual checks with overrides
    let config_check = GlobalConfigCheck {
        config_dir: Some(config_dir.clone()),
    };
    let workspace_check = WorkspaceConfigCheck {
        workspace: Some(workspace.clone()),
    };
    let session_check = SessionStorageCheck {
        workspace: Some(workspace),
        config_dir: Some(config_dir),
    };

    let config_result = config_check.run().await;
    let workspace_result = workspace_check.run().await;
    let session_result = session_check.run().await;

    assert_eq!(config_result.status, CheckStatus::Pass);
    assert_eq!(workspace_result.status, CheckStatus::Pass);
    assert_eq!(session_result.status, CheckStatus::Pass);
    assert!(session_result.message.contains("1 saved"));
}

// Covers: FR98 (doctor health)
#[tokio::test]
async fn test_full_doctor_flow_missing_config() {
    let tmp = tempfile::TempDir::new().unwrap();
    let workspace = tmp.path().to_path_buf();
    let config_dir = tmp.path().join("empty_config");

    let config_check = GlobalConfigCheck {
        config_dir: Some(config_dir.clone()),
    };
    let workspace_check = WorkspaceConfigCheck {
        workspace: Some(workspace.clone()),
    };
    let session_check = SessionStorageCheck {
        workspace: Some(workspace),
        config_dir: Some(config_dir),
    };

    let config_result = config_check.run().await;
    let workspace_result = workspace_check.run().await;
    let session_result = session_check.run().await;

    assert_eq!(config_result.status, CheckStatus::Fail);
    assert_eq!(workspace_result.status, CheckStatus::Warning);
    // No init run + no sessions dir → not initialized (Pass)
    assert_eq!(session_result.status, CheckStatus::Pass);
}

// ──────────────────────────────────────────────────
// Review patches: WorkspaceDirCheck (P1)
// ──────────────────────────────────────────────────

// Covers: FR98 (doctor health)
#[tokio::test]
async fn test_workspace_dir_exists_integration() {
    let tmp = tempfile::TempDir::new().unwrap();
    let workspace = tmp.path().to_path_buf();
    std::fs::create_dir_all(workspace.join(".claude")).unwrap();

    let check = WorkspaceDirCheck {
        workspace: Some(workspace),
    };
    let result = check.run().await;
    assert_eq!(result.status, CheckStatus::Pass);
}

// Covers: FR98 (doctor health)
#[tokio::test]
async fn test_workspace_dir_missing_integration() {
    let tmp = tempfile::TempDir::new().unwrap();
    let workspace = tmp.path().to_path_buf();

    let check = WorkspaceDirCheck {
        workspace: Some(workspace),
    };
    let result = check.run().await;
    assert_eq!(result.status, CheckStatus::Warning);
    assert!(result.fix.is_some());
}

// ──────────────────────────────────────────────────
// Review patches: API server error 5xx (P2)
// ──────────────────────────────────────────────────

// Story 13.2 AC8b: DELIBERATE UPDATE — ApiKeyCheck is now key-presence only (no network).
// Server error detection moved to ProviderConnectivityCheck (AC8).
#[tokio::test]
async fn test_api_key_server_error_500() {
    // De-billed: ApiKeyCheck no longer makes network calls. Key is set → Pass.
    let check = ApiKeyCheck {
        key_var_override: Some(Some("ANTHROPIC_API_KEY")),
    };
    let result = check.run().await;
    assert_eq!(result.status, CheckStatus::Pass);
    assert!(result.message.contains("set"));
}

// ──────────────────────────────────────────────────
// Review patches: Permissions null (P8)
// ──────────────────────────────────────────────────

// Covers: FR98 (doctor health)
#[tokio::test]
async fn test_workspace_config_permissions_null_integration() {
    let tmp = tempfile::TempDir::new().unwrap();
    let workspace = tmp.path().to_path_buf();
    let claude_dir = workspace.join(".claude");
    std::fs::create_dir_all(&claude_dir).unwrap();
    std::fs::write(claude_dir.join("settings.json"), r#"{"permissions":null}"#).unwrap();

    let check = WorkspaceConfigCheck {
        workspace: Some(workspace),
    };
    let result = check.run().await;
    assert_eq!(result.status, CheckStatus::Warning);
    assert!(result.message.contains("missing 'permissions' key"));
}

// ──────────────────────────────────────────────────
// Review patches: GlobalConfigCheck schema error (P6)
// ──────────────────────────────────────────────────

// Covers: FR98 (doctor health), FR97 (init wizard)
#[tokio::test]
async fn test_global_config_valid_toml_wrong_field_type_integration() {
    // Story 5-1 Task 3.5 removed `#[serde(deny_unknown_fields)]` to keep
    // configs forward-compatible as new sections are added (skills, agents,
    // profiles…). So "unknown section" is no longer a failure mode here —
    // we exercise field-type mismatch instead, which still must surface as
    // "invalid config format".
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().to_path_buf();
    std::fs::write(
        config_dir.join("config.toml"),
        "log_max_size_mb = \"not-a-number\"",
    )
    .unwrap();

    let check = GlobalConfigCheck {
        config_dir: Some(config_dir),
    };
    let result = check.run().await;
    assert_eq!(result.status, CheckStatus::Fail);
    assert!(result.message.contains("invalid config format"));
}

#[tokio::test]
async fn test_global_config_unknown_section_is_forward_compatible_integration() {
    // Story 5-1 Task 3.5: unknown top-level TOML sections MUST pass — adding
    // a new section in a future rustain release cannot break users pinned
    // to an older binary sharing the same team config.
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().to_path_buf();
    std::fs::write(
        config_dir.join("config.toml"),
        "[future_section]\nkey = \"value\"",
    )
    .unwrap();

    let check = GlobalConfigCheck {
        config_dir: Some(config_dir),
    };
    let result = check.run().await;
    assert_eq!(result.status, CheckStatus::Pass);
}
