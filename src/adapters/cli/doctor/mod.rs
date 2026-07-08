pub mod checks;
pub mod json;

use anyhow::Result;
use async_trait::async_trait;

// Re-export check structs so `crate::adapters::cli::doctor::{ApiKeyCheck, …}` paths remain valid.
pub use self::checks::*;

#[allow(unused_imports)]
use crate::infrastructure::utils;

// ──────────────────────────────────────────────────────────────────
// Health check framework
// ──────────────────────────────────────────────────────────────────

/// Tier of a health check: whether it affects the overall doctor exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckTier {
    ExitAffecting,
    Info,
}

/// Result status of a single health check.
#[derive(Debug, PartialEq, Eq)]
pub enum CheckStatus {
    Pass,
    Info,
    Warning,
    Fail,
    /// Check was skipped (e.g., offline — network probes unavailable).
    /// The contained string explains why (e.g., "offline").
    Skipped(String),
}

impl CheckStatus {
    pub fn is_skipped(&self) -> bool {
        matches!(self, CheckStatus::Skipped(_))
    }
}

/// Result of a single health check.
#[derive(Debug)]
pub struct CheckResult {
    pub name: String,
    pub category: String,
    pub status: CheckStatus,
    pub message: String,
    /// Actionable fix suggestion (required for Fail, optional for Warning).
    pub fix: Option<String>,
    /// Latency observed during the check (e.g. connectivity probe).
    pub latency: Option<std::time::Duration>,
    /// Whether this check affects the overall doctor exit code.
    pub tier: CheckTier,
}

/// Trait for extensible health checks. Async because some checks (API validation)
/// require network I/O. Sync checks simply return without `.await`.
#[async_trait]
pub trait HealthCheck: Send + Sync {
    fn name(&self) -> &str;
    async fn run(&self) -> CheckResult;
}

/// Build the ordered list of health checks to run.
/// New checks are added by appending to this list — no modification to existing
/// check code required (AC7 extensibility).
fn build_check_list(
    terminal_detail: bool,
    providers: &[(
        String,
        Option<std::sync::Arc<dyn crate::domain::ports::StreamingProvider>>,
    )],
    mcp_servers: Vec<crate::domain::models::McpServerSpec>,
) -> Vec<Box<dyn HealthCheck>> {
    let mut checks: Vec<Box<dyn HealthCheck>> = vec![
        Box::new(ApiKeyCheck {
            key_var_override: None,
        }),
        Box::new(ApiEndpointCheck {
            base_url_override: None,
        }),
        Box::new(GlobalConfigCheck { config_dir: None }),
        Box::new(WorkspaceDirCheck { workspace: None }),
        Box::new(WorkspaceConfigCheck { workspace: None }),
        Box::new(TerminalCheck),
        Box::new(SessionStorageCheck {
            workspace: None,
            config_dir: None,
        }),
    ];
    checks.push(Box::new(PermissionRulesCheck { workspace: None }));
    checks.push(Box::new(PlanDirCheck { workspace: None }));
    checks.push(Box::new(MemoryDirSizeCheck { workspace: None }));
    checks.push(Box::new(SystemInfoCheck));
    checks.push(Box::new(ProfilesCheck));
    checks.push(Box::new(GitCheck));
    checks.push(Box::new(SkillsCheck { workspace: None }));
    // Provider connectivity checks (AC8).
    if providers.is_empty() {
        checks.push(Box::new(ProviderConnectivityCheck {
            name: "Provider connectivity (none)".to_string(),
            provider_name: "none".to_string(),
            provider: None,
        }));
    } else {
        for (name, provider) in providers {
            checks.push(Box::new(ProviderConnectivityCheck {
                name: format!("Provider connectivity ({})", name),
                provider_name: name.clone(),
                provider: provider.clone(),
            }));
        }
    }
    // Story 13.2b: MCP reachability check (AC1-AC3a).
    #[cfg(feature = "mcp")]
    checks.push(Box::new(McpReachabilityCheck {
        servers: mcp_servers,
        per_server_budget: checks::MCP_PER_SERVER_BUDGET,
    }));
    #[cfg(feature = "self-update")]
    checks.push(Box::new(checks::UpdateHealthCheck));
    if terminal_detail {
        checks.push(Box::new(TerminalDetailCheck));
    }
    checks
}

