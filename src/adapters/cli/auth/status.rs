//! `auth status` handler (Story 13.4b).
//!
//! Offline provenance reporter over the shared credential cascade.
//!
//! Grant-lifetime display adapted from oh-my-pi.
//!
//! MIT License
//!
//! Copyright (c) 2025 Mario Zechner
//! Copyright (c) 2025-2026 Can Bölük
//! Copyright (c) 2026 Stencil Labs, Inc.
//!
//! Permission is hereby granted, free of charge, to any person obtaining a copy
//! of this software and associated documentation files (the "Software"), to deal
//! in the Software without restriction, including without limitation the rights
//! to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
//! copies of the Software, and to permit persons to whom the Software is
//! furnished to do so, subject to the following conditions:
//!
//! The above copyright notice and this permission notice shall be included in all
//! copies or substantial portions of the Software.
//!
//! THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
//! IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
//! FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
//! AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
//! LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
//! OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
//! SOFTWARE.

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::adapters::cli::auth::providers;
use crate::domain::models::credential::{AuthSnapshot, AuthSource, AuthStatus};
use crate::domain::ports::AuthStorePort;
use crate::domain::services::key_cascade::SelectionMaterial;

pub const AUTH_STATUS_SCHEMA_VERSION: &str = "2.0";
const NO_PROVIDERS_MESSAGE: &str =
    "No providers configured. Run `rustain auth login <provider>` to add one.";

const ANTHROPIC_OAUTH_GRANT_TTL_MS: i64 = 30 * 24 * 60 * 60 * 1000;
const RELOGIN_WARN_WINDOW_MS: i64 = 7 * 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, Serialize)]
struct OAuthGrant {
    who: String,
    grant_expires_at: DateTime<Utc>,
}

/// One presentation row. No secret material — presence/source/timestamp only.
#[derive(Debug, Clone)]
struct StatusRow {
    provider: String,
    display_name: String,
    status: AuthStatus,
    source: Option<AuthSource>,
    disabled_cause: Option<String>,
    last_validated: Option<DateTime<Utc>>,
    requires_key: bool,
    oauth_grants: Vec<OAuthGrant>,
}

/// Run `rustain auth status`.
///
/// # Errors
///
/// Returns `Err` only for JSON serialization failures; store errors are warnings.
/// It never builds a provider and never performs network validation.
pub async fn run_auth_status(
    json: bool,
    store: &Arc<dyn AuthStorePort>,
    config: &crate::domain::models::AppConfig,
) -> Result<()> {
    let (entries, store_error) = super::snapshot_or_warn(store).await;
    if let Some(error) = &store_error {
        eprintln!("warning: auth.json unreadable; stored credentials ignored: {error}");
    }
    let rows = build_status_rows(
        providers::all_providers(),
        &entries,
        &config.provider,
        |key| crate::infrastructure::utils::env_var_trimmed(key),
    );

    if json {
        println!("{}", render_json(&rows, store_error.as_deref())?);
    } else if rows.is_empty() {
        println!("{NO_PROVIDERS_MESSAGE}");
    } else {
        println!("{}", render_human(&rows));
    }

    tracing::info!(subcommand = "auth-status", configured = rows.len());
    Ok(())
}

/// Combine the static provider table and snapshot without provider construction.
fn build_status_rows(
    providers: &[providers::ProviderMeta],
    snapshot: &AuthSnapshot,
    configured: &std::collections::BTreeMap<String, crate::domain::models::ProviderConfig>,
    env_lookup: impl Fn(&str) -> Option<String>,
) -> Vec<StatusRow> {
    let known_ids: HashSet<&str> = providers.iter().map(|p| p.id).collect();
    let mut rows = Vec::new();
    let mut append = |id: &str, display: &str, default_env: &str| {
        let selected =
            super::source_for_configured(id, default_env, configured, snapshot, &env_lookup);
        let (source, status, last_validated, disabled_cause) = if let Some(selection) = selected {
            let stale = matches!(selection.material, SelectionMaterial::NeedsRefresh { .. });
            (
                Some(selection.source),
                if !stale && selection.last_validated.is_some() {
                    AuthStatus::Authenticated
                } else {
                    AuthStatus::Unknown
                },
                selection.last_validated,
                None,
            )
        } else if let Some(row) = snapshot
            .rows(id)
            .iter()
            .find(|row| row.disabled_cause.is_some())
        {
            (
                None,
                AuthStatus::Invalid,
                row.last_validated,
                row.disabled_cause.clone(),
            )
        } else if id == "anthropic"
            && snapshot.rows(id).iter().any(|row| {
                row.disabled_cause.is_none()
                    && matches!(row.credential, crate::domain::models::Credential::OAuth(_))
            })
        {
            // Grant lifetime reporting is independent of endpoint permission.
            (None, AuthStatus::Unknown, None, None)
        } else {
            return;
        };
        rows.push(StatusRow {
            provider: id.to_owned(),
            display_name: display.to_owned(),
            source,
            status,
            last_validated,
            disabled_cause,
            requires_key: true,
            oauth_grants: if id == "anthropic" {
                snapshot
                    .rows(id)
                    .iter()
                    .filter(|row| row.disabled_cause.is_none())
                    .filter_map(|row| {
                        let crate::domain::models::Credential::OAuth(oauth) = &row.credential
                        else {
                            return None;
                        };
                        Some(OAuthGrant {
                            who: super::login::oauth_who(&oauth.identity)
                                .unwrap_or_else(|| display.to_owned()),
                            grant_expires_at: DateTime::from_timestamp_millis(
                                oauth
                                    .authorized_at_ms?
                                    .saturating_add(ANTHROPIC_OAUTH_GRANT_TTL_MS),
                            )?,
                        })
                    })
                    .collect()
            } else {
                Vec::new()
            },
        });
    };
    for meta in providers.iter().filter(|meta| meta.requires_key) {
        append(meta.id, meta.display_name, meta.api_key_env);
    }
    for id in snapshot
        .providers
        .keys()
        .chain(configured.keys())
        .filter(|id| !known_ids.contains(id.as_str()))
        .collect::<std::collections::BTreeSet<_>>()
    {
        append(id, id, "");
    }
    rows
}

