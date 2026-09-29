//! Verified RAP peer-frame delivery.
//!
//! # Transparency scope
//!
//! This module records only peer-origin deliveries: `OwnershipKind::Peer` /
//! `NodeOrigin::Remote`. Local `Owned`-to-`Owned` subagent chatter is explicitly
//! out of scope — FR92 concerns another team member's agent, and journaling all
//! internal chatter would reproduce `DF-18-2-JOURNAL-GROWTH` at a much higher rate.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use tokio::sync::{Mutex, broadcast, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

use crate::domain::events::AppEvent;
use crate::domain::models::{
    AgentEnvelope, AgentId, AgentMessage, AgentMetrics, CapabilityTokenId, CorrelationId, Envelope,
    MessageHeader, MessageKind, NodeState, PeerId, SemanticMessageType, SubagentEnvelope,
    SubagentEvent,
};
use crate::domain::ports::{
    AgentMessageBus, PeerDeliveryOutcome, PeerDeliveryRecord, PeerInteractionRecorder,
};
use crate::domain::services::transparency::MAX_PEER_ID_BYTES;
use crate::infrastructure::subagent::{AgentHandle, MailboxBudget, NodeTree};

pub const MAX_PEER_MESSAGE_BYTES: usize = 64 * 1024;
const PEER_INGEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Late settlement fan-out depth. One slot per frame that can be in flight
/// across every peer context; a subscriber that falls this far behind has
/// stopped caring about the reservation it parked.
const SETTLEMENT_CAPACITY: usize = 256;

/// Distinct sender names one admitted peer may bind (Story 18.4d, ruling P12).
///
/// The binding table is keyed by the **sender name**, and any
/// `<peer_id>/<anything>` is a name the signing rule accepts — so an admitted
/// peer could mint unbounded distinct senders and grow this map without bound.
/// A real deployment uses a handful of agent paths per peer; thirty-two is
/// generous for that and finite for the other case.
const MAX_SENDERS_PER_PEER: usize = 32;

/// The sender path suffix a topic frame uses, under this host's own `PeerId`.
pub const TOPIC_SENDER_SUFFIX: &str = "topic-gossip";

/// The recipient path suffix a topic frame addresses, under this host's own
/// `PeerId`. ⚑ No node is ever materialized for it — a topic frame branches
/// away before `ensure_peer_context` — but the name stays inside this host's
/// namespace so the invariant does not depend on that branch.
pub const TOPIC_RECIPIENT_SUFFIX: &str = "topic-gossip-peer";

/// Longest a topic frame stays valid, in wall milliseconds.
///
/// A head advertisement is a statement about *now*; one that stays replayable
/// for hours is one an observer can hold and re-present. Sixty seconds matches
/// the ping frame's ceiling, and for the same reason.
pub const TOPIC_FRAME_TTL_MS: i64 = 60_000;

/// How long a shared handle stays live, in wall milliseconds.
///
/// ⚑ Longer than a frame's TTL on purpose: a frame is in flight for seconds, a
/// handle is context a teammate's agent reads across a working day. Bounded
/// anyway, because an immortal handle is a claim nobody can withdraw — and this
/// cut ships no cross-host retract (`DF-18-4-CROSSHOST-RETRACT`, target R4).
pub const HANDLE_TTL_MS: i64 = 24 * 60 * 60 * 1_000;

/// Recipient consent decision, separate from operational consumer failures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerifiedPeerConsent {
    Accept,
    Decline,
}

/// Terminal outcome of one peer frame, published after the ingest worker
/// settles it.
///
/// A caller whose own wait timed out no longer holds the acknowledgement
/// receiver, but the worker still reaches exactly one terminal result. Cross-host
/// transports subscribe to this so a replay reservation parked on an uncertain
/// wait is resolved by what actually happened rather than by a guess.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameSettlement {
    pub correlation_id: String,
    pub accepted: bool,
}

/// Application-side consumer reached only after cryptographic verification and
/// message-bus admission. Consent is decided before the durable acceptance
/// append; `ingest` is called only after that append succeeds.
#[async_trait]
pub trait VerifiedPeerConsumer: Send + Sync {
    async fn consent(
        &self,
        recipient: &AgentId,
        content: &AgentMessage,
        peer_id: &PeerId,
    ) -> Result<VerifiedPeerConsent, String>;

    async fn ingest(
        &self,
        recipient: &AgentId,
        content: AgentMessage,
        peer_id: &PeerId,
    ) -> Result<(), String>;

    async fn ingest_with_policy(
        &self,
        recipient: &AgentId,
        content: AgentMessage,
        peer_id: &PeerId,
        response_policy: crate::domain::ports::PeerResponsePolicy,
    ) -> Result<(), String> {
        let _ = response_policy;
        self.ingest(recipient, content, peer_id).await
    }
}

/// Shared post-verification RAP delivery seam.
///
/// The bus slot and node tree are injected by the composition root. This type
/// never creates a parallel bus: it materializes a Peer-owned local context,
/// delivers through the configured slot, waits for truthful ingest, and then
/// emits the receipt.
pub struct VerifiedPeerFrameHandler {
    node_tree: NodeTree,
    agent_message_bus: Arc<ArcSwap<Arc<dyn AgentMessageBus>>>,
    domain_tx: mpsc::UnboundedSender<AppEvent>,
    consumer: Arc<dyn VerifiedPeerConsumer>,
    materialized: Arc<Mutex<HashSet<AgentId>>>,
    verified_senders: Arc<Mutex<VerifiedSenders>>,
    pending_ingest: Arc<Mutex<HashMap<String, oneshot::Sender<Result<(), PeerDeliveryError>>>>>,
    recorder: Arc<dyn PeerInteractionRecorder>,
    settlements: broadcast::Sender<FrameSettlement>,
    /// Story 18.4a — the replicated Topic log, shared with the context provider
    /// that reads it. ⛔ Two stores is a host whose agent reads a different log
    /// than the one its transport writes, so the composition root passes one
    /// `Arc` to both.
    topics: Arc<crate::adapters::rap::topic::PeerTopicStore>,
    /// Where a divergent head is journaled and where re-gossip is sent from.
    ///
    /// ⚑ Filled **after** construction, and that is forced by the composition
    /// order rather than chosen: the handler is built by
    /// `AttachServer::configure_peer_recorder`, and the transport it must
    /// re-gossip on is bound later, by the listener composition that receives
    /// this handler. A `OnceLock` makes the late binding explicit and
    /// single-shot — ⛔ never a slot a second composition can silently replace.
    ///
    /// Empty in the composition paths that own no transport; a topic frame is
    /// then admitted and compared but nothing is written or re-advertised,
    /// which is refused honestly rather than passed off as success.
    topic_effects: Arc<std::sync::OnceLock<TopicEffects>>,
    /// This sender's outbound gossip position per peer, for the life of one
    /// process. ⛔ Never durable: 18.4d's D9 rejected a durable sender cursor
    /// with evidence — the receiver's `ReplayWindow` is in memory, so a sender
    /// that remembers a head the receiver forgot forks the feed permanently.
    /// The position starts optimistically and self-corrects once from what the
    /// receiver names.
    gossip_positions: Arc<Mutex<HashMap<PeerId, crate::domain::models::FeedPosition>>>,
    /// Injected clock, mirroring `IrohPeerIngress::with_now`, so the topic path
    /// can be driven deterministically. Wall **milliseconds** — the unit the
    /// peer-frame verify seam on this path uses.
    now: Arc<dyn Fn() -> i64 + Send + Sync>,
}

/// The two effects a topic frame can have beyond the in-memory store.
#[derive(Clone)]
pub struct TopicEffects {
    /// Journal root for `RoomEvent::PeerEquivocated`.
    pub workspace: std::path::PathBuf,
    /// The transport the re-gossip fan-out goes out on.
    pub transport: Arc<dyn crate::domain::ports::PeerTransport>,
    /// This host's signer, for the re-gossip frame.
    pub signer: crate::adapters::rap::AgentSigner,
}

impl std::fmt::Debug for TopicEffects {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TopicEffects")
            .field("workspace", &self.workspace)
            .finish_non_exhaustive()
    }
}

impl VerifiedPeerFrameHandler {
    pub fn new(
        node_tree: NodeTree,
        agent_message_bus: Arc<ArcSwap<Arc<dyn AgentMessageBus>>>,
        domain_tx: mpsc::UnboundedSender<AppEvent>,
        consumer: Arc<dyn VerifiedPeerConsumer>,
        recorder: Arc<dyn PeerInteractionRecorder>,
    ) -> Self {
        Self {
            node_tree,
            agent_message_bus,
            domain_tx,
            consumer,
            recorder,
            materialized: Arc::new(Mutex::new(HashSet::new())),
            verified_senders: Arc::new(Mutex::new(VerifiedSenders::default())),
            pending_ingest: Arc::new(Mutex::new(HashMap::new())),
            settlements: broadcast::Sender::new(SETTLEMENT_CAPACITY),
            topics: Arc::new(crate::adapters::rap::topic::PeerTopicStore::new()),
            topic_effects: Arc::new(std::sync::OnceLock::new()),
            gossip_positions: Arc::new(Mutex::new(HashMap::new())),
            now: Arc::new(|| {
                crate::domain::clock::Clock::wall_now_ms(
                    &crate::domain::clock::SystemClock::default(),
                )
            }),
        }
    }

