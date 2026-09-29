//! `rustain peer share <alias> <artifact> --topic <id>` — the Topic's first
//! production producer (Story 18.4a, FR150).
//!
//! # What this verb is for
//!
//! Before it, `Topic`, `ContextRef` and `PeerTransport::gossip_topic` existed
//! with **no** non-test caller: a mechanism with no trigger, which is the class
//! Epic 18 has already paid for three times (`DF-14-5-1`, and 17.3b/c/d's three
//! withdrawn "occurs in production" claims). This verb is that trigger, and it
//! is deliberately the smallest one that makes the claim true: resolve an alias,
//! resolve one room artifact, mint a signed handle, advertise it into a Topic.
//!
//! # Disclosure is an operator act, ⛔ never automatic
//!
//! A Topic gains a handle only when someone runs this verb. ⛔ Nothing sweeps
//! the memory store, nothing shares on capture, and no turn publishes context as
//! a side effect. That is not caution — FR96 makes sharing breadth a consent
//! decision, and an automatic publisher would take it away from the operator.
//!
//! # A handle, never a body
//!
//! COLLAB invariant 14: *"envelopes carry signed handles; bodies move only by
//! authorized, content-addressed fetch."* What crosses is the artifact id, its
//! content hash, its producer, and a ≤240-byte summary. ⛔ The artifact's
//! contents stay here, and this cut ships no body-fetch protocol at all.
//!
//! # Authority
//!
//! This verb mints no `CapabilityToken`, references none, and grants no tool
//! or action authority. ⚑ The daemon-side share act **does** grant the
//! addressed peer membership of this Topic — the capability grant AC7 names,
//! and nothing beyond it (Story 18.4a code-review D1). The receiving agent
//! reads the summary as **tainted** context — a signature establishes
//! attribution, never truth (COLLAB invariant 15).

use crate::domain::models::{ArtifactKind, ContextSummary, EvidenceArtifact};

/// Why a share never left this host. Every arm sends nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShareRefusal {
    /// No entry with that alias in `.rustain/p2p.json`.
    UnknownAlias,
    /// The entry exists but carries no pinned key, so it admits nobody.
    Unpinned,
    /// The alias is pinned but this host holds no address for it.
    NoReach,
    /// A relay-only address, on a host that composed no relay.
    RelayNotConfigured,
    /// This build was compiled without the peer transport.
    FeatureDisabled,
    /// No daemon is running for this workspace (code-review D3: the daemon is
    /// the producer — it holds the Topic store and the bound transport).
    DaemonUnavailable,
    /// No artifact with that id in this workspace's room.
    UnknownArtifact { artifact: String },
    /// The summary is empty or over the 240-byte ceiling.
    ///
    /// ⛔ Refused rather than truncated: a shortened summary is a different
    /// claim than the one the signature would cover.
    Summary { reason: String },
    /// Something local failed before anything was sent.
    LocalFault { reason: String },
}

/// The summary a handle carries when the operator supplied none.
///
/// ⛔ States what the artifact **is**, never what it means or whether it is
/// correct: the receiving model weighs a peer's claim, and a summary that
/// editorialised would be putting words in the producer's mouth.
#[must_use]
pub fn derived_summary(artifact: &EvidenceArtifact) -> String {
    format!(
        "{} artifact produced by {}",
        kind_word(artifact.kind),
        artifact.producer.as_str()
    )
}

/// The one-word name for an artifact kind, used in derived copy.
#[must_use]
pub fn kind_word(kind: ArtifactKind) -> &'static str {
    match kind {
        ArtifactKind::Evidence => "record",
        ArtifactKind::Patch => "patch",
        ArtifactKind::TestResult => "test-result",
        ArtifactKind::Decision => "decision",
        ArtifactKind::Review => "review",
        ArtifactKind::InputRequest => "input-request",
        // ⛔ Never "unknown artifact": the row would read as a failure when the
        // real state is a kind this build has not been taught.
        _ => "unnamed-kind",
    }
}

/// Validate an operator-supplied or derived summary.
///
/// # Errors
///
/// [`ShareRefusal::Summary`] with the constructor's own reason.
pub fn validate_summary(text: &str) -> Result<ContextSummary, ShareRefusal> {
    ContextSummary::new(text).map_err(|error| ShareRefusal::Summary {
        reason: error.to_string(),
    })
}

