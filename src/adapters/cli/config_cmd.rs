//! `rustain config` CLI subcommands — Story 8.1 AC-9 (reload) + Story 13.2a (show/edit/path/validate).
//!
//! All four new commands are purely local and never contact a provider.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;

use crate::adapters::cli::commands::Cli;
use crate::domain::ports::ProfileResolver;
use crate::infrastructure::utils::strip_url_userinfo;
use anyhow::{Context, Result};
use serde::Serialize;

// ---------------------------------------------------------------------------
// Existing: `rustain config reload` (Story 8.1 AC-9, byte-for-byte unchanged)
// ---------------------------------------------------------------------------

/// Run `rustain config reload` from outside a running rustain process.
/// Prints cross-process-not-supported message and exits 0.
pub async fn run_config_reload() -> Result<()> {
    println!("{}", config_reload_message());
    Ok(())
}

/// Return the exact `config reload` message.
///
/// Exposed so the regression test can assert byte-for-byte output without
/// relying on stdout capture.
pub fn config_reload_message() -> &'static str {
    "To reload a running TUI, type /config reload in it. To reload daemon \
     cron jobs on Unix, send SIGHUP to the daemon PID, for example: \
     kill -HUP $(cat <workspace>/.rustain/daemon.pid)."
}

// ---------------------------------------------------------------------------
// Layer descriptor — shared between `build_figment`, `config path`, `config edit`
// ---------------------------------------------------------------------------

/// Describes one layer in the figment merge chain.
///
/// Story 13.2a AC4: `build_figment` and `config path`/`config edit` both derive
/// their layer knowledge from this shared type, preventing drift.
#[derive(Debug, Clone, Serialize)]
pub struct LayerDescriptor {
    /// Human label (e.g. "CLI flags", "workspace config").
    pub kind: &'static str,
    /// Filesystem path for file-based layers; `None` for env/CLI/defaults.
    pub path: Option<PathBuf>,
    /// Whether the path exists on disk (always `false` for non-file layers).
    pub exists: bool,
    /// Priority rank: 1 = highest (CLI flags), 7 = lowest (built-in defaults).
    pub priority: u8,
}

/// Compute the 7-layer descriptor list for the figment merge chain.
///
/// This is the single source of truth for layer ordering. Both `build_figment`
/// (which BUILDS the config) and `config path`/`config edit` (which REPORT it)
/// consume this list. DRY/anti-drift per Story 13.2a AC4.
pub fn config_layer_paths(cli: &Cli) -> Result<Vec<LayerDescriptor>> {
    let cwd =
        std::env::current_dir().with_context(|| "Unable to determine current working directory")?;

    // Layer 4: workspace config path, respecting --config-file override
    let workspace_path = if let Some(override_path) = &cli.config_file {
        override_path.clone()
    } else {
        cwd.join(".rustain").join("config.toml")
    };

    // Layer 3: local override JSON
    let local_override_path = cwd.join(".claude").join("rustain-settings.json");

    // Layer 5: user-global config
    let user_global_path =
        dirs::home_dir().map(|h| h.join(".config").join("rustain").join("config.toml"));

    Ok(vec![
        LayerDescriptor {
            kind: "CLI flags",
            path: None,
            exists: false,
            priority: 1,
        },
        LayerDescriptor {
            kind: "RUSTAIN_* env vars",
            path: None,
            exists: false,
            priority: 2,
        },
        LayerDescriptor {
            kind: "local override (rustain-settings.json)",
            path: Some(local_override_path.clone()),
            exists: local_override_path.exists(),
            priority: 3,
        },
        LayerDescriptor {
            kind: "workspace config",
            path: Some(workspace_path.clone()),
            exists: workspace_path.exists(),
            priority: 4,
        },
        LayerDescriptor {
            kind: "user-global config",
            path: user_global_path.clone(),
            exists: user_global_path.as_ref().is_some_and(|p| p.exists()),
            priority: 5,
        },
        LayerDescriptor {
            kind: "active profile defaults",
            path: None,
            exists: false,
            priority: 6,
        },
        LayerDescriptor {
            kind: "built-in defaults",
            path: None,
            exists: false,
            priority: 7,
        },
    ])
}
// ---------------------------------------------------------------------------
// Fail-closed ConfigDisplay DTO (Story 13.2a AC2 — OQ3 resolution)
// ---------------------------------------------------------------------------

