//! `rustain peer ping <alias>` — the first frame (Story 18.4d, AC5 / FR162).
//!
//! # What this verb is for
//!
//! Until this story, `PeerTransport::send_to` had **zero** non-test callers: the
//! whole cross-host transport existed with no producer, which is the
//! "mechanism without a trigger" class this epic keeps paying for. This verb is
//! that producer, and it is deliberately the smallest one that proves the claim:
//! resolve an alias, dial, send one signed frame, print what the peer said.
//!
//! # Zero authority
//!
//! A peer message is tainted content (FR151). This verb mints no
//! `CapabilityToken`, references none, and grants nothing. The frame's own
//! recipient is a peer-owned node on the far side; nothing about sending it
//! widens what anyone may do.
//!
//! # The wire identities are a permanent contract (D17)
//!
//! The receiver **materializes** `header.recipient` as a local node and
//! **permanently binds** `header.sender` to the transport peer. Both are therefore
//! protocol, not fixture detail, and both are pinned here:
//!
//! * `sender`    = `<this host's PeerId>/peer-ping`
//! * `recipient` = `<this host's PeerId>/peer-ping-recipient`
//! * `kind`      = `MessageKind::PeerMessage` — the only kind the delivery front
//!   door accepts.
//! * `body`      = the JSON string [`PING_BODY`]. A JSON *string* body, because an
//!   object is accepted only with a string `msg` field and a fixed shape is one
//!   fewer thing to get wrong.
//! * `not_after` = now + [`PING_TTL_MS`], in **wall milliseconds** — the unit the
//!   verify seam on this path uses (see `AttachServer::peer_envelope`), not the
//!   seconds a `PeerTicket` uses.
//! * `nonce` and `correlation_id` are **unique per frame**, so `--count 2` cannot
//!   be refused as its own nonce replay.
//!
//! Both identities are rooted at **this host's own** `PeerId`. That is not
//! decoration: it means a sender can only ever name a node inside its own
//! identity namespace, so this producer can never be the one that asks a receiver
//! to materialize a node it did not choose.

use crate::domain::models::PeerId;

/// Longest a ping frame stays valid, in wall milliseconds.
///
/// Short on purpose: the frame is a liveness probe, and a probe that stays
/// replayable for hours is a probe an observer can hold and re-present.
pub const PING_TTL_MS: i64 = 60_000;

/// The fixed frame body. ⛔ Not operator-supplied: this verb carries no content,
/// so there is nothing for an operator to be surprised by on the far side.
pub const PING_BODY: &str = "ping";

/// The sender path suffix, under this host's own `PeerId`.
pub const PING_SENDER_SUFFIX: &str = "peer-ping";

/// The recipient path suffix, under this host's own `PeerId`.
pub const PING_RECIPIENT_SUFFIX: &str = "peer-ping-recipient";

/// Why a ping never left this host. Every arm sends nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PingRefusal {
    /// No such alias in `.rustain/p2p.json`.
    UnknownAlias,
    /// The alias exists but no key answers for it, so it admits nobody and this
    /// host has no identity to dial.
    Unpinned,
    /// Pinned, but no address on file — nothing to dial.
    NoReach,
    /// Reach is on file, but every address it names is a relay this host does
    /// not use (Story 18.4c, D13).
    ///
    /// ⛔ Not a verdict about the peer: an offered relay is a claim, this
    /// host's configured set is the fact, and the two simply do not intersect.
    RelayNotConfigured,
    /// The dial itself failed. ⚠ Distinct from `Unreachable`, which means the
    /// address book had no entry at all.
    DialFailed { reason: String },
    /// This build has no transport adapter compiled in.
    FeatureDisabled,
    /// The local identity key or reach store could not be read.
    LocalFault { reason: String },
}

/// Parse `--interval`: a bare millisecond count, or a suffixed `500ms` / `2s`.
///
/// # Errors
///
/// Returns the operator-facing message. A zero interval is accepted — it means
/// "as fast as the connection allows", which is a real thing to ask for.
pub fn parse_interval(spec: &str) -> Result<std::time::Duration, String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err("--interval needs a duration, for example 500ms or 2s".to_owned());
    }
    let (digits, multiplier) = if let Some(rest) = spec.strip_suffix("ms") {
        (rest, 1)
    } else if let Some(rest) = spec.strip_suffix('s') {
        (rest, 1_000)
    } else if let Some(rest) = spec.strip_suffix('m') {
        (rest, 60_000)
    } else {
        (spec, 1)
    };
    let value: u64 = digits.trim().parse().map_err(|_| {
        format!("'{spec}' is not a duration — use 500ms, 2s or a millisecond count")
    })?;
    let millis = value
        .checked_mul(multiplier)
        .ok_or_else(|| format!("'{spec}' is longer than this verb waits"))?;
    Ok(std::time::Duration::from_millis(millis))
}

/// Validate `--count`.
///
/// # Errors
///
/// `--count 0` is refused **at parse**: sending nothing and reporting success is
/// the kind of quiet no-op this surface exists to avoid.
pub fn validate_count(count: u32) -> Result<u32, String> {
    if count == 0 {
        return Err(
            "--count must be at least 1: --count 0 would send nothing and report nothing."
                .to_owned(),
        );
    }
    Ok(count)
}

/// The sender identity for a ping from `peer_id`.
#[must_use]
pub fn ping_sender_path(peer_id: &PeerId) -> String {
    format!("{}/{PING_SENDER_SUFFIX}", peer_id.as_str())
}

/// The recipient identity a ping from `peer_id` addresses.
#[must_use]
pub fn ping_recipient_path(peer_id: &PeerId) -> String {
    format!("{}/{PING_RECIPIENT_SUFFIX}", peer_id.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_zero_is_refused_at_parse() {
        assert!(validate_count(0).is_err());
        assert_eq!(validate_count(1), Ok(1));
    }

    #[test]
    fn interval_accepts_the_three_shipped_spellings() {
        assert_eq!(
            parse_interval("500ms"),
            Ok(std::time::Duration::from_millis(500))
        );
        assert_eq!(parse_interval("2s"), Ok(std::time::Duration::from_secs(2)));
        assert_eq!(
            parse_interval("250"),
            Ok(std::time::Duration::from_millis(250))
        );
        assert!(parse_interval("soon").is_err());
        assert!(parse_interval("").is_err());
    }

    #[test]
    fn both_wire_identities_are_rooted_at_the_local_identity() {
        let peer = PeerId::from_public_key(
            &ed25519_dalek::SigningKey::from_bytes(&[5u8; 32])
                .verifying_key()
                .to_bytes(),
        )
        .expect("peer id");
        // A sender that is not rooted at the signer is refused at signing time,
        // and a recipient outside this namespace would ask the far side to
        // materialize a node this host did not choose.
        assert!(ping_sender_path(&peer).starts_with(&format!("{}/", peer.as_str())));
        assert!(ping_recipient_path(&peer).starts_with(&format!("{}/", peer.as_str())));
        assert_ne!(ping_sender_path(&peer), ping_recipient_path(&peer));
    }
}
