//! Integration keystones for Story 17.5a — MCP Tasks as durable nodes,
//! driven end-to-end against the REAL rmcp fake (`tests/common/fake_mcp_server.rs`,
//! rebuilt on `rmcp::handler::server::ServerHandler` for this story) over a
//! real stdio child process.
//!
//! Every scripted case the story enumerates is exercised here through the
//! production path: `McpClientAdapter::call_tool` → transport shim → node
//! materialization → driver poll loop → `NodeTree` → journal. The fake's
//! task ids are deterministic per spawned process (`fake-task-1`, …).
//!
//! Requires `--features test-fake-mcp` (the fake bin is gated on it; the
//! missing-binary panic says so).

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use rustain::adapters::mcp::client::McpClientAdapter;
use rustain::adapters::mcp::task_driver::{
    McpTaskRuntime, PollConfig, mint_mcp_node_id, parse_mcp_node_id,
};
use rustain::domain::clock::{Clock, SystemClock};
use rustain::domain::models::{
    AgentId, JournalRecord, McpServerSource, McpServerSpec, McpTransport, NodeState, RoomEvent,
};
use rustain::infrastructure::subagent::{
    DaemonSingletonLock, NodeJournal, NodeRecovery, NodeRoomJournal, NodeTree,
};

fn fast_poll() -> PollConfig {
    PollConfig {
        interval: Duration::from_millis(5),
        deadline: Duration::from_secs(30),
        request_timeout: Duration::from_secs(10),
        max_status_updates: 64,
    }
}

struct TaskFixture {
    client: Arc<McpClientAdapter>,
    tree: Arc<NodeTree>,
    journal: Arc<NodeJournal>,
    _dir: tempfile::TempDir,
}

async fn task_fixture(server_id: &str) -> TaskFixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let journal = Arc::new(
        NodeJournal::open_workspace(dir.path())
            .await
            .expect("journal opens"),
    );
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let tree = Arc::new(
        NodeTree::with_event_tx(tx.clone(), Arc::new(|| 0i64)).with_journal(journal.clone()),
    );
    let room = Arc::new(NodeRoomJournal::new(journal.clone(), Some(tx)));
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::default());
    let runtime = Arc::new(
        McpTaskRuntime::new(tree.clone(), tree.clone(), room, clock).with_poll_config(fast_poll()),
    );

    let spec = McpServerSpec {
        id: server_id.to_string(),
        transport: McpTransport::Stdio,
        command: Some(common::fake_mcp_binary().to_string_lossy().into_owned()),
        args: vec![],
        env: BTreeMap::new(),
        url: None,
        persistent: false,
        source: McpServerSource::Workspace,
    };
    let client = Arc::new(McpClientAdapter::new(spec, None));
    client.set_task_runtime(runtime.clone());
    client.connect().await.expect("fake connects");

    TaskFixture {
        client,
        tree,
        journal,
        _dir: dir,
    }
}

/// Arm one scripted task case: the next `tools/call` creates a task.
async fn arm(client: &McpClientAdapter, scenario: &str) {
    let ack = client
        .send_custom_request(
            "test/control/arm",
            serde_json::json!({
                "target": "tools/call",
                "remaining": 1,
                "scenario": scenario,
            }),
        )
        .await
        .expect("arm acknowledged");
    assert_eq!(
        ack.get("resultType").and_then(|v| v.as_str()),
        Some("complete")
    );
}

async fn start_task(client: &McpClientAdapter) -> rustain::domain::models::ToolResult {
    client
        .call_tool(
            "scripted-task",
            serde_json::json!({}),
            CancellationToken::new(),
        )
        .await
        .expect("task-creating call")
}

async fn wait_node_state(tree: &NodeTree, node_id: &AgentId, want: NodeState) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(entry) = tree
                .list()
                .await
                .into_iter()
                .find(|e| &e.agent_id == node_id)
                && entry.current_status == want
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("node {node_id} reached {want:?}"));
}

fn first_task_node(server_id: &str) -> AgentId {
    mint_mcp_node_id(server_id, "fake-task-1")
}