    /// Bind this handler to the Topic log the agent's context provider reads.
    ///
    /// ⚑ Rule 1, first half: without this the handler fills a store nothing
    /// reads. The composition root passes the **same** `Arc` the `"composite"`
    /// context adapter was built with.
    #[must_use]
    pub fn with_topics(mut self, topics: Arc<crate::adapters::rap::topic::PeerTopicStore>) -> Self {
        self.topics = topics;
        self
    }

    /// Bind the journal and the transport a topic frame's effects need.
    ///
    /// ⚑ Rule 1, second half: without this, `PeerTransport::gossip_topic` has
    /// no production caller and `RoomEvent::PeerEquivocated` has no producer.
    /// Late-bound because the transport is composed after the handler.
    ///
    /// Returns `false` when effects were already bound — ⛔ a second
    /// composition never silently replaces the first.
    pub fn bind_topic_effects(&self, effects: TopicEffects) -> bool {
        self.topic_effects.set(effects).is_ok()
    }

    /// Replace the wall-millisecond clock this handler reads (hermetic tests).
    ///
    /// Mirrors `IrohPeerIngress::with_now` rather than inventing a second
    /// injection shape. The double is a `MockClock`; ⛔ not a sleep.
    #[must_use]
    pub fn with_now(mut self, now: impl Fn() -> i64 + Send + Sync + 'static) -> Self {
        self.now = Arc::new(now);
        self
    }

    /// The Topic log this handler fills.
    #[must_use]
    pub fn topics(&self) -> Arc<crate::adapters::rap::topic::PeerTopicStore> {
        Arc::clone(&self.topics)
    }

    /// Observe terminal frame outcomes, including those that settle after the
    /// caller's own wait has already timed out.
    pub fn subscribe_settlements(&self) -> broadcast::Receiver<FrameSettlement> {
        self.settlements.subscribe()
    }

    /// The durable recorder this handler journals through.
    ///
    /// Exposed so the transport ingress records the refusals that happen
    /// **before** this handler is ever reached through the same sink. ⛔ Not so a
    /// caller can journal on its behalf: the delivery outcomes below stay this
    /// type's own responsibility.
    #[must_use]
    pub fn recorder(&self) -> Arc<dyn PeerInteractionRecorder> {
        Arc::clone(&self.recorder)
    }

    pub async fn handle_verified_peer_frame(
        &self,
        envelope: AgentEnvelope<serde_json::Value>,
        peer_id: PeerId,
    ) -> Result<(), PeerDeliveryError> {
        // ⚑ Code-review P9: the shared header rules run BEFORE the kind
        // dispatch, so a topic frame is held to the same contract as a message
        // frame. The message path re-checks the identifier ceiling inside
        // `translate_verified_peer_envelope`; without this early guard the
        // topic branch skipped it, and an admitted peer could store
        // frame-sized correlation ids as Topic keys. The namespace rule is
        // uniform too: every admitted frame's recipient stays inside the
        // sender's own identity, whatever the kind.
        if envelope.header.sender.as_str().len() > MAX_PEER_ID_BYTES
            || envelope.header.recipient.as_str().len() > MAX_PEER_ID_BYTES
            || envelope.header.correlation_id.0.len() > MAX_PEER_ID_BYTES
        {
            return Err(PeerDeliveryError::IdentifierTooLong);
        }
        if !recipient_rooted_at(&envelope.header.recipient, &peer_id) {
            return Err(PeerDeliveryError::RecipientNotInSenderNamespace {
                recipient: envelope.header.recipient.as_str().to_owned(),
                peer_id: peer_id.to_string(),
            });
        }
        // Story 18.4a — replication traffic branches **before** anything that
        // makes a frame a message. A topic frame materializes no node, binds no
        // sender name, reaches no bus and wakes no agent: it updates the Topic
        // log and may journal an observation, and that is all. Branching on the
        // signed `kind` rather than sniffing the body is what keeps that a
        // compile-checked decision.
        if envelope.header.kind == MessageKind::TopicGossip {
            return self.handle_topic_frame(envelope, peer_id).await;
        }
        let mut local = translate_verified_peer_envelope(envelope)?;
        self.bind_verified_sender(&local.header.sender, &peer_id)
            .await?;
        // ⚑ DF-18-4d-RECIPIENT-NAMESPACE, closed here (18.4a owns the verified-
        // peer delivery front door). `header.sender` has been rooted at the
        // signer's `PeerId` since 17.1a — `sender_bound_to_signer` enforces it
        // on both the signing and the verifying side — but `header.recipient`
        // was materialized **verbatim**, so one admitted peer could name a node
        // inside another peer's namespace and this host would create it. The
        // recipient is now held to the same rule as the sender: an admitted
        // peer may address a node under its own identity and nowhere else.
        //
        // ⛔ Refused, ⛔ never rewritten: silently re-rooting the name would make
        // two different senders' frames land on one node.
        if !recipient_rooted_at(&local.header.recipient, &peer_id) {
            return Err(PeerDeliveryError::RecipientNotInSenderNamespace {
                recipient: local.header.recipient.as_str().to_owned(),
                peer_id: peer_id.to_string(),
            });
        }
        local.header.verified_peer_id = Some(peer_id);
        let recipient = local.header.recipient.clone();
        self.ensure_peer_context(recipient.clone()).await?;

        let correlation = local.header.correlation_id.0.clone();
        let (ack_tx, ack_rx) = oneshot::channel();
        {
            let mut pending = self.pending_ingest.lock().await;
            if pending.contains_key(&correlation) {
                return Err(PeerDeliveryError::DuplicateCorrelation(correlation));
            }
            pending.insert(correlation.clone(), ack_tx);
        }

        if let Err(error) = self
            .agent_message_bus
            .load()
            .deliver(&recipient, local)
            .await
        {
            self.pending_ingest.lock().await.remove(&correlation);
            // The bus never accepted the frame, so no worker will settle it.
            // Publish the refusal here or a parked reservation waits forever.
            let _ = self.settlements.send(FrameSettlement {
                correlation_id: correlation,
                accepted: false,
            });
            return Err(PeerDeliveryError::Delivery(error.to_string()));
        }

        match tokio::time::timeout(PEER_INGEST_TIMEOUT, ack_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(PeerDeliveryError::IngestChannelClosed),
            // Keep ownership of this correlation until the worker settles.
            // Otherwise a late worker can remove and satisfy a retry's sender.
            Err(_) => Err(PeerDeliveryError::IngestTimeout),
        }
    }

    /// Admit one verified topic-gossip frame, journal any divergence, and
    /// re-advertise what this host holds (Story 18.4a — FR150, FR150-a).
    ///
    /// # Ordering is the contract
    ///
    /// 1. prepare the admission against pre-state (nothing mutates),
    /// 2. append the durable record for every divergence,
    /// 3. commit the admission,
    /// 4. **then** re-gossip — and only when the admission changed what this
    ///    host would advertise.
    ///
    /// ⚑ Code-review P6: journaling happens **before** the commit, so a journal
    /// failure leaves the store untouched — a retried frame (the ingress rolls
    /// back the replay reservation on an error) cannot advance a feed twice or
    /// journal the same observation twice.
    ///
    /// ⚑ Code-review D4 (team ruling): re-gossip fires only when the admission
    /// admitted handles — the one event that changes this host's advertised
    /// holdings. A heads-only frame changes only the observation map, so
    /// re-advertising after it re-signs and forwards an unchanged holdings set
    /// around the membership cycle forever. The durable side must not depend
    /// on a remote hearing about it — the same rule the transport ingress
    /// states for admission refusals. A re-gossip that fails is logged and
    /// dropped: it is fire-and-forget by construction, and a peer that never
    /// hears an advertisement simply keeps the head it holds.
    async fn handle_topic_frame(
        &self,
        envelope: AgentEnvelope<serde_json::Value>,
        peer_id: PeerId,
    ) -> Result<(), PeerDeliveryError> {
        let gossip: crate::adapters::rap::topic::TopicGossip =
            serde_json::from_value(envelope.body.clone())
                .map_err(|error| PeerDeliveryError::TopicRefused(error.to_string()))?;
        let now_ms = (self.now)();
        let prepared = self
            .topics
            .prepare(&envelope.header, &peer_id, gossip, now_ms)
            .await
            .map_err(|error| PeerDeliveryError::TopicRefused(error.to_string()))?;

        if !prepared.divergences.is_empty() {
            let Some(effects) = self.topic_effects.get() else {
                // ⛔ Not silently `Ok`: a build with no journal and no transport
                // cannot honour the record-and-advertise half, and saying so is
                // the difference between "nothing happened" and "we did not
                // notice".
                return Err(PeerDeliveryError::TopicRefused(
                    "a divergent topic head was observed but this host has no journal bound to \
                     record it"
                        .to_owned(),
                ));
            };
            for divergence in &prepared.divergences {
                let event = divergence.to_room_event();
                match crate::infrastructure::subagent::node_journal::NodeJournal::open_workspace(
                    &effects.workspace,
                )
                .await
                {
                    Ok(journal) => {
                        if let Err(error) = journal.append_room(event).await {
                            return Err(PeerDeliveryError::TopicRefused(format!(
                                "two different topic heads were seen but the observation could \
                                 not be recorded ({error})"
                            )));
                        }
                    }
                    Err(error) => {
                        return Err(PeerDeliveryError::TopicRefused(format!(
                            "two different topic heads were seen but the journal could not be \
                             opened ({error})"
                        )));
                    }
                }
            }
        }

        let admission = self.topics.commit(prepared).await;

        if admission.head.is_some()
            && let Some(effects) = self.topic_effects.get()
        {
            self.regossip(effects, &peer_id, &admission).await;
        }
        Ok(())
    }