/// Fail-closed display DTO for `config show`.
///
/// Hand-mapped field-by-field from `AppConfig`. A new field added to `AppConfig`
/// is **invisible** until explicitly mapped here (fail-closed), at which point
/// the developer makes a redaction decision. This DTO feeds BOTH TOML and JSON
/// output — one format cannot leak while the other is clean.
///
/// URL-bearing fields pass through `strip_url_userinfo` during mapping.
#[derive(Debug, Serialize)]
pub struct ConfigDisplay<'a> {
    model: String,
    active_profile: String,
    log_level: String,
    log_max_size_mb: u64,
    log_retain_count: usize,
    snapshot_retention_count: Option<usize>,
    default_plan_mode: bool,
    skills: SkillsDisplay,
    runtime: RuntimeDisplay,
    layout: LayoutDisplay,
    provider: std::collections::BTreeMap<String, ProviderDisplay<'a>>,
    mouse: MouseDisplay,
    tool_progress: ToolProgressDisplay,
    router: RouterDisplay,
    pricing: std::collections::BTreeMap<String, PricingDisplay>,
    budget: BudgetDisplay,
    tools: ToolsDisplay,
    assembler: AssemblerDisplay,
    skill_exposure: SkillExposureDisplay,
    sandbox: SandboxDisplay,
    search: SearchDisplay,
    plan: PlanDisplay,
    subagents: SubagentsDisplay,
    daemon: DaemonDisplay,
    mcp_servers: Vec<McpServerDisplay<'a>>,
}

#[derive(Debug, Serialize)]
struct McpServerDisplay<'a> {
    id: String,
    transport: String,
    command: Option<String>,
    args: Vec<String>,
    url: Option<std::borrow::Cow<'a, str>>,
    env: std::collections::BTreeMap<String, String>,
    persistent: bool,
    source: String,
}
#[derive(Debug, Serialize)]
struct SkillsDisplay {
    disabled: Vec<String>,
}

#[derive(Debug, Serialize)]
struct EventBusDisplay {
    raw_capacity: usize,
}

#[derive(Debug, Serialize)]
struct RuntimeDisplay {
    event_bus: EventBusDisplay,
}

#[derive(Debug, Serialize)]
struct AutoPanelsDisplay {
    on_task_plan: String,
}

#[derive(Debug, Serialize)]
struct LayoutDisplay {
    auto_panels: AutoPanelsDisplay,
    density_mode: String,
}

#[derive(Debug, Serialize)]
struct ProviderDisplay<'a> {
    provider_id: String,
    model_id: String,
    /// Env var NAME (safe — never the resolved value).
    api_key_env: String,
    enabled: bool,
    kind: Option<String>,
    /// URL with userinfo stripped.
    base_url: Option<std::borrow::Cow<'a, str>>,
    context_window: Option<u32>,
    supports_tools: Option<bool>,
    discover_models: bool,
    model_filter: Vec<String>,
    cache_ttl_seconds: u64,
}

#[derive(Debug, Serialize)]
struct MouseDisplay {
    wheel_lines: u16,
    capture: bool,
}

#[derive(Debug, Serialize)]
struct ToolProgressDisplay {
    live_tail: bool,
    tail_lines: u8,
    threshold_ms: u64,
}

#[derive(Debug, Serialize)]
struct RouterDisplay {
    default_tier: String,
    threshold_tokens: u32,
    max_retries: u32,
    tier_models: std::collections::BTreeMap<String, String>,
    step_tiers: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Serialize)]
struct PricingDisplay {
    input_per_million: Option<f64>,
    output_per_million: Option<f64>,
    cache_creation_per_million: Option<f64>,
    cache_read_per_million: Option<f64>,
    reasoning_per_million: Option<f64>,
}

#[derive(Debug, Serialize)]
struct BudgetDisplay {
    daily_limit_usd: Option<f64>,
}

