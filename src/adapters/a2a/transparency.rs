//! NFR67 transparency for inbound A2A admission.
//!
//! Story 18.1b, AC2b / Task 4; extended by Story 18.2, AC1. Every accept,
//! refuse, pending-approval, cancellation and first-status-query of an inbound
//! task lands on the **canonical** journal/room-event path — durable first, bus
//! second. Story 18.2 builds the `transparency.jsonl` projection and the viewer
//! on top of these events; emitting them here is what makes NFR67
//! protocol-level rather than a UI feature.
//!
//! The record is a *projection of canonical room events, not a second log*.
//! There is deliberately no `transparency.jsonl` write in this module: a second
//! append-only file would be a second source of truth to keep consistent, and
//! the one that drifts is the one nobody is looking at.
//!
//! # Fail-closed (Story 18.2, AC1)
//!
//! [`TransparencySink::record`] returns `Result`. It used to return `()` and
//! swallow the journal error, which made an **accept with no canonical record**
//! reachable in production — the audit log's one job, silently skipped. The
//! rule now:
//!
//! - **accept** → a journal failure is fatal to the request. The caller must
//!   not launch the turn; it refuses instead.
//! - **refusal** → the refusal is still returned to the peer. A failed journal
//!   must never convert a refusal into an accept.
//!
//! The trade is deliberate and costs availability (NFR63): a peer who can
//! exhaust the disk can make this instance refuse all work. Auditability wins,
//! and the operator is told — see the latch below.
//!
//! # Latched failure notice
//!
//! Journal failures arrive in bursts (a full disk fails every append). One
//! notice per failed refusal would bury the transcript in the exact situation
//! where the operator needs to read it. So the sink counts failures and emits
//! [`DomainEventPayload::TransparencyJournalFailed`] carrying the **running
//! total**; the surfacing layer renders it into one persistent row keyed by a
//! stable id, so the count increments in place instead of adding a line.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::Mutex;

use crate::domain::events::{AppEvent, DomainEventPayload};
use crate::domain::models::{AgentId, Direction, JournalRecord, PeerId, RejectReason, RoomEvent};
use crate::domain::ports::{
    EventEmitter, PeerDeliveryOutcome, PeerDeliveryRecord, PeerInteractionRecorder, RoomJournal,
    RoomJournalError, RoomJournalReader,
};
use crate::domain::services::transparency::{
    MAX_PEER_ID_BYTES, TRUNCATION_MARKER, sanitize_disclosable,
};

/// Gate prefix for a task parked on a human decision.
pub const APPROVAL_GATE_PREFIX: &str = "a2a-inbound-approval:";
/// Gate prefix for the first status query observed for a task (FR92, P-5).
pub const STATUS_QUERY_GATE_PREFIX: &str = "a2a-status-query:";

/// What happened to one inbound task, in the vocabulary of admission.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum InboundOutcome {
    /// Admitted and executing as `node`.
    Accepted {
        peer: PeerId,
        node: AgentId,
        task_id: String,
    },
    /// Refused by policy, by a declined approval, or by a failed launch.
    Refused {
        peer: PeerId,
        task_id: String,
        reason: String,
    },
    /// Accepted but parked on a human decision (R10).
    AwaitingApproval { peer: PeerId, task_id: String },
    /// The **first** `tasks/get` observed for a task.
    ///
    /// FR92 names "every incoming A2A message, request, *or status query*".
    /// Journaling every poll floods the log — a 2s poll on a ten-minute task
    /// writes ~300 rows saying "still working" — but journaling none hides
    /// peer reconnaissance, which is precisely what an audit log exists to
    /// catch. First-observation-only is bounded by task count rather than by
    /// time, and closes FR92 instead of descoping it.
    StatusQueried { peer: PeerId, task_id: String },
    /// Result content was handed back to the remote peer.
    Disclosed {
        peer: PeerId,
        node: AgentId,
        task_id: String,
        disclosed_bytes: usize,
    },
}

impl InboundOutcome {
    /// `true` when a journal failure must abort the operation.
    ///
    /// Accepts and disclosures are fail-closed: neither work nor result content
    /// may cross without its canonical record. Refusals and observations still
    /// return when journaling fails because suppressing them would turn a
    /// refusal into an accept.
    #[must_use]
    pub fn is_fail_closed(&self) -> bool {
        matches!(self, Self::Accepted { .. } | Self::Disclosed { .. })
    }
}

