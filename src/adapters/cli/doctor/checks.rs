//! Health check implementations for `rustain doctor`.
//!
//! Each struct implements `HealthCheck` and is appended to `build_check_list` in `mod.rs`.
//! New checks are added by creating a struct here and appending it to the list — no
//! modification to existing check code required.

use async_trait::async_trait;

use crate::infrastructure::{paths, permission_rules, terminal_info, utils};

use super::{CheckResult, CheckStatus, CheckTier, HealthCheck};

// ──────────────────────────────────────────────────────────────────
// API key presence check (de-billed in Story 13.2 AC8b — key presence only, no network).
// Reachability/auth validation moved to ProviderConnectivityCheck (AC8).
// ──────────────────────────────────────────────────────────────────

/// Check that an API key env var is set (key-presence only, NO network).
/// Reachability + auth validation delegated to `ProviderConnectivityCheck` (AC8).
pub struct ApiKeyCheck {
    /// Override for testing: Some(Some("VAR_NAME")) = key found, Some(None) = no key, None = read env.
    pub key_var_override: Option<Option<&'static str>>,
}

impl ApiKeyCheck {
    fn resolve_key_var(&self) -> Option<&'static str> {
        match &self.key_var_override {
            Some(val) => *val,
            None => crate::adapters::cli::init::find_api_key_var(),
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
                category: "api".to_string(),
                status: CheckStatus::Fail,
                message: "not set".to_string(),
                fix: Some(
                    "Set ANTHROPIC_API_KEY or ANTHROPIC_AUTH_TOKEN in your shell profile"
                        .to_string(),
                ),
                latency: None,
                tier: CheckTier::ExitAffecting,
            };
        };

        // Key-presence only (AC8b de-bill): no network call.
        // Auth validation is now handled by ProviderConnectivityCheck (AC8).
        CheckResult {
            name: self.name().to_string(),
            category: "api".to_string(),
            status: CheckStatus::Pass,
            message: format!("set (via {})", var_name),
            fix: None,
            latency: None,
            tier: CheckTier::ExitAffecting,
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
                category: "api".to_string(),
                status: CheckStatus::Pass,
                message: format!("{} (custom)", url),
                fix: None,
                latency: None,
                tier: CheckTier::ExitAffecting,
            },
            None => CheckResult {
                name: self.name().to_string(),
                category: "api".to_string(),
                status: CheckStatus::Pass,
                message: "https://api.anthropic.com (default)".to_string(),
                fix: None,
                latency: None,
                tier: CheckTier::ExitAffecting,
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
                        category: "config".to_string(),
                        status: CheckStatus::Fail,
                        message: "cannot determine config directory".to_string(),
                        fix: Some("Ensure $HOME is set".to_string()),
                        latency: None,
                        tier: CheckTier::ExitAffecting,
                    };
                }
            },
        };

        if !config_path.exists() {
            return CheckResult {
                name: self.name().to_string(),
                category: "config".to_string(),
                status: CheckStatus::Fail,
                message: format!("missing ({})", config_path.display()),
                fix: Some("Run 'rustain init' to create initial configuration".to_string()),
                latency: None,
                tier: CheckTier::ExitAffecting,
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
                                category: "config".to_string(),
                                status: CheckStatus::Pass,
                                message: format!("{}", config_path.display()),
                                fix: None,
                                latency: None,
                                tier: CheckTier::ExitAffecting,
                            },
                            Err(_) => CheckResult {
                                name: self.name().to_string(),
                                category: "config".to_string(),
                                status: CheckStatus::Fail,
                                message: format!(
                                    "invalid config format ({})",
                                    config_path.display()
                                ),
                                fix: Some("Run 'rustain init' to regenerate config".to_string()),
                                latency: None,
                                tier: CheckTier::ExitAffecting,
                            },
                        }
                    }
                    Err(_) => CheckResult {
                        name: self.name().to_string(),
                        category: "config".to_string(),
                        status: CheckStatus::Fail,
                        message: format!("invalid TOML syntax ({})", config_path.display()),
                        fix: Some("Run 'rustain init' to regenerate config".to_string()),
                        latency: None,
                        tier: CheckTier::ExitAffecting,
                    },
                }
            }
            Err(_) => CheckResult {
                name: self.name().to_string(),
                category: "config".to_string(),
                status: CheckStatus::Fail,
                message: format!("cannot read ({})", config_path.display()),
                fix: Some("Check file permissions".to_string()),
                latency: None,
                tier: CheckTier::ExitAffecting,
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
                        category: "config".to_string(),
                        status: CheckStatus::Warning,
                        message: "cannot determine workspace directory".to_string(),
                        fix: None,
                        latency: None,
                        tier: CheckTier::ExitAffecting,
                    };
                }
            },
        };

        let settings_path = workspace.join(".claude").join("settings.json");

        if !settings_path.exists() {
            return CheckResult {
                name: self.name().to_string(),
                category: "config".to_string(),
                status: CheckStatus::Warning,
                message: format!("missing ({})", settings_path.display()),
                fix: Some("Run 'rustain init' in this workspace".to_string()),
                latency: None,
                tier: CheckTier::ExitAffecting,
            };
        }

        match std::fs::read_to_string(&settings_path) {
            Ok(content) => match serde_json::from_str::<serde_json::Value>(&content) {
                Ok(val) => {
                    if val.get("permissions").is_some_and(|v| !v.is_null()) {
                        CheckResult {
                            name: self.name().to_string(),
                            category: "config".to_string(),
                            status: CheckStatus::Pass,
                            message: format!("{}", settings_path.display()),
                            fix: None,
                            latency: None,
                            tier: CheckTier::ExitAffecting,
                        }
                    } else {
                        CheckResult {
                            name: self.name().to_string(),
                            category: "config".to_string(),
                            status: CheckStatus::Warning,
                            message: format!(
                                "missing 'permissions' key ({})",
                                settings_path.display()
                            ),
                            fix: Some(
                                "Run 'rustain init' to regenerate workspace config".to_string(),
                            ),
                            latency: None,
                            tier: CheckTier::ExitAffecting,
                        }
                    }
                }
                Err(_) => CheckResult {
                    name: self.name().to_string(),
                    category: "config".to_string(),
                    status: CheckStatus::Fail,
                    message: format!("invalid JSON ({})", settings_path.display()),
                    fix: Some("Delete and run 'rustain init' to regenerate".to_string()),
                    latency: None,
                    tier: CheckTier::ExitAffecting,
                },
            },
            Err(_) => CheckResult {
                name: self.name().to_string(),
                category: "config".to_string(),
                status: CheckStatus::Fail,
                message: format!("cannot read ({})", settings_path.display()),
                fix: Some("Check file permissions".to_string()),
                latency: None,
                tier: CheckTier::ExitAffecting,
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
                        category: "config".to_string(),
                        status: CheckStatus::Warning,
                        message: "cannot determine workspace directory".to_string(),
                        fix: None,
                        latency: None,
                        tier: CheckTier::ExitAffecting,
                    };
                }
            },
        };

        let claude_dir = workspace.join(".claude");
        if claude_dir.is_dir() {
            CheckResult {
                name: self.name().to_string(),
                category: "config".to_string(),
                status: CheckStatus::Pass,
                message: format!("{}", claude_dir.display()),
                fix: None,
                latency: None,
                tier: CheckTier::ExitAffecting,
            }
        } else {
            CheckResult {
                name: self.name().to_string(),
                category: "config".to_string(),
                status: CheckStatus::Warning,
                message: format!("missing ({})", claude_dir.display()),
                fix: Some("Run 'rustain init' to create workspace structure".to_string()),
                latency: None,
                tier: CheckTier::ExitAffecting,
            }
        }
    }
}