    /// The operator-disclosure producer (Story 18.4a; code-review D1 + D3).
    ///
    /// One call is the whole act, in the one process that holds both halves:
    /// grant the addressee membership of the Topic (**D1** — the share act IS
    /// the capability grant; without it a Topic's founder is its only member
    /// forever and re-gossip never fans out), mint the signed handle,
    /// advertise it on the bound transport, and record the **accepted** frame
    /// into this host's own log.
    ///
    /// ⚑ **P1/P2, structural:** the recorded header is the frame actually sent
    /// and accepted — ⛔ never a fabricated header (a head computed over a
    /// header that never shipped false-fires `PeerEquivocated` on every
    /// receiver), and ⛔ never a refused attempt (a refused frame chained
    /// locally desyncs this host's feed from every receiver's, permanently).
    /// The frame carries no self-head in its payload: the receiver computes
    /// the head from the frame itself, so the two cannot disagree.
    ///
    /// # Errors
    ///
    /// [`PeerDeliveryError::TopicRefused`] when no transport is bound (the
    /// daemon runs without the p2p listener), when the advertisement could not
    /// be written, or when the receiver corrected the feed position twice —
    /// in every case **nothing was recorded and nothing was granted... except
    /// the membership grant**, which is local bookkeeping the operator asked
    /// for and a refused send does not revoke.
    pub async fn share_handle(
        &self,
        addressee: &PeerId,
        artifact: &crate::domain::models::EvidenceArtifact,
        topic: &crate::domain::models::CorrelationId,
        summary: crate::domain::models::ContextSummary,
    ) -> Result<(), PeerDeliveryError> {
        let Some(effects) = self.topic_effects.get() else {
            return Err(PeerDeliveryError::TopicRefused(
                "this daemon has no peer transport bound; start it with the p2p listener to share"
                    .to_owned(),
            ));
        };
        let local = effects.signer.identity().peer_id.clone();
        let now_ms = (self.now)();
        let handle = crate::domain::models::ContextRef {
            artifact: artifact.id.clone(),
            content_hash: artifact.content_hash,
            producer: artifact.producer.clone(),
            // ⛔ Always this host's own identity: a handle issued under another
            // peer's name is the relabelling COLLAB D7 forbids, and the
            // receiver refuses it anyway.
            issuer: local.clone(),
            summary,
            // This host holds the artifact; it did not necessarily author it.
            provenance: crate::domain::models::ContextRefProvenance::Observed,
            not_after: now_ms.saturating_add(HANDLE_TTL_MS),
        };

        // D1: the operator's share act is the grant. Local, first, and kept
        // even if the send fails — the grant is this host's bookkeeping of a
        // disclosure decision the operator already made.
        self.topics.grant_membership(topic, addressee).await;

        let mut position = self
            .gossip_positions
            .lock()
            .await
            .get(addressee)
            .cloned()
            .unwrap_or_else(crate::domain::models::FeedPosition::start);
        for attempt in 0..2u32 {
            // ⚑ Heads are NOT embedded: the receiver derives this host's head
            // from the frame itself. An embedded self-head computed over
            // anything else is the false-equivocation defect (P1).
            let body = match serde_json::to_value(crate::adapters::rap::topic::TopicGossip {
                refs: vec![handle.clone()],
                heads: Vec::new(),
            }) {
                Ok(body) => body,
                Err(error) => {
                    return Err(PeerDeliveryError::TopicRefused(format!(
                        "the topic frame could not be encoded ({error})"
                    )));
                }
            };
            let envelope = topic_frame(&effects.signer, topic, &position, now_ms, body)?;
            match effects
                .transport
                .gossip_topic(addressee, envelope.clone())
                .await
            {
                Ok(guidance) => {
                    let corrected = guidance
                        .as_ref()
                        .and_then(|offered| position.accept_guidance(offered));
                    match corrected {
                        Some(next) if attempt == 0 => {
                            // Refused with guidance: re-sign once at the
                            // receiver's position. ⛔ Nothing is recorded for
                            // the refused frame (P2).
                            position = next;
                            continue;
                        }
                        Some(_) => {
                            return Err(PeerDeliveryError::TopicRefused(
                                "the peer's expected feed position moved twice; the share was \
                                 not recorded and may not have landed"
                                    .to_owned(),
                            ));
                        }
                        None => {
                            // Accepted (or no position volunteered). Record the
                            // frame actually sent — the header the receiver
                            // chained — so this host's log is the same function
                            // of the same frames (AC8's "both hosts agree").
                            self.topics
                                .record_local(&envelope.header, &local, vec![handle])
                                .await
                                .map_err(|error| {
                                    PeerDeliveryError::TopicRefused(format!(
                                        "the local topic head did not advance ({error})"
                                    ))
                                })?;
                            if let Ok(hash) = crate::adapters::rap::entry_hash(&envelope.header) {
                                let advanced = crate::domain::models::FeedPosition::advanced(
                                    position.next_sequence,
                                    hash,
                                );
                                self.gossip_positions
                                    .lock()
                                    .await
                                    .insert(addressee.clone(), advanced);
                            }
                            return Ok(());
                        }
                    }
                }
                Err(error) => {
                    return Err(PeerDeliveryError::TopicRefused(format!(
                        "the advertisement could not be written ({error})"
                    )));
                }
            }
        }
        Err(PeerDeliveryError::TopicRefused(
            "the advertisement was never written".to_owned(),
        ))
    }

    /// Advertise the heads this host holds to the Topic's other members.
    ///
    /// ⚑ **This is the production caller of `PeerTransport::gossip_topic`**, and
    /// it is what makes divergence detection *cross-peer*: peer C learns what
    /// peer B observed about peer A's feed, which C's own `ReplayWindow`
    /// structurally cannot see.
    ///
    /// ⛔ Heads only. Re-advertising another peer's signed **handles** is
    /// authorized re-publication (FR155), struck from this story and deferred
    /// with `DF-18-CRYPTO-CLUSTER` item C5.
    ///
    /// ⛔ Members only (AC7): a peer with no membership grant for the Topic is
    /// not sent the hashes. The hash list is itself disclosure.
    async fn regossip(
        &self,
        effects: &TopicEffects,
        origin: &PeerId,
        admission: &crate::adapters::rap::topic::TopicAdmission,
    ) {
        if admission.holdings.is_empty() {
            return;
        }
        let body = match serde_json::to_value(crate::adapters::rap::topic::TopicGossip {
            refs: Vec::new(),
            heads: admission.holdings.clone(),
        }) {
            Ok(body) => body,
            Err(error) => {
                tracing::warn!(error = %error, "topic head advertisement could not be encoded");
                return;
            }
        };
        let local = effects.signer.identity().peer_id.clone();
        for member in &admission.members {
            // ⛔ Never back to the peer this frame came from (that is an echo,
            // not an observation) and never to this host's own identity.
            if member == origin || member == &local {
                continue;
            }
            self.advertise_once(effects, member, &admission.topic, &body)
                .await;
        }
    }

    /// Write one advertisement, self-correcting the feed position at most once.
    ///
    /// The retry is bounded at one because a hostile or buggy receiver must not
    /// be able to spin the sender — the same ceiling `peer ping` applies, and
    /// for the same reason.
    async fn advertise_once(
        &self,
        effects: &TopicEffects,
        member: &PeerId,
        topic: &crate::domain::models::CorrelationId,
        body: &serde_json::Value,
    ) {
        let mut position = self
            .gossip_positions
            .lock()
            .await
            .get(member)
            .cloned()
            .unwrap_or_else(crate::domain::models::FeedPosition::start);
        for attempt in 0..2u32 {
            let envelope = match topic_frame(
                &effects.signer,
                topic,
                &position,
                (self.now)(),
                body.clone(),
            ) {
                Ok(envelope) => envelope,
                Err(error) => {
                    tracing::warn!(error = %error, "topic head advertisement could not be signed");
                    return;
                }
            };
            let header_hash = crate::adapters::rap::entry_hash(&envelope.header).ok();
            match effects.transport.gossip_topic(member, envelope).await {
                Ok(guidance) => {
                    let corrected = guidance
                        .as_ref()
                        .and_then(|offered| position.accept_guidance(offered));
                    match corrected {
                        Some(next) if attempt == 0 => {
                            position = next;
                            continue;
                        }
                        _ => {
                            // Optimistically advance. ⛔ Not a claim the peer
                            // took it: the position is this sender's own
                            // bookkeeping, and the receiver corrects it on the
                            // next frame if it disagrees.
                            if let Some(hash) = header_hash {
                                let advanced = crate::domain::models::FeedPosition::advanced(
                                    position.next_sequence,
                                    hash,
                                );
                                self.gossip_positions
                                    .lock()
                                    .await
                                    .insert(member.clone(), advanced);
                            }
                            return;
                        }
                    }
                }
                Err(error) => {
                    // Fire-and-forget: a peer that never hears this keeps the
                    // head it holds, which is a correct state, not a lost one.
                    tracing::debug!(
                        peer = %member,
                        error = %error,
                        "topic head advertisement was not delivered"
                    );
                    return;
                }
            }
        }
    }