#[derive(Debug, Serialize)]
struct ToolsDisplay {
    exposure: String,
}

#[derive(Debug, Serialize)]
struct AssemblerDisplay {
    strategy: String,
}

#[derive(Debug, Serialize)]
struct SkillExposureDisplay {
    kind: String,
}

#[derive(Debug, Serialize)]
struct SandboxDisplay {
    adapter: String,
}

#[derive(Debug, Serialize)]
struct SearchDisplay {
    skills: String,
    tools: String,
}

#[derive(Debug, Serialize)]
struct PlanDisplay {
    concurrent_tasks_max: usize,
    subtask_failure_policy: String,
}

#[derive(Debug, Serialize)]
struct SubagentsDisplay {
    auto_approve: String,
}

#[derive(Debug, Serialize)]
struct DaemonDisplay {
    daily_reset: String,
    idle_timeout: String,
    low_power_emits_boundary: bool,
}

impl<'a> ConfigDisplay<'a> {
    /// Build the fail-closed DTO from a resolved `AppConfig` and optional active profile.
    ///
    /// Every URL field passes through `strip_url_userinfo`. Every field is
    /// an explicit opt-in — if `AppConfig` gains a new field, it stays
    /// invisible here until mapped.
    pub fn from_config(
        config: &'a crate::domain::models::AppConfig,
        active_profile: Option<&'a crate::domain::models::ResolvedProfile>,
    ) -> Self {
        let provider = config
            .provider
            .iter()
            .map(|(key, p)| {
                (
                    key.clone(),
                    ProviderDisplay {
                        provider_id: p.provider_id.clone(),
                        model_id: p.model_id.clone(),
                        api_key_env: p.api_key_env.clone(),
                        enabled: p.enabled,
                        kind: p.kind.clone(),
                        base_url: p.base_url.as_deref().map(strip_url_userinfo),
                        context_window: p.context_window,
                        supports_tools: p.supports_tools,
                        discover_models: p.discover_models,
                        model_filter: p.model_filter.clone(),
                        cache_ttl_seconds: p.cache_ttl_seconds,
                    },
                )
            })
            .collect();

        let mcp_servers = active_profile
            .map(|p| &p.mcp_servers)
            .into_iter()
            .flatten()
            .map(|s| McpServerDisplay {
                id: s.id.clone(),
                transport: format!("{:?}", s.transport).to_lowercase(),
                command: s.command.clone(),
                args: s.args.clone(),
                url: s.url.as_deref().map(strip_url_userinfo),
                env: s.env.clone(),
                persistent: s.persistent,
                source: format!("{:?}", s.source).to_lowercase(),
            })
            .collect();

        let tier_models = config
            .router
            .tier_models
            .iter()
            .map(|(k, v)| (format!("{k:?}"), v.clone()))
            .collect();
        let step_tiers = config
            .router
            .step_tiers
            .iter()
            .map(|(k, v)| (format!("{k:?}"), format!("{v:?}")))
            .collect();

        let pricing = config
            .pricing
            .iter()
            .map(|(key, p)| {
                (
                    key.clone(),
                    PricingDisplay {
                        input_per_million: finite_or_none(p.input_per_million),
                        output_per_million: finite_or_none(p.output_per_million),
                        cache_creation_per_million: p
                            .cache_creation_per_million
                            .and_then(finite_or_none),
                        cache_read_per_million: p.cache_read_per_million.and_then(finite_or_none),
                        reasoning_per_million: p.reasoning_per_million.and_then(finite_or_none),
                    },
                )
            })
            .collect();
        Self {
            model: config.model.clone(),
            active_profile: config.active_profile.clone(),
            log_level: config.log_level.clone(),
            log_max_size_mb: config.log_max_size_mb,
            log_retain_count: config.log_retain_count,
            snapshot_retention_count: config.snapshot_retention_count,
            default_plan_mode: config.default_plan_mode,
            skills: SkillsDisplay {
                disabled: config.skills.disabled.clone(),
            },
            runtime: RuntimeDisplay {
                event_bus: EventBusDisplay {
                    raw_capacity: config.runtime.event_bus.raw_capacity,
                },
            },
            layout: LayoutDisplay {
                auto_panels: AutoPanelsDisplay {
                    on_task_plan: config.layout.auto_panels.on_task_plan.clone(),
                },
                density_mode: format!("{:?}", config.layout.density_mode).to_lowercase(),
            },
            provider,
            mouse: MouseDisplay {
                wheel_lines: config.mouse.wheel_lines,
                capture: config.mouse.capture,
            },
            tool_progress: ToolProgressDisplay {
                live_tail: config.tool_progress.live_tail,
                tail_lines: config.tool_progress.tail_lines,
                threshold_ms: config.tool_progress.threshold_ms,
            },
            router: RouterDisplay {
                default_tier: format!("{:?}", config.router.default_tier),
                threshold_tokens: config.router.threshold_tokens,
                max_retries: config.router.max_retries,
                tier_models,
                step_tiers,
            },
            pricing,
            budget: BudgetDisplay {
                daily_limit_usd: config.budget.daily_limit_usd.and_then(finite_or_none),
            },
            tools: ToolsDisplay {
                exposure: config.tools.exposure.clone(),
            },
            assembler: AssemblerDisplay {
                strategy: config.assembler.strategy.clone(),
            },
            skill_exposure: SkillExposureDisplay {
                kind: config.skill_exposure.kind.clone(),
            },
            sandbox: SandboxDisplay {
                adapter: config.sandbox.adapter.clone(),
            },
            search: SearchDisplay {
                skills: config.search.skills.clone(),
                tools: config.search.tools.clone(),
            },
            plan: PlanDisplay {
                concurrent_tasks_max: config.plan.concurrent_tasks_max,
                subtask_failure_policy: format!("{:?}", config.plan.subtask_failure_policy),
            },
            subagents: SubagentsDisplay {
                auto_approve: format!("{:?}", config.subagents.auto_approve),
            },
            daemon: DaemonDisplay {
                daily_reset: config.daemon.daily_reset.clone(),
                idle_timeout: config.daemon.idle_timeout.clone(),
                low_power_emits_boundary: config.daemon.low_power_emits_boundary,
            },
            mcp_servers,
        }
    }
}