// ──────────────────────────────────────────────────────────────────
// Task 5: Terminal capability checks
// ──────────────────────────────────────────────────────────────────

/// Basic terminal info (always runs).
pub(super) struct TerminalCheck;

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
            category: "terminal".to_string(),
            status: CheckStatus::Pass,
            message,
            fix,
            latency: None,
            tier: CheckTier::ExitAffecting,
        }
    }
}

/// Detailed terminal diagnostics (only with --terminal flag).
pub(super) struct TerminalDetailCheck;

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
            category: "terminal".to_string(),
            status: CheckStatus::Pass,
            message: details.join("; "),
            fix: None,
            latency: None,
            tier: CheckTier::ExitAffecting,
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
                        category: "session".to_string(),
                        status: CheckStatus::Warning,
                        message: "cannot determine workspace directory".to_string(),
                        fix: None,
                        latency: None,
                        tier: CheckTier::ExitAffecting,
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
                    category: "session".to_string(),
                    status: CheckStatus::Fail,
                    message: "session directory missing".to_string(),
                    fix: Some("Run 'rustain init' to create session storage".to_string()),
                    latency: None,
                    tier: CheckTier::ExitAffecting,
                }
            } else {
                CheckResult {
                    name: self.name().to_string(),
                    category: "session".to_string(),
                    status: CheckStatus::Pass,
                    message: "not initialized".to_string(),
                    fix: None,
                    latency: None,
                    tier: CheckTier::ExitAffecting,
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
                    category: "session".to_string(),
                    status: CheckStatus::Fail,
                    message: "cannot read session directory".to_string(),
                    fix: Some("Check directory permissions".to_string()),
                    latency: None,
                    tier: CheckTier::ExitAffecting,
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
                category: "session".to_string(),
                status: CheckStatus::Pass,
                message: "empty (no sessions yet)".to_string(),
                fix: None,
                latency: None,
                tier: CheckTier::ExitAffecting,
            };
        }

        if corrupted > 0 {
            CheckResult {
                name: self.name().to_string(),
                category: "session".to_string(),
                status: CheckStatus::Warning,
                message: format!(
                    "{} saved, {} corrupted ({})",
                    session_count, corrupted, size_display
                ),
                fix: Some("Remove corrupted session files from .claude/sessions/".to_string()),
                latency: None,
                tier: CheckTier::ExitAffecting,
            }
        } else {
            CheckResult {
                name: self.name().to_string(),
                category: "session".to_string(),
                status: CheckStatus::Pass,
                message: format!("{} saved ({})", session_count, size_display),
                fix: None,
                latency: None,
                tier: CheckTier::ExitAffecting,
            }
        }
    }
}

// ──────────────────────────────────────────────────────────────────
// ──────────────────────────────────────────────────────────────────
// Permission rules check (Story 6-0c)
// ──────────────────────────────────────────────────────────────────

pub struct PermissionRulesCheck {
    pub workspace: Option<std::path::PathBuf>,
}

#[async_trait]
impl HealthCheck for PermissionRulesCheck {
    fn name(&self) -> &str {
        "Permission rules"
    }

