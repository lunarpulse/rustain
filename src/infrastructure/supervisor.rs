//! Host-local orchestration supervisor.
//!
//! Owns admission state and child lifecycle policy. The fork-join executor is
//! the dispatcher and reaches admission through this single seam; it never
//! manipulates semaphore state directly.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use tokio::sync::{Mutex, Notify, OwnedSemaphorePermit, Semaphore};

use crate::domain::clock::Clock;
use crate::domain::models::{
    AgentId, Budget, CapabilityToken, NodeCheckpoint, NodeState, OrchestrationError, RoomEvent,
    WaitReason,
};
use crate::domain::services::authority_ledger::AuthorityLedger;
use crate::infrastructure::runtime::event_bus::EventBus;

/// Churn guard: permits a complete eight-spoke human fan-out plus headroom, but
/// refuses a tight fail/respawn loop before it can self-DoS the host.
pub const SPAWN_RATE_LIMIT: usize = 16;
pub const SPAWN_RATE_WINDOW_MS: i64 = 1_000;

/// RAII admission guard. Dropping it releases the wave-local sub-permit and the
/// shared global running permit exactly once. Queue capacity is held only while
/// `Supervisor::admit` awaits the running permit.
#[derive(Debug)]
pub struct AdmissionPermit {
    _running: OwnedSemaphorePermit,
    _wave_local: Option<OwnedSemaphorePermit>,
    coordinator: AgentId,
    recorded_at: i64,
}

impl AdmissionPermit {
    /// The sliding-window churn timestamp recorded for this admission. A runner
    /// that fails AFTER admission passes this to `refund_failed_spawn` so the
    /// exact reservation is retired — never a blind `pop_back` that (under
    /// concurrency) would erase another spawn's reservation.
    #[must_use]
    pub fn recorded_at(&self) -> i64 {
        self.recorded_at
    }

    #[must_use]
    pub fn coordinator(&self) -> &AgentId {
        &self.coordinator
    }
}

#[derive(Default)]
struct RateState {
    by_coordinator: HashMap<AgentId, VecDeque<i64>>,
}

#[derive(Default)]
struct ReadinessState {
    notifies: HashMap<AgentId, Arc<Notify>>,
    waiting_by_artifact: HashMap<AgentId, HashSet<AgentId>>,
}

/// Park is policy over the existing `Suspended` checkpoint state. It is not a
/// lifecycle variant and carries no transient runtime handle.
pub struct ParkPolicy;

impl ParkPolicy {
    #[must_use]
    pub fn park(mut checkpoint: NodeCheckpoint) -> NodeCheckpoint {
        checkpoint.state = NodeState::Suspended;
        checkpoint.wait_reason = Some(WaitReason::AwaitingUpstreamArtifact);
        checkpoint
    }

    #[must_use]
    pub fn revive(mut checkpoint: NodeCheckpoint) -> NodeCheckpoint {
        checkpoint.wait_reason = None;
        checkpoint
    }
}

/// Single owner of host-local admission state.
pub struct Supervisor {
    /// The REAL global host cap. Every wave and every concurrent run admits
    /// through this one shared instance (never a per-wave fork).
    running: Arc<Semaphore>,
    /// The shared bounded wait-queue: how many acquirers may block before the
    /// supervisor refuses (`Ok(None)`).
    wait_slots: Arc<Semaphore>,
    /// Per-wave concurrency sub-bound. `None` on the composition-root owner;
    /// `Some` on a `for_wave` handle — which still draws from the SHARED
    /// `running`/`wait_slots` above, so the global cap cannot be exceeded by a
    /// second wave or a concurrent run.
    wave_local: Option<Arc<Semaphore>>,
    limit: Mutex<usize>,
    maximum_limit: usize,
    ledger: Arc<AuthorityLedger>,
    root_authority: CapabilityToken,
    clock: Arc<dyn Clock>,
    event_bus: Arc<EventBus>,
    /// Durable room journal — admission refusals are persisted here (R5) so a
    /// deferral survives a restart, not just a live event.
    journal: Option<Arc<crate::infrastructure::subagent::NodeJournal>>,
    rate: Arc<Mutex<RateState>>,
    readiness: Arc<Mutex<ReadinessState>>,
    /// Held recovered-occupancy reservations. Permits are HELD (never
    /// `forget()`-ed) so `release_recovered_occupancy` can return capacity once
    /// the recovered nodes reach terminal — the fix for the permanent shrink.
    /// Permit state itself is never journaled (ruling R3).
    recovered: Arc<Mutex<Vec<OwnedSemaphorePermit>>>,
    #[cfg(any(test, feature = "test-instrumentation"))]
    revive_wakes: Arc<std::sync::atomic::AtomicUsize>,
}

