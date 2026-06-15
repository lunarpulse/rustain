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

// ──────────────────────────────────────────────────
// Story 13.2b: MCP reachability check tests (P0)
// ──────────────────────────────────────────────────

use rustain::adapters::cli::doctor::checks::{
    MCP_PER_SERVER_BUDGET, McpReachabilityCheck, map_connect_result,
};
use rustain::adapters::mcp::error::McpError;
use rustain::domain::models::{McpServerSpec, McpTransport, mcp_server_spec::McpServerSource};

/// Helper to build a stdio McpServerSpec for tests.
fn test_mcp_spec(id: &str, command: Option<&str>, transport: McpTransport) -> McpServerSpec {
    McpServerSpec {
        id: id.to_string(),
        transport,
        command: command.map(|s| s.to_string()),
        args: vec![],
        env: Default::default(),
        url: None,
        persistent: false,
        source: McpServerSource::Workspace,
    }
}

// ── P0 #2: Exhaustive table-driven unit test of map_connect_result ──

#[test]
fn test_map_connect_result_ok_with_tools_is_pass_info() {
    let (status, tier) = map_connect_result(&Ok(()), 3);
    assert_eq!(status, CheckStatus::Pass);
    assert_eq!(tier, CheckTier::Info);
}

#[test]
fn test_map_connect_result_ok_zero_tools_is_info() {
    let (status, tier) = map_connect_result(&Ok(()), 0);
    assert_eq!(status, CheckStatus::Info);
    assert_eq!(tier, CheckTier::Info);
}

#[test]
fn test_map_connect_result_unsupported_is_skipped_info() {
    let (status, tier) = map_connect_result(
        &Err(McpError::Unsupported("http transport deferred".to_string())),
        0,
    );
    assert!(status.is_skipped());
    assert_eq!(tier, CheckTier::Info);
}

#[test]
fn test_map_connect_result_spawn_failed_is_fail_exit_affecting() {
    let (status, tier) =
        map_connect_result(&Err(McpError::SpawnFailed("no such binary".to_string())), 0);
    assert_eq!(status, CheckStatus::Fail);
    assert_eq!(tier, CheckTier::ExitAffecting);
}

#[test]
fn test_map_connect_result_handshake_failed_is_fail_exit_affecting() {
    let (status, tier) = map_connect_result(
        &Err(McpError::HandshakeFailed("initialize failed".to_string())),
        0,
    );
    assert_eq!(status, CheckStatus::Fail);
    assert_eq!(tier, CheckTier::ExitAffecting);
}

#[test]
fn test_map_connect_result_tools_list_failed_is_fail_exit_affecting() {
    let (status, tier) = map_connect_result(
        &Err(McpError::ToolsListFailed("tools/list error".to_string())),
        0,
    );
    assert_eq!(status, CheckStatus::Fail);
    assert_eq!(tier, CheckTier::ExitAffecting);
}

#[test]
fn test_map_connect_result_child_exited_is_fail_exit_affecting() {
    let (status, tier) =
        map_connect_result(&Err(McpError::ChildExited("exit code 1".to_string())), 0);
    assert_eq!(status, CheckStatus::Fail);
    assert_eq!(tier, CheckTier::ExitAffecting);
}

#[test]
fn test_map_connect_result_transport_closed_is_warning_info() {
    let (status, tier) = map_connect_result(
        &Err(McpError::TransportClosed("connection reset".to_string())),
        0,
    );
    assert_eq!(status, CheckStatus::Warning);
    assert_eq!(tier, CheckTier::Info);
}

#[test]
fn test_map_connect_result_timeout_is_warning_info() {
    let (status, tier) = map_connect_result(&Err(McpError::Timeout(5)), 0);
    assert_eq!(status, CheckStatus::Warning);
    assert_eq!(tier, CheckTier::Info);
}

