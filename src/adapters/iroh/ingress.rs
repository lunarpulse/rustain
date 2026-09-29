use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tokio::sync::{Mutex, broadcast, mpsc};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use super::IrohPeerTransport;
use crate::adapters::p2p_config::peer_dial_verdict_from_workspace;
use crate::adapters::rap::{
    FrameSettlement, PeerDeliveryError, ReplayReservation, ReplayWindow, VerifiedPeerFrameHandler,
    VerifyError, verify_envelope_reserved,
};
use crate::domain::models::{CorrelationId, FeedPosition, FrameRefusal, FrameReply, PeerId};
use crate::domain::ports::{
    FrameResponder, InboundFrame, PeerInteractionRecorder, PeerTransport, PeerTransportError,
    TransportRefusalRecord,
};
use crate::domain::services::peer_dial::{PeerDialRefusal, PeerDialVerdict};
use crate::domain::services::refusal_quota::{RefusalJournalQuota, RefusalRecordVerdict};

/// Frames one peer may have waiting behind its own in-flight frame. Deeper only
/// buys latency for a peer whose next frame the replay window would refuse
/// anyway while the current one is pending.
const PEER_QUEUE_DEPTH: usize = 8;

/// Concurrently serviced peers. One stalled recipient must not stop the others
/// (that is the point of the per-peer split), but an unadmitted peer must not be
/// able to mint workers without bound either.
const MAX_PEER_WORKERS: usize = 64;

/// How often the suppressed-refusal counter is summarized to the log.
///
/// ⛔ The summary is logged, never journaled: a durable record of suppressed
/// volume is exactly the unbounded durable write the quota exists to prevent.
const REFUSAL_SUMMARY_INTERVAL: Duration = Duration::from_secs(300);

/// Owns the accepted-frame receiver and admits each frame through the shared
/// RAP verifier and verified-peer delivery front door.
///
/// Frames are dispatched to one worker per peer: different peers make progress
/// concurrently, while a single peer's frames stay in arrival order because that
/// peer's worker processes them one at a time. Ordering is not an optimisation
/// here — the replay window chains each sender's frames by `prev_hash`, so a
/// reordered pair would be refused as a fork.
pub struct IrohPeerIngress {
    inbound: Mutex<mpsc::Receiver<InboundFrame>>,
    handler: Arc<VerifiedPeerFrameHandler>,
    replay: Arc<Mutex<ReplayWindow>>,
    /// Where an allowlist refusal is journaled. The same sink the delivery front
    /// door uses, taken from the handler rather than injected a second time: two
    /// recorders is one the composition root can forget to wire.
    recorder: Arc<dyn PeerInteractionRecorder>,
    /// The bound on those durable rows (D16).
    quota: Arc<Mutex<RefusalJournalQuota>>,
    workspace: PathBuf,
    now: Arc<dyn Fn() -> i64 + Send + Sync>,
}

impl std::fmt::Debug for IrohPeerIngress {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IrohPeerIngress")
            .field("workspace", &self.workspace)
            .finish_non_exhaustive()
    }
}

impl IrohPeerIngress {
    pub fn new(
        transport: Arc<IrohPeerTransport>,
        handler: Arc<VerifiedPeerFrameHandler>,
        workspace: PathBuf,
    ) -> Result<Self, PeerTransportError> {
        Self::with_now(transport, handler, workspace, || {
            crate::domain::clock::Clock::wall_now_ms(&crate::domain::clock::SystemClock::default())
        })
    }

    /// Construct an ingress with an injected clock for hermetic verification.
    pub fn with_now(
        transport: Arc<IrohPeerTransport>,
        handler: Arc<VerifiedPeerFrameHandler>,
        workspace: PathBuf,
        now: impl Fn() -> i64 + Send + Sync + 'static,
    ) -> Result<Self, PeerTransportError> {
        Ok(Self::with_inbound(
            transport.inbound()?,
            handler,
            workspace,
            now,
        ))
    }

