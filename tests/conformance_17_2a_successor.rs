use std::sync::Arc;

use rustain::domain::models::{
    AgentId, AgentMessage, AgentMetrics, CapabilityTokenId, CorrelationId, Envelope, JournalRecord,
    MessageHeader, MessageKind, NodeState,
};
use rustain::domain::ports::{AgentMessageBus, RelationshipDeliveryPolicy};
use rustain::infrastructure::subagent::{
    AgentHandle, DaemonSingletonLock, LocalMessageBus, MailboxBudget, NodeJournal, NodeRecovery,
    NodeTree, SpoolMeta, SubagentSpool,
};

fn live_node_handle(
    agent_id: AgentId,
) -> (
    AgentHandle,
    tokio::sync::mpsc::Receiver<rustain::domain::models::Op>,
) {
    let (command_tx, command_rx) = tokio::sync::mpsc::channel(1);
    let (status, _status_rx) = tokio::sync::watch::channel(NodeState::Created);
    let (_metrics_tx, metrics) = tokio::sync::watch::channel(AgentMetrics::default());
    (
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
        },
        command_rx,
    )
}

fn node_handle(agent_id: AgentId) -> AgentHandle {
    live_node_handle(agent_id).0
}

async fn terminal_tree() -> (NodeTree, Arc<NodeJournal>, tempfile::TempDir, AgentId) {
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
    (tree, journal, workspace, predecessor)
}

#[tokio::test]
async fn successor_spawns_under_stable_alias_without_reviving_predecessor() {
    let (tree, journal, _workspace, predecessor) = terminal_tree().await;
    let successor = AgentId::parse("successor").expect("valid fixture agent id");
    let spawned = tree
        .spawn_successor(
            &predecessor,
            "worker",
            AgentId::root(),
            node_handle(successor.clone()),
        )
        .await
        .expect("successor spawns from a terminal predecessor");
    assert_eq!(spawned, successor);
    assert_eq!(tree.resolve_alias("worker").await, Some(successor.clone()));
    assert_eq!(
        tree.predecessor_of(&successor).await,
        Some(predecessor.clone())
    );

    // Revival is rejected: the terminal predecessor stays terminal.
    tree.set_state(&predecessor, NodeState::Running).await;
    let predecessor_state = tree
        .list()
        .await
        .into_iter()
        .find(|entry| entry.agent_id == predecessor)
        .expect("predecessor present")
        .current_status;
    assert_eq!(
        predecessor_state,
        NodeState::Completed,
        "mutant: a revival edge would flip the terminal predecessor back to Running"
    );

    let has_successor_record = journal
        .load()
        .await
        .expect("load journal")
        .into_iter()
        .any(|entry| matches!(entry.record, JournalRecord::Successor { .. }));
    assert!(has_successor_record, "successor spawn must be journaled");
}

#[tokio::test]
async fn successor_alias_and_lineage_survive_recovery() {
    let (tree, journal, workspace, predecessor) = terminal_tree().await;
    let successor = AgentId::parse("successor-recovered").expect("valid fixture agent id");
    tree.spawn_successor(
        &predecessor,
        "durable-worker",
        AgentId::root(),
        node_handle(successor.clone()),
    )
    .await
    .expect("successor spawn");

    let recovered =
        NodeTree::with_now_fn(Arc::new(|| 1_700_000_000_001)).with_journal(journal.clone());
    let singleton = DaemonSingletonLock::try_acquire(workspace.path())
        .await
        .expect("acquire singleton lock");
    NodeRecovery::reconcile(&journal, &recovered, &singleton, "local")
        .await
        .expect("recover successor lineage");

    assert_eq!(
        recovered.resolve_alias("durable-worker").await,
        Some(successor.clone())
    );
    assert_eq!(
        recovered.predecessor_of(&successor).await,
        Some(predecessor)
    );
}

#[tokio::test]
async fn stable_alias_recovers_the_predecessor_durable_spool() {
    let (tree, journal, workspace, predecessor) = terminal_tree().await;
    let spool = SubagentSpool::new(workspace.path().join("spool"))
        .await
        .expect("open spool");
    spool
        .append("task-prior", b"durable predecessor transcript")
        .await
        .expect("append predecessor transcript");
    spool
        .write_meta(
            "task-prior",
            &SpoolMeta {
                status: NodeState::Completed,
                tokens_in: 3,
                tokens_out: 4,
                started_at: 1_700_000_000_000,
                ended_at: Some(1_700_000_000_001),
                subagent_type: "test".into(),
                agent_id: predecessor.as_str().to_string(),
            },
        )
        .await
        .expect("write predecessor metadata");
    tree.bind_alias(&predecessor, "durable-worker")
        .await
        .expect("bind stable alias");

    let recovered =
        NodeTree::with_now_fn(Arc::new(|| 1_700_000_000_002)).with_journal(journal.clone());
    let singleton = DaemonSingletonLock::try_acquire(workspace.path())
        .await
        .expect("acquire singleton lock");
    NodeRecovery::reconcile(&journal, &recovered, &singleton, "local")
        .await
        .expect("recover stable alias");

    let recovered_node = recovered
        .resolve_alias("durable-worker")
        .await
        .expect("alias resolves after restart");
    let task_id = spool
        .task_id_for_agent(&recovered_node)
        .await
        .expect("scan spool metadata")
        .expect("spool id for recovered node");
    assert_eq!(task_id, "task-prior");
    assert_eq!(
        spool.read_full(&task_id).await.expect("read transcript"),
        "durable predecessor transcript"
    );
}