    async fn run(&self) -> CheckResult {
        let workspace = match &self.workspace {
            Some(w) => w.clone(),
            None => match paths::workspace_dir() {
                Ok(w) => w,
                Err(_) => {
                    return CheckResult {
                        name: self.name().to_string(),
                        category: "permissions".to_string(),
                        status: CheckStatus::Warning,
                        message: "cannot determine workspace directory".to_string(),
                        fix: None,
                        latency: None,
                        tier: CheckTier::ExitAffecting,
                    };
                }
            },
        };

        let user_config = match paths::config_dir() {
            Ok(d) => d.join("config.toml"),
            Err(_) => {
                return CheckResult {
                    name: self.name().to_string(),
                    category: "permissions".to_string(),
                    status: CheckStatus::Warning,
                    message: "cannot determine config directory".to_string(),
                    fix: None,
                    latency: None,
                    tier: CheckTier::ExitAffecting,
                };
            }
        };
        let workspace_rules = workspace.join(".rustain").join("permissions.toml");

        match permission_rules::load_rules(&user_config, &workspace_rules) {
            Ok(ruleset) => {
                if ruleset.has_catchall() {
                    CheckResult {
                        name: self.name().to_string(),
                        category: "permissions".to_string(),
                        status: CheckStatus::Pass,
                        message: "catch-all rule present".to_string(),
                        fix: None,
                        latency: None,
                        tier: CheckTier::ExitAffecting,
                    }
                } else {
                    CheckResult {
                        name: self.name().to_string(),
                        category: "permissions".to_string(),
                        status: CheckStatus::Warning,
                        message: "no catch-all rule in permissions.toml".to_string(),
                        fix: Some(format!(
                            r#"Add a catch-all [[rules]] pattern = "*" action = "ask" scope = "tool" to {}"#,
                            workspace_rules.display()
                        )),
                        latency: None,
                        tier: CheckTier::ExitAffecting,
                    }
                }
            }
            Err(_) => CheckResult {
                name: self.name().to_string(),
                category: "permissions".to_string(),
                status: CheckStatus::Warning,
                message: "failed to load permission rules".to_string(),
                fix: Some(
                    "Check ~/.rustain/config.toml and workspace/.rustain/permissions.toml"
                        .to_string(),
                ),
                latency: None,
                tier: CheckTier::ExitAffecting,
            },
        }
    }
}

pub struct PlanDirCheck {
    pub workspace: Option<std::path::PathBuf>,
}

#[async_trait]
impl HealthCheck for PlanDirCheck {
    fn name(&self) -> &str {
        "Plan directory"
    }

    async fn run(&self) -> CheckResult {
        let workspace = match &self.workspace {
            Some(w) => w.clone(),
            None => match paths::workspace_dir() {
                Ok(w) => w,
                Err(_) => {
                    return CheckResult {
                        name: self.name().to_string(),
                        category: "plans".to_string(),
                        status: CheckStatus::Warning,
                        message: "cannot determine workspace directory".to_string(),
                        fix: None,
                        latency: None,
                        tier: CheckTier::ExitAffecting,
                    };
                }
            },
        };

        let plans_dir = workspace.join(".rustain").join("plans");
        match std::fs::create_dir_all(&plans_dir) {
            Ok(()) => CheckResult {
                name: self.name().to_string(),
                category: "plans".to_string(),
                status: CheckStatus::Pass,
                message: format!("Plan directory writable: {}", plans_dir.display()),
                fix: None,
                latency: None,
                tier: CheckTier::ExitAffecting,
            },
            Err(e) => CheckResult {
                name: self.name().to_string(),
                category: "plans".to_string(),
                status: CheckStatus::Warning,
                message: format!("Cannot create plan directory: {}", e),
                fix: Some(format!("Ensure {} is writable", plans_dir.display())),
                latency: None,
                tier: CheckTier::ExitAffecting,
            },
        }
    }
}

// ──────────────────────────────────────────────────────────────────
// Story 11.1: Memory-directory size report (AC7 — awareness only)
// ──────────────────────────────────────────────────────────────────

/// Report the total on-disk size of `{workspace}/.rustain/memory/`.
///
/// Awareness-only (AC7): daily logs are kept indefinitely — no auto-deletion.
/// A missing directory is informational ("no memory yet"), NOT a failure. This
/// check NEVER returns `Fail`.
pub struct MemoryDirSizeCheck {
    pub workspace: Option<std::path::PathBuf>,
}

#[async_trait]
impl HealthCheck for MemoryDirSizeCheck {
    fn name(&self) -> &str {
        "Memory dir"
    }

