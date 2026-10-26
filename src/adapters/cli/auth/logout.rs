use crate::domain::ports::AuthStorePort;
use std::sync::Arc;

/// Removes every persisted row, including disabled grants; environment is untouched.
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
