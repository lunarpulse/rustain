//! `rustain auth accounts [PROVIDER] [--json]`: each stored account, the active
//! choice and the source a request would use. Offline; never builds a provider.

use std::sync::Arc;

use crate::domain::models::{AppConfig, CredentialOrigin};
use crate::domain::ports::AuthStorePort;
use crate::infrastructure::auth_accounts::{
    AccountKind, ProviderAccounts, ServedSource, describe, labelled,
};

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
            let state = match (&row.disabled_cause, row.selected, row.served) {
                (Some(cause), _, _) => format!("disabled: {cause}"),
                (None, true, true) => "active".into(),
                (None, true, false) => "selected".into(),
                (None, false, true) => "serving".into(),
                (None, false, false) => String::new(),
            };
            let line = format!(
                "  #{}  {}  {}  {}  {state}",
                row.id.0,
                row.label,
                row.kind.token(),
                origin_token(row.origin),
            );
            out.push_str(line.trim_end());
            out.push('\n');
        }
    }
    out
}

fn header(accounts: &ProviderAccounts) -> String {
    let serving = accounts.served_phrase().map_or_else(
        || "no usable credential".to_owned(),
        |phrase| format!("serving {phrase}"),
    );
    format!(
        "{} ({}): {serving}\n",
        accounts.display_name, accounts.provider
    )
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
                rows: vec![],
            },
            ProviderAccounts {
                provider: "xai".into(),
                display_name: "xAI".into(),
                served: None,
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
}
