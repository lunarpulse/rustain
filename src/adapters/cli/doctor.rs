use anyhow::Result;
use async_trait::async_trait;

use crate::infrastructure::{paths, terminal_info, utils};

// ──────────────────────────────────────────────────────────────────
// Health check framework (Task 2)
// ──────────────────────────────────────────────────────────────────

/// Result status of a single health check.
#[derive(Debug, PartialEq, Eq)]
pub enum CheckStatus {
    Pass,
    Warning,
    Fail,
}

/// Result of a single health check.
#[derive(Debug)]
pub struct CheckResult {
    pub name: String,
    pub status: CheckStatus,
    pub message: String,
    /// Actionable fix suggestion (required for Fail, optional for Warning).
    pub fix: Option<String>,
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
fn build_check_list(terminal_detail: bool) -> Vec<Box<dyn HealthCheck>> {
    let mut checks: Vec<Box<dyn HealthCheck>> = vec![
        Box::new(ApiKeyCheck {
            key_var_override: None,
            key_value_override: None,
            base_url_override: None,
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
    if terminal_detail {
        checks.push(Box::new(TerminalDetailCheck));
    }
    checks
}

/// Entry point for `rustain doctor`. Runs all checks and displays results.
pub async fn run_doctor(terminal_detail: bool) -> Result<()> {
    println!("rustain doctor\n");

    let checks = build_check_list(terminal_detail);
    let mut results = Vec::with_capacity(checks.len());
    for check in &checks {
        results.push(check.run().await);
    }
    display_results(&results);

    let failures = results
        .iter()
        .filter(|r| matches!(r.status, CheckStatus::Fail))
        .count();
    if failures > 0 {
        anyhow::bail!("rustain doctor: {} failure(s) found", failures);
    }
    Ok(())
}

/// Format and print all check results with Unicode indicators, then summary.
pub fn display_results(results: &[CheckResult]) {
    for r in results {
        let icon = match r.status {
            CheckStatus::Pass => "\u{2713}", // ✓
            CheckStatus::Warning => "!",
            CheckStatus::Fail => "\u{2717}", // ✗
        };
        println!("{} {}: {}", icon, r.name, r.message);
        if let Some(ref fix) = r.fix {
            let label = match r.status {
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
    let warn_count = results
        .iter()
        .filter(|r| r.status == CheckStatus::Warning)
        .count();
    let fail_count = results
        .iter()
        .filter(|r| r.status == CheckStatus::Fail)
        .count();
    println!(
        "\n{} passed, {} warnings, {} failures",
        pass_count, warn_count, fail_count
    );
}

// ──────────────────────────────────────────────────────────────────
// Task 3: API key health check
// ──────────────────────────────────────────────────────────────────

/// Check that an API key env var is set and optionally validate it against the API.
pub struct ApiKeyCheck {
    /// Override for testing: Some(Some("VAR_NAME")) = key found, Some(None) = no key, None = read env.
    pub key_var_override: Option<Option<&'static str>>,
    /// Override key value for testing (avoids reading env var value).
    pub key_value_override: Option<String>,
    /// Override base URL for testing: Some(Some(url)) = custom, Some(None) = default, None = read env.
    pub base_url_override: Option<Option<String>>,
}

impl ApiKeyCheck {
    fn resolve_key_var(&self) -> Option<&'static str> {
        match &self.key_var_override {
            Some(val) => *val,
            None => super::init::find_api_key_var(),
        }
    }

    fn resolve_base_url(&self) -> (String, bool) {
        let custom = match &self.base_url_override {
            Some(val) => val.clone(),
            None => utils::env_var_trimmed("ANTHROPIC_BASE_URL"),
        };
        let is_custom = custom.is_some();
        let url = utils::normalize_base_url(
            &custom.unwrap_or_else(|| "https://api.anthropic.com".to_string()),
        );
        (url, is_custom)
    }

    fn resolve_key_value(&self, var_name: &str) -> String {
        match &self.key_value_override {
            Some(val) => val.clone(),
            None => utils::env_var_trimmed(var_name).unwrap_or_default(),
        }
    }
}

#[async_trait]
impl HealthCheck for ApiKeyCheck {
    fn name(&self) -> &str {
        "API key"
    }

    async fn run(&self) -> CheckResult {
        let key_var = self.resolve_key_var();

        let Some(var_name) = key_var else {
            return CheckResult {
                name: self.name().to_string(),
                status: CheckStatus::Fail,
                message: "not set".to_string(),
                fix: Some(
                    "Set ANTHROPIC_API_KEY or ANTHROPIC_AUTH_TOKEN in your shell profile"
                        .to_string(),
                ),
            };
        };

        // Attempt lightweight credit-free API validation
        let (base_url, is_custom) = self.resolve_base_url();
        let url = format!("{}/v1/messages", base_url);

        // Build auth header based on which env var is set (same logic as AnthropicAdapter)
        // NEVER read the key value into output — only use it for the HTTP header.
        let key_value = self.resolve_key_value(var_name);
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build();

        let client = match client {
            Ok(c) => c,
            Err(_) => {
                return CheckResult {
                    name: self.name().to_string(),
                    status: CheckStatus::Warning,
                    message: "key set but validation failed (HTTP client error)".to_string(),
                    fix: Some(format!("Key found in {}.", var_name)),
                };
            }
        };

        let mut req = client
            .post(&url)
            .header("content-type", "application/json")
            .header("anthropic-version", "2023-06-01")
            .body(r#"{"model":"x","max_tokens":1,"messages":[]}"#);

        // Use correct auth header per AuthMode
        if var_name == "ANTHROPIC_AUTH_TOKEN" {
            req = req.header("authorization", format!("Bearer {}", key_value));
        } else {
            req = req.header("x-api-key", &key_value);
        }

        match req.send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                if status == 401 || status == 403 {
                    let fix_msg = if is_custom {
                        format!("Check your API key with your provider ({})", base_url)
                    } else {
                        "Check your API key at https://console.anthropic.com/".to_string()
                    };
                    CheckResult {
                        name: self.name().to_string(),
                        status: CheckStatus::Fail,
                        message: "invalid key".to_string(),
                        fix: Some(fix_msg),
                    }
                } else if status >= 500 {
                    // 5xx — server error, key validity unknown
                    CheckResult {
                        name: self.name().to_string(),
                        status: CheckStatus::Warning,
                        message: format!(
                            "API server error (HTTP {}). Key found in {}.",
                            status, var_name
                        ),
                        fix: Some(
                            "API server may be temporarily unavailable. Try again later."
                                .to_string(),
                        ),
                    }
                } else {
                    // 200, 400, 422, 429, etc. — auth accepted, request rejected = key valid
                    CheckResult {
                        name: self.name().to_string(),
                        status: CheckStatus::Pass,
                        message: format!("valid (via {})", var_name),
                        fix: None,
                    }
                }
            }
            Err(_) => CheckResult {
                name: self.name().to_string(),
                status: CheckStatus::Warning,
                message: "key set but validation failed (network error)".to_string(),
                fix: Some(format!(
                    "Check your internet connection. Key found in {}.",
                    var_name
                )),
            },
        }
    }
}

/// Report which API endpoint is configured.
pub struct ApiEndpointCheck {
    /// Override for testing (bypasses env var read).
    pub base_url_override: Option<Option<String>>,
}

impl ApiEndpointCheck {
    /// Resolve the base URL: use override if provided, else read env var.
    fn resolve_base_url(&self) -> Option<String> {
        match &self.base_url_override {
            Some(val) => val.clone(),
            None => utils::env_var_trimmed("ANTHROPIC_BASE_URL"),
        }
        .map(|s| utils::normalize_base_url(&s))
    }
}

#[async_trait]
impl HealthCheck for ApiEndpointCheck {
    fn name(&self) -> &str {
        "API endpoint"
    }

    async fn run(&self) -> CheckResult {
        match self.resolve_base_url() {
            Some(url) => CheckResult {
                name: self.name().to_string(),
                status: CheckStatus::Pass,
                message: format!("{} (custom)", url),
                fix: None,
            },
            None => CheckResult {
                name: self.name().to_string(),
                status: CheckStatus::Pass,
                message: "https://api.anthropic.com (default)".to_string(),
                fix: None,
            },
        }
    }
}

// ──────────────────────────────────────────────────────────────────
// Task 4: Configuration file checks
// ──────────────────────────────────────────────────────────────────

/// Check global config.toml exists and is valid TOML.
pub struct GlobalConfigCheck {
    pub config_dir: Option<std::path::PathBuf>,
}

#[async_trait]
impl HealthCheck for GlobalConfigCheck {
    fn name(&self) -> &str {
        "Global config"
    }

    async fn run(&self) -> CheckResult {
        let config_path = match &self.config_dir {
            Some(dir) => dir.join("config.toml"),
            None => match paths::config_dir() {
                Ok(dir) => dir.join("config.toml"),
                Err(_) => {
                    return CheckResult {
                        name: self.name().to_string(),
                        status: CheckStatus::Fail,
                        message: "cannot determine config directory".to_string(),
                        fix: Some("Ensure $HOME is set".to_string()),
                    };
                }
            },
        };

        if !config_path.exists() {
            return CheckResult {
                name: self.name().to_string(),
                status: CheckStatus::Fail,
                message: format!("missing ({})", config_path.display()),
                fix: Some("Run 'rustain init' to create initial configuration".to_string()),
            };
        }

        match std::fs::read_to_string(&config_path) {
            Ok(content) => {
                // First check if it's valid TOML at all
                match content.parse::<toml::Table>() {
                    Ok(_) => {
                        // Valid TOML syntax — now check if it matches AppConfig schema
                        match toml::from_str::<crate::domain::models::AppConfig>(&content) {
                            Ok(_) => CheckResult {
                                name: self.name().to_string(),
                                status: CheckStatus::Pass,
                                message: format!("{}", config_path.display()),
                                fix: None,
                            },
                            Err(_) => CheckResult {
                                name: self.name().to_string(),
                                status: CheckStatus::Fail,
                                message: format!(
                                    "invalid config format ({})",
                                    config_path.display()
                                ),
                                fix: Some("Run 'rustain init' to regenerate config".to_string()),
                            },
                        }
                    }
                    Err(_) => CheckResult {
                        name: self.name().to_string(),
                        status: CheckStatus::Fail,
                        message: format!("invalid TOML syntax ({})", config_path.display()),
                        fix: Some("Run 'rustain init' to regenerate config".to_string()),
                    },
                }
            }
            Err(_) => CheckResult {
                name: self.name().to_string(),
                status: CheckStatus::Fail,
                message: format!("cannot read ({})", config_path.display()),
                fix: Some("Check file permissions".to_string()),
            },
        }
    }
}

/// Check workspace .claude/settings.json exists and is valid JSON.
pub struct WorkspaceConfigCheck {
    pub workspace: Option<std::path::PathBuf>,
}

#[async_trait]
impl HealthCheck for WorkspaceConfigCheck {
    fn name(&self) -> &str {
        "Workspace config"
    }

    async fn run(&self) -> CheckResult {
        let workspace = match &self.workspace {
            Some(w) => w.clone(),
            None => match paths::workspace_dir() {
                Ok(w) => w,
                Err(_) => {
                    return CheckResult {
                        name: self.name().to_string(),
                        status: CheckStatus::Warning,
                        message: "cannot determine workspace directory".to_string(),
                        fix: None,
                    };
                }
            },
        };

        let settings_path = workspace.join(".claude").join("settings.json");

        if !settings_path.exists() {
            return CheckResult {
                name: self.name().to_string(),
                status: CheckStatus::Warning,
                message: format!("missing ({})", settings_path.display()),
                fix: Some("Run 'rustain init' in this workspace".to_string()),
            };
        }

        match std::fs::read_to_string(&settings_path) {
            Ok(content) => match serde_json::from_str::<serde_json::Value>(&content) {
                Ok(val) => {
                    if val.get("permissions").is_some_and(|v| !v.is_null()) {
                        CheckResult {
                            name: self.name().to_string(),
                            status: CheckStatus::Pass,
                            message: format!("{}", settings_path.display()),
                            fix: None,
                        }
                    } else {
                        CheckResult {
                            name: self.name().to_string(),
                            status: CheckStatus::Warning,
                            message: format!(
                                "missing 'permissions' key ({})",
                                settings_path.display()
                            ),
                            fix: Some(
                                "Run 'rustain init' to regenerate workspace config".to_string(),
                            ),
                        }
                    }
                }
                Err(_) => CheckResult {
                    name: self.name().to_string(),
                    status: CheckStatus::Fail,
                    message: format!("invalid JSON ({})", settings_path.display()),
                    fix: Some("Delete and run 'rustain init' to regenerate".to_string()),
                },
            },
            Err(_) => CheckResult {
                name: self.name().to_string(),
                status: CheckStatus::Fail,
                message: format!("cannot read ({})", settings_path.display()),
                fix: Some("Check file permissions".to_string()),
            },
        }
    }
}

/// Check that {workspace}/.claude/ directory exists (AC3 / Task 4.3).
pub struct WorkspaceDirCheck {
    pub workspace: Option<std::path::PathBuf>,
}

#[async_trait]
impl HealthCheck for WorkspaceDirCheck {
    fn name(&self) -> &str {
        "Workspace dir"
    }

    async fn run(&self) -> CheckResult {
        let workspace = match &self.workspace {
            Some(w) => w.clone(),
            None => match paths::workspace_dir() {
                Ok(w) => w,
                Err(_) => {
                    return CheckResult {
                        name: self.name().to_string(),
                        status: CheckStatus::Warning,
                        message: "cannot determine workspace directory".to_string(),
                        fix: None,
                    };
                }
            },
        };

        let claude_dir = workspace.join(".claude");
        if claude_dir.is_dir() {
            CheckResult {
                name: self.name().to_string(),
                status: CheckStatus::Pass,
                message: format!("{}", claude_dir.display()),
                fix: None,
            }
        } else {
            CheckResult {
                name: self.name().to_string(),
                status: CheckStatus::Warning,
                message: format!("missing ({})", claude_dir.display()),
                fix: Some("Run 'rustain init' to create workspace structure".to_string()),
            }
        }
    }
}

// ──────────────────────────────────────────────────────────────────
// Task 5: Terminal capability checks
// ──────────────────────────────────────────────────────────────────

/// Basic terminal info (always runs).
struct TerminalCheck;

#[async_trait]
impl HealthCheck for TerminalCheck {
    fn name(&self) -> &str {
        "Terminal"
    }

    async fn run(&self) -> CheckResult {
        let emulator = utils::env_var_trimmed("TERM_PROGRAM").unwrap_or_else(|| {
            utils::env_var_trimmed("TERM").unwrap_or_else(|| "unknown".to_string())
        });

        let color = terminal_info::detect_color_capability();

        let size = match crossterm::terminal::size() {
            Ok((cols, rows)) => format!("{}x{}", cols, rows),
            Err(_) => "unknown".to_string(),
        };

        let multiplexer = if utils::env_var_is_set("TMUX") {
            Some("tmux")
        } else if utils::env_var_is_set("STY") {
            Some("screen")
        } else {
            None
        };

        let (message, fix) = match multiplexer {
            Some(mux) => (
                format!("{}, {}, {} ({} detected)", emulator, color, size, mux),
                Some(format!(
                    "{} prefix key may conflict with rustain chord keys. Run 'rustain doctor --terminal' for details.",
                    mux
                )),
            ),
            None => (format!("{}, {}, {}", emulator, color, size), None),
        };

        CheckResult {
            name: self.name().to_string(),
            status: CheckStatus::Pass,
            message,
            fix,
        }
    }
}

/// Detailed terminal diagnostics (only with --terminal flag).
struct TerminalDetailCheck;

#[async_trait]
impl HealthCheck for TerminalDetailCheck {
    fn name(&self) -> &str {
        "Terminal details"
    }

    async fn run(&self) -> CheckResult {
        let mut details = Vec::new();

        // Key detection: tmux prefix conflict
        if utils::env_var_is_set("TMUX") {
            details.push("tmux detected: Ctrl+B prefix may conflict with chord keys".to_string());
        }

        // Unicode support
        let has_utf8 = utils::env_var_trimmed("LANG")
            .or_else(|| utils::env_var_trimmed("LC_ALL"))
            .or_else(|| utils::env_var_trimmed("LC_CTYPE"))
            .map(|v| v.to_lowercase().contains("utf-8") || v.to_lowercase().contains("utf8"))
            .unwrap_or(false);
        details.push(format!(
            "Unicode: {}",
            if has_utf8 {
                "UTF-8 locale"
            } else {
                "non-UTF-8 (may affect rendering)"
            }
        ));

        // Color env vars
        let colorterm = utils::env_var_trimmed("COLORTERM").unwrap_or_else(|| "unset".to_string());
        let term = utils::env_var_trimmed("TERM").unwrap_or_else(|| "unset".to_string());
        let no_color = if utils::env_var_is_set("NO_COLOR") {
            "set"
        } else {
            "unset"
        };
        details.push(format!(
            "COLORTERM={}, TERM={}, NO_COLOR={}",
            colorterm, term, no_color
        ));

        // Clipboard support heuristic (OSC 52)
        let term_program = utils::env_var_trimmed("TERM_PROGRAM").unwrap_or_default();
        let osc52_terminals = ["alacritty", "kitty", "iTerm2", "foot", "wezterm"];
        let clipboard = if osc52_terminals
            .iter()
            .any(|t| term_program.eq_ignore_ascii_case(t))
        {
            "OSC 52 likely supported"
        } else {
            "OSC 52 support unknown"
        };
        details.push(format!("Clipboard: {}", clipboard));

        // SSH detection
        let is_ssh = utils::env_var_is_set("SSH_CLIENT") || utils::env_var_is_set("SSH_TTY");
        if is_ssh {
            details.push("SSH session detected".to_string());
        }

        CheckResult {
            name: self.name().to_string(),
            status: CheckStatus::Pass,
            message: details.join("; "),
            fix: None,
        }
    }
}

// ──────────────────────────────────────────────────────────────────
// Task 6: Session storage checks
// ──────────────────────────────────────────────────────────────────

/// Check session storage directory, count sessions, detect corruption.
pub struct SessionStorageCheck {
    pub workspace: Option<std::path::PathBuf>,
    /// Override config dir for testability (to check if init has been run).
    pub config_dir: Option<std::path::PathBuf>,
}

#[async_trait]
impl HealthCheck for SessionStorageCheck {
    fn name(&self) -> &str {
        "Sessions"
    }

    async fn run(&self) -> CheckResult {
        let workspace = match &self.workspace {
            Some(w) => w.clone(),
            None => match paths::workspace_dir() {
                Ok(w) => w,
                Err(_) => {
                    return CheckResult {
                        name: self.name().to_string(),
                        status: CheckStatus::Warning,
                        message: "cannot determine workspace directory".to_string(),
                        fix: None,
                    };
                }
            },
        };

        let sessions_dir = paths::sessions_dir(&workspace);

        if !sessions_dir.exists() {
            // Check if init has been run (config.toml exists)
            let init_run = match &self.config_dir {
                Some(dir) => dir.join("config.toml").exists(),
                None => paths::config_dir()
                    .map(|d| d.join("config.toml").exists())
                    .unwrap_or(false),
            };

            return if init_run {
                CheckResult {
                    name: self.name().to_string(),
                    status: CheckStatus::Fail,
                    message: "session directory missing".to_string(),
                    fix: Some("Run 'rustain init' to create session storage".to_string()),
                }
            } else {
                CheckResult {
                    name: self.name().to_string(),
                    status: CheckStatus::Pass,
                    message: "not initialized".to_string(),
                    fix: None,
                }
            };
        }

        let mut session_count: usize = 0;
        let mut total_size: u64 = 0;
        let mut corrupted: usize = 0;

        match std::fs::read_dir(&sessions_dir) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.ends_with(".meta.json"))
                    {
                        session_count += 1;
                        if let Ok(meta) = std::fs::metadata(&path) {
                            total_size += meta.len();
                        }
                        // Check valid JSON structure (not full deserialization)
                        if let Ok(content) = std::fs::read_to_string(&path) {
                            if serde_json::from_str::<serde_json::Value>(&content).is_err() {
                                corrupted += 1;
                            }
                        } else {
                            corrupted += 1;
                        }
                    }
                }
            }
            Err(_) => {
                return CheckResult {
                    name: self.name().to_string(),
                    status: CheckStatus::Fail,
                    message: "cannot read session directory".to_string(),
                    fix: Some("Check directory permissions".to_string()),
                };
            }
        }

        let size_display = if total_size >= 1_048_576 {
            format!("{:.1} MB", total_size as f64 / 1_048_576.0)
        } else if total_size >= 1024 {
            format!("{:.1} KB", total_size as f64 / 1024.0)
        } else {
            format!("{} B", total_size)
        };

        if session_count == 0 {
            return CheckResult {
                name: self.name().to_string(),
                status: CheckStatus::Pass,
                message: "empty (no sessions yet)".to_string(),
                fix: None,
            };
        }

        if corrupted > 0 {
            CheckResult {
                name: self.name().to_string(),
                status: CheckStatus::Warning,
                message: format!(
                    "{} saved, {} corrupted ({})",
                    session_count, corrupted, size_display
                ),
                fix: Some("Remove corrupted session files from .claude/sessions/".to_string()),
            }
        } else {
            CheckResult {
                name: self.name().to_string(),
                status: CheckStatus::Pass,
                message: format!("{} saved ({})", session_count, size_display),
                fix: None,
            }
        }
    }
}

// ──────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── CheckResult formatting tests (Task 8.2, 8.3) ──