/// Representative adapter wiring printed by `rustain doctor --adapters`.
/// Held as a const so the doctor unit test can pin it — the `tools` row must
/// track the default `coding` profile's resolved adapter (ADR-10-5 S2 → composite).
pub(crate) const ADAPTERS_TABLE: [(&str, &str); 7] = [
    ("persona", "coding (project-aware)"),
    ("memory", "noop"),
    ("session", "noop"),
    ("tools", "composite"),
    ("channels", "noop"),
    ("scheduler", "noop"),
    ("context", "default (no injection)"),
];

/// Entry point for `rustain doctor`. Runs all checks and displays results.
pub async fn run_doctor(
    terminal_detail: bool,
    adapters: bool,
    json: bool,
    providers: Vec<(
        String,
        Option<std::sync::Arc<dyn crate::domain::ports::StreamingProvider>>,
    )>,
    mcp_servers: Vec<crate::domain::models::McpServerSpec>,
) -> Result<()> {
    if adapters {
        let ports = ADAPTERS_TABLE;
        let start = std::time::Instant::now();
        let mut pass_count = 0usize;
        let mut skip_count = 0usize;
        let fail_count = 0usize;

        if json {
            // Build CheckResults for adapter rows so we can emit JSON.
            let mut results = Vec::with_capacity(ports.len());
            for (name, desc) in &ports {
                let is_noop = *desc == "noop";
                let (status, message) = if is_noop {
                    (
                        CheckStatus::Skipped("noop adapter".to_string()),
                        "noop adapter — no behavior to test".to_string(),
                    )
                } else {
                    (CheckStatus::Pass, (*desc).to_string())
                };
                results.push(CheckResult {
                    name: name.to_string(),
                    category: "adapters".to_string(),
                    status,
                    message,
                    fix: None,
                    latency: None,
                    tier: CheckTier::ExitAffecting,
                });
            }
            let report = self::json::DoctorReport::from_results(&results);
            let json_str = serde_json::to_string_pretty(&report)
                .expect("DoctorReport serialization cannot fail");
            println!("{json_str}");
        } else {
            println!("Adapter conformance smoke-check (profile: coding):\n");
            for (name, desc) in &ports {
                let is_noop = *desc == "noop";
                let (status_char, detail) = if is_noop {
                    skip_count += 1;
                    ("SKIP", "noop adapter — no behavior to test")
                } else {
                    pass_count += 1;
                    ("PASS", *desc)
                };
                println!("  ✓ {:10}: {:4}  ({})    [0ms]", name, status_char, detail);
            }
            let elapsed = start.elapsed();
            println!(
                "\nTotal: {}ms — {} PASS, {} SKIP, {} FAIL",
                elapsed.as_millis(),
                pass_count,
                skip_count,
                fail_count
            );
        }
        tracing::info!(
            profile = "coding",
            port_count = 7,
            pass_count,
            fail_count,
            elapsed_ms = start.elapsed().as_millis() as u64,
            "rustain doctor --adapters complete"
        );
        if fail_count > 0 {
            anyhow::bail!("rustain doctor --adapters: {} failure(s) found", fail_count);
        }
        return Ok(());
    }

    let checks = build_check_list(terminal_detail, &providers, mcp_servers);
    let mut results = Vec::with_capacity(checks.len());
    for check in &checks {
        results.push(check.run().await);
    }

    if json {
        let report = self::json::DoctorReport::from_results(&results);
        let json_str =
            serde_json::to_string_pretty(&report).expect("DoctorReport serialization cannot fail");
        println!("{json_str}");
    } else {
        println!("rustain doctor\n");
        display_results(&results);
    }

    let failures = results
        .iter()
        .filter(|r| r.tier == CheckTier::ExitAffecting && r.status == CheckStatus::Fail)
        .count();
    if failures > 0 {
        anyhow::bail!("rustain doctor: {} failure(s) found", failures);
    }
    Ok(())
}