    async fn bind_verified_sender(
        &self,
        sender: &AgentId,
        peer_id: &PeerId,
    ) -> Result<(), PeerDeliveryError> {
        self.verified_senders.lock().await.bind(sender, peer_id)
    }

    async fn ensure_peer_context(&self, recipient: AgentId) -> Result<(), PeerDeliveryError> {
        let mut materialized = self.materialized.lock().await;
        if materialized.contains(&recipient) {
            if self.node_tree.delivery_target(&recipient).await.is_some() {
                return Ok(());
            }
            materialized.remove(&recipient);
        }
        // A daemon restart restores this context's durable node with no worker
        // behind it, and the in-memory set above starts empty — so without
        // this, the first frame after a restart collides with the husk at
        // registration below, for every admitted sender, forever. A live
        // context is not in `awaiting_resume`, so this is a no-op for it.
        self.node_tree
            .retire_unresumed_peer_context(&recipient)
            .await;

        let (command_tx, mut command_rx) = mpsc::channel(1);
        let (status_tx, _) = watch::channel(NodeState::Created);
        let (_, metrics_rx) = watch::channel(AgentMetrics::default());
        let mailbox_budget = MailboxBudget::new();
        let cancel = CancellationToken::new();
        self.node_tree
            .register_peer(
                recipient.clone(),
                AgentHandle {
                    agent_id: recipient.clone(),
                    token: CapabilityTokenId::nil(),
                    command_tx,
                    cancel_token: cancel.clone(),
                    depth: 0,
                    subagent_type: "remote-peer".into(),
                    spawned_at: 0,
                    status: status_tx.clone(),
                    metrics: metrics_rx,
                    isolated: false,
                    mailbox_budget: mailbox_budget.clone(),
                },
            )
            .await
            .map_err(|error| PeerDeliveryError::Registration(error.to_string()))?;
        materialized.insert(recipient.clone());

        let node_tree = self.node_tree.clone();
        let domain_tx = self.domain_tx.clone();
        let consumer = self.consumer.clone();
        let recorder = self.recorder.clone();
        let verified_senders = self.verified_senders.clone();
        let pending_ingest = self.pending_ingest.clone();
        let materialized_set = self.materialized.clone();
        let settlements = self.settlements.clone();
        tokio::spawn(async move {
            loop {
                let op = tokio::select! {
                    _ = cancel.cancelled() => None,
                    op = command_rx.recv() => op,
                };
                let Some(op) = op else {
                    node_tree.set_state(&recipient, NodeState::Cancelled).await;
                    break;
                };
                match op {
                    crate::domain::models::Op::Kill => {
                        node_tree.set_state(&recipient, NodeState::Cancelled).await;
                        break;
                    }
                    crate::domain::models::Op::Deliver(delivery) => {
                        let disposition = delivery.disposition;
                        let response_policy = delivery.response_policy;
                        let header = delivery.envelope.header;
                        let correlation = header.correlation_id.0.clone();
                        let body = delivery.envelope.body;
                        let content_bytes = body.content.len();
                        let peer_id = verified_senders.lock().await.peer_for(&header.sender);

                        // Dispatch owns the reservation from this point onward.
                        // Release before consent/journal/consumer awaits so one
                        // slow recipient cannot consume mailbox capacity.
                        mailbox_budget.release();

                        let mut policy_refused = false;
                        let result = match peer_id.as_ref() {
                            None => Err(PeerDeliveryError::UnboundSender),
                            Some(peer_id) => match consumer
                                .consent(&recipient, &body, peer_id)
                                .await
                            {
                                Err(error) => Err(PeerDeliveryError::Consumer(error)),
                                Ok(VerifiedPeerConsent::Decline)
                                    if crate::domain::models::may_consent_refuse(disposition) =>
                                {
                                    policy_refused = true;
                                    let record = PeerDeliveryRecord {
                                        peer: peer_id.clone(),
                                        node: recipient.clone(),
                                        correlation_id: header.correlation_id.clone(),
                                        content_bytes,
                                        outcome: PeerDeliveryOutcome::Refused,
                                    };
                                    if let Err(error) = recorder.record_peer_delivery(record).await
                                    {
                                        tracing::error!(
                                            %error,
                                            recipient = %recipient,
                                            "failed to journal consent-refused peer delivery"
                                        );
                                    }
                                    Err(PeerDeliveryError::Declined)
                                }
                                Ok(VerifiedPeerConsent::Decline) => {
                                    Err(PeerDeliveryError::DeclineNotPermitted)
                                }
                                Ok(VerifiedPeerConsent::Accept) => {
                                    // Durable first: no consumer turn or other
                                    // irreversible effect starts until the
                                    // canonical acceptance exists.
                                    let record = PeerDeliveryRecord {
                                        peer: peer_id.clone(),
                                        node: recipient.clone(),
                                        correlation_id: header.correlation_id.clone(),
                                        content_bytes,
                                        outcome: PeerDeliveryOutcome::Accepted,
                                    };
                                    match recorder.record_peer_delivery(record).await {
                                        Err(error) => {
                                            tracing::error!(
                                                %error,
                                                recipient = %recipient,
                                                "refusing peer delivery because it could not be journaled"
                                            );
                                            Err(PeerDeliveryError::Transparency)
                                        }
                                        Ok(()) => consumer
                                            .ingest_with_policy(
                                                &recipient,
                                                body,
                                                peer_id,
                                                response_policy,
                                            )
                                            .await
                                            .map_err(PeerDeliveryError::Consumer),
                                    }
                                }
                            },
                        };

                        if policy_refused {
                            let receipt = crate::domain::models::refusal_receipt(
                                &header,
                                &recipient,
                                crate::domain::models::RefuseReason::Policy,
                            );
                            if domain_tx.send(receipt).is_err() {
                                settle(
                                    &pending_ingest,
                                    &settlements,
                                    correlation,
                                    Err(PeerDeliveryError::EventChannelClosed),
                                )
                                .await;
                                continue;
                            }
                        } else if result.is_ok() {
                            node_tree.mark_tainted(&recipient).await;
                            let receipt = AppEvent::Subagent(SubagentEnvelope::new(
                                header.sender.as_str().to_owned(),
                                recipient.clone(),
                                header.kind.clone(),
                                SubagentEvent::MessageDelivered {
                                    correlation_id: header.correlation_id.clone(),
                                },
                            ));
                            if domain_tx.send(receipt).is_err() {
                                settle(
                                    &pending_ingest,
                                    &settlements,
                                    correlation,
                                    Err(PeerDeliveryError::EventChannelClosed),
                                )
                                .await;
                                continue;
                            }
                        }
                        settle(&pending_ingest, &settlements, correlation, result).await;
                    }
                    _ => {}
                }
            }

            while let Ok(op) = command_rx.try_recv() {
                if let crate::domain::models::Op::Deliver(delivery) = op {
                    mailbox_budget.release();
                    let correlation = delivery.envelope.header.correlation_id.0.clone();
                    settle(
                        &pending_ingest,
                        &settlements,
                        correlation,
                        Err(PeerDeliveryError::ContextClosed),
                    )
                    .await;
                }
            }
            materialized_set.lock().await.remove(&recipient);
        });
        Ok(())
    }

    pub async fn clear_all_taint(&self) {
        let recipients: Vec<AgentId> = self.materialized.lock().await.iter().cloned().collect();
        for recipient in recipients {
            self.node_tree.clear_taint(&recipient).await;
        }
    }
}

/// The permanent sender-name → peer binding, bounded per peer.
///
/// # Why the binding is permanent
///
/// Once a sender name has been seen from one peer, no other peer may ever claim
/// it. That is what stops an admitted peer from impersonating another admitted
/// peer's agent, and it is why the table never forgets an entry.
///
/// # Why it is bounded (Story 18.4d, ruling P12)
///
/// "Permanent" and "unbounded" together is a memory leak an admitted peer
/// controls: the signing rule accepts any `<peer_id>/<anything>` as a sender, so
/// one peer can mint distinct names forever. The per-peer count is therefore
/// capped at [`MAX_SENDERS_PER_PEER`]. ⛔ The cap does **not** evict: evicting a
/// binding is exactly the impersonation window the binding exists to close, so a
/// peer past its cap is refused instead.
#[derive(Default)]
struct VerifiedSenders {
    bound: HashMap<AgentId, PeerId>,
    per_peer: HashMap<PeerId, usize>,
}

impl VerifiedSenders {
    fn peer_for(&self, sender: &AgentId) -> Option<PeerId> {
        self.bound.get(sender).cloned()
    }

    fn bind(&mut self, sender: &AgentId, peer_id: &PeerId) -> Result<(), PeerDeliveryError> {
        match self.bound.get(sender) {
            Some(bound) if bound != peer_id => Err(PeerDeliveryError::PeerBindingMismatch),
            // The legitimate repeat: the same peer resending under a name it
            // already owns costs nothing and consumes no new budget.
            Some(_) => Ok(()),
            None => {
                let count = self.per_peer.entry(peer_id.clone()).or_insert(0);
                if *count >= MAX_SENDERS_PER_PEER {
                    return Err(PeerDeliveryError::SenderBudgetExhausted);
                }
                *count += 1;
                self.bound.insert(sender.clone(), peer_id.clone());
                Ok(())
            }
        }
    }
}