impl Supervisor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        max_running: usize,
        max_waiters: usize,
        ledger: Arc<AuthorityLedger>,
        root_authority: CapabilityToken,
        clock: Arc<dyn Clock>,
        event_bus: Arc<EventBus>,
    ) -> Self {
        let max_running = max_running.max(1);
        Self {
            running: Arc::new(Semaphore::new(max_running)),
            wait_slots: Arc::new(Semaphore::new(max_waiters.max(1))),
            wave_local: None,
            limit: Mutex::new(max_running),
            maximum_limit: max_running,
            ledger,
            root_authority,
            clock,
            event_bus,
            journal: None,
            rate: Arc::new(Mutex::new(RateState::default())),
            readiness: Arc::new(Mutex::new(ReadinessState::default())),
            recovered: Arc::new(Mutex::new(Vec::new())),
            #[cfg(any(test, feature = "test-instrumentation"))]
            revive_wakes: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// Bind the durable room journal so admission refusals are persisted (R5).
    /// Called before the supervisor is wrapped in `Arc` at the composition root.
    #[must_use]
    pub fn with_journal(
        mut self,
        journal: Arc<crate::infrastructure::subagent::NodeJournal>,
    ) -> Self {
        self.journal = Some(journal);
        self
    }

    /// Create a per-wave admission handle. It SHARES the composition-root
    /// `running`/`wait_slots` semaphores (the real global host cap and bounded
    /// queue) and adds a wave-local sub-bound of `requested_running`. Admission
    /// draws a wave-local permit AND a shared global permit, so neither a second
    /// wave nor a concurrent run can push the host past its cap. The root
    /// remains the single owner of churn, budget, clock, journal, and readiness.
    #[must_use]
    pub fn for_wave(&self, requested_running: usize, _max_waiters: usize) -> Self {
        Self {
            running: Arc::clone(&self.running),
            wait_slots: Arc::clone(&self.wait_slots),
            wave_local: Some(Arc::new(Semaphore::new(requested_running.max(1)))),
            limit: Mutex::new(self.maximum_limit),
            maximum_limit: self.maximum_limit,
            ledger: Arc::clone(&self.ledger),
            root_authority: self.root_authority.clone(),
            clock: Arc::clone(&self.clock),
            event_bus: Arc::clone(&self.event_bus),
            journal: self.journal.clone(),
            rate: Arc::clone(&self.rate),
            readiness: Arc::clone(&self.readiness),
            recovered: Arc::clone(&self.recovered),
            #[cfg(any(test, feature = "test-instrumentation"))]
            revive_wakes: Arc::clone(&self.revive_wakes),
        }
    }

    /// Ordered rate → concurrency → budget admission cascade.
    ///
    /// `Ok(None)` is a host-local defer/refusal; it never enters a signed peer
    /// envelope. The rate ticket is recorded ONLY after every gate passes
    /// (ironclaw record-after-success), so a refusal or a cancelled acquire
    /// never spends a churn slot. The wave-local and shared-global running
    /// permits are released by RAII on every exit path (cancel-by-drop).
    pub async fn admit(
        &self,
        coordinator: &AgentId,
        spoke: &str,
        needed: Budget,
    ) -> Result<Option<AdmissionPermit>, OrchestrationError> {
        // Gate 1 (rate): read-only churn check; nothing is recorded yet.
        if !self.rate_within_limit(coordinator).await {
            self.emit_refused(coordinator, spoke, "rate").await;
            return Ok(None);
        }

        // Gate 2 (concurrency): the bounded wait-queue governs how many may
        // block; past the bound, refuse (caller backs off).
        let queue_permit = match Arc::clone(&self.wait_slots).try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                self.emit_refused(coordinator, spoke, "capacity").await;
                return Ok(None);
            }
        };
        // Per-wave sub-bound: dropping this future frees the wave slot.
        let wave_local = match &self.wave_local {
            Some(sem) => Some(
                Arc::clone(sem)
                    .acquire_owned()
                    .await
                    .map_err(|_| OrchestrationError::Internal("wave semaphore closed".into()))?,
            ),
            None => None,
        };
        // Shared global running permit — the real host cap. Dropping the queue
        // permit only after acquiring keeps the queue bound honest.
        let running = Arc::clone(&self.running)
            .acquire_owned()
            .await
            .map_err(|_| OrchestrationError::Internal("supervisor semaphore closed".into()))?;
        drop(queue_permit);

        // Gate 3 (budget): reuse the ledger's conservation head.
        let available = self
            .ledger
            .available(&self.root_authority.id)
            .map_err(|error| OrchestrationError::Internal(error.to_string()))?;
        if available.requests < needed.requests || available.cost_micros < needed.cost_micros {
            return Err(OrchestrationError::BudgetPaused { available, needed });
        }

        // Every gate passed → record the churn ticket exactly once.
        let recorded_at = self.record_rate(coordinator).await;
        Ok(Some(AdmissionPermit {
            _running: running,
            _wave_local: wave_local,
            coordinator: coordinator.clone(),
            recorded_at,
        }))
    }

    /// Read-only churn gate: is the coordinator under the sliding-window cap?
    /// Expired ticks are pruned here; nothing is recorded (record-after-success).
    async fn rate_within_limit(&self, coordinator: &AgentId) -> bool {
        let now = self.clock.wall_now_ms();
        let cutoff = now.saturating_sub(SPAWN_RATE_WINDOW_MS);
        let mut state = self.rate.lock().await;
        let history = state.by_coordinator.entry(coordinator.clone()).or_default();
        while history
            .front()
            .is_some_and(|timestamp| *timestamp <= cutoff)
        {
            history.pop_front();
        }
        history.len() < SPAWN_RATE_LIMIT
    }

    /// Record one churn ticket after a successful admission; returns its
    /// timestamp so a failed spawn can retire this exact reservation.
    async fn record_rate(&self, coordinator: &AgentId) -> i64 {
        let now = self.clock.wall_now_ms();
        let mut state = self.rate.lock().await;
        state
            .by_coordinator
            .entry(coordinator.clone())
            .or_default()
            .push_back(now);
        now
    }

    async fn emit_refused(&self, coordinator: &AgentId, spoke: &str, gate: &'static str) {
        let event = RoomEvent::AdmissionDeferred {
            coordinator: coordinator.clone(),
            spoke: spoke.to_string(),
            gate: gate.to_string(),
        };
        // Durable-first (R5): a refusal is operator-observable across a restart,
        // not just a transient live event.
        if let Some(journal) = &self.journal
            && let Err(error) = journal.append_room(event.clone()).await
        {
            tracing::warn!(%error, "failed to journal admission refusal");
        }
        let _ = self
            .event_bus
            .emit_domain(crate::domain::events::AppEvent::DomainEvent(event.into()));
    }

    /// Retire the exact churn reservation recorded for a spawn that failed
    /// after admission. Removes the matching timestamp — never a blind
    /// `pop_back`, which under concurrency erases another spawn's reservation.
    pub async fn refund_failed_spawn(&self, coordinator: &AgentId, recorded_at: i64) {
        let mut state = self.rate.lock().await;
        if let Some(history) = state.by_coordinator.get_mut(coordinator) {
            if let Some(pos) = history
                .iter()
                .position(|timestamp| *timestamp == recorded_at)
            {
                history.remove(pos);
            }
            if history.is_empty() {
                state.by_coordinator.remove(coordinator);
            }
        }
    }

    pub fn available_running_permits(&self) -> usize {
        self.running.available_permits()
    }

    pub fn available_wait_slots(&self) -> usize {
        self.wait_slots.available_permits()
    }

    /// TEST-ONLY invariant pin. Story 17.2b has no production resize caller;
    /// the future consumer is a budget throttle or configuration knob. The
    /// shared semaphore is resized in place and is never replaced.
    #[cfg(any(test, feature = "test-instrumentation"))]
    pub async fn resize_for_test(&self, new_limit: usize) -> Result<(), OrchestrationError> {
        self.resize(new_limit).await
    }

    /// Register one parked downstream node against durable upstream handles.
    /// The supervisor is the sole writer of this side-state; `NodeState`
    /// remains unchanged.
    pub async fn park_on_dependencies(
        &self,
        node: AgentId,
        producers: impl IntoIterator<Item = AgentId>,
    ) {
        let mut readiness = self.readiness.lock().await;
        readiness
            .notifies
            .entry(node.clone())
            .or_insert_with(|| Arc::new(Notify::new()));
        for producer in producers {
            readiness
                .waiting_by_artifact
                .entry(producer)
                .or_default()
                .insert(node.clone());
        }
    }

    /// Drop readiness registrations for nodes that never reached their wave
    /// (early budget/mint refusal, cancellation, or a mid-wave durable-store
    /// error). Idempotent — a node already revived/cleared is a no-op. Prevents
    /// the composition-root readiness maps from growing without bound across
    /// rejected fan-outs.
    pub async fn clear_parked<'a>(&self, nodes: impl IntoIterator<Item = &'a AgentId>) {
        let mut readiness = self.readiness.lock().await;
        for node in nodes {
            readiness.notifies.remove(node);
            readiness.waiting_by_artifact.retain(|_, waiters| {
                waiters.remove(node);
                !waiters.is_empty()
            });
        }
    }

    /// Push-driven artifact landing. `Notify::notify_one` stores at most one
    /// permit, so concurrent lands for the same node coalesce naturally.
    pub async fn artifact_created(&self, producer: &AgentId) {
        let readiness = self.readiness.lock().await;
        let Some(nodes) = readiness.waiting_by_artifact.get(producer) else {
            return;
        };
        for node in nodes {
            if let Some(notify) = readiness.notifies.get(node) {
                notify.notify_one();
            }
        }
    }

    /// Wait for one coalesced revive or cancellation, then clear every
    /// registration owned by this parked node.
    pub async fn wait_for_artifact(
        &self,
        node: &AgentId,
        cancel: tokio_util::sync::CancellationToken,
    ) -> bool {
        let notify = {
            let readiness = self.readiness.lock().await;
            readiness.notifies.get(node).cloned()
        };
        let Some(notify) = notify else {
            return false;
        };
        let revived = tokio::select! {
            _ = notify.notified() => true,
            _ = cancel.cancelled() => false,
        };
        let mut readiness = self.readiness.lock().await;
        readiness.notifies.remove(node);
        readiness.waiting_by_artifact.retain(|_, nodes| {
            nodes.remove(node);
            !nodes.is_empty()
        });
        #[cfg(any(test, feature = "test-instrumentation"))]
        if revived {
            self.revive_wakes
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        revived
    }

    #[cfg(any(test, feature = "test-instrumentation"))]
    pub fn revive_wake_count_for_test(&self) -> usize {
        self.revive_wakes.load(std::sync::atomic::Ordering::Relaxed)
    }

    async fn resize(&self, new_limit: usize) -> Result<(), OrchestrationError> {
        if new_limit == 0 || new_limit > self.maximum_limit {
            return Err(OrchestrationError::Internal(format!(
                "supervisor limit must be within 1..={}",
                self.maximum_limit
            )));
        }
        let mut limit = self.limit.lock().await;
        match new_limit.cmp(&*limit) {
            std::cmp::Ordering::Greater => self.running.add_permits(new_limit - *limit),
            std::cmp::Ordering::Less => {
                let remove = (*limit - new_limit) as u32;
                Arc::clone(&self.running)
                    .acquire_many_owned(remove)
                    .await
                    .map_err(|_| {
                        OrchestrationError::Internal("supervisor semaphore closed".into())
                    })?
                    .forget();
            }
            std::cmp::Ordering::Equal => {}
        }
        *limit = new_limit;
        Ok(())
    }

    /// Reconstruct transient capacity from the recovered active-node count.
    /// Permits are HELD (not `forget()`-ed) so `release_recovered_occupancy`
    /// can return them once the recovered nodes reach terminal — closing the
    /// permanent-shrink leak. Permit state is never journaled (ruling R3);
    /// composition roots call this with their `Running`/`Waiting` recovery fold.
    pub async fn derive_recovered_occupancy(
        &self,
        recovered_running_or_waiting: usize,
    ) -> Result<(), OrchestrationError> {
        let take = recovered_running_or_waiting.min(self.maximum_limit) as u32;
        if take == 0 {
            return Ok(());
        }
        let permit = Arc::clone(&self.running)
            .acquire_many_owned(take)
            .await
            .map_err(|_| OrchestrationError::Internal("supervisor semaphore closed".into()))?;
        self.recovered.lock().await.push(permit);
        Ok(())
    }

    /// Return recovered-occupancy capacity once the recovered nodes have
    /// reached a terminal state. Drops every held reservation. The lifecycle
    /// caller is the recovered node's terminal transition (a budget-throttle or
    /// the daemon's post-reconcile sweep); until then the derived fill holds.
    pub async fn release_recovered_occupancy(&self) {
        self.recovered.lock().await.clear();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::domain::clock::{Clock, MockClock};
    use crate::domain::models::{AgentId, Budget, CapabilityFlag, CapabilitySet, CapabilityToken};
    use crate::domain::services::authority_ledger::AuthorityLedger;
    use crate::infrastructure::runtime::event_bus::EventBus;

    use super::Supervisor;

    fn supervisor(max_running: usize, max_waiters: usize) -> (Supervisor, Arc<MockClock>, AgentId) {
        let clock = Arc::new(MockClock::at_wall_ms(0));
        let coordinator = AgentId::root();
        let root = CapabilityToken::root(
            coordinator.clone(),
            CapabilitySet::from_flags(&[CapabilityFlag::Spawn]),
            Budget {
                requests: 1_000,
                cost_micros: 1_000_000,
            },
            3,
            None,
            None,
        );
        let ledger = Arc::new(AuthorityLedger::new(root.clone()));
        let supervisor = Supervisor::new(
            max_running,
            max_waiters,
            ledger,
            root,
            Arc::clone(&clock) as Arc<dyn Clock>,
            Arc::new(EventBus::new(32).0),
        );
        (supervisor, clock, coordinator)
    }

    #[tokio::test]
    async fn queued_cancellation_reclaims_wait_slot() {
        let (supervisor, _clock, coordinator) = supervisor(1, 1);
        let supervisor = Arc::new(supervisor);
        let held = supervisor
            .admit(&coordinator, "held", Budget::default())
            .await
            .unwrap()
            .expect("first admission");

        let queued_supervisor = Arc::clone(&supervisor);
        let queued_coordinator = coordinator.clone();
        let queued = tokio::spawn(async move {
            queued_supervisor
                .admit(&queued_coordinator, "queued", Budget::default())
                .await
        });
        tokio::task::yield_now().await;
        assert_eq!(supervisor.available_wait_slots(), 0);
        queued.abort();
        let _ = queued.await;
        assert_eq!(supervisor.available_wait_slots(), 1);

        drop(held);
        assert!(
            supervisor
                .admit(&coordinator, "replacement", Budget::default())
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn held_cancellation_reclaims_running_permit() {
        let (supervisor, _clock, coordinator) = supervisor(1, 1);
        let held = supervisor
            .admit(&coordinator, "held", Budget::default())
            .await
            .unwrap()
            .expect("first admission");
        assert_eq!(supervisor.available_running_permits(), 0);
        drop(held);
        assert_eq!(supervisor.available_running_permits(), 1);
    }

    #[tokio::test]
    async fn whole_fanout_cancel_reclaims_running_and_parked_capacity() {
        let (supervisor, _clock, coordinator) = supervisor(1, 2);
        let supervisor = Arc::new(supervisor);
        let held = supervisor
            .admit(&coordinator, "running", Budget::default())
            .await
            .unwrap()
            .unwrap();
        let mut parked = Vec::new();
        for label in ["parked-a", "parked-b"] {
            let supervisor = Arc::clone(&supervisor);
            let coordinator = coordinator.clone();
            parked.push(tokio::spawn(async move {
                supervisor
                    .admit(&coordinator, label, Budget::default())
                    .await
            }));
        }
        tokio::task::yield_now().await;
        assert_eq!(supervisor.available_wait_slots(), 0);
        for task in parked {
            task.abort();
            let _ = task.await;
        }
        drop(held);
        assert_eq!(supervisor.available_wait_slots(), 2);
        assert_eq!(supervisor.available_running_permits(), 1);
    }

    #[tokio::test]
    async fn queue_overflow_refuses_without_blocking() {
        let (supervisor, _clock, coordinator) = supervisor(1, 1);
        let supervisor = Arc::new(supervisor);
        let _held = supervisor
            .admit(&coordinator, "held", Budget::default())
            .await
            .unwrap()
            .unwrap();
        let queued_supervisor = Arc::clone(&supervisor);
        let queued_coordinator = coordinator.clone();
        let queued = tokio::spawn(async move {
            queued_supervisor
                .admit(&queued_coordinator, "queued", Budget::default())
                .await
        });
        tokio::task::yield_now().await;
        assert!(
            supervisor
                .admit(&coordinator, "refused", Budget::default())
                .await
                .unwrap()
                .is_none()
        );
        queued.abort();
    }

    #[tokio::test]
    async fn hot_respawn_churn_is_rate_refused_but_paced_work_is_not() {
        let (supervisor, clock, coordinator) = supervisor(8, 8);
        for idx in 0..super::SPAWN_RATE_LIMIT {
            let permit = supervisor
                .admit(&coordinator, &format!("hot-{idx}"), Budget::default())
                .await
                .unwrap()
                .expect("within churn limit");
            drop(permit);
        }
        assert!(
            supervisor
                .admit(&coordinator, "hot-refused", Budget::default())
                .await
                .unwrap()
                .is_none()
        );

        clock.set_wall_anchor_ms(super::SPAWN_RATE_WINDOW_MS + 1);
        assert!(
            supervisor
                .admit(&coordinator, "paced", Budget::default())
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn resize_conserves_capacity_on_the_same_semaphore_test_only() {
        let (supervisor, _clock, coordinator) = supervisor(3, 2);
        supervisor.resize_for_test(2).await.unwrap();
        let held = supervisor
            .admit(&coordinator, "held", Budget::default())
            .await
            .unwrap()
            .unwrap();
        supervisor.resize_for_test(3).await.unwrap();
        assert_eq!(supervisor.available_running_permits(), 2);
        supervisor.resize_for_test(1).await.unwrap();
        assert_eq!(supervisor.available_running_permits(), 0);
        drop(held);
        assert_eq!(supervisor.available_running_permits(), 1);
    }

    #[tokio::test]
    async fn recovered_occupancy_is_derived_not_journaled() {
        let (supervisor, _clock, _coordinator) = supervisor(4, 4);
        supervisor.derive_recovered_occupancy(3).await.unwrap();
        assert_eq!(supervisor.available_running_permits(), 1);
    }
    #[tokio::test]
    async fn failed_spawn_refunds_rate_reservation() {
        let (supervisor, _clock, coordinator) = supervisor(1, 1);
        for idx in 0..(super::SPAWN_RATE_LIMIT * 2) {
            let permit = supervisor
                .admit(&coordinator, &format!("failed-{idx}"), Budget::default())
                .await
                .unwrap()
                .expect("failed spawns do not exhaust churn allowance");
            let recorded_at = permit.recorded_at();
            drop(permit);
            supervisor
                .refund_failed_spawn(&coordinator, recorded_at)
                .await;
        }
    }

    #[test]
    fn park_is_suspended_checkpoint_policy_not_a_new_state() {
        let checkpoint = crate::domain::models::AgentNode {
            id: AgentId::parse("parked").unwrap(),
            token: crate::domain::models::CapabilityTokenId::root(),
            parent: Some(AgentId::root()),
            ownership: crate::domain::models::OwnershipKind::Owned,
            state: crate::domain::models::NodeState::Running,
            origin: crate::domain::models::NodeOrigin::Subagent,
            foreground: false,
            effective_model: "test".into(),
            tokens_in: 0,
            tokens_out: 0,
            turns: 0,
            subagent_type: "test".into(),
            spawned_at: 0,
            depth: 1,
            tainted: false,
            waiting_since: None,
        }
        .checkpoint();
        let parked = super::ParkPolicy::park(checkpoint);
        assert_eq!(parked.state, crate::domain::models::NodeState::Suspended);
        assert_eq!(
            parked.wait_reason,
            Some(crate::domain::models::WaitReason::AwaitingUpstreamArtifact)
        );
        let revived = super::ParkPolicy::revive(parked);
        assert_eq!(revived.state, crate::domain::models::NodeState::Suspended);
        assert_eq!(revived.wait_reason, None);
    }

    #[tokio::test]
    async fn concurrent_artifact_revives_coalesce_to_one_wake() {
        let (supervisor, _clock, _coordinator) = supervisor(2, 2);
        let supervisor = Arc::new(supervisor);
        let node = AgentId::parse("downstream").unwrap();
        let producer = AgentId::parse("upstream").unwrap();
        supervisor
            .park_on_dependencies(node.clone(), [producer.clone()])
            .await;

        let waiter_supervisor = Arc::clone(&supervisor);
        let waiter_node = node.clone();
        let waiter = tokio::spawn(async move {
            waiter_supervisor
                .wait_for_artifact(&waiter_node, tokio_util::sync::CancellationToken::new())
                .await
        });
        tokio::task::yield_now().await;

        let mut lands = Vec::new();
        for _ in 0..32 {
            let supervisor = Arc::clone(&supervisor);
            let producer = producer.clone();
            lands.push(tokio::spawn(async move {
                supervisor.artifact_created(&producer).await;
            }));
        }
        for land in lands {
            land.await.unwrap();
        }
        assert!(waiter.await.unwrap());
        assert_eq!(supervisor.revive_wake_count_for_test(), 1);
    }
}