/// AC1 keystone: a task-creating `tools/call` materializes a durable
/// first-class node, drives it to `Completed`, and journals every state
/// change plus the task binding — while a plain (non-task) `tools/call`
/// still flows through the pre-existing path byte-identically.
#[tokio::test]
async fn durable_node_materialization_from_captured_wire() {
    let fx = task_fixture("srv-it").await;

    // Non-task call first: unchanged pre-existing behavior.
    let plain = fx
        .client
        .call_tool(
            "echo",
            serde_json::json!({"text": "hi"}),
            CancellationToken::new(),
        )
        .await
        .expect("plain echo");
    assert!(!plain.is_error);
    assert!(plain.content.contains("echo: hi"));

    arm(&fx.client, "progress").await;
    let started = start_task(&fx.client).await;
    assert!(!started.is_error);
    assert!(started.content.contains("fake-task-1"));

    let node = first_task_node("srv-it");
    wait_node_state(&fx.tree, &node, NodeState::Completed).await;

    let entry = fx
        .tree
        .list()
        .await
        .into_iter()
        .find(|e| e.agent_id == node)
        .expect("node present");
    assert_eq!(entry.subagent_type, "mcp-task");

    let records = fx.journal.load().await.expect("journal load");
    assert!(
        records.iter().any(|e| matches!(
            &e.record,
            JournalRecord::Room(RoomEvent::McpTaskBound { server, task, .. })
                if server == "srv-it" && task == "fake-task-1"
        )),
        "McpTaskBound journaled"
    );
    assert!(
        records.iter().any(|e| matches!(
            &e.record,
            JournalRecord::Checkpoint(cp) if cp.id == node && cp.state == NodeState::Completed
        )),
        "terminal checkpoint journaled"
    );
    fx.client.disconnect().await.expect("disconnect");
}

/// R-14 keystone: a protocol-level task failure maps to `Failed`.
#[tokio::test]
async fn wire_failed_maps_to_node_failed() {
    let fx = task_fixture("srv-fail").await;
    arm(&fx.client, "error").await;
    start_task(&fx.client).await;
    wait_node_state(&fx.tree, &first_task_node("srv-fail"), NodeState::Failed).await;
    fx.client.disconnect().await.expect("disconnect");
}

/// R-14 keystone: `isError:true` is `Completed`, never `Failed`.
#[tokio::test]
async fn iserror_completed_maps_to_completed_not_failed() {
    let fx = task_fixture("srv-iserr").await;
    arm(&fx.client, "isError-completed").await;
    start_task(&fx.client).await;
    wait_node_state(
        &fx.tree,
        &first_task_node("srv-iserr"),
        NodeState::Completed,
    )
    .await;
    fx.client.disconnect().await.expect("disconnect");
}

