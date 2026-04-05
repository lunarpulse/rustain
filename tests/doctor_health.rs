//! Integration tests for Story 2-4: Setup Health Check.

use clap::Parser;
use rustain::adapters::cli::commands::{Cli, Command};
use rustain::adapters::cli::doctor::{
    ApiKeyCheck, CheckResult, CheckStatus, GlobalConfigCheck, HealthCheck, SessionStorageCheck,
    WorkspaceDirCheck, WorkspaceConfigCheck, display_results,
};
use rustain::domain::models::AppConfig;

// ──────────────────────────────────────────────────
// Task 8.1: CLI subcommand parsing
// ──────────────────────────────────────────────────

/// `rustain doctor` sets command = Some(Command::Doctor { terminal: false }).
#[test]
fn test_cli_doctor_subcommand() {
    let cli = Cli::parse_from(["rustain", "doctor"]);
    assert!(matches!(
        cli.command,
        Some(Command::Doctor { terminal: false })
    ));
    assert!(!cli.new);
    assert!(cli.session.is_none());
    assert_eq!(cli.log_level, "info");
}

/// `rustain doctor --terminal` sets terminal = true.
#[test]
fn test_cli_doctor_terminal_flag() {
    let cli = Cli::parse_from(["rustain", "doctor", "--terminal"]);
    assert!(matches!(
        cli.command,
        Some(Command::Doctor { terminal: true })
    ));
}

/// Existing subcommands still work after adding Doctor.
#[test]
fn test_cli_init_still_works() {
    let cli = Cli::parse_from(["rustain", "init"]);
    assert!(matches!(cli.command, Some(Command::Init)));
}

/// Bare `rustain` still sets command = None.
#[test]
fn test_cli_no_subcommand_still_works() {
    let cli = Cli::parse_from(["rustain"]);
    assert!(cli.command.is_none());
}

/// Existing --new flag still works.
#[test]
fn test_cli_new_flag_unchanged() {
    let cli = Cli::parse_from(["rustain", "--new"]);
    assert!(cli.command.is_none());
    assert!(cli.new);
}

/// Existing --session flag still works.
#[test]
fn test_cli_session_flag_unchanged() {
    let cli = Cli::parse_from(["rustain", "--session", "abc-123"]);
    assert!(cli.command.is_none());
    assert_eq!(cli.session, Some("abc-123".to_string()));
}

/// --log-level works as global flag with doctor subcommand.
#[test]
fn test_cli_log_level_with_doctor() {
    let cli = Cli::parse_from(["rustain", "--log-level", "debug", "doctor"]);
    assert!(matches!(
        cli.command,
        Some(Command::Doctor { terminal: false })
    ));
    assert_eq!(cli.log_level, "debug");
}

// ──────────────────────────────────────────────────
// Task 8.2: CheckResult formatting
// ──────────────────────────────────────────────────

#[test]
fn test_display_results_formats_all_statuses() {
    let results = vec![
        CheckResult {
            name: "Pass check".to_string(),
            status: CheckStatus::Pass,
            message: "all good".to_string(),
            fix: None,
        },
        CheckResult {
            name: "Warn check".to_string(),
            status: CheckStatus::Warning,
            message: "something off".to_string(),
            fix: Some("try this".to_string()),
        },
        CheckResult {
            name: "Fail check".to_string(),
            status: CheckStatus::Fail,
            message: "broken".to_string(),
            fix: Some("fix it".to_string()),
        },
    ];
    // Should not panic; output goes to stdout
    display_results(&results);
}

// ──────────────────────────────────────────────────
// Task 8.3: Summary counting
// ──────────────────────────────────────────────────

#[test]
fn test_summary_counts_various_combos() {
    let results = vec![
        CheckResult {
            name: "A".to_string(),
            status: CheckStatus::Pass,
            message: "".to_string(),
            fix: None,
        },
        CheckResult {
            name: "B".to_string(),
            status: CheckStatus::Pass,
            message: "".to_string(),
            fix: None,
        },
        CheckResult {
            name: "C".to_string(),
            status: CheckStatus::Pass,
            message: "".to_string(),
            fix: None,
        },
        CheckResult {
            name: "D".to_string(),
            status: CheckStatus::Warning,
            message: "".to_string(),
            fix: None,
        },
        CheckResult {
            name: "E".to_string(),
            status: CheckStatus::Fail,
            message: "".to_string(),
            fix: None,
        },
        CheckResult {
            name: "F".to_string(),
            status: CheckStatus::Fail,
            message: "".to_string(),
            fix: None,
        },
    ];
    let pass = results.iter().filter(|r| r.status == CheckStatus::Pass).count();
    let warn = results.iter().filter(|r| r.status == CheckStatus::Warning).count();
    let fail = results.iter().filter(|r| r.status == CheckStatus::Fail).count();
    assert_eq!(pass, 3);
    assert_eq!(warn, 1);
    assert_eq!(fail, 2);
}

// ──────────────────────────────────────────────────
// Task 8.4: GlobalConfigCheck
// ──────────────────────────────────────────────────

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