/// Durable-first emitter for inbound-admission transparency records.
///
/// `inert()` exists for the discovery-only deployment, which has no journal —
/// and, crucially, also accepts no tasks, so it has nothing to disclose.
pub struct TransparencySink {
    journal: Option<Arc<dyn RoomJournal>>,
    reader: Option<Arc<dyn RoomJournalReader>>,
    notices: Option<Arc<dyn EventEmitter>>,
    /// Running count of journal-append failures. Latched: the surfacing layer
    /// renders this into one persistent row, never one row per failure.
    failures: AtomicU64,
    /// Serializes a counter increment with the matching event emission. The
    /// counter was atomic already, but concurrent callers could publish 2
    /// before 1 and make the operator's latched display regress.
    failure_emit_lock: Mutex<()>,
}

impl TransparencySink {
    #[must_use]
    pub fn new(journal: Arc<dyn RoomJournal>) -> Self {
        Self {
            journal: Some(journal),
            reader: None,
            notices: None,
            failures: AtomicU64::new(0),
            failure_emit_lock: Mutex::new(()),
        }
    }

    /// Route the latched journal-failure condition to the operator.
    ///
    /// Optional because the standalone discovery server has no event bus; a
    /// sink without one still fails closed, it just logs instead of surfacing.
    #[must_use]
    pub fn with_notices(mut self, notices: Arc<dyn EventEmitter>) -> Self {
        self.notices = Some(notices);
        self
    }

    /// Supply the canonical read side used only while recovering the bounded
    /// in-memory task table after a restart.
    #[must_use]
    pub fn with_reader(mut self, reader: Arc<dyn RoomJournalReader>) -> Self {
        self.reader = Some(reader);
        self
    }

    #[must_use]
    pub fn inert() -> Self {
        Self {
            journal: None,
            reader: None,
            notices: None,
            failures: AtomicU64::new(0),
            failure_emit_lock: Mutex::new(()),
        }
    }

    /// How many journal appends this sink has failed to make. Every one of
    /// these is a transparency record that does not exist.
    #[must_use]
    pub fn journal_failures(&self) -> u64 {
        self.failures.load(Ordering::Relaxed)
    }

    /// Whether the durable journal already records this task's first
    /// `tasks/get`. This is called only during startup rehydration; it scans
    /// the canonical stream rather than maintaining another durable or
    /// unbounded task-id index.
    pub async fn has_recorded_status_query(&self, peer: &PeerId, task_id: &str) -> bool {
        let Some(reader) = &self.reader else {
            return false;
        };
        let gate = format!("{STATUS_QUERY_GATE_PREFIX}{}", disclosable_task_id(task_id));
        match reader.load_entries().await {
            Ok(entries) => entries.into_iter().any(|entry| {
                matches!(
                    entry.record,
                    JournalRecord::Room(RoomEvent::AdmissionDeferred {
                        spoke,
                        gate: recorded_gate,
                        ..
                    }) if spoke == peer.as_str() && recorded_gate == gate
                )
            }),
            Err(error) => {
                tracing::warn!(
                    %error,
                    peer = %peer,
                    "cannot recover A2A first-status-query observation from the canonical journal"
                );
                false
            }
        }
    }

