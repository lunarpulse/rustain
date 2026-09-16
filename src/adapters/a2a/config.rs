use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

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
    /// The **name** of the environment variable holding this peer's API key.
    /// ⛔ Never the key. Additive: no `deny_unknown_fields` exists anywhere in
    /// this config, so an older roster keeps parsing and a newer one keeps
    /// loading on a build that predates the field.
    #[serde(default)]
    auth: Option<String>,
    /// Path to this peer's PEM trust anchor, relative to the roster root or
    /// absolute. ⛔ Not dereferenced here — `resolve_ca_cert_paths` joins it and
    /// the client loads it, so a missing file refuses at send time (`A27`).
    #[serde(default, rename = "caCert", alias = "ca_cert")]
    ca_cert: Option<String>,
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
            // `auth` is a variable NAME and `ca_cert` a path: both are read as
            // opaque strings and ⛔ never dereferenced here. `ca_cert` stays
            // relative until `resolve_ca_cert_paths` joins it to the roster
            // root (`A27`); snake_case is selected first, as `pinned_key` is.
            // A present value of the wrong TOML type is a malformed security
            // setting — ⛔ never silently dropped to `None` (a mistyped
            // `ca_cert = true` would otherwise load an UNANCHORED peer).
            let auth = match table.get("auth") {
                None => None,
                Some(value) => Some(value.as_str().map(str::to_owned).ok_or_else(|| {
                    A2aConfigError::MalformedPeer {
                        peer: id.clone(),
                        reason: "auth must be a string naming an environment variable".to_owned(),
                    }
                })?),
            };
            let ca_cert = match table.get("ca_cert").or_else(|| table.get("caCert")) {
                None => None,
                Some(value) => Some(value.as_str().map(PathBuf::from).ok_or_else(|| {
                    A2aConfigError::MalformedPeer {
                        peer: id.clone(),
                        reason: "ca_cert/caCert must be a string path to a PEM file".to_owned(),
                    }
                })?),
            };

            let peer_url = RedactedUrl::from(url);
            refuse_plaintext_anchor(id, &peer_url, ca_cert.is_some())?;
            let spec = A2aPeerSpec::new(
                id.clone(),
                peer_url,
                A2aPeerSource::Profile {
                    profile_name: profile_name.to_owned(),
                },
            )
            .with_pinned_key(pinned_key)
            .with_auth(auth)
            .with_ca_cert(ca_cert);
            spec.validate_id()
                .map_err(|source| A2aConfigError::InvalidPeer {
                    peer: id.clone(),
                    source,
                })?;
            Ok(spec)
        })
        .collect()
}