// ---------------------------------------------------------------------------
// `rustain config show` (Story 13.2a AC2)
// ---------------------------------------------------------------------------

/// Render the fully-resolved configuration to a string.
///
/// Returns TOML or JSON depending on `json`. This helper is separate from
/// `run_config_show` so tests can assert on the rendered string without
/// capturing stdout. It renders a fail-closed `ConfigDisplay` DTO (never the
/// live `AppConfig`). TOML output goes through `toml::Value` round-trip to avoid
/// `ValuesMustBeEmittedBeforeTables`. JSON uses `serde_json::to_string_pretty`.
///
/// **Structural guarantee:** this function and its helpers contain ZERO
/// `std::env::var()` or `env_var_trimmed` calls — it serializes the resolved
/// config as-loaded. The config holds env-var *names* (`api_key_env`), not
/// secret values.
pub async fn render_config_show(
    json: bool,
    profile_resolver: &Arc<dyn ProfileResolver>,
    cli: &Cli,
) -> Result<String> {
    let config = match crate::infrastructure::config::try_load(cli, profile_resolver.as_ref()) {
        Ok(c) => c,
        Err(e) => {
            // Log the original error (may contain raw config values) for diagnostics,
            // but do NOT include it in the user-facing anyhow message to avoid leaking
            // userinfo-bearing URLs from the figment extraction error.
            tracing::error!("config show: failed to load configuration: {e:#}");
            anyhow::bail!("Configuration is invalid (run `rustain config validate` for details)");
        }
    };

    // Resolve the active profile so MCP server URLs can be shown and stripped.
    // Failure here is non-fatal — we simply omit the MCP section.
    let active_profile = profile_resolver.resolve_active();

    let display = ConfigDisplay::from_config(&config, active_profile.as_ref());

    if json {
        config_display_to_json(&display)
    } else {
        config_display_to_toml(&display)
    }
}