    /// Record one admission outcome.
    ///
    /// Ordering is the contract: the `RoomJournal` port appends durably and only
    /// then publishes on the bus. A bus-first emit would surface an event that a
    /// crash could erase, which is exactly the tamper-evidence NFR63 forbids.
    ///
    /// # Errors
    ///
    /// Returns [`RoomJournalError::Append`] when the durable write failed. An
    /// **accept** caller MUST treat this as fatal and refuse; a refusal caller
    /// MUST still return its refusal.
    pub async fn record(&self, outcome: InboundOutcome) -> Result<(), RoomJournalError> {
        let Some(journal) = &self.journal else {
            tracing::debug!(?outcome, "A2A inbound outcome (no journal configured)");
            return Ok(());
        };
        let event = match outcome {
            InboundOutcome::Accepted {
                peer,
                node,
                task_id,
            } => RoomEvent::RemoteEnvelopeAccepted {
                peer,
                node,
                // The admission record's subject is the *decision*, not the
                // payload; the executed turn's own content is journaled by the
                // turn path. Hashing the instruction here would put remote text
                // into the room event twice.
                content_hash: crate::domain::models::ContentHash::from_bytes([0u8; 32]),
                direction: Direction::Inbound,
                task: Some(disclosable_task_id(&task_id)),
            },
            InboundOutcome::Refused {
                peer,
                task_id,
                reason,
            } => RoomEvent::RemoteEnvelopeRejected {
                peer,
                // Strip-on-write (AC8). Inbound refusal reasons are
                // host-authored today, but "audited clean once" is not an
                // invariant — the sanitizer is the invariant.
                reason: RejectReason::Policy {
                    detail: sanitize_disclosable(
                        &reason,
                        crate::domain::services::transparency::MAX_SUMMARY_BYTES,
                    ),
                },
                direction: Direction::Inbound,
                task: Some(disclosable_task_id(&task_id)),
            },
            // `AdmissionDeferred` is the shipped room vocabulary for exactly
            // this: a host-local admission decision held open for observability.
            // A task waiting on a human is the event a transparency log exists
            // for, so it is journaled, not merely logged. The status-query
            // record reuses the same shape under its own gate prefix rather
            // than minting a variant for a record that carries no new fields.
            InboundOutcome::AwaitingApproval { peer, task_id } => RoomEvent::AdmissionDeferred {
                coordinator: AgentId::root(),
                spoke: peer.as_str().to_owned(),
                gate: format!("{APPROVAL_GATE_PREFIX}{}", disclosable_task_id(&task_id)),
            },
            InboundOutcome::StatusQueried { peer, task_id } => RoomEvent::AdmissionDeferred {
                coordinator: AgentId::root(),
                spoke: peer.as_str().to_owned(),
                gate: format!(
                    "{STATUS_QUERY_GATE_PREFIX}{}",
                    disclosable_task_id(&task_id)
                ),
            },
            InboundOutcome::Disclosed {
                peer,
                node,
                task_id,
                disclosed_bytes,
            } => RoomEvent::PeerDisclosure {
                peer: Some(peer),
                node,
                task: Some(disclosable_task_id(&task_id)),
                disclosed_bytes,
            },
        };
        if let Err(error) = journal.record_event(event).await {
            self.latch_failure(&error).await;
            return Err(error);
        }
        Ok(())
    }

    /// Count the failure and tell the operator, without one notice per record.
    async fn latch_failure(&self, error: &RoomJournalError) {
        let _emit_guard = self.failure_emit_lock.lock().await;
        let failures = self.failures.fetch_add(1, Ordering::Relaxed) + 1;
        tracing::error!(
            %error,
            failures,
            "failed to journal A2A transparency record; records are missing from the audit log"
        );
        if let Some(notices) = &self.notices {
            notices.emit(AppEvent::DomainEvent(
                DomainEventPayload::TransparencyJournalFailed {
                    failures,
                    detail: error.to_string(),
                },
            ));
        }
    }
}

#[async_trait::async_trait]
impl PeerInteractionRecorder for TransparencySink {
    async fn record_peer_delivery(&self, record: PeerDeliveryRecord) -> Result<(), String> {
        let PeerDeliveryRecord {
            peer,
            node,
            correlation_id,
            content_bytes,
            outcome,
        } = record;
        let task_id = correlation_id.0;
        let outcome = match outcome {
            PeerDeliveryOutcome::Accepted => InboundOutcome::Accepted {
                peer,
                node,
                task_id,
            },
            PeerDeliveryOutcome::Refused => InboundOutcome::Refused {
                peer,
                task_id,
                reason: format!("peer delivery refused ({content_bytes} bytes)"),
            },
        };
        self.record(outcome)
            .await
            .map_err(|error| error.to_string())
    }
}

