//! Story 19.18b `K02` — the model answers a parked question on the same task.
//!
//! Front door and helpers: `b_k01_model_park` (real `A2aEgress::compose` →
//! `provider().invoke`, a `wiremock` third-party agent). Kills `M03` (resume
//! keeps the entry: `parked_count()` and the exact reuse error), `M04` (no
//! peer check: the cross-peer leg) and `M05` (no `taskId`: the request body).

#![cfg(feature = "test-instrumentation")]

use rustain::domain::models::{Direction, NodeState, WaitReason};
use serde_json::{Value, json};

use super::b_k01_model_park::{
    Answer, CONTEXT, ITEM_CAPABILITY, ModelRail, NEEDS_MESSAGE, OTHER, OTHER_PEER, PEER, QUESTION,
    REPLY_CAPABILITY, SKILL, ScriptedPeer, TaskIds, UNKNOWN, answer, assert_handle, assert_refused,
    dispatched, model_rail, only_rows, park, parse_content, question_content, quiescence, refusal,
    rejected, result_hash, task_body,
};
use super::*;

const SECOND_QUESTION: &str = "Should the replica be paused during the migration?";

/// The dispatch row's endpoint for this rail's first send (an unpinned peer's
/// origin digest); every later dispatch to the same peer repeats it.
async fn first_endpoint(rail: &ModelRail) -> Option<String> {
    match rail.rows().await.into_iter().next() {
        Some((_, RoomEvent::RemoteEnvelopeDispatched { endpoint, .. })) => endpoint,
        other => panic!("the first row is the dispatch: {other:?}"),
    }
}

/// The continuation carriers a resumed `message/send` must hold, and nothing
/// the model controls beyond its text.
fn assert_continuation(resumed: &Value, first: &Value, task: &str, handle: &str, text: &str) {
    let message = &resumed["params"]["message"];
    assert_eq!(
        message["taskId"], task,
        "AC3: the answer names T: {resumed}"
    );
    assert_eq!(
        message["contextId"], CONTEXT,
        "AC3: and its context: {resumed}"
    );
    let fresh = message["messageId"].as_str().expect("messageId");
    assert!(
        fresh.starts_with("rustain-"),
        "a host-minted message id: {fresh}"
    );
    assert_ne!(
        fresh, first["params"]["message"]["messageId"],
        "the answer is a new message"
    );
    assert_eq!(message["role"], "user");
    assert_eq!(message["parts"], json!([{ "kind": "text", "text": text }]));
    let wire = serde_json::to_string(resumed).expect("serialize");
    assert!(
        !wire.contains(handle),
        "the handle never reaches the wire: {wire}"
    );
    assert!(
        message.get("continue").is_none() && resumed["params"].get("continue").is_none(),
        "`continue` is stripped before build_message: {wire}"
    );
}

/// Park, answer `main`, and finish on the same task. The reply capability is
/// presented iff `T` is this host's own bound message id (the `task_params`
/// rule), and the parked transport keeps polling `T` with the same binding.
async fn resume_round_trip(ids: TaskIds) {
    let peer = ScriptedPeer::start(PEER, ids).await;
    peer.script(
        "message/send",
        [Answer::State("working"), Answer::State("working")],
    );
    peer.script(
        "tasks/get",
        [Answer::Ask(QUESTION), Answer::State("completed")],
    );
    let rail = model_rail(vec![peer.spec()]).await;
    let handle = park(&rail, &peer, QUESTION).await;
    let task = peer.task_id();
    let node = rail.peer_node().await;
    let endpoint = first_endpoint(&rail).await;
    let rows_before = rail.rows().await.len();

    let content = rail
        .invoke(PEER, answer(&handle, "main"))
        .await
        .expect("the answered task completes");

    // The wire: send, poll, answer on T, poll T.
    assert_eq!(
        peer.methods(),
        ["message/send", "tasks/get", "message/send", "tasks/get"]
    );
    let requests = peer.requests();
    let (first, resumed) = (&requests[0], &requests[2]);
    assert_continuation(resumed, first, &task, &handle, "main");
    let own = first["params"]["message"]["metadata"][ITEM_CAPABILITY].clone();
    assert!(
        own.is_string(),
        "the first send was capability-bearing: {first}"
    );
    let presented = resumed["params"]["message"]["metadata"].get(REPLY_CAPABILITY);
    let poll = &requests[3]["params"];
    match ids {
        TaskIds::Minted => {
            assert_eq!(
                presented, None,
                "T is not this host's message id: {resumed}"
            );
            assert_eq!(*poll, json!({ "id": task }));
        }
        TaskIds::OfMessage => {
            assert_eq!(
                presented,
                Some(&own),
                "T is the bound message id: {resumed}"
            );
            assert_eq!(
                *poll,
                json!({ "id": task, "metadata": { ITEM_CAPABILITY: own } })
            );
        }
    }
    assert!(
        peer.requests_for("tasks/cancel").is_empty(),
        "a completed task is never cancelled"
    );

    // The tool returns the remote result, the node completes, the handle is spent.
    let completed = task_body(&task, "completed", None);
    assert_eq!(parse_content(&content), completed);
    let after = rail.peer_node().await;
    assert_eq!(
        after.agent_id, node.agent_id,
        "the answer continues the parked node"
    );
    assert_eq!(after.current_status, NodeState::Completed);
    assert_eq!(
        rail.parked(),
        0,
        "M03: a resumed handle leaves the registry"
    );
    let rows = only_rows(&rail.rows().await[rows_before..]);
    assert_eq!(
        rows,
        vec![
            dispatched(&peer.spec(), resumed, endpoint),
            RoomEvent::RemoteEnvelopeAccepted {
                rail: Some(rustain::domain::models::PeerRail::A2a),
                peer: peer.spec().resolved_identity(),
                node: node.agent_id.clone(),
                content_hash: result_hash(&completed),
                direction: Direction::Outbound,
                task: Some(task.clone()),
            },
        ]
    );

    // Reuse: the exact unknown/used refusal, and nothing moves.
    let before = quiescence(&rail, &[&peer]).await;
    let reused = rail.invoke(PEER, answer(&handle, "again")).await;
    assert_eq!(reused, Err(refusal(PEER, UNKNOWN)), "M03");
    assert_eq!(
        quiescence(&rail, &[&peer]).await,
        before,
        "a refusal touches nothing"
    );
}

