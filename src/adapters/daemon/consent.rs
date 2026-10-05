//! Pending sender-consent manager (Story 18.3d).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::{Mutex, oneshot};

use crate::adapters::policy::JournalConsentProjection;
use crate::domain::clock::Clock;
use crate::domain::models::{
    DeliveryDisposition, EffectivePolicy, PeerId, PolicyPrincipal, RequestId, RoomEvent,
    may_consent_refuse,
};
use crate::domain::ports::{
    ConsentProjectionQuery, ConsentSlot, ConsentState, InboundApprovalDecision, RoomJournal,
    RoomJournalReader,
};

struct PendingSender {
    /// The group's first registrant: its card copy is the group's (Story
    /// 19.18d `P21`, disclosed); a joiner's summary is pooled, not shown.
    first: PolicyPrincipal,
    summaries: Vec<String>,
    waiters: Vec<oneshot::Sender<InboundApprovalDecision>>,
}

/// A ticket plus whether its sender needs a newly rendered card.
pub(crate) struct PendingConsentRegistration {
    pub(crate) pending: bool,
    pub(crate) first_for_sender: bool,
    pub(crate) decision: oneshot::Receiver<InboundApprovalDecision>,
}

/// Story 19.18d `P21` — a pending group is one consent **cell**: the slot a
/// grant for this principal writes, plus its peer. A RAP sender and the bound
/// A2A callers of one pin share a cell, so they share one card and one answer;
/// a pseudonym and a pin with equal bytes stay two cells.
fn pending_key(principal: &PolicyPrincipal) -> (ConsentSlot, PeerId) {
    (ConsentSlot::of(principal), principal.peer().clone())
}

/// Groups pending tasks by consent cell and owns durable grants.
pub(crate) struct PendingConsentManager {
    projection: Arc<JournalConsentProjection>,
    journal: Arc<dyn RoomJournal>,
    clock: Arc<dyn Clock>,
    reader: Option<Arc<dyn RoomJournalReader>>,
    last_seq: AtomicU64,
    policy: tokio::sync::Mutex<Option<Arc<EffectivePolicy>>>,
    pending: Mutex<HashMap<(ConsentSlot, PeerId), PendingSender>>,
    always_requests: Mutex<HashSet<RequestId>>,
}

impl PendingConsentManager {
    pub(crate) fn new(
        projection: Arc<JournalConsentProjection>,
        journal: Arc<dyn RoomJournal>,
        clock: Arc<dyn Clock>,
        reader: Option<Arc<dyn RoomJournalReader>>,
    ) -> Self {
        Self {
            projection,
            journal,
            clock,
            reader,
            last_seq: AtomicU64::new(0),
            policy: tokio::sync::Mutex::new(None),
            pending: Mutex::new(HashMap::new()),
            always_requests: Mutex::new(HashSet::new()),
        }
    }