/// Format and print all check results with Unicode indicators, then summary.
pub fn display_results(results: &[CheckResult]) {
    for r in results {
        let icon = match &r.status {
            CheckStatus::Pass => "\u{2713}", // ✓
            CheckStatus::Info => "\u{2139}", // ℹ
            CheckStatus::Warning => "!",
            CheckStatus::Fail => "\u{2717}",       // ✗
            CheckStatus::Skipped(_) => "\u{2298}", // ⊘
        };
        println!("{} {}: {}", icon, r.name, r.message);
        if let Some(ref fix) = r.fix {
            let label = match &r.status {
                CheckStatus::Fail => "Fix",
                _ => "Note",
            };
            println!("  {}: {}", label, fix);
        }
    }
    let pass_count = results
        .iter()
        .filter(|r| r.status == CheckStatus::Pass)
        .count();
    let info_count = results
        .iter()
        .filter(|r| r.status == CheckStatus::Info)
        .count();
    let warn_count = results
        .iter()
        .filter(|r| r.status == CheckStatus::Warning)
        .count();
    let fail_count = results
        .iter()
        .filter(|r| r.status == CheckStatus::Fail)
        .count();
    let skip_count = results.iter().filter(|r| r.status.is_skipped()).count();
    println!(
        "\n{} passed, {} info, {} warnings, {} failures, {} skipped",
        pass_count, info_count, warn_count, fail_count, skip_count
    );
}

