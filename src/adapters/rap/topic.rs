//! Topic replication: signed handles, per-issuer heads, and divergence
//! detection (Story 18.4a — FR150, FR150-a, NFR71).
//!
//! # What replicates, and what does not
//!
//! COLLAB invariant 13, load-bearing: *"context is a projection, never a
//! replicated object. The log replicates; `assemble` is pure and recomputes.
//! **Any code path that ships an assembled context across a boundary is a
//! bug.**"* What crosses the wire here is a list of [`ContextRef`] handles and a
//! list of [`TopicHead`] observations. ⛔ No `ContextBundle`, no
//! `ProvenancedEntry`, no artifact body, and no assembled prefix.
//!
//! # This is git, not a CRDT
//!
//! Each issuer's contribution to a Topic is a chain: the head after admitting a
//! frame is `sha256(domain ‖ previous head ‖ that frame's own entry hash)`,
//! reusing [`crate::adapters::rap::entry_hash`] — the *same* per-sender feed
//! identity `ReplayWindow` already chains on, so the head and the rule that
//! orders it cannot drift. Nothing merges, nothing votes, and there is no total
//! order across issuers.
//!
//! # Why re-gossip carries heads and never handles
//!
//! When this host admits a frame it re-advertises **the heads it holds** to the
//! Topic's other members. That is what turns a single-receiver fork check into
//! cross-peer detection: peer C learns what peer B observed about peer A's feed
//! and can compare it against what A told C directly.
//!
//! ⛔ It does **not** re-advertise the handles. Re-publishing another peer's
//! signed content is FR155 (authorized handle re-publication), which is struck
//! from this story and deferred with `DF-18-CRYPTO-CLUSTER` (item C5). A head is
//! an observation about a feed; a handle is someone else's signed claim, and
//! forwarding one is a different act with a different authorization.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::domain::models::{
    AgentEnvelopeHeader, ContentHash, ContextRef, ContextRefError, CorrelationId, HeadVerdict,
    PeerId, RoomEvent, TopicHead,
};
use crate::domain::services::topic::{
    MAX_HANDLES_PER_FRAME, assemble_handles, compare_head, live_handles,
};

use super::wire::{VerifyError, entry_hash};

/// Domain-separation tag for the Topic head chain.
///
/// Rooted in the RAP domain and distinct from the envelope and Attach-proof
/// transcripts, so a head can never be confused with an envelope signature or
/// replayed as one.
pub const TOPIC_HEAD_DOMAIN: &[u8] = b"RAP/1\0topic-head\0";

/// Head observations one gossip frame may carry.
///
/// The re-gossip fan-out advertises what this host holds for a Topic, which is
/// bounded by the number of issuers in it. Sixty-four issuers is far past any
/// real team and finite for the other case.
pub const MAX_HEADS_PER_FRAME: usize = 64;

/// The body of a [`crate::domain::models::MessageKind::TopicGossip`] frame.
///
/// ⛔ Not a message: nothing here is delivered to an agent, materializes a node
/// or reaches the message bus. It is replication traffic, and the delivery front
/// door routes it away from the bus before any of that happens.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TopicGossip {
    /// Signed handles this frame contributes to the Topic. May be empty: a
    /// head-only frame is the re-gossip case.
    #[serde(default)]
    pub refs: Vec<ContextRef>,
    /// Heads this host holds for the Topic, one per issuer. May be empty.
    #[serde(default)]
    pub heads: Vec<TopicHead>,
}

/// What admitting one gossip frame produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TopicAdmission {
    /// The Topic the frame addressed.
    pub topic: CorrelationId,
    /// This host's head for the sending issuer's feed after the frame landed.
    /// `None` when the frame carried no handles.
    pub head: Option<TopicHead>,
    /// Every head observation that could not be reconciled with what this host
    /// already held. ⛔ Recorded, never enforced.
    pub divergences: Vec<TopicDivergence>,
    /// Heads this host holds for the Topic after the frame, for re-gossip.
    pub holdings: Vec<TopicHead>,
    /// The Topic's members after the frame, for the re-gossip fan-out.
    pub members: BTreeSet<PeerId>,
}

