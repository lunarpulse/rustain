//! `auth status` handler (Story 13.4b).
//!
//! Read-only reporter over the 13.4a auth store.  Mirrors the live
//! env -> auth.json resolution order without constructing providers or probing
//! the network.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::adapters::cli::auth::providers;
use crate::domain::models::credential::{AuthSource, AuthStatus, ProviderStatus};
use crate::domain::ports::AuthStorePort;

pub const AUTH_STATUS_SCHEMA_VERSION: &str = "1.0";
const NO_PROVIDERS_MESSAGE: &str =
    "No providers configured. Run `rustain auth login <provider>` to add one.";

/// One presentation row. No secret material — presence/source/timestamp only.
#[derive(Debug, Clone)]
struct StatusRow {
    provider: String,
    display_name: String,
    status: AuthStatus,
    source: AuthSource,
    last_validated: Option<DateTime<Utc>>,
    requires_key: bool,
}

/// Run `rustain auth status`.
///
/// # Errors
///
/// Returns `Err` only for auth-store read errors or JSON serialization failures.
/// It never builds a provider and never performs network validation.
pub async fn run_auth_status(json: bool, store: &Arc<dyn AuthStorePort>) -> Result<()> {
    let entries = store.list().await?;
    let rows = build_status_rows(providers::all_providers(), &entries, |key| {
        crate::infrastructure::utils::env_var_trimmed(key)
    });

    if json {
        println!("{}", render_json(&rows)?);
    } else if rows.is_empty() {
        println!("{NO_PROVIDERS_MESSAGE}");
    } else {
        println!("{}", render_human(&rows));
    }

    tracing::info!(subcommand = "auth-status", configured = rows.len());
    Ok(())
}

/// Pure: combine the static provider table, an injected env lookup, and the
/// auth.json entries (from `AuthStorePort::list`) into rows to render.
///
/// `env_lookup(env_var_name) -> Option<String>` is injected so tests assert
/// env precedence without mutating process environment.
fn build_status_rows(
    providers: &[providers::ProviderMeta],
    auth_json: &[ProviderStatus],
    env_lookup: impl Fn(&str) -> Option<String>,
) -> Vec<StatusRow> {
    let auth_by_provider: HashMap<&str, &ProviderStatus> = auth_json
        .iter()
        .map(|status| (status.provider.as_str(), status))
        .collect();
    let known_ids: HashSet<&str> = providers.iter().map(|provider| provider.id).collect();
    let mut rows = Vec::new();

    for meta in providers {
        if !meta.requires_key {
            continue;
        }

        if let Some((source, last_validated)) =
            super::detect_source(meta, &auth_by_provider, &env_lookup)
        {
            let status = if last_validated.is_some() {
                AuthStatus::Authenticated
            } else {
                AuthStatus::Unknown
            };
            rows.push(StatusRow {
                provider: meta.id.to_string(),
                display_name: meta.display_name.to_string(),
                status,
                source,
                last_validated,
                requires_key: meta.requires_key,
            });
        }
    }
    let unknown_ids: BTreeSet<&str> = auth_json
        .iter()
        .map(|status| status.provider.as_str())
        .filter(|provider| !known_ids.contains(provider))
        .collect();
    for provider in unknown_ids {
        if let Some(stored) = auth_by_provider.get(provider) {
            rows.push(auth_json_row(
                provider,
                provider,
                true,
                stored.last_validated,
            ));
        }
    }

    rows
}

/// Test-only: expose `build_status_rows` result for cross-module parity checks.
/// Returns `(provider_id, requires_key)` pairs for configured providers.
#[cfg(test)]
pub fn build_status_rows_for_test(
    providers: &[providers::ProviderMeta],
    auth_json: &[ProviderStatus],
    env_lookup: impl Fn(&str) -> Option<String>,
) -> Vec<(String, bool)> {
    build_status_rows(providers, auth_json, env_lookup)
        .into_iter()
        .map(|r| (r.provider, r.requires_key))
        .collect()
}