/// Sanitize a task id for persistence while keeping the truncation marker
/// inside the field bound. New wire ids are already bounded, but restart
/// recovery must also tolerate malformed legacy pending-task files.
fn disclosable_task_id(task_id: &str) -> String {
    let limit = if task_id.len() > MAX_PEER_ID_BYTES {
        MAX_PEER_ID_BYTES.saturating_sub(TRUNCATION_MARKER.len())
    } else {
        MAX_PEER_ID_BYTES
    };
    sanitize_disclosable(task_id, limit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;

    struct BrokenJournal;

    #[async_trait::async_trait]
    impl RoomJournal for BrokenJournal {
        async fn record_event(&self, _event: RoomEvent) -> Result<(), RoomJournalError> {
            tokio::task::yield_now().await;
            Err(RoomJournalError::Append("disk full".to_owned()))
        }
    }

    #[derive(Default)]
    struct Spy(Mutex<Vec<AppEvent>>);

    impl EventEmitter for Spy {
        fn emit(&self, event: AppEvent) {
            self.0.lock().push(event);
        }
    }

    fn peer() -> PeerId {
        PeerId::from_public_key(&[3u8; 32]).unwrap()
    }

    #[tokio::test]
    async fn a_broken_journal_makes_record_fail_and_latches_a_counted_condition() {
        let spy = Arc::new(Spy::default());
        let sink = TransparencySink::new(Arc::new(BrokenJournal)).with_notices(spy.clone());

        for task_id in ["t-1", "t-2", "t-3", "t-4"] {
            let result = sink
                .record(InboundOutcome::Refused {
                    peer: peer(),
                    task_id: task_id.to_owned(),
                    reason: "policy".to_owned(),
                })
                .await;
            assert!(result.is_err(), "a swallowed journal error is the defect");
        }

        assert_eq!(sink.journal_failures(), 4);
        let events = spy.0.lock();
        assert_eq!(events.len(), 4, "one event per failure carrying the count");
        // The count is what makes this ONE row rather than four: the surfacing
        // layer keys on a stable id and overwrites.
        let counts: Vec<u64> = events
            .iter()
            .filter_map(|event| match event {
                AppEvent::DomainEvent(DomainEventPayload::TransparencyJournalFailed {
                    failures,
                    ..
                }) => Some(*failures),
                _ => None,
            })
            .collect();
        assert_eq!(counts, vec![1, 2, 3, 4]);
    }

    #[test]
    fn accepts_and_disclosures_are_fail_closed() {
        assert!(
            InboundOutcome::Accepted {
                peer: peer(),
                node: AgentId::root(),
                task_id: "t-1".to_owned(),
            }
            .is_fail_closed()
        );
        assert!(
            InboundOutcome::Disclosed {
                peer: peer(),
                node: AgentId::root(),
                task_id: "t-1".to_owned(),
                disclosed_bytes: 1,
            }
            .is_fail_closed()
        );
        assert!(
            !InboundOutcome::Refused {
                peer: peer(),
                task_id: "t-1".to_owned(),
                reason: String::new()
            }
            .is_fail_closed()
        );
        assert!(
            !InboundOutcome::StatusQueried {
                peer: peer(),
                task_id: String::new()
            }
            .is_fail_closed()
        );
    }

    #[tokio::test]
    async fn concurrent_failures_publish_non_decreasing_counts() {
        let spy = Arc::new(Spy::default());
        let sink =
            Arc::new(TransparencySink::new(Arc::new(BrokenJournal)).with_notices(spy.clone()));
        let left = sink.record(InboundOutcome::Refused {
            peer: peer(),
            task_id: "left".to_owned(),
            reason: "policy".to_owned(),
        });
        let right = sink.record(InboundOutcome::Refused {
            peer: peer(),
            task_id: "right".to_owned(),
            reason: "policy".to_owned(),
        });
        let (left, right) = tokio::join!(left, right);
        assert!(left.is_err());
        assert!(right.is_err());
        let counts: Vec<u64> = spy
            .0
            .lock()
            .iter()
            .filter_map(|event| match event {
                AppEvent::DomainEvent(DomainEventPayload::TransparencyJournalFailed {
                    failures,
                    ..
                }) => Some(*failures),
                _ => None,
            })
            .collect();
        assert_eq!(counts, vec![1, 2]);
    }

    #[tokio::test]
    async fn an_inert_sink_succeeds_and_never_latches() {
        let sink = TransparencySink::inert();
        assert!(
            sink.record(InboundOutcome::Refused {
                peer: peer(),
                task_id: "t-1".to_owned(),
                reason: "policy".to_owned(),
            })
            .await
            .is_ok()
        );
        assert_eq!(sink.journal_failures(), 0);
    }
}
