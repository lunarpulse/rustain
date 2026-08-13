use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

pub use crate::domain::models::P2pConfigState;
use crate::domain::models::{P2pPeerSpec, PeerId, PinnedKey};
use crate::domain::services::peer_dial::{PeerDialVerdict, peer_dial_verdict};
use crate::infrastructure::paths::workspace_p2p_config_path;

// Unlike the A2A parser, this transport allowlist is deliberately fail-closed:
// reject unknown fields so a typo cannot silently disable operator policy.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceRoot {
    #[serde(default)]
    listen: bool,
    #[serde(default)]
    agents: BTreeMap<String, PeerInput>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PeerInput {
    #[serde(default, rename = "pinnedKey", alias = "pinned_key")]
    pinned_key: Option<PinnedKeyInput>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PinnedKeyInput {
    alg: String,
    x: String,
    #[serde(default)]
    kid: Option<String>,
}

/// Read the listener trigger from `.rustain/p2p.json`.
///
/// This stays outside the `p2p` feature gate so a binary that cannot honor an
/// enabled listener fails loudly instead of silently ignoring operator intent.
pub fn p2p_listener_requested(path: &Path) -> Result<bool, String> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("failed to read {}: {error}", path.display())),
    };
    let root: WorkspaceRoot = serde_json::from_str(&content)
        .map_err(|error| format!("invalid JSON in {}: {error}", path.display()))?;
    Ok(root.listen)
}

/// Load the transport-specific allowlist without a feature gate.
///
/// A build without the `p2p` adapter must still reject malformed operator
/// configuration loudly instead of silently treating it as an empty list.
pub fn load_workspace_p2p_config(path: &Path) -> P2pConfigState {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return P2pConfigState::Absent;
        }
        Err(error) => {
            return P2pConfigState::Malformed {
                reason: format!("failed to read {}: {error}", path.display()),
            };
        }
    };
    let root: WorkspaceRoot = match serde_json::from_str(&content) {
        Ok(root) => root,
        Err(error) => {
            return P2pConfigState::Malformed {
                reason: format!("invalid JSON in {}: {error}", path.display()),
            };
        }
    };

    let mut peers = Vec::with_capacity(root.agents.len());
    for (id, input) in root.agents {
        if id.trim().is_empty() {
            return P2pConfigState::Malformed {
                reason: "transport peer id must not be empty or whitespace".to_owned(),
            };
        }
        let pinned_key = match input.pinned_key {
            Some(pin) => match PinnedKey::parse(&pin.alg, pin.x, pin.kid) {
                Ok(pin) => Some(pin),
                Err(error) => {
                    return P2pConfigState::Malformed {
                        reason: format!("invalid transport peer {id:?}: {error}"),
                    };
                }
            },
            None => None,
        };
        peers.push(P2pPeerSpec::new(id, pinned_key));
    }
    P2pConfigState::Present(peers)
}

/// Re-read the transport list and apply the pure verdict core.
///
/// This seam intentionally reads only `.rustain/p2p.json`; the independent
/// `.rustain/a2a.json` HTTP admission policy cannot relax transport access.
pub fn peer_dial_verdict_from_workspace(workspace: &Path, peer_id: &PeerId) -> PeerDialVerdict {
    let config = load_workspace_p2p_config(&workspace_p2p_config_path(workspace));
    peer_dial_verdict(peer_id, &config)
}
