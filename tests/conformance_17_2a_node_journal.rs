use std::path::Path;
use std::sync::Arc;

use rustain::domain::events::{AppEvent, DomainEventPayload};
use rustain::domain::models::{
    AgentId, CapabilityTokenId, CorrelationId, HostBinding, JournalRecord, NodeCheckpoint,
    NodeOrigin, NodeState, OrchestrationRoomId, RoomEvent, WaveId, WireOwnershipKind,
};
use rustain::infrastructure::subagent::{
    AgentHandle, DaemonSingletonLock, JournalError, MailboxBudget, NodeJournal, NodeRecovery,
    NodeTree,
};

fn checkpoint(id: &str, state: NodeState) -> NodeCheckpoint {
    NodeCheckpoint {
        id: AgentId::parse(id).expect("valid fixture agent id"),
        token: CapabilityTokenId::root(),
        parent: None,
        ownership: WireOwnershipKind::Owned,
        state,
        origin: NodeOrigin::Subagent,
        foreground: true,
        effective_model: "test-model".into(),
        tokens_in: 0,
        tokens_out: 0,
        turns: 0,
        subagent_type: "test".into(),
        spawned_at: 1_700_000_000_000,
        depth: 1,
        tainted: false,
        waiting_since: None,
    }
}

async fn append_raw(path: &Path, bytes: &[u8]) {
    use tokio::io::AsyncWriteExt;
    let mut file = tokio::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .await
        .expect("open journal for fault injection");
    file.write_all(bytes).await.expect("append fault bytes");
    file.sync_data().await.expect("sync fault bytes");
}

fn node_handle(agent_id: AgentId) -> AgentHandle {
    let (command_tx, _command_rx) = tokio::sync::mpsc::channel(1);
    let (status, _status_rx) = tokio::sync::watch::channel(NodeState::Created);
    let (_metrics_tx, metrics) =
        tokio::sync::watch::channel(rustain::domain::models::AgentMetrics::default());
    AgentHandle {
        isolated: false,
        agent_id,
        token: CapabilityTokenId::root(),
        command_tx,
        cancel_token: tokio_util::sync::CancellationToken::new(),
        depth: 1,
        subagent_type: "test".into(),
        spawned_at: 1_700_000_000_000,
        status,
        metrics,
        mailbox_budget: MailboxBudget::new(),
    }
}

#[tokio::test]
async fn ordered_room_journal_roundtrips_records_and_discards_torn_tail() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let room = OrchestrationRoomId::parse("room-journal").expect("valid room id");
    let journal = NodeJournal::open(workspace.path(), room)
        .await
        .expect("open journal");

    let first = journal
        .append_checkpoint(checkpoint("node-a", NodeState::Running))
        .await
        .expect("append checkpoint");
    let second = journal
        .append_room(RoomEvent::NodeRegistered {
            node: AgentId::parse("node-a").expect("valid fixture agent id"),
            origin: NodeOrigin::Subagent,
            host: HostBinding::new("host-a", "workspace-a"),
        })
        .await
        .expect("append room event");

    assert_eq!(first.seq, 1);
    assert_eq!(second.seq, 2);
    append_raw(journal.path(), br#"{"schema_version":1,"seq":3,"record":"#).await;

    let recovered = journal.load().await.expect("recover before torn tail");
    assert_eq!(recovered.len(), 2);
    assert!(matches!(recovered[0].record, JournalRecord::Checkpoint(_)));
    assert!(matches!(recovered[1].record, JournalRecord::Room(_)));
}

#[tokio::test]
async fn journal_rejects_schema_mismatch_instead_of_guessing() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let room = OrchestrationRoomId::parse("room-schema").expect("valid room id");
    let journal = NodeJournal::open(workspace.path(), room)
        .await
        .expect("open journal");

    append_raw(
        journal.path(),
        br#"{"schema_version":999,"seq":1,"record":{"kind":"room","payload":{"event":"host_bound_unavailable","node":"node-a","host":{"host_id":"host-a","workspace_id":"workspace-a"}}}}
"#,
    )
    .await;

    let error = journal
        .load()
        .await
        .expect_err("schema drift must fail closed");
    assert!(error.to_string().contains("unsupported journal schema"));
}