    async fn run(&self) -> CheckResult {
        let workspace = match &self.workspace {
            Some(w) => w.clone(),
            None => match paths::workspace_dir() {
                Ok(w) => w,
                Err(_) => {
                    return CheckResult {
                        name: self.name().to_string(),
                        category: "memory".to_string(),
                        status: CheckStatus::Warning,
                        message: "cannot determine workspace directory".to_string(),
                        fix: None,
                        latency: None,
                        tier: CheckTier::ExitAffecting,
                    };
                }
            },
        };

        let rustain_dir = workspace.join(".rustain");
        let memory_dir = rustain_dir.join("memory");
        // Story 11.2 — the long-term curated tier is a SIBLING of memory/.
        let memory_md = rustain_dir.join("MEMORY.md");

        let mut total: u64 = 0;
        let mut file_count: usize = 0;
        // Story 11.3a — the vector index (`index.bin`) lives in memory/ but is
        // NOT a day file; its size is reported in the total but attributed
        // separately so the day-file count stays accurate.
        let mut index_bin_size: u64 = 0;
        // Daily-log day files (Story 11.1). A missing dir is fine — memory may
        // simply not have been written yet; MEMORY.md may still exist below.
        if let Ok(entries) = std::fs::read_dir(&memory_dir) {
            for entry in entries.flatten() {
                let ft = match entry.file_type() {
                    Ok(ft) => ft,
                    Err(_) => continue,
                };
                if ft.is_file() && !ft.is_symlink() {
                    if let Ok(meta) = entry.metadata() {
                        total += meta.len();
                        if entry.file_name() == "index.bin" {
                            index_bin_size = meta.len();
                        } else {
                            file_count += 1;
                        }
                    }
                }
            }
        }

        // Long-term MEMORY.md (Story 11.2). `symlink_metadata` does NOT follow
        // links, so `is_file()` is true only for a real regular file — skipping
        // symlinks, mirroring the day-file loop above.
        let mut memory_md_size: u64 = 0;
        if let Ok(meta) = std::fs::symlink_metadata(&memory_md) {
            if meta.is_file() {
                memory_md_size = meta.len();
                total += memory_md_size;
            }
        }

        // Missing all is fine — memory simply hasn't been written yet (AC7).
        if file_count == 0 && memory_md_size == 0 && index_bin_size == 0 {
            return CheckResult {
                name: self.name().to_string(),
                category: "memory".to_string(),
                status: CheckStatus::Pass,
                message: "no memory yet".to_string(),
                fix: None,
                latency: None,
                tier: CheckTier::ExitAffecting,
            };
        }

        let size_display = if total >= 1_048_576 {
            format!("{:.1} MB", total as f64 / 1_048_576.0)
        } else if total >= 1024 {
            format!("{:.1} KB", total as f64 / 1024.0)
        } else {
            format!("{total} B")
        };

        // Awareness-only sub-note when MEMORY.md crosses the 20KB framing (AC3).
        // Still never a failure (consistent with 11.1 AC7).
        let md_note = if memory_md_size > 20 * 1024 {
            format!(
                " (MEMORY.md {:.1} KB — consider pruning)",
                memory_md_size as f64 / 1024.0
            )
        } else if memory_md_size > 0 {
            " (incl. MEMORY.md)".to_string()
        } else {
            String::new()
        };

        // Story 11.3a — attribute the vector index size (awareness-only, never a
        // failure; the index is rebuildable from memory content).
        let index_note = if index_bin_size > 0 {
            format!(
                " (incl. vector index {:.1} KB)",
                index_bin_size as f64 / 1024.0
            )
        } else {
            String::new()
        };

        CheckResult {
            name: self.name().to_string(),
            category: "memory".to_string(),
            status: CheckStatus::Pass,
            message: format!("{file_count} day file(s), {size_display}{md_note}{index_note}"),
            fix: None,
            latency: None,
            tier: CheckTier::ExitAffecting,
        }
    }
}

// ──────────────────────────────────────────────────────────────────
// Story 13.2 AC8: Non-billable provider connectivity probe
// ──────────────────────────────────────────────────────────────────

/// Non-billable provider connectivity check (Story 13.2 AC8).
///
/// Uses `connectivity_probe()` (free `GET /v1/models` or `/api/tags`) to validate
/// reachability + auth for each configured provider. Reports latency on success.
/// - 200 → Pass (latency)
/// - 401/403 → Fail (auth)
/// - transport error → Skipped (offline)
/// - 404/405 → Skipped (endpoint unsupported)
///
/// Honestly states it proves auth+reachability only, NOT that streaming/messages works.
pub struct ProviderConnectivityCheck {
    /// Display name (e.g. "Provider connectivity (anthropic)").
    pub name: String,
    /// Provider name for display.
    pub provider_name: String,
    /// The provider to probe. `None` means the provider is not configured.
    pub provider: Option<std::sync::Arc<dyn crate::domain::ports::StreamingProvider>>,
}

#[async_trait]
impl HealthCheck for ProviderConnectivityCheck {
    fn name(&self) -> &str {
        &self.name
    }

    async fn run(&self) -> CheckResult {
        let Some(ref provider) = self.provider else {
            return CheckResult {
                name: self.name.clone(),
                category: "api".to_string(),
                status: CheckStatus::Skipped("not configured".to_string()),
                message: "provider not configured".to_string(),
                fix: None,
                latency: None,
                tier: CheckTier::ExitAffecting,
            };
        };

        let start = std::time::Instant::now();
        match provider.connectivity_probe().await {
            Ok(outcome) => CheckResult {
                name: self.name.clone(),
                category: "api".to_string(),
                status: CheckStatus::Pass,
                message: format!(
                    "reachable ({}ms) — proves auth+reachability, not chat health",
                    outcome.latency.as_millis()
                ),
                fix: None,
                latency: Some(start.elapsed()),
                tier: CheckTier::ExitAffecting,
            },
            Err(crate::domain::errors::ProviderError::AuthenticationFailed) => CheckResult {
                name: self.name.clone(),
                category: "api".to_string(),
                status: CheckStatus::Fail,
                message: "authentication failed".to_string(),
                fix: Some("Check your API key or token.".to_string()),
                latency: None,
                tier: CheckTier::ExitAffecting,
            },
            Err(crate::domain::errors::ProviderError::Offline(msg)) => CheckResult {
                name: self.name.clone(),
                category: "api".to_string(),
                status: CheckStatus::Skipped("offline".to_string()),
                message: format!("skipped — offline, network probes unavailable ({})", msg),
                fix: None,
                latency: None,
                tier: CheckTier::ExitAffecting,
            },
            Err(crate::domain::errors::ProviderError::EndpointUnsupported(status)) => CheckResult {
                name: self.name.clone(),
                category: "api".to_string(),
                status: CheckStatus::Skipped("endpoint unsupported".to_string()),
                message: format!("skipped — endpoint unsupported (HTTP {})", status),
                fix: None,
                latency: None,
                tier: CheckTier::ExitAffecting,
            },
            Err(e) => CheckResult {
                name: self.name.clone(),
                category: "api".to_string(),
                status: CheckStatus::Fail,
                message: e.to_string(),
                fix: Some("Check your provider configuration.".to_string()),
                latency: None,
                tier: CheckTier::ExitAffecting,
            },
        }
    }
}

