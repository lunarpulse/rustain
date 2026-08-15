//! Pure copy and row builders for the `peer` family (Story 18.4b).
//!
//! Effect-free: no file is read, no journal is appended, nothing is printed.
//! Both faces render exactly these strings — `rustain peer …` through
//! `cli/peer/*.rs` and `/peer …` through
//! `infrastructure::runtime::peer_bridge`. Divergence between the faces is the
//! defect this shape prevents, exactly as `cli/team/rows.rs` prevents it for the
//! transparency log.
//!
//! # Wording discipline
//!
//! Every string here is inside the ceiling
//! `conformance_p2p_ingress.rs::every_p2p_operator_string_stays_within_the_wording_ceiling`
//! scans, because AC7 adds this module to its `owned_modules`. Three of the
//! banned words are the natural ones for this surface, so they are named here to
//! keep them out: the shipped word for a pinned peer is **pinned** (never the
//! `TrustTier::Verified` spelling), a key the operator holds locally is a
//! **signing key** or **local identity key**, and a read failure is a **failed
//! read**.

use std::fmt::Write as _;

use crate::adapters::cli::peer::ping::PingRefusal;
use crate::domain::models::{
    FrameOutcome, PeerAdmissionOutcome, PeerId, PeerTicket, PinnedKey, peer_fingerprint,
};
use crate::domain::services::peer_admission::{PeerRoster, PeerRosterRow, ROTATED_KEY_DOCTRINE};

/// Schema version for `peer list --json`, following the `session list` idiom.
pub const PEER_LIST_SCHEMA_VERSION: &str = "1.0";

/// What the operator is looking at, said once and reused.
const CONFIG_LABEL: &str = ".rustain/p2p.json";

/// The reach limit, matching the shipped listener line word for word
/// (`UX-DR-PT-07`). ⛔ No connection-state indicator: `relayed` cannot occur
/// under this cut's endpoint preset, `direct` is never observed because no path
/// observation is performed, and an unreachable dial is an address-map miss
/// rather than a network verdict.
const REACH_LIMIT: &str = "Reach limit: directly-addressable peers only; relay disabled.";

/// How the fingerprint's encoding is named wherever one is shown.
///
/// Three encodings of one key already reach operators — the base64url `x` in
/// the config, the hex peer id, and the transport's own raw-key hex — and they
/// are not interchangeable, so every surface says which one it is showing
/// (ruling A6).
const FINGERPRINT_ENCODING: &str =
    "peer id digest: sha-256 of the key, hex, head…tail; `peer show` prints it whole";

/// Strip terminal control sequences from an alias before it reaches a terminal.
///
/// A `p2p.json` alias is the operator's own map key rather than remote-supplied,
/// so the reason to sanitize is not spoofing by a peer: it is that a
/// **hand-edited config file is untrusted input to a terminal**. A ticket's
/// suggested name *is* remote-supplied, and it is sanitized for the original
/// reason.
#[must_use]
pub fn sanitize_for_terminal(value: &str) -> String {
    crate::adapters::cli::session::list::sanitize_title(value)
}

/// The 12-column head…tail form of a peer id, or `—` when there is no pin.
#[must_use]
pub fn short_peer_id(peer_id: Option<&PeerId>) -> String {
    match peer_id {
        Some(peer_id) => peer_fingerprint(peer_id),
        None => "—".to_owned(),
    }
}