#[tokio::test]
async fn node_tree_state_transition_is_durably_checkpointed() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let room = OrchestrationRoomId::parse("room-lifecycle").expect("valid room id");
    let journal = Arc::new(
        NodeJournal::open(workspace.path(), room)
            .await
            .expect("open journal"),
    );
    let tree = NodeTree::with_now_fn(Arc::new(|| 1_700_000_000_000)).with_journal(journal.clone());
    let node = AgentId::parse("node-lifecycle").expect("valid fixture agent id");
    tree.register(node.clone(), AgentId::root(), node_handle(node.clone()))
        .await
        .expect("register node");

    tree.set_state(&node, NodeState::Running).await;
    tree.set_state(&node, NodeState::Completed).await;

    let records = journal.load().await.expect("load lifecycle journal");
    let states = records
        .into_iter()
        .filter_map(|entry| match entry.record {
            JournalRecord::Checkpoint(checkpoint) => Some(checkpoint.state),
            JournalRecord::Room(_) => None,
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        states,
        vec![NodeState::Created, NodeState::Running, NodeState::Completed]
    );
}

#[test]
fn journal_error_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<JournalError>();
}

#[tokio::test]
async fn reconcile_requires_singleton_and_recovers_running_exactly_once() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let journal = Arc::new(
        NodeJournal::open_workspace(workspace.path())
            .await
            .expect("open journal"),
    );
    let node = AgentId::parse("node-recovery").expect("valid fixture agent id");
    journal
        .append_checkpoint(checkpoint(node.as_str(), NodeState::Running))
        .await
        .expect("append running checkpoint");
    let failed = AgentId::parse("node-failed").expect("valid fixture agent id");
    journal
        .append_checkpoint(checkpoint(failed.as_str(), NodeState::Failed))
        .await
        .expect("append failed checkpoint");
    let tree = NodeTree::with_now_fn(Arc::new(|| 1_700_000_000_000)).with_journal(journal.clone());
    let singleton = DaemonSingletonLock::try_acquire(workspace.path())
        .await
        .expect("acquire daemon singleton");

    let first = NodeRecovery::reconcile(&journal, &tree, &singleton, "host-a")
        .await
        .expect("first reconcile");
    let second = NodeRecovery::reconcile(&journal, &tree, &singleton, "host-a")
        .await
        .expect("second reconcile");

    assert_eq!(first.suspended, vec![node.clone()]);
    assert_eq!(first.failed, vec![failed.clone()]);
    assert!(
        second.suspended.is_empty(),
        "second replay must be idempotent"
    );
    let recovered = tree
        .list()
        .await
        .into_iter()
        .find(|entry| entry.agent_id == node)
        .expect("recovered node present");
    assert_eq!(recovered.current_status, NodeState::Suspended);
    let failed_recovered = tree
        .list()
        .await
        .into_iter()
        .find(|entry| entry.agent_id == failed)
        .expect("failed node restored");
    assert_eq!(failed_recovered.current_status, NodeState::Failed);
    assert!(
        tree.delivery_target(&node).await.is_some(),
        "positive control: transient handle was rebuilt"
    );
    let checkpoints = journal
        .load()
        .await
        .expect("load reconciled journal")
        .into_iter()
        .filter(|entry| matches!(entry.record, JournalRecord::Checkpoint(_)))
        .count();
    assert_eq!(
        checkpoints, 3,
        "running plus failed plus one suspended recovery record"
    );
}

#[tokio::test]
async fn daemon_singleton_lock_rejects_a_live_second_reconciler() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let first = DaemonSingletonLock::try_acquire(workspace.path())
        .await
        .expect("first daemon owns singleton");
    let second = DaemonSingletonLock::try_acquire(workspace.path()).await;
    assert!(
        second.is_err(),
        "mutant: a second live daemon must not recover"
    );
    drop(first);
    DaemonSingletonLock::try_acquire(workspace.path())
        .await
        .expect("lock is released with daemon ownership");
}

#[tokio::test]
async fn journal_projects_room_and_marks_foreign_host_handles_unavailable() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let room_id = OrchestrationRoomId::parse("room-projection").expect("valid room id");
    let journal = NodeJournal::open(workspace.path(), room_id)
        .await
        .expect("open journal");
    let node = AgentId::parse("node-room").expect("valid fixture agent id");
    journal
        .append_room(RoomEvent::NodeRegistered {
            node: node.clone(),
            origin: NodeOrigin::Subagent,
            host: HostBinding::new("host-a", "workspace-a"),
        })
        .await
        .expect("append node event");
    journal
        .append_room(RoomEvent::WaveStarted {
            wave: WaveId::parse("wave-room").expect("valid wave id"),
            coordinator: node.clone(),
            spokes: Vec::new(),
        })
        .await
        .expect("append wave event");

    let local = journal
        .project_room("host-a")
        .await
        .expect("project local room");
    assert_eq!(
        local.waves().len(),
        1,
        "positive control: journal event bites"
    );
    assert!(!local.nodes()[&node].host_bound_unavailable);
    let foreign = journal
        .project_room("host-b")
        .await
        .expect("project foreign room");
    assert!(
        foreign.nodes()[&node].host_bound_unavailable,
        "foreign host must never receive a fabricated live handle"
    );
}