// ──────────────────────────────────────────────────────────────────
// Story 13.2 Task 5: Category-tier diagnostic checks
// ──────────────────────────────────────────────────────────────────

/// Info-tier: reports OS, arch, rustain version, terminal. Always Pass.
pub struct SystemInfoCheck;

#[async_trait]
impl HealthCheck for SystemInfoCheck {
    fn name(&self) -> &str {
        "System info"
    }

    async fn run(&self) -> CheckResult {
        let os = std::env::consts::OS;
        let arch = std::env::consts::ARCH;
        let version = env!("CARGO_PKG_VERSION");
        let terminal = utils::env_var_trimmed("TERM_PROGRAM")
            .or_else(|| utils::env_var_trimmed("TERM"))
            .unwrap_or_else(|| "unknown".to_string());

        CheckResult {
            name: self.name().to_string(),
            category: "system_info".to_string(),
            status: CheckStatus::Pass,
            message: format!(
                "{}/{}, rustain v{}, terminal: {}",
                os, arch, version, terminal
            ),
            fix: None,
            latency: None,
            tier: CheckTier::Info,
        }
    }
}

/// Resolve the active profile's Tools adapter name, or `None` if resolution fails.
/// Doctor must never fail over an informational hint, so every error → `None`.
fn resolve_active_tools_adapter(active: &str, config_dir: &std::path::Path) -> Option<String> {
    use crate::adapters::profile_resolver::toml_resolver::TomlProfileResolver;
    use crate::domain::models::PortDimension;
    use crate::domain::ports::ProfileResolver;
    TomlProfileResolver::new(active, config_dir.to_path_buf())
        .ok()?
        .resolve_active()?
        .selection
        .dimensions
        .get(&PortDimension::Tools)
        .map(|r| r.adapter.clone())
}

/// ADR-10-5 §Consequences action item — classify whether the active profile's
/// Tools adapter hosts the subagent/`task` capability, as a fragment appended to
/// the `Profiles` doctor line. Only `composite` builds a `CompositeToolsetAdapter`,
/// the sole adapter that registers `SubagentProvider` (ADR-10-2). Surfacing this
/// closes the silent-missing-feature trap for `base`/custom-profile users.
fn tools_reachability_label(tools_adapter: Option<&str>) -> String {
    match tools_adapter {
        Some("composite") => " — tools: composite (subagents/task available)".to_string(),
        Some(other) if !other.is_empty() => format!(
            " — tools: {other} (subagents/task UNAVAILABLE; set [tools] adapter = \"composite\" to enable)"
        ),
        _ => String::new(),
    }
}

/// Check profiles directory for .toml profile files.
pub struct ProfilesCheck;

#[async_trait]
impl HealthCheck for ProfilesCheck {
    fn name(&self) -> &str {
        "Profiles"
    }

    async fn run(&self) -> CheckResult {
        let config_dir = match paths::config_dir() {
            Ok(d) => d,
            Err(_) => {
                return CheckResult {
                    name: self.name().to_string(),
                    category: "profiles".to_string(),
                    status: CheckStatus::Warning,
                    message: "cannot determine config directory".to_string(),
                    fix: None,
                    latency: None,
                    tier: CheckTier::ExitAffecting,
                };
            }
        };

        let profiles_dir = config_dir.join("profiles");
        let active =
            utils::env_var_trimmed("RUSTAIN_PROFILE").unwrap_or_else(|| "coding".to_string());

        if !profiles_dir.is_dir() {
            return CheckResult {
                name: self.name().to_string(),
                category: "profiles".to_string(),
                status: CheckStatus::Warning,
                message: format!("profiles directory missing ({})", profiles_dir.display()),
                fix: Some("Run 'rustain init' to create default profiles".to_string()),
                latency: None,
                tier: CheckTier::ExitAffecting,
            };
        }

        let (count, read_err) = match std::fs::read_dir(&profiles_dir) {
            Ok(entries) => {
                let mut total = 0usize;
                let mut errors = 0usize;
                for entry in entries {
                    match entry {
                        Ok(e) => {
                            if e.path().extension().is_some_and(|ext| ext == "toml") {
                                total += 1;
                            }
                        }
                        Err(_) => errors += 1,
                    }
                }
                if total == 0 && errors > 0 {
                    return CheckResult {
                        name: self.name().to_string(),
                        category: "profiles".to_string(),
                        status: CheckStatus::Warning,
                        message: "cannot read profile entries (permission denied)".to_string(),
                        fix: Some(format!("Check permissions on {}", profiles_dir.display())),
                        latency: None,
                        tier: CheckTier::ExitAffecting,
                    };
                }
                (total, false)
            }
            Err(_) => (0, true),
        };

        if read_err {
            CheckResult {
                name: self.name().to_string(),
                category: "profiles".to_string(),
                status: CheckStatus::Warning,
                message: "cannot read profile entries (permission denied)".to_string(),
                fix: Some(format!("Check permissions on {}", profiles_dir.display())),
                latency: None,
                tier: CheckTier::ExitAffecting,
            }
        } else if count == 0 {
            CheckResult {
                name: self.name().to_string(),
                category: "profiles".to_string(),
                status: CheckStatus::Warning,
                message: format!("no profiles found in {}", profiles_dir.display()),
                fix: Some("Run 'rustain init' to create default profiles".to_string()),
                latency: None,
                tier: CheckTier::ExitAffecting,
            }
        } else {
            CheckResult {
                name: self.name().to_string(),
                category: "profiles".to_string(),
                status: CheckStatus::Pass,
                message: format!(
                    "{} profile(s), active: {}{}",
                    count,
                    active,
                    tools_reachability_label(
                        resolve_active_tools_adapter(&active, &config_dir).as_deref()
                    )
                ),
                fix: None,
                latency: None,
                tier: CheckTier::ExitAffecting,
            }
        }
    }
}