    /// Register a task without starting it. Trusted senders and dispositions
    /// that forbid refusal resolve immediately and never create a card.
    pub(crate) async fn register(
        &self,
        sender: PolicyPrincipal,
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
        // D1: refresh the cached fold from the durable journal before deciding,
        // so a `/team untrust` issued from another client takes effect live.
        self.refresh_projection().await;
        // F2: a TOML `[interaction.overrides]` entry implies consent (the
        // operator authored it). Checked before the journaled projection per
        // the AC1 precedence rule; only an applicable (pinned, matching)
        // override implies consent.
        if self.toml_implies_consent(&sender).await {
            let _ = decision_tx.send(InboundApprovalDecision::AllowAlways);
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
        let key = pending_key(&sender);
        let first_for_sender = !pending.contains_key(&key);
        let group = pending.entry(key).or_insert_with(|| PendingSender {
            first: sender,
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

    /// Current card copy for one sender's consent cell. There is exactly one
    /// pending group per cell, regardless of how many tasks have joined it;
    /// its copy is the first registrant's (Story 19.18d `P21`/`P22`).
    pub(crate) async fn card_text(&self, sender: &PolicyPrincipal) -> Option<String> {
        let pending = self.pending.lock().await;
        let group = pending.get(&pending_key(sender))?;
        Some(crate::infrastructure::runtime::transparency_bridge::consent_card_text(&group.first))
    }

    /// Persist standing trust before the approval runtime is resolved.
    ///
    /// The request marker tells the detached watcher that the subsequent
    /// `ApprovalOutcome::Once` came from `[a]`, not `[y]`.
    pub(crate) async fn grant_always(
        &self,
        request_id: RequestId,
        sender: &PolicyPrincipal,
    ) -> bool {
        if !self.persist_grant(sender).await {
            return false;
        }
        self.always_requests.lock().await.insert(request_id);
        true
    }

    pub(crate) async fn take_granted(&self, request_id: &RequestId) -> bool {
        self.always_requests.lock().await.remove(request_id)
    }

    /// Drop a sender's pending group without resolving it. Used when admission
    /// setup failed before a card could render, so the next delivery retries.
    pub(crate) async fn drop_pending(&self, sender: &PolicyPrincipal) {
        self.pending.lock().await.remove(&pending_key(sender));
    }

    /// Re-fold the cached projection from the durable journal when it has
    /// advanced (D1). Idempotent and best-effort: a read error leaves the cached
    /// snapshot in place. The sync query contract is unchanged.
    async fn refresh_projection(&self) {
        let Some(reader) = &self.reader else {
            return;
        };
        let entries = match reader.load_entries().await {
            Ok(entries) => entries,
            Err(error) => {
                tracing::warn!(
                    %error,
                    "consent projection refresh failed; proceeding with cached state"
                );
                return;
            }
        };
        let max_seq = entries.last().map(|entry| entry.seq).unwrap_or(0);
        if max_seq > self.last_seq.load(Ordering::Acquire) {
            self.projection.replace_from(&entries);
            self.last_seq.store(max_seq, Ordering::Release);
        }
    }

    /// Install the resolved effective policy so the gate can honour TOML
    /// overrides as implied consent (F2). Called once after composition.
    pub(crate) async fn set_effective_policy(&self, policy: Arc<EffectivePolicy>) {
        *self.policy.lock().await = Some(policy);
    }

    /// `true` when an applicable (pinned, matching) TOML override implies
    /// consent for `sender`. Unpinned/mismatched overrides do not apply.
    async fn toml_implies_consent(&self, sender: &PolicyPrincipal) -> bool {
        let guard = self.policy.lock().await;
        let Some(policy) = guard.as_ref() else {
            return false;
        };
        crate::domain::services::team_policy::sender_policy_for(
            policy,
            sender,
            crate::domain::models::SemanticMessageType::Unknown,
        )
        .is_some()
    }

    /// Resolve the sender's whole waiting group. `AllowAlways` is fail-closed:
    /// waiters are released only after the grant is durably appended.
    pub(crate) async fn resolve(
        &self,
        sender: &PolicyPrincipal,
        requested: InboundApprovalDecision,
    ) {
        let resolved = if requested == InboundApprovalDecision::AllowAlways
            && self.projection.consent_for(sender) != ConsentState::Trusted
        {
            if self.persist_grant(sender).await {
                InboundApprovalDecision::AllowAlways
            } else {
                InboundApprovalDecision::Decline
            }
        } else {
            requested
        };

        let waiters = self
            .pending
            .lock()
            .await
            .remove(&pending_key(sender))
            .map_or_else(Vec::new, |group| group.waiters);
        for waiter in waiters {
            let _ = waiter.send(resolved);
        }
    }

    async fn persist_grant(&self, sender: &PolicyPrincipal) -> bool {
        if self.projection.consent_for(sender) == ConsentState::Trusted {
            return true;
        }
        let event = RoomEvent::ConsentGranted {
            sender: Some(sender.peer().clone()),
            granted_at: self.clock.wall_now_ms(),
            principal: Some(sender.kind()),
        };
        match self.journal.record_event(event.clone()).await {
            Ok(()) => {
                self.projection.apply(&event);
                true
            }
            Err(error) => {
                tracing::error!(%error, peer = %sender.peer(), "durable consent grant failed");
                false
            }
        }
    }

    #[cfg(test)]
    pub(crate) async fn waiting_count(&self, sender: &PolicyPrincipal) -> usize {
        self.pending
            .lock()
            .await
            .get(&pending_key(sender))
            .map_or(0, |group| group.waiters.len())
    }
}

#[cfg(all(test, feature = "a2a"))]
pub(super) fn loopback_collision_policy(
    workspace: &std::path::Path,
) -> (EffectivePolicy, crate::domain::models::PeerId) {
    let dir = workspace.join(".rustain");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(crate::domain::models::INDIVIDUAL_POLICY_FILE),
        "[interaction.defaults]\nresponse_mode = \"notify-and-wait\"\nnotification = \"queue\"\n\
         [interaction.overrides.loopback-pin]\n\
         peer_id = \"122004a153777cbc690bd5eed4ac78fb5b0fa74428bb8a98ee62fea8172be68acc0b\"\n\
         response_mode = \"notify-and-auto\"\nnotification = \"immediate\"\nauto_response = \"pinned reply\"\n",
    ).unwrap();
    std::fs::write(
        dir.join("a2a.json"),
        r#"{"agents":{"loopback-pin":{"url":"https://peer.example/a2a","pinnedKey":{"alg":"EdDSA","x":"ZO2qP7kxDpjNsYPN2_FW2ZZKBcAX-n-O48JikJ-jZ1k"}}}}"#,
    ).unwrap();
    let peers = crate::adapters::a2a::config::parse_workspace_a2a_config(
        &crate::infrastructure::paths::workspace_a2a_config_path(workspace),
    )
    .unwrap();
    let peer = crate::adapters::a2a::exec::SubmitterKey::loopback().pseudonymous_peer_id();
    assert_eq!(peers[0].pinned_identity(), Some(peer.clone()));
    let (policy, _) = crate::adapters::policy::resolve_workspace_policy(
        workspace,
        &peers,
        &crate::adapters::policy::EmptyConsentProjection,
    )
    .unwrap();
    (policy, peer)
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
            None,
        ));
        (manager, projection)
    }