    #[test]
    fn test_display_results_pass() {
        let results = vec![CheckResult {
            name: "Test".to_string(),
            status: CheckStatus::Pass,
            message: "ok".to_string(),
            fix: None,
        }];
        // Should not panic
        display_results(&results);
    }

    #[test]
    fn test_display_results_fail_with_fix() {
        let results = vec![CheckResult {
            name: "Test".to_string(),
            status: CheckStatus::Fail,
            message: "bad".to_string(),
            fix: Some("fix it".to_string()),
        }];
        display_results(&results);
    }

    #[test]
    fn test_display_results_warning_with_note() {
        let results = vec![CheckResult {
            name: "Test".to_string(),
            status: CheckStatus::Warning,
            message: "hmm".to_string(),
            fix: Some("note this".to_string()),
        }];
        display_results(&results);
    }

    #[test]
    fn test_summary_counting() {
        let results = [
            CheckResult {
                name: "A".to_string(),
                status: CheckStatus::Pass,
                message: String::new(),
                fix: None,
            },
            CheckResult {
                name: "B".to_string(),
                status: CheckStatus::Pass,
                message: String::new(),
                fix: None,
            },
            CheckResult {
                name: "C".to_string(),
                status: CheckStatus::Warning,
                message: String::new(),
                fix: None,
            },
            CheckResult {
                name: "D".to_string(),
                status: CheckStatus::Fail,
                message: String::new(),
                fix: None,
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
    async fn test_global_config_valid_toml_wrong_schema() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config_dir = tmp.path().to_path_buf();
        // Valid TOML but with unknown section — deny_unknown_fields rejects it.
        std::fs::write(config_dir.join("config.toml"), "[section]\nkey = \"value\"").unwrap();

        let check = GlobalConfigCheck {
            config_dir: Some(config_dir),
        };
        let result = check.run().await;
        assert_eq!(result.status, CheckStatus::Fail);
        assert!(result.message.contains("invalid config format"));
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
        let checks = build_check_list(false);
        assert!(checks.len() >= 7, "Should have at least 7 checks");
        // Verify names
        let names: Vec<&str> = checks.iter().map(|c| c.name()).collect();
        assert!(names.contains(&"API key"));
        assert!(names.contains(&"API endpoint"));
        assert!(names.contains(&"Global config"));
        assert!(names.contains(&"Workspace dir"));
        assert!(names.contains(&"Workspace config"));
        assert!(names.contains(&"Terminal"));
        assert!(names.contains(&"Sessions"));
    }

    #[test]
    fn test_build_check_list_with_terminal_detail() {
        let checks_without = build_check_list(false);
        let checks_with = build_check_list(true);
        assert_eq!(
            checks_with.len(),
            checks_without.len() + 1,
            "Terminal detail flag should add one check"
        );
        let names: Vec<&str> = checks_with.iter().map(|c| c.name()).collect();
        assert!(names.contains(&"Terminal details"));
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
