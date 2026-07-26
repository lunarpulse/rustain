use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

use crate::domain::models::{A2aPeerSource, A2aPeerSpec, A2aPeerSpecError, PinnedKey, RedactedUrl};

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum A2aConfigError {
    #[error("failed to read {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid JSON in {path}: {source}")]
    Json {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("invalid A2A peer {peer:?}: {source}")]
    InvalidPeer {
        peer: String,
        #[source]
        source: A2aPeerSpecError,
    },
    #[error("invalid A2A peer {peer:?}: {reason}")]
    MalformedPeer { peer: String, reason: String },
}

#[derive(Debug, Deserialize)]
struct WorkspaceRoot {
    #[serde(default)]
    agents: BTreeMap<String, PeerInput>,
    #[serde(default)]
    server: Option<A2aServerConfig>,
}

/// Operator policy for tasks arriving from remote agents (Story 18.1b).
///
/// Deliberately **not** wired to `[subagents] auto_approve`: that knob governs
/// subagents *we* launched, and `ApprovalRuntime` explicitly refuses to apply it
/// to `ApprovalSource::RemotePeer`. Inheriting it here would silently hand a
/// network peer the auto-approval an operator granted to their own subagents.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum A2aAdmissionPolicy {
    /// Refuse every inbound task. The default: an endpoint that starts running
    /// strangers' work the moment it is reachable is a footgun.
    #[default]
    Deny,
    /// Ask the operator. Answers `auth-required` on the wire — never blocks.
    Ask,
    /// Accept without asking.
    Allow,
}

/// The `server` block of `.rustain/a2a.json`.
///
/// Parsed without the `a2a` feature so a misconfigured build fails loudly at
/// startup instead of silently ignoring the operator's intent — the same reason
/// peer parsing is ungated. It therefore names no `rustls`, `axum`, or
/// `SecretString` type: only the *environment-variable names* keys are read
/// from, so the secrets never live in a file that gets committed.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct A2aServerConfig {
    #[serde(default)]
    pub admission: A2aAdmissionPolicy,
    /// Environment variable holding the legacy shared API key. It remains part
    /// of the effective key set for backwards-compatible deployments.
    #[serde(default, rename = "apiKeyEnv", alias = "api_key_env")]
    pub api_key_env: Option<String>,
    /// Additional environment-variable names holding accepted API keys.
    ///
    /// The effective set is the union of this list and [`Self::api_key_env`].
    #[serde(default, rename = "apiKeys", alias = "api_keys")]
    pub api_keys: Option<Vec<String>>,
    /// Public host and port clients should use for this listener, for example
    /// `a2a.example.com:8443`. Required for wildcard binds.
    #[serde(default, rename = "advertisedHost", alias = "advertised_host")]
    pub advertised_host: Option<String>,
    #[serde(default)]
    pub tls: Option<A2aTlsConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct A2aTlsConfig {
    /// PEM certificate chain.
    pub cert: std::path::PathBuf,
    /// PEM private key (PKCS#8, PKCS#1 or SEC1).
    pub key: std::path::PathBuf,
}

#[derive(Debug, Deserialize)]
struct PeerInput {
    url: RedactedUrl,
    #[serde(default, rename = "pinnedKey", alias = "pinned_key")]
    pinned_key: Option<PinnedKeyInput>,
}

#[derive(Debug, Deserialize)]
struct PinnedKeyInput {
    alg: String,
    x: String,
    #[serde(default)]
    kid: Option<String>,
}

impl PinnedKeyInput {
    fn parse(self) -> Result<PinnedKey, A2aPeerSpecError> {
        PinnedKey::parse(&self.alg, self.x, self.kid)
    }
}

pub fn parse_workspace_a2a_config(path: &Path) -> Result<Vec<A2aPeerSpec>, A2aConfigError> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    let content = std::fs::read_to_string(path).map_err(|source| A2aConfigError::Read {
        path: path.display().to_string(),
        source,
    })?;
    let root: WorkspaceRoot =
        serde_json::from_str(&content).map_err(|source| A2aConfigError::Json {
            path: path.display().to_string(),
            source,
        })?;

    root.agents
        .into_iter()
        .map(|(id, peer)| build_spec(id, peer, A2aPeerSource::Workspace))
        .collect()
}

/// Read the `server` block from the workspace A2A config.
///
/// `Ok(None)` means "no `server` block", which is the loopback-only,
/// refuse-every-task posture Story 18.1a shipped.
pub fn parse_workspace_a2a_server_config(
    path: &Path,
) -> Result<Option<A2aServerConfig>, A2aConfigError> {
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(path).map_err(|source| A2aConfigError::Read {
        path: path.display().to_string(),
        source,
    })?;
    let root: WorkspaceRoot =
        serde_json::from_str(&content).map_err(|source| A2aConfigError::Json {
            path: path.display().to_string(),
            source,
        })?;
    Ok(root.server)
}

