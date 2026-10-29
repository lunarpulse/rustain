//! `rustain auth accounts [PROVIDER] [--json]`: each stored account, the active
//! choice and the source a request would use. Offline; never builds a provider.

use std::sync::Arc;

use crate::adapters::auth::host;
use crate::domain::models::{AppConfig, CredentialOrigin};
use crate::domain::ports::AuthStorePort;
use crate::infrastructure::auth_accounts::{AccountKind, ProviderAccounts, ServedSource, describe};

pub const ACCOUNTS_SCHEMA_VERSION: &str = "1.0";

pub async fn run_auth_accounts(
    provider: Option<&str>,
    json: bool,
    store: &Arc<dyn AuthStorePort>,
    app: &AppConfig,
) -> anyhow::Result<()> {
    let (snapshot, store_error) = super::snapshot_or_warn(store).await;
    if let Some(error) = &store_error {
        eprintln!("warning: auth.json unreadable; stored credentials ignored: {error}");
    }
    let ids: Vec<String> = match provider {
        Some(provider) => vec![provider.to_owned()],
        None => snapshot
            .providers
            .iter()
            .filter(|(_, rows)| !rows.is_empty())
            .map(|(id, _)| id.clone())
            .collect(),
    };
    let lookup = crate::infrastructure::utils::env_var_trimmed;
    let listed: Vec<_> = ids
        .iter()
        .map(|id| describe(&snapshot, app, id, &lookup))
        .collect();
    if json {
        let mut out = serde_json::json!({
            "schema_version": ACCOUNTS_SCHEMA_VERSION,
            "providers": listed.iter().map(provider_json).collect::<Vec<_>>(),
        });
        if let Some(error) = store_error {
            out["store_error"] = error.into();
        }
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else if listed.is_empty() {
        println!("No stored credentials.");
    } else {
        print!("{}", render_human(&listed));
    }
    Ok(())
}

fn render_human(listed: &[ProviderAccounts]) -> String {
    let mut out = String::new();
    for accounts in listed {
        if accounts.rows.is_empty() {
            out.push_str(&format!(
                "No stored credentials for {}.\n",
                accounts.provider
            ));
            if accounts.served.is_some() {
                out.push_str(&header(accounts));
            }
            continue;
        }
        out.push_str(&header(accounts));
        for row in &accounts.rows {
            let line = format!(
                "  #{}  {}  {}  {}  {}",
                row.id.0,
                row.label,
                row.kind.token(),
                origin_token(row.origin),
                host::row_state(row),
            );
            out.push_str(line.trim_end());
            out.push('\n');
        }
    }
    out
}

fn header(accounts: &ProviderAccounts) -> String {
    format!("{}\n", host::account_header(accounts))
}

pub(super) fn origin_token(origin: CredentialOrigin) -> &'static str {
    match origin {
        CredentialOrigin::Login => "login",
        CredentialOrigin::Stored => "stored",
    }
}

/// `{"source":<auth status token>,"id":N}`, `{"source":"env","var":…}`,
/// `{"source":"runtime"}`, or `null`.
pub(super) fn served_json(accounts: &ProviderAccounts) -> serde_json::Value {
    match &accounts.served {
        Some(ServedSource::Row(id)) => {
            let source = match accounts.row(*id).map(|row| (row.kind, row.origin)) {
                Some((AccountKind::OAuth, _)) => "oauth",
                Some((AccountKind::ApiKey, CredentialOrigin::Login)) => "login_key",
                _ => "stored_key",
            };
            serde_json::json!({"source": source, "id": id.0})
        }
        Some(ServedSource::Env(var)) => serde_json::json!({"source": "env", "var": var}),
        Some(ServedSource::Runtime) => serde_json::json!({"source": "runtime"}),
        None => serde_json::Value::Null,
    }
}