/// Test-only: expose `build_status_rows` result for cross-module parity checks.
/// Returns `(provider_id, requires_key)` pairs for configured providers.
#[cfg(test)]
pub fn build_status_rows_for_test(
    providers: &[providers::ProviderMeta],
    auth_json: &AuthSnapshot,
    env_lookup: impl Fn(&str) -> Option<String>,
) -> Vec<(String, bool)> {
    build_status_rows(providers, auth_json, &Default::default(), env_lookup)
        .into_iter()
        .map(|r| (r.provider, r.requires_key))
        .collect()
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
    let now = crate::infrastructure::utils::auth_now_ms();
    for grant in rows.iter().flat_map(|row| &row.oauth_grants) {
        let remaining = grant
            .grant_expires_at
            .timestamp_millis()
            .saturating_sub(now);
        if remaining > RELOGIN_WARN_WINDOW_MS {
            continue;
        }
        let line = if remaining <= 0 {
            format!(
                "  ⚠ {} — grant is past Anthropic's ~30d lifetime; re-login now\n",
                grant.who
            )
        } else {
            format!(
                "  ⚠ {} — re-login within {} (Anthropic expires OAuth grants ~30d after login)\n",
                grant.who,
                format_duration(remaining)
            )
        };
        out.push_str(&line);
    }
    out.trim_end().to_string()
}

// omp formatDuration: whole larger units, rounded tenths below one minute.
fn format_duration(ms: i64) -> String {
    if ms <= 0 {
        return "0ms".into();
    }
    if ms < 1000 {
        return format!("{ms}ms");
    }
    if ms < 60_000 {
        let tenths = (ms + 50) / 100;
        return if tenths < 600 {
            format!("{}.{:01}s", tenths / 10, tenths % 10)
        } else {
            "1m".into()
        };
    }
    let (unit, remainder_unit, label, remainder_label) = if ms >= 86_400_000 {
        (86_400_000, 3_600_000, "d", "h")
    } else if ms >= 3_600_000 {
        (3_600_000, 60_000, "h", "m")
    } else {
        (60_000, 1000, "m", "s")
    };
    let whole = ms / unit;
    let remainder = ms % unit / remainder_unit;
    if remainder > 0 {
        format!("{whole}{label}{remainder}{remainder_label}")
    } else {
        format!("{whole}{label}")
    }
}

fn render_json(rows: &[StatusRow], store_error: Option<&str>) -> Result<String> {
    let output = StatusJson {
        schema_version: AUTH_STATUS_SCHEMA_VERSION,
        providers: rows.iter().map(StatusProviderJson::from).collect(),
        store_error,
    };
    Ok(serde_json::to_string_pretty(&output)?)
}

#[derive(Serialize)]
struct StatusJson<'a> {
    schema_version: &'a str,
    providers: Vec<StatusProviderJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    store_error: Option<&'a str>,
}

#[derive(Serialize)]
struct StatusProviderJson {
    provider: String,
    status: &'static str,
    source: &'static str,
    last_validated: Option<String>,
    requires_key: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    disabled_cause: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    oauth_grants: Vec<OAuthGrant>,
}