#[tokio::test]
async fn test_api_key_not_set() {
    let check = ApiKeyCheck {
        key_var_override: Some(None), // simulate no key found
        key_value_override: None,
        base_url_override: Some(None),
    };
    let result = check.run().await;
    assert_eq!(result.status, CheckStatus::Fail);
    assert!(result.message.contains("not set"));
    assert!(result.fix.is_some());
}

#[tokio::test]
async fn test_api_key_valid_with_mock_400() {
    let mut server = mockito::Server::new_async().await;

    let mock = server
        .mock("POST", "/v1/messages")
        .with_status(400)
        .with_body(r#"{"type":"error","error":{"type":"invalid_request_error"}}"#)
        .create_async()
        .await;

    let check = ApiKeyCheck {
        key_var_override: Some(Some("ANTHROPIC_API_KEY")),
        key_value_override: Some("test-key-value".to_string()),
        base_url_override: Some(Some(server.url())),
    };
    let result = check.run().await;
    assert_eq!(result.status, CheckStatus::Pass);
    assert!(result.message.contains("valid"));
    assert!(result.message.contains("ANTHROPIC_API_KEY"));

    mock.assert_async().await;
}

#[tokio::test]
async fn test_api_key_invalid_401_with_mock() {
    let mut server = mockito::Server::new_async().await;

    let mock = server
        .mock("POST", "/v1/messages")
        .with_status(401)
        .with_body(r#"{"type":"error","error":{"type":"authentication_error"}}"#)
        .create_async()
        .await;

    let check = ApiKeyCheck {
        key_var_override: Some(Some("ANTHROPIC_API_KEY")),
        key_value_override: Some("bad-key".to_string()),
        base_url_override: Some(Some(server.url())),
    };
    let result = check.run().await;
    assert_eq!(result.status, CheckStatus::Fail);
    assert!(result.message.contains("invalid key"));

    mock.assert_async().await;
}

#[tokio::test]
async fn test_api_key_bearer_auth_token() {
    let mut server = mockito::Server::new_async().await;

    let mock = server
        .mock("POST", "/v1/messages")
        .match_header("authorization", mockito::Matcher::Regex("Bearer .+".to_string()))
        .with_status(400)
        .create_async()
        .await;

    let check = ApiKeyCheck {
        key_var_override: Some(Some("ANTHROPIC_AUTH_TOKEN")),
        key_value_override: Some("some-uuid-token".to_string()),
        base_url_override: Some(Some(server.url())),
    };
    let result = check.run().await;
    assert_eq!(result.status, CheckStatus::Pass);
    assert!(result.message.contains("ANTHROPIC_AUTH_TOKEN"));

    mock.assert_async().await;
}

#[tokio::test]
async fn test_api_key_custom_url_fix_message() {
    let mut server = mockito::Server::new_async().await;

    let mock = server
        .mock("POST", "/v1/messages")
        .with_status(401)
        .create_async()
        .await;

    let check = ApiKeyCheck {
        key_var_override: Some(Some("ANTHROPIC_API_KEY")),
        key_value_override: Some("bad-key".to_string()),
        base_url_override: Some(Some(server.url())),
    };
    let result = check.run().await;
    assert_eq!(result.status, CheckStatus::Fail);
    assert!(
        result.fix.as_ref().unwrap().contains("your provider"),
        "Fix should mention provider for custom URL, got: {}",
        result.fix.unwrap()
    );

    mock.assert_async().await;
}

// ──────────────────────────────────────────────────
// Task 8.10: terminal_info relocation
// ──────────────────────────────────────────────────

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

#[tokio::test]
async fn test_api_key_server_error_500() {
    let mut server = mockito::Server::new_async().await;

    let mock = server
        .mock("POST", "/v1/messages")
        .with_status(500)
        .with_body(r#"{"type":"error","error":{"type":"api_error"}}"#)
        .create_async()
        .await;

    let check = ApiKeyCheck {
        key_var_override: Some(Some("ANTHROPIC_API_KEY")),
        key_value_override: Some("test-key".to_string()),
        base_url_override: Some(Some(server.url())),
    };
    let result = check.run().await;
    assert_eq!(result.status, CheckStatus::Warning);
    assert!(result.message.contains("API server error"));

    mock.assert_async().await;
}

// ──────────────────────────────────────────────────
// Review patches: Permissions null (P8)
// ──────────────────────────────────────────────────

#[tokio::test]
async fn test_workspace_config_permissions_null_integration() {
    let tmp = tempfile::TempDir::new().unwrap();
    let workspace = tmp.path().to_path_buf();
    let claude_dir = workspace.join(".claude");
    std::fs::create_dir_all(&claude_dir).unwrap();
    std::fs::write(
        claude_dir.join("settings.json"),
        r#"{"permissions":null}"#,
    )
    .unwrap();

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

#[tokio::test]
async fn test_global_config_valid_toml_wrong_schema_integration() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().to_path_buf();
    std::fs::write(config_dir.join("config.toml"), "[section]\nkey = \"value\"").unwrap();

    let check = GlobalConfigCheck {
        config_dir: Some(config_dir),
    };
    let result = check.run().await;
    assert_eq!(result.status, CheckStatus::Fail);
    assert!(result.message.contains("invalid config format"));
}