/// One irreconcilable pair of heads, as observed. ⛔ Never an accusation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TopicDivergence {
    /// The peer that advertised the head this host could not reconcile.
    pub advertiser: PeerId,
    /// Whose feed the two heads describe.
    pub issuer: PeerId,
    pub topic: CorrelationId,
    pub sequence: u64,
    pub held: Vec<u8>,
    pub advertised: Vec<u8>,
}

impl TopicDivergence {
    /// The durable record for this observation.
    ///
    /// ⛔ Detection and recording only (FR150-a): the event states that two
    /// claims disagree and nothing about who was wrong, and no caller may treat
    /// it as grounds for exclusion.
    #[must_use]
    pub fn to_room_event(&self) -> RoomEvent {
        RoomEvent::PeerEquivocated {
            peer: Some(self.advertiser.clone()),
            issuer: Some(self.issuer.clone()),
            topic: self.topic.0.clone(),
            sequence: self.sequence,
            held: hex::encode(&self.held),
            advertised: hex::encode(&self.advertised),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TopicError {
    #[error(transparent)]
    Handle(#[from] ContextRefError),
    #[error(
        "peer {peer} holds no membership grant for this topic; nothing was admitted and no \
         hashes were advertised to it"
    )]
    NotAMember { peer: String },
    #[error("a topic frame carried {count} head observations, over the {max} ceiling")]
    TooManyHeads { count: usize, max: usize },
    #[error("a topic frame carried neither handles nor head observations")]
    EmptyFrame,
    #[error("the topic head could not be chained: {0}")]
    Chain(String),
}

/// Chain one frame onto a Topic head.
///
/// `sha256(TOPIC_HEAD_DOMAIN ‖ previous ‖ entry_hash(header))`. Reusing the
/// shipped [`entry_hash`] is deliberate: it is already the per-sender feed entry
/// identity `ReplayWindow` chains `prev_hash` against, so a Topic head and the
/// replay rule that orders it are computed from the same bytes and cannot
/// disagree about what a frame was.
///
/// # Errors
///
/// Propagates a canonical-encoding failure from [`entry_hash`].
pub fn advance_head(previous: &[u8], header: &AgentEnvelopeHeader) -> Result<Vec<u8>, VerifyError> {
    let mut hasher = Sha256::new();
    hasher.update(TOPIC_HEAD_DOMAIN);
    hasher.update(previous);
    hasher.update(entry_hash(header)?);
    Ok(hasher.finalize().to_vec())
}

#[derive(Clone, Debug, Default)]
struct TopicFeed {
    sequence: u64,
    head: Vec<u8>,
    handles: Vec<ContextRef>,
}

/// A head this host holds for one feed position, and **who it learned it
/// from**.
///
/// The source matters when a later direct frame disagrees with an earlier
/// advertisement (code-review P4): the divergence record must name the peer
/// that made the contradicted claim, and only the source carries it. For a
/// directly observed frame the source is the issuer itself.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ObservedHead {
    head: Vec<u8>,
    source: PeerId,
}

/// Feed positions retained per `(topic, issuer)` in the observation map.
///
/// A safety valve, not a hot path (code-review P11): without it every
/// well-formed advertised position is a permanent map entry, and a member
/// could grow a long-lived daemon's memory without limit. Compares against
/// positions older than the retained window read as first sightings again —
/// a repeated observation is recorded, which is the honest direction for a
/// detection-only mechanism.
const MAX_OBSERVED_PER_FEED: usize = 8;

/// A validated admission, computed against pre-state and not yet applied.
///
/// ⚑ Code-review P6: the production front door calls [`PeerTopicStore::prepare`],
/// journals every divergence, and only then calls [`PeerTopicStore::commit`].
/// A journal failure leaves the store untouched, so the retried frame the
/// rolled-back replay reservation allows cannot advance a feed twice.
#[derive(Debug)]
pub struct PreparedAdmission {
    topic: CorrelationId,
    sender: PeerId,
    founding: bool,
    refs: Vec<ContextRef>,
    chained: Option<(u64, Vec<u8>)>,
    /// Computed against pre-state; the front door journals these before commit.
    pub divergences: Vec<TopicDivergence>,
    first_sightings: Vec<((CorrelationId, PeerId, u64), ObservedHead)>,
    now_unix: i64,
}

#[derive(Debug, Default)]
struct TopicState {
    /// `(topic, issuer)` → that issuer's chain within the Topic.
    feeds: BTreeMap<(CorrelationId, PeerId), TopicFeed>,
    /// `(topic, issuer, sequence)` → the head this host holds for that feed
    /// position, whether observed directly or advertised by another peer, and
    /// **who** the observation came from (see [`ObservedHead`]).
    observed: BTreeMap<(CorrelationId, PeerId, u64), ObservedHead>,
    /// Who may be advertised a Topic's hashes (AC7).
    members: BTreeMap<CorrelationId, BTreeSet<PeerId>>,
}

/// The replicated Topic log this host holds, and the divergence detector over it.
///
/// One instance is shared by the verified-peer delivery front door (which fills
/// it) and `PeerContextProvider` (which reads it). ⛔ Two instances is a host
/// whose agent reads a different log than the one its transport writes.
#[derive(Debug, Default)]
pub struct PeerTopicStore {
    state: Mutex<TopicState>,
}

impl PeerTopicStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Grant `peer` membership of `topic`.
    ///
    /// ⚑ Membership is a **capability grant** (COLLAB D2), ⛔ never a new
    /// allowlist file. The only two ways in are this call — made by the operator
    /// act that shares into a Topic — and being the identity that established
    /// the Topic on this host. `architecture-review-18-4c-b-2026-08-17.md:90`
    /// names why a third admission model is refused: `ADR-18-4-01` D4 exists
    /// precisely because that hazard already occurred once.
    pub async fn grant_membership(&self, topic: &CorrelationId, peer: &PeerId) {
        self.state
            .lock()
            .await
            .members
            .entry(topic.clone())
            .or_default()
            .insert(peer.clone());
    }