/// Display the fully-resolved configuration.
///
/// Renders a fail-closed `ConfigDisplay` DTO (never the live `AppConfig`).
/// TOML output goes through `toml::Value` round-trip to avoid
/// `ValuesMustBeEmittedBeforeTables`. JSON uses `serde_json::to_string_pretty`.
///
/// **Structural guarantee:** this function and its helpers contain ZERO
/// `std::env::var()` or `env_var_trimmed` calls — it serializes the resolved
/// config as-loaded. The config holds env-var *names* (`api_key_env`), not
/// secret values.
pub async fn run_config_show(
    json: bool,
    profile_resolver: &Arc<dyn ProfileResolver>,
    cli: &Cli,
) -> Result<()> {
    let output = render_config_show(json, profile_resolver, cli).await?;
    println!("{output}");
    Ok(())
}

// ---------------------------------------------------------------------------
// `rustain config edit` (Story 13.2a AC3)
// ---------------------------------------------------------------------------

/// Open the active config file in `$EDITOR`.
///
pub async fn run_config_edit(global: bool, cli: &Cli) -> Result<()> {
    #[cfg(not(feature = "test-instrumentation"))]
    if !std::io::stdin().is_terminal() {
        anyhow::bail!("rustain config edit requires an interactive terminal.");
    }

    let layers = config_layer_paths(cli)?;

    // global → Layer-5 (priority 5, user-global); else Layer-4 (priority 4, workspace)
    let target_priority: u8 = if global { 5 } else { 4 };
    let target_layer = layers
        .iter()
        .find(|l| l.priority == target_priority)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Internal error: could not find config layer priority {}",
                target_priority
            )
        })?;

    let target_path = target_layer
        .path
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("No file path for {} layer", target_layer.kind))?;

    // Verify workspace > global precedence — don't open a layer that can't win
    if global {
        let ws_layer = layers.iter().find(|l| l.priority == 4);
        if let Some(ws) = ws_layer {
            if ws.exists {
                eprintln!(
                    "Note: workspace config ({}) has higher precedence than global config.",
                    ws.path
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default()
                );
            }
        }
    }

    // Create parent dirs + comment-only scaffold if absent.
    // Use create_new to avoid silently overwriting a file created by another process (TOCTOU).
    if !target_path.exists() {
        if let Some(parent) = target_path.parent() {
            std::fs::create_dir_all(parent)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
            }
        }
        let mut file = {
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            opts.open(target_path).with_context(|| {
                format!(
                    "Failed to create scaffold config at {}",
                    target_path.display()
                )
            })?
        };
        use std::io::Write;
        file.write_all(b"# rustain config\n")?;
        println!("Created {}", target_path.display());
    }

    let editor = crate::infrastructure::utils::editor::resolve_editor()?;
    let status = crate::infrastructure::utils::editor::run_editor(&editor, target_path)?;
    if !status.success() {
        anyhow::bail!("editor '{}' exited with status {}", editor, status);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// `rustain config path` (Story 13.2a AC4)
// ---------------------------------------------------------------------------

/// Render the config layer list to a string.
///
/// Separate from `run_config_path` so tests can assert on the rendered string.
pub fn render_config_path(json: bool, cli: &Cli) -> Result<String> {
    let layers = config_layer_paths(cli)?;

    if json {
        let json_layers: Vec<serde_json::Value> = layers
            .iter()
            .map(|l| {
                let mut obj = serde_json::Map::new();
                obj.insert("layer".into(), serde_json::Value::String(l.kind.into()));
                obj.insert(
                    "priority".into(),
                    serde_json::Value::Number(l.priority.into()),
                );
                if let Some(p) = &l.path {
                    obj.insert(
                        "path".into(),
                        serde_json::Value::String(p.display().to_string()),
                    );
                    obj.insert("exists".into(), serde_json::Value::Bool(l.exists));
                }
                serde_json::Value::Object(obj)
            })
            .collect();
        Ok(serde_json::to_string_pretty(&json_layers)?)
    } else {
        let mut lines = vec!["Config layers (highest → lowest priority):".to_string()];
        lines.push(String::new());
        for layer in &layers {
            let marker = if let Some(p) = &layer.path {
                if layer.exists {
                    format!("✓ {} — {}", layer.kind, p.display())
                } else {
                    format!("✗ {} — {} (absent)", layer.kind, p.display())
                }
            } else {
                format!("· {} (non-file layer)", layer.kind)
            };
            lines.push(format!("  {}. {}", layer.priority, marker));
        }
        Ok(lines.join("\n"))
    }
}

/// Display config file locations and their precedence order.
pub async fn run_config_path(json: bool, cli: &Cli) -> Result<()> {
    let output = render_config_path(json, cli)?;
    println!("{output}");
    Ok(())
}

// ---------------------------------------------------------------------------
// `rustain config validate` (Story 13.2a AC5)
// ---------------------------------------------------------------------------

/// Validate configuration without launching the TUI or contacting a provider.
///
/// Mirrors `profile/validate.rs` exit-code discipline: 0 = valid, non-zero = invalid.
pub async fn run_config_validate(
    json: bool,
    profile_resolver: &Arc<dyn ProfileResolver>,
    cli: &Cli,
) -> Result<()> {
    match crate::infrastructure::config::try_load(cli, profile_resolver.as_ref()) {
        Ok(_config) => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "valid": true,
                        "errors": []
                    }))?
                );
            } else {
                println!("Configuration is valid.");
            }
            Ok(())
        }
        Err(e) => {
            let error_msg = e.to_string();
            // Log the original validation error here; the SubcommandExit wrapper
            // returned below has no source chain, so logging now preserves detail.
            tracing::error!("Configuration is invalid: {error_msg}");
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "valid": false,
                        "errors": [error_msg]
                    }))?
                );
            } else {
                eprintln!("Configuration is invalid: {error_msg}");
            }
            // Non-zero exit via SubcommandExit (mirrors profile validate)
            Err(crate::infrastructure::startup::SubcommandExit(
                crate::infrastructure::startup::SubcommandExit::GENERIC,
            )
            .into())
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Sanitize a provider name so it is safe to use as a TOML table key.
///
/// TOML bare keys may contain only `[A-Za-z0-9_-]`. Keys with other characters
/// must be quoted strings. This helper returns a quoted key when necessary.
fn toml_safe_key(key: &str) -> String {
    if key
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        key.to_string()
    } else {
        // toml::Value will serialize this as a quoted string when it is the key
        // of a table. We quote it explicitly here so the human output is valid.
        format!("\"{}\"", key.replace('\"', "\\\""))
    }
}

