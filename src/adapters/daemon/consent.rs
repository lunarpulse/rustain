//! Pending sender-consent manager (Story 18.3d).

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{Mutex, oneshot};

use crate::adapters::policy::JournalConsentProjection;
use crate::domain::clock::Clock;
use crate::domain::models::{DeliveryDisposition, PeerId, RoomEvent, may_consent_refuse};
use crate::domain::ports::{
    ConsentProjectionQuery, ConsentState, InboundApprovalDecision, RoomJournal,
};

struct PendingSender {
    summaries: Vec<String>,
    waiters: Vec<oneshot::Sender<InboundApprovalDecision>>,
}

/// A ticket plus whether its sender needs a newly rendered card.
pub(crate) struct PendingConsentRegistration {
    pub(crate) pending: bool,
    pub(crate) first_for_sender: bool,
    pub(crate) decision: oneshot::Receiver<InboundApprovalDecision>,
}

/// Groups pending tasks by authenticated sender and owns durable grants.
pub(crate) struct PendingConsentManager {
    projection: Arc<JournalConsentProjection>,
    journal: Arc<dyn RoomJournal>,
    clock: Arc<dyn Clock>,
    pending: Mutex<HashMap<PeerId, PendingSender>>,
}

impl PendingConsentManager {
    pub(crate) fn new(
        projection: Arc<JournalConsentProjection>,
        journal: Arc<dyn RoomJournal>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            projection,
            journal,
            clock,
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// Register a task without starting it. Trusted senders and dispositions
    /// that forbid refusal resolve immediately and never create a card.
    pub(crate) async fn register(
        &self,
        sender: PeerId,
        summary: &str,
        disposition: DeliveryDisposition,
    ) -> PendingConsentRegistration {
        let (decision_tx, decision) = oneshot::channel();
        if !may_consent_refuse(disposition) {
            let _ = decision_tx.send(InboundApprovalDecision::AllowOnce);
            return PendingConsentRegistration {
                pending: false,
                first_for_sender: false,
                decision,
            };
        }
        if self.projection.consent_for(&sender) == ConsentState::Trusted {
            let _ = decision_tx.send(InboundApprovalDecision::AllowAlways);
            return PendingConsentRegistration {
                pending: false,
                first_for_sender: false,
                decision,
            };
        }

        let mut pending = self.pending.lock().await;
        let first_for_sender = !pending.contains_key(&sender);
        let group = pending.entry(sender).or_insert_with(|| PendingSender {
            summaries: Vec::new(),
            waiters: Vec::new(),
        });
        group.summaries.push(summary.chars().take(400).collect());
        group.waiters.push(decision_tx);
        PendingConsentRegistration {
            pending: true,
            first_for_sender,
            decision,
        }
    }

    /// Current card copy for one sender. There is exactly one pending group,
    /// regardless of how many tasks have joined it.
    pub(crate) async fn card_text(&self, sender: &PeerId) -> Option<String> {
        let pending = self.pending.lock().await;
        let count = pending.get(sender)?.waiters.len();
        Some(format!(
            "New teammate: {}\n{count} waiting\n[y] respond once  [n] decline  [a] always trust",
            sender.as_str()
        ))
    }

    /// Resolve the sender's whole waiting group. `AllowAlways` is fail-closed:
    /// waiters are released only after the grant is durably appended.
    pub(crate) async fn resolve(&self, sender: &PeerId, requested: InboundApprovalDecision) {
        let resolved = if requested == InboundApprovalDecision::AllowAlways {
            let event = RoomEvent::ConsentGranted {
                sender: Some(sender.clone()),
                granted_at: self.clock.wall_now_ms(),
            };
            match self.journal.record_event(event.clone()).await {
                Ok(()) => {
                    self.projection.apply(&event);
                    InboundApprovalDecision::AllowAlways
                }
                Err(error) => {
                    tracing::error!(%error, peer = %sender, "durable consent grant failed");
                    InboundApprovalDecision::Decline
                }
            }
        } else {
            requested
        };

        let waiters = self
            .pending
            .lock()
            .await
            .remove(sender)
            .map_or_else(Vec::new, |group| group.waiters);
        for waiter in waiters {
            let _ = waiter.send(resolved);
        }
    }

    #[cfg(test)]
    pub(crate) async fn waiting_count(&self, sender: &PeerId) -> usize {
        self.pending
            .lock()
            .await
            .get(sender)
            .map_or(0, |group| group.waiters.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{DeliveryDisposition, PeerId, RoomEvent};
    use crate::domain::ports::{
        ConsentProjectionQuery, ConsentState, RoomJournal, RoomJournalError,
    };
    use std::sync::Arc;

    #[derive(Default)]
    struct RecordingJournal {
        events: tokio::sync::Mutex<Vec<RoomEvent>>,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl RoomJournal for RecordingJournal {
        async fn record_event(&self, event: RoomEvent) -> Result<(), RoomJournalError> {
            if self.fail {
                return Err(RoomJournalError::Append("injected failure".to_owned()));
            }
            self.events.lock().await.push(event);
            Ok(())
        }
    }

    fn peer(byte: u8) -> PeerId {
        PeerId::from_public_key(&[byte; 32]).unwrap()
    }

    fn manager(
        journal: Arc<RecordingJournal>,
    ) -> (
        Arc<PendingConsentManager>,
        Arc<crate::adapters::policy::JournalConsentProjection>,
    ) {
        let projection = Arc::new(crate::adapters::policy::JournalConsentProjection::default());
        let manager = Arc::new(PendingConsentManager::new(
            projection.clone(),
            journal,
            Arc::new(crate::domain::clock::MockClock::at_wall_ms(42)),
        ));
        (manager, projection)
    }

    #[tokio::test]
    async fn one_sender_gets_one_card_and_yes_releases_only_current_waiters() {
        let journal = Arc::new(RecordingJournal::default());
        let (manager, projection) = manager(journal.clone());
        let sender = peer(1);
        crate::infrastructure::subagent::node_journal::NodeJournal::reset_load_count();

        let first = manager
            .register(sender.clone(), "first task", DeliveryDisposition::MayRefuse)
            .await;
        let second = manager
            .register(
                sender.clone(),
                "second task",
                DeliveryDisposition::MayRefuse,
            )
            .await;
        assert_eq!(
            crate::infrastructure::subagent::node_journal::NodeJournal::load_count(),
            0,
            "sender admission must query the cached projection, never reload the journal"
        );
        assert!(first.first_for_sender);
        assert!(!second.first_for_sender);
        assert_eq!(
            manager.card_text(&sender).await.unwrap(),
            format!(
                "New teammate: {}\n2 waiting\n[y] respond once  [n] decline  [a] always trust",
                sender.as_str()
            )
        );

        manager
            .resolve(&sender, InboundApprovalDecision::AllowOnce)
            .await;
        assert_eq!(
            first.decision.await.unwrap(),
            InboundApprovalDecision::AllowOnce
        );
        assert_eq!(
            second.decision.await.unwrap(),
            InboundApprovalDecision::AllowOnce
        );
        assert_eq!(projection.consent_for(&sender), ConsentState::None);
        assert!(journal.events.lock().await.is_empty());

        let next = manager
            .register(sender, "third task", DeliveryDisposition::MayRefuse)
            .await;
        assert!(next.first_for_sender, "[y] must not create durable trust");
    }

    #[tokio::test]
    async fn always_appends_once_updates_projection_and_bypasses_future_cards() {
        let journal = Arc::new(RecordingJournal::default());
        let (manager, projection) = manager(journal.clone());
        let sender = peer(2);
        let pending = manager
            .register(sender.clone(), "task", DeliveryDisposition::MayRefuse)
            .await;

        manager
            .resolve(&sender, InboundApprovalDecision::AllowAlways)
            .await;
        assert_eq!(
            pending.decision.await.unwrap(),
            InboundApprovalDecision::AllowAlways
        );
        assert_eq!(projection.consent_for(&sender), ConsentState::Trusted);
        assert!(matches!(
            journal.events.lock().await.as_slice(),
            [RoomEvent::ConsentGranted { sender: Some(recorded), granted_at: 42 }] if recorded == &sender
        ));

        let bypass = manager
            .register(sender, "later", DeliveryDisposition::MayRefuse)
            .await;
        assert!(!bypass.pending);
        assert_eq!(
            bypass.decision.await.unwrap(),
            InboundApprovalDecision::AllowAlways
        );
        assert_eq!(
            journal.events.lock().await.len(),
            1,
            "duplicate grant must be idempotent"
        );
    }

    #[tokio::test]
    async fn durable_grant_failure_declines_and_nonrefusable_delivery_bypasses() {
        let journal = Arc::new(RecordingJournal {
            events: tokio::sync::Mutex::new(Vec::new()),
            fail: true,
        });
        let (manager, projection) = manager(journal);
        let sender = peer(3);
        let pending = manager
            .register(sender.clone(), "task", DeliveryDisposition::MayRefuse)
            .await;

        manager
            .resolve(&sender, InboundApprovalDecision::AllowAlways)
            .await;
        assert_eq!(
            pending.decision.await.unwrap(),
            InboundApprovalDecision::Decline
        );
        assert_eq!(projection.consent_for(&sender), ConsentState::None);

        let bypass = manager
            .register(peer(4), "owner report", DeliveryDisposition::MustReport)
            .await;
        assert!(!bypass.pending);
        assert_eq!(
            bypass.decision.await.unwrap(),
            InboundApprovalDecision::AllowOnce
        );
    }
}
