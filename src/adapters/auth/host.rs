//! What every login host (the `auth login` CLI and the TUI sign-in overlay)
//! shares: the pre-flow plan, the sign-in targets, and the exact result lines.
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

use crate::adapters::auth::flows::LoginResult;
use crate::adapters::cli::auth::providers;
use crate::domain::models::{
    AppConfig, AuthMethod, AuthSnapshot, CredentialId, CredentialIdentity, ProviderConfig,
};
use crate::infrastructure::auth_accounts::{
    AccountKind, AccountRow, ProviderAccounts, ServedSource,
};
use crate::infrastructure::model_discovery::RefreshReport;
use anyhow::{Result, bail};

/// The pre-flow decisions every login host shares: display name, keyless
/// providers, the methods in picker order and the validation config.
pub struct LoginPlan {
    pub provider_id: String,
    pub display: String,
    pub keyless: bool,
    pub methods: &'static [AuthMethod],
    pub validation_config: ProviderConfig,
}
static API_KEY_ONLY: [AuthMethod; 1] = [AuthMethod::ApiKey];
pub fn plan(provider_id: &str, app: &AppConfig) -> Result<LoginPlan> {
    let (display, requires_key, validation_config) = resolve_provider(provider_id, app)?;
    Ok(LoginPlan {
        provider_id: provider_id.to_owned(),
        display,
        keyless: !requires_key,
        methods: providers::lookup(provider_id).map_or(&API_KEY_ONLY[..], |m| m.auth_methods),
        validation_config,
    })
}
pub fn keyless_line(display: &str) -> String {
    format!("{display} does not require an API key — no credentials to store.")
}
pub fn credentials_saved_line() -> Result<String> {
    Ok(format!(
        "Credentials saved to {}",
        crate::infrastructure::paths::data_dir()?
            .join("auth.json")
            .display()
    ))
}
/// One sign-in target: a key-requiring built-in some enabled entry reads, or a
/// configured provider whose rows no built-in owns and that names an API-key
/// variable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginTarget {
    /// The store key `auth login` takes.
    pub id: String,
    pub display: String,
    /// Enabled configured entries whose credentials this target signs in.
    pub entries: Vec<String>,
}
/// Sign-in targets in picker order: built-ins in table order, then configured
/// providers by display name. Keyless providers are not listed, nor is a
/// built-in no enabled entry reads (disabled in configuration, or shadowed by
/// an enabled kind alias with its own rows): nothing would use its login. An
/// explicit `auth login <id>` / `/login <id>` still signs it in.
pub fn login_targets(app: &AppConfig) -> Vec<LoginTarget> {
    let entries = crate::infrastructure::provider_availability::effective_providers(app);
    let readers = |id: &str| {
        entries
            .iter()
            .filter(|entry| entry.cfg.enabled && entry.rows_key() == id)
            .map(|entry| entry.id.clone())
            .collect::<Vec<_>>()
    };
    let mut targets: Vec<_> = providers::all_providers()
        .iter()
        .filter(|meta| meta.requires_key)
        .map(|meta| LoginTarget {
            id: meta.id.into(),
            display: meta.display_name.into(),
            entries: readers(meta.id),
        })
        .filter(|target| !target.entries.is_empty())
        .collect();
    let mut configured: Vec<_> = entries
        .iter()
        .filter(|entry| {
            entry.cfg.enabled
                && providers::lookup(entry.rows_key()).is_none()
                && !entry.cfg.api_key_env.trim().is_empty()
        })
        .map(|entry| LoginTarget {
            id: entry.rows_key().into(),
            display: entry.display_name(),
            entries: readers(entry.rows_key()),
        })
        .collect();
    configured.sort_by(|a, b| a.display.cmp(&b.display).then(a.id.cmp(&b.id)));
    configured.dedup_by(|a, b| a.id == b.id);
    targets.extend(configured);
    targets
}
pub fn oauth_who(identity: &CredentialIdentity) -> Option<String> {
    let base = identity.email.as_ref().or(identity.account_id.as_ref());
    let org = identity.org_name.as_ref().or(identity.org_id.as_ref());
    let who = match (base, org) {
        (Some(base), Some(org)) => format!("{base} ({org})"),
        (Some(base), None) => base.clone(),
        (None, Some(org)) => org.clone(),
        _ => return None,
    };
    // Provider-supplied identity reaches the terminal; strip controls as other CLI labels do.
    Some(crate::adapters::cli::session::list::sanitize_title(&who))
}
pub(crate) fn oauth_success(login_name: &str, identity: &CredentialIdentity) -> String {
    let prefix = format!("Logged in to {login_name}");
    oauth_who(identity).map_or_else(|| prefix.clone(), |who| format!("{prefix} as {who}"))
}
/// The human result line: a minted or pasted browser key names the product (no
/// identity), a subscription names its account, an API key its validation.
pub fn success_line(method: &AuthMethod, result: &LoginResult, display: &str) -> String {
    if result.method == "browser_key" {
        format!("✓ Logged in to {}.", method.login_name().unwrap_or(display))
    } else if let Some(login_name) = method.login_name() {
        oauth_success(login_name, &result.identity)
    } else if result.validated {
        format!("✓ {display} credentials stored successfully.")
    } else {
        format!("⚠  Could not confirm key — {display} credentials stored unvalidated.")
    }
}
pub(crate) fn resolve_provider(
    provider: &str,
    app: &AppConfig,
) -> Result<(String, bool, ProviderConfig)> {
    if let Some(meta) = providers::lookup(provider) {
        // A configured built-in still keeps its custom origin and header policy.
        let config = app
            .provider
            .get(provider)
            .cloned()
            .unwrap_or_else(|| ProviderConfig {
                provider_id: provider.into(),
                model_id: String::new(),
                api_key_env: meta.api_key_env.into(),
                enabled: true,
                kind: Some(provider.into()),
                base_url: None,
                auth_header: None,
                context_window: None,
                supports_tools: None,
                discover_models: false,
                model_filter: vec![],
                cache_ttl_seconds: 0,
            });
        return Ok((meta.display_name.into(), meta.requires_key, config));
    }
    if let Some(config) = app.provider.get(provider) {
        return Ok((provider.into(), true, config.clone()));
    }
    let mut valid: Vec<_> = providers::all_provider_ids().map(str::to_owned).collect();
    for id in app.provider.keys() {
        if !valid.contains(id) {
            valid.push(id.clone());
        }
    }
    bail!(
        "unknown provider '{provider}'. Valid providers: {}",
        valid.join(", ")
    )
}