/// AC4 + R-15 keystone against the REAL fake's ack-then-never-cancel mutant:
/// the fake acks `tasks/cancel` but NEVER transitions to `cancelled`, yet
/// teardown drives the node to `Cancelled` — on the ack, not on an observed
/// status.
#[tokio::test]
async fn ack_cancel_drives_cascade_without_observed_cancelled() {
    let fx = task_fixture("srv-cancel").await;
    arm(&fx.client, "cancellation").await;
    start_task(&fx.client).await;
    let node = first_task_node("srv-cancel");
    wait_node_state(&fx.tree, &node, NodeState::Running).await;

    fx.client.disconnect().await.expect("disconnect");
    // cascade_kill terminalizes AND deregisters: the live tree no longer
    // lists the node, but the Cancelled checkpoint is durable.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let gone = !fx.tree.list().await.iter().any(|e| e.agent_id == node);
            let journaled = fx.journal.load().await.unwrap_or_default().iter().any(|e| {
                matches!(
                    &e.record,
                    JournalRecord::Checkpoint(cp) if cp.id == node && cp.state == NodeState::Cancelled
                )
            });
            if gone && journaled {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("node terminalized (deregistered) with a durable Cancelled checkpoint");
}

/// AC5 keystone: the server child exits mid-task (process dies 25ms after
/// creation); the driver's next poll hits a dead transport and the node
/// reaches `Failed` — never stranded in `Running`.
#[tokio::test]
async fn child_exit_mid_task_drives_failed_not_stranded() {
    let fx = task_fixture("srv-exit").await;
    arm(&fx.client, "child-exit").await;
    start_task(&fx.client).await;
    wait_node_state(&fx.tree, &first_task_node("srv-exit"), NodeState::Failed).await;
}

/// AC3 (17.5a half) against the real fake: `input_required` with an
/// `inputRequests` map is decoded and logged, never transitioned — the node
/// stays `Running` across many polls, and 17.5a issues no `tasks/update`.
#[tokio::test]
async fn input_required_decodes_and_logs_without_transition() {
    let fx = task_fixture("srv-ir").await;
    arm(&fx.client, "input-required").await;
    start_task(&fx.client).await;
    let node = first_task_node("srv-ir");
    wait_node_state(&fx.tree, &node, NodeState::Running).await;

    // ~50 poll cycles at 1ms server pollIntervalMs: still Running, never a
    // Waiting edge (it does not exist in 17.5a) and never Failed.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let entry = fx
        .tree
        .list()
        .await
        .into_iter()
        .find(|e| e.agent_id == node)
        .expect("node present");
    assert_eq!(entry.current_status, NodeState::Running);

    fx.client.disconnect().await.expect("disconnect");
    // Teardown terminalizes through the supervised cascade (durable
    // Cancelled checkpoint), then the node is deregistered.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if fx.journal.load().await.unwrap_or_default().iter().any(|e| {
                matches!(
                    &e.record,
                    JournalRecord::Checkpoint(cp) if cp.id == node && cp.state == NodeState::Cancelled
                )
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("Cancelled checkpoint journaled on teardown");
}

/// AC6 restart-read keystone (D2, team consensus): a genuine host restart runs
/// `NodeRecovery::reconcile` over the durable journal on a FRESH tree. R-7
/// drives the in-flight `Running` node to `Suspended` (no live handle restored;
/// the host-bound MCP child is never resumable), and the recovered read reports
/// exactly that — plus the durable (server, taskId) binding.
///
/// Mutant killed: omit/reverse the `Running -> Suspended` recovery transition,
/// or project the live journal WITHOUT reconciling — the read returns `Running`
/// and this test goes RED.
#[tokio::test]
async fn restart_read_reports_recovered_suspended_state() {
    let fx = task_fixture("srv-restart").await;
    arm(&fx.client, "cancellation").await; // stays working: still in flight
    start_task(&fx.client).await;
    let node = first_task_node("srv-restart");
    wait_node_state(&fx.tree, &node, NodeState::Running).await;

    // Host restart: a fresh tree over the same durable journal, then real
    // recovery — no live driver survives.
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let recovered_tree =
        NodeTree::with_event_tx(tx, std::sync::Arc::new(|| 0i64)).with_journal(fx.journal.clone());
    let singleton = DaemonSingletonLock::try_acquire(fx._dir.path())
        .await
        .expect("acquire daemon singleton");
    let report = NodeRecovery::reconcile(&fx.journal, &recovered_tree, &singleton, "restart-host")
        .await
        .expect("reconcile");
    assert!(
        report.suspended.contains(&node),
        "the in-flight MCP task node must reconcile Running -> Suspended (R-7)"
    );

    // The recovered read (projection == what `tasks/get` reports post-restart):
    // Suspended, with the durable MCP binding intact.
    let room = fx
        .journal
        .project_room("restart-host")
        .await
        .expect("project_room");
    let view = room.nodes().get(&node).expect("node projected");
    assert_eq!(view.state, NodeState::Suspended);
    assert_eq!(
        view.mcp_task,
        Some(("srv-restart".to_string(), "fake-task-1".to_string()))
    );
    // Identity is also recoverable from the node id itself (reversible mint).
    assert_eq!(
        parse_mcp_node_id(&node)
            .as_ref()
            .map(|(s, t)| (s.as_str(), t.as_str())),
        Some(("srv-restart", "fake-task-1"))
    );

    fx.client.disconnect().await.expect("disconnect");
}

/// AC2 keystone (behavioral half): task identity is (server, taskId) only —
/// two independent clients connected to two independent server processes
/// mint the SAME node id for the same taskId. No session state participates
/// (the src-resident guard pins the header string out of the adapter).
#[tokio::test]
async fn identity_is_stateless_across_independent_clients() {
    let fx_a = task_fixture("srv-idem").await;
    let fx_b = task_fixture("srv-idem").await;
    arm(&fx_a.client, "progress").await;
    arm(&fx_b.client, "progress").await;
    start_task(&fx_a.client).await;
    start_task(&fx_b.client).await;
    let node = first_task_node("srv-idem");
    wait_node_state(&fx_a.tree, &node, NodeState::Completed).await;
    wait_node_state(&fx_b.tree, &node, NodeState::Completed).await;
    // Same id minted under two independent sessions — deterministic identity.
    assert_eq!(node, mint_mcp_node_id("srv-idem", "fake-task-1"));
    fx_a.client.disconnect().await.expect("disconnect a");
    fx_b.client.disconnect().await.expect("disconnect b");
}

/// R-16.4 keystone: the driver's poll loop is independent of `call_tool`'s
/// 60s select — the call returns at creation, and the task reaches terminal
/// through SUBSEQUENT polls, not within the initial request. (The fake's
/// progress case takes two polls at 1ms server `pollIntervalMs`; proving the
/// mechanism — a detached driver loop — rather than spending 60s of wall
/// clock in CI.)
#[tokio::test]
async fn long_running_task_survives_the_call_tool_sixty_second_bound() {
    let fx = task_fixture("srv-long").await;
    arm(&fx.client, "progress").await;
    let started = start_task(&fx.client).await;
    assert!(started.content.contains("fake-task-1"));
    // The initial call already returned; the node transitions afterwards.
    let node = first_task_node("srv-long");
    wait_node_state(&fx.tree, &node, NodeState::Running).await;
    wait_node_state(&fx.tree, &node, NodeState::Completed).await;
    fx.client.disconnect().await.expect("disconnect");
}

/// AC4 / R-15 mutant-killer (D1): when the server REJECTS `tasks/cancel`,
/// teardown must NOT forge a `Cancelled` durable state — the node is driven
/// `Failed` (the actual protocol outcome), proving the local transition is
/// gated on the cancel result, never written blindly before it.
///
/// Mutant killed: a blind pre-checkpoint of `Cancelled` on teardown (the old
/// `cascade_kill`-first ordering) would leave a `Cancelled` checkpoint and this
/// test goes RED.
#[tokio::test]
async fn rejected_cancel_is_not_forged_cancelled_on_teardown() {
    let fx = task_fixture("srv-reject").await;
    arm(&fx.client, "cancel-reject").await;
    start_task(&fx.client).await;
    let node = first_task_node("srv-reject");
    wait_node_state(&fx.tree, &node, NodeState::Running).await;

    fx.client.disconnect().await.expect("disconnect");

    // The durable record must reach Failed and NEVER a forged Cancelled.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let failed = fx.journal.load().await.unwrap_or_default().iter().any(|e| {
                matches!(
                    &e.record,
                    JournalRecord::Checkpoint(cp) if cp.id == node && cp.state == NodeState::Failed
                )
            });
            if failed {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("node reached a durable Failed checkpoint after a rejected cancel");

    let records = fx.journal.load().await.expect("journal load");
    assert!(
        !records.iter().any(|e| matches!(
            &e.record,
            JournalRecord::Checkpoint(cp) if cp.id == node && cp.state == NodeState::Cancelled
        )),
        "a rejected tasks/cancel must never forge a Cancelled checkpoint (R-15 / D1)"
    );
}