    /// Whether `peer` may be advertised this Topic's hashes.
    pub async fn is_member(&self, topic: &CorrelationId, peer: &PeerId) -> bool {
        self.state
            .lock()
            .await
            .members
            .get(topic)
            .is_some_and(|members| members.contains(peer))
    }

    /// The members of `topic`, for an advertisement fan-out.
    pub async fn members(&self, topic: &CorrelationId) -> BTreeSet<PeerId> {
        self.state
            .lock()
            .await
            .members
            .get(topic)
            .cloned()
            .unwrap_or_default()
    }

    /// Heads this host holds for `topic`, one per issuer.
    pub async fn holdings(&self, topic: &CorrelationId) -> Vec<TopicHead> {
        let state = self.state.lock().await;
        Self::holdings_locked(&state, topic)
    }

    fn holdings_locked(state: &TopicState, topic: &CorrelationId) -> Vec<TopicHead> {
        state
            .feeds
            .iter()
            .filter(|((feed_topic, _), feed)| feed_topic == topic && feed.sequence > 0)
            .map(|((feed_topic, issuer), feed)| TopicHead {
                topic: feed_topic.clone(),
                issuer: issuer.clone(),
                sequence: feed.sequence,
                head: feed.head.clone(),
            })
            .collect()
    }

    /// Every live handle this host holds, across every Topic.
    ///
    /// Ordered and deduplicated by the pure core, so the answer is a function of
    /// the *set* held and never of the order it arrived in (NFR71).
    pub async fn live_handles(&self, now_unix: i64) -> Vec<ContextRef> {
        let state = self.state.lock().await;
        let all: Vec<ContextRef> = state
            .feeds
            .values()
            .flat_map(|feed| feed.handles.iter().cloned())
            .collect();
        assemble_handles(live_handles(all, now_unix))
    }

    /// Admit one verified topic-gossip frame (FR150, FR150-a).
    ///
    /// ⚠ The caller must already have verified the envelope through the single
    /// `rap::wire` verify seam. This function re-checks only what a signature
    /// cannot: that each handle's claimed `issuer` is the peer that signed the
    /// frame, that the handle is still live, that the artifact id re-derives to
    /// the content hash, and that the sender holds a membership grant for the
    /// Topic.
    ///
    /// Composed from [`Self::prepare`] + [`Self::commit`]; the production front
    /// door uses the split so a divergence is journaled **before** any state
    /// moves (code-review P6).
    ///
    /// # Errors
    ///
    /// [`TopicError::NotAMember`] when a peer that is not a member of an
    /// established Topic gossips into it — nothing is admitted and nothing is
    /// advertised back. [`TopicError::Handle`] when a handle claims an issuer
    /// other than the signer (the relabelling COLLAB D7 forbids), has expired,
    /// or names an artifact id that is not its content hash.
    pub async fn admit(
        &self,
        header: &AgentEnvelopeHeader,
        sender: &PeerId,
        gossip: TopicGossip,
        now_unix: i64,
    ) -> Result<TopicAdmission, TopicError> {
        let prepared = self.prepare(header, sender, gossip, now_unix).await?;
        Ok(self.commit(prepared).await)
    }