impl From<&StatusRow> for StatusProviderJson {
    fn from(row: &StatusRow) -> Self {
        Self {
            provider: row.provider.clone(),
            status: status_json_token(&row.status),
            source: source_json_token(&row.source),
            last_validated: row.last_validated.map(|ts| ts.to_rfc3339()),
            requires_key: row.requires_key,
            disabled_cause: row.disabled_cause.clone(),
            oauth_grants: row.oauth_grants.clone(),
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

fn source_human_label(source: &Option<AuthSource>) -> &'static str {
    match source {
        Some(AuthSource::Runtime) => "runtime",
        Some(AuthSource::StoredOAuth) => "oauth",
        Some(AuthSource::LoginKey) => "login",
        Some(AuthSource::Env { .. }) => "env",
        Some(AuthSource::StoredKey) => "stored",
        None => "disabled",
    }
}

fn source_json_token(source: &Option<AuthSource>) -> &'static str {
    match source {
        Some(AuthSource::Runtime) => "runtime",
        Some(AuthSource::StoredOAuth) => "oauth",
        Some(AuthSource::LoginKey) => "login_key",
        Some(AuthSource::Env { .. }) => "env",
        Some(AuthSource::StoredKey) => "stored_key",
        None => "disabled",
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

    fn status(
        provider: &str,
        last_validated: Option<DateTime<Utc>>,
    ) -> (String, crate::domain::models::StoredCredential) {
        use crate::domain::models::{
            Credential, CredentialGeneration, CredentialId, CredentialOrigin, SecretString,
            StoredCredential,
        };
        (
            provider.to_owned(),
            StoredCredential {
                id: CredentialId(1),
                rev: 1,
                generation: CredentialGeneration([1; 16]),
                origin: CredentialOrigin::Stored,
                credential: Credential::ApiKey(SecretString::new(
                    "stored-status-secret".to_owned(),
                )),
                last_validated,
                disabled_cause: None,
            },
        )
    }

    fn build_status_rows(
        providers: &[providers::ProviderMeta],
        entries: &[(String, crate::domain::models::StoredCredential)],
        env_lookup: impl Fn(&str) -> Option<String>,
    ) -> Vec<StatusRow> {
        let mut snapshot = AuthSnapshot::default();
        for (provider, row) in entries {
            snapshot
                .providers
                .entry(provider.clone())
                .or_default()
                .push(row.clone());
        }
        super::build_status_rows(providers, &snapshot, &Default::default(), env_lookup)
    }

    #[test]
    fn disabled_and_stale_oauth_are_literal_and_secret_free() {
        use crate::domain::models::{
            Credential, CredentialIdentity, OAuthCredential, SecretString,
        };
        let mut disabled = status("openai", Some(fixed_time()));
        disabled.1.disabled_cause = Some("oauth refresh failed: revoked".to_owned());
        let mut stale = status("anthropic", Some(fixed_time()));
        stale.1.credential = Credential::OAuth(OAuthCredential {
            access: SecretString::new("status-stale-access".to_owned()),
            refresh: SecretString::new("status-stale-refresh".to_owned()),
            expires_at_ms: 0,
            identity: CredentialIdentity::default(),
            authorized_at_ms: None,
        });
        let rows = build_status_rows(providers::all_providers(), &[disabled, stale], |_| None);
        let human = render_human(&rows);
        let json = render_json(&rows, None).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["schema_version"], "2.0");
        assert_eq!(value["providers"][0]["source"], "oauth");
        assert_eq!(value["providers"][0]["status"], "unknown");
        assert_eq!(value["providers"][1]["source"], "disabled");
        assert_eq!(value["providers"][1]["status"], "invalid");
        assert_eq!(
            value["providers"][1]["disabled_cause"],
            "oauth refresh failed: revoked"
        );
        assert!(human.contains("oauth") && human.contains("⚠"));
        assert!(human.contains("disabled") && human.contains("✗"));
        for secret in [
            "stored-status-secret",
            "status-stale-access",
            "status-stale-refresh",
        ] {
            assert!(!human.contains(secret));
            assert!(!json.contains(secret));
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
        assert!(matches!(with_env[0].source, Some(AuthSource::Env { .. })));
        assert!(matches!(with_env[0].status, AuthStatus::Unknown));
        assert!(with_env[0].last_validated.is_none());

        let without_env = build_status_rows(providers::all_providers(), &entries, |_| None);
        assert!(matches!(without_env[0].source, Some(AuthSource::StoredKey)));
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
        assert!(matches!(rows[0].source, Some(AuthSource::Env { .. })));
    }

    #[test]
    fn empty_env_values_are_absent() {
        let entries = vec![status("anthropic", Some(fixed_time()))];
        let rows = build_status_rows(providers::all_providers(), &entries, |name| {
            (name == "ANTHROPIC_AUTH_TOKEN").then(|| "   ".to_string())
        });
        assert!(matches!(rows[0].source, Some(AuthSource::StoredKey)));
    }

    #[test]
    fn empty_status_has_message_and_json_empty_array() {
        let rows = build_status_rows(providers::all_providers(), &[], |_| None);
        assert!(rows.is_empty());
        assert!(NO_PROVIDERS_MESSAGE.contains("auth login"));

        let json = render_json(&rows, None).unwrap();
        assert!(json.contains("\"schema_version\": \"2.0\""));
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
        assert!(matches!(row.source, Some(AuthSource::StoredKey)));
    }

    #[test]
    fn json_shape_is_snake_case_and_has_no_key_field() {
        let rows = build_status_rows(
            providers::all_providers(),
            &[status("anthropic", Some(fixed_time()))],
            |_| None,
        );
        let json = render_json(&rows, None).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["schema_version"], "2.0");
        assert_eq!(value["providers"][0]["provider"], "anthropic");
        assert_eq!(value["providers"][0]["status"], "authenticated");
        assert_eq!(value["providers"][0]["source"], "stored_key");
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
        let json = render_json(&rows, None).unwrap();

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