    /// Compose an ingress over an already-owned frame receiver.
    ///
    /// The transport's whole contribution to this type is that receiver, so this
    /// is the seam a hermetic test uses to drive verification, admission and the
    /// real delivery front door without binding a QUIC endpoint. It is not a
    /// bypass: every frame still travels the same `process_frame` path the
    /// production listener uses.
    pub fn with_inbound(
        inbound: mpsc::Receiver<InboundFrame>,
        handler: Arc<VerifiedPeerFrameHandler>,
        workspace: PathBuf,
        now: impl Fn() -> i64 + Send + Sync + 'static,
    ) -> Self {
        Self {
            inbound: Mutex::new(inbound),
            recorder: handler.recorder(),
            handler,
            replay: Arc::new(Mutex::new(ReplayWindow::default())),
            quota: Arc::new(Mutex::new(RefusalJournalQuota::new())),
            workspace,
            now: Arc::new(now),
        }
    }

    /// Refusals counted in memory instead of journaled, for the bound's own
    /// ratchet. ⛔ Not an operator surface: no shipped copy reports this.
    pub async fn suppressed_refusals(&self) -> u64 {
        self.quota.lock().await.suppressed()
    }

    /// Accept and process one frame, returning its committed feed sequence.
    pub async fn accept_next(&self) -> Result<u64, PeerIngressError> {
        let frame = self.next_frame().await.ok_or(PeerTransportError::Closed)?;
        self.process_frame(frame).await
    }

    /// Dispatch accepted frames to per-peer workers until shutdown or close.
    pub async fn run(self: Arc<Self>, shutdown: CancellationToken) -> Result<(), PeerIngressError> {
        let mut workers: HashMap<PeerId, mpsc::Sender<InboundFrame>> = HashMap::new();
        let mut tasks = JoinSet::new();
        let mut summary = tokio::time::interval(REFUSAL_SUMMARY_INTERVAL);
        summary.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick fires immediately and would summarize nothing.
        summary.tick().await;

        loop {
            let frame = tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = summary.tick() => {
                    self.log_refusal_summary().await;
                    continue;
                }
                frame = self.next_frame() => match frame {
                    Some(frame) => frame,
                    None => break,
                },
            };

            // A worker exits when its peer's sender is dropped; reap those slots
            // before deciding whether the cap has been reached.
            workers.retain(|_, sender| !sender.is_closed());

            let sender = match workers.get(&frame.peer_id) {
                Some(sender) => sender.clone(),
                None => {
                    if workers.len() >= MAX_PEER_WORKERS {
                        tracing::warn!(
                            peer = %frame.peer_id,
                            "peer frame refused: too many peers are already being serviced"
                        );
                        continue;
                    }
                    // ⚑ DF-18-4d-PREADMISSION-WORKER-SLOTS, closed here. A
                    // worker used to be minted **before** `admit_frame` read the
                    // allowlist, so sixty-four unadmitted stranger keys could
                    // pin every slot and lock out every peer the operator
                    // actually pinned — a denial of service that needs no
                    // signature and no admission. The allowlist is consulted
                    // before a slot is *created*, ⛔ never instead of the
                    // per-frame check in `admit_frame`: that one stays, because
                    // a revocation must take effect on an already-open
                    // connection (AC5, 18.4b), and a slot that exists is not a
                    // frame that was admitted.
                    if !self.dialable(&frame.peer_id).await {
                        tracing::warn!(
                            peer = %frame.peer_id,
                            "peer frame refused before a worker slot was created: not admitted"
                        );
                        continue;
                    }
                    let (sender, receiver) = mpsc::channel(PEER_QUEUE_DEPTH);
                    tasks.spawn(peer_worker(Arc::clone(&self), receiver));
                    workers.insert(frame.peer_id.clone(), sender.clone());
                    sender
                }
            };

            if let Err(error) = sender.try_send(frame) {
                tracing::warn!(error = %error, "peer frame refused: peer queue is full");
            }
        }

