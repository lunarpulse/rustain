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
use crate::domain::ports::AgentMessageBus;
use crate::infrastructure::subagent::{AgentHandle, MailboxBudget, NodeTree};

pub const MAX_PEER_MESSAGE_BYTES: usize = 64 * 1024;
const PEER_INGEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Application-side consumer reached only after cryptographic verification and
/// message-bus admission. Returning `Ok` means the content entered the local
/// recipient context; only then may replay state and a delivery receipt commit.
#[async_trait]
pub trait VerifiedPeerConsumer: Send + Sync {
    async fn ingest(
        &self,
        recipient: &AgentId,
        content: AgentMessage,
        peer_id: &PeerId,
    ) -> Result<(), String>;
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
}

impl VerifiedPeerFrameHandler {
    pub fn new(
        node_tree: NodeTree,
        agent_message_bus: Arc<ArcSwap<Arc<dyn AgentMessageBus>>>,
        domain_tx: mpsc::UnboundedSender<AppEvent>,
        consumer: Arc<dyn VerifiedPeerConsumer>,
    ) -> Self {
        Self {
            node_tree,
            agent_message_bus,
            domain_tx,
            consumer,
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
        let local = translate_verified_peer_envelope(envelope)?;
        self.bind_verified_sender(&local.header.sender, &peer_id)
            .await?;
        let recipient = local.header.recipient.clone();
        self.ensure_peer_context(recipient.clone()).await?;

        let correlation = local.header.correlation_id.0.clone();
        let (ack_tx, ack_rx) = oneshot::channel();
        if self
            .pending_ingest
            .lock()
            .await
            .insert(correlation.clone(), ack_tx)
            .is_some()
        {
            return Err(PeerDeliveryError::DuplicateCorrelation(correlation));
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
            Err(_) => {
                self.pending_ingest.lock().await.remove(&correlation);
                Err(PeerDeliveryError::IngestTimeout)
            }
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
                        let correlation = delivery.envelope.header.correlation_id.0.clone();
                        let peer_id = verified_senders
                            .lock()
                            .await
                            .get(&delivery.envelope.header.sender)
                            .cloned();
                        let result = match peer_id {
                            Some(peer_id) => consumer
                                .ingest(&recipient, delivery.envelope.body, &peer_id)
                                .await
                                .map_err(PeerDeliveryError::Consumer),
                            None => Err(PeerDeliveryError::UnboundSender),
                        };
                        mailbox_budget.release();
                        if result.is_ok() {
                            node_tree.mark_tainted(&recipient).await;
                            let receipt = AppEvent::Subagent(SubagentEnvelope::new(
                                delivery.envelope.header.sender.as_str().to_owned(),
                                recipient.clone(),
                                delivery.envelope.header.kind,
                                SubagentEvent::MessageDelivered {
                                    correlation_id: delivery.envelope.header.correlation_id,
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
        AgentEnvelopeHeader, CorrelationId, Ed25519Sig, MessageKind, PeerIdentity,
    };
    use crate::domain::ports::{DeliveryPolicy, RelationshipDeliveryPolicy};
    use crate::infrastructure::agent_message_bus::LocalMessageBus;

    struct RecordingConsumer(Mutex<Vec<String>>);

    #[async_trait]
    impl VerifiedPeerConsumer for RecordingConsumer {
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
        (
            VerifiedPeerFrameHandler::new(node_tree.clone(), bus_slot, domain_tx, consumer.clone()),
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
}