#[tokio::test]
async fn durable_lifecycle_events_are_mirrored_to_live_domain_bus() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let journal = Arc::new(
        NodeJournal::open_workspace(workspace.path())
            .await
            .expect("open journal"),
    );
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let tree = NodeTree::with_event_tx(event_tx, Arc::new(|| 1_700_000_000_000))
        .with_journal(journal)
        .with_host_binding(HostBinding::new("host-a", "workspace-a"));
    let node = AgentId::parse("node-events").expect("valid fixture agent id");
    tree.register(node.clone(), AgentId::root(), node_handle(node.clone()))
        .await
        .expect("register node");
    tree.set_state(&node, NodeState::Running).await;

    let mut registered = false;
    let mut running = false;
    while let Ok(event) = event_rx.try_recv() {
        match event {
            AppEvent::DomainEvent(DomainEventPayload::Room(RoomEvent::NodeRegistered {
                node: event_node,
                ..
            })) if event_node == node => registered = true,
            AppEvent::DomainEvent(DomainEventPayload::Room(RoomEvent::NodeStateChanged {
                node: event_node,
                from: NodeState::Created,
                to: NodeState::Running,
            })) if event_node == node => running = true,
            _ => {}
        }
    }
    assert!(
        registered,
        "journaled registration must reach live reactivity"
    );
    assert!(running, "journaled transition must reach live reactivity");
}

#[tokio::test]
async fn waiting_hazard_dwell_is_wall_clock_and_survives_restart() {
    use rustain::domain::models::{WAITING_HAZARD_THRESHOLD_MS, waiting_hazard};

    let workspace = tempfile::tempdir().expect("temporary workspace");
    let waiting_since = 1_700_000_000_000i64;
    let node = AgentId::parse("node-hazard").expect("valid fixture agent id");
    {
        let journal = Arc::new(
            NodeJournal::open_workspace(workspace.path())
                .await
                .expect("open journal"),
        );
        let tree =
            NodeTree::with_now_fn(Arc::new(move || waiting_since)).with_journal(journal.clone());
        tree.register(node.clone(), AgentId::root(), node_handle(node.clone()))
            .await
            .expect("register node");
        tree.set_state(&node, NodeState::Running).await;
        tree.set_state(&node, NodeState::Waiting).await;
    }

    // Simulate a fresh process: reopen and read back the durable checkpoint.
    let reopened = NodeJournal::open_workspace(workspace.path())
        .await
        .expect("reopen journal");
    let checkpoint = reopened
        .load()
        .await
        .expect("load journal")
        .into_iter()
        .filter_map(|entry| match entry.record {
            JournalRecord::Checkpoint(cp) if cp.id == node && cp.state == NodeState::Waiting => {
                Some(cp)
            }
            _ => None,
        })
        .next_back()
        .expect("durable Waiting checkpoint");
    assert_eq!(checkpoint.waiting_since, Some(waiting_since));

    let before = waiting_since + WAITING_HAZARD_THRESHOLD_MS - 1;
    assert!(
        waiting_hazard(&checkpoint, before, WAITING_HAZARD_THRESHOLD_MS).is_none(),
        "hazard must not fire before the dwell threshold"
    );
    let after = waiting_since + WAITING_HAZARD_THRESHOLD_MS + 1;
    let hazard = waiting_hazard(&checkpoint, after, WAITING_HAZARD_THRESHOLD_MS)
        .expect("dwell across restart still escalates on wall clock");
    assert!(hazard.dwell_ms >= WAITING_HAZARD_THRESHOLD_MS);
}

#[tokio::test]
async fn reopen_on_different_host_does_not_fabricate_a_live_handle() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let journal = Arc::new(
        NodeJournal::open_workspace(workspace.path())
            .await
            .expect("open journal"),
    );
    let node = AgentId::parse("node-foreign").expect("valid fixture agent id");
    journal
        .append_room(RoomEvent::NodeRegistered {
            node: node.clone(),
            origin: NodeOrigin::Subagent,
            host: HostBinding::new("host-a", "workspace-a"),
        })
        .await
        .expect("append registration on host-a");
    journal
        .append_checkpoint(checkpoint(node.as_str(), NodeState::Running))
        .await
        .expect("append running checkpoint");
    let tree = NodeTree::with_now_fn(Arc::new(|| 1_700_000_000_000)).with_journal(journal.clone());
    let singleton = DaemonSingletonLock::try_acquire(workspace.path())
        .await
        .expect("acquire daemon singleton");

    // Reopened on a DIFFERENT host: no live handle, marked unavailable.
    let report = NodeRecovery::reconcile(&journal, &tree, &singleton, "host-b")
        .await
        .expect("reconcile on foreign host");
    assert_eq!(report.host_bound_unavailable, vec![node.clone()]);
    assert!(report.suspended.is_empty());
    assert!(
        tree.delivery_target(&node).await.is_none(),
        "a foreign-host node must never receive a fabricated live handle"
    );
}

