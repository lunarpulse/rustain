//! Layered configuration loader.
//!
//! Resolves `AppConfig` from a stack of layered providers using `figment`
//! (Epic 7 retro AI-7.2 / Path A). Field-level merge is the binding behavior
//! per Epic 8 Story 8.1 AC — adding `[pricing."my-model"]` in a user file
//! ADDS to the default pricing catalog instead of REPLACING it. The same
//! holds for `[provider.X]` and `[router.step_tiers]`.
//!
//! Layer order (later layers override earlier layers at the key level):
//!
//! 0. **`-c` overrides** — dynamic CLI key/value overrides (Story 13.6)
//! 1. **CLI flags** — `CliOverrides` from `Cli` struct (Story 8.1 AC-2)
//! 2. **Environment variables** — `RUSTAIN_*` prefixed env vars (Story 8.1 AC-3)
//! 3. **Local override** — `{workspace}/.claude/rustain-settings.json` (Story 8.1 AC-4)
//! 4. **Workspace config** — `<cwd>/.rustain/config.toml` if present
//! 5. **User-global config** — `~/.config/rustain/config.toml` if present
//! 6. **Active profile defaults** — via `ProfileResolver` (Story 8.2; no-op until then)
//! 7. **Built-in defaults** — `AppConfig::default()` serialized into the merge chain

use std::path::Path;

use figment::Figment;
use figment::providers::{Env, Format, Json, Serialized, Toml};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::adapters::cli::commands::Cli;
use crate::domain::errors::{ConfigError, DomainError};
use crate::domain::models::AppConfig;
use crate::domain::ports::ProfileResolver;

pub const CONFIG_OVERRIDE_PRIORITY: u8 = 0;
pub const CLI_FLAGS_PRIORITY: u8 = 1;
pub const ENV_VARS_PRIORITY: u8 = 2;
pub const LOCAL_OVERRIDE_PRIORITY: u8 = 3;
pub const WORKSPACE_CONFIG_PRIORITY: u8 = 4;
pub const USER_GLOBAL_CONFIG_PRIORITY: u8 = 5;
pub const ACTIVE_PROFILE_PRIORITY: u8 = 6;
pub const BUILT_IN_DEFAULTS_PRIORITY: u8 = 7;

pub const KNOWN_TOP_LEVEL_FIELDS: &[&str] = &[
    "model",
    "active_profile",
    "log_level",
    "log_max_size_mb",
    "log_retain_count",
    "snapshot_retention_count",
    "skills",
    "runtime",
    "layout",
    "default_plan_mode",
    "provider",
    "mouse",
    "tool_progress",
    "router",
    "pricing",
    "budget",
    "tools",
    "assembler",
    "skill_exposure",
    "sandbox",
    "search",
    "plan",
    "subagents",
    "daemon",
    "fanout_spawn_gate_threshold",
];

const CREDENTIAL_PATH_SEGMENTS: &[&str] = &[
    "api_key_env",
    "api_key",
    "secret",
    "token",
    "credential",
    "password",
];
const CREDENTIAL_TOP_LEVEL: &[&str] = &["auth"];

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{message}")]
pub struct ConfigOverrideError {
    message: String,
}

impl ConfigOverrideError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

pub fn parse_config_overrides(pairs: &[String]) -> Result<Value, ConfigOverrideError> {
    let mut root = Map::new();

    for pair in pairs {
        let (key, raw_value) = pair.split_once('=').ok_or_else(|| {
            ConfigOverrideError::new(format!(
                "Invalid -c override '{pair}'. Expected KEY=VALUE, for example: -c model=gpt-4o"
            ))
        })?;
        if key.is_empty() {
            return Err(ConfigOverrideError::new(
                "Invalid -c override: key must not be empty. Expected KEY=VALUE.",
            ));
        }

        let segments: Vec<&str> = key.split('.').collect();
        if segments.iter().any(|segment| segment.is_empty()) {
            return Err(ConfigOverrideError::new(format!(
                "Invalid -c override '{key}': dot-path segments must not be empty."
            )));
        }

        validate_config_override_key(key, &segments)?;
        warn_unknown_config_override_top_level(key, segments[0]);
        insert_config_override_value(&mut root, &segments, raw_value)?;
    }

    Ok(Value::Object(root))
}

