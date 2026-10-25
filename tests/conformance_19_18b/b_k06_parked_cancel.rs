//! Both parked inbound modes keep a real owner and serialize cancellation with answers.
use super::*;
use rustain::domain::models::{NodeState, PeerRowStage};
use rustain::infrastructure::runtime::attach_loop::apply_peer_row_operation;
use serde_json::json;

const NOTICE: &str =
    "[cancelled] This request was cancelled before you answered; no response was sent.";
const TASK: &str = "parked-response";

async fn live_stage(
    operator: &mut tokio::net::UnixStream,
    live: &mut Conversation,
    node: &str,
    stage: PeerRowStage,
) {
    let row_id = format!("peer-response-{node}");
    tokio::time::timeout(SETTLE, async {
        loop {
            if live
                .peer_row_controls
                .get(&row_id)
                .is_some_and(|control| control.stage == stage)
            {
                return;
            }
            if let Some(DaemonFrame::PeerRowOperation(operation)) =
                read_frame::<_, DaemonFrame>(operator).await.unwrap()
            {
                apply_peer_row_operation(live, (*operation).clone());
            }
        }
    })
    .await
    .expect("committed live row operation");
}

async fn seeded(
    mode: ResponseMode,
) -> (
    Host,
    tokio::net::UnixStream,
    CancellationToken,
    String,
    Conversation,
) {
    let mut host = Host::serve(A2aAdmissionPolicy::Allow, mode).await;
    let (mut operator, shutdown) = attach_operator(&mut host).await;
    let answer = rpc(&host.endpoint, json!({
        "jsonrpc":"2.0", "id":1, "method":"message/send",
        "params":{"message":{"kind":"message", "messageId":TASK, "role":"user", "parts":[{"kind":"text", "text":"question"}]}}
    })).await;
    assert_eq!(answer["result"]["id"], TASK, "{answer}");
    let node = tokio::time::timeout(SETTLE, async {
        loop {
            if let Some(node) = host
                .node_tree
                .list()
                .await
                .into_iter()
                .find(|node| node.current_status == NodeState::Waiting)
            {
                return node.agent_id.as_str().to_owned();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("real parked inbound node");
    let mut live = Conversation::default();
    live_stage(
        &mut operator,
        &mut live,
        &node,
        if mode == ResponseMode::NotifyAndWait {
            PeerRowStage::AwaitingOperator
        } else {
            PeerRowStage::Ready
        },
    )
    .await;
    assert!(live.peer_row_controls[&format!("peer-response-{node}")].node_resolvable);
    (host, operator, shutdown, node, live)
}

async fn cancel(host: &Host) -> serde_json::Value {
    rpc(
        &host.endpoint,
        json!({"jsonrpc":"2.0","id":2,"method":"tasks/cancel","params":{"id":TASK}}),
    )
    .await
}

async fn assert_settled(
    host: &Host,
    operator: &mut tokio::net::UnixStream,
    live: &mut Conversation,
    node: &str,
) {
    assert_eq!(persisted_card(host, node).await.as_deref(), Some(NOTICE));
    live_stage(operator, live, node, PeerRowStage::Settled).await;
    let row_id = format!("peer-response-{node}");
    assert_eq!(
        live.messages
            .iter()
            .find(|row| row.id == row_id)
            .unwrap()
            .content,
        NOTICE
    );
    assert!(!live.peer_row_controls[&row_id].node_resolvable);
    assert!(
        !live.peer_row_controls.values().any(|control| matches!(
            control.stage,
            PeerRowStage::AwaitingOperator | PeerRowStage::Ready | PeerRowStage::Drafting
        )),
        "no pending control remains on the attached client"
    );
    tokio::time::timeout(SETTLE, async {
        while host
            .node_tree
            .list()
            .await
            .iter()
            .any(|entry| entry.agent_id.as_str() == node)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("settled node deregistered");
    assert!(
        replies(&room_events(&host.journal).await).is_empty(),
        "cancellation cannot author a reply turn"
    );
}

#[tokio::test]
async fn notify_and_wait_obeys_tasks_cancel() {
    for mode in [ResponseMode::NotifyAndWait, ResponseMode::NotifyAndDraft] {
        let (host, mut operator, shutdown, node, mut live) = seeded(mode).await;
        let answer = cancel(&host).await;
        assert_eq!(
            answer["result"]["status"]["state"], "canceled",
            "parked owner cancellation must settle: {answer}"
        );
        assert_settled(&host, &mut operator, &mut live, &node).await;
        shutdown.cancel();
        host.cancel.cancel();
    }
}

#[tokio::test]
async fn local_kill_settles_an_already_cancelled_wait_or_draft() {
    for mode in [ResponseMode::NotifyAndWait, ResponseMode::NotifyAndDraft] {
        let (host, mut operator, shutdown, node, mut live) = seeded(mode).await;
        let node_id = rustain::domain::models::AgentId::parse(&node).unwrap();
        let status = host.node_tree.status_rx(&node_id).await.unwrap();
        host.node_tree
            .cascade_kill(&node_id, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(*status.borrow(), NodeState::Cancelled);
        // The status checkpoint can precede the persistence commit; wait on the
        // actual live row operation, not a timer or the already-terminal node.
        live_stage(&mut operator, &mut live, &node, PeerRowStage::Settled).await;
        assert_settled(&host, &mut operator, &mut live, &node).await;
        let answer = poll_state(&host.endpoint, TASK, "canceled").await;
        assert_eq!(answer["result"]["status"]["state"], "canceled", "{answer}");
        assert_eq!(*status.borrow(), NodeState::Cancelled);
        shutdown.cancel();
        host.cancel.cancel();
    }
}

#[tokio::test]
async fn resolving_first_preserves_the_completed_answer() {
    for mode in [ResponseMode::NotifyAndWait, ResponseMode::NotifyAndDraft] {
        let (host, mut operator, shutdown, node, _live) = seeded(mode).await;
        let resolution = resolve(
            &mut operator,
            &node,
            PeerDraftAction::WriteOwn {
                content: "human answer".into(),
            },
        )
        .await;
        assert!(
            matches!(
                resolution,
                DaemonFrame::RecipientActionResult {
                    outcome: RecipientActionOutcome::Applied { reply: Some(_) },
                    ..
                }
            ),
            "{resolution:?}"
        );
        let completed = poll_state(&host.endpoint, TASK, "completed").await;
        assert_eq!(
            completed["result"]["status"]["state"], "completed",
            "{completed}"
        );
        let before = room_events(&host.journal).await;
        assert_eq!(replies(&before).len(), 1);
        let answer = cancel(&host).await;
        assert_eq!(answer["result"]["status"]["state"], "completed", "{answer}");
        assert_eq!(
            answer["result"]["artifacts"],
            completed["result"]["artifacts"]
        );
        assert_eq!(replies(&room_events(&host.journal).await), replies(&before));
        assert_ne!(persisted_card(&host, &node).await.as_deref(), Some(NOTICE));
        shutdown.cancel();
        host.cancel.cancel();
    }
}

#[tokio::test]
async fn cancelling_first_makes_later_resolution_a_settled_row_noop() {
    for mode in [ResponseMode::NotifyAndWait, ResponseMode::NotifyAndDraft] {
        let (host, mut operator, shutdown, node, mut live) = seeded(mode).await;
        let answer = cancel(&host).await;
        assert_eq!(answer["result"]["status"]["state"], "canceled", "{answer}");
        assert_settled(&host, &mut operator, &mut live, &node).await;
        let before = room_events(&host.journal).await.len();
        let resolution = resolve(
            &mut operator,
            &node,
            PeerDraftAction::WriteOwn {
                content: "too late".into(),
            },
        )
        .await;
        assert!(
            matches!(
                resolution,
                DaemonFrame::RecipientActionResult {
                    outcome: RecipientActionOutcome::Idempotent,
                    ..
                }
            ),
            "{resolution:?}"
        );
        assert_eq!(room_events(&host.journal).await.len(), before);
        assert!(replies(&room_events(&host.journal).await).is_empty());
        assert_eq!(persisted_card(&host, &node).await.as_deref(), Some(NOTICE));
        shutdown.cancel();
        host.cancel.cancel();
    }
}
