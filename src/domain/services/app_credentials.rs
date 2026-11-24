//! The one policy for app-owned credential environment names (Story Q-6 AC1;
//! D-23/EQ-4, D-35).
//!
//! A name is *app-owned* when the application itself reads that variable as a
//! credential: a supported provider's key or token (default names and aliases),
//! a provider's configured `api_key_env` — even a nonstandard one — a channel
//! credential such as the Telegram bot token, or the parent-only MCP HTTP
//! auth-token family.
//!
//! The classification is by **name only**. Nothing here reads an environment
//! value or recognizes a secret by its shape, so the policy cannot be defeated
//! by a credential that does not look like one.
//!
//! Two enforcement points share this list:
//! - inherited omission — the final command-construction boundary of the Bash
//!   tool and of an MCP stdio child removes every app-owned name from the child
//!   environment (`adapters::child_env`);
//! - explicit refusal — MCP stdio `command`/`args`/`env` references to an
//!   app-owned name are refused at resolution, before the value is looked up
//!   (`McpServerSpec::stdio_reference_refusal`, `expand_stdio_env_vars`).
//!
//! Configured names are registered by the configuration loader once the
//! resolved provider selectors are known. The registry only ever grows within a
//! process: a name that stopped being configured after a reload stays omitted,
//! which can only make the child environment smaller, never leak.

use std::collections::BTreeSet;
use std::sync::RwLock;

/// Provider credential variables the application reads by default, aliases
/// included. `adapters/cli/auth/providers.rs` and `env_vars_for` are the
/// sources; a unit test there fails if a catalog name is missing here.
pub const PROVIDER_CREDENTIAL_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "OPENAI_API_KEY",
    "OPENAI_CODEX_OAUTH_TOKEN",
    "OPENROUTER_API_KEY",
    "GOOGLE_API_KEY",
    "GEMINI_API_KEY",
    "DEEPSEEK_API_KEY",
    "MOONSHOT_API_KEY",
    "XAI_API_KEY",
    "XAI_OAUTH_TOKEN",
    "COPILOT_GITHUB_TOKEN",
];

/// The Telegram channel credential variable.
pub const TELEGRAM_BOT_TOKEN_ENV: &str = "TELEGRAM_BOT_TOKEN";

/// Channel credential variables.
pub const CHANNEL_CREDENTIAL_ENV: &[&str] = &[TELEGRAM_BOT_TOKEN_ENV];

/// The parent-only MCP HTTP auth-token family: the process-wide fallback and
/// every `<prefix>_<SERVER>` per-server name.
pub const MCP_AUTH_ENV_PREFIX: &str = "RUSTAIN_MCP_HTTP_AUTH_TOKEN";

static CONFIGURED: RwLock<BTreeSet<String>> = RwLock::new(BTreeSet::new());

/// Register credential variable names taken from resolved configuration
/// (`provider.<id>.api_key_env`). Blank names are ignored.
pub fn register_configured_credential_names<I, S>(names: I)
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut configured = CONFIGURED
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for name in names {
        let name = name.as_ref().trim();
        if !name.is_empty() {
            configured.insert(name.to_owned());
        }
    }
}

/// Whether `name` is an app-owned credential variable. Names are compared
/// exactly: environment names are case-sensitive on every supported target.
#[must_use]
pub fn is_app_credential_name(name: &str) -> bool {
    PROVIDER_CREDENTIAL_ENV.contains(&name)
        || CHANNEL_CREDENTIAL_ENV.contains(&name)
        || name.starts_with(MCP_AUTH_ENV_PREFIX)
        || CONFIGURED
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(name)
}

/// Every exact app-owned name currently known (the prefix family aside), for a
/// spawner that removes names without enumerating the parent environment.
#[must_use]
pub fn known_credential_names() -> Vec<String> {
    let mut names: BTreeSet<String> = PROVIDER_CREDENTIAL_ENV
        .iter()
        .chain(CHANNEL_CREDENTIAL_ENV)
        .map(|name| (*name).to_owned())
        .collect();
    names.extend(
        CONFIGURED
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .cloned(),
    );
    names.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_aliases_channel_and_mcp_auth_family_are_app_owned() {
        for name in PROVIDER_CREDENTIAL_ENV {
            assert!(is_app_credential_name(name), "{name}");
        }
        assert!(is_app_credential_name("TELEGRAM_BOT_TOKEN"));
        assert!(is_app_credential_name("RUSTAIN_MCP_HTTP_AUTH_TOKEN"));
        assert!(is_app_credential_name(
            "RUSTAIN_MCP_HTTP_AUTH_TOKEN_REMOTE_CI"
        ));
    }

    #[test]
    fn ordinary_names_are_not_app_owned() {
        for name in ["PATH", "HOME", "LANG", "MY_SERVER_TOKEN", "openai_api_key"] {
            assert!(!is_app_credential_name(name), "{name}");
        }
    }

    #[test]
    fn configured_names_join_the_same_policy_once_registered() {
        let name = "Q6_UNIT_CONFIGURED_CUSTOM_KEY";
        assert!(!is_app_credential_name(name));
        register_configured_credential_names([name, "   "]);
        assert!(is_app_credential_name(name));
        assert!(known_credential_names().iter().any(|known| known == name));
        assert!(!is_app_credential_name(""));
    }
}