/// Info-tier: reports git availability and whether cwd is inside a repo. Always Pass.
pub struct GitCheck;

#[async_trait]
impl HealthCheck for GitCheck {
    fn name(&self) -> &str {
        "Git"
    }

    async fn run(&self) -> CheckResult {
        let output = tokio::process::Command::new("git")
            .args(["rev-parse", "--is-inside-work-tree"])
            .output()
            .await;

        let message = match output {
            Ok(o) if o.status.success() => "available, inside git repo".to_string(),
            Ok(_) => "available, not inside git repo".to_string(),
            Err(_) => "git not found on PATH".to_string(),
        };

        CheckResult {
            name: self.name().to_string(),
            category: "git".to_string(),
            status: CheckStatus::Pass,
            message,
            fix: None,
            latency: None,
            tier: CheckTier::Info,
        }
    }
}

/// Check for local skill directories in workspace.
pub struct SkillsCheck {
    pub workspace: Option<std::path::PathBuf>,
}

#[async_trait]
impl HealthCheck for SkillsCheck {
    fn name(&self) -> &str {
        "Skills"
    }

    async fn run(&self) -> CheckResult {
        let workspace = match &self.workspace {
            Some(w) => w.clone(),
            None => match paths::workspace_dir() {
                Ok(w) => w,
                Err(_) => {
                    return CheckResult {
                        name: self.name().to_string(),
                        category: "skills".to_string(),
                        status: CheckStatus::Pass,
                        message: "cannot determine workspace directory".to_string(),
                        fix: None,
                        latency: None,
                        tier: CheckTier::ExitAffecting,
                    };
                }
            },
        };

        if !workspace.is_dir() {
            return CheckResult {
                name: self.name().to_string(),
                category: "skills".to_string(),
                status: CheckStatus::Warning,
                message: "workspace is not a valid directory".to_string(),
                fix: None,
                latency: None,
                tier: CheckTier::ExitAffecting,
            };
        }

        let mut count = 0usize;
        // Scan .claude/skills/ and .rustain/skills/
        for skills_dir in [
            workspace.join(".claude").join("skills"),
            workspace.join(".rustain").join("skills"),
        ] {
            if let Ok(entries) = std::fs::read_dir(&skills_dir) {
                for entry in entries.flatten() {
                    if entry.path().is_dir() {
                        count += 1;
                    }
                }
            }
        }

        CheckResult {
            name: self.name().to_string(),
            category: "skills".to_string(),
            status: CheckStatus::Pass,
            message: format!("{} skill directory(ies) found", count),
            fix: None,
            latency: None,
            tier: CheckTier::ExitAffecting,
        }
    }
}

// ──────────────────────────────────────────────────────────────────
// Story 13.2b: MCP server reachability check (AC1-AC3a)
// Feature-gated: `mcp` (default ON).
// ──────────────────────────────────────────────────────────────────

#[cfg(feature = "mcp")]
mod mcp_check {
    use super::*;

    use crate::adapters::mcp::error::McpError;
    use crate::domain::models::McpServerSpec;

    /// Doctor-side budget for each MCP server probe.
    /// Wraps the adapter's internal 10s timeout — this fires first.
    /// NOT config (OQ-B2: one const, not a user-facing knob).
    pub const MCP_PER_SERVER_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