fn auth_json_row(
    provider: &str,
    display_name: &str,
    requires_key: bool,
    last_validated: Option<DateTime<Utc>>,
) -> StatusRow {
    StatusRow {
        provider: provider.to_string(),
        display_name: display_name.to_string(),
        status: if last_validated.is_some() {
            AuthStatus::Authenticated
        } else {
            AuthStatus::Unknown
        },
        source: AuthSource::AuthJson,
        last_validated,
        requires_key,
    }
}

fn render_human(rows: &[StatusRow]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{:<16} {:<6} {:<10} {}\n",
        "PROVIDER", "STATUS", "SOURCE", "LAST VALIDATED"
    ));
    for row in rows {
        out.push_str(&format!(
            "{:<16} {:<6} {:<10} {}\n",
            truncate_str(&row.display_name, 16),
            status_glyph(&row.status),
            source_human_label(&row.source),
            row.last_validated
                .map(|ts| ts.to_rfc3339())
                .unwrap_or_else(|| "—".to_string())
        ));
    }
    out.trim_end().to_string()
}

fn render_json(rows: &[StatusRow]) -> Result<String> {
    let output = StatusJson {
        schema_version: AUTH_STATUS_SCHEMA_VERSION,
        providers: rows.iter().map(StatusProviderJson::from).collect(),
    };
    Ok(serde_json::to_string_pretty(&output)?)
}

#[derive(Serialize)]
struct StatusJson<'a> {
    schema_version: &'a str,
    providers: Vec<StatusProviderJson>,
}

#[derive(Serialize)]
struct StatusProviderJson {
    provider: String,
    status: &'static str,
    source: &'static str,
    last_validated: Option<String>,
    requires_key: bool,
}

impl From<&StatusRow> for StatusProviderJson {
    fn from(row: &StatusRow) -> Self {
        Self {
            provider: row.provider.clone(),
            status: status_json_token(&row.status),
            source: source_json_token(&row.source),
            last_validated: row.last_validated.map(|ts| ts.to_rfc3339()),
            requires_key: row.requires_key,
        }
    }
}

fn status_glyph(status: &AuthStatus) -> &'static str {
    match status {
        AuthStatus::Authenticated => "✓",
        AuthStatus::Unknown => "⚠",
        AuthStatus::Invalid => "✗",
    }
}

fn status_json_token(status: &AuthStatus) -> &'static str {
    match status {
        AuthStatus::Authenticated => "authenticated",
        AuthStatus::Unknown => "unknown",
        AuthStatus::Invalid => "invalid",
    }
}

fn source_human_label(source: &AuthSource) -> &'static str {
    match source {
        AuthSource::Env => "env",
        AuthSource::AuthJson => "auth.json",
        AuthSource::Config => "config",
        AuthSource::None => "none",
    }
}

fn source_json_token(source: &AuthSource) -> &'static str {
    match source {
        AuthSource::Env => "env",
        AuthSource::AuthJson => "auth_json",
        AuthSource::Config => "config",
        AuthSource::None => "none",
    }
}