        // Dropping the senders lets each worker finish the frame it holds and
        // drain what it already accepted, rather than losing it mid-delivery.
        drop(workers);
        while tasks.join_next().await.is_some() {}
        self.log_refusal_summary().await;
        Ok(())
    }

    /// Report suppressed refusal volume to the log. ⛔ Never to the journal.
    async fn log_refusal_summary(&self) {
        if let Some(summary) = self.quota.lock().await.take_summary() {
            tracing::info!(
                suppressed = summary.suppressed,
                forgotten_sources = summary.forgotten_sources,
                tracked_sources = summary.tracked_sources,
                "peer refusals were rate-bounded; the suppressed repeats are counted here only"
            );
        }
    }

    /// Whether the operator's allowlist admits this identity **right now**.
    ///
    /// ⛔ Not a substitute for `admit_frame`'s own read: this one decides
    /// whether a *slot* may be created, and the per-frame read decides whether
    /// a *frame* is admitted. Collapsing the two would make a revocation take
    /// effect only for peers that had no worker yet.
    async fn dialable(&self, peer: &PeerId) -> bool {
        let workspace = self.workspace.clone();
        let presented = peer.clone();
        // Blocking file I/O, off the async worker thread — the same discipline
        // `admit_frame` uses for the same read.
        matches!(
            tokio::task::spawn_blocking(move || peer_dial_verdict_from_workspace(
                &workspace, &presented
            ))
            .await,
            Ok(PeerDialVerdict::Admit)
        )
    }

    async fn next_frame(&self) -> Option<InboundFrame> {
        self.inbound.lock().await.recv().await
    }

    /// Admit one frame, answer its sender, and record the refusal this layer owns.
    ///
    /// Ordering is the contract, in this order and no other:
    ///
    /// 1. decide,
    /// 2. append the receiver's own durable row for an admission refusal,
    /// 3. **then** answer the sender.
    ///
    /// The sender's row and this one are independent records of one event, which
    /// is exactly what a two-host capture cross-checks — so the durable side must
    /// not depend on the remote hearing about it.
    async fn process_frame(&self, mut frame: InboundFrame) -> Result<u64, PeerIngressError> {
        let responder = frame.responder.take();
        let peer_id = frame.peer_id.clone();
        let correlation = frame.envelope.header.correlation_id.clone();
        let outcome = self.admit_frame(frame).await;

        if let Err(error) = &outcome {
            if let Some(detail) = admission_refusal_detail(error) {
                self.journal_refusal(&peer_id, detail, &correlation).await;
            }
        }
        if let Some(responder) = responder {
            responder.answer(self.verdict_for(&peer_id, &outcome).await);
        }
        outcome
    }

    /// The reply this receiver hands back for one outcome.
    ///
    /// ⛔ It is a `FrameReply` — a type with no path to carry: a receiver
    /// observes nothing about how the sender's bytes reached it, and this value
    /// is encoded onto the wire, where no path field exists. The path claim
    /// belongs to the sender, minted beside the answer it received.
    async fn verdict_for(
        &self,
        peer: &PeerId,
        outcome: &Result<u64, PeerIngressError>,
    ) -> FrameReply {
        let Err(error) = outcome else {
            return FrameReply::accepted();
        };
        let refusal = refusal_class(error);
        let reply = FrameReply::refused(refusal);
        if refusal == FrameRefusal::FeedPositionMismatch {
            // The whole point of the acknowledged frame: tell the sender the
            // position this window will accept, so it never has to remember one.
            let expected = self.replay.lock().await.expected_position(peer);
            reply.with_expected(expected)
        } else {
            reply
        }
    }

    /// Append the durable refusal row, within the quota.
    async fn journal_refusal(&self, peer: &PeerId, detail: String, correlation: &CorrelationId) {
        let admitted = {
            let mut quota = self.quota.lock().await;
            quota.admit(peer, (self.now)())
        };
        if admitted != RefusalRecordVerdict::Journal {
            return;
        }
        if let Err(error) = self
            .recorder
            .record_transport_refusal(TransportRefusalRecord {
                peer: peer.clone(),
                detail,
                correlation_id: Some(correlation.clone()),
            })
            .await
        {
            // ⛔ Not fatal, and deliberately so: suppressing the refusal because
            // it could not be journaled would turn a refusal into an admission.
            tracing::error!(
                %error,
                peer = %peer,
                "a peer admission refusal could not be journaled; the frame is still refused"
            );
        }
    }

    async fn admit_frame(&self, frame: InboundFrame) -> Result<u64, PeerIngressError> {
        // Re-read per frame so a removal takes effect on an already-open
        // connection (AC5). The read is blocking file I/O and must not run on
        // the async worker thread.
        let workspace = self.workspace.clone();
        let presented = frame.peer_id.clone();
        let verdict = tokio::task::spawn_blocking(move || {
            peer_dial_verdict_from_workspace(&workspace, &presented)
        })
        .await
        .map_err(|error| PeerTransportError::Shutdown(error.to_string()))?;
        match verdict {
            PeerDialVerdict::Admit => {}
            PeerDialVerdict::Refuse(reason) => return Err(refusal_error(reason).into()),
        }

        if frame.envelope.signer.peer_id != frame.peer_id {
            return Err(PeerTransportError::SignatureInvalid(
                "envelope signer does not match the transport endpoint".to_owned(),
            )
            .into());
        }

        // Subscribe before delivering: a settlement published while nobody is
        // listening would strand the reservation this frame may have to park.
        let settlements = self.handler.subscribe_settlements();
        let correlation = frame.envelope.header.correlation_id.0.clone();

        let reservation = {
            let mut replay = self.replay.lock().await;
            verify_envelope_reserved(&frame.envelope, (self.now)(), &mut replay)
                .map_err(verification_error)?
        };
        let sequence = frame.envelope.header.sequence;

        match self
            .handler
            .handle_verified_peer_frame(frame.envelope, frame.peer_id)
            .await
        {
            Ok(()) => {
                let committed = self.replay.lock().await.commit(reservation);
                debug_assert!(committed, "owned replay reservation must still be pending");
                Ok(sequence)
            }
            Err(error) if settles_later(&error) => {
                // The wait expired but the recipient may already have taken the
                // message and the worker can still succeed. Committing would
                // discard a frame that was never ingested; rolling back would
                // let the same signed frame be delivered twice. Park the
                // reservation instead — this peer's next frame is refused as
                // pending until the worker's own outcome resolves it.
                self.spawn_settlement_resolver(correlation, reservation, settlements);
                Err(PeerIngressError::Delivery(error))
            }
            Err(error) => {
                let rolled_back = self.replay.lock().await.rollback(&reservation);
                debug_assert!(
                    rolled_back,
                    "a frame refused before ingest must release its own replay reservation"
                );
                Err(PeerIngressError::Delivery(error))
            }
        }
    }

    /// Resolve a parked reservation from the delivery worker's real outcome.
    fn spawn_settlement_resolver(
        &self,
        correlation: String,
        reservation: ReplayReservation,
        mut settlements: broadcast::Receiver<FrameSettlement>,
    ) {
        let replay = Arc::clone(&self.replay);
        tokio::spawn(async move {
            let accepted = loop {
                match settlements.recv().await {
                    Ok(settlement) if settlement.correlation_id == correlation => {
                        break Some(settlement.accepted);
                    }
                    Ok(_) => continue,
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        tracing::error!(
                            correlation,
                            missed,
                            "peer frame settlements were dropped; a replay reservation may stay parked"
                        );
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break None,
                }
            };

            match accepted {
                Some(true) => {
                    replay.lock().await.commit(reservation);
                }
                Some(false) => {
                    replay.lock().await.rollback(&reservation);
                }
                None => tracing::warn!(
                    correlation,
                    "the delivery front door closed before this frame settled; \
                     its replay reservation stays parked"
                ),
            }
        });
    }
}

