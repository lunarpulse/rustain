//! Conformance test for AC-10-2-4: subagent ownership events propagate through
//! the existing AppEvent bus via CapabilityEvent::Registered/Deregistered.

use std::sync::Arc;

use rustain::domain::events::{AppEvent, CapabilityEvent};
use rustain::domain::models::{AgentId, NodeState};
use rustain::infrastructure::subagent::NodeTree;
use tokio::sync::mpsc;
use tokio::sync::watch;

#[tokio::test]
async fn test_subagent_ownership_events_flow_through_app_event_bus() {
    let (tx, mut rx) = mpsc::unbounded_channel::<AppEvent>();
    let now = Arc::new(|| 1_700_000_000_000_i64);
    let registry = NodeTree::with_event_tx(tx, now);

    let root = AgentId::root();
    let a = AgentId::new();
    let b = AgentId::new();
    let c = AgentId::new();

    // Register a 3-level chain: root → a → b → c
    for (agent, parent) in [
        (a.clone(), root.clone()),
        (b.clone(), a.clone()),
        (c.clone(), b.clone()),
    ] {
        let (cmd_tx, _cmd_rx) = mpsc::channel(1);
        let (status_tx, _status_rx) = watch::channel(NodeState::Created);
        let (_metrics_tx, metrics_rx) =
            watch::channel(rustain::domain::models::AgentMetrics::default());
        let handle = rustain::infrastructure::subagent::AgentHandle {
            isolated: false,
            agent_id: agent.clone(),
            token: rustain::domain::models::CapabilityTokenId::nil(),
            command_tx: cmd_tx,
            cancel_token: tokio_util::sync::CancellationToken::new(),
            depth: 0,
            subagent_type: "test".into(),
            spawned_at: 0,
            status: status_tx,
            metrics: metrics_rx,
            mailbox_budget: rustain::infrastructure::subagent::MailboxBudget::new(),
        };
        registry.register(agent, parent, handle).await.unwrap();
    }

    // Collect Registered events
    let mut registered = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let AppEvent::CapabilityEvent(CapabilityEvent::Registered { capability }) = event {
            registered.push(capability.id.tool.clone());
        }
    }
    assert_eq!(
        registered.len(),
        3,
        "Expected 3 Registered events for 3 agents"
    );

    // Deregister all
    registry.deregister(&c).await;
    registry.deregister(&b).await;
    registry.deregister(&a).await;

    // Collect Deregistered events
    let mut deregistered = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let AppEvent::CapabilityEvent(CapabilityEvent::Deregistered { capability }) = event {
            deregistered.push(capability.id.tool.clone());
        }
    }
    assert_eq!(
        deregistered.len(),
        3,
        "Expected 3 Deregistered events for 3 agents"
    );
}