/// Operator copy for a refused share.
///
/// ⛔ Every arm says what was **not** done. Nothing here implies a frame left
/// this host.
#[must_use]
pub fn share_refusal_text(alias: &str, refusal: &ShareRefusal) -> String {
    match refusal {
        ShareRefusal::UnknownAlias => format!(
            "No peer named '{alias}' is configured, so nothing was shared. Run `rustain peer \
             list` to see the roster."
        ),
        ShareRefusal::Unpinned => format!(
            "'{alias}' has no pinned key, so it admits nobody and nothing was shared. Re-import \
             their ticket with `rustain peer add`."
        ),
        ShareRefusal::NoReach => format!(
            "This host holds no network address for '{alias}', so nothing was shared. Import a \
             fresh ticket with `rustain peer add`."
        ),
        ShareRefusal::RelayNotConfigured => format!(
            "The only address on file for '{alias}' needs a relay this host has not configured, \
             so nothing was shared."
        ),
        ShareRefusal::FeatureDisabled => format!(
            "This build was compiled without the peer transport, so nothing was shared with \
             '{alias}'."
        ),
        ShareRefusal::DaemonUnavailable => format!(
            "No daemon is running for this workspace, so nothing was shared with '{alias}'. \
             Start one with `rustain daemon run` — the daemon owns the peer transport and the \
             topic log this verb writes."
        ),
        ShareRefusal::UnknownArtifact { artifact } => format!(
            "No artifact `{artifact}` exists in this workspace's room, so nothing was shared with \
             '{alias}'. Run `rustain team log` or `/artifacts` to see what this host holds."
        ),
        ShareRefusal::Summary { reason } => {
            format!("The summary was not usable ({reason}), so nothing was shared with '{alias}'.")
        }
        ShareRefusal::LocalFault { reason } => {
            format!("Nothing was shared with '{alias}': {reason}")
        }
    }
}

/// Operator copy for a completed share.
///
/// ⛔ Says the handle was **advertised**, never that the peer took it, agreed
/// with it, or read it: topic gossip is fire-and-forget and nothing answered.
#[must_use]
pub fn share_sent_text(alias: &str, topic: &str, artifact: &str) -> String {
    format!(
        "Advertised the handle for `{artifact}` to '{alias}' in topic '{topic}'. The artifact \
         itself stayed on this host — only the signed reference and its summary were sent, and \
         nothing came back to confirm it was taken."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{
        AgentId, ArtifactId, CapabilityTokenId, ContentHash, HostBinding, MAX_CONTEXT_SUMMARY_BYTES,
    };

    fn artifact(kind: ArtifactKind) -> EvidenceArtifact {
        let hash = ContentHash::from_bytes([3u8; 32]);
        EvidenceArtifact {
            id: ArtifactId::from(hash),
            kind,
            producer: AgentId::parse("worker-1").expect("valid agent id"),
            content_hash: hash,
            authority: CapabilityTokenId::nil(),
            provenance: Vec::new(),
            depends_on: Vec::new(),
            review: None,
            host: HostBinding::new("host", "workspace"),
        }
    }

    #[test]
    fn the_derived_summary_states_the_artifact_and_never_judges_it() {
        let text = derived_summary(&artifact(ArtifactKind::Decision));
        assert_eq!(text, "decision artifact produced by worker-1");
        // Wording ceiling: no shipped string may claim verification.
        for banned in [
            "verified",
            "authenticated",
            "proof",
            "evidence trail",
            "tamper",
            "secure",
        ] {
            assert!(
                !text.to_ascii_lowercase().contains(banned),
                "derived copy must not claim {banned}"
            );
        }
    }

    #[test]
    fn an_over_long_summary_is_refused_rather_than_shortened() {
        let over = "a".repeat(MAX_CONTEXT_SUMMARY_BYTES + 1);
        let refusal = validate_summary(&over).expect_err("over the ceiling");
        assert!(matches!(refusal, ShareRefusal::Summary { .. }));
        let text = share_refusal_text("ana", &refusal);
        assert!(text.contains("nothing was shared"));
        assert!(validate_summary(&"a".repeat(MAX_CONTEXT_SUMMARY_BYTES)).is_ok());
    }

    #[test]
    fn the_sent_line_never_claims_the_peer_took_it() {
        let text = share_sent_text("ana", "architecture", "art-1");
        assert!(text.contains("Advertised"));
        for banned in ["accepted", "delivered", "received", "confirmed that"] {
            assert!(
                !text.to_ascii_lowercase().contains(banned),
                "a fire-and-forget advertisement must not claim {banned}"
            );
        }
    }
}