async fn peer_worker(ingress: Arc<IrohPeerIngress>, mut frames: mpsc::Receiver<InboundFrame>) {
    while let Some(frame) = frames.recv().await {
        if let Err(error) = ingress.process_frame(frame).await {
            tracing::warn!(error = %error, "peer frame refused");
        }
    }
}

/// An outcome that says nothing about whether the recipient took the message.
fn settles_later(error: &PeerDeliveryError) -> bool {
    matches!(
        error,
        PeerDeliveryError::IngestTimeout | PeerDeliveryError::IngestChannelClosed
    )
}

/// Keep a feed-position refusal distinguishable from a forged one: a replayed
/// frame carries a valid signature, and the operator action is not the same.
fn verification_error(error: VerifyError) -> PeerTransportError {
    match error {
        VerifyError::Replay { .. }
        | VerifyError::ReplayPending { .. }
        | VerifyError::NonceReplay { .. }
        | VerifyError::FeedForkOrGap { .. } => {
            PeerTransportError::ReplayRejected(error.to_string())
        }
        VerifyError::Expired { .. } => PeerTransportError::FrameExpired(error.to_string()),
        other => PeerTransportError::SignatureInvalid(other.to_string()),
    }
}

/// The class this receiver names to the sender.
///
/// Classes, never prose: the sender renders its own sentence, so nothing this
/// host writes here can end up quoted verbatim in a remote operator's terminal.
fn refusal_class(error: &PeerIngressError) -> FrameRefusal {
    match error {
        PeerIngressError::Transport(error) => match error {
            PeerTransportError::AllowlistAbsent
            | PeerTransportError::AllowlistEmpty
            | PeerTransportError::AllowlistMalformed(_)
            | PeerTransportError::PeerUnpinned(_)
            | PeerTransportError::PeerUnlisted(_) => FrameRefusal::NotAdmitted,
            PeerTransportError::SignatureInvalid(_) => FrameRefusal::SignatureInvalid,
            PeerTransportError::ReplayRejected(_) => FrameRefusal::FeedPositionMismatch,
            PeerTransportError::FrameExpired(_) => FrameRefusal::Expired,
            _ => FrameRefusal::Unavailable,
        },
        PeerIngressError::Delivery(error) => match error {
            PeerDeliveryError::InvalidKind
            | PeerDeliveryError::InvalidBody
            | PeerDeliveryError::BodyTooLarge
            | PeerDeliveryError::IdentifierTooLong
            | PeerDeliveryError::DuplicateCorrelation(_) => FrameRefusal::Malformed,
            PeerDeliveryError::PeerBindingMismatch => FrameRefusal::SignatureInvalid,
            PeerDeliveryError::Declined => FrameRefusal::Declined,
            _ => FrameRefusal::Unavailable,
        },
    }
}

