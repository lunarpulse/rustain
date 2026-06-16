//! `auth list` handler (Story 13.4c).
//!
//! Enumerates **all** supported providers from the static table with auth
//! methods, configured status (env → auth.json precedence via the shared
//! `detect_source`), active-default marker (config-intent only), and signup
//! URLs.  Never builds a provider, never probes the network (AC5).

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use serde::Serialize;

use crate::adapters::cli::auth::providers;
use crate::domain::models::credential::{AuthMethod, ProviderStatus};
use crate::domain::ports::AuthStorePort;

pub const AUTH_LIST_SCHEMA_VERSION: &str = "1.0";

// ---------------------------------------------------------------------------
// Decision core
// ---------------------------------------------------------------------------

/// One presentation row.  No secret material — name/method/url/markers only.
#[derive(Debug, Clone)]
struct ListRow {
    provider: String,
    display_name: String,
    auth_methods: Vec<AuthMethod>,
    signup_url: String,
    requires_key: bool,
    /// `Some(true)` = credential found; `Some(false)` = absent; `None` = keyless.
    configured: Option<bool>,
    /// True when this provider is the active default (config-intent, Q1).
    is_default: bool,
}

/// Pure: combine the static provider table, an injected env lookup, the
/// auth.json entries, and the active-default id into the rows to render.
///
/// `env_lookup` is injected so tests assert configured/precedence WITHOUT
/// mutating the process environment.
fn build_list_rows(
    providers: &[providers::ProviderMeta],
    auth_json: &[ProviderStatus],
    env_lookup: impl Fn(&str) -> Option<String>,
    default_provider_id: &str,
) -> Vec<ListRow> {
    let auth_by_provider: HashMap<&str, &ProviderStatus> = auth_json
        .iter()
        .map(|s| (s.provider.as_str(), s))
        .collect();

    providers
        .iter()
        .map(|meta| {
            let configured = if meta.requires_key {
                Some(super::detect_source(meta, &auth_by_provider, &env_lookup).is_some())
            } else {
                // Keyless (e.g. ollama) — no credential concept (Q2).
                None
            };

            let auth_methods = if meta.requires_key {
                meta.auth_methods.to_vec()
            } else {
                // Keyless: ignore the placeholder `[ApiKey]` in providers.rs:82.
                Vec::new()
            };

            ListRow {
                provider: meta.id.to_string(),
                display_name: meta.display_name.to_string(),
                auth_methods,
                signup_url: meta.signup_url.to_string(),
                requires_key: meta.requires_key,
                configured,
                is_default: meta.id == default_provider_id,
            }
        })
        .collect()
}

/// Map `AuthMethod` variants to human-readable tokens.
/// `#[non_exhaustive]` catch-all keeps forward-compat for Epic 19.
fn auth_method_token(m: &crate::domain::models::credential::AuthMethod) -> &'static str {
    use crate::domain::models::credential::AuthMethod;
    match m {
        AuthMethod::ApiKey => "api-key",
        // Forward-compat catch-all for Epic 19 additions.
        #[allow(unreachable_patterns)]
        _ => "unknown",
    }
}

/// Map `AuthMethod` variants to snake_case JSON tokens.
fn auth_method_json_token(m: &crate::domain::models::credential::AuthMethod) -> &'static str {
    use crate::domain::models::credential::AuthMethod;
    match m {
        AuthMethod::ApiKey => "api_key",
        #[allow(unreachable_patterns)]
        _ => "unknown",
    }
}

// ---------------------------------------------------------------------------
// Active-default detection (AC3, AC5)
// ---------------------------------------------------------------------------

