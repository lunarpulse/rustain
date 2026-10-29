//! Stored accounts of one provider: row labels, the active choice and the source
//! a request would be served from. The one implementation behind `auth
//! accounts`, `auth use`, `auth logout --id` and the TUI account picker; it
//! reads the store and configuration and prints nothing.
//!
//! Account labels adapted from oh-my-pi.
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

use std::sync::Arc;

use crate::domain::errors::AuthError;
use crate::domain::models::{
    AppConfig, AuthSnapshot, AuthSource, Credential, CredentialId, CredentialOrigin, RowRemoval,
    SelectOutcome, StoredCredential,
};
use crate::domain::ports::AuthStorePort;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountKind {
    OAuth,
    ApiKey,
}

impl AccountKind {
    pub fn token(self) -> &'static str {
        match self {
            Self::OAuth => "oauth",
            Self::ApiKey => "api_key",
        }
    }
}

/// Where a request for the provider would get its credential now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServedSource {
    Row(CredentialId),
    Env(String),
    Runtime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountRow {
    pub id: CredentialId,
    pub kind: AccountKind,
    pub origin: CredentialOrigin,
    /// Sanitized display label; never key bytes.
    pub label: String,
    pub selected: bool,
    pub served: bool,
    pub disabled_cause: Option<String>,
}

/// Why no source serves a provider's rows: nothing enabled in this process reads them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unread {
    /// The entry that reads these rows is disabled in configuration.
    Disabled,
    /// No configured or bundled entry reads these rows (for example an enabled
    /// kind alias with its own rows shadows the built-in).
    NoReader,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderAccounts {
    /// The store key `auth login` and `auth logout` take.
    pub provider: String,
    pub display_name: String,
    pub served: Option<ServedSource>,
    /// Set only when no enabled entry reads these rows (`served` is then `None`).
    pub unread: Option<Unread>,
    /// File order.
    pub rows: Vec<AccountRow>,
}

impl ProviderAccounts {
    pub fn row(&self, id: CredentialId) -> Option<&AccountRow> {
        self.rows.iter().find(|row| row.id == id)
    }

    /// The served source in words: `<label> (#N)`, the variable name, or `runtime key`.
    pub fn served_phrase(&self) -> Option<String> {
        Some(match self.served.as_ref()? {
            ServedSource::Row(id) => self.row(*id).map_or_else(|| format!("#{}", id.0), labelled),
            ServedSource::Env(var) => var.clone(),
            ServedSource::Runtime => "runtime key".to_owned(),
        })
    }

    /// The header state: `serving <phrase>`, why nothing reads the rows, or
    /// `no usable credential`.
    pub fn state_phrase(&self) -> String {
        match (self.served_phrase(), self.unread) {
            (Some(phrase), _) => format!("serving {phrase}"),
            (None, Some(Unread::Disabled)) => "disabled in configuration".to_owned(),
            (None, Some(Unread::NoReader)) => "not read by any enabled provider".to_owned(),
            (None, None) => "no usable credential".to_owned(),
        }
    }

    /// The sentence for a provider nothing serves.
    pub fn unserved_sentence(&self) -> String {
        let display = &self.display_name;
        match self.unread {
            Some(Unread::Disabled) => format!("{display} is disabled in configuration."),
            Some(Unread::NoReader) => format!("No enabled provider reads {display} credentials."),
            None => format!("{display} has no usable credential."),
        }
    }
}