pub fn extract_profile_a2a_peers(
    tools_config: Option<&toml::Value>,
    profile_name: &str,
) -> Result<Vec<A2aPeerSpec>, A2aConfigError> {
    let Some(a2a_value) = tools_config.and_then(|value| value.get("a2a")) else {
        return Ok(Vec::new());
    };
    let a2a_table = a2a_value
        .as_table()
        .ok_or_else(|| A2aConfigError::MalformedPeer {
            peer: "<profile>".to_owned(),
            reason: "a2a must be a TOML table".to_owned(),
        })?;

    a2a_table
        .iter()
        .map(|(id, value)| {
            let table = value
                .as_table()
                .ok_or_else(|| A2aConfigError::MalformedPeer {
                    peer: id.clone(),
                    reason: "peer entry must be a TOML table".to_owned(),
                })?;
            let url = table
                .get("url")
                .and_then(toml::Value::as_str)
                .ok_or_else(|| A2aConfigError::MalformedPeer {
                    peer: id.clone(),
                    reason: "url must be a string".to_owned(),
                })?;
            let pinned_key = table
                .get("pinned_key")
                .or_else(|| table.get("pinnedKey"))
                .map(|value| parse_profile_pin(id, value))
                .transpose()?;

            let spec = A2aPeerSpec {
                id: id.clone(),
                url: RedactedUrl::from(url),
                pinned_key,
                source: A2aPeerSource::Profile {
                    profile_name: profile_name.to_owned(),
                },
            };
            spec.validate_id()
                .map_err(|source| A2aConfigError::InvalidPeer {
                    peer: id.clone(),
                    source,
                })?;
            Ok(spec)
        })
        .collect()
}

pub fn merge_a2a_specs(workspace: Vec<A2aPeerSpec>, profile: Vec<A2aPeerSpec>) -> Vec<A2aPeerSpec> {
    let mut merged = BTreeMap::new();
    for spec in profile {
        merged.insert(spec.id.clone(), spec);
    }
    for spec in workspace {
        merged.insert(spec.id.clone(), spec);
    }
    merged.into_values().collect()
}

fn build_spec(
    id: String,
    input: PeerInput,
    source: A2aPeerSource,
) -> Result<A2aPeerSpec, A2aConfigError> {
    let pinned_key = input
        .pinned_key
        .map(PinnedKeyInput::parse)
        .transpose()
        .map_err(|source| A2aConfigError::InvalidPeer {
            peer: id.clone(),
            source,
        })?;
    let spec = A2aPeerSpec {
        id: id.clone(),
        url: input.url,
        pinned_key,
        source,
    };
    spec.validate_id()
        .map_err(|source| A2aConfigError::InvalidPeer { peer: id, source })?;
    Ok(spec)
}

fn parse_profile_pin(id: &str, value: &toml::Value) -> Result<PinnedKey, A2aConfigError> {
    let table = value
        .as_table()
        .ok_or_else(|| A2aConfigError::MalformedPeer {
            peer: id.to_owned(),
            reason: "pinned_key must be a TOML table".to_owned(),
        })?;
    let required = |name: &str| {
        table
            .get(name)
            .and_then(toml::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| A2aConfigError::MalformedPeer {
                peer: id.to_owned(),
                reason: format!("pinned_key.{name} must be a string"),
            })
    };
    let alg = required("alg")?;
    let x = required("x")?;
    let kid = table
        .get("kid")
        .and_then(toml::Value::as_str)
        .map(str::to_owned);
    PinnedKey::parse(&alg, x, kid).map_err(|source| A2aConfigError::InvalidPeer {
        peer: id.to_owned(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_config_unions_legacy_and_additional_key_environment_names() {
        let dir = tempfile::tempdir().expect("temp workspace");
        let path = dir.path().join("a2a.json");
        std::fs::write(
            &path,
            r#"{
                "server": {
                    "admission": "allow",
                    "apiKeyEnv": "A2A_LEGACY_KEY",
                    "apiKeys": ["A2A_ROTATED_KEY", "A2A_BACKUP_KEY"],
                    "advertisedHost": "a2a.example.com:8443"
                }
            }"#,
        )
        .expect("write config");

        let config = parse_workspace_a2a_server_config(&path)
            .expect("parse")
            .expect("server block");
        assert_eq!(config.admission, A2aAdmissionPolicy::Allow);
        assert_eq!(config.api_key_env.as_deref(), Some("A2A_LEGACY_KEY"));
        assert_eq!(
            config.api_keys,
            Some(vec![
                "A2A_ROTATED_KEY".to_owned(),
                "A2A_BACKUP_KEY".to_owned()
            ])
        );
        assert_eq!(
            config.advertised_host.as_deref(),
            Some("a2a.example.com:8443")
        );
    }

    #[test]
    fn server_config_accepts_snake_case_aliases() {
        let dir = tempfile::tempdir().expect("temp workspace");
        let path = dir.path().join("a2a.json");
        std::fs::write(
            &path,
            r#"{
                "server": {
                    "api_key_env": "A2A_LEGACY_KEY",
                    "api_keys": ["A2A_ROTATED_KEY"],
                    "advertised_host": "a2a.internal:9443"
                }
            }"#,
        )
        .expect("write config");

        let config = parse_workspace_a2a_server_config(&path)
            .expect("parse")
            .expect("server block");
        assert_eq!(config.api_key_env.as_deref(), Some("A2A_LEGACY_KEY"));
        assert_eq!(config.api_keys, Some(vec!["A2A_ROTATED_KEY".to_owned()]));
        assert_eq!(config.advertised_host.as_deref(), Some("a2a.internal:9443"));
    }
}
