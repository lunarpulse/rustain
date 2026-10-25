//! `K05` (AC1 + AC5 + AC7 end to end, M02) — the model rail against a real
//! rustain peer whose operator must decide (`ModePolicy(NotifyAndWait)`).
//! B answers `auth-required`; the rail names it (AC5, never the AC2
//! `requested input` park), cancels B's task over HTTP, and B's parked card
//! settles with `PARKED_CANCELLED_NOTICE`.
use super::*;

const NOTICE: &str =
    "[cancelled] This request was cancelled before you answered; no response was sent.";

/// The model rail, composed as production composes it (copied from
/// `conformance_19_18c_a2a_item_capability.rs::model_rail`), with the node
/// tree kept so the delegation node can be observed.
async fn model_rail(
    peers: Vec<A2aPeerSpec>,
) -> (A2aEgress, Journal, Arc<tempfile::TempDir>, NodeTree) {
    let journal = journal().await;
    let keys = Arc::new(tempfile::tempdir().expect("keys"));
    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
    let tree = NodeTree::new();
    let egress = A2aEgress::compose(
        peers,
        tree.clone(),
        journal.room.clone(),
        journal.node.clone(),
        event_tx,
        rustain::adapters::a2a::item_capability::load_item_capability_key(Ok(keys
            .path()
            .to_path_buf())),
    )
    .expect("compose");
    tokio::time::timeout(SETTLE, egress.await_cards_settled())
        .await
        .expect("cards settle");
    (egress, journal, keys, tree)
}

fn invoke(egress: &A2aEgress, peer: &str) -> tokio::task::JoinHandle<Result<String, String>> {
    use rustain::domain::ports::CapabilityProvider;
    let provider = egress.provider().clone();
    let id = rustain::domain::models::CapabilityId {
        protocol: "a2a".to_owned(),
        server: peer.to_owned(),
        tool: "delegate".to_owned(),
    };
    tokio::spawn(async move {
        provider
            .invoke(
                &id,
                serde_json::json!({ "message": "delegated" }),
                CancellationToken::new(),
            )
            .await
            .map(|result| result.content)
            .map_err(|error| error.to_string())
    })
}

#[tokio::test]
async fn k05_auth_required_is_named_cancelled_and_settles_the_peer_card() {
    let mut b = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    // A live writer, so B's turn forwarder runs and the card is persisted.
    let (_operator, shutdown) = attach_operator(&mut b).await;
    let (egress, journal, keys, tree) = model_rail(vec![b.peer("b")]).await;

    let outcome = tokio::time::timeout(SETTLE, invoke(&egress, "b"))
        .await
        .expect("delegation settles")
        .expect("join");
    let error = outcome.expect_err("auth-required stops the rail");

    // The original task, from A's own durable dispatch row.
    let rows = room_events(&journal).await;
    let task = rows
        .iter()
        .find_map(|(_, row)| match row {
            RoomEvent::RemoteEnvelopeDispatched {
                task: Some(task), ..
            } => Some(task.clone()),
            _ => None,
        })
        .expect("the model rail's dispatch row");

    // AC5: named, not the AC2 park.
    assert!(
        error.contains(&format!(
            "remote agent requires authorization or a decision on its side that this delegation cannot provide (task {task} cancelled)"
        )),
        "{error}"
    );
    assert!(!error.contains("requested input"), "{error}");
    // This structural counter exists only in the instrumented CI lane;
    // the HTTP/node/journal behavior below runs with plain `a2a` as well.
    #[cfg(feature = "test-instrumentation")]
    assert_eq!(
        egress.runtime().parked_count(),
        0,
        "auth-required never parks"
    );

    // AC1: the cancel was served; B's task is `canceled` over real HTTP.
    let served = poll_state(&b.endpoint, &task, "canceled").await;
    assert_eq!(served["result"]["status"]["state"], "canceled", "{served}");
    let context = served["result"]["contextId"]
        .as_str()
        .map_or_else(|| "—".to_owned(), str::to_owned);

    // AC5 journal: exactly one outbound authorization rejection, sanitized
    // detail, correlated to the task.
    let rejections: Vec<_> = rows
        .iter()
        .filter_map(|(_, row)| match row {
            RoomEvent::RemoteEnvelopeRejected {
                reason: rustain::domain::models::RejectReason::Policy { detail },
                direction,
                task,
                ..
            } => Some((detail.clone(), *direction, task.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        rejections,
        vec![(
            format!(
                "remote agent requires authorization (task {task}, context {context}); cancelled"
            ),
            rustain::domain::models::Direction::Outbound,
            Some(task.clone()),
        )],
        "{rows:?}"
    );

    // The delegation node failed; nothing waits on A's side.
    let nodes = tree.list().await;
    assert!(
        nodes
            .iter()
            .any(|entry| entry.current_status == rustain::domain::models::NodeState::Failed),
        "{nodes:?}"
    );
    assert!(
        !nodes
            .iter()
            .any(|entry| entry.current_status == rustain::domain::models::NodeState::Waiting),
        "{nodes:?}"
    );

    // AC7: B's persisted row carries the notice and is Settled, not resolvable.
    let storage = FileSystemStorage::with_workspace_root(
        rustain::infrastructure::paths::sessions_dir(&b.journal.path),
        b.journal.path.clone(),
    );
    let conversation = storage
        .load_conversation(CONVERSATION_ID)
        .await
        .expect("load B conversation")
        .expect("B conversation persisted");
    let row = conversation
        .messages
        .iter()
        .find(|message| message.id.starts_with("peer-response-"))
        .expect("B persisted the peer card");
    assert_eq!(row.content, NOTICE);
    let node = row
        .id
        .strip_prefix("peer-response-")
        .expect("row prefix")
        .to_owned();
    assert_eq!(persisted_card(&b, &node).await.as_deref(), Some(NOTICE));
    let control = conversation
        .peer_row_controls
        .get(&row.id)
        .expect("B row control");
    assert_eq!(
        serde_json::to_value(control.stage).expect("stage"),
        serde_json::json!("settled")
    );
    assert!(!control.node_resolvable, "a settled card is not answerable");

    // B's inbound node is deregistered (structural wait, no sleep).
    let deadline = tokio::time::Instant::now() + SETTLE;
    while b
        .node_tree
        .list()
        .await
        .iter()
        .any(|entry| entry.agent_id.as_str() == node)
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "B's parked node was never deregistered"
        );
        tokio::task::yield_now().await;
    }

    shutdown.cancel();
    b.cancel.cancel();
    drop(egress);
    drop(journal);
    drop(keys);
}
