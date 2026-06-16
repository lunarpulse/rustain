pub mod list;
pub mod login;
pub mod providers;
pub mod status;

use chrono::{DateTime, Utc};

use crate::domain::models::credential::{AuthSource, ProviderStatus};

// ---------------------------------------------------------------------------
// Shared credential-precedence helpers (Story 13.4c, Task 2a).
//
// Single source of truth for "is a credential present for this provider?"
// and "where does it come from?". Both `status.rs` and `list.rs` call these
// so the env → auth.json + Anthropic dual-token logic can never drift.
// ---------------------------------------------------------------------------

/// Check whether an env-var credential is present for `meta`, including
/// the Anthropic dual-token rule (`ANTHROPIC_AUTH_TOKEN` **or**
/// `ANTHROPIC_API_KEY`). Empty/whitespace-only values are treated as absent
/// (via `env_var_trimmed` semantics in the injected `env_lookup`).
///
/// Relocated verbatim from `status.rs:120-134` (Story 13.4b).
pub(crate) fn env_present(
    meta: &providers::ProviderMeta,
    env_lookup: &impl Fn(&str) -> Option<String>,
) -> bool {
    if meta.id == "anthropic" {
        return env_value_present("ANTHROPIC_AUTH_TOKEN", env_lookup)
            || env_value_present("ANTHROPIC_API_KEY", env_lookup);
    }

    !meta.api_key_env.is_empty() && env_value_present(meta.api_key_env, env_lookup)
}

/// True when `env_lookup(env_name)` yields a non-empty, non-whitespace value.
pub(crate) fn env_value_present(
    env_name: &str,
    env_lookup: &impl Fn(&str) -> Option<String>,
) -> bool {
    env_lookup(env_name).is_some_and(|value| !value.trim().is_empty())
}

/// Full credential-source detection following the live precedence:
/// env wins → auth.json fallback → `None`.
///
/// Mirrors `provider_factory::resolve_auth` (env → auth.json, env wins,
/// including Anthropic dual-token). Returns the winning `AuthSource` and,
/// for auth.json entries, the stored `last_validated` timestamp.
pub(crate) fn detect_source(
    meta: &providers::ProviderMeta,
    auth_by_provider: &std::collections::HashMap<&str, &ProviderStatus>,
    env_lookup: &impl Fn(&str) -> Option<String>,
) -> Option<(AuthSource, Option<DateTime<Utc>>)> {
    if env_present(meta, env_lookup) {
        return Some((AuthSource::Env, None));
    }
    if let Some(stored) = auth_by_provider.get(meta.id) {
        return Some((AuthSource::AuthJson, stored.last_validated));
    }
    None
}