#[test]
fn test_map_connect_result_cancelled_is_warning_info() {
    let (status, tier) = map_connect_result(&Err(McpError::Cancelled), 0);
    assert_eq!(status, CheckStatus::Warning);
    assert_eq!(tier, CheckTier::Info);
}

#[test]
fn test_map_connect_result_internal_is_warning_info() {
    let (status, tier) = map_connect_result(&Err(McpError::Internal("unexpected".to_string())), 0);
    assert_eq!(status, CheckStatus::Warning);
    assert_eq!(tier, CheckTier::Info);
}

#[test]
fn test_map_connect_result_call_tool_failed_is_warning_info() {
    let (status, tier) =
        map_connect_result(&Err(McpError::CallToolFailed("tool error".to_string())), 0);
    assert_eq!(status, CheckStatus::Warning);
    assert_eq!(tier, CheckTier::Info);
}

// ── P0 #3: Zero servers → Skipped ──

#[tokio::test]
async fn test_mcp_zero_servers_is_skipped() {
    let check = McpReachabilityCheck {
        servers: vec![],
        per_server_budget: MCP_PER_SERVER_BUDGET,
    };
    let result = check.run().await;
    assert!(
        result.status.is_skipped(),
        "zero servers should be Skipped, got {:?}",
        result.status
    );
    assert_eq!(result.category, "mcp");
    assert_eq!(result.tier, CheckTier::Info);
    assert!(result.message.contains("no MCP servers configured"));
}

// ── P0 #1: Broken stdio → Fail + ExitAffecting (anti-vacuous positive control) ──

#[tokio::test]
async fn test_mcp_broken_stdio_spawn_failed_is_fail_exit_affecting() {
    // Non-existent binary → SpawnFailed → Fail/ExitAffecting.
    let spec = test_mcp_spec(
        "broken-server",
        Some("__nonexistent_binary_13_2b__"),
        McpTransport::Stdio,
    );
    let check = McpReachabilityCheck {
        servers: vec![spec],
        per_server_budget: std::time::Duration::from_millis(500),
    };
    let result = check.run().await;
    assert_eq!(
        result.status,
        CheckStatus::Fail,
        "broken stdio should Fail: {:?}",
        result.message
    );
    assert_eq!(result.tier, CheckTier::ExitAffecting);
    assert_eq!(result.category, "mcp");
    assert!(result.fix.is_some(), "Fail should have a fix hint");
}

// ── P0 #2a: Exit-neutral negative control ──
// An Info/Skipped/Warning row must NOT change the exit code.
// Pair with #1 to ensure the positive control moves exit and the negative doesn't.

#[tokio::test]
async fn test_mcp_exit_neutral_negative_control() {
    // Non-stdio transport → Skipped/Info → exit-neutral.
    let spec = test_mcp_spec("http-server", None, McpTransport::Http);
    let check = McpReachabilityCheck {
        servers: vec![spec],
        per_server_budget: MCP_PER_SERVER_BUDGET,
    };
    let result = check.run().await;
    // Must NOT be Fail or ExitAffecting.
    assert_ne!(
        result.status,
        CheckStatus::Fail,
        "exit-neutral: should not Fail"
    );
    assert_eq!(
        result.tier,
        CheckTier::Info,
        "exit-neutral: tier should be Info"
    );
}

// ── P0 #4a: Non-stdio transport → Skipped("transport not supported") ──

#[tokio::test]
async fn test_mcp_http_transport_is_skipped() {
    let spec = test_mcp_spec("http-server", None, McpTransport::Http);
    let check = McpReachabilityCheck {
        servers: vec![spec],
        per_server_budget: MCP_PER_SERVER_BUDGET,
    };
    let result = check.run().await;
    assert!(
        result.status.is_skipped(),
        "Http transport should be Skipped, got {:?}",
        result.status
    );
    assert!(
        result.message.contains("transport not supported") || result.message.contains("skipped"),
        "message should mention transport: {}",
        result.message
    );
    assert_eq!(result.tier, CheckTier::Info);
}