#[tokio::test]
async fn spawn_successor_rejects_a_live_predecessor() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let journal = Arc::new(
        NodeJournal::open_workspace(workspace.path())
            .await
            .expect("open journal"),
    );
    let tree = NodeTree::with_now_fn(Arc::new(|| 1_700_000_000_000)).with_journal(journal);
    let live = AgentId::parse("live-node").expect("valid fixture agent id");
    tree.register(live.clone(), AgentId::root(), node_handle(live.clone()))
        .await
        .expect("register node");
    tree.set_state(&live, NodeState::Running).await;

    let successor = AgentId::parse("successor").expect("valid fixture agent id");
    assert!(
        tree.spawn_successor(&live, "worker", AgentId::root(), node_handle(successor))
            .await
            .is_err(),
        "a non-terminal predecessor must never spawn a successor"
    );
}

#[tokio::test]
async fn must_report_obligation_is_stamped_by_delivery_and_journaled_on_terminal() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let journal = Arc::new(
        NodeJournal::open_workspace(workspace.path())
            .await
            .expect("open journal"),
    );
    let tree = NodeTree::with_now_fn(Arc::new(|| 1_700_000_000_000)).with_journal(journal.clone());
    let node = AgentId::parse("must-report-node").expect("valid fixture agent id");
    let (handle, _command_rx) = live_node_handle(node.clone());
    tree.register(node.clone(), AgentId::root(), handle)
        .await
        .expect("register recipient");
    tree.set_state(&node, NodeState::Running).await;

    let correlation_id = CorrelationId::new("corr-violated");
    let envelope = Envelope {
        header: MessageHeader {
            sender: AgentId::parse("parent").expect("valid fixture parent id"),
            recipient: node.clone(),
            correlation_id: correlation_id.clone(),
            kind: MessageKind::PeerMessage,
            sequence: None,
            verified_peer_id: None,
        },
        body: AgentMessage::new("report this result"),
    };
    let bus = LocalMessageBus::new(tree.clone(), Arc::new(RelationshipDeliveryPolicy));
    bus.deliver(&node, envelope)
        .await
        .expect("owned delivery is accepted");
    tree.set_state(&node, NodeState::Completed).await;

    let journaled = journal
        .obligation_violations()
        .await
        .expect("read journal violations");
    assert_eq!(journaled, vec![(node, correlation_id)]);
}

#[tokio::test]
async fn owner_report_with_matching_correlation_discharges_obligation() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let journal = Arc::new(
        NodeJournal::open_workspace(workspace.path())
            .await
            .expect("open journal"),
    );
    let tree = NodeTree::with_now_fn(Arc::new(|| 1_700_000_000_000)).with_journal(journal.clone());
    let parent = AgentId::parse("report-parent").expect("valid parent id");
    let worker = AgentId::parse("report-worker").expect("valid worker id");
    let (parent_handle, _parent_rx) = live_node_handle(parent.clone());
    let (worker_handle, _worker_rx) = live_node_handle(worker.clone());
    tree.register(parent.clone(), AgentId::root(), parent_handle)
        .await
        .expect("register parent");
    tree.register(worker.clone(), AgentId::root(), worker_handle)
        .await
        .expect("register worker");
    tree.set_state(&parent, NodeState::Running).await;
    tree.set_state(&worker, NodeState::Running).await;

    let correlation_id = CorrelationId::new("corr-reported");
    let bus = LocalMessageBus::new(tree.clone(), Arc::new(RelationshipDeliveryPolicy));
    bus.deliver(
        &worker,
        Envelope {
            header: MessageHeader {
                sender: parent.clone(),
                recipient: worker.clone(),
                correlation_id: correlation_id.clone(),
                kind: MessageKind::PeerMessage,
                sequence: None,
                verified_peer_id: None,
            },
            body: AgentMessage::new("produce a report"),
        },
    )
    .await
    .expect("assignment delivery");
    bus.deliver(
        &parent,
        Envelope {
            header: MessageHeader {
                sender: worker.clone(),
                recipient: parent.clone(),
                correlation_id: correlation_id.clone(),
                kind: MessageKind::OwnerReport,
                sequence: None,
                verified_peer_id: None,
            },
            body: AgentMessage::new("completed report"),
        },
    )
    .await
    .expect("owner report delivery");
    tree.set_state(&worker, NodeState::Completed).await;

    assert!(
        journal
            .obligation_violations()
            .await
            .expect("read violations")
            .into_iter()
            .all(|(node, correlation)| node != worker || correlation != correlation_id),
        "a matching OwnerReport must discharge the worker obligation"
    );
}