/// Mirror startup's active-provider selection WITHOUT building providers:
/// the lexicographically-first **enabled** `[provider]` entry (`BTreeMap`
/// iteration is deterministic — `config.rs:662-664`), else `"anthropic"`
/// (startup.rs:1728/1740).
///
/// Reports configured *intent*, NOT build-verified runtime fact (Q1).
/// `ProviderConfig.enabled` defaults to `true` (`default_enabled()`).
fn active_default_provider_id(cfg: &crate::domain::models::AppConfig) -> String {
    cfg.provider
        .iter()
        .find(|(_id, c)| c.enabled)
        .map(|(id, _)| id.clone())
        .unwrap_or_else(|| "anthropic".to_string())
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

/// Run `rustain auth list`.
///
/// # Errors
///
/// Returns `Err` only for auth-store read errors or JSON serialization failures.
/// It never builds a provider and never performs network validation (AC5).
pub async fn run_auth_list(
    json: bool,
    app_config: &crate::domain::models::AppConfig,
    store: &Arc<dyn AuthStorePort>,
) -> Result<()> {
    let entries = store.list().await?;
    let default_id = active_default_provider_id(app_config);
    let rows = build_list_rows(
        providers::all_providers(),
        &entries,
        |k| crate::infrastructure::utils::env_var_trimmed(k),
        &default_id,
    );

    if json {
        println!("{}", render_json(&rows)?);
    } else {
        println!("{}", render_human(&rows));
    }

    tracing::info!(
        subcommand = "auth-list",
        providers = rows.len(),
        configured = rows
            .iter()
            .filter(|r| r.configured == Some(true))
            .count()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Human rendering
// ---------------------------------------------------------------------------

fn render_human(rows: &[ListRow]) -> String {
    // Compute column widths.
    let provider_header = "PROVIDER";
    let methods_header = "AUTH METHODS";
    let configured_header = "CONFIGURED";
    let url_header = "SIGNUP URL";

    let provider_width = rows
        .iter()
        .map(|r| {
            let extra = if r.is_default { 2 } else { 0 }; // " *"
            r.display_name.chars().count() + extra
        })
        .max()
        .unwrap_or(0)
        .max(provider_header.len());

    let methods_width = rows
        .iter()
        .map(|r| human_methods(r).len())
        .max()
        .unwrap_or(0)
        .max(methods_header.len());

    let configured_width = configured_header.len();

    let mut out = String::new();
    out.push_str(&format!(
        "{:<pw$}  {:<mw$}  {:<cw$}  {}\n",
        provider_header,
        methods_header,
        configured_header,
        url_header,
        pw = provider_width,
        mw = methods_width,
        cw = configured_width,
    ));

    for row in rows {
        let name = if row.is_default {
            format!("{} *", row.display_name)
        } else {
            row.display_name.clone()
        };

        let methods = human_methods(row);
        let configured = match row.configured {
            Some(true) => "✓",
            Some(false) => "",
            None => "n/a",
        };

        out.push_str(&format!(
            "{:<pw$}  {:<mw$}  {:<cw$}  {}\n",
            name,
            methods,
            configured,
            row.signup_url,
            pw = provider_width,
            mw = methods_width,
            cw = configured_width,
        ));
    }

    out.push_str(
        "\n* = active default provider\n\
         Run `rustain auth status` for credential sources, \
         or `rustain auth login <provider>` to add one.",
    );
    out
}

fn human_methods(row: &ListRow) -> String {
    if !row.requires_key {
        return "none (local)".to_string();
    }
    row.auth_methods.iter().map(auth_method_token).collect::<Vec<_>>().join(", ")
}

// ---------------------------------------------------------------------------
// JSON rendering
// ---------------------------------------------------------------------------

fn render_json(rows: &[ListRow]) -> Result<String> {
    let output = ListJson {
        schema_version: AUTH_LIST_SCHEMA_VERSION,
        providers: rows.iter().map(ListProviderJson::from).collect(),
    };
    Ok(serde_json::to_string_pretty(&output)?)
}

#[derive(Serialize)]
struct ListJson<'a> {
    schema_version: &'a str,
    providers: Vec<ListProviderJson>,
}

#[derive(Serialize)]
struct ListProviderJson {
    provider: String,
    display_name: String,
    auth_methods: Vec<&'static str>,
    signup_url: String,
    requires_key: bool,
    configured: Option<bool>,
    is_default: bool,
}

impl From<&ListRow> for ListProviderJson {
    fn from(row: &ListRow) -> Self {
        let auth_methods = if row.requires_key {
            row.auth_methods
                .iter()
                .map(auth_method_json_token)
                .collect()
        } else {
            vec!["none"]
        };
        Self {
            provider: row.provider.clone(),
            display_name: row.display_name.clone(),
            auth_methods,
            signup_url: row.signup_url.clone(),
            requires_key: row.requires_key,
            configured: row.configured,
            is_default: row.is_default,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::credential::{AuthSource, AuthStatus, ProviderStatus};
    use crate::domain::models::{AppConfig, ProviderConfig};
    use chrono::{DateTime, Utc};

    fn status(provider: &str, last_validated: Option<DateTime<Utc>>) -> ProviderStatus {
        ProviderStatus {
            provider: provider.to_string(),
            status: AuthStatus::Authenticated,
            source: AuthSource::AuthJson,
            last_validated,
        }
    }

    fn fixed_time() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-06-15T22:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn make_provider_config(id: &str, enabled: bool) -> ProviderConfig {
        ProviderConfig {
            provider_id: id.to_string(),
            model_id: String::new(),
            api_key_env: String::new(),
            enabled,
            kind: None,
            base_url: None,
            context_window: None,
            supports_tools: None,
            discover_models: false,
            model_filter: vec!["*".to_string()],
            cache_ttl_seconds: 3600,
        }
    }

    fn make_config(providers: Vec<(&str, bool)>) -> AppConfig {
        let mut cfg = AppConfig::default();
        for (id, enabled) in providers {
            cfg.provider
                .insert(id.to_string(), make_provider_config(id, enabled));
        }
        cfg
    }

    // P0-1: Redaction canary
    #[test]
    fn redaction_canary_absent_from_human_and_json_outputs() {
        const SECRET: &str = "SECRET-CANARY-DEADBEEF";
        let rows = build_list_rows(
            providers::all_providers(),
            &[],
            |name| (name == "ANTHROPIC_API_KEY").then(|| SECRET.to_string()),
            "anthropic",
        );
        let human = render_human(&rows);
        let json = render_json(&rows).unwrap();

        // Positive control: provider id, ✓, signup URL, auth method present.
        assert!(human.contains("Anthropic"), "human missing provider name");
        assert!(human.contains("✓"), "human missing configured marker");
        assert!(
            human.contains("https://console.anthropic.com/"),
            "human missing signup URL"
        );
        assert!(human.contains("api-key"), "human missing auth method");

        // Canary: secret absent from all outputs.
        assert!(
            !human.contains(SECRET),
            "human output leaked secret: {human}"
        );
        assert!(!json.contains(SECRET), "json output leaked secret: {json}");
    }

    // P0-2: All 7 providers listed
    #[test]
    fn all_seven_providers_listed() {
        let rows = build_list_rows(providers::all_providers(), &[], |_| None, "anthropic");
        assert_eq!(rows.len(), 7, "expected 7 providers from static table");
        assert!(
            rows.iter().any(|r| r.provider == "ollama"),
            "keyless ollama should be present"
        );
    }

    // P0-3: Configured marker — positive AND negative control
    #[test]
    fn configured_marker_positive_and_negative_control() {
        let entries = vec![status("openai", Some(fixed_time()))];

        // Env present → configured = Some(true).
        let with_env = build_list_rows(
            providers::all_providers(),
            &entries,
            |name| (name == "OPENAI_API_KEY").then(|| "sk-env".to_string()),
            "anthropic",
        );
        let openai_env = with_env.iter().find(|r| r.provider == "openai").unwrap();
        assert_eq!(openai_env.configured, Some(true), "env present → configured");

        // Auth.json only → configured = Some(true).
        let json_only = build_list_rows(
            providers::all_providers(),
            &entries,
            |_| None,
            "anthropic",
        );
        let openai_json = json_only.iter().find(|r| r.provider == "openai").unwrap();
        assert_eq!(
            openai_json.configured,
            Some(true),
            "auth.json present → configured"
        );

        // Neither → configured = Some(false).
        let neither = build_list_rows(providers::all_providers(), &[], |_| None, "anthropic");
        let openai_none = neither.iter().find(|r| r.provider == "openai").unwrap();
        assert_eq!(
            openai_none.configured,
            Some(false),
            "no credential → not configured"
        );
    }

    // P0-4: Anthropic dual-token
    #[test]
    fn anthropic_dual_token_auth_token_counts() {
        let rows = build_list_rows(
            providers::all_providers(),
            &[],
            |name| (name == "ANTHROPIC_AUTH_TOKEN").then(|| "bearer-token".to_string()),
            "anthropic",
        );
        let anthropic = rows.iter().find(|r| r.provider == "anthropic").unwrap();
        assert_eq!(
            anthropic.configured,
            Some(true),
            "ANTHROPIC_AUTH_TOKEN should count"
        );
    }

    #[test]
    fn anthropic_dual_token_empty_whitespace_is_absent() {
        let rows = build_list_rows(
            providers::all_providers(),
            &[],
            |name| (name == "ANTHROPIC_AUTH_TOKEN").then(|| "   ".to_string()),
            "anthropic",
        );
        let anthropic = rows.iter().find(|r| r.provider == "anthropic").unwrap();
        assert_eq!(
            anthropic.configured,
            Some(false),
            "whitespace-only ANTHROPIC_AUTH_TOKEN should be absent"
        );
    }

    // P0-5a: Active-default — first-enabled wins, alphabetical
    #[test]
    fn active_default_first_enabled_alpha() {
        let cfg = make_config(vec![("openai", true), ("anthropic", true)]);
        let default_id = active_default_provider_id(&cfg);
        assert_eq!(default_id, "anthropic", "BTreeMap: anthropic < openai");

        let rows = build_list_rows(providers::all_providers(), &[], |_| None, &default_id);
        let default_count = rows.iter().filter(|r| r.is_default).count();
        assert_eq!(default_count, 1, "exactly one default");
        assert!(
            rows.iter()
                .find(|r| r.is_default)
                .unwrap()
                .provider
                == "anthropic"
        );
    }

    // P0-5b: Active-default — skips disabled
    #[test]
    fn active_default_skips_disabled() {
        let cfg = make_config(vec![("anthropic", false), ("openai", true)]);
        let default_id = active_default_provider_id(&cfg);
        assert_eq!(default_id, "openai");

        let rows = build_list_rows(providers::all_providers(), &[], |_| None, &default_id);
        let default_count = rows.iter().filter(|r| r.is_default).count();
        assert_eq!(default_count, 1);
    }

    // P0-5c: Active-default — anthropic fallback
    #[test]
    fn active_default_anthropic_fallback() {
        let cfg = make_config(vec![]);
        let default_id = active_default_provider_id(&cfg);
        assert_eq!(default_id, "anthropic");

        let rows = build_list_rows(providers::all_providers(), &[], |_| None, &default_id);
        let default_count = rows.iter().filter(|r| r.is_default).count();
        assert_eq!(default_count, 1);
    }

    // P0-5d: Enabled-flag-absent default (defaults to true)
    #[test]
    fn active_default_enabled_flag_absent_defaults_true() {
        // Deserialize a config where the [provider] section omits `enabled`.
        // Serde's default_enabled() must treat it as true, so deepseek is selected.
        let toml = r#"
[provider.deepseek]
provider_id = "deepseek"
model_id = "deepseek-chat"
api_key_env = "DEEPSEEK_API_KEY"
"#;
        let cfg: AppConfig = toml::from_str(toml).expect("deserialize config");
        let default_id = active_default_provider_id(&cfg);
        assert_eq!(
            default_id, "deepseek",
            "omitted enabled flag should default to true"
        );
    }

    // P0-6: Active-default OUT of table → zero stars
    #[test]
    fn active_default_out_of_table_zero_stars() {
        let cfg = make_config(vec![("custom-llm", true)]);
        let default_id = active_default_provider_id(&cfg);
        assert_eq!(default_id, "custom-llm");

        let rows = build_list_rows(providers::all_providers(), &[], |_| None, &default_id);
        assert_eq!(rows.len(), 7, "still 7 static rows");
        let default_count = rows.iter().filter(|r| r.is_default).count();
        assert_eq!(
            default_count, 0,
            "custom id not in table → zero default markers"
        );
    }

    // P0-7: Build-state independence (config-intent doc-lock)
    #[test]
    fn active_default_build_fail_still_stars() {
        // Config whose first-enabled would fail to build (bogus base_url / missing key).
        let mut cfg = AppConfig::default();
        let mut pc = make_provider_config("openai", true);
        pc.base_url = Some("http://bogus:1/fake".to_string());
        cfg.provider.insert("openai".to_string(), pc);
        let default_id = active_default_provider_id(&cfg);
        assert_eq!(default_id, "openai");

        let rows = build_list_rows(providers::all_providers(), &[], |_| None, &default_id);
        let openai_row = rows.iter().find(|r| r.provider == "openai").unwrap();
        assert!(openai_row.is_default, "config-intent: * even if build would fail");
    }

    // P0-8: --json shape + snake_case
    #[test]
    fn json_shape_snake_case_and_complete() {
        let entries = vec![status("anthropic", Some(fixed_time()))];
        let rows = build_list_rows(
            providers::all_providers(),
            &entries,
            |_| None,
            "anthropic",
        );
        let json_str = render_json(&rows).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json_str).unwrap();

        assert_eq!(value["schema_version"], "1.0");
        let providers_arr = value["providers"].as_array().unwrap();
        assert_eq!(providers_arr.len(), 7);

        let anthropic = &providers_arr[0];
        assert_eq!(anthropic["provider"], "anthropic");
        assert_eq!(anthropic["display_name"], "Anthropic");
        assert_eq!(anthropic["auth_methods"], serde_json::json!(["api_key"]));
        assert_eq!(anthropic["signup_url"], "https://console.anthropic.com/");
        assert_eq!(anthropic["requires_key"], true);
        assert_eq!(anthropic["configured"], true);
        assert_eq!(anthropic["is_default"], true);

        // No key/source fields.
        assert!(anthropic.get("api_key").is_none());
        assert!(anthropic.get("key").is_none());
        assert!(anthropic.get("source").is_none());

        // Keyless provider.
        let ollama = providers_arr.iter().find(|p| p["provider"] == "ollama").unwrap();
        assert!(
            ollama["configured"].is_null(),
            "keyless configured should be null, not false"
        );
        assert_eq!(ollama["auth_methods"], serde_json::json!(["none"]));
        assert_eq!(ollama["requires_key"], false);
        assert_eq!(ollama["is_default"], false);

        // Validate valid JSON parse.
        assert!(serde_json::from_str::<serde_json::Value>(&json_str).is_ok());
    }

    // P0-9: Keyless rendering
    #[test]
    fn keyless_rendering() {
        let rows = build_list_rows(providers::all_providers(), &[], |_| None, "anthropic");
        let ollama = rows.iter().find(|r| r.provider == "ollama").unwrap();
        assert_eq!(ollama.configured, None, "keyless → None");
        assert!(ollama.auth_methods.is_empty(), "keyless → empty auth_methods");

        // Negative guard: does NOT echo the placeholder ApiKey.
        assert!(
            !ollama.auth_methods.iter().any(|m| matches!(m, AuthMethod::ApiKey)),
            "keyless should not echo placeholder ApiKey"
        );

        // Human render.
        let human = render_human(&rows);
        assert!(human.contains("none (local)"), "human: 'none (local)' for keyless");
        assert!(human.contains("n/a"), "human: 'n/a' for keyless configured");

        // JSON render.
        let json_str = render_json(&rows).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let ollama_json = value["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["provider"] == "ollama")
            .unwrap();
        assert!(
            ollama_json["configured"].is_null(),
            "JSON: configured should be null, NOT false"
        );
        assert_eq!(ollama_json["auth_methods"], serde_json::json!(["none"]));
    }

    // P0-10: Shared-core parity backstop
    #[test]
    fn list_status_configured_parity() {
        use crate::adapters::cli::auth::status;

        // Fixed fixture: anthropic via env, openai via auth.json.
        let auth_entries = vec![ProviderStatus {
            provider: "openai".to_string(),
            status: AuthStatus::Authenticated,
            source: AuthSource::AuthJson,
            last_validated: Some(fixed_time()),
        }];
        let env_lookup = |name: &str| -> Option<String> {
            (name == "ANTHROPIC_API_KEY").then(|| "sk-test".to_string())
        };

        let list_rows = build_list_rows(
            providers::all_providers(),
            &auth_entries,
            env_lookup,
            "anthropic",
        );
        let status_rows = status::build_status_rows_for_test(
            providers::all_providers(),
            &auth_entries,
            env_lookup,
        );
        let status_configured: std::collections::HashSet<&str> = status_rows
            .iter()
            .map(|(pid, _)| pid.as_str())
            .collect();

        // For every key-requiring provider in the static table, list and status
        // must agree on configured-ness.
        for meta in providers::all_providers() {
            if !meta.requires_key {
                continue;
            }
            let list_configured = list_rows
                .iter()
                .find(|r| r.provider == meta.id)
                .map(|r| r.configured == Some(true))
                .unwrap_or(false);
            let status_configured = status_configured.contains(meta.id);
            assert_eq!(
                list_configured, status_configured,
                "parity: {} configured mismatch — list={}, status={}",
                meta.id, list_configured, status_configured
            );
        }
    }

    // P0-12: Offline-safe — no provider construction
    #[test]
    fn list_module_does_not_call_network_or_provider_builders() {
        let source = include_str!("list.rs");
        let probe = ["connectivity", "_probe"].concat();
        let builder = ["build_provider", "_for_config"].concat();
        let init = ["init_provider", "_layer"].concat();
        assert!(!source.contains(&probe));
        assert!(!source.contains(&builder));
        assert!(!source.contains(&init));
    }
}
