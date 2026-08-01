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
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

use crate::domain::events::AppEvent;
use crate::domain::models::{
    AgentEnvelope, AgentId, AgentMessage, AgentMetrics, CapabilityTokenId, Envelope, MessageHeader,
    MessageKind, NodeState, PeerId, SubagentEnvelope, SubagentEvent,
};
use crate::domain::ports::{
    AgentMessageBus, PeerDeliveryOutcome, PeerDeliveryRecord, PeerInteractionRecorder,
};
use crate::domain::services::transparency::MAX_PEER_ID_BYTES;
use crate::infrastructure::subagent::{AgentHandle, MailboxBudget, NodeTree};

pub const MAX_PEER_MESSAGE_BYTES: usize = 64 * 1024;
const PEER_INGEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Recipient consent decision, separate from operational consumer failures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerifiedPeerConsent {
    Accept,
    Decline,
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
    verified_senders: Arc<Mutex<HashMap<AgentId, PeerId>>>,
    pending_ingest: Arc<Mutex<HashMap<String, oneshot::Sender<Result<(), PeerDeliveryError>>>>>,
    recorder: Arc<dyn PeerInteractionRecorder>,
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
            verified_senders: Arc::new(Mutex::new(HashMap::new())),
            pending_ingest: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn handle_verified_peer_frame(
        &self,
        envelope: AgentEnvelope<serde_json::Value>,
        peer_id: PeerId,
    ) -> Result<(), PeerDeliveryError> {
        let mut local = translate_verified_peer_envelope(envelope)?;
        self.bind_verified_sender(&local.header.sender, &peer_id)
            .await?;
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

    async fn bind_verified_sender(
        &self,
        sender: &AgentId,
        peer_id: &PeerId,
    ) -> Result<(), PeerDeliveryError> {
        let mut senders = self.verified_senders.lock().await;
        match senders.get(sender) {
            Some(bound) if bound != peer_id => Err(PeerDeliveryError::PeerBindingMismatch),
            Some(_) => Ok(()),
            None => {
                senders.insert(sender.clone(), peer_id.clone());
                Ok(())
            }
        }
    }

    async fn ensure_peer_context(&self, recipient: AgentId) -> Result<(), PeerDeliveryError> {
        let mut materialized = self.materialized.lock().await;
        if materialized.contains(&recipient) {
            if self.node_tree.delivery_target(&recipient).await.is_some() {
                return Ok(());
            }
            materialized.remove(&recipient);
        }

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
                        let peer_id = verified_senders.lock().await.get(&header.sender).cloned();

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
                                if let Some(ack) = pending_ingest.lock().await.remove(&correlation)
                                {
                                    let _ = ack.send(Err(PeerDeliveryError::EventChannelClosed));
                                }
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
                                if let Some(ack) = pending_ingest.lock().await.remove(&correlation)
                                {
                                    let _ = ack.send(Err(PeerDeliveryError::EventChannelClosed));
                                }
                                continue;
                            }
                        }
                        if let Some(ack) = pending_ingest.lock().await.remove(&correlation) {
                            let _ = ack.send(result);
                        }
                    }
                    _ => {}
                }
            }

            while let Ok(op) = command_rx.try_recv() {
                if let crate::domain::models::Op::Deliver(delivery) = op {
                    mailbox_budget.release();
                    let correlation = delivery.envelope.header.correlation_id.0.clone();
                    if let Some(ack) = pending_ingest.lock().await.remove(&correlation) {
                        let _ = ack.send(Err(PeerDeliveryError::ContextClosed));
                    }
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
    }

    fn peer() -> PeerIdentity {
        PeerIdentity::from_public_key(vec![7; 32]).expect("test peer identity")
    }

    fn envelope(kind: MessageKind, body: serde_json::Value) -> AgentEnvelope<serde_json::Value> {
        AgentEnvelope::new(
            AgentEnvelopeHeader {
                sender: AgentId::from_validated("peer-agent"),
                recipient: AgentId::from_validated("local-peer-session"),
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
        assert!(
            node_tree
                .is_tainted(&AgentId::from_validated("local-peer-session"))
                .await
        );
        handler.clear_all_taint().await;
        assert!(
            !node_tree
                .is_tainted(&AgentId::from_validated("local-peer-session"))
                .await,
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
        let id = AgentId::from_validated("local-peer-session");
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
        ) -> crate::domain::ports::PeerResponsePolicy {
            crate::domain::ports::PeerResponsePolicy {
                mode: self.0,
                auto_response: None,
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
            .delivery_target(&AgentId::from_validated("local-peer-session"))
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
        let recipient = AgentId::from_validated("local-peer-session");
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