/// `<label> (#N)`, unless the label already ends in `#N`.
pub fn labelled(row: &AccountRow) -> String {
    let suffix = format!("#{}", row.id.0);
    if row.label.ends_with(&suffix) {
        row.label.clone()
    } else {
        format!("{} ({suffix})", row.label)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AccountError {
    #[error("No stored credential #{} for {provider}. Run `rustain auth accounts {provider}`.", .id.0)]
    NotFound { provider: String, id: CredentialId },
    #[error("Credential #{} for {provider} is disabled ({cause}); log in again.", .id.0)]
    Disabled {
        provider: String,
        id: CredentialId,
        cause: String,
    },
    #[error(transparent)]
    Store(#[from] AuthError),
}

/// omp `logout.ts` labels: an OAuth row by its first non-blank email, account or
/// project, else `OAuth credential #<id>`, with its organization appended when
/// that differs; an API key as `API key #<id>`. Provider-supplied identity is
/// stripped of terminal controls.
pub fn label(row: &StoredCredential) -> String {
    let Credential::OAuth(oauth) = &row.credential else {
        return format!("API key #{}", row.id.0);
    };
    fn first<'a>(values: &[&'a Option<String>]) -> Option<&'a str> {
        values
            .iter()
            .filter_map(|value| value.as_deref())
            .map(str::trim)
            .find(|value| !value.is_empty())
    }
    let identity = &oauth.identity;
    let base = first(&[&identity.email, &identity.account_id, &identity.project_id])
        .map_or_else(|| format!("OAuth credential #{}", row.id.0), str::to_owned);
    let label = match first(&[&identity.org_name, &identity.org_id]) {
        Some(org) if org != base => format!("{base} ({org})"),
        _ => base,
    };
    crate::adapters::cli::session::list::sanitize_title(&label)
}

/// The provider's accounts as `snapshot` and the configuration serve them: the
/// served source is the first enabled entry reading these rows, through the
/// same cascade, environment list and stored-row rule as availability. With no
/// enabled reader nothing serves them (`unread` says why): nothing in the
/// process would build that provider.
pub fn describe(
    snapshot: &AuthSnapshot,
    app: &AppConfig,
    provider: &str,
    env_lookup: &dyn Fn(&str) -> Option<String>,
) -> ProviderAccounts {
    let entries = super::provider_availability::effective_providers(app);
    let reading = |enabled: bool| {
        entries
            .iter()
            .find(|entry| entry.cfg.enabled == enabled && entry.rows_key() == provider)
    };
    let (entry, disabled) = (reading(true), reading(false));
    let meta = crate::adapters::cli::auth::providers::lookup(provider);
    let selection = entry.and_then(|entry| {
        crate::adapters::cli::auth::source_for(
            provider,
            &entry.env_vars(),
            snapshot,
            env_lookup,
            entry.row_policy(env_lookup),
        )
    });
    let unread = match (entry, disabled) {
        (Some(_), _) => None,
        (None, Some(_)) => Some(Unread::Disabled),
        (None, None) => Some(Unread::NoReader),
    };
    let served = selection.map(|selection| match (selection.source, selection.row) {
        (AuthSource::Runtime, _) => ServedSource::Runtime,
        (AuthSource::Env { var }, _) => ServedSource::Env(var),
        (_, Some(id)) => ServedSource::Row(id),
        (_, None) => unreachable!("a stored source always names its row"),
    });
    let selected = snapshot.selected(provider);
    let rows = snapshot
        .rows(provider)
        .iter()
        .map(|row| AccountRow {
            id: row.id,
            kind: match row.credential {
                Credential::OAuth(_) => AccountKind::OAuth,
                _ => AccountKind::ApiKey,
            },
            origin: row.origin,
            label: label(row),
            selected: selected == Some(row.id),
            served: served == Some(ServedSource::Row(row.id)),
            disabled_cause: row
                .disabled_cause
                .as_deref()
                .map(crate::adapters::cli::session::list::sanitize_title),
        })
        .collect();
    ProviderAccounts {
        provider: provider.to_owned(),
        display_name: entry.or(disabled).map_or_else(
            || meta.map_or(provider, |meta| meta.display_name).to_owned(),
            |entry| entry.display_name(),
        ),
        served,
        unread,
        rows,
    }
}