/// Guard against non-finite f64 values before serializing to TOML.
///
/// toml::Value::try_from panics on NaN/Infinity; this replaces them with
/// `None` so the field is omitted rather than crashing the CLI.
fn finite_or_none(val: f64) -> Option<f64> {
    if val.is_finite() { Some(val) } else { None }
}

/// Replace a raw provider-keyed map with TOML-safe keys in a `toml::Value` tree.
///
/// ConfigDisplay uses `BTreeMap<String, _>` for provider/pricing, which serde
/// serializes to maps. When converting to TOML, bare keys with special chars
/// become invalid. This post-processes the serialized `toml::Value` to quote
/// such keys.
fn sanitize_toml_map_keys(value: toml::Value) -> toml::Value {
    match value {
        toml::Value::Table(table) => {
            let mut new_table = toml::value::Table::new();
            for (k, v) in table {
                new_table.insert(toml_safe_key(&k), sanitize_toml_map_keys(v));
            }
            toml::Value::Table(new_table)
        }
        toml::Value::Array(arr) => {
            toml::Value::Array(arr.into_iter().map(sanitize_toml_map_keys).collect())
        }
        other => other,
    }
}
/// Render a `ConfigDisplay` to pretty TOML with TOML-safe provider/pricing keys.
fn config_display_to_toml(display: &ConfigDisplay<'_>) -> Result<String> {
    let mut value = toml::Value::try_from(display)?;
    value = sanitize_toml_map_keys(value);
    Ok(toml::to_string_pretty(&value)?)
}

/// Render a `ConfigDisplay` to pretty JSON with finite floats guaranteed.
fn config_display_to_json(display: &ConfigDisplay<'_>) -> Result<String> {
    Ok(serde_json::to_string_pretty(display)?)
}