/// The line a sign-in host shows after rediscovery; `None` when the report
/// carries its own notice (which the host shows verbatim instead).
pub fn discovery_line(display: &str, report: &RefreshReport) -> Option<String> {
    if report.notice.is_some() {
        None
    } else if report.status == crate::domain::models::provider::DiscoveryStatus::Unavailable {
        Some(format!(
            "Provider {display} unavailable — showing bundled models."
        ))
    } else {
        Some(format!(
            "Discovered {} models for {display}.",
            report.model_count
        ))
    }
}

/// `Active <display> account: <label> (#N).`
pub fn active_line(accounts: &ProviderAccounts, id: crate::domain::models::CredentialId) -> String {
    let label = accounts.row(id).map_or_else(
        || format!("#{}", id.0),
        crate::infrastructure::auth_accounts::labelled,
    );
    format!("Active {} account: {label}.", accounts.display_name)
}

/// The `auth status` source vocabulary for what serves a provider, plus omp's
/// `env: <VAR>`.
pub fn source_label(accounts: &ProviderAccounts) -> Option<String> {
    Some(match accounts.served.as_ref()? {
        ServedSource::Row(id) => {
            let row = accounts.row(*id)?;
            if row.kind == AccountKind::OAuth {
                "oauth".into()
            } else if row.origin == crate::domain::models::CredentialOrigin::Login {
                "login".into()
            } else {
                "stored".into()
            }
        }
        ServedSource::Env(var) => format!("env: {var}"),
        ServedSource::Runtime => "runtime".into(),
    })
}

/// A provider row's status in the sign-in, logout and accounts pickers and the
/// `rustain init` list: the serving source, else why nothing reads the rows,
/// else `not signed in`; ` · N accounts` with several stored rows.
pub fn status_text(accounts: &ProviderAccounts) -> String {
    let mut status = match (source_label(accounts), accounts.unread) {
        (Some(source), _) => source,
        (None, Some(_)) => accounts.state_phrase(),
        (None, None) => "not signed in".to_owned(),
    };
    if accounts.rows.len() > 1 {
        status.push_str(&format!(" · {} accounts", accounts.rows.len()));
    }
    status
}

/// `<Display> (<id>): <state>`: the `auth accounts` header and the TUI account
/// step's first line.
pub fn account_header(accounts: &ProviderAccounts) -> String {
    format!(
        "{} ({}): {}",
        accounts.display_name,
        accounts.provider,
        accounts.state_phrase()
    )
}

/// A row's state column: `disabled: <cause>`, `active` (selected and served),
/// `selected`, `serving`, or empty.
pub fn row_state(row: &AccountRow) -> String {
    match (&row.disabled_cause, row.selected, row.served) {
        (Some(cause), _, _) => format!("disabled: {cause}"),
        (None, true, true) => "active".into(),
        (None, true, false) => "selected".into(),
        (None, false, true) => "serving".into(),
        (None, false, false) => String::new(),
    }
}