/// Render the roster in the existing `- <alias> (<id>): <fact>` grammar, the
/// one already used by `team_command.rs` and `room_command.rs`.
///
/// All four allowlist states render distinctly, and **empty is not an error**:
/// an allowlist that is present and empty is a well-formed "admit nobody" and a
/// deliberate posture.
#[must_use]
pub fn render_roster(roster: &PeerRoster, listen: Option<bool>) -> String {
    let listener = match listen {
        Some(true) => "    listener: on",
        Some(false) => "    listener: off",
        None => "",
    };
    let mut out = format!("Transport peers — {CONFIG_LABEL}{listener}\n");
    match roster {
        PeerRoster::Absent => {
            out.push_str("\nNo peer allowlist. This host admits no peer.\n");
        }
        PeerRoster::Empty => {
            out.push_str("\nAllowlist present and empty — admits nobody.\n");
        }
        PeerRoster::Unreadable { reason } => {
            let _ = writeln!(
                out,
                "\nAllowlist unreadable: {}. This host admits no peer.",
                sanitize_for_terminal(reason)
            );
        }
        PeerRoster::Populated(rows) => {
            out.push('\n');
            let alias_width = rows
                .iter()
                .map(|row| sanitize_for_terminal(&row.alias).chars().count())
                .max()
                .unwrap_or(0);
            for row in rows {
                let _ = writeln!(out, "{}", roster_line(row, alias_width));
            }
            let _ = writeln!(
                out,
                "\n{} configured. Admission is checked per frame; this list is\n\
                 configuration, not connection status.",
                rows.len()
            );
        }
    }
    out.push('\n');
    out.push_str(REACH_LIMIT);
    out.push('\n');
    out
}

/// One roster row.
///
/// **Status precedes the id in the narrow variant**, inherited from
/// `transparency_panel.rs`: the row is ordered so the decision survives
/// truncation. ⛔ No column for which the domain holds no fact — no last-seen,
/// no connection state, no tier. An absent fact renders `—`, never a zero and
/// never a fabricated date.
#[must_use]
pub fn roster_line(row: &PeerRosterRow, alias_width: usize) -> String {
    let alias = sanitize_for_terminal(&row.alias);
    let pad = alias_width.saturating_sub(alias.chars().count());
    let id = short_peer_id(row.peer_id.as_ref());
    if row.is_pinned() {
        format!("- {alias}{:pad$} ({id}): pinned", "", pad = pad)
    } else {
        format!(
            "- {alias}{:pad$} ({id}): no pinned key — admits nobody",
            "",
            pad = pad
        )
    }
}

/// The `peer add` confirm card (`UX-DR-PT-02`).
///
/// It is a **local configuration act**, not a consent decision: it answers *is
/// this the key I meant to pin?*, the same question the HTTP path already asks
/// in `a2a/client.rs`. ⛔ Never raised by an inbound connection, so no second
/// door appears beside the remote-peer approval front door.
///
/// # Why this card shows the addresses and every other surface shows a count
///
/// This is the **only human checkpoint** before this host dials what a stranger
/// named, and "reachable at 1 direct address" cannot tell `203.0.113.7:4433`
/// apart from `169.254.169.254:80`. So the card renders the addresses
/// themselves; the roster, the logs and every non-consent surface stay
/// count-only, and the blob itself stays hand-off-sensitive.
///
/// The addresses are **claimed** by the issuer. The ticket is self-signed, so the
/// signature proves key possession and nothing about who owns those sockets — ⛔
/// the card never says otherwise.
#[must_use]
pub fn confirm_card_text(alias: &str, ticket: &PeerTicket, peer_id: &PeerId) -> String {
    let alias = sanitize_for_terminal(alias);
    let fingerprint = peer_fingerprint(peer_id);
    let mut out = format!("Add peer — {alias}\n\n");
    let _ = writeln!(
        out,
        "  fingerprint    {fingerprint}   ({FINGERPRINT_ENCODING})"
    );
    let _ = writeln!(out, "  reachable at   {}", reachable_clause(ticket));
    for address in
        crate::domain::services::peer_reach_filter::describe_ticket_reach(&ticket.addresses)
    {
        let _ = writeln!(out, "                 claimed: {address}");
    }
    let _ = writeln!(out, "  ticket expires {}", expiry_label(ticket.not_after));
    if let Some(name) = ticket.suggested_name.as_deref() {
        let _ = writeln!(
            out,
            "  they call it   {} (their suggestion; your alias above is what gets written)",
            sanitize_for_terminal(name)
        );
    }
    out.push_str(
        "\nA ticket conveys reachability and a candidate identity, never trust.\n\
         Confirming pins this key. It says who may reach this host — not that\n\
         what they send is true.\n\n\
         Awaiting your decision.  [y] Confirm and pin  [n] Cancel (Esc)\n",
    );
    out
}