    /// Validate one gossip frame and compute its effects **without mutating**.
    ///
    /// ⚑ Code-review P6: the front door journals the returned divergences and
    /// only then calls [`Self::commit`]. A journal failure therefore leaves the
    /// store untouched, and a retried frame cannot advance a feed twice.
    ///
    /// # Errors
    ///
    /// Same arms as [`Self::admit`].
    pub async fn prepare(
        &self,
        header: &AgentEnvelopeHeader,
        sender: &PeerId,
        gossip: TopicGossip,
        now_unix: i64,
    ) -> Result<PreparedAdmission, TopicError> {
        let topic = header.correlation_id.clone();
        if gossip.refs.is_empty() && gossip.heads.is_empty() {
            return Err(TopicError::EmptyFrame);
        }
        if gossip.refs.len() > MAX_HANDLES_PER_FRAME {
            return Err(TopicError::Handle(ContextRefError::TooManyHandles {
                count: gossip.refs.len(),
                max: MAX_HANDLES_PER_FRAME,
            }));
        }
        if gossip.heads.len() > MAX_HEADS_PER_FRAME {
            return Err(TopicError::TooManyHeads {
                count: gossip.heads.len(),
                max: MAX_HEADS_PER_FRAME,
            });
        }
        // ⛔ Attribution is checked before anything is stored: a peer may only
        // issue handles under its **own** identity. Signing someone else's
        // `content_hash` under someone else's issuer is the relabelling attack
        // COLLAB D7 closed, and the signature alone does not close it — the
        // signature proves who signed, not whom they claimed to be.
        for handle in &gossip.refs {
            if &handle.issuer != sender {
                return Err(TopicError::Handle(ContextRefError::IssuerNotSigner {
                    claimed: handle.issuer.to_string(),
                    signer: sender.to_string(),
                }));
            }
            if !handle.is_live(now_unix) {
                return Err(TopicError::Handle(ContextRefError::Expired {
                    now_unix,
                    not_after: handle.not_after,
                }));
            }
            // ⚑ Code-review P10: an `ArtifactId` IS the canonical content hash
            // (`artifact.rs`), so a handle whose id does not re-derive to its
            // `content_hash` is incoherent — dedup keys on the hash while
            // artifact lookups key on the id, and the mismatch splits them.
            // ⛔ `parse_hex`, never `ArtifactId::content_hash()`: a malformed
            // wire id must be refused, not panicked on.
            match ContentHash::parse_hex(handle.artifact.as_str()) {
                Ok(derived) if derived == handle.content_hash => {}
                _ => {
                    return Err(TopicError::Handle(ContextRefError::ArtifactHashMismatch {
                        artifact: handle.artifact.as_str().to_owned(),
                    }));
                }
            }
        }

        let state = self.state.lock().await;

        // AC7 — membership gates participation. An established Topic admits only
        // its members; a Topic this host has never seen is established by the
        // frame that opens it, and its sender is its first member. That is the
        // capability grant, not a second allowlist: the sender already passed
        // transport admission or it would never have reached this seam.
        let members = state.members.get(&topic);
        let founding = members.is_none_or(|members| members.is_empty());
        if !founding && !members.is_some_and(|members| members.contains(sender)) {
            return Err(TopicError::NotAMember {
                peer: sender.to_string(),
            });
        }

        // The head this frame would chain, computed against pre-state.
        let chained = if gossip.refs.is_empty() {
            None
        } else {
            let feed = state.feeds.get(&(topic.clone(), sender.clone()));
            let (previous_head, previous_sequence) = feed
                .map(|feed| (feed.head.as_slice(), feed.sequence))
                .unwrap_or((&[], 0));
            let chained = advance_head(previous_head, header)
                .map_err(|error| TopicError::Chain(error.to_string()))?;
            Some((previous_sequence.saturating_add(1), chained))
        };

        // The cross-peer compare. Every advertisement — including one this host
        // could have derived itself — is checked against what it already holds.
        let mut divergences = Vec::new();
        let mut first_sightings: Vec<((CorrelationId, PeerId, u64), ObservedHead)> = Vec::new();
        for advert in &gossip.heads {
            if advert.topic != topic {
                // A frame speaks for the Topic its own signed header names.
                // ⛔ Silently folding a foreign topic in would let one frame
                // write a head into a Topic it was never addressed to.
                continue;
            }
            let key = (topic.clone(), advert.issuer.clone(), advert.sequence);
            let held = state.observed.get(&key);
            match compare_head(held.map(|seen| seen.head.as_slice()), advert) {
                HeadVerdict::First => {
                    first_sightings.push((
                        key,
                        ObservedHead {
                            head: advert.head.clone(),
                            source: sender.clone(),
                        },
                    ));
                }
                HeadVerdict::Agrees | HeadVerdict::Malformed => {}
                HeadVerdict::Equivocated { held, advertised } => {
                    divergences.push(TopicDivergence {
                        advertiser: sender.clone(),
                        issuer: advert.issuer.clone(),
                        topic: topic.clone(),
                        sequence: advert.sequence,
                        held,
                        advertised,
                    });
                }
            }
        }

        // ⚑ Code-review P4: the compare runs BOTH ways. A head this host
        // computes from a direct frame must also be checked against an earlier
        // advertisement for the same position — previously the insert below
        // overwrote the advertisement unaudited, so a bogus advertised head was
        // never recorded once the real frame landed.
        if let Some((sequence, chained)) = &chained {
            let key = (topic.clone(), sender.clone(), *sequence);
            if let Some(seen) = state.observed.get(&key) {
                let computed = TopicHead {
                    topic: topic.clone(),
                    issuer: sender.clone(),
                    sequence: *sequence,
                    head: chained.clone(),
                };
                if let HeadVerdict::Equivocated { held, advertised } =
                    compare_head(Some(seen.head.as_slice()), &computed)
                {
                    divergences.push(TopicDivergence {
                        advertiser: seen.source.clone(),
                        issuer: sender.clone(),
                        topic: topic.clone(),
                        sequence: *sequence,
                        held,
                        advertised,
                    });
                }
            }
        }

        Ok(PreparedAdmission {
            topic,
            sender: sender.clone(),
            founding,
            refs: gossip.refs,
            chained,
            divergences,
            first_sightings,
            now_unix,
        })
    }