/// Resolve one correlation exactly once: answer the caller if it is still
/// waiting, and publish the outcome for a caller whose wait already expired.
///
/// The oneshot alone is not enough. A transport that timed out has dropped its
/// receiver but may still be holding replay state for this frame, and a dropped
/// `send` tells it nothing. Every terminal path routes through here so that
/// "the worker settled" is observable even when nobody is left on the oneshot.
async fn settle(
    pending_ingest: &Mutex<HashMap<String, oneshot::Sender<Result<(), PeerDeliveryError>>>>,
    settlements: &broadcast::Sender<FrameSettlement>,
    correlation: String,
    result: Result<(), PeerDeliveryError>,
) {
    let accepted = result.is_ok();
    if let Some(ack) = pending_ingest.lock().await.remove(&correlation) {
        let _ = ack.send(result);
    }
    let _ = settlements.send(FrameSettlement {
        correlation_id: correlation,
        accepted,
    });
}

/// Peer-path invariant, recipient side (`DF-18-4d-RECIPIENT-NAMESPACE`).
///
/// Mirrors the shipped sender rule (`rap::wire::sender_bound_to_signer`) rather
/// than inventing a second one: a recipient is the bare `PeerId` or a peer path
/// `<peer_id>/<child>[/...]` under it. ⛔ The rule is deliberately the *same*
/// shape — a second, subtly different namespace predicate is how one side ends
/// up admitting what the other refuses.
#[must_use]
pub fn recipient_rooted_at(recipient: &AgentId, peer_id: &PeerId) -> bool {
    let name = recipient.as_str();
    let pid = peer_id.as_str();
    name == pid || name.starts_with(&format!("{pid}/"))
}

/// Sign one topic-gossip frame from this host (Story 18.4a).
///
/// # The wire identities are protocol, not fixture detail
///
/// Both are rooted at **this host's own** `PeerId`, exactly as `peer ping`'s
/// are: a sender may only ever name a node inside its own identity namespace,
/// so this producer can never ask a receiver to materialize a node it did not
/// choose. ⚑ In practice the receiver materializes nothing at all — a topic
/// frame branches away before `ensure_peer_context` — but the invariant is
/// stated by the identity rather than relied on from the branch.
///
/// `not_after_ms` is wall **milliseconds**, the unit this frame path's verify
/// seam uses; ⛔ not the seconds a `PeerTicket` uses.
///
/// # Errors
///
/// Propagates a signing failure from the single `rap::wire` sign seam.
pub fn topic_frame(
    signer: &crate::adapters::rap::AgentSigner,
    topic: &CorrelationId,
    position: &crate::domain::models::FeedPosition,
    now_ms: i64,
    body: serde_json::Value,
) -> Result<AgentEnvelope<serde_json::Value>, PeerDeliveryError> {
    let local = signer.identity().peer_id.clone();
    let sender = AgentId::from_peer_path(&format!("{}/{TOPIC_SENDER_SUFFIX}", local.as_str()))
        .map_err(|error| PeerDeliveryError::TopicRefused(error.to_string()))?;
    let recipient =
        AgentId::from_peer_path(&format!("{}/{TOPIC_RECIPIENT_SUFFIX}", local.as_str()))
            .map_err(|error| PeerDeliveryError::TopicRefused(error.to_string()))?;
    signer
        .sign(
            sender,
            recipient,
            topic.clone(),
            MessageKind::TopicGossip,
            String::new(),
            position.next_sequence,
            now_ms.saturating_add(TOPIC_FRAME_TTL_MS),
            format!(
                "topic-{}-{now_ms}-{}",
                std::process::id(),
                position.next_sequence
            ),
            position.prev_hash.clone(),
            body,
        )
        .map_err(|error| PeerDeliveryError::TopicRefused(error.to_string()))
}

pub fn translate_verified_peer_envelope(
    envelope: AgentEnvelope<serde_json::Value>,
) -> Result<Envelope<AgentMessage>, PeerDeliveryError> {
    if envelope.header.sender.as_str().len() > MAX_PEER_ID_BYTES
        || envelope.header.recipient.as_str().len() > MAX_PEER_ID_BYTES
        || envelope.header.correlation_id.0.len() > MAX_PEER_ID_BYTES
    {
        return Err(PeerDeliveryError::IdentifierTooLong);
    }
    if envelope.header.kind != MessageKind::PeerMessage {
        return Err(PeerDeliveryError::InvalidKind);
    }
    let content = envelope
        .body
        .as_str()
        .or_else(|| envelope.body.get("msg").and_then(serde_json::Value::as_str))
        .ok_or(PeerDeliveryError::InvalidBody)?;
    if content.len() > MAX_PEER_MESSAGE_BYTES {
        return Err(PeerDeliveryError::BodyTooLarge);
    }
    Ok(Envelope::new(
        MessageHeader {
            sender: envelope.header.sender,
            recipient: envelope.header.recipient,
            correlation_id: envelope.header.correlation_id,
            kind: envelope.header.kind,
            message_type: SemanticMessageType::parse(&envelope.header.message_type),
            sequence: None,
            verified_peer_id: None,
        },
        AgentMessage::new(content),
    ))
}