/// The key-mismatch alarm (`UX-DR-PT-01`, test gate 13) — AC4's headline.
///
/// Rendered as a never-truncated `FeedbackLevel::Error` block. ⛔ It binds **no
/// key at all** and is deliberately not routed through the apply card's
/// dispatch: that table is single-sourced and consulted mode-blind, so a card
/// painting no `y` would still accept on `y` and rebind the pin — the exact
/// single-keystroke rebind this surface exists to forbid.
///
/// Both fingerprints render head…tail so **both ends stay checkable**, and the
/// only accept path is two separate verbs.
#[must_use]
pub fn key_mismatch_text(alias: &str, on_file: &PeerId, offered: &PeerId) -> String {
    let alias = sanitize_for_terminal(alias);
    let on_file = peer_fingerprint(on_file);
    let offered = peer_fingerprint(offered);
    // ⛔ States the observation, never the cause: no attack, no compromise, no
    // interception, no impersonation. A key change is indistinguishable from a
    // legitimate rotation and the copy says so.
    format!(
        "Key mismatch — {alias}\n\n\
         The key pinned for this peer no longer matches the one this\n\
         ticket offers. Nothing was changed.\n\n\
         \x20 on file   {on_file}\n\
         \x20 offered   {offered}\n\
         \x20           ({FINGERPRINT_ENCODING})\n\n\
         {doctrine}. This may be a routine key\n\
         rotation or a different party; this product does not tell them\n\
         apart and does not guess.\n\n\
         To see both keys in full:   rustain peer show {alias}\n\
         To accept the new key:      rustain peer revoke {alias}\n\
         \x20                           then import this ticket again.",
        doctrine = capitalize_first(ROTATED_KEY_DOCTRINE),
    )
}

/// The revocation row (`UX-DR-PT-05`).
///
/// States the one true observable and **concedes what it cannot undo**. ⛔ Never
/// claims a connection closed, a session ended, or a key rotated: per-frame
/// admission makes removal immediate, and that is all it makes.
#[must_use]
pub fn revocation_text(alias: &str, peer_id: Option<&PeerId>) -> String {
    let alias = sanitize_for_terminal(alias);
    let id = short_peer_id(peer_id);
    format!(
        "Recorded revocation of {alias} ({id}).\n\
         Their next frame is refused; no restart needed.\n\n\
         What this does not do: it does not close a connection that is\n\
         already open, and it does not reach anything they already\n\
         received. Bytes already delivered stay delivered."
    )
}

/// The idempotent no-op line, following the room-role seam word for word.
#[must_use]
pub fn nothing_changed_text(what: &str, alias: &str) -> String {
    format!(
        "{what} for {}; nothing changed.",
        sanitize_for_terminal(alias)
    )
}

/// `peer show` — **both keys whole**, because a 12-column head…tail form is a
/// recognition aid and not a comparison aid, and AC4 tells the operator to come
/// here to compare.
#[must_use]
pub fn show_text(alias: &str, pinned: Option<&PinnedKey>, peer_id: Option<&PeerId>) -> String {
    let alias = sanitize_for_terminal(alias);
    let mut out = format!("{alias} — {CONFIG_LABEL}\n\n");
    match (pinned, peer_id) {
        (Some(pinned), Some(peer_id)) => {
            let _ = writeln!(out, "pinned key (JWK x, base64url)\n  {}", pinned.x);
            if let Some(kid) = pinned.kid.as_deref() {
                let _ = writeln!(out, "  key id: {}", sanitize_for_terminal(kid));
            }
            let _ = writeln!(
                out,
                "\npeer id (sha-256 of that key, hex, whole)\n  {peer_id}"
            );
            let _ = writeln!(
                out,
                "  short form used in rows and cards: {}",
                peer_fingerprint(peer_id)
            );
            out.push_str(
                "\nBoth keys are printed whole on purpose: the 12-column head…tail form\n\
                 used in rows is a recognition aid, not a comparison aid. The short form\n\
                 elides the leading 1220, which is the same multihash header on every\n\
                 peer id in existence and would otherwise spend four columns on a\n\
                 constant.\n",
            );
        }
        _ => out.push_str(
            "No pinned key. This entry admits nobody — the alias is configured\n\
             but no key answers for it.\n",
        ),
    }
    out.push('\n');
    out.push_str(REACH_LIMIT);
    out.push('\n');
    out
}