/// Resolve every relative `ca_cert` against the root that located the roster.
///
/// Story 19.14 `A27`. The root is passed **explicitly** because no root reaches
/// the client: the client adapter's constructor sees only the spec, and `TomlProfileResolver`
/// is the one place that knows which directory `.rustain/a2a.json` was found in.
/// Applied to the merged set, so workspace and profile peers resolve identically —
/// ⛔ a profile peer's anchor is **not** relative to the profile TOML's directory.
///
/// `Path::join` semantics only: a relative path is joined, an absolute path passes
/// through unchanged. ⛔ No `stat`, no `canonicalize` — a roster must load whether
/// or not the file exists, and an unloadable anchor refuses at send time.
pub fn resolve_ca_cert_paths(root: &Path, specs: &mut [A2aPeerSpec]) {
    for spec in specs {
        if let Some(ca_cert) = spec.ca_cert.as_ref() {
            if ca_cert.is_relative() {
                spec.ca_cert = Some(root.join(ca_cert));
            }
        }
    }
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

/// A trust anchor on a plaintext URL is silently inert: no TLS handshake ever
/// happens, so the anchor is never consulted and the operator's configured
/// trust restriction has no effect. ⛔ Fail loudly at load (code review
/// 2026-09-15, roundtable consensus — spec never ruled this combination), with
/// a message naming the peer and both remedies. An unparseable URL skips the
/// check: it is already refused by the client's `parse_and_validate_url`.
fn refuse_plaintext_anchor(
    id: &str,
    url: &RedactedUrl,
    anchor_present: bool,
) -> Result<(), A2aConfigError> {
    if anchor_present && matches!(url.parse_url(), Ok(parsed) if parsed.scheme() == "http") {
        return Err(A2aConfigError::MalformedPeer {
            peer: id.to_owned(),
            reason: "caCert requires an https URL: remove caCert or use https".to_owned(),
        });
    }
    Ok(())
}

fn build_spec(
    id: String,
    input: PeerInput,
    source: A2aPeerSource,
) -> Result<A2aPeerSpec, A2aConfigError> {
    refuse_plaintext_anchor(&id, &input.url, input.ca_cert.is_some())?;
    let pinned_key = input
        .pinned_key
        .map(PinnedKeyInput::parse)
        .transpose()
        .map_err(|source| A2aConfigError::InvalidPeer {
            peer: id.clone(),
            source,
        })?;
    let spec = A2aPeerSpec::new(id.clone(), input.url, source)
        .with_pinned_key(pinned_key)
        .with_auth(input.auth)
        .with_ca_cert(input.ca_cert.map(PathBuf::from));
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

    /// AC5 (B5): a malformed `a2a.json` must error, never silently fall back to
    /// `Default::default()` (= `Deny`) — that silent fallback was the
    /// authority-widening-warning mute the AC5 defect fixes. A trailing comma
    /// or smart quote must surface as a parse error so the daemon warns
    /// unconditionally. (The warning *logic* is covered by `policy_startup`'s
    /// `should_warn_auto_authority_widening` positive-control tests.)
    #[test]
    fn malformed_a2a_json_fails_rather_than_silently_defaulting_to_deny() {
        let dir = tempfile::tempdir().expect("temp workspace");
        let path = dir.path().join("a2a.json");
        std::fs::write(&path, "{ \"server\": { \"admission\": \"allow\", } }")
            .expect("write malformed config");
        parse_workspace_a2a_server_config(&path)
            .expect_err("malformed a2a.json must error, not silently default to Deny");
    }

    /// Code review 2026-09-15: an anchor on a plaintext URL is silently inert
    /// (no TLS handshake ever happens), so the combination is rejected at parse
    /// with both remedies named — on BOTH parsers.
    #[test]
    fn a_workspace_anchor_on_a_plaintext_url_is_rejected_loudly() {
        let dir = tempfile::tempdir().expect("temp workspace");
        let path = dir.path().join("a2a.json");
        std::fs::write(
            &path,
            r#"{"agents":{"dev":{"url":"http://localhost:9100","caCert":"certs/ca.pem"}}}"#,
        )
        .expect("write config");

        let error = parse_workspace_a2a_config(&path)
            .expect_err("http + caCert must not load as a silently unanchored peer");
        let text = error.to_string();
        assert!(text.contains("\"dev\""), "names the peer: {text}");
        assert!(
            text.contains("caCert requires an https URL: remove caCert or use https"),
            "names both remedies: {text}"
        );
    }

    #[test]
    fn a_profile_anchor_on_a_plaintext_url_is_rejected_loudly() {
        let value: toml::Value = toml::from_str(
            r#"
            [a2a.dev]
            url = "http://localhost:9100"
            ca_cert = "certs/ca.pem"
            "#,
        )
        .expect("profile tools config");

        let error = extract_profile_a2a_peers(Some(&value), "coding")
            .expect_err("http + caCert must not load as a silently unanchored peer");
        assert!(
            error
                .to_string()
                .contains("caCert requires an https URL: remove caCert or use https"),
            "names both remedies: {error}"
        );
    }

    #[test]
    fn an_https_anchor_still_loads() {
        let dir = tempfile::tempdir().expect("temp workspace");
        let path = dir.path().join("a2a.json");
        std::fs::write(
            &path,
            r#"{"agents":{"peer":{"url":"https://peer.example","caCert":"certs/ca.pem"}}}"#,
        )
        .expect("write config");

        let peers = parse_workspace_a2a_config(&path).expect("https + caCert loads");
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].ca_cert.as_deref(), Some(Path::new("certs/ca.pem")));
    }

    /// Code review 2026-09-15: a present but mistyped security field is a
    /// malformed peer — `and_then(Value::as_str)` silently turned `ca_cert =
    /// true` into an UNANCHORED peer on platform roots.
    #[test]
    fn a_non_string_profile_anchor_or_auth_fails_loud() {
        for toml_text in [
            "[a2a.peer]\nurl = \"https://peer.example\"\nca_cert = true\n",
            "[a2a.peer]\nurl = \"https://peer.example\"\ncaCert = 123\n",
            "[a2a.peer]\nurl = \"https://peer.example\"\nauth = 123\n",
        ] {
            let value: toml::Value = toml::from_str(toml_text).expect("profile tools config");
            let error = extract_profile_a2a_peers(Some(&value), "coding")
                .expect_err("a mistyped trust field must not silently vanish");
            assert!(
                matches!(error, A2aConfigError::MalformedPeer { .. }),
                "mistyped field is a malformed peer: {error}"
            );
        }
    }
}