    /// Pure mapper: all branch logic for MCP connect outcomes.
    /// Zero I/O, exhaustively unit-testable.
    ///
    /// `res` is the result of `McpClientAdapter::connect()`.
    /// `tool_count` is from `McpClientAdapter::tool_count()` (0 if connect failed).
    pub fn map_connect_result(
        res: &Result<(), McpError>,
        tool_count: usize,
    ) -> (CheckStatus, CheckTier) {
        match res {
            Ok(()) if tool_count >= 1 => (CheckStatus::Pass, CheckTier::Info),
            Ok(()) => (CheckStatus::Info, CheckTier::Info),
            Err(McpError::Unsupported(reason)) => (
                CheckStatus::Skipped(format!("transport not supported: {reason}")),
                CheckTier::Info,
            ),
            Err(McpError::SpawnFailed(_)) => (CheckStatus::Fail, CheckTier::ExitAffecting),
            Err(McpError::HandshakeFailed(_)) => (CheckStatus::Fail, CheckTier::ExitAffecting),
            Err(McpError::ToolsListFailed(_)) => (CheckStatus::Fail, CheckTier::ExitAffecting),
            Err(McpError::ChildExited(_)) => (CheckStatus::Fail, CheckTier::ExitAffecting),
            Err(McpError::TransportClosed(_)) => (CheckStatus::Warning, CheckTier::Info),
            Err(McpError::Timeout(_)) => (CheckStatus::Warning, CheckTier::Info),
            Err(McpError::Cancelled) => (CheckStatus::Warning, CheckTier::Info),
            Err(McpError::Internal(_)) => (CheckStatus::Warning, CheckTier::Info),
            Err(McpError::CallToolFailed(_)) => (CheckStatus::Warning, CheckTier::Info),
            Err(McpError::TaskProtocol(_)) => (CheckStatus::Warning, CheckTier::Info),
            Err(McpError::TaskFailed(_)) => (CheckStatus::Warning, CheckTier::Info),
        }
    }

    /// MCP server reachability check (Story 13.2b AC1-AC3a).
    ///
    /// For each configured `McpServerSpec`, attempts a bounded reachability handshake
    /// via `McpClientAdapter::connect()` and maps the result through `map_connect_result`.
    /// Servers are probed concurrently via `JoinSet`; each adapter is task-local and
    /// dropped inside the task (teardown via `kill_on_drop`, AC2).
    pub struct McpReachabilityCheck {
        pub servers: Vec<McpServerSpec>,
        pub per_server_budget: std::time::Duration,
    }

    #[async_trait]
    impl HealthCheck for McpReachabilityCheck {
        fn name(&self) -> &str {
            "MCP server reachability"
        }

        async fn run(&self) -> CheckResult {
            // Zero servers → Skipped (not a vacuous Pass).
            if self.servers.is_empty() {
                return CheckResult {
                    name: self.name().to_string(),
                    category: "mcp".to_string(),
                    status: CheckStatus::Skipped("no MCP servers configured".to_string()),
                    message: "no MCP servers configured".to_string(),
                    fix: None,
                    latency: None,
                    tier: CheckTier::Info,
                };
            }

            use crate::adapters::mcp::client::McpClientAdapter;

            let budget = self.per_server_budget;
            let mut join_set = tokio::task::JoinSet::new();
            // P5: Bounded concurrency — cap concurrent probes to avoid resource exhaustion.
            let concurrency_limit = std::sync::Arc::new(tokio::sync::Semaphore::new(8));
            let start = std::time::Instant::now();

            for spec in &self.servers {
                let spec = spec.clone();
                let per_budget = budget;
                let server_id = spec.id.clone();
                let limit = concurrency_limit.clone();
                join_set.spawn(async move {
                    let _permit = limit.acquire().await.expect("semaphore never closed");
                    // Doctor path: adapter is NOT Arc-wrapped; self_weak is None.
                    // This means ClientHandler callbacks get a dangling weak ref.
                    // Acceptable for doctor (no callbacks needed), but fragile.
                    // P4: If callbacks become needed, wrap in Arc + call set_self_weak.
                    let adapter = McpClientAdapter::new(spec, None);
                    let res = tokio::time::timeout(per_budget, adapter.connect()).await;
                    let (connect_result, tool_count) = match res {
                        Ok(inner) => {
                            let tc = adapter.tool_count();
                            (inner, tc)
                        }
                        Err(_elapsed) => {
                            // Outer timeout fired — Info/Warning (not Fail).
                            // McpError::Timeout carries whole seconds (matches client.rs call sites).
                            // Ceiling so sub-second budgets report 1s, not 0s.
                            let secs = (per_budget.as_millis() as u64).div_ceil(1000).max(1);
                            (Err(McpError::Timeout(secs)), 0)
                        }
                    };
                    let (status, tier) = map_connect_result(&connect_result, tool_count);
                    // Adapter drops here → kill_on_drop tears down child.
                    (server_id, status, tier, connect_result, tool_count)
                });
            }

            // Collect results with an outer wall-clock ceiling.
            let wall_ceiling = budget + std::time::Duration::from_secs(2);
            let collect_result = tokio::time::timeout(wall_ceiling, async {
                let mut rows = Vec::new();
                while let Some(res) = join_set.join_next().await {
                    match res {
                        Ok(row) => rows.push(row),
                        Err(e) => {
                            // JoinError (panic/cancel) — treat as Warning.
                            // server_id is not available here; use task index or "unknown".
                            rows.push((
                                "unknown".to_string(),
                                CheckStatus::Warning,
                                CheckTier::Info,
                                Err(McpError::Internal(format!("task join error: {e}"))),
                                0,
                            ));
                        }
                    }
                }
                rows
            })
            .await;

            let rows = match collect_result {
                Ok(rows) => rows,
                Err(_wall_timeout) => {
                    // Wall-clock ceiling breached — abort remaining tasks.
                    join_set.abort_all();
                    return CheckResult {
                        name: self.name().to_string(),
                        category: "mcp".to_string(),
                        status: CheckStatus::Warning,
                        message: "MCP reachability check exceeded wall-clock budget".to_string(),
                        fix: None,
                        latency: Some(start.elapsed()),
                        tier: CheckTier::Info,
                    };
                }
            };

            // Aggregate: find the worst status + build message.
            let mut any_fail = false;
            let mut any_exit_affecting = false;
            let mut messages = Vec::with_capacity(rows.len());
            let mut fix_hints = Vec::new();

            for (server_id, status, tier, connect_result, tool_count) in &rows {
                let detail = match &status {
                    CheckStatus::Pass => format!("{server_id}: reachable ({tool_count} tool(s))"),
                    CheckStatus::Info => {
                        if *tool_count == 0 && connect_result.is_ok() {
                            format!("{server_id}: reachable, 0 tools exposed")
                        } else {
                            let reason = connect_result
                                .as_ref()
                                .err()
                                .map(|e| e.to_string())
                                .unwrap_or_else(|| "reachable, 0 tools exposed".to_string());
                            format!("{server_id}: {reason}")
                        }
                    }
                    CheckStatus::Warning => {
                        let reason = connect_result
                            .as_ref()
                            .err()
                            .map(|e| e.to_string())
                            .unwrap_or_else(|| "warning".to_string());
                        format!("{server_id}: {reason}")
                    }
                    CheckStatus::Fail => {
                        any_fail = true;
                        let reason = connect_result
                            .as_ref()
                            .err()
                            .map(|e| e.to_string())
                            .unwrap_or_default();
                        fix_hints.push(format!(
                            "{server_id}: check command/path, ensure binary exists and is executable"
                        ));
                        format!("{server_id}: FAILED — {reason}")
                    }
                    CheckStatus::Skipped(reason) => format!("{server_id}: skipped — {reason}"),
                };
                if *tier == CheckTier::ExitAffecting {
                    any_exit_affecting = true;
                }
                messages.push(detail);
            }

            let overall_status = if any_fail {
                CheckStatus::Fail
            } else if rows
                .iter()
                .any(|(_, s, _, _, _)| matches!(s, CheckStatus::Warning))
            {
                CheckStatus::Warning
            } else if rows
                .iter()
                .any(|(_, s, _, _, _)| matches!(s, CheckStatus::Info))
            {
                CheckStatus::Info
            } else if rows.iter().all(|(_, s, _, _, _)| s.is_skipped()) {
                CheckStatus::Skipped("all MCP servers skipped".to_string())
            } else {
                CheckStatus::Pass
            };
            let overall_tier = if any_exit_affecting {
                CheckTier::ExitAffecting
            } else {
                CheckTier::Info
            };

            CheckResult {
                name: self.name().to_string(),
                category: "mcp".to_string(),
                status: overall_status,
                message: messages.join("; "),
                fix: if fix_hints.is_empty() {
                    None
                } else {
                    Some(fix_hints.join("; "))
                },
                latency: Some(start.elapsed()),
                tier: overall_tier,
            }
        }
    }
}

