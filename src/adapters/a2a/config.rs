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
    agents: BTreeMap<String, PeerInput>,
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
