use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

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
use crate::domain::models::PeerId;
use crate::domain::ports::{InboundFrame, PeerTransport, PeerTransportError};
use crate::domain::services::peer_dial::{PeerDialRefusal, PeerDialVerdict};

/// Frames one peer may have waiting behind its own in-flight frame. Deeper only
/// buys latency for a peer whose next frame the replay window would refuse
/// anyway while the current one is pending.
const PEER_QUEUE_DEPTH: usize = 8;

/// Concurrently serviced peers. One stalled recipient must not stop the others
/// (that is the point of the per-peer split), but an unadmitted peer must not be
/// able to mint workers without bound either.
const MAX_PEER_WORKERS: usize = 64;

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
            handler,
            replay: Arc::new(Mutex::new(ReplayWindow::default())),
            workspace,
            now: Arc::new(now),
        }
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

        loop {
            let frame = tokio::select! {
                _ = shutdown.cancelled() => break,
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
        Ok(())
    }

    async fn next_frame(&self) -> Option<InboundFrame> {
        self.inbound.lock().await.recv().await
    }

    async fn process_frame(&self, frame: InboundFrame) -> Result<u64, PeerIngressError> {
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