// ── Review-patch behavioral proofs (2026-07-11 code review) ──────────────────

/// D1: a second writer opening the same room must re-derive the tail under the
/// cross-process lock, never reuse a cached `next_seq`. The old per-instance
/// cache made both writers allocate `seq=1` and poison the whole log.
#[tokio::test]
async fn concurrent_writers_share_one_ordered_sequence() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let writer_a = NodeJournal::open_workspace(workspace.path())
        .await
        .expect("open journal a");
    let writer_b = NodeJournal::open_workspace(workspace.path())
        .await
        .expect("open journal b");

    let first = writer_a
        .append_checkpoint(checkpoint("node-a", NodeState::Running))
        .await
        .expect("writer a append");
    let second = writer_b
        .append_checkpoint(checkpoint("node-b", NodeState::Running))
        .await
        .expect("writer b append");

    assert_eq!(first.seq, 1);
    assert_eq!(
        second.seq, 2,
        "a second writer must re-derive the durable tail, not reuse a cached seq"
    );
    let loaded = writer_a.load().await.expect("load shared journal");
    assert_eq!(
        loaded.iter().map(|entry| entry.seq).collect::<Vec<_>>(),
        vec![1, 2],
        "the shared log stays contiguous"
    );
}

/// P11: a non-crash partial write leaves a torn tail; the next append must
/// truncate it under the lock before writing so the log never gains a corrupt
/// middle record.
#[tokio::test]
async fn append_repairs_torn_tail_before_writing() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let room = OrchestrationRoomId::parse("room-torn").expect("valid room id");
    let journal = NodeJournal::open(workspace.path(), room)
        .await
        .expect("open journal");
    journal
        .append_checkpoint(checkpoint("node-a", NodeState::Running))
        .await
        .expect("first append");
    // A partial, un-terminated record left by an ENOSPC/EIO mid-write.
    append_raw(journal.path(), br#"{"schema_version":1,"seq":2,"record":"#).await;

    let next = journal
        .append_checkpoint(checkpoint("node-a", NodeState::Completed))
        .await
        .expect("append after torn tail");
    assert_eq!(
        next.seq, 2,
        "torn tail truncated; sequence stays contiguous"
    );
    let loaded = journal.load().await.expect("load after repair");
    assert_eq!(
        loaded.iter().map(|entry| entry.seq).collect::<Vec<_>>(),
        vec![1, 2]
    );
}

/// D2 + P2: dwell rides the injected wall clock, escalates once per waiting
/// epoch, and the hazard is durably journaled (a monotonic `Instant` would
/// reset across a restart and never fire).
#[tokio::test]
async fn waiting_hazard_escalates_once_over_persisted_wall_dwell() {
    use std::sync::atomic::{AtomicI64, Ordering};
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let clock = Arc::new(AtomicI64::new(1_700_000_000_000));
    let now_fn = {
        let clock = clock.clone();
        Arc::new(move || clock.load(Ordering::SeqCst))
    };
    let journal = Arc::new(
        NodeJournal::open_workspace(workspace.path())
            .await
            .expect("open journal"),
    );
    let tree = NodeTree::with_now_fn(now_fn).with_journal(journal.clone());
    let node = AgentId::parse("node-wait").expect("valid fixture agent id");
    tree.register(node.clone(), AgentId::root(), node_handle(node.clone()))
        .await
        .expect("register node");
    tree.set_state(&node, NodeState::Running).await;
    tree.set_state(&node, NodeState::Waiting).await;

    assert!(
        tree.raise_due_hazards(60_000).await.is_empty(),
        "no escalation before the dwell threshold"
    );
    clock.fetch_add(60_001, Ordering::SeqCst);
    assert_eq!(
        tree.raise_due_hazards(60_000).await,
        vec![node.clone()],
        "dwell past threshold escalates"
    );
    assert!(
        tree.raise_due_hazards(60_000).await.is_empty(),
        "one hazard per waiting epoch (idempotent)"
    );
    let hazards = journal
        .load()
        .await
        .expect("load hazard journal")
        .into_iter()
        .filter(|entry| matches!(entry.record, JournalRecord::HazardRaised { .. }))
        .count();
    assert_eq!(hazards, 1, "the hazard is durably journaled exactly once");
}