#[tokio::test]
async fn test_mcp_sse_transport_is_skipped() {
    let spec = test_mcp_spec("sse-server", None, McpTransport::Sse);
    let check = McpReachabilityCheck {
        servers: vec![spec],
        per_server_budget: MCP_PER_SERVER_BUDGET,
    };
    let result = check.run().await;
    assert!(
        result.status.is_skipped(),
        "SSE transport should be Skipped, got {:?}",
        result.status
    );
    assert_eq!(result.tier, CheckTier::Info);
}

// ── P0 #5: Bounded + concurrent (wall-clock ≈ one budget, not N×budget) ──

#[tokio::test]
async fn test_mcp_concurrent_bounded_wall_clock() {
    // 3 broken servers with very short budgets — wall-clock should be ~budget, not 3×budget.
    let specs: Vec<McpServerSpec> = (0..3)
        .map(|i| {
            test_mcp_spec(
                &format!("broken-{i}"),
                Some("__nonexistent_binary_13_2b__"),
                McpTransport::Stdio,
            )
        })
        .collect();
    let budget = std::time::Duration::from_millis(500);
    let check = McpReachabilityCheck {
        servers: specs,
        per_server_budget: budget,
    };
    let start = std::time::Instant::now();
    let _result = check.run().await;
    let elapsed = start.elapsed();
    // Should complete in roughly 1 budget period, not 3×.
    // Allow generous margin for CI: 3s ceiling for 500ms budget.
    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "3 servers should run concurrently; elapsed {:?} >= 3s (budget {:?})",
        elapsed,
        budget
    );
    // P7: Ownership-scoped reaping — every spawned child should be reaped by Drop.
    // We can't assert try_wait() here (child is doctor-internal), but we verify
    // the process table is clean by checking no stray processes with our test prefix.
    // This is a best-effort assertion; full OS-level proof is in the nightly L3 lane.
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg("ps aux | grep __nonexistent_binary_13_2b__ | grep -v grep || true")
        .output()
        .expect("ps command should work");
    let ps_out = String::from_utf8_lossy(&output.stdout);
    assert!(
        ps_out.trim().is_empty(),
        "Stray processes found after doctor check: {}",
        ps_out
    );
}

// ── P0 #6: Probe-timeout → Warning, exit-neutral ──

#[tokio::test]
async fn test_mcp_probe_timeout_is_warning_exit_neutral() {
    // Use a spec that will spawn but hang forever (cat - reads stdin, never completes handshake).
    // With a very short budget, the outer timeout fires → Warning/Info.
    let spec = test_mcp_spec("hanging-server", Some("cat"), McpTransport::Stdio);
    let check = McpReachabilityCheck {
        servers: vec![spec],
        per_server_budget: std::time::Duration::from_millis(200),
    };
    let result = check.run().await;
    assert_eq!(
        result.status,
        CheckStatus::Warning,
        "timeout should be Warning, got {:?}",
        result.status
    );
    assert_eq!(result.tier, CheckTier::Info, "timeout should be Info tier");
    assert_eq!(result.category, "mcp");
    // P8: Assert message carries timeout reason for --json consumers.
    assert!(
        result.message.to_lowercase().contains("timeout") || result.message.contains("exceeded"),
        "timeout message should carry reason: {}",
        result.message
    );
}