/// `auth logout --id` and TUI `/logout`: `Logged out of <Display> account
/// <label>.`, then what serves the provider now.
pub fn logout_lines(
    after: &ProviderAccounts,
    id: CredentialId,
    label: Option<String>,
) -> Vec<String> {
    let display = &after.display_name;
    let now = match after.served_phrase() {
        Some(phrase) => format!("{display} now uses {phrase}."),
        None if after.rows.is_empty() => format!("No {display} credentials remain."),
        // Disabled rows, rows this endpoint may not receive, or rows nothing reads.
        None => after.unserved_sentence(),
    };
    vec![
        format!(
            "Logged out of {display} account {}.",
            label.unwrap_or_else(|| format!("#{}", id.0))
        ),
        now,
    ]
}

/// `auth use` and TUI `/accounts`: the active line, plus a note when the
/// chosen row is not what a request would send.
pub fn select_lines(after: &ProviderAccounts, id: CredentialId) -> Vec<String> {
    let mut lines = vec![active_line(after, id)];
    // A selected stored key never outranks the environment, for example.
    if after.served != Some(ServedSource::Row(id)) {
        lines.push(match after.served_phrase() {
            Some(phrase) => format!(
                "Note: {} is currently served by {phrase}.",
                after.display_name
            ),
            None => format!("Note: {}", after.unserved_sentence()),
        });
    }
    lines
}

/// Every store key with at least one stored row, as `describe` sees it
/// (disabled rows and disabled configuration included: such credentials are
/// still worth removing): built-ins in table order, then other keys by
/// display name, then id.
pub fn stored_accounts(
    snapshot: &AuthSnapshot,
    app: &AppConfig,
    env_lookup: &dyn Fn(&str) -> Option<String>,
) -> Vec<ProviderAccounts> {
    let position = |id: &str| {
        providers::all_providers()
            .iter()
            .position(|meta| meta.id == id)
    };
    let mut listed: Vec<_> = snapshot
        .providers
        .iter()
        .filter(|(_, rows)| !rows.is_empty())
        .map(|(id, _)| {
            crate::infrastructure::auth_accounts::describe(snapshot, app, id, env_lookup)
        })
        .collect();
    listed.sort_by(
        |a, b| match (position(&a.provider), position(&b.provider)) {
            (Some(a), Some(b)) => a.cmp(&b),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a
                .display_name
                .cmp(&b.display_name)
                .then_with(|| a.provider.cmp(&b.provider)),
        },
    );
    listed
}

/// Whether `id` names a provider: a built-in, or the store key of a configured
/// or bundled entry.
pub fn known_provider(app: &AppConfig, id: &str) -> bool {
    providers::lookup(id).is_some()
        || crate::infrastructure::provider_availability::effective_providers(app)
            .iter()
            .any(|entry| entry.rows_key() == id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{Credential, CredentialOrigin, StoredCredential};
    use crate::infrastructure::auth_accounts::Unread;

    #[test]
    fn stored_accounts_orders_builtins_then_display_and_id() {
        let mut app = AppConfig::default();
        let mut snapshot = AuthSnapshot::default();
        for (id, kind) in [
            ("z-google", "google"),
            ("google", "google"),
            ("z-claude", "anthropic"),
            ("openrouter", "openrouter"),
            ("a-google", "google"),
            ("openai", "openai"),
        ] {
            let mut cfg = plan("openai", &app).unwrap().validation_config;
            cfg.provider_id = id.into();
            cfg.kind = Some(kind.into());
            cfg.enabled = id != "google";
            app.provider.insert(id.into(), cfg);
            snapshot.providers.insert(
                id.into(),
                vec![StoredCredential {
                    id: CredentialId(1),
                    rev: 1,
                    generation: Default::default(),
                    origin: CredentialOrigin::Login,
                    credential: Credential::new_api_key("ordering-fixture".into()),
                    last_validated: None,
                    disabled_cause: None,
                }],
            );
        }
        snapshot.providers.insert("empty".into(), vec![]);
        let listed = stored_accounts(&snapshot, &app, &|_| None);
        assert_eq!(
            listed
                .iter()
                .map(|row| row.provider.as_str())
                .collect::<Vec<_>>(),
            [
                "openai",
                "openrouter",
                "google",
                "z-claude",
                "a-google",
                "z-google"
            ]
        );
        assert_eq!(
            listed
                .iter()
                .map(|row| row.display_name.as_str())
                .collect::<Vec<_>>(),
            [
                "OpenAI",
                "OpenRouter",
                "Google AI",
                "Anthropic",
                "Google AI",
                "Google AI"
            ]
        );
        assert_eq!(listed[2].unread, Some(Unread::Disabled));
    }
}
