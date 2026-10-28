use crate::domain::models::{AppConfig, CredentialId};
use crate::domain::ports::AuthStorePort;
use crate::infrastructure::auth_accounts::{AccountError, accounts, logout_account};
use std::sync::Arc;

/// Removes every persisted row, including disabled grants, and the provider's
/// active-account choice; environment is untouched.
pub async fn run_auth_logout(
    provider: &str,
    json: bool,
    store: &Arc<dyn AuthStorePort>,
) -> anyhow::Result<()> {
    let removed = store.snapshot().await?.rows(provider).len();
    store.remove(provider).await?;
    let api_key_env = super::providers::lookup(provider).map_or("", |meta| meta.api_key_env);
    let still_set: Vec<&str> = super::env_vars_for(provider, api_key_env)
        .into_iter()
        .filter(|name| {
            !name.is_empty() && crate::infrastructure::utils::env_var_trimmed(name).is_some()
        })
        .collect();
    let status = if removed == 0 {
        "no_stored_credentials"
    } else {
        "logged_out"
    };
    if json {
        println!(
            "{}",
            serde_json::json!({"provider":provider,"status":status,"removed":removed,"env_still_set":still_set})
        );
    } else if removed == 0 {
        println!("No stored credentials for {provider}.");
    } else {
        println!("Logged out of {provider}.");
    }
    for name in &still_set {
        eprintln!("{name} is still set and will still be used; unset it separately.");
    }
    if still_set.is_empty() {
        eprintln!(
            "Environment variables are unchanged; unset them separately if this provider is still configured."
        );
    }
    Ok(())
}

/// `auth logout <provider> --id <N>`: exactly that account; other accounts,
/// providers, environment and configuration are untouched. Removing the active
/// account clears the choice, and the provider falls back to the default order.
pub async fn run_auth_logout_row(
    provider: &str,
    id: u64,
    json: bool,
    store: &Arc<dyn AuthStorePort>,
    app: &AppConfig,
) -> anyhow::Result<()> {
    let id = CredentialId(id);
    // The label is read before the row goes; the caller prints no error.
    let label = match accounts(store, app, provider).await {
        Ok(before) => before.row(id).map(|row| row.label.clone()),
        Err(error) => {
            eprintln!("{error}");
            return Err(error.into());
        }
    };
    let after = match logout_account(store, app, provider, id).await {
        Ok(after) => after,
        Err(error) => {
            if json && matches!(error, AccountError::NotFound { .. }) {
                println!(
                    "{}",
                    serde_json::json!({"provider":provider,"status":"not_found","id":id.0})
                );
            }
            eprintln!("{error}");
            return Err(error.into());
        }
    };
    if json {
        println!(
            "{}",
            serde_json::json!({
                "provider": provider,
                "status": "logged_out",
                "removed": 1,
                "id": id.0,
                "remaining": after.rows.len(),
                "served": super::accounts::served_json(&after),
            })
        );
    } else {
        println!(
            "Logged out of {} account {}.",
            after.display_name,
            label.unwrap_or_else(|| format!("#{}", id.0))
        );
    }
    let display = &after.display_name;
    match after.served_phrase() {
        Some(phrase) => eprintln!("{display} now uses {phrase}."),
        None if after.rows.is_empty() => eprintln!("No {display} credentials remain."),
        // Disabled rows, or rows this endpoint may not receive, still remain.
        None => eprintln!("{display} has no usable credential."),
    }
    Ok(())
}
