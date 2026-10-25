//! Story 19.18b `K03` — a parked continuation obeys a real node kill.
//!
//! The kill is production's `NodeTree::cascade_kill` on the kept tree clone:
//! it checkpoints the node `Cancelled` first, then fires the node token and
//! `Op::Kill` (D3). ⛔ No token is cancelled directly. Waits are structural:
//! the peer's request log and the runtime's `MessageRefused` receipt, which
//! follows the durable settlement row. Kills `M06` (the parked owner reacts to
//! neither the node token nor `Op::Kill`: no `tasks/cancel` is observed).

#![cfg(feature = "test-instrumentation")]

use rustain::domain::models::NodeState;
use serde_json::json;

use super::b_k01_model_park::{
    Answer, HTTP_CANARY, ITEM_CAPABILITY, ModelRail, PEER, QUESTION, ScriptedPeer, TaskIds,
    UNKNOWN, answer, assert_refused, model_rail, only_rows, park, quiescence, refusal, rejected,
};
use super::*;

const CONFIRMED: &str = "peer reported terminal state canceled";

/// Park on `peer`, kill the node through the tree, and wait for the peer's
/// `tasks/cancel` and the settlement receipt. Returns the handle and the
/// rows written by the settlement.
async fn park_then_kill(rail: &mut ModelRail, peer: &ScriptedPeer) -> (String, Vec<RoomEvent>) {
    let handle = park(rail, peer, QUESTION).await;
    let node = rail.peer_node().await;
    assert_eq!(node.current_status, NodeState::Waiting);
    assert_eq!(rail.parked(), 1);
    // `cascade_kill` deregisters the node: keep its real status receiver.
    let status = rail
        .tree
        .status_rx(&node.agent_id)
        .await
        .expect("the parked node's status channel");
    let rows_before = rail.rows().await.len();

    let killed = rail
        .tree
        .cascade_kill(&node.agent_id, SETTLE)
        .await
        .expect("the kill settles");
    assert_eq!(killed, vec![node.agent_id.clone()]);

    peer.await_requests("tasks/cancel", 1).await;
    let envelope = rail.await_refused().await;
    assert_refused(&envelope, &node, &peer.task_id());

    assert_eq!(
        *status.borrow(),
        NodeState::Cancelled,
        "the checkpointed Cancelled stands; never back through Running/Waiting"
    );
    assert!(
        rail.peer_nodes().await.is_empty(),
        "the killed node is deregistered"
    );
    assert_eq!(rail.parked(), 0, "the kill removed the parked entry");
    assert_eq!(
        peer.methods(),
        ["message/send", "tasks/get", "tasks/cancel"],
        "exactly one remote cancel, and no further poll"
    );
    let settled = only_rows(&rail.rows().await[rows_before..]);
    (handle, settled)
}

/// After a kill the handle is refused exactly, with zero requests, rows or
/// node mutation.
async fn assert_late_refusal(rail: &ModelRail, peer: &ScriptedPeer, handle: &str) {
    let before = quiescence(rail, &[peer]).await;
    assert_eq!(
        rail.invoke(PEER, answer(handle, "too late")).await,
        Err(refusal(PEER, UNKNOWN))
    );
    assert_eq!(quiescence(rail, &[peer]).await, before);
}

/// `K03` — `cascade_kill` on a parked node: the peer receives `tasks/cancel(T)`
/// with the host's own capability (`T` is its bound message id), the node stays
/// `Cancelled`, the confirmed Canceled row is journaled, and the handle is
/// refused afterwards.
#[tokio::test]
async fn k03_cascade_kill_cancels_the_parked_remote_task() {
    let peer = ScriptedPeer::start(PEER, TaskIds::OfMessage).await;
    peer.script("message/send", [Answer::State("working")]);
    peer.script("tasks/get", [Answer::Ask(QUESTION)]);
    peer.script("tasks/cancel", [Answer::State("canceled")]);
    let mut rail = model_rail(vec![peer.spec()]).await;

    let (handle, settled) = park_then_kill(&mut rail, &peer).await;

    let task = peer.task_id();
    let own = peer.requests()[0]["params"]["message"]["metadata"][ITEM_CAPABILITY].clone();
    assert!(own.is_string(), "the delegated send was capability-bearing");
    assert_eq!(
        peer.requests_for("tasks/cancel")[0]["params"],
        json!({ "id": task, "metadata": { ITEM_CAPABILITY: own } }),
        "AC4: tasks/cancel(T) with the own capability"
    );
    assert_eq!(
        settled,
        vec![rejected(&peer.spec(), &task, CONFIRMED)],
        "the existing Canceled row shape, and nothing else"
    );
    assert_late_refusal(&rail, &peer, &handle).await;
}

/// `K03` leg 2 — the peer's cancel answers HTTP 500: the node is still
/// `Cancelled`, the journal says the remote cancel was NOT confirmed
/// (sanitized, no peer body), and no Canceled row claims otherwise.
#[tokio::test]
async fn k03_an_unconfirmed_remote_cancel_is_never_claimed() {
    let peer = ScriptedPeer::start(PEER, TaskIds::Minted).await;
    peer.script("message/send", [Answer::State("working")]);
    peer.script("tasks/get", [Answer::Ask(QUESTION)]);
    peer.script("tasks/cancel", [Answer::Http(500)]);
    let mut rail = model_rail(vec![peer.spec()]).await;

    let (handle, settled) = park_then_kill(&mut rail, &peer).await;

    let task = peer.task_id();
    assert_eq!(
        peer.requests_for("tasks/cancel")[0]["params"],
        json!({ "id": task }),
        "a minted id is not this host's message id: no capability"
    );
    assert_eq!(
        settled,
        vec![rejected(
            &peer.spec(),
            &task,
            "remote task cancel was not confirmed: A2A peer returned HTTP 500",
        )]
    );
    let all = format!("{:?}", rail.rows().await);
    assert!(
        !all.contains(CONFIRMED),
        "never claim a remote cancel: {all}"
    );
    assert!(
        !all.contains(HTTP_CANARY),
        "the peer's body never reaches the journal"
    );
    assert_late_refusal(&rail, &peer, &handle).await;
}