fn provider_json(accounts: &ProviderAccounts) -> serde_json::Value {
    serde_json::json!({
        "provider": accounts.provider,
        "display_name": accounts.display_name,
        "served": served_json(accounts),
        "accounts": accounts.rows.iter().map(|row| serde_json::json!({
            "id": row.id.0,
            "kind": row.kind.token(),
            "origin": origin_token(row.origin),
            "label": row.label,
            "selected": row.selected,
            "served": row.served,
            "disabled_cause": row.disabled_cause,
        })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::CredentialId;
    use crate::infrastructure::auth_accounts::AccountRow;

    fn row(id: u64, label: &str, selected: bool, served: bool) -> AccountRow {
        AccountRow {
            id: CredentialId(id),
            kind: AccountKind::ApiKey,
            origin: CredentialOrigin::Login,
            label: label.into(),
            selected,
            served,
            disabled_cause: None,
        }
    }

    #[test]
    fn human_rows_carry_the_state_column_and_the_served_header() {
        let mut disabled = row(3, "API key #3", false, false);
        disabled.disabled_cause = Some("revoked".into());
        let listed = [
            ProviderAccounts {
                provider: "openai".into(),
                display_name: "OpenAI".into(),
                served: Some(ServedSource::Row(CredentialId(2))),
                unread: None,
                rows: vec![
                    row(1, "API key #1", true, false),
                    row(2, "API key #2", false, true),
                    disabled,
                    row(4, "API key #4", false, false),
                ],
            },
            ProviderAccounts {
                provider: "openrouter".into(),
                display_name: "OpenRouter".into(),
                served: Some(ServedSource::Env("OPENROUTER_API_KEY".into())),
                unread: None,
                rows: vec![],
            },
            ProviderAccounts {
                provider: "xai".into(),
                display_name: "xAI".into(),
                served: None,
                unread: None,
                rows: vec![row(9, "API key #9", true, false)],
            },
        ];
        assert_eq!(
            render_human(&listed),
            "OpenAI (openai): serving API key #2\n\
             \x20 #1  API key #1  api_key  login  selected\n\
             \x20 #2  API key #2  api_key  login  serving\n\
             \x20 #3  API key #3  api_key  login  disabled: revoked\n\
             \x20 #4  API key #4  api_key  login\n\
             No stored credentials for openrouter.\n\
             OpenRouter (openrouter): serving OPENROUTER_API_KEY\n\
             xAI (xai): no usable credential\n\
             \x20 #9  API key #9  api_key  login  selected\n"
        );
    }

    fn accounts(
        served: Option<ServedSource>,
        unread: Option<crate::infrastructure::auth_accounts::Unread>,
        rows: Vec<AccountRow>,
    ) -> ProviderAccounts {
        ProviderAccounts {
            provider: "openai".into(),
            display_name: "OpenAI".into(),
            served,
            unread,
            rows,
        }
    }

    /// The texts `auth logout --id`, `auth use` and the TUI share: today's CLI
    /// bytes for an enabled reader, the disabled-entry sentence otherwise.
    #[test]
    fn shared_account_texts_keep_the_cli_bytes() {
        use crate::infrastructure::auth_accounts::Unread;
        let id = CredentialId(2);
        let served = accounts(
            Some(ServedSource::Row(CredentialId(1))),
            None,
            vec![row(1, "API key #1", true, true)],
        );
        assert_eq!(
            host::logout_lines(&served, id, Some("API key #2".into())),
            [
                "Logged out of OpenAI account API key #2.",
                "OpenAI now uses API key #1."
            ]
        );
        assert_eq!(
            host::logout_lines(&accounts(None, None, vec![]), id, None),
            [
                "Logged out of OpenAI account #2.",
                "No OpenAI credentials remain."
            ]
        );
        let mut revoked = row(1, "API key #1", false, false);
        revoked.disabled_cause = Some("revoked".into());
        assert_eq!(
            host::logout_lines(&accounts(None, None, vec![revoked.clone()]), id, None)[1],
            "OpenAI has no usable credential."
        );
        assert_eq!(
            host::logout_lines(
                &accounts(None, Some(Unread::NoReader), vec![revoked]),
                id,
                None
            )[1],
            "No enabled provider reads OpenAI credentials."
        );

        let env = accounts(
            Some(ServedSource::Env("OPENAI_API_KEY".into())),
            None,
            vec![row(2, "API key #2", true, false)],
        );
        assert_eq!(
            host::select_lines(&env, id),
            [
                "Active OpenAI account: API key #2.",
                "Note: OpenAI is currently served by OPENAI_API_KEY."
            ]
        );
        let row_served = accounts(
            Some(ServedSource::Row(id)),
            None,
            vec![row(2, "API key #2", true, true)],
        );
        assert_eq!(
            host::select_lines(&row_served, id),
            ["Active OpenAI account: API key #2."]
        );
        let disabled = accounts(
            None,
            Some(Unread::Disabled),
            vec![row(2, "API key #2", true, false)],
        );
        assert_eq!(
            host::select_lines(&disabled, id)[1],
            "Note: OpenAI is disabled in configuration."
        );
        assert_eq!(
            host::select_lines(
                &accounts(None, None, vec![row(2, "API key #2", true, false)]),
                id
            )[1],
            "Note: OpenAI has no usable credential."
        );
        assert_eq!(
            header(&disabled),
            "OpenAI (openai): disabled in configuration\n"
        );
        assert_eq!(host::status_text(&disabled), "disabled in configuration");
        assert_eq!(host::status_text(&served), "login");
        assert_eq!(
            host::status_text(&accounts(None, None, vec![])),
            "not signed in"
        );
    }
}