    #[tokio::test]
    async fn one_sender_gets_one_card_and_yes_releases_only_current_waiters() {
        let journal = Arc::new(RecordingJournal::default());
        let (manager, projection) = manager(journal.clone());
        let sender = PolicyPrincipal::rap(peer(1));
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
        let sender = PolicyPrincipal::rap(peer(2));
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
            [RoomEvent::ConsentGranted { sender: Some(recorded), granted_at: 42, .. }] if recorded == sender.peer()
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
        let sender = PolicyPrincipal::rap(peer(3));
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
            .register(
                PolicyPrincipal::rap(peer(4)),
                "owner report",
                DeliveryDisposition::MustReport,
            )
            .await;
        assert!(!bypass.pending);
        assert_eq!(
            bypass.decision.await.unwrap(),
            InboundApprovalDecision::AllowOnce
        );
    }

    #[tokio::test]
    async fn k01_consent_slots_and_manager_decision_table() {
        use crate::domain::models::{JournalEntry, JournalRecord, PrincipalKind};
        use crate::domain::ports::ConsentSlot;
        use ConsentState::{None as Absent, Revoked, Trusted};

        let sender = peer(41);
        let pinned = PolicyPrincipal::rap(sender.clone());
        let pseudonymous = PolicyPrincipal::Pseudonymous(sender.clone());
        let slots = [
            ConsentSlot::Pinned,
            ConsentSlot::Pseudonymous,
            ConsentSlot::Legacy,
        ];
        let kinds = [
            Some(PrincipalKind::Pinned),
            Some(PrincipalKind::Pseudonymous),
            None,
        ];
        let states = [Absent, Trusted, Revoked];
        // Rows are typed state, columns legacy state: explicit precedence oracle.
        let decisions = [
            [Absent, Trusted, Revoked],
            [Trusted, Trusted, Trusted],
            [Revoked, Revoked, Revoked],
        ];
        let grant = |principal| RoomEvent::ConsentGranted {
            sender: Some(sender.clone()),
            granted_at: 42,
            principal,
        };
        let revoke = || RoomEvent::ConsentRevoked {
            sender: Some(sender.clone()),
            revoked_at: 43,
        };
        let project = |events: Vec<RoomEvent>| {
            let entries: Vec<_> = events
                .into_iter()
                .enumerate()
                .map(|(index, event)| {
                    JournalEntry::new(index as u64 + 1, JournalRecord::Room(event), 44)
                })
                .collect();
            JournalConsentProjection::from_entries(&entries)
        };

        for pin in 0..3 {
            for pseudo in 0..3 {
                for legacy in 0..3 {
                    let wanted = [states[pin], states[pseudo], states[legacy]];
                    let mut events = Vec::new();
                    for (index, state) in wanted.iter().enumerate() {
                        if *state != Absent {
                            events.push(grant(kinds[index]));
                        }
                    }
                    if wanted.contains(&Revoked) {
                        events.push(revoke());
                        for (index, state) in wanted.iter().enumerate() {
                            if *state == Trusted {
                                events.push(grant(kinds[index]));
                            }
                        }
                    }
                    let projection = Arc::new(project(events));
                    let known: Vec<_> = slots
                        .iter()
                        .zip(wanted)
                        .filter(|(_, state)| *state != Absent)
                        .map(|(slot, _)| (*slot, sender.clone()))
                        .collect();
                    assert_eq!(projection.known_senders(), known, "{wanted:?}");
                    for (slot, state) in slots.iter().zip(wanted) {
                        assert_eq!(projection.slot_state(*slot, &sender), state, "{wanted:?}");
                    }
                    let manager = PendingConsentManager::new(
                        projection,
                        Arc::new(RecordingJournal::default()),
                        Arc::new(crate::domain::clock::MockClock::at_wall_ms(42)),
                        None,
                    );
                    for (principal, expected) in [
                        (&pinned, decisions[pin][legacy]),
                        (&pseudonymous, decisions[pseudo][legacy]),
                    ] {
                        assert_eq!(
                            manager.projection.consent_for(principal),
                            expected,
                            "{wanted:?}"
                        );
                        let registration = manager
                            .register(
                                principal.clone(),
                                "collision task",
                                DeliveryDisposition::MayRefuse,
                            )
                            .await;
                        assert_eq!(registration.pending, expected != Trusted, "{wanted:?}");
                        if expected == Trusted {
                            assert_eq!(
                                registration.decision.await.unwrap(),
                                InboundApprovalDecision::AllowAlways
                            );
                        } else {
                            assert!(registration.first_for_sender);
                            manager
                                .resolve(principal, InboundApprovalDecision::AllowOnce)
                                .await;
                            assert_eq!(
                                registration.decision.await.unwrap(),
                                InboundApprovalDecision::AllowOnce
                            );
                        }
                    }
                }
            }
        }

        let unknown = project(vec![grant(Some(PrincipalKind::Unknown))]);
        assert_eq!(
            unknown.known_senders(),
            vec![(ConsentSlot::Legacy, sender.clone())]
        );
        assert_eq!(unknown.slot_state(ConsentSlot::Pinned, &sender), Absent);
        assert_eq!(
            unknown.slot_state(ConsentSlot::Pseudonymous, &sender),
            Absent
        );
        assert_eq!(unknown.consent_for(&pinned), Trusted);
        assert_eq!(unknown.consent_for(&pseudonymous), Trusted);

        let never_granted = project(vec![
            RoomEvent::ConsentGranted {
                sender: None,
                granted_at: 42,
                principal: Some(PrincipalKind::Pinned),
            },
            revoke(),
        ]);
        assert!(never_granted.known_senders().is_empty());
        for slot in slots {
            assert_eq!(never_granted.slot_state(slot, &sender), Absent);
        }
        let legacy_only = project(vec![grant(None), revoke()]);
        assert_eq!(
            legacy_only.known_senders(),
            vec![(ConsentSlot::Legacy, sender.clone())]
        );
        assert_eq!(legacy_only.slot_state(ConsentSlot::Pinned, &sender), Absent);
        assert_eq!(
            legacy_only.slot_state(ConsentSlot::Pseudonymous, &sender),
            Absent
        );
        assert_eq!(
            legacy_only.slot_state(ConsentSlot::Legacy, &sender),
            Revoked
        );

        let all = project(vec![
            grant(kinds[0]),
            grant(kinds[1]),
            grant(None),
            revoke(),
        ]);
        assert_eq!(
            all.known_senders(),
            slots
                .iter()
                .map(|slot| (*slot, sender.clone()))
                .collect::<Vec<_>>()
        );
        for slot in slots {
            assert_eq!(all.slot_state(slot, &sender), Revoked);
        }
        all.apply(&grant(kinds[0]));
        assert_eq!(all.consent_for(&pinned), Trusted);
        assert_eq!(all.consent_for(&pseudonymous), Revoked);
        assert_eq!(all.slot_state(ConsentSlot::Legacy, &sender), Revoked);
    }

    #[cfg(feature = "a2a")]
    #[tokio::test]
    async fn k09b_written_pin_override_implies_only_pinned_consent() {
        let workspace = tempfile::tempdir().unwrap();
        let (policy, peer) = loopback_collision_policy(workspace.path());
        let (manager, _) = manager(Arc::new(RecordingJournal::default()));
        manager.set_effective_policy(Arc::new(policy)).await;
        let pinned = manager
            .register(
                PolicyPrincipal::rap(peer.clone()),
                "pinned",
                DeliveryDisposition::MayRefuse,
            )
            .await;
        assert!(!pinned.pending);
        assert_eq!(
            pinned.decision.await.unwrap(),
            InboundApprovalDecision::AllowAlways
        );
        let pseudonymous = PolicyPrincipal::Pseudonymous(peer);
        let registration = manager
            .register(
                pseudonymous.clone(),
                "pseudonymous collision",
                DeliveryDisposition::MayRefuse,
            )
            .await;
        assert!(registration.pending && registration.first_for_sender);
        manager
            .resolve(&pseudonymous, InboundApprovalDecision::Decline)
            .await;
        assert_eq!(
            registration.decision.await.unwrap(),
            InboundApprovalDecision::Decline
        );
    }
}
