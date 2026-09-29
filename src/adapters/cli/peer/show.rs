//! `rustain peer show <alias>` — both keys whole (Story 18.4b, AC5).
//!
//! AC4 sends the operator here to compare a mismatch, so nothing on this surface
//! is truncated. Read-only.

use std::io::Write;

use anyhow::Result;

use crate::adapters::cli::peer::rows::{sanitize_for_terminal, show_text};
use crate::domain::services::peer_admission::{PeerRoster, configured_peer_by_alias, peer_roster};

/// Render one entry, or say which of the four allowlist states prevented it.
pub fn render_peer_show(
    alias: &str,
    config: &crate::domain::models::P2pConfigState,
    relay: &crate::domain::models::RelayConfigState,
    out: &mut impl Write,
) -> Result<()> {
    let peers = match config {
        crate::domain::models::P2pConfigState::Present(peers) => peers,
        other => {
            // Reuse the four-state copy rather than inventing a second wording
            // for "there is nothing to show".
            write!(
                out,
                "{}",
                crate::adapters::cli::peer::rows::render_roster(&peer_roster(other), None, relay)
            )?;
            return Ok(());
        }
    };
    match configured_peer_by_alias(alias, peers) {
        Some(spec) => {
            let peer_id = spec.pinned_identity();
            write!(
                out,
                "{}",
                show_text(&spec.id, spec.pinned_key.as_ref(), peer_id.as_ref(), relay)
            )?;
        }
        None => {
            writeln!(
                out,
                "No entry named {:?} in .rustain/p2p.json. `rustain peer list` shows what is\nconfigured.",
                sanitize_for_terminal(alias)
            )?;
            // Naming the four states here too keeps a populated-but-missing
            // alias distinct from an absent or unreadable allowlist.
            if let PeerRoster::Populated(rows) = peer_roster(config) {
                writeln!(out, "{} configured.", rows.len())?;
            }
        }
    }
    Ok(())
}