/// `K02` — a third-party agent's own task id: `taskId`/`contextId` on the
/// wire, no reply capability, `Completed`, `RemoteEnvelopeAccepted`, the handle
/// spent (`M03`, `M05`).
#[tokio::test]
async fn k02_resume_answers_a_third_party_task_on_the_same_task() {
    resume_round_trip(TaskIds::Minted).await;
}

/// `K02` — when `T` is this host's own message id, the answer presents the
/// parked own capability as its reply capability (AC3's `task_params` rule).
#[tokio::test]
async fn k02_resume_presents_the_bound_reply_capability_for_its_own_task() {
    resume_round_trip(TaskIds::OfMessage).await;
}

/// `K02` controls — every refusal on a live handle makes zero requests, writes
/// zero rows and leaves the node untouched; another peer's tool and a missing
/// message never consume it (`M04`); the same handle then resumes, and a
/// second question mints a different handle.
#[tokio::test]
async fn k02_refusals_spend_nothing_and_the_live_handle_still_resumes() {
    let peer = ScriptedPeer::start(PEER, TaskIds::Minted).await;
    let other = ScriptedPeer::start(OTHER, TaskIds::Minted).await;
    peer.script(
        "message/send",
        [
            Answer::State("working"),
            Answer::State("working"),
            Answer::State("working"),
        ],
    );
    peer.script(
        "tasks/get",
        [
            Answer::Ask(QUESTION),
            Answer::Ask(SECOND_QUESTION),
            Answer::State("completed"),
        ],
    );
    let rail = model_rail(vec![peer.spec(), other.spec()]).await;

    // AC3: the model sees an optional, described `continue`.
    {
        use rustain::domain::ports::CapabilityProvider;
        let tools = rail.egress.provider().discover().await.expect("discover");
        let tool = tools
            .iter()
            .find(|tool| tool.id.server == PEER && tool.id.tool == SKILL)
            .expect("the clarifier tool");
        let schema = &tool.input_schema;
        assert_eq!(schema["properties"]["continue"]["type"], "string");
        assert!(
            schema["properties"]["continue"]["description"]
                .as_str()
                .is_some_and(|text| !text.is_empty()),
            "{schema}"
        );
        assert_eq!(
            schema["required"],
            json!(["message"]),
            "continue stays optional"
        );
        assert_eq!(schema["additionalProperties"], false);
    }

    let handle = park(&rail, &peer, QUESTION).await;
    let node = rail.peer_node().await;
    let unknown = "a2a-continue-0000000000000000";
    let controls = [
        (OTHER, answer(&handle, "main"), OTHER_PEER),
        (PEER, json!({ "continue": handle }), NEEDS_MESSAGE),
        (PEER, answer(&handle, ""), NEEDS_MESSAGE),
        (
            PEER,
            json!({ "continue": handle, "message": 7 }),
            NEEDS_MESSAGE,
        ),
        (
            PEER,
            answer("", "main"),
            "continue must be a non-empty string",
        ),
        (
            PEER,
            json!({ "continue": 7, "message": "main" }),
            "continue must be a non-empty string",
        ),
        (PEER, answer(unknown, "main"), UNKNOWN),
        (OTHER, answer(unknown, "main"), UNKNOWN),
    ];
    let before = quiescence(&rail, &[&peer, &other]).await;
    for (target, input, reason) in controls {
        let refused = rail.invoke(target, input.clone()).await;
        assert_eq!(refused, Err(refusal(target, reason)), "{input}");
        assert_eq!(
            quiescence(&rail, &[&peer, &other]).await,
            before,
            "zero requests, rows and node mutation, and the handle stays parked: {input}"
        );
    }

    // The same handle still resumes; the peer asks again → a NEW handle.
    let content = rail
        .invoke(PEER, answer(&handle, "main"))
        .await
        .expect("the unconsumed handle resumes");
    let second = parse_content(&content)["continue"]
        .as_str()
        .expect("a second handle")
        .to_owned();
    assert_handle(&second);
    assert_ne!(second, handle, "a new park mints a new handle");
    assert_eq!(
        parse_content(&content),
        question_content(&second, &peer.task_id(), SECOND_QUESTION)
    );
    let requests = peer.requests();
    assert_continuation(&requests[2], &requests[0], &peer.task_id(), &handle, "main");
    let parked = rail.peer_node().await;
    assert_eq!(parked.agent_id, node.agent_id);
    assert_eq!(parked.current_status, NodeState::Waiting);
    assert_eq!(parked.wait_reason, Some(WaitReason::AwaitingHumanInput));
    assert_eq!(rail.parked(), 1, "the old entry left, the new one parked");
    assert!(
        other.requests().is_empty(),
        "the other peer was never contacted"
    );

    // The spent first handle and the other peer's tool refuse the second one.
    let before = quiescence(&rail, &[&peer, &other]).await;
    for (target, input, reason) in [
        (PEER, answer(&handle, "again"), UNKNOWN),
        (OTHER, answer(&second, "yes"), OTHER_PEER),
    ] {
        assert_eq!(
            rail.invoke(target, input).await,
            Err(refusal(target, reason))
        );
        assert_eq!(quiescence(&rail, &[&peer, &other]).await, before);
    }

    let done = rail
        .invoke(PEER, answer(&second, "yes"))
        .await
        .expect("the second answer completes");
    assert_eq!(
        parse_content(&done),
        task_body(&peer.task_id(), "completed", None)
    );
    let requests = peer.requests();
    assert_continuation(&requests[4], &requests[0], &peer.task_id(), &second, "yes");
    assert_eq!(rail.peer_node().await.current_status, NodeState::Completed);
    assert_eq!(rail.parked(), 0);
    assert!(peer.requests_for("tasks/cancel").is_empty());
}

