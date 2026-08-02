//! Notification-urgency router and journal-rebuilt digest (Story 18.3d).

use std::collections::VecDeque;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::domain::clock::Clock;
use crate::domain::models::{
    AgentId, InteractionPolicySnapshot, JournalEntry, JournalRecord, NotificationUrgency, PeerId,
    RoomEvent,
};
use crate::domain::ports::{RoomJournal, RoomJournalError};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SurfaceInteraction {
    pub(crate) peer: PeerId,
    pub(crate) node: AgentId,
    pub(crate) task: Option<String>,
    pub(crate) text: String,
    pub(crate) notification: NotificationUrgency,
    pub(crate) provenance: InteractionPolicySnapshot,
    pub(crate) recorded_at_ms: i64,
}

impl SurfaceInteraction {
    pub(crate) fn journal_event(&self) -> RoomEvent {
        RoomEvent::PeerInteractionSurfaced {
            peer: Some(self.peer.clone()),
            node: self.node.clone(),
            task: self.task.clone(),
            notification: self.notification,
            provenance: self.provenance.clone(),
        }
    }

    fn replayed(entry: &JournalEntry, event: &RoomEvent) -> Option<Self> {
        let RoomEvent::PeerInteractionSurfaced {
            peer: Some(peer),
            node,
            task,
            notification,
            provenance,
        } = event
        else {
            return None;
        };
        Some(Self {
            peer: peer.clone(),
            node: node.clone(),
            task: task.clone(),
            text: task.as_ref().map_or_else(
                || format!("Peer interaction from {peer}"),
                |task| format!("Peer task {task} from {peer}"),
            ),
            notification: *notification,
            provenance: provenance.clone(),
            recorded_at_ms: entry.recorded_at_ms,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum UrgencyRoute {
    Immediate(SurfaceInteraction),
    Queued,
    Digested,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DigestBatch {
    pub(crate) items: Vec<SurfaceInteraction>,
    pub(crate) flushed_at_ms: i64,
}

impl DigestBatch {
    pub(crate) fn content(&self) -> String {
        let mut content = format!("Team digest — {} interactions", self.items.len());
        for item in &self.items {
            let task = item.task.as_deref().unwrap_or(item.node.as_str());
            content.push_str(&format!("\n- {} · {task}", item.peer));
        }
        content
    }
}

struct UrgencyState {
    queue: VecDeque<SurfaceInteraction>,
    digest: Vec<SurfaceInteraction>,
    last_flush_at_ms: i64,
}

/// One effect shell for immediate, idle-queued, and digest-tier surfacing.
pub(crate) struct UrgencyRouter {
    clock: Arc<dyn Clock>,
    journal: Arc<dyn RoomJournal>,
    digest_interval_ms: i64,
    append_order: Mutex<()>,
    state: Mutex<UrgencyState>,
}

impl UrgencyRouter {
    pub(crate) fn new(
        clock: Arc<dyn Clock>,
        journal: Arc<dyn RoomJournal>,
        entries: &[JournalEntry],
        digest_interval_ms: i64,
    ) -> Self {
        let mut digest = Vec::new();
        let mut last_flush_at_ms = clock.wall_now_ms();
        for entry in entries {
            if let JournalRecord::Room(event) = &entry.record {
                match event {
                    RoomEvent::PeerInteractionSurfaced {
                        notification: NotificationUrgency::Digest,
                        ..
                    } => {
                        if let Some(interaction) = SurfaceInteraction::replayed(entry, event) {
                            digest.push(interaction);
                        }
                    }
                    RoomEvent::PeerDigestFlushed { flushed_at, .. } => {
                        digest.clear();
                        last_flush_at_ms = *flushed_at;
                    }
                    _ => {}
                }
            }
        }
        Self {
            clock,
            journal,
            digest_interval_ms,
            append_order: Mutex::new(()),
            state: Mutex::new(UrgencyState {
                queue: VecDeque::new(),
                digest,
                last_flush_at_ms,
            }),
        }
    }

    /// Journal first, then choose interruption timing. `NoticeLevel` is not
    /// consulted: urgency and severity remain independent quantities.
    pub(crate) async fn route(
        &self,
        mut interaction: SurfaceInteraction,
    ) -> Result<UrgencyRoute, RoomJournalError> {
        interaction.recorded_at_ms = self.clock.wall_now_ms();
        let _append_order = self.append_order.lock().await;
        self.journal
            .record_event(interaction.journal_event())
            .await?;
        match interaction.notification {
            NotificationUrgency::Immediate => Ok(UrgencyRoute::Immediate(interaction)),
            NotificationUrgency::Queue => {
                self.state.lock().await.queue.push_back(interaction);
                Ok(UrgencyRoute::Queued)
            }
            NotificationUrgency::Digest => {
                self.state.lock().await.digest.push(interaction);
                Ok(UrgencyRoute::Digested)
            }
        }
    }

    pub(crate) async fn take_idle_queue(&self) -> Vec<SurfaceInteraction> {
        self.state.lock().await.queue.drain(..).collect()
    }

    pub(crate) async fn pending_digest_count(&self) -> usize {
        self.state.lock().await.digest.len()
    }

    /// Deadline comparison is deliberately the first decision inside the fold:
    /// an empty accumulator cannot short-circuit the clock read.
    pub(crate) async fn flush_due(&self) -> Result<Option<DigestBatch>, RoomJournalError> {
        self.flush(false).await
    }

    pub(crate) async fn flush_pending_on_start(
        &self,
    ) -> Result<Option<DigestBatch>, RoomJournalError> {
        self.flush(true).await
    }

    async fn flush(&self, force: bool) -> Result<Option<DigestBatch>, RoomJournalError> {
        let now = self.clock.wall_now_ms();
        let _append_order = self.append_order.lock().await;
        let count = {
            let state = self.state.lock().await;
            let deadline_elapsed =
                now.saturating_sub(state.last_flush_at_ms) >= self.digest_interval_ms;
            if !force && !deadline_elapsed {
                return Ok(None);
            }
            if state.digest.is_empty() {
                return Ok(None);
            }
            state.digest.len()
        };
        self.journal
            .record_event(RoomEvent::PeerDigestFlushed {
                flushed_at: now,
                count,
            })
            .await?;
        let mut state = self.state.lock().await;
        let items = state.digest.drain(..count).collect();
        state.last_flush_at_ms = now;
        Ok(Some(DigestBatch {
            items,
            flushed_at_ms: now,
        }))
    }
}

/// Periodic adapter shell. The interval only wakes the task; every deadline
/// decision reads the injected [`Clock`] inside [`UrgencyRouter::flush_due`].
pub(crate) async fn run_digest_flusher(
    router: Arc<UrgencyRouter>,
    server: std::sync::Weak<super::server::AttachServer>,
    shutdown: tokio_util::sync::CancellationToken,
) {
    let mut wake = tokio::time::interval(std::time::Duration::from_secs(1));
    wake.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = wake.tick() => {
                match router.flush_due().await {
                    Ok(Some(batch)) => {
                        let Some(server) = server.upgrade() else {
                            break;
                        };
                        if let Err(error) = server.surface_digest_batch(batch).await {
                            tracing::error!(%error, "failed to surface peer digest");
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        tracing::error!(%error, "failed to journal peer digest flush");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::clock::MockClock;
    use crate::domain::models::{
        InteractionPolicySnapshot, JournalEntry, JournalRecord, NotificationUrgency, PeerId,
        RoomEvent,
    };
    use crate::domain::ports::{RoomJournal, RoomJournalError};
    use std::sync::Arc;
    use std::time::Duration;

    #[derive(Default)]
    struct RecordingJournal(tokio::sync::Mutex<Vec<RoomEvent>>);

    #[async_trait::async_trait]
    impl RoomJournal for RecordingJournal {
        async fn record_event(&self, event: RoomEvent) -> Result<(), RoomJournalError> {
            self.0.lock().await.push(event);
            Ok(())
        }
    }

    fn interaction(byte: u8, urgency: NotificationUrgency) -> SurfaceInteraction {
        SurfaceInteraction {
            peer: PeerId::from_public_key(&[byte; 32]).unwrap(),
            node: crate::domain::models::AgentId::from_validated(format!("node-{byte}")),
            task: Some(format!("task-{byte}")),
            text: format!("message-{byte}"),
            notification: urgency,
            provenance: InteractionPolicySnapshot::default(),
            recorded_at_ms: 0,
        }
    }

    #[tokio::test]
    async fn immediate_queue_and_digest_take_three_distinct_routes() {
        let clock = Arc::new(MockClock::at_wall_ms(1_000));
        let journal = Arc::new(RecordingJournal::default());
        let router = UrgencyRouter::new(clock, journal.clone(), &[], 15 * 60_000);

        assert!(matches!(
            router
                .route(interaction(1, NotificationUrgency::Immediate))
                .await
                .unwrap(),
            UrgencyRoute::Immediate(_)
        ));
        assert!(matches!(
            router
                .route(interaction(2, NotificationUrgency::Queue))
                .await
                .unwrap(),
            UrgencyRoute::Queued
        ));
        assert!(matches!(
            router
                .route(interaction(3, NotificationUrgency::Digest))
                .await
                .unwrap(),
            UrgencyRoute::Digested
        ));
        assert_eq!(router.take_idle_queue().await.len(), 1);
        assert_eq!(router.pending_digest_count().await, 1);
        assert_eq!(
            journal.0.lock().await.len(),
            3,
            "every route journals immediately"
        );
    }

    #[tokio::test]
    async fn mock_clock_flush_is_journaled_once_and_restart_rebuilds_without_loss() {
        let clock = Arc::new(MockClock::at_wall_ms(10_000));
        let journal = Arc::new(RecordingJournal::default());
        let router = UrgencyRouter::new(clock.clone(), journal.clone(), &[], 60_000);
        router
            .route(interaction(4, NotificationUrgency::Digest))
            .await
            .unwrap();

        assert!(router.flush_due().await.unwrap().is_none());
        clock.advance(Duration::from_secs(60));
        let batch = router.flush_due().await.unwrap().expect("deadline flush");
        assert_eq!(batch.items.len(), 1);
        assert!(
            router.flush_due().await.unwrap().is_none(),
            "no double flush"
        );

        let entries: Vec<_> = journal
            .0
            .lock()
            .await
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, event)| {
                JournalEntry::new(
                    index as u64 + 1,
                    JournalRecord::Room(event),
                    10_000 + index as i64,
                )
            })
            .collect();
        let restarted = UrgencyRouter::new(clock.clone(), journal.clone(), &entries, 60_000);
        assert_eq!(restarted.pending_digest_count().await, 0);

        let mut after_flush = entries;
        after_flush.push(JournalEntry::new(
            after_flush.len() as u64 + 1,
            JournalRecord::Room(interaction(5, NotificationUrgency::Digest).journal_event()),
            80_000,
        ));
        let restarted = UrgencyRouter::new(clock, journal, &after_flush, 60_000);
        let startup = restarted
            .flush_pending_on_start()
            .await
            .unwrap()
            .expect("startup flushes rebuilt pending digest");
        assert_eq!(startup.items.len(), 1);
    }

    #[tokio::test]
    async fn immediate_arrival_never_flushes_pending_digest() {
        let clock = Arc::new(MockClock::at_wall_ms(5_000));
        let journal = Arc::new(RecordingJournal::default());
        let router = UrgencyRouter::new(clock, journal, &[], 60_000);
        router
            .route(interaction(6, NotificationUrgency::Digest))
            .await
            .unwrap();
        router
            .route(interaction(7, NotificationUrgency::Immediate))
            .await
            .unwrap();
        assert_eq!(router.pending_digest_count().await, 1);
    }
}
