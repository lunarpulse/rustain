//! Inherited-environment omission at the final command-construction boundary
//! (Story Q-6 AC1; D-23/EQ-4, D-35).
//!
//! The Bash tool and the MCP stdio launcher both call [`omit_app_credentials`]
//! on the command they are about to spawn. The parent process environment is
//! neither mutated nor inspected for output: the child simply never inherits an
//! app-owned credential variable. Everything else the child needs — `PATH`,
//! locale and runtime settings — is inherited unchanged.
//!
//! The name list is `domain::services::app_credentials`; this module only
//! applies it to a `Command`.

use crate::domain::services::app_credentials::{MCP_AUTH_ENV_PREFIX, known_credential_names};

/// Remove every app-owned credential variable from `cmd`'s inherited
/// environment. Variables set explicitly on `cmd` *after* this call are kept:
/// an MCP server's own `env` entries are applied afterwards.
pub fn omit_app_credentials(cmd: &mut tokio::process::Command) {
    for name in known_credential_names() {
        cmd.env_remove(name);
    }
    // The MCP auth-token family has a per-server suffix, so it is matched by
    // prefix against the names actually present.
    for (name, _) in std::env::vars_os() {
        if name
            .to_str()
            .is_some_and(|name| name.starts_with(MCP_AUTH_ENV_PREFIX))
        {
            cmd.env_remove(name);
        }
    }
}