#[cfg(feature = "mcp")]
pub use mcp_check::*;

// ──────────────────────────────────────────────────────────────────
// Story 13.3a AC11: Update health check
// Feature-gated: `self-update`.
// ──────────────────────────────────────────────────────────────────

/// Update health check (Story 13.3a, AC11). Local/no-network, Info-tier always.
/// Reports current version + trusted signing key-id(s). Offline → Skipped.
#[cfg(feature = "self-update")]
pub struct UpdateHealthCheck;

#[cfg(feature = "self-update")]
#[async_trait]
impl HealthCheck for UpdateHealthCheck {
    fn name(&self) -> &str {
        "Update health"
    }

    async fn run(&self) -> CheckResult {
        let current = env!("CARGO_PKG_VERSION");
        let key_ids: Vec<&str> = crate::adapters::self_update::trust::TRUSTED_KEYS
            .iter()
            .map(|k| &k[..8]) // first 8 chars as key-id summary
            .collect();
        CheckResult {
            name: self.name().to_string(),
            category: "update".to_string(),
            status: CheckStatus::Info,
            message: format!(
                "Current: v{}; trusted key-ids: {}",
                current,
                key_ids.join(", ")
            ),
            fix: None,
            latency: None,
            tier: CheckTier::Info,
        }
    }
}

#[cfg(test)]
mod reachability_label_tests {
    use super::tools_reachability_label;

    #[test]
    fn composite_adapter_reports_subagents_available() {
        let label = tools_reachability_label(Some("composite"));
        assert!(
            label.contains("available"),
            "composite must be available, got: {label}"
        );
        assert!(
            !label.contains("UNAVAILABLE"),
            "composite must not be UNAVAILABLE, got: {label}"
        );
    }

    #[test]
    fn builtin_only_adapter_reports_subagents_unavailable() {
        // `base` (and `personal-assistant`, which inherits it) resolve to
        // builtin-only — the silent-missing-feature trap this check closes.
        let label = tools_reachability_label(Some("builtin-only"));
        assert!(
            label.contains("UNAVAILABLE"),
            "builtin-only must be UNAVAILABLE, got: {label}"
        );
        assert!(
            label.contains("composite"),
            "hint must name the fix (composite), got: {label}"
        );
    }

    #[test]
    fn builtin_full_adapter_reports_subagents_unavailable() {
        let label = tools_reachability_label(Some("builtin-full"));
        assert!(
            label.contains("UNAVAILABLE"),
            "builtin-full must be UNAVAILABLE, got: {label}"
        );
    }

    #[test]
    fn resolution_failure_is_silent() {
        // Doctor must never break over an informational hint.
        assert_eq!(tools_reachability_label(None), "");
    }
}
