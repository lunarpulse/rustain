pub mod list;
pub mod login;
pub mod providers;
pub mod status;

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
}

/// Availability includes stale OAuth credentials; construction uses select_ready.
pub(crate) fn source_for(
    provider_id: &str,
    env_vars: &[&str],
    snapshot: &AuthSnapshot,
    env_lookup: &dyn Fn(&str) -> Option<String>,
) -> Option<SourceSelection> {
    let selection = select(
        &CascadeInputs {
            runtime: None,
            rows: snapshot.rows(provider_id),
            env_vars,
            env_lookup,
            now_ms: chrono::Utc::now().timestamp_millis(),
        },
        &Default::default(),
    )?;
    Some(SourceSelection {
        source: selection.source,
        material: selection.material,
        last_validated: selection.last_validated,
    })
}