/// The clause naming what a ticket says about reachability.
///
/// With no address configured it says exactly that, ⛔ never that the peer is
/// unreachable: an empty address list is a missing configuration, not a network
/// observation.
///
/// ⚠ **N counts transport addresses inside the bundle, not vector elements**
/// (Story 18.4d, D4). One `EndpointAddr` is one endpoint that may be reachable
/// several ways, and the operator is being told how many ways — so the shipped
/// `ticket.addresses.len()` would have rendered "1 direct address" for a bundle
/// holding three.
#[must_use]
pub fn reachable_clause(ticket: &PeerTicket) -> String {
    match crate::domain::services::peer_reach_filter::transport_address_count(&ticket.addresses) {
        0 => "no address is configured for this host".to_owned(),
        1 => "1 direct address".to_owned(),
        many => format!("{many} direct addresses"),
    }
}

// ── `peer ping` copy (Story 18.4d, AC5) ─────────────────────────────────────
//
// # The one rule these strings exist to enforce
//
// **accepted** and **refused** may be said ONLY when repeating a verdict this
// host actually received and validated. A frame that was written and never
// answered reports an unknown outcome. ⛔ Never *delivered*, never
// *acknowledged*, never *verified*, never *secure*: the transport carried bytes
// and a peer answered, and that is the whole of what is known.
//
// The refusal reason is rendered from the **class** the peer named, through the
// shared label table — never from remote-authored prose, so no stranger's text
// can reach an operator's terminal or slip past the wording ceiling.

/// One frame, reporting exactly what the verdict said.
#[must_use]
pub fn ping_single_text(alias: &str, peer_id: &PeerId, outcome: FrameOutcome) -> String {
    let alias = sanitize_for_terminal(alias);
    let fp = peer_fingerprint(peer_id);
    match outcome {
        FrameOutcome::Accepted => format!("Sent 1 frame to {alias} ({fp}) — accepted."),
        FrameOutcome::Refused(refusal) => format!(
            "Sent 1 frame to {alias} ({fp}) — refused: {}.",
            crate::domain::services::transparency::frame_refusal_label(refusal)
        ),
        FrameOutcome::Unanswered => format!(
            "Sent 1 frame to {alias} ({fp}) — outcome unknown; the peer did not answer. Check \
             the peer's transport-admission log."
        ),
    }
}

/// Every frame of a `--count` run was accepted, on one connection.
#[must_use]
pub fn ping_multi_text(alias: &str, peer_id: &PeerId, count: u32) -> String {
    format!(
        "Sent {count} frames to {} ({}) on one connection.",
        sanitize_for_terminal(alias),
        peer_fingerprint(peer_id)
    )
}

/// The honesty clause for a run whose guided retry transmitted more envelopes
/// than the frame count names. Empty when no retry happened, so the pinned
/// copy above stays byte-identical in the common case.
#[must_use]
pub fn ping_retry_clause(frames: u32, transmitted: u32) -> String {
    if transmitted <= frames {
        return String::new();
    }
    format!(" {transmitted} envelopes were transmitted; the peer guided one position retry.")
}

/// A `--count` run that stopped. ⛔ It never implies the remaining frames were
/// attempted: they were not.
#[must_use]
pub fn ping_partial_text(
    alias: &str,
    peer_id: &PeerId,
    accepted: u32,
    total: u32,
    failed_frame: u32,
    reason: &str,
) -> String {
    format!(
        "Sent {accepted} of {total} frames to {} ({}); frame {failed_frame} failed: {reason}. \
         Nothing further was attempted.",
        sanitize_for_terminal(alias),
        peer_fingerprint(peer_id)
    )
}