#[test]
fn test_mcp_json_category_and_exit_math() {
    use rustain::adapters::cli::doctor::json::DoctorReport;

    // Build results with one mcp Fail/ExitAffecting and one mcp Pass/Info.
    let results = vec![
        CheckResult {
            name: "MCP server reachability".to_string(),
            category: "mcp".to_string(),
            status: CheckStatus::Fail,
            message: "broken: FAILED — no such binary".to_string(),
            fix: Some("check binary".to_string()),
            latency: None,
            tier: CheckTier::ExitAffecting,
        },
        CheckResult {
            name: "Other check".to_string(),
            category: "system".to_string(),
            status: CheckStatus::Pass,
            message: "ok".to_string(),
            fix: None,
            latency: None,
            tier: CheckTier::Info,
        },
    ];
    let report = DoctorReport::from_results(&results);
    assert_eq!(report.summary.failures, 1);
    assert_eq!(report.summary.passed, 1);

    // Verify mcp category appears in serialized JSON.
    let json_str = serde_json::to_string_pretty(&report).unwrap();
    assert!(
        json_str.contains("\"mcp\""),
        "JSON should contain mcp category"
    );
    assert!(
        json_str.contains("\"fail\""),
        "JSON should contain fail status"
    );

    // Exit-math: only Fail + ExitAffecting should flip exit.
    let exit_failures = results
        .iter()
        .filter(|r| r.tier == CheckTier::ExitAffecting && r.status == CheckStatus::Fail)
        .count();
    assert_eq!(exit_failures, 1, "only 1 ExitAffecting Fail");

    // Info-tier Pass does NOT flip exit.
    let info_failures = results
        .iter()
        .filter(|r| r.tier == CheckTier::Info && r.status == CheckStatus::Fail)
        .count();
    assert_eq!(info_failures, 0, "Info-tier should have 0 failures");
}

// ── P9: Per-server grouping in DoctorReport ──

#[test]
fn test_mcp_per_server_grouping_in_json() {
    use rustain::adapters::cli::doctor::json::DoctorReport;

    // Build results with multiple per-server MCP rows.
    let results = vec![CheckResult {
        name: "MCP server reachability".to_string(),
        category: "mcp".to_string(),
        status: CheckStatus::Pass,
        message: "server-a: reachable (3 tools); server-b: FAILED — no such binary".to_string(),
        fix: Some(
            "server-b: check command/path, ensure binary exists and is executable".to_string(),
        ),
        latency: None,
        tier: CheckTier::ExitAffecting,
    }];
    let report = DoctorReport::from_results(&results);
    assert_eq!(report.checks.len(), 1, "one aggregate row per category");
    assert_eq!(report.checks[0].category, "mcp");
    assert!(
        report.checks[0].message.contains("server-a:"),
        "message should contain server-a detail"
    );
    assert!(
        report.checks[0].message.contains("server-b:"),
        "message should contain server-b detail"
    );

    // JSON serialization preserves per-server detail in message.
    let json_str = serde_json::to_string_pretty(&report).unwrap();
    assert!(
        json_str.contains("server-a:"),
        "JSON should contain server-a"
    );
    assert!(
        json_str.contains("server-b:"),
        "JSON should contain server-b"
    );
    assert!(json_str.contains("mcp"), "JSON should contain mcp category");
}

// ── P0 #7 extended: status × tier → exit-code table ──

#[test]
fn test_exit_code_math_table() {
    // Only Fail + ExitAffecting flips exit. All other combos are exit-neutral.
    let test_cases: Vec<(CheckStatus, CheckTier, bool)> = vec![
        (CheckStatus::Fail, CheckTier::ExitAffecting, true),
        (CheckStatus::Fail, CheckTier::Info, false),
        (CheckStatus::Pass, CheckTier::ExitAffecting, false),
        (CheckStatus::Pass, CheckTier::Info, false),
        (CheckStatus::Warning, CheckTier::ExitAffecting, false),
        (CheckStatus::Warning, CheckTier::Info, false),
        (
            CheckStatus::Skipped("test".to_string()),
            CheckTier::ExitAffecting,
            false,
        ),
        (
            CheckStatus::Skipped("test".to_string()),
            CheckTier::Info,
            false,
        ),
    ];
    for (status, tier, should_flip) in test_cases {
        let flips = tier == CheckTier::ExitAffecting && status == CheckStatus::Fail;
        assert_eq!(
            flips, should_flip,
            "status={:?} tier={:?} expected flip={} got flip={}",
            status, tier, should_flip, flips
        );
    }
}
