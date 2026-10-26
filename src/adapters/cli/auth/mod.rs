pub mod list;
pub mod login;
pub mod logout;
pub mod providers;
pub mod status;
pub mod terminal_ui;

use crate::domain::models::{AuthSnapshot, AuthSource};
use crate::domain::services::key_cascade::{CascadeInputs, SelectionMaterial, select};

/// Ordered environment names used by provenance and ACP availability.
pub(crate) fn env_vars_for<'a>(provider_id: &str, api_key_env: &'a str) -> [&'a str; 2] {
    if provider_id == "anthropic" {
        ["ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_API_KEY"]
    } else {
        [api_key_env, ""]
    }
}

pub(crate) struct SourceSelection {
    pub source: AuthSource,
    pub material: SelectionMaterial,
    pub last_validated: Option<chrono::DateTime<chrono::Utc>>,
    pub row: Option<(crate::domain::models::CredentialId, u64)>,
    pub identity: Option<String>,
}

impl SourceSelection {
    pub(crate) fn from_selection(
        selection: crate::domain::services::key_cascade::Selection<'_>,
        provider_id: &str,
    ) -> Self {
        let identity = selection.row.and_then(|row| match &row.credential {
            crate::domain::models::Credential::OAuth(oauth) => {
                oauth
                    .identity
                    .key(crate::adapters::auth::policies::org_scoped_identity(
                        provider_id,
                    ))
            }
            _ => None,
        });
        Self {
            source: selection.source,
            material: selection.material,
            last_validated: selection.last_validated,
            row: selection.row.map(|row| (row.id, row.rev)),
            identity,
        }
    }
}

/// Availability and request construction share this provenance reading.
pub(crate) fn source_for(
    provider_id: &str,
    env_vars: &[&str],
    snapshot: &AuthSnapshot,
    env_lookup: &dyn Fn(&str) -> Option<String>,
    oauth_allowed: bool,
) -> Option<SourceSelection> {
    let selection = select(
        &CascadeInputs {
            runtime: None,
            rows: snapshot.rows(provider_id),
            env_vars,
            env_lookup,
            oauth_allowed,
            now_ms: chrono::Utc::now().timestamp_millis(),
        },
        &Default::default(),
    )?;
    Some(SourceSelection::from_selection(selection, provider_id))
}

/// Read failures must not hide usable environment credentials on offline surfaces.
pub(crate) async fn snapshot_or_warn(
    store: &std::sync::Arc<dyn crate::domain::ports::AuthStorePort>,
) -> (AuthSnapshot, Option<String>) {
    match store.snapshot().await {
        Ok(snapshot) => (snapshot, None),
        Err(error) => {
            tracing::warn!("auth.json unreadable; stored credentials ignored: {error}");
            (AuthSnapshot::default(), Some(error.to_string()))
        }
    }
}

/// Observe exactly the configured factory's rows, environment list and OAuth origin.
pub(crate) fn source_for_configured(
    provider_id: &str,
    default_env: &str,
    configured: &std::collections::BTreeMap<String, crate::domain::models::ProviderConfig>,
    snapshot: &AuthSnapshot,
    env_lookup: &dyn Fn(&str) -> Option<String>,
) -> Option<SourceSelection> {
    let configured = configured.get_key_value(provider_id).or_else(|| {
        configured
            .iter()
            .find(|(id, cfg)| cfg.enabled && cfg.kind.as_deref().unwrap_or(id) == provider_id)
    });
    if let Some((id, cfg)) = configured {
        let kind = cfg.kind.as_deref().unwrap_or(id);
        let rows_key = if kind == "anthropic" {
            &cfg.provider_id
        } else {
            id
        };
        let env_name = cfg.api_key_env.as_str();
        let env_vars = if env_name.is_empty() {
            &[][..]
        } else {
            std::slice::from_ref(&env_name)
        };
        let oauth_allowed = kind != "anthropic"
            || crate::infrastructure::provider_factory::anthropic_oauth_allowed(
                &crate::infrastructure::provider_factory::anthropic_base_url(Some(cfg), env_lookup),
            );
        source_for(rows_key, env_vars, snapshot, env_lookup, oauth_allowed)
    } else {
        let oauth_allowed = provider_id != "anthropic"
            || crate::infrastructure::provider_factory::anthropic_oauth_allowed(
                &crate::infrastructure::provider_factory::anthropic_base_url(None, env_lookup),
            );
        source_for(
            provider_id,
            &env_vars_for(provider_id, default_env),
            snapshot,
            env_lookup,
            oauth_allowed,
        )
    }
}