/// The durable detail for a refusal **this layer owns**, or `None` when the
/// refusal belongs to a layer that journals its own.
///
/// The delivery front door already journals acceptance and consent refusals, so
/// only the transport-allowlist arm is missing — and it is the one `peer revoke`
/// needs. ⛔ The detail never implies the envelope was signature-checked: at this
/// point it was not. What is known is that QUIC bound the connection to the key
/// the row names, and that is all the row says.
fn admission_refusal_detail(error: &PeerIngressError) -> Option<String> {
    let PeerIngressError::Transport(error) = error else {
        return None;
    };
    let reason = match error {
        PeerTransportError::AllowlistAbsent => "no peer allowlist on this host",
        PeerTransportError::AllowlistEmpty => "the peer allowlist admits nobody",
        PeerTransportError::AllowlistMalformed(_) => "the peer allowlist did not parse",
        PeerTransportError::PeerUnpinned(_) => "the matching entry has no pinned key",
        PeerTransportError::PeerUnlisted(_) => "this key is not in the peer allowlist",
        _ => return None,
    };
    Some(format!(
        "transport admission refused a frame from the connected key: {reason}"
    ))
}

fn refusal_error(reason: PeerDialRefusal) -> PeerTransportError {
    match reason {
        PeerDialRefusal::ConfigAbsent => PeerTransportError::AllowlistAbsent,
        PeerDialRefusal::EmptyAllowlist => PeerTransportError::AllowlistEmpty,
        PeerDialRefusal::MalformedConfig { reason } => {
            PeerTransportError::AllowlistMalformed(reason)
        }
        PeerDialRefusal::MissingPinnedKey { peer } => PeerTransportError::PeerUnpinned(peer),
        PeerDialRefusal::NotAllowlisted { peer_id } => PeerTransportError::PeerUnlisted(peer_id),
    }
}

#[derive(Debug, Error)]
pub enum PeerIngressError {
    #[error(transparent)]
    Transport(#[from] PeerTransportError),
    #[error(transparent)]
    Delivery(#[from] PeerDeliveryError),
}