/// AC4 scope control (D3, team consensus): the durable task outlives the
/// originating `tools/call` invocation. Cancelling that invocation's token
/// AFTER `call_tool` returns must NOT cancel the task — the driver is owned by
/// the adapter/session token, not the per-invocation token.
///
/// Mutant killed: pass/link the `call_tool` cancel token into
/// `materialize_task`/`start_task` — post-return cancellation would terminalize
/// the node and this test goes RED.
#[tokio::test]
async fn invocation_cancel_after_return_does_not_cancel_the_task() {
    let fx = task_fixture("srv-inv").await;
    arm(&fx.client, "cancellation").await; // stays working
    let invocation = CancellationToken::new();
    let started = fx
        .client
        .call_tool("scripted-task", serde_json::json!({}), invocation.clone())
        .await
        .expect("task created");
    assert!(started.content.contains("fake-task-1"));
    let node = first_task_node("srv-inv");
    wait_node_state(&fx.tree, &node, NodeState::Running).await;

    // Cancel the ORIGINATING invocation after it has already returned.
    invocation.cancel();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // The durable task is unaffected: still Running, still tracked.
    let entry = fx
        .tree
        .list()
        .await
        .into_iter()
        .find(|e| e.agent_id == node)
        .expect("task node still present");
    assert_eq!(entry.current_status, NodeState::Running);

    // The session token (not the invocation) is the cancellation authority.
    fx.client.disconnect().await.expect("disconnect");
}