fn truncate_str(s: &str, max_len: usize) -> String {
    if s.chars().count() <= max_len {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_len.saturating_sub(1)).collect();
        format!("{}\u{2026}", truncated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn env_precedence_positive_and_negative_controls() {
        let entries = vec![status("openai", Some(fixed_time()))];
        let with_env = build_status_rows(providers::all_providers(), &entries, |name| {
            (name == "OPENAI_API_KEY").then(|| "sk-env".to_string())
        });
        assert!(matches!(with_env[0].source, AuthSource::Env));
        assert!(matches!(with_env[0].status, AuthStatus::Unknown));
        assert!(with_env[0].last_validated.is_none());

        let without_env = build_status_rows(providers::all_providers(), &entries, |_| None);
        assert!(matches!(without_env[0].source, AuthSource::AuthJson));
        assert!(matches!(without_env[0].status, AuthStatus::Authenticated));
        assert_eq!(without_env[0].last_validated, Some(fixed_time()));
    }

    #[test]
    fn anthropic_auth_token_counts_as_env_credential() {
        let rows = build_status_rows(providers::all_providers(), &[], |name| {
            (name == "ANTHROPIC_AUTH_TOKEN").then(|| "bearer-token".to_string())
        });
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].provider, "anthropic");
        assert!(matches!(rows[0].source, AuthSource::Env));
    }

    #[test]
    fn empty_env_values_are_absent() {
        let entries = vec![status("anthropic", Some(fixed_time()))];
        let rows = build_status_rows(providers::all_providers(), &entries, |name| {
            (name == "ANTHROPIC_AUTH_TOKEN").then(|| "   ".to_string())
        });
        assert!(matches!(rows[0].source, AuthSource::AuthJson));
    }

    #[test]
    fn empty_status_has_message_and_json_empty_array() {
        let rows = build_status_rows(providers::all_providers(), &[], |_| None);
        assert!(rows.is_empty());
        assert!(NO_PROVIDERS_MESSAGE.contains("auth login"));

        let json = render_json(&rows).unwrap();
        assert!(json.contains("\"schema_version\": \"1.0\""));
        assert!(json.contains("\"providers\": []"));
    }

    #[test]
    fn last_validated_maps_status_and_human_rendering() {
        let entries = vec![
            status("anthropic", Some(fixed_time())),
            status("openai", None),
        ];
        let rows = build_status_rows(providers::all_providers(), &entries, |_| None);
        assert!(matches!(rows[0].status, AuthStatus::Authenticated));
        assert!(matches!(rows[1].status, AuthStatus::Unknown));

        let human = render_human(&rows);
        assert!(human.contains("✓"));
        assert!(human.contains("⚠"));
        assert!(human.contains("2026-06-15T22:00:00+00:00"));
        assert!(human.contains("—"));
    }

    #[test]
    fn keyless_provider_is_omitted_when_unconfigured() {
        let rows = build_status_rows(providers::all_providers(), &[], |_| None);
        assert!(!rows.iter().any(|row| row.provider == "ollama"));
    }

    #[test]
    fn unknown_stored_provider_is_rendered() {
        let rows = build_status_rows(
            providers::all_providers(),
            &[status("custom-provider", Some(fixed_time()))],
            |_| None,
        );
        let row = rows
            .iter()
            .find(|row| row.provider == "custom-provider")
            .unwrap();
        assert_eq!(row.display_name, "custom-provider");
        assert!(row.requires_key);
        assert!(matches!(row.source, AuthSource::AuthJson));
    }

    #[test]
    fn json_shape_is_snake_case_and_has_no_key_field() {
        let rows = build_status_rows(
            providers::all_providers(),
            &[status("anthropic", Some(fixed_time()))],
            |_| None,
        );
        let json = render_json(&rows).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["schema_version"], "1.0");
        assert_eq!(value["providers"][0]["provider"], "anthropic");
        assert_eq!(value["providers"][0]["status"], "authenticated");
        assert_eq!(value["providers"][0]["source"], "auth_json");
        assert_eq!(value["providers"][0]["requires_key"], true);
        assert!(value["providers"][0].get("api_key").is_none());
        assert!(value["providers"][0].get("key").is_none());
    }

    #[test]
    fn redaction_canary_absent_from_human_and_json_outputs() {
        const SECRET: &str = "SECRET-CANARY-DEADBEEF";
        let rows = build_status_rows(providers::all_providers(), &[], |name| {
            (name == "ANTHROPIC_API_KEY").then(|| SECRET.to_string())
        });
        let human = render_human(&rows);
        let json = render_json(&rows).unwrap();

        assert!(human.contains("Anthropic"));
        assert!(human.contains("env"));
        assert!(human.contains("⚠"));
        assert!(json.contains("anthropic"));
        assert!(json.contains("env"));
        assert!(
            !human.contains(SECRET),
            "human output leaked secret: {human}"
        );
        assert!(!json.contains(SECRET), "json output leaked secret: {json}");
    }

    #[test]
    fn status_module_does_not_call_network_or_provider_builders() {
        let source = include_str!("status.rs");
        let probe = ["connectivity", "_probe"].concat();
        let builder = ["build_provider", "_for_config"].concat();
        assert!(!source.contains(&probe));
        assert!(!source.contains(&builder));
    }
}