fn validate_config_override_key(key: &str, segments: &[&str]) -> Result<(), ConfigOverrideError> {
    if let Some(blocked) = segments
        .iter()
        .find(|segment| CREDENTIAL_PATH_SEGMENTS.contains(segment))
    {
        let provider = provider_hint_for_segments(segments);
        if *blocked == "api_key_env" {
            return Err(ConfigOverrideError::new(format!(
                "Cannot set credential-adjacent key '{key}' via -c (credential keys are restricted). To change the env var source, edit your config file or use: rustain auth login {provider}"
            )));
        }
        return Err(ConfigOverrideError::new(format!(
            "Cannot set credential key '{key}' via -c (secrets would appear in shell history). Use: rustain auth login {provider}"
        )));
    }

    if CREDENTIAL_TOP_LEVEL.contains(&segments[0]) {
        return Err(ConfigOverrideError::new(format!(
            "Cannot set credential key '{key}' via -c (secrets would appear in shell history). Use: rustain auth login <provider>"
        )));
    }

    Ok(())
}

fn provider_hint_for_segments<'a>(segments: &'a [&'a str]) -> &'a str {
    if segments.len() > 1 && segments[0] == "provider" {
        segments[1]
    } else {
        "<provider>"
    }
}

fn warn_unknown_config_override_top_level(key: &str, top_level: &str) {
    if KNOWN_TOP_LEVEL_FIELDS.contains(&top_level) {
        return;
    }
    let message = format!(
        "warning: unknown config key '{top_level}' from -c '{key}'. The key will still be passed to the config loader. Known keys include: model, provider, router, tools, daemon"
    );
    tracing::warn!("{message}");
    eprintln!("{message}");
}

fn insert_config_override_value(
    root: &mut Map<String, Value>,
    segments: &[&str],
    raw_value: &str,
) -> Result<(), ConfigOverrideError> {
    let leaf = segments[segments.len() - 1];
    let mut current = root;
    for (depth, segment) in segments[..segments.len() - 1].iter().enumerate() {
        let depth = depth + 1;
        let entry = current
            .entry((*segment).to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if !entry.is_object() {
            let prior_key = segments[..depth].join(".");
            let conflict_key = segments.join(".");
            return Err(ConfigOverrideError::new(format!(
                "Conflicting -c overrides: '{prior_key}' and '{conflict_key}' cannot both be set."
            )));
        }
        current = entry
            .as_object_mut()
            .expect("entry was just verified to be object");
    }

    let value = serde_json::from_str::<Value>(raw_value)
        .unwrap_or_else(|_| Value::String(raw_value.to_owned()));
    if !matches!(value, Value::String(_) | Value::Bool(_) | Value::Number(_)) {
        return Err(ConfigOverrideError::new(format!(
            "Invalid -c value for '{key}': arrays, objects, and null are not supported. Use scalar values (strings, numbers, booleans).",
            key = segments.join(".")
        )));
    }

    current.insert(leaf.to_string(), value);
    Ok(())
}
/// CLI overrides for the figment chain (Story 8.1 AC-2).
/// Each field is an `Option` — absent flags contribute nothing to the layer.
#[derive(Serialize, Default)]
struct CliOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    log_level: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    snapshot_retention_count: Option<usize>,
    /// Story 9.4 — overrides `app_config.tools.exposure`. Routed via nested
    /// figment key `tools.exposure` so it merges into the AppConfig.tools
    /// struct at the same precedence layer as `RUSTAIN_TOOLS__EXPOSURE`.
    #[serde(skip_serializing_if = "Option::is_none", rename = "tools")]
    tools: Option<CliToolsOverride>,
    /// Story 9.6 — overrides `app_config.skill_exposure.kind`. Routed via nested
    /// figment key `skill_exposure.kind`.
    #[serde(skip_serializing_if = "Option::is_none", rename = "skill_exposure")]
    skill_exposure: Option<CliSkillExposureOverride>,
    /// Story 9.5 — overrides `app_config.sandbox.adapter`. Routed via nested
    /// figment key `sandbox.adapter`.
    #[serde(skip_serializing_if = "Option::is_none", rename = "sandbox")]
    sandbox: Option<CliSandboxOverride>,
}

#[derive(Serialize, Default)]
struct CliToolsOverride {
    #[serde(skip_serializing_if = "Option::is_none", rename = "exposure")]
    exposure: Option<String>,
}

#[derive(Serialize, Default)]
struct CliSkillExposureOverride {
    #[serde(skip_serializing_if = "Option::is_none", rename = "kind")]
    kind: Option<String>,
}

#[derive(Serialize, Default)]
struct CliSandboxOverride {
    #[serde(skip_serializing_if = "Option::is_none", rename = "adapter")]
    adapter: Option<String>,
}

impl From<&Cli> for CliOverrides {
    fn from(cli: &Cli) -> Self {
        Self {
            model: cli.model.clone(),
            log_level: cli.log_level.clone(),
            snapshot_retention_count: cli.snapshot_retention,
            tools: cli.tool_exposure.clone().map(|exposure| CliToolsOverride {
                exposure: Some(exposure),
            }),
            skill_exposure: cli
                .skill_exposure
                .clone()
                .map(|kind| CliSkillExposureOverride { kind: Some(kind) }),
            sandbox: cli
                .sandbox_adapter
                .clone()
                .map(|adapter| CliSandboxOverride {
                    adapter: Some(adapter),
                }),
        }
    }
}

/// Load application configuration via the layered figment merge chain.
///
/// Returns `AppConfig::default()` if no config files are present and no
/// figment errors occur. Malformed files trigger a `tracing::error!` and
/// fall through to the next layer (file is skipped, not fatal).
pub fn load(cli: &Cli, profile_resolver: &dyn ProfileResolver) -> AppConfig {
    load_with_config_overrides(cli, profile_resolver, None)
}

pub fn load_with_config_overrides(
    cli: &Cli,
    profile_resolver: &dyn ProfileResolver,
    cli_config_overrides: Option<&Value>,
) -> AppConfig {
    match try_load_with_config_overrides(cli, profile_resolver, cli_config_overrides) {
        Ok(config) => config,
        Err(e) => {
            tracing::error!(
                "Layered config extraction failed: {:?}. Falling back to defaults. \
                 Run `rustain doctor` to diagnose.",
                e
            );
            AppConfig::default()
        }
    }
}

/// Fallible version of `load()` that returns a `DomainError` instead of
/// silently falling back to defaults. Used by the reload path so failures
/// don't silently swap defaults — they preserve the prior config per AC-11.
pub fn try_load(
    cli: &Cli,
    profile_resolver: &dyn ProfileResolver,
) -> Result<AppConfig, DomainError> {
    try_load_with_config_overrides(cli, profile_resolver, None)
}

pub fn try_load_with_config_overrides(
    cli: &Cli,
    profile_resolver: &dyn ProfileResolver,
    cli_config_overrides: Option<&Value>,
) -> Result<AppConfig, DomainError> {
    let figment = build_figment(cli, profile_resolver, cli_config_overrides);

    let mut config: AppConfig = figment.extract().map_err(|e| {
        DomainError::Config(ConfigError::Extract {
            reason: e.to_string(),
        })
    })?;

    // Post-deserialization validation
    if let Err(e) = config.layout.auto_panels.validate() {
        tracing::warn!(
            "Config layout.auto_panels has invalid value: {} — \
             falling back to default for that key.",
            e
        );
        config.layout.auto_panels = Default::default();
    }

    // models.dev live pricing: overlay the cached snapshot onto the bundled +
    // user-merged `config.pricing` (config wins, models.dev fills gaps). No
    // network here — the background refresh task keeps the disk cache fresh.
    #[cfg(feature = "models-dev")]
    crate::adapters::models_dev::merge_into_config(&mut config);
    Ok(config)
}

/// Build the full config figment chain.
///
/// File-layer paths are derived from `config_layer_paths()` (the shared
/// source of truth introduced in Story 13.2a AC4) so `build_figment`,
/// `config path`, and `config edit` all agree.
fn build_figment(
    cli: &Cli,
    profile_resolver: &dyn ProfileResolver,
    cli_config_overrides: Option<&Value>,
) -> Figment {
    let layers = match crate::adapters::cli::config_cmd::config_layer_paths(cli) {
        Ok(layers) => layers,
        Err(e) => {
            tracing::error!("Failed to resolve config layer paths: {e}");
            // Fall back to an empty layer list so the merge still produces a valid
            // (if default-only) configuration rather than panicking.
            Vec::new()
        }
    };

    // Start with the lowest-priority layer and merge higher-priority layers on top.
    let mut figment = Figment::new();
    for layer in layers.iter().rev() {
        match layer.priority {
            CONFIG_OVERRIDE_PRIORITY => {}
            BUILT_IN_DEFAULTS_PRIORITY => {
                figment = figment.merge(Serialized::defaults(AppConfig::default()));
            }
            ACTIVE_PROFILE_PRIORITY => {
                if let Some(profile_value) = profile_resolver.resolve_active_profile_defaults() {
                    figment = figment.merge(Serialized::defaults(profile_value));
                }
            }
            USER_GLOBAL_CONFIG_PRIORITY => {
                if let Some(path) = &layer.path {
                    figment = merge_toml_if_valid(figment, path, "user-global");
                }
            }
            WORKSPACE_CONFIG_PRIORITY => {
                if let Some(path) = &layer.path {
                    figment = merge_toml_if_valid(figment, path, "workspace");
                }
            }
            LOCAL_OVERRIDE_PRIORITY => {
                if let Some(path) = &layer.path {
                    figment = merge_json_if_valid(figment, path, "local-override");
                }
            }
            ENV_VARS_PRIORITY => figment = figment.merge(Env::prefixed("RUSTAIN_").split("__")),
            CLI_FLAGS_PRIORITY => {
                figment = figment.merge(Serialized::globals(CliOverrides::from(cli)));
            }
            _ => {}
        }
    }

    if let Some(overrides) = cli_config_overrides {
        figment = figment.merge(Serialized::globals(overrides));
    }

    figment
}

/// Back-compat entry point for code that hasn't migrated to the new signature.
/// Used during the transitional period of Story 8.1's ArcSwap migration.
pub fn load_default() -> AppConfig {
    let cli = Cli {
        log_level: Some("info".to_string()),
        command: None,
        new: false,
        session: None,
        snapshot_retention: None,
        config_file: None,
        model: None,
        config_override: Vec::new(),
        profile: None,
        persona: None,
        memory: None,
        session_adapter: None,
        tools: None,
        channels: None,
        scheduler: None,
        context: None,
        tool_exposure: None,
        skill_exposure: None,
        sandbox_adapter: None,
        serve_a2a: None,
    };
    load(
        &cli,
        &crate::adapters::profile_resolver::noop::NoopProfileResolver,
    )
}

/// Merge a TOML file into the figment if it exists AND parses.
fn merge_toml_if_valid(figment: Figment, path: &Path, label: &str) -> Figment {
    if !path.exists() {
        return figment;
    }

    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(
                "Config file unreadable at {} ({}): {}. Skipping {} layer.",
                path.display(),
                label,
                e,
                label
            );
            return figment;
        }
    };
    if let Err(e) = toml::from_str::<toml::Value>(&content) {
        tracing::error!(
            "Config file at {} ({}) is malformed: {}. Skipping {} layer. \
             Fix the file or run `rustain doctor` to diagnose.",
            path.display(),
            label,
            e,
            label
        );
        return figment;
    }

    tracing::info!("Merging {} config layer from {}", label, path.display());
    figment.merge(Toml::file(path))
}

