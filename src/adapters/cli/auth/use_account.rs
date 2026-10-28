//! `rustain auth use <provider> --id <N> [--json]`: the provider's active account.

use std::sync::Arc;

use crate::domain::models::{AppConfig, CredentialId};
use crate::domain::ports::AuthStorePort;
use crate::infrastructure::auth_accounts::{ServedSource, select_account};

pub async fn run_auth_use(
    provider: &str,
    id: u64,
    json: bool,
    store: &Arc<dyn AuthStorePort>,
    app: &AppConfig,
) -> anyhow::Result<()> {
    let id = CredentialId(id);
    let after = match select_account(store, app, provider, id).await {
        Ok(after) => after,
        Err(error) => {
            eprintln!("{error}");
            return Err(error.into());
        }
    };
    if json {
        println!(
            "{}",
            serde_json::json!({
                "provider": provider,
                "status": "selected",
                "id": id.0,
                "served": super::accounts::served_json(&after),
            })
        );
    } else {
        println!("{}", super::accounts::active_line(&after, id));
    }
    // A selected stored key never outranks the environment, for example.
    if after.served != Some(ServedSource::Row(id)) {
        match after.served_phrase() {
            Some(phrase) => eprintln!(
                "Note: {} is currently served by {phrase}.",
                after.display_name
            ),
            None => eprintln!("Note: {} has no usable credential.", after.display_name),
        }
    }
    Ok(())
}
