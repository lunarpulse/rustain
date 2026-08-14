//! `rustain peer list` — the transport roster (Story 18.4b, AC5).
//!
//! Read-only over `.rustain/p2p.json`. ⛔ Not the room: a peer is a transport
//! principal, never a room row, and this surface asserts no binding between a
//! room node and a peer.

use std::io::Write;

use anyhow::Result;
use serde::Serialize;

use crate::adapters::cli::peer::rows::{
    PEER_LIST_SCHEMA_VERSION, render_roster, sanitize_for_terminal,
};
use crate::domain::services::peer_admission::PeerRoster;

/// `--json` twin, following the `session list` schema-version idiom.
#[derive(Debug, Serialize)]
struct PeerListJson<'a> {
    schema_version: &'a str,
    config_path: &'a str,
    /// One of `absent`, `empty`, `unreadable`, `populated` — the four allowlist
    /// input states, kept distinct here for the same reason the human render
    /// keeps them distinct.
    state: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    peers: Vec<PeerRowJson>,
    /// The reach limit, stated wherever peer configuration is shown.
    reach: &'a str,
    /// ⛔ Present so a consumer cannot mistake this list for connection state.
    configuration_not_connection_status: bool,
}

#[derive(Debug, Serialize)]
struct PeerRowJson {
    alias: String,
    /// `null` when the entry carries no pinned key: absent facts are null, ⛔
    /// never zeroed and never inferred.
    peer_id: Option<String>,
    pinned: bool,
}

const CONFIG_PATH: &str = ".rustain/p2p.json";
const REACH: &str = "directly-addressable peers only; relay disabled";

/// Render the roster, human or JSON.
pub fn render_peer_list(
    roster: &PeerRoster,
    listen: Option<bool>,
    json: bool,
    out: &mut impl Write,
) -> Result<()> {
    if !json {
        write!(out, "{}", render_roster(roster, listen))?;
        return Ok(());
    }
    let (state, reason, peers) = match roster {
        PeerRoster::Absent => ("absent", None, Vec::new()),
        PeerRoster::Empty => ("empty", None, Vec::new()),
        PeerRoster::Unreadable { reason } => (
            "unreadable",
            Some(sanitize_for_terminal(reason)),
            Vec::new(),
        ),
        PeerRoster::Populated(rows) => (
            "populated",
            None,
            rows.iter()
                .map(|row| PeerRowJson {
                    alias: sanitize_for_terminal(&row.alias),
                    peer_id: row.peer_id.as_ref().map(|id| id.as_str().to_owned()),
                    pinned: row.is_pinned(),
                })
                .collect(),
        ),
    };
    let payload = PeerListJson {
        schema_version: PEER_LIST_SCHEMA_VERSION,
        config_path: CONFIG_PATH,
        state,
        reason,
        peers,
        reach: REACH,
        configuration_not_connection_status: true,
    };
    writeln!(out, "{}", serde_json::to_string_pretty(&payload)?)?;
    Ok(())
}

/// Whether the listener flag could be read, for the header. A failed read is
/// reported as unknown rather than guessed as off: `listen: false` is a
/// security-relevant claim.
#[must_use]
pub fn listen_flag(path: &std::path::Path) -> Option<bool> {
    crate::adapters::p2p_config::p2p_listener_requested(path).ok()
}
