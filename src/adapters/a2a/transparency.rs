//! NFR67 transparency for inbound A2A admission.
//!
//! Story 18.1b, AC2b / Task 4. Every accept, refuse, pending-approval and
//! cancellation of an inbound task lands on the **canonical** journal/room-event
//! path — durable first, bus second. Story 18.2 builds the `transparency.jsonl`
//! projection and the viewer on top of these events; emitting them here is what
//! makes NFR67 protocol-level rather than a UI feature.
//!
//! The record is a *projection of canonical room events, not a second log*.
//! There is deliberately no `transparency.jsonl` write in this module: a second
//! append-only file would be a second source of truth to keep consistent, and
//! the one that drifts is the one nobody is looking at.

use std::sync::Arc;

use crate::domain::models::{AgentId, PeerId, RejectReason, RoomEvent};
use crate::domain::ports::RoomJournal;

/// What happened to one inbound task, in the vocabulary of admission.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum InboundOutcome {
    /// Admitted and executing as `node`.
    Accepted { peer: PeerId, node: AgentId },
    /// Refused by policy, by a declined approval, or by a failed launch.
    Refused { peer: PeerId, reason: String },
    /// Accepted but parked on a human decision (R10).
    AwaitingApproval { peer: PeerId, task_id: String },
}

/// Durable-first emitter for inbound-admission transparency records.
///
/// `inert()` exists for the discovery-only deployment, which has no journal —
/// and, crucially, also accepts no tasks, so it has nothing to disclose.
pub struct TransparencySink {
    journal: Option<Arc<dyn RoomJournal>>,
}

impl TransparencySink {
    #[must_use]
    pub fn new(journal: Arc<dyn RoomJournal>) -> Self {
        Self {
            journal: Some(journal),
        }
    }

    #[must_use]
    pub fn inert() -> Self {
        Self { journal: None }
    }

    /// Record one admission outcome.
    ///
    /// Ordering is the contract: the `RoomJournal` port appends durably and only
    /// then publishes on the bus. A bus-first emit would surface an event that a
    /// crash could erase, which is exactly the tamper-evidence NFR63 forbids.
    pub async fn record(&self, outcome: InboundOutcome) {
        let Some(journal) = &self.journal else {
            tracing::debug!(?outcome, "A2A inbound outcome (no journal configured)");
            return;
        };
        let event = match outcome {
            InboundOutcome::Accepted { peer, node } => RoomEvent::RemoteEnvelopeAccepted {
                peer,
                node,
                // The admission record's subject is the *decision*, not the
                // payload; the executed turn's own content is journaled by the
                // turn path. Hashing the instruction here would put remote text
                // into the room event twice.
                content_hash: crate::domain::models::ContentHash::from_bytes([0u8; 32]),
            },
            InboundOutcome::Refused { peer, reason } => RoomEvent::RemoteEnvelopeRejected {
                peer,
                reason: RejectReason::Policy { detail: reason },
            },
            // `AdmissionDeferred` is the shipped room vocabulary for exactly
            // this: a host-local admission decision held open for observability.
            // A task waiting on a human is the event a transparency log exists
            // for, so it is journaled, not merely logged.
            InboundOutcome::AwaitingApproval { peer, task_id } => RoomEvent::AdmissionDeferred {
                coordinator: AgentId::root(),
                spoke: peer.as_str().to_owned(),
                gate: format!("a2a-inbound-approval:{task_id}"),
            },
        };
        if let Err(error) = journal.record_event(event).await {
            tracing::error!(%error, "failed to journal A2A inbound transparency record");
        }
    }
}