/// Why nothing was sent.
#[must_use]
pub fn ping_refusal_text(alias: &str, refusal: &PingRefusal) -> String {
    let alias = sanitize_for_terminal(alias);
    match refusal {
        PingRefusal::UnknownAlias => {
            format!("Cannot ping {alias}: unknown alias. Nothing was sent.")
        }
        PingRefusal::Unpinned => format!(
            "Cannot ping {alias}: no pinned key — this host has not admitted them. Nothing was \
             sent."
        ),
        PingRefusal::NoReach => format!(
            "Cannot ping {alias}: no network address on file — import a ticket that carries one. \
             Nothing was sent."
        ),
        PingRefusal::DialFailed { reason } => format!(
            "Cannot ping {alias}: dial failed ({}). Nothing was sent.",
            sanitize_for_terminal(reason)
        ),
        PingRefusal::FeatureDisabled => format!(
            "Cannot ping {alias}: this build was compiled without the peer transport. Nothing \
             was sent."
        ),
        PingRefusal::LocalFault { reason } => format!(
            "Cannot ping {alias}: this host could not prepare the frame ({}). Nothing was sent.",
            sanitize_for_terminal(reason)
        ),
    }
}

/// The line printed after a reach-only refresh of an already-pinned peer.
///
/// ⛔ It does not claim a new pin: the key on file is unchanged, and saying
/// otherwise would describe a trust decision the operator did not make.
#[must_use]
pub fn reach_refreshed_text(alias: &str, peer_id: &PeerId) -> String {
    let alias = sanitize_for_terminal(alias);
    format!(
        "Updated the network address on file for {alias} ({}).\n\
         The pinned key is unchanged and no new trust decision was recorded.\n\
         `rustain peer ping {alias}` now dials the address this ticket carried.",
        peer_fingerprint(peer_id)
    )
}

/// The reach-refresh confirm card: same key, new address.
///
/// A separate card because it asks a different question. The `peer add` card asks
/// *is this the key I meant to pin?*; this one asks *should this host dial these
/// addresses instead?* — and the key is not up for decision, so the card says so.
#[must_use]
pub fn reach_refresh_card_text(alias: &str, ticket: &PeerTicket, peer_id: &PeerId) -> String {
    let alias = sanitize_for_terminal(alias);
    let mut out = format!("Update address — {alias}\n\n");
    let _ = writeln!(
        out,
        "  fingerprint    {}   ({FINGERPRINT_ENCODING})",
        peer_fingerprint(peer_id)
    );
    let _ = writeln!(out, "  already pinned yes — this key is unchanged");
    let _ = writeln!(out, "  reachable at   {}", reachable_clause(ticket));
    for address in
        crate::domain::services::peer_reach_filter::describe_ticket_reach(&ticket.addresses)
    {
        let _ = writeln!(out, "                 claimed: {address}");
    }
    let _ = writeln!(out, "  ticket expires {}", expiry_label(ticket.not_after));
    out.push_str(
        "\nThis changes only where this host dials them. Their key stays pinned\n\
         exactly as it is, and admission is unchanged.\n\n\
         Awaiting your decision.  [y] Update the address  [n] Cancel (Esc)\n",
    );
    out
}

/// Human-readable expiry. Local time, matching `session list`'s format.
#[must_use]
pub fn expiry_label(not_after: i64) -> String {
    use chrono::{DateTime, Local};
    let stamp = DateTime::from_timestamp(not_after, 0).unwrap_or(DateTime::UNIX_EPOCH);
    stamp
        .with_timezone(&Local)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

/// The wire label a journaled admission outcome renders as, for `--json`.
#[must_use]
pub fn outcome_label(outcome: PeerAdmissionOutcome) -> &'static str {
    match outcome {
        PeerAdmissionOutcome::Pinned => "pinned",
        PeerAdmissionOutcome::Revoked => "revoked",
        PeerAdmissionOutcome::ImportRefused => "import-refused",
        PeerAdmissionOutcome::Unknown => "unknown",
    }
}

fn capitalize_first(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}