    /// Apply a prepared admission. Infallible: every fallible check ran in
    /// [`Self::prepare`], so a caller that journaled the divergences can never
    /// be left with a recorded observation and no admission.
    pub async fn commit(&self, prepared: PreparedAdmission) -> TopicAdmission {
        let mut state = self.state.lock().await;
        let PreparedAdmission {
            topic,
            sender,
            founding,
            refs,
            chained,
            divergences,
            first_sightings,
            now_unix,
        } = prepared;

        if founding {
            state
                .members
                .entry(topic.clone())
                .or_default()
                .insert(sender.clone());
        }

        let head = if let Some((sequence, chained)) = chained {
            let feed = state
                .feeds
                .entry((topic.clone(), sender.clone()))
                .or_default();
            feed.sequence = sequence;
            feed.head = chained.clone();
            feed.handles.extend(refs);
            // Order and dedup on the way in so the stored feed is already a set
            // and a duplicate frame cannot grow it.
            feed.handles = assemble_handles(std::mem::take(&mut feed.handles));
            state.observed.insert(
                (topic.clone(), sender.clone(), sequence),
                ObservedHead {
                    head: chained.clone(),
                    source: sender.clone(),
                },
            );
            Some(TopicHead {
                topic: topic.clone(),
                issuer: sender.clone(),
                sequence,
                head: chained,
            })
        } else {
            None
        };

        for (key, sighting) in first_sightings {
            state.observed.insert(key, sighting);
        }

        // ⚑ Code-review P11: hygiene, every admission, while the lock is held.
        // Expired handles are dropped from the stored feeds rather than cloned
        // and filtered on every read forever, and the observation map keeps
        // only the latest few positions per feed — a member must not be able
        // to grow this host's memory without limit by advertising positions.
        for feed in state.feeds.values_mut() {
            feed.handles.retain(|handle| handle.is_live(now_unix));
        }
        let mut feed_positions: BTreeMap<(CorrelationId, PeerId), Vec<u64>> = BTreeMap::new();
        for (feed_topic, issuer, sequence) in state.observed.keys() {
            feed_positions
                .entry((feed_topic.clone(), issuer.clone()))
                .or_default()
                .push(*sequence);
        }
        let mut evict = Vec::new();
        for ((feed_topic, issuer), mut sequences) in feed_positions {
            sequences.sort_unstable();
            let keep_from = sequences.len().saturating_sub(MAX_OBSERVED_PER_FEED);
            for sequence in sequences.drain(..keep_from) {
                evict.push((feed_topic.clone(), issuer.clone(), sequence));
            }
        }
        for key in evict {
            state.observed.remove(&key);
        }

        let holdings = Self::holdings_locked(&state, &topic);
        let members = state.members.get(&topic).cloned().unwrap_or_default();
        TopicAdmission {
            topic,
            head,
            divergences,
            holdings,
            members,
        }
    }