/// Merge a JSON file into the figment if it exists AND parses.
/// Story 8.1 AC-4 — CC-compatible `rustain-settings.json` override layer.
fn merge_json_if_valid(figment: Figment, path: &Path, label: &str) -> Figment {
    if !path.exists() {
        return figment;
    }

    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(
                "Config file unreadable at {}: {}. Skipping {} layer.",
                path.display(),
                e,
                label
            );
            return figment;
        }
    };
    if let Err(e) = serde_json::from_str::<serde_json::Value>(&content) {
        tracing::error!(
            "JSON config file at {} is malformed: line {} col {}: {}. \
             Skipping {} layer.",
            path.display(),
            e.line(),
            e.column(),
            e,
            label
        );
        return figment;
    }

    tracing::info!("Merging {} config layer from {}", label, path.display());
    figment.merge(Json::file(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::profile_resolver::noop::NoopProfileResolver;
    use figment::providers::Toml;

    fn test_cli() -> Cli {
        Cli {
            log_level: Some("info".to_string()),
            command: None,
            new: false,
            session: None,
            snapshot_retention: None,
            config_file: None,
            model: None,
            config_override: Vec::new(),
            profile: None,
            persona: None,
            memory: None,
            session_adapter: None,
            tools: None,
            channels: None,
            scheduler: None,
            context: None,
            tool_exposure: None,
            skill_exposure: None,
            sandbox_adapter: None,
            serve_a2a: None,
        }
    }

    /// Helper: construct a figment from defaults + a TOML string layer.
    fn figment_with_user_layer(user_toml: &str) -> Figment {
        Figment::from(Serialized::defaults(AppConfig::default())).merge(Toml::string(user_toml))
    }

    #[test]
    fn pricing_user_entry_merges_into_default_catalog() {
        let user_toml = r#"
            model = "test-model"
            [pricing."my-custom-model"]
            input_per_million = 0.50
            output_per_million = 1.00
        "#;
        let config: AppConfig = figment_with_user_layer(user_toml)
            .extract()
            .expect("extract figment");

        let custom = config
            .pricing
            .get("my-custom-model")
            .expect("user-supplied my-custom-model present in merged catalog");
        assert_eq!(custom.input_per_million, 0.50);
        assert_eq!(custom.output_per_million, 1.00);

        assert!(
            config.pricing.contains_key("claude-sonnet-4-6"),
            "default Sonnet pricing must survive a user adding one custom entry"
        );
        assert!(
            config.pricing.contains_key("gpt-4o"),
            "default GPT-4o pricing must survive a user adding one custom entry"
        );
        let sonnet = config.pricing.get("claude-sonnet-4-6").unwrap();
        assert_eq!(sonnet.input_per_million, 3.00);
        assert_eq!(sonnet.output_per_million, 15.00);
    }

    #[test]
    fn pricing_partial_struct_override_preserves_other_fields() {
        let user_toml = r#"
            model = "test-model"
            [pricing."claude-sonnet-4-6"]
            input_per_million = 2.50
        "#;
        let config: AppConfig = figment_with_user_layer(user_toml)
            .extract()
            .expect("extract figment");

        let sonnet = config.pricing.get("claude-sonnet-4-6").unwrap();
        assert_eq!(sonnet.input_per_million, 2.50);
        assert_eq!(sonnet.output_per_million, 15.00);
        assert_eq!(sonnet.cache_creation_per_million, Some(3.75));
        assert_eq!(sonnet.cache_read_per_million, Some(0.30));
    }

    #[test]
    fn provider_user_entry_merges_into_default_map() {
        let user_toml = r#"
            model = "test-model"
            [provider.openrouter]
            provider_id = "openrouter"
            model_id = "anthropic/claude-3.5-sonnet"
            api_key_env = "OPENROUTER_API_KEY"
        "#;
        let config: AppConfig = figment_with_user_layer(user_toml)
            .extract()
            .expect("extract figment");
        assert!(config.provider.contains_key("openrouter"));
        assert_eq!(
            config.provider["openrouter"].model_id,
            "anthropic/claude-3.5-sonnet"
        );
    }

    #[test]
    fn router_step_tiers_user_override_merges_into_defaults() {
        use crate::domain::models::router::{ModelTier, StepKind};

        let user_toml = r#"
            model = "test-model"
            [router.step_tiers]
            codegen = "cheap_agentic"
        "#;
        let config: AppConfig = figment_with_user_layer(user_toml)
            .extract()
            .expect("extract figment");

        assert_eq!(
            config.router.step_tiers.get(&StepKind::Codegen),
            Some(&ModelTier::CheapAgentic),
        );
        assert_eq!(
            config.router.step_tiers.get(&StepKind::Plan),
            Some(&ModelTier::Flagship),
        );
    }

    #[test]
    fn pricing_later_layer_overrides_earlier_at_key_level() {
        let figment = Figment::from(Serialized::defaults(AppConfig::default()))
            .merge(Toml::string(
                r#"
                model = "test-model"
                [pricing."claude-sonnet-4-6"]
                input_per_million = 2.00
                output_per_million = 10.00
            "#,
            ))
            .merge(Toml::string(
                r#"
                [pricing."claude-sonnet-4-6"]
                input_per_million = 1.50
            "#,
            ));

        let config: AppConfig = figment.extract().expect("extract figment");
        let sonnet = config.pricing.get("claude-sonnet-4-6").unwrap();
        assert_eq!(sonnet.input_per_million, 1.50);
        assert_eq!(sonnet.output_per_million, 10.00);
    }

    #[test]
    fn provider_discover_models_survives_figment_roundtrip() {
        let user_toml = r#"
            model = "deepseek/deepseek-v4-flash"

            [provider.openrouter]
            provider_id    = "openrouter"
            kind           = "openai-compatible"
            model_id       = "deepseek/deepseek-v4-flash"
            api_key_env    = "OPENROUTER_API_KEY"
            enabled        = true
            base_url       = "https://openrouter.ai/api/v1"
            discover_models = true
            context_window = 131072
            cache_ttl_seconds = 3600
        "#;
        let config: AppConfig = figment_with_user_layer(user_toml)
            .extract()
            .expect("extract figment");

        let or = config
            .provider
            .get("openrouter")
            .expect("openrouter provider must be present");
        assert!(or.discover_models);
        assert_eq!(or.kind.as_deref(), Some("openai-compatible"));
    }

    #[test]
    fn five_openai_compatible_providers_all_survive_figment_merge() {
        let user_toml = r#"
            model = "deepseek/deepseek-v4-flash"

            [provider.openai]
            provider_id = "openai"
            kind = "openai-compatible"
            model_id = "gpt-5.5-pro"
            api_key_env = "OPENAI_API_KEY"
            enabled = true
            base_url = "https://api.openai.com/v1"
            discover_models = true

            [provider.openrouter]
            provider_id = "openrouter"
            kind = "openai-compatible"
            model_id = "deepseek/deepseek-v4-flash"
            api_key_env = "OPENROUTER_API_KEY"
            enabled = true
            base_url = "https://openrouter.ai/api/v1"
            discover_models = true

            [provider.deepseek]
            provider_id = "deepseek"
            kind = "openai-compatible"
            model_id = "deepseek-chat"
            api_key_env = "DEEPSEEK_API_KEY"
            base_url = "https://api.deepseek.com"
            enabled = true
            discover_models = true

            [provider.moonshot]
            provider_id = "moonshot"
            kind = "openai-compatible"
            model_id = "moonshot-v1-auto"
            api_key_env = "MOONSHOT_API_KEY"
            base_url = "https://api.kimi.com/coding/v1"
            enabled = true
            discover_models = true

            [provider.zhipu]
            provider_id = "zhipu"
            kind = "openai-compatible"
            model_id = "glm-4.7-flash"
            api_key_env = "ZAI_API_KEY"
            enabled = true
            base_url = "https://api.z.ai/api/coding/paas/v4"
            discover_models = true
            supports_tools = true
        "#;
        let config: AppConfig = figment_with_user_layer(user_toml)
            .extract()
            .expect("extract figment");

        for id in ["openai", "openrouter", "deepseek", "moonshot", "zhipu"] {
            let p = config
                .provider
                .get(id)
                .unwrap_or_else(|| panic!("provider '{id}' missing from merged config"));
            assert!(p.discover_models);
            assert_eq!(p.kind.as_deref(), Some("openai-compatible"));
        }
        assert_eq!(config.provider.len(), 5);
    }

    #[test]
    fn budget_snake_case_user_does_not_collide_with_defaults_layer() {
        let user_toml = r#"
            model = "test-model"
            [budget]
            daily_limit_usd = 10.00
        "#;
        let config: AppConfig = figment_with_user_layer(user_toml)
            .extract()
            .expect("merging defaults + user budget MUST NOT produce duplicate field");
        assert_eq!(config.budget.daily_limit_usd, Some(10.00));
    }

    #[test]
    fn empty_user_layer_produces_default_config() {
        let config: AppConfig = figment_with_user_layer("")
            .extract()
            .expect("extract figment from empty user layer");
        assert!(config.pricing.contains_key("claude-sonnet-4-6"));
        assert_eq!(config.model, "claude-sonnet-4-6");
    }

    /// Story 8.1 AC-1 — try_load with NoopProfileResolver returns defaults
    /// when no files exist (no panic, no error).
    #[test]
    fn try_load_no_files_returns_defaults() {
        let cli = test_cli();
        let resolver = NoopProfileResolver;
        let config =
            try_load(&cli, &resolver).expect("try_load with no files should return defaults");
        assert!(!config.model.is_empty());
        assert!(config.pricing.contains_key("claude-sonnet-4-6"));
    }

    /// Story 8.1 AC-6 — multi-layer field-level merge: defaults + TOML + env
    /// all contributing to the same pricing map.
    #[test]
    fn multi_layer_pricing_merge_with_figment() {
        let user_toml = r#"
            model = "toml-model"
            [pricing."claude-sonnet-4-6"]
            input_per_million = 4.00
        "#;

        let figment = Figment::from(Serialized::defaults(AppConfig::default()))
            .merge(Toml::string(user_toml))
            .merge(Env::prefixed("RUSTAIN_").split("__"));

        let config: AppConfig = figment.extract().expect("multi-layer extract");
        // TOML overrides defaults for sonnet input pricing (field-level merge)
        let sonnet = config.pricing.get("claude-sonnet-4-6").unwrap();
        assert_eq!(sonnet.input_per_million, 4.00);
        // Default catalog entries still present
        assert!(config.pricing.contains_key("gpt-4o"));
    }

    /// Story 8.1 AC-1 — CLI layer wins over all others.
    #[test]
    fn cli_layer_top_priority() {
        let cli = Cli {
            log_level: Some("debug".to_string()),
            config_file: None,
            model: None,
            ..test_cli()
        };

        let figment = Figment::from(Serialized::defaults(AppConfig::default()))
            .merge(Env::prefixed("RUSTAIN_").split("__"))
            .merge(Serialized::globals(CliOverrides::from(&cli)));

        let config: AppConfig = figment.extract().expect("extract");
        // CLI log_level overrides default (defaults has "warn", CLI has "debug")
        assert_eq!(config.log_level, "debug");
    }

    /// Story 8.1 AC-4 — JSON layer parses camelCase. The `model` key is the
    /// same in both snake_case and camelCase conventions, so it verifies JSON
    /// layer integration. Full camelCase alias coverage is AC-13 scope.
    #[test]
    fn json_layer_parses_camelcase_keys() {
        let json_str = r#"{"model": "json-model"}"#;
        let figment =
            Figment::from(Serialized::defaults(AppConfig::default())).merge(Json::string(json_str));

        let config: AppConfig = figment.extract().expect("extract JSON");
        assert_eq!(config.model, "json-model");
    }

    /// Story 8.1 AC-3 — env vars with double-underscore nesting.
    #[test]
    fn env_nesting_with_double_underscore_figment() {
        // SAFETY: cargo test runs single-threaded; no other test touches RUSTAIN_LOG_LEVEL.
        unsafe {
            std::env::set_var("RUSTAIN_LOG_LEVEL", "trace");
        }

        let figment = Figment::from(Serialized::defaults(AppConfig::default()))
            .merge(Env::prefixed("RUSTAIN_").split("__"));

        let config: AppConfig = figment.extract().expect("extract env");
        // Env layer overrides the default log_level
        assert_eq!(config.log_level, "trace");

        unsafe {
            std::env::remove_var("RUSTAIN_LOG_LEVEL");
        }
    }

    /// Story 8.1 AC-1 — full 7-layer priority chain: sets model in all layers,
    /// asserts CLI wins (highest-priority layer).
    #[test]
    fn test_layer_priority_full_chain() {
        // Layer 7: built-in defaults
        let mut figment = Figment::from(Serialized::defaults(AppConfig::default()));
        // Layer 6: profile defaults (via mock resolver returning a model)
        let profile_value = figment::value::Value::from(figment::value::Dict::from_iter(vec![(
            "model".to_string(),
            figment::value::Value::from("profile-model"),
        )]));
        figment = figment.merge(Serialized::defaults(profile_value));
        // Layer 5: user-global TOML (simulated via string)
        figment = figment.merge(Toml::string(r#"model = "user-toml-model""#));
        // Layer 4: workspace TOML
        figment = figment.merge(Toml::string(r#"model = "workspace-toml-model""#));
        // Layer 3: JSON local override
        figment = figment.merge(Json::string(r#"{"model": "json-override-model"}"#));
        // Layer 2: env vars
        unsafe {
            std::env::set_var("RUSTAIN_MODEL", "env-model");
        }
        figment = figment.merge(Env::prefixed("RUSTAIN_").split("__"));
        // Layer 1: CLI flags (TOP)
        let cli = Cli {
            model: Some("cli-model".to_string()),
            log_level: None,
            config_file: None,
            ..test_cli()
        };
        figment = figment.merge(Serialized::globals(CliOverrides::from(&cli)));

        let config: AppConfig = figment.extract().expect("full chain extract");
        assert_eq!(config.model, "cli-model", "CLI layer (highest) must win");

        unsafe {
            std::env::remove_var("RUSTAIN_MODEL");
        }
    }

    /// Story 8.1 AC-6 — multi-layer merge for provider: defaults + 2 TOML
    /// layers contribute different providers without whole-map replacement.
    #[test]
    fn multi_layer_provider_merge_preserves_all_entries() {
        let layer1 = r#"
            [provider.openai]
            provider_id = "openai"
            model_id = "gpt-4o"
            api_key_env = "OPENAI_API_KEY"
            kind = "openai-compatible"
            enabled = true
        "#;
        let layer2 = r#"
            [provider.openrouter]
            provider_id = "openrouter"
            model_id = "anthropic/claude-3.5-sonnet"
            api_key_env = "OPENROUTER_API_KEY"
            kind = "openai-compatible"
            enabled = true
        "#;
        let figment = Figment::from(Serialized::defaults(AppConfig::default()))
            .merge(Toml::string(layer1))
            .merge(Toml::string(layer2));

        let config: AppConfig = figment.extract().expect("provider multi-layer extract");
        assert!(config.provider.contains_key("openai"));
        assert!(config.provider.contains_key("openrouter"));
        assert!(config.provider["openai"].enabled);
        assert!(config.provider["openrouter"].enabled);
    }

    /// Story 8.1 AC-6 — multi-layer merge for step_tiers: defaults + user
    /// override merge without whole-map replacement.
    #[test]
    fn multi_layer_step_tiers_merge_preserves_other_entries() {
        use crate::domain::models::router::{ModelTier, StepKind};

        let user_toml = r#"
            [router.step_tiers]
            codegen = "flagship"
        "#;
        let figment = Figment::from(Serialized::defaults(AppConfig::default()))
            .merge(Toml::string(user_toml))
            .merge(Env::prefixed("RUSTAIN_").split("__"));

        let config: AppConfig = figment.extract().expect("step_tiers multi-layer extract");
        // User override: codegen → Flagship
        assert_eq!(
            config.router.step_tiers.get(&StepKind::Codegen),
            Some(&ModelTier::Flagship)
        );
        // Default entries survive
        assert!(config.router.step_tiers.contains_key(&StepKind::Plan));
        assert!(config.router.step_tiers.contains_key(&StepKind::Edit));
    }

    /// Story 8.1 AC-6 — multi-layer merge for tier_models: user entries
    /// are additive (default tier_models map is empty, so nothing to clobber).
    #[test]
    fn multi_layer_tier_models_merge_preserves_other_entries() {
        use crate::domain::models::router::ModelTier;

        let layer1 = r#"
            [router.tier_models]
            cheap_agentic = "gpt-4o"
        "#;
        let layer2 = r#"
            [router.tier_models]
            flagship = "claude-sonnet-4-6"
        "#;
        let figment = Figment::from(Serialized::defaults(AppConfig::default()))
            .merge(Toml::string(layer1))
            .merge(Toml::string(layer2));

        let config: AppConfig = figment.extract().expect("tier_models multi-layer extract");
        // Layer2 overrides cheap_agentic? No — cheap_agentic is in layer1 only.
        // Both entries from different layers survive field-level merge.
        assert_eq!(
            config
                .router
                .tier_models
                .get(&ModelTier::CheapAgentic)
                .map(|s| s.as_str()),
            Some("gpt-4o")
        );
        // Layer2 adds flagship
        assert_eq!(
            config
                .router
                .tier_models
                .get(&ModelTier::Flagship)
                .map(|s| s.as_str()),
            Some("claude-sonnet-4-6")
        );
    }

    #[test]
    fn config_override_parser_builds_typed_deep_merge_tree() {
        let pairs = vec![
            "provider.ollama.base_url=http://localhost:11434/v1".to_string(),
            "provider.ollama.model_id=llama3.2".to_string(),
            "router.threshold_tokens=100000".to_string(),
            "default_plan_mode=true".to_string(),
            "model=".to_string(),
            "base_url=http://host?key=val".to_string(),
        ];

        let value = parse_config_overrides(&pairs).expect("valid -c overrides");

        assert_eq!(
            value
                .pointer("/provider/ollama/base_url")
                .and_then(|v| v.as_str()),
            Some("http://localhost:11434/v1")
        );
        assert_eq!(
            value
                .pointer("/provider/ollama/model_id")
                .and_then(|v| v.as_str()),
            Some("llama3.2")
        );
        assert_eq!(
            value
                .pointer("/router/threshold_tokens")
                .and_then(|v| v.as_u64()),
            Some(100000)
        );
        assert_eq!(
            value
                .pointer("/default_plan_mode")
                .and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(value.pointer("/model").and_then(|v| v.as_str()), Some(""));
        assert_eq!(
            value.pointer("/base_url").and_then(|v| v.as_str()),
            Some("http://host?key=val")
        );
    }

    #[test]
    fn config_override_parser_rejects_credentials_and_malformed_only() {
        for pair in [
            "api_key_env=X",
            "provider.openai.api_key_env=X",
            "auth.token=X",
            "secret=X",
            "model",
            "=value",
            "..key=value",
            "key..sub=value",
        ] {
            assert!(
                parse_config_overrides(&[pair.to_string()]).is_err(),
                "{pair} must fail"
            );
        }

        for pair in [
            "provider.ollama.model_id=X",
            "provider.openai.base_url=http://localhost",
            "model=gpt-4o",
            "router.threshold_tokens=100000",
            "tokenizer=cl100k",
            "secretary=enabled",
            "credentials_note=metadata-only",
            "nonexistent.key=value",
        ] {
            assert!(
                parse_config_overrides(&[pair.to_string()]).is_ok(),
                "{pair} must pass"
            );
        }
    }

    #[test]
    fn config_override_layer_beats_typed_cli_flags() {
        let cli = Cli {
            model: Some("typed-model".to_string()),
            ..test_cli()
        };
        let overrides =
            parse_config_overrides(&["model=override-model".to_string()]).expect("override parses");

        let config: AppConfig = build_figment(&cli, &NoopProfileResolver, Some(&overrides))
            .extract()
            .expect("extract config");

        assert_eq!(config.model, "override-model");
    }
}