pub async fn accounts(
    store: &Arc<dyn AuthStorePort>,
    app: &AppConfig,
    provider: &str,
) -> Result<ProviderAccounts, AuthError> {
    let snapshot = store.snapshot().await?;
    Ok(describe(
        &snapshot,
        app,
        provider,
        &crate::infrastructure::utils::env_var_trimmed,
    ))
}

/// Makes row `id` the provider's active account; durable, and served on the
/// next request of every process. Returns the after-state.
pub async fn select_account(
    store: &Arc<dyn AuthStorePort>,
    app: &AppConfig,
    provider: &str,
    id: CredentialId,
) -> Result<ProviderAccounts, AccountError> {
    match store.select(provider, id).await? {
        SelectOutcome::Selected => Ok(accounts(store, app, provider).await?),
        SelectOutcome::NotFound => Err(AccountError::NotFound {
            provider: provider.to_owned(),
            id,
        }),
        SelectOutcome::Disabled => {
            let after = accounts(store, app, provider).await?;
            Err(AccountError::Disabled {
                provider: provider.to_owned(),
                id,
                cause: after
                    .row(id)
                    .and_then(|row| row.disabled_cause.clone())
                    .unwrap_or_else(|| "disabled".to_owned()),
            })
        }
    }
}

/// Removes row `id` only; removing the active row clears the choice, and the
/// after-state's `served` shows the fallback.
pub async fn logout_account(
    store: &Arc<dyn AuthStorePort>,
    app: &AppConfig,
    provider: &str,
    id: CredentialId,
) -> Result<ProviderAccounts, AccountError> {
    match store.remove_row(provider, id).await? {
        RowRemoval::Removed => Ok(accounts(store, app, provider).await?),
        RowRemoval::NotFound => Err(AccountError::NotFound {
            provider: provider.to_owned(),
            id,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{
        CredentialGeneration, CredentialIdentity, OAuthCredential, SecretString,
    };

    fn oauth(id: u64, identity: CredentialIdentity) -> StoredCredential {
        StoredCredential {
            id: CredentialId(id),
            rev: 1,
            generation: CredentialGeneration([1; 16]),
            origin: CredentialOrigin::Login,
            credential: Credential::OAuth(OAuthCredential {
                access: SecretString::new("ACCESS-CANARY".into()),
                refresh: SecretString::new("REFRESH-CANARY".into()),
                expires_at_ms: i64::MAX,
                authorized_at_ms: None,
                identity,
                routing: Default::default(),
            }),
            last_validated: None,
            disabled_cause: None,
        }
    }

    #[test]
    fn labels_follow_the_omp_order_and_never_carry_secrets() {
        let identity = |email: &str, account: &str, project: &str, org_name: &str, org: &str| {
            let some = |value: &str| (!value.is_empty()).then(|| value.to_owned());
            CredentialIdentity {
                email: some(email),
                account_id: some(account),
                project_id: some(project),
                org_name: some(org_name),
                org_id: some(org),
            }
        };
        for (identity, want) in [
            (identity("me@x.io", "acct", "proj", "", ""), "me@x.io"),
            (identity("  ", "acct", "proj", "", ""), "acct"),
            (identity("", "", "proj", "", ""), "proj"),
            (identity("", "", "", "", ""), "OAuth credential #7"),
            (
                identity("me@x.io", "", "", "Team", "org-1"),
                "me@x.io (Team)",
            ),
            (identity("me@x.io", "", "", " ", "org-1"), "me@x.io (org-1)"),
            // An organization equal to the base adds nothing (Codex workspace id).
            (identity("", "acct", "", "", "acct"), "acct"),
            (
                identity("", "", "", "plus", ""),
                "OAuth credential #7 (plus)",
            ),
            (identity("a\u{1b}]0;x\u{7}@b", "", "", "", ""), "a@b"),
        ] {
            let row = oauth(7, identity);
            let label = label(&row);
            assert_eq!(label, want);
            assert!(!label.contains("CANARY"));
        }
        let key = StoredCredential {
            credential: Credential::new_api_key("sk-KEY-CANARY".into()),
            ..oauth(3, CredentialIdentity::default())
        };
        assert_eq!(label(&key), "API key #3");
    }

    #[test]
    fn labelled_appends_the_id_once() {
        let row = |label: &str| AccountRow {
            id: CredentialId(4),
            kind: AccountKind::ApiKey,
            origin: CredentialOrigin::Login,
            label: label.into(),
            selected: false,
            served: false,
            disabled_cause: None,
        };
        assert_eq!(labelled(&row("API key #4")), "API key #4");
        assert_eq!(labelled(&row("me@x.io")), "me@x.io (#4)");
        assert_eq!(labelled(&row("API key #14")), "API key #14 (#4)");
    }

    fn entry(kind: &str, enabled: bool) -> crate::domain::models::ProviderConfig {
        crate::domain::models::ProviderConfig {
            provider_id: kind.into(),
            model_id: "m".into(),
            api_key_env: "OPENAI_API_KEY".into(),
            enabled,
            kind: Some(kind.into()),
            base_url: None,
            auth_header: None,
            context_window: None,
            supports_tools: None,
            discover_models: false,
            model_filter: vec![],
            cache_ttl_seconds: 0,
        }
    }

    /// Only an enabled entry reading the rows serves them; otherwise `unread`
    /// names why, and the rows stay listed.
    #[test]
    fn only_an_enabled_reader_serves_stored_rows() {
        let mut snapshot = AuthSnapshot::default();
        let key = StoredCredential {
            credential: Credential::new_api_key("sk-ROW-CANARY".into()),
            ..oauth(1, CredentialIdentity::default())
        };
        snapshot.providers.insert("openai".into(), vec![key]);
        let env = |name: &str| (name == "OPENAI_API_KEY").then(|| "sk-ENV-CANARY".to_owned());
        let with = |entries: &[(&str, crate::domain::models::ProviderConfig)]| {
            let mut app = AppConfig::default();
            for (id, cfg) in entries {
                app.provider.insert((*id).into(), cfg.clone());
            }
            describe(&snapshot, &app, "openai", &env)
        };

        let enabled = with(&[("openai", entry("openai", true))]);
        assert_eq!(enabled.served, Some(ServedSource::Row(CredentialId(1))));
        assert_eq!(enabled.unread, None);
        assert_eq!(enabled.state_phrase(), "serving API key #1");

        let disabled = with(&[("openai", entry("openai", false))]);
        assert_eq!(
            (disabled.served.clone(), disabled.unread),
            (None, Some(Unread::Disabled))
        );
        assert_eq!(disabled.display_name, "OpenAI");
        assert_eq!(disabled.rows.len(), 1);
        assert_eq!(disabled.state_phrase(), "disabled in configuration");
        assert_eq!(
            disabled.unserved_sentence(),
            "OpenAI is disabled in configuration."
        );

        // An enabled kind alias with its own rows shadows the built-in.
        let alias = with(&[("work", entry("openai", true))]);
        assert_eq!(
            (alias.served.clone(), alias.unread),
            (None, Some(Unread::NoReader))
        );
        assert_eq!(alias.state_phrase(), "not read by any enabled provider");
        assert_eq!(
            alias.unserved_sentence(),
            "No enabled provider reads OpenAI credentials."
        );

        // An enabled reader with nothing usable keeps the old wording.
        snapshot.providers.get_mut("openai").unwrap()[0].disabled_cause = Some("revoked".into());
        let none = describe(&snapshot, &AppConfig::default(), "openai", &|_| None);
        assert_eq!((none.served.clone(), none.unread), (None, None));
        assert_eq!(none.state_phrase(), "no usable credential");
        assert_eq!(none.unserved_sentence(), "OpenAI has no usable credential.");
    }
}