#[derive(Debug, thiserror::Error)]
pub enum PeerDeliveryError {
    #[error("peer frames must use PeerMessage kind")]
    InvalidKind,
    #[error("peer frame body must be a string")]
    InvalidBody,
    #[error("peer frame body exceeds {MAX_PEER_MESSAGE_BYTES} bytes")]
    BodyTooLarge,
    #[error("peer frame identifier exceeds {MAX_PEER_ID_BYTES} bytes")]
    IdentifierTooLong,
    #[error("peer sender is already bound to a different verified PeerId")]
    PeerBindingMismatch,
    #[error("this peer already holds the maximum of {MAX_SENDERS_PER_PEER} bound sender names")]
    SenderBudgetExhausted,
    #[error("duplicate in-flight peer correlation id: {0}")]
    DuplicateCorrelation(String),
    #[error("verified peer sender was not bound")]
    UnboundSender,
    #[error("could not materialize peer recipient: {0}")]
    Registration(String),
    #[error("message bus delivery failed: {0}")]
    Delivery(String),
    #[error("peer recipient rejected ingest: {0}")]
    Consumer(String),
    #[error("peer recipient declined ingest")]
    Declined,
    #[error("recipient relationship does not permit consent refusal")]
    DeclineNotPermitted,
    #[error("peer delivery could not be journaled")]
    Transparency,
    #[error("peer ingest acknowledgement timed out")]
    IngestTimeout,
    #[error("peer ingest acknowledgement channel closed")]
    IngestChannelClosed,
    #[error("peer recipient context closed")]
    ContextClosed,
    #[error("delivery receipt event channel closed")]
    EventChannelClosed,
    /// A topic-gossip frame was refused (Story 18.4a).
    ///
    /// ⛔ Distinct from every delivery arm above: nothing was delivered,
    /// nothing was materialized, and no agent saw the frame. It carries the
    /// reason the replication layer declined it.
    #[error("peer topic frame refused: {0}")]
    TopicRefused(String),
    /// The frame named a recipient outside the sending peer's own namespace
    /// (`DF-18-4d-RECIPIENT-NAMESPACE`, closed by Story 18.4a).
    ///
    /// ⛔ Never rewritten into a legal name: re-rooting would let two senders'
    /// frames land on one node.
    #[error("peer {peer_id} may not address {recipient}: it is outside that peer's namespace")]
    RecipientNotInSenderNamespace { recipient: String, peer_id: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{
        AgentEnvelopeHeader, CorrelationId, DeliveryDisposition, Ed25519Sig, MessageHeader,
        MessageKind, OwnershipKind, PeerIdentity, RefuseReason,
    };
    use crate::domain::ports::{DeliveryPolicy, RelationshipDeliveryPolicy};
    use crate::infrastructure::agent_message_bus::LocalMessageBus;

    struct RecordingConsumer(Mutex<Vec<String>>);

    #[async_trait]
    impl VerifiedPeerConsumer for RecordingConsumer {
        async fn consent(
            &self,
            _recipient: &AgentId,
            _content: &AgentMessage,
            _peer_id: &PeerId,
        ) -> Result<VerifiedPeerConsent, String> {
            Ok(VerifiedPeerConsent::Accept)
        }

        async fn ingest(
            &self,
            _recipient: &AgentId,
            content: AgentMessage,
            _peer_id: &PeerId,
        ) -> Result<(), String> {
            self.0.lock().await.push(content.content);
            Ok(())
        }
    }

    #[derive(Default)]
    struct RecordingRecorder(Mutex<Vec<PeerDeliveryRecord>>);

    #[async_trait]
    impl PeerInteractionRecorder for RecordingRecorder {
        async fn record_peer_delivery(&self, record: PeerDeliveryRecord) -> Result<(), String> {
            self.0.lock().await.push(record);
            Ok(())
        }

        async fn record_transport_refusal(
            &self,
            _record: crate::domain::ports::TransportRefusalRecord,
        ) -> Result<(), String> {
            Ok(())
        }
    }

    fn peer() -> PeerIdentity {
        PeerIdentity::from_public_key(vec![7; 32]).expect("test peer identity")
    }

    /// The fixture recipient, rooted at the fixture peer's own namespace — the
    /// shape every shipped producer signs and the shape
    /// `DF-18-4d-RECIPIENT-NAMESPACE` (closed by 18.4a) now requires of a
    /// recipient. ⚠ Pre-18.4a these fixtures used bare `peer-agent` /
    /// `local-peer-session`, which no receiver accepts any more.
    fn fixture_recipient() -> AgentId {
        let pid = peer().peer_id.as_str().to_owned();
        AgentId::from_peer_path(&format!("{pid}/local-peer-session"))
            .expect("peer-rooted fixture recipient")
    }

    fn envelope(kind: MessageKind, body: serde_json::Value) -> AgentEnvelope<serde_json::Value> {
        let recipient = fixture_recipient();
        let pid = peer().peer_id.as_str().to_owned();
        let sender = AgentId::from_peer_path(&format!("{pid}/peer-agent"))
            .expect("peer-rooted fixture sender");
        AgentEnvelope::new(
            AgentEnvelopeHeader {
                message_type: String::new(),
                sender,
                recipient,
                correlation_id: CorrelationId::new("corr-1"),
                kind,
                sequence: 9,
                not_after: i64::MAX,
                nonce: "nonce".into(),
                content_hash: vec![1],
                prev_hash: vec![2],
            },
            body,
            peer(),
            Ed25519Sig(vec![]),
        )
    }

    fn handler() -> (
        VerifiedPeerFrameHandler,
        NodeTree,
        mpsc::UnboundedReceiver<AppEvent>,
        Arc<RecordingConsumer>,
    ) {
        let (domain_tx, domain_rx) = mpsc::unbounded_channel();
        let node_tree = NodeTree::new();
        let bus = Arc::new(LocalMessageBus::new(
            node_tree.clone(),
            Arc::new(RelationshipDeliveryPolicy) as Arc<dyn DeliveryPolicy>,
        )) as Arc<dyn AgentMessageBus>;
        let bus_slot = Arc::new(ArcSwap::from_pointee(bus));
        let consumer = Arc::new(RecordingConsumer(Mutex::new(Vec::new())));
        let recorder = Arc::new(RecordingRecorder::default());
        (
            VerifiedPeerFrameHandler::new(
                node_tree.clone(),
                bus_slot,
                domain_tx,
                consumer.clone(),
                recorder,
            ),
            node_tree,
            domain_rx,
            consumer,
        )
    }

    /// A daemon restart restores the recipient's durable node with no worker
    /// behind it; the first frame after the restart must still land — the
    /// husk is retired and the context rematerialized.
    #[tokio::test]
    async fn peer_context_is_rematerialized_after_a_daemon_restart() {
        let (first_handler, node_tree, _domain_rx, _consumer) = handler();
        let signed = envelope(MessageKind::PeerMessage, serde_json::json!("one"));
        let peer_id = signed.signer.peer_id.clone();
        first_handler
            .handle_verified_peer_frame(signed, peer_id.clone())
            .await
            .unwrap();
        let id = fixture_recipient();
        assert!(node_tree.delivery_target(&id).await.is_some());

        // The restart: a fresh handler (empty in-memory set) over a tree
        // whose recipient node was restored from its durable checkpoint —
        // fabricated handle, awaiting a resume that never comes.
        let (fresh_handler, fresh_tree, _domain_rx2, _consumer2) = handler();
        fresh_tree
            .restore_checkpoint(crate::domain::models::NodeCheckpoint {
                id: id.clone(),
                token: CapabilityTokenId::nil(),
                parent: Some(AgentId::root()),
                ownership: crate::domain::models::subagent_view::WireOwnershipKind::Peer,
                state: NodeState::Cancelled,
                origin: crate::domain::models::NodeOrigin::Remote,
                foreground: false,
                effective_model: String::new(),
                tokens_in: 0,
                tokens_out: 0,
                turns: 0,
                subagent_type: "remote-peer".to_owned(),
                spawned_at: 0,
                depth: 1,
                tainted: false,
                waiting_since: None,
                wait_reason: None,
            })
            .await
            .expect("the husk restores");

        let mut next = envelope(MessageKind::PeerMessage, serde_json::json!("two"));
        next.header.correlation_id = CorrelationId::new("corr-restart");
        fresh_handler
            .handle_verified_peer_frame(next, peer_id)
            .await
            .expect("the restored husk is retired and the context rematerialized");
    }

    /// The retirement must never touch a live context.
    #[tokio::test]
    async fn only_an_unresumed_remote_peer_husk_is_retired() {
        let (handler, node_tree, _domain_rx, _consumer) = handler();
        let signed = envelope(MessageKind::PeerMessage, serde_json::json!("one"));
        let peer_id = signed.signer.peer_id.clone();
        handler
            .handle_verified_peer_frame(signed, peer_id)
            .await
            .unwrap();
        let id = fixture_recipient();
        assert!(
            !node_tree.retire_unresumed_peer_context(&id).await,
            "a live context is not a husk"
        );
        assert!(node_tree.delivery_target(&id).await.is_some());
    }

    #[test]
    fn translation_accepts_only_bounded_peer_text_and_strips_wire_sequence() {
        let translated = translate_verified_peer_envelope(envelope(
            MessageKind::PeerMessage,
            serde_json::json!("hello"),
        ))
        .expect("valid peer text");
        assert_eq!(translated.body.content, "hello");
        assert_eq!(translated.header.sequence, None);
        assert!(matches!(translated.header.kind, MessageKind::PeerMessage));
        assert!(matches!(
            translate_verified_peer_envelope(envelope(
                MessageKind::OwnerReport,
                serde_json::json!("hello")
            )),
            Err(PeerDeliveryError::InvalidKind)
        ));
        assert!(matches!(
            translate_verified_peer_envelope(envelope(
                MessageKind::PeerMessage,
                serde_json::json!({"text": "hello"})
            )),
            Err(PeerDeliveryError::InvalidBody)
        ));
        let mut oversized_identifier = envelope(MessageKind::PeerMessage, serde_json::json!("x"));
        oversized_identifier.header.correlation_id =
            CorrelationId::new("x".repeat(MAX_PEER_ID_BYTES + 1));
        assert!(matches!(
            translate_verified_peer_envelope(oversized_identifier),
            Err(PeerDeliveryError::IdentifierTooLong)
        ));
        assert!(matches!(
            translate_verified_peer_envelope(envelope(
                MessageKind::PeerMessage,
                serde_json::Value::String("x".repeat(MAX_PEER_MESSAGE_BYTES + 1))
            )),
            Err(PeerDeliveryError::BodyTooLarge)
        ));
    }

    #[tokio::test]
    async fn verified_peer_frame_uses_composed_bus_and_ingests_before_receipt() {
        let (handler, node_tree, mut domain_rx, consumer) = handler();
        let signed = envelope(
            MessageKind::PeerMessage,
            serde_json::json!({"msg": "hello", "tainted": false}),
        );
        let peer_id = signed.signer.peer_id.clone();
        handler
            .handle_verified_peer_frame(signed, peer_id)
            .await
            .expect("live local bus must ingest verified peer frame");

        assert_eq!(consumer.0.lock().await.as_slice(), &["hello"]);
        let event = domain_rx.recv().await.expect("delivery receipt");
        assert!(matches!(
            event,
            AppEvent::Subagent(SubagentEnvelope {
                event: SubagentEvent::MessageDelivered { correlation_id },
                ..
            }) if correlation_id == CorrelationId::new("corr-1")
        ));
        assert!(node_tree.is_tainted(&fixture_recipient()).await);
        handler.clear_all_taint().await;
        assert!(
            !node_tree.is_tainted(&fixture_recipient()).await,
            "a local true-context reset must clear the peer-tainted context"
        );
    }

    #[tokio::test]
    async fn peer_context_handles_kill_and_can_be_rematerialized() {
        let (handler, node_tree, _domain_rx, _consumer) = handler();
        let signed = envelope(MessageKind::PeerMessage, serde_json::json!("one"));
        let peer_id = signed.signer.peer_id.clone();
        handler
            .handle_verified_peer_frame(signed, peer_id.clone())
            .await
            .unwrap();
        let id = fixture_recipient();
        node_tree
            .cascade_kill(&id, Duration::from_secs(1))
            .await
            .expect("peer context cooperates with kill");

        let mut next = envelope(MessageKind::PeerMessage, serde_json::json!("two"));
        next.header.correlation_id = CorrelationId::new("corr-2");
        handler
            .handle_verified_peer_frame(next, peer_id)
            .await
            .expect("stale materialization is recreated");
    }

    // ── Story 18.3 (AC1) — consent enforcement on the live peer path ──

    /// Always declines. The SAME hostile consumer drives both halves of the
    /// differential, so the only variable is the stamped disposition.
    struct DecliningConsumer;

    #[async_trait]
    impl VerifiedPeerConsumer for DecliningConsumer {
        async fn consent(
            &self,
            _recipient: &AgentId,
            _content: &AgentMessage,
            _peer_id: &PeerId,
        ) -> Result<VerifiedPeerConsent, String> {
            Ok(VerifiedPeerConsent::Decline)
        }

        async fn ingest(
            &self,
            _recipient: &AgentId,
            _content: AgentMessage,
            _peer_id: &PeerId,
        ) -> Result<(), String> {
            panic!("declined content must never reach ingest")
        }
    }

    struct FailingIngestConsumer;

    #[async_trait]
    impl VerifiedPeerConsumer for FailingIngestConsumer {
        async fn consent(
            &self,
            _recipient: &AgentId,
            _content: &AgentMessage,
            _peer_id: &PeerId,
        ) -> Result<VerifiedPeerConsent, String> {
            Ok(VerifiedPeerConsent::Accept)
        }

        async fn ingest(
            &self,
            _recipient: &AgentId,
            _content: AgentMessage,
            _peer_id: &PeerId,
        ) -> Result<(), String> {
            Err("injected ingest failure".to_owned())
        }
    }

    struct BrokenRecorder;

    #[async_trait]
    impl PeerInteractionRecorder for BrokenRecorder {
        async fn record_peer_delivery(&self, _record: PeerDeliveryRecord) -> Result<(), String> {
            Err("injected journal failure".to_owned())
        }

        async fn record_transport_refusal(
            &self,
            _record: crate::domain::ports::TransportRefusalRecord,
        ) -> Result<(), String> {
            Err("injected journal failure".to_owned())
        }
    }

    struct BlockingFirstRecorder {
        calls: std::sync::atomic::AtomicUsize,
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }

    impl BlockingFirstRecorder {
        fn new() -> Self {
            Self {
                calls: std::sync::atomic::AtomicUsize::new(0),
                entered: tokio::sync::Notify::new(),
                release: tokio::sync::Notify::new(),
            }
        }
    }

    #[async_trait]
    impl PeerInteractionRecorder for BlockingFirstRecorder {
        async fn record_peer_delivery(&self, _record: PeerDeliveryRecord) -> Result<(), String> {
            if self.calls.fetch_add(1, std::sync::atomic::Ordering::AcqRel) == 0 {
                self.entered.notify_one();
                self.release.notified().await;
            }
            Ok(())
        }

        async fn record_transport_refusal(
            &self,
            _record: crate::domain::ports::TransportRefusalRecord,
        ) -> Result<(), String> {
            Ok(())
        }
    }

    /// Stamps a fixed disposition regardless of ownership, so the same arm can be
    /// shown a `MustReport` delivery. Mirrors the hostile-policy pattern the
    /// 14-4a `t7` keystones use on the local bus.
    struct ForceDisposition(DeliveryDisposition);

    impl DeliveryPolicy for ForceDisposition {
        fn decide(
            &self,
            _header: &MessageHeader,
            _ownership: OwnershipKind,
        ) -> DeliveryDisposition {
            self.0
        }
    }

    fn handler_with(
        policy: Arc<dyn DeliveryPolicy>,
        consumer: Arc<dyn VerifiedPeerConsumer>,
    ) -> (
        VerifiedPeerFrameHandler,
        NodeTree,
        mpsc::UnboundedReceiver<AppEvent>,
    ) {
        handler_with_recorder(policy, consumer, Arc::new(RecordingRecorder::default()))
    }

    fn handler_with_recorder(
        policy: Arc<dyn DeliveryPolicy>,
        consumer: Arc<dyn VerifiedPeerConsumer>,
        recorder: Arc<dyn PeerInteractionRecorder>,
    ) -> (
        VerifiedPeerFrameHandler,
        NodeTree,
        mpsc::UnboundedReceiver<AppEvent>,
    ) {
        let (domain_tx, domain_rx) = mpsc::unbounded_channel();
        let node_tree = NodeTree::new();
        let bus =
            Arc::new(LocalMessageBus::new(node_tree.clone(), policy)) as Arc<dyn AgentMessageBus>;
        let bus_slot = Arc::new(ArcSwap::from_pointee(bus));
        (
            VerifiedPeerFrameHandler::new(
                node_tree.clone(),
                bus_slot,
                domain_tx,
                consumer,
                recorder,
            ),
            node_tree,
            domain_rx,
        )
    }

    struct FixedResponseMode(crate::domain::models::ResponseMode);

    impl DeliveryPolicy for FixedResponseMode {
        fn decide(&self, _header: &MessageHeader, ownership: OwnershipKind) -> DeliveryDisposition {
            crate::domain::models::relationship_disposition(ownership)
        }

        fn response_policy_for_peer(
            &self,
            _peer_id: &PeerId,
            _message_type: crate::domain::models::SemanticMessageType,
        ) -> crate::domain::ports::PeerResponsePolicy {
            crate::domain::ports::PeerResponsePolicy {
                mode: self.0,
                auto_response: None,
                ..Default::default()
            }
        }
    }

    #[derive(Default)]
    struct TypeResponseModes(parking_lot::Mutex<Vec<crate::domain::models::SemanticMessageType>>);

    impl DeliveryPolicy for TypeResponseModes {
        fn decide(&self, _header: &MessageHeader, ownership: OwnershipKind) -> DeliveryDisposition {
            crate::domain::models::relationship_disposition(ownership)
        }

        fn response_policy_for_peer(
            &self,
            _peer_id: &PeerId,
            message_type: crate::domain::models::SemanticMessageType,
        ) -> crate::domain::ports::PeerResponsePolicy {
            self.0.lock().push(message_type);
            crate::domain::ports::PeerResponsePolicy {
                mode: match message_type {
                    crate::domain::models::SemanticMessageType::Consultation => {
                        crate::domain::models::ResponseMode::NotifyAndDraft
                    }
                    crate::domain::models::SemanticMessageType::BugReport => {
                        crate::domain::models::ResponseMode::NotifyAndAuto
                    }
                    _ => crate::domain::models::ResponseMode::NotifyAndWait,
                },
                ..Default::default()
            }
        }
    }

    #[derive(Default)]
    struct ModeRecordingConsumer(Mutex<Vec<crate::domain::models::ResponseMode>>);

    #[async_trait]
    impl VerifiedPeerConsumer for ModeRecordingConsumer {
        async fn consent(
            &self,
            _recipient: &AgentId,
            _content: &AgentMessage,
            _peer_id: &PeerId,
        ) -> Result<VerifiedPeerConsent, String> {
            Ok(VerifiedPeerConsent::Accept)
        }

        async fn ingest(
            &self,
            _recipient: &AgentId,
            _content: AgentMessage,
            _peer_id: &PeerId,
        ) -> Result<(), String> {
            Err("response policy was discarded".to_owned())
        }

        async fn ingest_with_policy(
            &self,
            _recipient: &AgentId,
            _content: AgentMessage,
            _peer_id: &PeerId,
            response_policy: crate::domain::ports::PeerResponsePolicy,
        ) -> Result<(), String> {
            self.0.lock().await.push(response_policy.mode);
            Ok(())
        }
    }

    #[tokio::test]
    async fn verified_front_door_branches_with_the_bus_selected_response_mode() {
        let consumer = Arc::new(ModeRecordingConsumer::default());
        let (handler, _tree, _events) = handler_with(
            Arc::new(FixedResponseMode(
                crate::domain::models::ResponseMode::NotifyAndDraft,
            )),
            consumer.clone(),
        );
        let signed = envelope(MessageKind::PeerMessage, serde_json::json!("hello"));
        let peer_id = signed.signer.peer_id.clone();
        handler
            .handle_verified_peer_frame(signed, peer_id)
            .await
            .expect("verified delivery reaches the mode-aware consumer");
        assert_eq!(
            consumer.0.lock().await.as_slice(),
            &[crate::domain::models::ResponseMode::NotifyAndDraft]
        );
    }

    #[tokio::test]
    async fn verified_front_door_resolves_response_mode_from_the_signed_message_type() {
        let policy = Arc::new(TypeResponseModes::default());
        let consumer = Arc::new(ModeRecordingConsumer::default());
        let (handler, _tree, _events) = handler_with(policy.clone(), consumer.clone());

        for (index, (token, expected_type)) in [
            (
                "consultation",
                crate::domain::models::SemanticMessageType::Consultation,
            ),
            (
                "bug_report",
                crate::domain::models::SemanticMessageType::BugReport,
            ),
            (
                "future_type",
                crate::domain::models::SemanticMessageType::Unknown,
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let mut signed = envelope(MessageKind::PeerMessage, serde_json::json!("typed message"));
            signed.header.message_type = token.to_owned();
            signed.header.correlation_id = CorrelationId::new(format!("typed-{index}"));
            let peer_id = signed.signer.peer_id.clone();
            handler
                .handle_verified_peer_frame(signed, peer_id)
                .await
                .expect("typed delivery reaches the mode-aware consumer");
            assert_eq!(policy.0.lock()[index], expected_type);
        }

        assert_eq!(
            consumer.0.lock().await.as_slice(),
            &[
                crate::domain::models::ResponseMode::NotifyAndDraft,
                crate::domain::models::ResponseMode::NotifyAndAuto,
                crate::domain::models::ResponseMode::NotifyAndWait,
            ],
            "consultation, bug_report, and Unknown must select distinct effective modes"
        );
    }

    /// Drive the FRONT DOOR — `handle_verified_peer_frame`, the only production
    /// caller of `LocalMessageBus::deliver` — over a real bus and a real
    /// `NodeTree` holding a real registered peer node. Returns whatever receipt
    /// reached the real `domain_tx`.
    ///
    /// Deliberately NOT `may_consent_refuse(...)` called directly, and NOT an
    /// `AgentDelivery` built in-test: either would prove the predicate rather
    /// than the path (AC1's forbidden bypass).
    async fn drive_declining_ingest(disposition: DeliveryDisposition) -> Option<AppEvent> {
        let (handler, _tree, mut domain_rx) = handler_with(
            Arc::new(ForceDisposition(disposition)),
            Arc::new(DecliningConsumer),
        );
        let signed = envelope(MessageKind::PeerMessage, serde_json::json!("hello"));
        let peer_id = signed.signer.peer_id.clone();
        let outcome = handler.handle_verified_peer_frame(signed, peer_id).await;
        assert!(
            outcome.is_err(),
            "a declining consumer must never report success"
        );
        // The arm sends the receipt BEFORE acking, and the front door awaits the
        // ack — so this is deterministic with no sleep.
        domain_rx.try_recv().ok()
    }

    /// [K1] AC1 differential — the same declining consumer through the same arm
    /// produces observably different sender-visible outcomes depending ONLY on
    /// the stamped disposition. Before 18.3 the arm never read `disposition`, so
    /// these two cases were indistinguishable.
    ///
    /// Mutants that turn this RED: (a) drop the `may_consent_refuse` read
    /// (`let consent_refused = result.is_err()`) — MustReport then also emits a
    /// Policy receipt; (c) emit `MessageDelivered` instead of `MessageRefused`
    /// on the refusal.
    #[tokio::test]
    async fn ac1_may_refuse_peer_gets_policy_receipt_but_must_report_does_not() {
        let refused = drive_declining_ingest(DeliveryDisposition::MayRefuse).await;
        assert!(
            matches!(
                &refused,
                Some(AppEvent::Subagent(SubagentEnvelope {
                    event: SubagentEvent::MessageRefused {
                        reason: RefuseReason::Policy,
                        correlation_id,
                    },
                    ..
                })) if *correlation_id == CorrelationId::new("corr-1")
            ),
            "a MayRefuse peer that declines must produce MessageRefused{{Policy}}; got {refused:?}"
        );

        let reported = drive_declining_ingest(DeliveryDisposition::MustReport).await;
        assert!(
            reported.is_none(),
            "a MustReport recipient cannot consent-refuse, so the IDENTICAL decline \
             must not produce a policy receipt; got {reported:?}"
        );
    }

    /// [K1] positive control — the arm can still fire the other way. Without
    /// this, "refusal works" would also be satisfied by an arm that refuses
    /// everything.
    #[tokio::test]
    async fn ac1_positive_control_accepting_peer_ingests_and_reports_delivered() {
        let (handler, _tree, mut domain_rx, consumer) = handler();
        let signed = envelope(MessageKind::PeerMessage, serde_json::json!("hello"));
        let peer_id = signed.signer.peer_id.clone();
        handler
            .handle_verified_peer_frame(signed, peer_id)
            .await
            .expect("a Peer recipient that does NOT refuse must still ingest");
        assert_eq!(consumer.0.lock().await.as_slice(), &["hello"]);
        assert!(matches!(
            domain_rx.try_recv().expect("delivery receipt"),
            AppEvent::Subagent(SubagentEnvelope {
                event: SubagentEvent::MessageDelivered { .. },
                ..
            })
        ));
    }

    /// Drive `n` verified frames through the real front door and report the
    /// peer node's `(reserved_total, released_total, live)` budget counters.
    async fn budget_after(
        policy: Arc<dyn DeliveryPolicy>,
        consumer: Arc<dyn VerifiedPeerConsumer>,
        n: usize,
    ) -> (usize, usize, usize) {
        let (handler, tree, _domain_rx) = handler_with(policy, consumer);
        for i in 0..n {
            let mut signed = envelope(MessageKind::PeerMessage, serde_json::json!("hello"));
            signed.header.correlation_id = CorrelationId::new(format!("corr-{i}"));
            let peer_id = signed.signer.peer_id.clone();
            let _ = handler.handle_verified_peer_frame(signed, peer_id).await;
        }
        let target = tree
            .delivery_target(&fixture_recipient())
            .await
            .expect("the peer node must still be registered");
        (
            target.mailbox_budget.reserved_total(),
            target.mailbox_budget.released_total(),
            target.mailbox_budget.current(),
        )
    }

    /// [K2] AC2 structural ratchet (Rule 4) — a DETERMINISTIC counter, never a
    /// timing window. All six settlement paths release exactly once: legacy
    /// relationship acceptance, the three response modes, consent refusal, and
    /// a consumer failure.
    #[tokio::test]
    async fn ac2_every_reserve_is_matched_by_exactly_one_release() {
        let accepting =
            || Arc::new(RecordingConsumer(Mutex::new(Vec::new()))) as Arc<dyn VerifiedPeerConsumer>;
        let cases: Vec<(&str, Arc<dyn DeliveryPolicy>, Arc<dyn VerifiedPeerConsumer>)> = vec![
            (
                "relationship-accepted",
                Arc::new(RelationshipDeliveryPolicy),
                accepting(),
            ),
            (
                "notify-and-wait",
                Arc::new(FixedResponseMode(
                    crate::domain::models::ResponseMode::NotifyAndWait,
                )),
                accepting(),
            ),
            (
                "notify-and-draft",
                Arc::new(FixedResponseMode(
                    crate::domain::models::ResponseMode::NotifyAndDraft,
                )),
                accepting(),
            ),
            (
                "notify-and-auto",
                Arc::new(FixedResponseMode(
                    crate::domain::models::ResponseMode::NotifyAndAuto,
                )),
                accepting(),
            ),
            (
                "consent-refused",
                Arc::new(RelationshipDeliveryPolicy),
                Arc::new(DecliningConsumer),
            ),
            (
                "consumer-error",
                Arc::new(FixedResponseMode(
                    crate::domain::models::ResponseMode::NotifyAndAuto,
                )),
                Arc::new(FailingIngestConsumer),
            ),
        ];
        assert_eq!(cases.len(), 6, "settlement path inventory changed");
        for (label, policy, consumer) in cases {
            let (reserved, released, live) = budget_after(policy, consumer, 4).await;
            assert_eq!(
                reserved, 4,
                "{label}: positive control — the deliveries must actually have reserved"
            );
            assert_eq!(
                released, reserved,
                "{label}: exactly one release per reserve (got {released} releases for {reserved} reserves)"
            );
            assert_eq!(live, 0, "{label}: no slot may be left outstanding");
        }
    }
    #[tokio::test(start_paused = true)]
    async fn timed_out_correlation_remains_owned_until_worker_settles() {
        let consumer = Arc::new(RecordingConsumer(Mutex::new(Vec::new())));
        let recorder = Arc::new(BlockingFirstRecorder::new());
        let (handler, _tree, _events) = handler_with_recorder(
            Arc::new(RelationshipDeliveryPolicy),
            consumer.clone(),
            recorder.clone(),
        );
        let handler = Arc::new(handler);
        let signed = envelope(MessageKind::PeerMessage, serde_json::json!("hello"));
        let peer_id = signed.signer.peer_id.clone();
        let entered = recorder.entered.notified();
        let first = {
            let handler = handler.clone();
            let signed = signed.clone();
            let peer_id = peer_id.clone();
            tokio::spawn(async move { handler.handle_verified_peer_frame(signed, peer_id).await })
        };
        entered.await;
        tokio::time::advance(PEER_INGEST_TIMEOUT + Duration::from_secs(1)).await;
        assert!(matches!(
            first.await.expect("first caller task"),
            Err(PeerDeliveryError::IngestTimeout)
        ));
        assert!(matches!(
            handler
                .handle_verified_peer_frame(signed.clone(), peer_id.clone())
                .await,
            Err(PeerDeliveryError::DuplicateCorrelation(_))
        ));

        recorder.release.notify_one();
        for _ in 0..32 {
            if handler.pending_ingest.lock().await.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(handler.pending_ingest.lock().await.is_empty());
        handler
            .handle_verified_peer_frame(signed, peer_id)
            .await
            .expect("retry succeeds only after the original worker settles");
        assert_eq!(consumer.0.lock().await.len(), 2);
    }

    /// A durable-append failure happens before consumer ingest, emits no false
    /// policy-refusal receipt, and leaves the context/correlation retryable.
    #[tokio::test]
    async fn ac5_journal_failure_prevents_ingest_and_keeps_context_retryable() {
        let consumer = Arc::new(RecordingConsumer(Mutex::new(Vec::new())));
        let (handler, tree, mut events) = handler_with_recorder(
            Arc::new(RelationshipDeliveryPolicy),
            consumer.clone(),
            Arc::new(BrokenRecorder),
        );
        let signed = envelope(MessageKind::PeerMessage, serde_json::json!("hello"));
        let peer_id = signed.signer.peer_id.clone();
        assert!(matches!(
            handler
                .handle_verified_peer_frame(signed.clone(), peer_id.clone())
                .await,
            Err(PeerDeliveryError::Transparency)
        ));
        assert!(
            consumer.0.lock().await.is_empty(),
            "content must not reach the consumer before its acceptance is durable"
        );
        let recipient = fixture_recipient();
        let status = tree
            .status_rx(&recipient)
            .await
            .expect("the peer context remains available for retry");
        assert_ne!(*status.borrow(), NodeState::Cancelled);
        assert!(
            events.try_recv().is_err(),
            "journal failure is not a recipient consent refusal"
        );

        assert!(matches!(
            handler.handle_verified_peer_frame(signed, peer_id).await,
            Err(PeerDeliveryError::Transparency)
        ));
        assert!(
            consumer.0.lock().await.is_empty(),
            "retry must remain durable-first too"
        );
    }
}