    /// Record this host's own contribution to a Topic (the `peer share` path).
    ///
    /// Mirrors [`Self::admit`] for the local issuer so a host's own head is the
    /// same function of the same frames as a remote observer's.
    pub async fn record_local(
        &self,
        header: &AgentEnvelopeHeader,
        issuer: &PeerId,
        handles: Vec<ContextRef>,
    ) -> Result<TopicHead, TopicError> {
        let topic = header.correlation_id.clone();
        let mut state = self.state.lock().await;
        state
            .members
            .entry(topic.clone())
            .or_default()
            .insert(issuer.clone());
        let feed = state
            .feeds
            .entry((topic.clone(), issuer.clone()))
            .or_default();
        let chained = advance_head(&feed.head, header)
            .map_err(|error| TopicError::Chain(error.to_string()))?;
        feed.sequence = feed.sequence.saturating_add(1);
        feed.head = chained.clone();
        feed.handles.extend(handles);
        feed.handles = assemble_handles(std::mem::take(&mut feed.handles));
        let sequence = feed.sequence;
        state.observed.insert(
            (topic.clone(), issuer.clone(), sequence),
            ObservedHead {
                head: chained.clone(),
                source: issuer.clone(),
            },
        );
        Ok(TopicHead {
            topic,
            issuer: issuer.clone(),
            sequence,
            head: chained,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{
        AgentId, ArtifactId, ContentHash, ContextRefProvenance, ContextSummary, MessageKind,
    };

    fn peer(seed: u8) -> PeerId {
        PeerId::from_public_key(&[seed; 32]).expect("valid key length")
    }

    fn header(topic: &str, sequence: u64, prev: Vec<u8>) -> AgentEnvelopeHeader {
        AgentEnvelopeHeader {
            sender: AgentId::parse("sender").expect("valid agent id"),
            recipient: AgentId::parse("recipient").expect("valid agent id"),
            correlation_id: CorrelationId::new(topic),
            kind: MessageKind::TopicGossip,
            sequence,
            not_after: 10_000,
            nonce: format!("nonce-{sequence}"),
            content_hash: vec![sequence as u8; 32],
            prev_hash: prev,
        }
    }

    fn handle(content: u8, issuer: &PeerId) -> ContextRef {
        let hash = ContentHash::from_bytes([content; 32]);
        ContextRef {
            artifact: ArtifactId::from(hash),
            content_hash: hash,
            producer: AgentId::parse("producer").expect("valid agent id"),
            issuer: issuer.clone(),
            summary: ContextSummary::new(format!("handle {content}")).expect("bounded"),
            provenance: ContextRefProvenance::Authored,
            not_after: 10_000,
        }
    }

    #[tokio::test]
    async fn a_handle_claiming_another_issuer_is_refused() {
        let store = PeerTopicStore::new();
        let signer = peer(1);
        let error = store
            .admit(
                &header("t", 1, Vec::new()),
                &signer,
                TopicGossip {
                    refs: vec![handle(1, &peer(2))],
                    heads: Vec::new(),
                },
                0,
            )
            .await
            .expect_err("a relabelled issuer is refused");
        assert!(matches!(
            error,
            TopicError::Handle(ContextRefError::IssuerNotSigner { .. })
        ));
        assert!(store.live_handles(0).await.is_empty());
    }

    #[tokio::test]
    async fn a_non_member_cannot_gossip_into_an_established_topic() {
        let store = PeerTopicStore::new();
        let founder = peer(1);
        store
            .admit(
                &header("t", 1, Vec::new()),
                &founder,
                TopicGossip {
                    refs: vec![handle(1, &founder)],
                    heads: Vec::new(),
                },
                0,
            )
            .await
            .expect("the founding frame establishes the topic");

        let stranger = peer(9);
        let error = store
            .admit(
                &header("t", 1, Vec::new()),
                &stranger,
                TopicGossip {
                    refs: vec![handle(2, &stranger)],
                    heads: Vec::new(),
                },
                0,
            )
            .await
            .expect_err("a non-member is refused");
        assert!(matches!(error, TopicError::NotAMember { .. }));
        // ⚑ The gate is not advisory: the stranger's handle is absent, and the
        // stranger is not in the member set an advertisement fans out to.
        assert_eq!(store.live_handles(0).await.len(), 1);
        assert!(!store.is_member(&CorrelationId::new("t"), &stranger).await);
    }

    #[tokio::test]
    async fn a_second_head_at_one_sequence_is_recorded_as_a_divergence() {
        let store = PeerTopicStore::new();
        let advertiser = peer(1);
        let subject = peer(2);
        let first = store
            .admit(
                &header("t", 1, Vec::new()),
                &advertiser,
                TopicGossip {
                    refs: Vec::new(),
                    heads: vec![TopicHead {
                        topic: CorrelationId::new("t"),
                        issuer: subject.clone(),
                        sequence: 4,
                        head: vec![7u8; 32],
                    }],
                },
                0,
            )
            .await
            .expect("the first observation is recorded");
        assert!(first.divergences.is_empty(), "first sighting is not a fork");

        let second = store
            .admit(
                &header("t", 2, Vec::new()),
                &advertiser,
                TopicGossip {
                    refs: Vec::new(),
                    heads: vec![TopicHead {
                        topic: CorrelationId::new("t"),
                        issuer: subject.clone(),
                        sequence: 4,
                        head: vec![8u8; 32],
                    }],
                },
                0,
            )
            .await
            .expect("the second observation is compared");
        assert_eq!(second.divergences.len(), 1);
        let divergence = &second.divergences[0];
        assert_eq!(divergence.issuer, subject);
        assert_eq!(divergence.advertiser, advertiser);
        assert_eq!(divergence.sequence, 4);
        assert!(matches!(
            divergence.to_room_event(),
            RoomEvent::PeerEquivocated { sequence: 4, .. }
        ));
    }

    #[tokio::test]
    async fn an_expired_handle_never_enters_the_log() {
        let store = PeerTopicStore::new();
        let signer = peer(1);
        let error = store
            .admit(
                &header("t", 1, Vec::new()),
                &signer,
                TopicGossip {
                    refs: vec![handle(1, &signer)],
                    heads: Vec::new(),
                },
                10_001,
            )
            .await
            .expect_err("an expired handle is refused");
        assert!(matches!(
            error,
            TopicError::Handle(ContextRefError::Expired { .. })
        ));
    }

    #[tokio::test]
    async fn a_handle_whose_artifact_id_is_not_its_content_hash_is_refused() {
        // Code-review P10: `ArtifactId` is canonically the content hash; a
        // handle that splits them would dedup by one identity and address by
        // another.
        let store = PeerTopicStore::new();
        let signer = peer(1);
        let mut incoherent = handle(1, &signer);
        incoherent.artifact = ArtifactId::from(ContentHash::from_bytes([9u8; 32]));
        let error = store
            .admit(
                &header("t", 1, Vec::new()),
                &signer,
                TopicGossip {
                    refs: vec![incoherent],
                    heads: Vec::new(),
                },
                0,
            )
            .await
            .expect_err("an incoherent handle is refused");
        assert!(matches!(
            error,
            TopicError::Handle(ContextRefError::ArtifactHashMismatch { .. })
        ));
        assert!(store.live_handles(0).await.is_empty());
    }

    #[tokio::test]
    async fn a_direct_frame_that_contradicts_an_earlier_advertisement_is_recorded() {
        // Code-review P4: the advertisement-first order. B's advertised head
        // for A's feed position 2 lands before A's real frame; when A's frame
        // arrives and computes a different head at that position, the
        // disagreement is a divergence — never a silent overwrite.
        let store = PeerTopicStore::new();
        let issuer = peer(1);
        let advertiser = peer(2);
        store
            .admit(
                &header("t", 1, Vec::new()),
                &issuer,
                TopicGossip {
                    refs: vec![handle(1, &issuer)],
                    heads: Vec::new(),
                },
                0,
            )
            .await
            .expect("the issuer's first frame founds the topic");
        store
            .grant_membership(&CorrelationId::new("t"), &advertiser)
            .await;
        store
            .admit(
                &header("t", 1, Vec::new()),
                &advertiser,
                TopicGossip {
                    refs: Vec::new(),
                    heads: vec![TopicHead {
                        topic: CorrelationId::new("t"),
                        issuer: issuer.clone(),
                        sequence: 2,
                        head: vec![0xAAu8; 32],
                    }],
                },
                0,
            )
            .await
            .expect("the advertisement is a first sighting");

        let second = store
            .admit(
                &header("t", 2, vec![1u8; 32]),
                &issuer,
                TopicGossip {
                    refs: vec![handle(2, &issuer)],
                    heads: Vec::new(),
                },
                0,
            )
            .await
            .expect("the direct frame is admitted");
        assert_eq!(
            second.divergences.len(),
            1,
            "the computed head contradicts the advertised one"
        );
        assert_eq!(second.divergences[0].advertiser, advertiser);
        assert_eq!(second.divergences[0].issuer, issuer);
        assert_eq!(second.divergences[0].held, vec![0xAAu8; 32]);
    }

    #[tokio::test]
    async fn admission_prunes_expired_handles_and_bounds_observations() {
        // Code-review P11: stored state is hygienic. Expired handles are
        // pruned at commit, and the observation map keeps only the latest few
        // positions per feed.
        let store = PeerTopicStore::new();
        let signer = peer(1);
        let mut expiring = handle(1, &signer);
        expiring.not_after = 5_000;
        store
            .admit(
                &header("t", 1, Vec::new()),
                &signer,
                TopicGossip {
                    refs: vec![expiring],
                    heads: Vec::new(),
                },
                0,
            )
            .await
            .expect("live at admission");
        // Ten advertised positions from a member, at a clock where the first
        // handle has expired.
        let advertiser = peer(2);
        store
            .grant_membership(&CorrelationId::new("t"), &advertiser)
            .await;
        store
            .admit(
                &header("t", 1, Vec::new()),
                &advertiser,
                TopicGossip {
                    refs: Vec::new(),
                    heads: (1u64..=10)
                        .map(|sequence| TopicHead {
                            topic: CorrelationId::new("t"),
                            issuer: signer.clone(),
                            sequence,
                            head: vec![sequence as u8; 32],
                        })
                        .collect(),
                },
                10_000,
            )
            .await
            .expect("advertisements admitted");

        let state = store.state.lock().await;
        let feed = state
            .feeds
            .get(&(CorrelationId::new("t"), signer.clone()))
            .expect("the feed exists");
        assert!(
            feed.handles.is_empty(),
            "the expired handle is pruned at commit, not filtered per read forever"
        );
        let retained = state
            .observed
            .keys()
            .filter(|(topic, issuer, _)| topic == &CorrelationId::new("t") && issuer == &signer)
            .count();
        assert!(
            retained <= MAX_OBSERVED_PER_FEED,
            "the observation map is bounded per feed, got {retained}"
        );
    }

    #[test]
    fn the_head_chain_is_domain_separated_and_order_dependent() {
        let first = header("t", 1, Vec::new());
        let second = header("t", 2, vec![1u8; 32]);
        let a = advance_head(&[], &first).expect("chains");
        let b = advance_head(&a, &second).expect("chains");
        let reversed =
            advance_head(&advance_head(&[], &second).expect("chains"), &first).expect("chains");
        assert_ne!(b, reversed, "a feed chain is order-dependent by design");
        assert_ne!(
            a,
            entry_hash(&first).expect("hashes"),
            "the domain tag must actually separate"
        );
    }
}