/// P4: an accepted MustReport obligation left undischarged by a crash is
/// rebuilt on recovery, so taking the recovered node terminal still journals
/// the violation (it was silently lost before).
#[tokio::test]
async fn recovered_obligation_becomes_violation_on_terminal() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let journal = Arc::new(
        NodeJournal::open_workspace(workspace.path())
            .await
            .expect("open journal"),
    );
    let node = AgentId::parse("node-oblig").expect("valid fixture agent id");
    let correlation = CorrelationId("corr-1".into());
    journal
        .append_checkpoint(checkpoint(node.as_str(), NodeState::Running))
        .await
        .expect("append running checkpoint");
    journal
        .append_obligation_accepted(node.clone(), correlation.clone())
        .await
        .expect("append accepted obligation");
    let tree = NodeTree::with_now_fn(Arc::new(|| 1_700_000_000_000)).with_journal(journal.clone());
    let singleton = DaemonSingletonLock::try_acquire(workspace.path())
        .await
        .expect("acquire singleton");
    NodeRecovery::reconcile(&journal, &tree, &singleton, "host-a")
        .await
        .expect("reconcile");

    tree.set_state(&node, NodeState::Cancelled).await;
    let violations = journal
        .obligation_violations()
        .await
        .expect("load violations");
    assert!(
        violations
            .iter()
            .any(|(recorded, corr)| recorded == &node && corr == &correlation),
        "a recovered undischarged obligation must journal a violation at terminal"
    );
}

/// P5: a crash-recovered node with no live runner must refuse delivery
/// honestly (awaiting-resume) rather than silently black-hole it into the
/// unconsumed inbox; normal semantics return after a resumer marks it.
#[tokio::test]
async fn recovered_node_awaits_resume_until_marked() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let journal = Arc::new(
        NodeJournal::open_workspace(workspace.path())
            .await
            .expect("open journal"),
    );
    let node = AgentId::parse("node-suspended").expect("valid fixture agent id");
    journal
        .append_checkpoint(checkpoint(node.as_str(), NodeState::Running))
        .await
        .expect("append running checkpoint");
    let tree = NodeTree::with_now_fn(Arc::new(|| 1_700_000_000_000)).with_journal(journal.clone());
    let singleton = DaemonSingletonLock::try_acquire(workspace.path())
        .await
        .expect("acquire singleton");
    NodeRecovery::reconcile(&journal, &tree, &singleton, "host-a")
        .await
        .expect("reconcile");

    let target = tree
        .delivery_target(&node)
        .await
        .expect("recovered node is addressable");
    assert!(
        target.awaiting_resume,
        "a recovered node with no live runner must not silently accept delivery"
    );
    tree.mark_resumed(&node).await;
    let target = tree
        .delivery_target(&node)
        .await
        .expect("still addressable");
    assert!(
        !target.awaiting_resume,
        "after resume, normal queuing semantics apply"
    );
}

/// P1: a follow-up must link to a genuinely-terminal predecessor even after the
/// terminal bridge deregistered it (a durable terminal checkpoint proves it) —
/// never a revival, the alias points at the NEW successor.
#[tokio::test]
async fn successor_links_after_terminal_predecessor_deregistered() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let journal = Arc::new(
        NodeJournal::open_workspace(workspace.path())
            .await
            .expect("open journal"),
    );
    let tree = NodeTree::with_now_fn(Arc::new(|| 1_700_000_000_000)).with_journal(journal.clone());
    let predecessor = AgentId::parse("predecessor").expect("valid fixture agent id");
    tree.register(
        predecessor.clone(),
        AgentId::root(),
        node_handle(predecessor.clone()),
    )
    .await
    .expect("register predecessor");
    tree.set_state(&predecessor, NodeState::Running).await;
    tree.set_state(&predecessor, NodeState::Completed).await;
    tree.deregister(&predecessor).await;

    let successor = AgentId::parse("successor").expect("valid fixture agent id");
    tree.register(
        successor.clone(),
        AgentId::root(),
        node_handle(successor.clone()),
    )
    .await
    .expect("register successor");
    tree.link_successor(&predecessor, &successor, "stable-alias")
        .await
        .expect("successor links to a journaled-terminal predecessor");
    assert_eq!(
        tree.resolve_alias("stable-alias").await,
        Some(successor),
        "the stable alias resolves to the successor"
    );
}