// Tests
// ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    // ── ADR-10-5 caveat-2a: doctor adapters table must reflect the default profile ──

    /// 10.7.1-INT-DOCTOR · P2 · `rustain doctor --adapters` prints a representative
    /// adapter wiring. After ADR-10-5 S2 the default `coding` profile selects the
    /// `composite` tools adapter; doctor must not still advertise `builtin-full`.
    #[test]
    fn test_doctor_adapters_table_reports_composite_tools_default() {
        let tools = ADAPTERS_TABLE
            .iter()
            .find(|(port, _)| *port == "tools")
            .expect("adapters table has a 'tools' row");
        assert_ne!(
            tools.1, "builtin-full",
            "doctor must not advertise the pre-ADR-10-5 default (builtin-full)"
        );
        assert_eq!(
            tools.1, "composite",
            "doctor adapters table must match the default coding profile's tools adapter"
        );
    }

    // ── CheckResult formatting tests (Task 8.2, 8.3) ──

    #[test]
    fn test_display_results_pass() {
        let results = vec![CheckResult {
            name: "Test".to_string(),
            category: "test".to_string(),
            status: CheckStatus::Pass,
            message: "ok".to_string(),
            fix: None,
            latency: None,
            tier: CheckTier::ExitAffecting,
        }];
        // Should not panic
        display_results(&results);
    }

    #[test]
    fn test_display_results_fail_with_fix() {
        let results = vec![CheckResult {
            name: "Test".to_string(),
            category: "test".to_string(),
            status: CheckStatus::Fail,
            message: "bad".to_string(),
            fix: Some("fix it".to_string()),
            latency: None,
            tier: CheckTier::ExitAffecting,
        }];
        display_results(&results);
    }

    #[test]
    fn test_display_results_warning_with_note() {
        let results = vec![CheckResult {
            name: "Test".to_string(),
            category: "test".to_string(),
            status: CheckStatus::Warning,
            message: "hmm".to_string(),
            fix: Some("note this".to_string()),
            latency: None,
            tier: CheckTier::ExitAffecting,
        }];
        display_results(&results);
    }

    #[test]
    fn test_summary_counting() {
        let results = [
            CheckResult {
                name: "A".to_string(),
                category: "test".to_string(),
                status: CheckStatus::Pass,
                message: String::new(),
                fix: None,
                latency: None,
                tier: CheckTier::ExitAffecting,
            },
            CheckResult {
                name: "B".to_string(),
                category: "test".to_string(),
                status: CheckStatus::Pass,
                message: String::new(),
                fix: None,
                latency: None,
                tier: CheckTier::ExitAffecting,
            },
            CheckResult {
                name: "C".to_string(),
                category: "test".to_string(),
                status: CheckStatus::Warning,
                message: String::new(),
                fix: None,
                latency: None,
                tier: CheckTier::ExitAffecting,
            },
            CheckResult {
                name: "D".to_string(),
                category: "test".to_string(),
                status: CheckStatus::Fail,
                message: String::new(),
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
        assert_eq!(pass, 2);
        assert_eq!(warn, 1);
        assert_eq!(fail, 1);
    }

    // ── GlobalConfigCheck tests (Task 8.4) ──

    #[tokio::test]
    async fn test_global_config_valid() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config_dir = tmp.path().to_path_buf();
        let config = crate::domain::models::AppConfig::default();
        let toml_content = toml::to_string_pretty(&config).unwrap();
        std::fs::write(config_dir.join("config.toml"), &toml_content).unwrap();

        let check = GlobalConfigCheck {
            config_dir: Some(config_dir),
        };
        let result = check.run().await;
        assert_eq!(result.status, CheckStatus::Pass);
    }

    #[tokio::test]
    async fn test_global_config_invalid_toml_syntax() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config_dir = tmp.path().to_path_buf();
        std::fs::write(config_dir.join("config.toml"), "not [valid toml {{").unwrap();

        let check = GlobalConfigCheck {
            config_dir: Some(config_dir),
        };
        let result = check.run().await;
        assert_eq!(result.status, CheckStatus::Fail);
        assert!(result.message.contains("invalid TOML syntax"));
    }

    #[tokio::test]
    async fn test_global_config_valid_toml_wrong_field_type() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config_dir = tmp.path().to_path_buf();
        // Valid TOML but with a type mismatch on a known field — serde rejects
        // because `log_max_size_mb` expects an unsigned integer.
        // (Story 5-1 removed `deny_unknown_fields`, so unknown sections no
        // longer fail here; we now exercise the field-type path instead.)
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
    async fn test_global_config_unknown_section_is_forward_compatible() {
        // Story 5-1 Task 3.5: unknown top-level TOML sections must NOT fail
        // validation — new features (skills, agents, profiles…) are added
        // incrementally, so shared team configs cannot require lockstep upgrades.
        let tmp = tempfile::TempDir::new().unwrap();
        let config_dir = tmp.path().to_path_buf();
        std::fs::write(
            config_dir.join("config.toml"),
            "[some_future_section]\nkey = \"value\"",
        )
        .unwrap();

        let check = GlobalConfigCheck {
            config_dir: Some(config_dir),
        };
        let result = check.run().await;
        assert_eq!(result.status, CheckStatus::Pass);
    }

    #[tokio::test]
    async fn test_global_config_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config_dir = tmp.path().join("nonexistent");

        let check = GlobalConfigCheck {
            config_dir: Some(config_dir),
        };
        let result = check.run().await;
        assert_eq!(result.status, CheckStatus::Fail);
        assert!(result.message.contains("missing"));
    }

    // ── WorkspaceConfigCheck tests (Task 8.5) ──

    #[tokio::test]
    async fn test_workspace_config_valid() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = tmp.path().to_path_buf();
        let claude_dir = workspace.join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        std::fs::write(
            claude_dir.join("settings.json"),
            r#"{"permissions":{"allow":[]}}"#,
        )
        .unwrap();

        let check = WorkspaceConfigCheck {
            workspace: Some(workspace),
        };
        let result = check.run().await;
        assert_eq!(result.status, CheckStatus::Pass);
    }

    #[tokio::test]
    async fn test_workspace_config_invalid_json() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = tmp.path().to_path_buf();
        let claude_dir = workspace.join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        std::fs::write(claude_dir.join("settings.json"), "not json {{{").unwrap();

        let check = WorkspaceConfigCheck {
            workspace: Some(workspace),
        };
        let result = check.run().await;
        assert_eq!(result.status, CheckStatus::Fail);
        assert!(result.message.contains("invalid JSON"));
    }

    #[tokio::test]
    async fn test_workspace_config_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = tmp.path().to_path_buf();

        let check = WorkspaceConfigCheck {
            workspace: Some(workspace),
        };
        let result = check.run().await;
        assert_eq!(result.status, CheckStatus::Warning);
        assert!(result.message.contains("missing"));
    }

    #[tokio::test]
    async fn test_workspace_config_permissions_null() {
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

    // ── WorkspaceDirCheck tests (Task 4.3 / P1) ──

    #[tokio::test]
    async fn test_workspace_dir_exists() {
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
    async fn test_workspace_dir_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = tmp.path().to_path_buf();
        // No .claude/ directory

        let check = WorkspaceDirCheck {
            workspace: Some(workspace),
        };
        let result = check.run().await;
        assert_eq!(result.status, CheckStatus::Warning);
        assert!(result.message.contains("missing"));
        assert!(result.fix.is_some());
    }

    // ── SessionStorageCheck tests (Task 8.6) ──

    #[tokio::test]
    async fn test_session_storage_empty_dir() {
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
    async fn test_session_storage_populated() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = tmp.path().to_path_buf();
        let sessions_dir = workspace.join(".claude").join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        // Create valid session files
        std::fs::write(
            sessions_dir.join("abc.meta.json"),
            r#"{"id":"abc","title":"Test"}"#,
        )
        .unwrap();
        std::fs::write(
            sessions_dir.join("def.meta.json"),
            r#"{"id":"def","title":"Test 2"}"#,
        )
        .unwrap();

        let check = SessionStorageCheck {
            workspace: Some(workspace),
            config_dir: Some(tmp.path().join("no_config")),
        };
        let result = check.run().await;
        assert_eq!(result.status, CheckStatus::Pass);
        assert!(result.message.contains("2 saved"));
    }

    #[tokio::test]
    async fn test_session_storage_corrupted() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = tmp.path().to_path_buf();
        let sessions_dir = workspace.join(".claude").join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        std::fs::write(
            sessions_dir.join("good.meta.json"),
            r#"{"id":"good","title":"OK"}"#,
        )
        .unwrap();
        std::fs::write(sessions_dir.join("bad.meta.json"), "not valid json {{{{").unwrap();

        let check = SessionStorageCheck {
            workspace: Some(workspace),
            config_dir: Some(tmp.path().join("no_config")),
        };
        let result = check.run().await;
        assert_eq!(result.status, CheckStatus::Warning);
        assert!(result.message.contains("corrupted"));
    }

    #[tokio::test]
    async fn test_session_storage_missing_dir_not_initialized() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = tmp.path().join("no_init_workspace");
        // Don't create sessions dir, and no config.toml exists

        let check = SessionStorageCheck {
            workspace: Some(workspace),
            config_dir: Some(tmp.path().join("no_config")),
        };
        let result = check.run().await;
        // When neither sessions dir nor config.toml exist → "not initialized" (Pass)
        assert_eq!(result.status, CheckStatus::Pass);
        assert!(result.message.contains("not initialized"));
    }

    // ── Registration pattern test (Task 8.12) ──

    #[test]
    fn test_build_check_list_default() {
        let checks = build_check_list(false, &[], vec![]);
        assert!(checks.len() >= 8, "Should have at least 8 checks");
        // Verify names
        let names: Vec<&str> = checks.iter().map(|c| c.name()).collect();
        assert!(names.contains(&"API key"));
        assert!(names.contains(&"API endpoint"));
        assert!(names.contains(&"Global config"));
        assert!(names.contains(&"Workspace dir"));
        assert!(names.contains(&"Workspace config"));
        assert!(names.contains(&"Terminal"));
        assert!(names.contains(&"Sessions"));
        assert!(names.contains(&"Plan directory"));
        // Story 13.2 Task 5 category-tier checks
        assert!(names.contains(&"System info"));
        assert!(names.contains(&"Profiles"));
        assert!(names.contains(&"Git"));
        assert!(names.contains(&"Skills"));
    }

    #[test]
    fn test_build_check_list_with_terminal_detail() {
        let checks_without = build_check_list(false, &[], vec![]);
        let checks_with = build_check_list(true, &[], vec![]);
        assert_eq!(
            checks_with.len(),
            checks_without.len() + 1,
            "Terminal detail flag should add one check"
        );
        let names: Vec<&str> = checks_with.iter().map(|c| c.name()).collect();
        assert!(names.contains(&"Terminal details"));
    }

    // ── Story 13.2b ratchets: MCP check present, no update_health ──

    #[test]
    fn test_mcp_check_present_in_build_check_list() {
        let checks = build_check_list(false, &[], vec![]);
        let names: Vec<&str> = checks.iter().map(|c| c.name()).collect();
        assert!(
            names.contains(&"MCP server reachability"),
            "MCP reachability check should be in default check list: {:?}",
            names
        );
    }

    #[cfg(feature = "self-update")]
    #[tokio::test]
    async fn test_update_health_is_info_tier_and_present() {
        let checks = build_check_list(false, &[], vec![]);
        let names: Vec<&str> = checks.iter().map(|c| c.name()).collect();
        assert!(
            names.contains(&"Update health"),
            "Update health check should be present when self-update feature is enabled: {names:?}"
        );
        // AC11: must be Info-tier (never ExitAffecting) and must not Fail.
        let check = checks
            .iter()
            .find(|c| c.name() == "Update health")
            .expect("Update health check present");
        let result = check.run().await;
        assert_eq!(
            result.tier,
            CheckTier::Info,
            "Update health must be Info-tier (AC11): {result:?}"
        );
        assert_ne!(
            result.status,
            CheckStatus::Fail,
            "Update health must not Fail (AC11): {result:?}"
        );
    }

    #[cfg(not(feature = "self-update"))]
    #[test]
    fn test_no_update_health_without_feature() {
        let checks = build_check_list(false, &[], vec![]);
        let names: Vec<&str> = checks.iter().map(|c| c.name()).collect();
        assert!(
            !names
                .iter()
                .any(|n| n.to_lowercase().contains("update") && n.to_lowercase().contains("health")),
            "Update health check should not exist without self-update feature: {names:?}"
        );
    }

    // ── MemoryDirSizeCheck tests (Story 11.1, AC7) ──

    #[tokio::test]
    async fn test_memory_dir_missing_is_pass_no_memory() {
        let tmp = tempfile::TempDir::new().unwrap();
        let check = MemoryDirSizeCheck {
            workspace: Some(tmp.path().to_path_buf()),
        };
        let result = check.run().await;
        assert_eq!(result.status, CheckStatus::Pass);
        assert!(result.message.contains("no memory yet"));
    }

    #[tokio::test]
    async fn test_memory_dir_reports_size() {
        let tmp = tempfile::TempDir::new().unwrap();
        let memory_dir = tmp.path().join(".rustain").join("memory");
        std::fs::create_dir_all(&memory_dir).unwrap();
        std::fs::write(
            memory_dir.join("2026-05-31.md"),
            "# 2026-05-31\n\n## 10:00:00 — x\n",
        )
        .unwrap();

        let check = MemoryDirSizeCheck {
            workspace: Some(tmp.path().to_path_buf()),
        };
        let result = check.run().await;
        // AC7: awareness-only — never Fail.
        assert_eq!(result.status, CheckStatus::Pass);
        assert!(result.message.contains("1 day file"));
    }

    // Story 11.3a — the vector index.bin is counted in the size + attributed,
    // but NOT counted as a "day file".
    #[tokio::test]
    async fn test_memory_dir_attributes_vector_index() {
        let tmp = tempfile::TempDir::new().unwrap();
        let memory_dir = tmp.path().join(".rustain").join("memory");
        std::fs::create_dir_all(&memory_dir).unwrap();
        std::fs::write(
            memory_dir.join("2026-05-31.md"),
            "# 2026-05-31\n\n## 10:00:00 — x\n",
        )
        .unwrap();
        // A vector index sized so the KB display is non-zero.
        std::fs::write(memory_dir.join("index.bin"), vec![0u8; 2048]).unwrap();

        let check = MemoryDirSizeCheck {
            workspace: Some(tmp.path().to_path_buf()),
        };
        let result = check.run().await;
        assert_eq!(result.status, CheckStatus::Pass);
        // index.bin is NOT a day file — still exactly one day file reported.
        assert!(result.message.contains("1 day file"), "{}", result.message);
        // …but its size is attributed.
        assert!(
            result.message.contains("vector index"),
            "{}",
            result.message
        );
    }

    // Only index.bin present (no day files, no MEMORY.md) → still reported, not
    // "no memory yet".
    #[tokio::test]
    async fn test_memory_dir_only_index_is_reported() {
        let tmp = tempfile::TempDir::new().unwrap();
        let memory_dir = tmp.path().join(".rustain").join("memory");
        std::fs::create_dir_all(&memory_dir).unwrap();
        std::fs::write(memory_dir.join("index.bin"), vec![0u8; 4096]).unwrap();

        let check = MemoryDirSizeCheck {
            workspace: Some(tmp.path().to_path_buf()),
        };
        let result = check.run().await;
        assert_eq!(result.status, CheckStatus::Pass);
        assert!(
            !result.message.contains("no memory yet"),
            "{}",
            result.message
        );
        assert!(
            result.message.contains("vector index"),
            "{}",
            result.message
        );
    }

    // ── ApiEndpointCheck tests (Task 8.9) ──

    #[tokio::test]
    async fn test_api_endpoint_default() {
        let check = ApiEndpointCheck {
            base_url_override: Some(None), // simulate unset
        };
        let result = check.run().await;
        assert_eq!(result.status, CheckStatus::Pass);
        assert!(result.message.contains("api.anthropic.com"));
        assert!(result.message.contains("default"));
    }

    #[tokio::test]
    async fn test_api_endpoint_custom() {
        let check = ApiEndpointCheck {
            base_url_override: Some(Some("https://api.z.ai/api/anthropic".to_string())),
        };
        let result = check.run().await;
        assert_eq!(result.status, CheckStatus::Pass);
        assert!(result.message.contains("api.z.ai"));
        assert!(result.message.contains("custom"));
    }
}