/// `K02` (AC3) — a JSON-RPC error answer to the continuation fails the node
/// (`Running → Failed`), journals a sanitized `RemoteEnvelopeRejected`, sends
/// the error to the model and spends the handle.
#[tokio::test]
async fn k02_a_json_rpc_error_answer_fails_the_node_and_reaches_the_model() {
    const HOSTILE: &str = "Task is \u{1b}[31mterminal\nnow";
    let peer = ScriptedPeer::start(PEER, TaskIds::Minted).await;
    peer.script(
        "message/send",
        [Answer::State("working"), Answer::RpcError(-32004, HOSTILE)],
    );
    peer.script("tasks/get", [Answer::Ask(QUESTION)]);
    let mut rail = model_rail(vec![peer.spec()]).await;
    let handle = park(&rail, &peer, QUESTION).await;
    let node = rail.peer_node().await;
    let endpoint = first_endpoint(&rail).await;
    let rows_before = rail.rows().await.len();

    let error = rail
        .invoke(PEER, answer(&handle, "main"))
        .await
        .expect_err("a JSON-RPC error answer is a tool error");
    assert!(
        error.starts_with(&refusal(
            PEER,
            "A2A transport failure: A2A JSON-RPC error -32004: "
        )),
        "the peer's error reaches the model: {error:?}"
    );
    let envelope = rail.await_refused().await;
    assert_refused(&envelope, &node, &peer.task_id());

    assert_eq!(
        peer.methods(),
        ["message/send", "tasks/get", "message/send"]
    );
    let requests = peer.requests();
    assert_continuation(&requests[2], &requests[0], &peer.task_id(), &handle, "main");
    let failed = rail.peer_node().await;
    assert_eq!(failed.agent_id, node.agent_id);
    assert_eq!(failed.current_status, NodeState::Failed);
    assert_eq!(rail.parked(), 0, "the claimed handle is spent");
    assert_eq!(
        only_rows(&rail.rows().await[rows_before..]),
        vec![
            dispatched(&peer.spec(), &requests[2], endpoint),
            rejected(
                &peer.spec(),
                &peer.task_id(),
                "A2A transport failure: A2A JSON-RPC error -32004: Task is [31mterminalnow",
            ),
        ],
        "the rejection detail is sanitized: no control bytes, one line"
    );

    let before = quiescence(&rail, &[&peer]).await;
    assert_eq!(
        rail.invoke(PEER, answer(&handle, "again")).await,
        Err(refusal(PEER, UNKNOWN))
    );
    assert_eq!(quiescence(&rail, &[&peer]).await, before);
}
