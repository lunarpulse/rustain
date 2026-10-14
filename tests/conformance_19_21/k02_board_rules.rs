use crate::harness::{
    GateRoomJournal, Host, Sender, acknowledge_received_item, folded_actions, read_room_events,
};
use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::tui::handlers::team_command::TeamBoardSelector;
use rustain::adapters::tui::state::TuiState;
use rustain::domain::models::{ResponseMode, RoomEvent};
use rustain::infrastructure::runtime::transparency_bridge::team_board_message;

pub(crate) struct ControlledPeer {
    pub(crate) server: wiremock::MockServer,
    pub(crate) spec: rustain::domain::models::A2aPeerSpec,
    pub(crate) dispatched_task: std::sync::Arc<parking_lot::Mutex<String>>,
}

#[derive(Clone)]
struct InterleavingPeerState {
    alias: String,
    card: serde_json::Value,
    reject_sends: bool,
    tasks: std::sync::Arc<tokio::sync::Mutex<Vec<String>>>,
    task_added: std::sync::Arc<tokio::sync::Notify>,
    list_started: std::sync::Arc<tokio::sync::Notify>,
    list_release: std::sync::Arc<tokio::sync::Notify>,
    list_requests: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

struct InterleavingPeer {
    spec: rustain::domain::models::A2aPeerSpec,
    state: InterleavingPeerState,
    server: tokio::task::JoinHandle<()>,
}

async fn interleaving_peer(alias: &str, reject_sends: bool) -> InterleavingPeer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind interleaving peer");
    let uri = format!("http://{}", listener.local_addr().expect("peer address"));
    let state = InterleavingPeerState {
        alias: alias.to_owned(),
        card: serde_json::json!({
            "name": alias,
            "skills": [],
            "url": format!("{uri}/jsonrpc"),
            "preferredTransport": "JSONRPC"
        }),
        reject_sends,
        tasks: std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new())),
        task_added: std::sync::Arc::new(tokio::sync::Notify::new()),
        list_started: std::sync::Arc::new(tokio::sync::Notify::new()),
        list_release: std::sync::Arc::new(tokio::sync::Notify::new()),
        list_requests: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    };
    let app = axum::Router::new()
        .route(
            "/.well-known/agent-card.json",
            axum::routing::get(interleaving_peer_card),
        )
        .route("/jsonrpc", axum::routing::post(interleaving_peer_rpc))
        .with_state(state.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("interleaving peer server");
    });
    InterleavingPeer {
        spec: rustain::domain::models::A2aPeerSpec::new(
            alias,
            rustain::domain::models::RedactedUrl::from(uri),
            rustain::domain::models::A2aPeerSource::Workspace,
        ),
        state,
        server,
    }
}

async fn interleaving_peer_card(
    axum::extract::State(state): axum::extract::State<InterleavingPeerState>,
) -> axum::Json<serde_json::Value> {
    axum::Json(state.card.clone())
}

async fn interleaving_peer_rpc(
    axum::extract::State(state): axum::extract::State<InterleavingPeerState>,
    axum::Json(request): axum::Json<serde_json::Value>,
) -> axum::Json<serde_json::Value> {
    let id = request
        .get("id")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let result = match request["method"].as_str().unwrap_or_default() {
        "message/send" => {
            let task = request["params"]["message"]["messageId"]
                .as_str()
                .expect("send message id")
                .to_owned();
            state.tasks.lock().await.push(task);
            state.task_added.notify_one();
            if state.reject_sends {
                serde_json::json!({
                    "id": "controlled-task",
                    "status": { "state": "rejected" }
                })
            } else {
                serde_json::json!({
                    "id": "controlled-task",
                    "status": { "state": "completed" },
                    "metadata": { "x-rustain-item-id": "ri_interleaving" }
                })
            }
        }
        "x-rustain-items/list" => {
            state
                .list_requests
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            state.list_started.notify_one();
            state.list_release.notified().await;
            let tasks = state.tasks.lock().await.clone();
            let items: Vec<_> = tasks
                .iter()
                .enumerate()
                .map(|(index, task)| {
                    serde_json::json!({
                        "itemId": format!("ri-{}-{index}", state.alias),
                        "task": task,
                        "state": if state.reject_sends || index > 0 {
                            "acknowledged"
                        } else {
                            "received"
                        },
                        "ordinal": index + 1
                    })
                })
                .collect();
            serde_json::json!({ "items": items })
        }
        _ => {
            return axum::Json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": "method not found" }
            }));
        }
    };
    axum::Json(serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result
    }))
}

impl InterleavingPeer {
    async fn wait_for_list(&self) {
        loop {
            let notified = self.state.list_started.notified();
            if self
                .state
                .list_requests
                .load(std::sync::atomic::Ordering::SeqCst)
                > 0
            {
                return;
            }
            notified.await;
        }
    }

    async fn wait_for_tasks(&self, expected: usize) {
        loop {
            let notified = self.state.task_added.notified();
            if self.state.tasks.lock().await.len() >= expected {
                return;
            }
            notified.await;
        }
    }

    async fn tasks(&self) -> Vec<String> {
        self.state.tasks.lock().await.clone()
    }

    fn release_list(&self) {
        self.state.list_release.notify_one();
    }

    fn list_request_count(&self) -> usize {
        self.state
            .list_requests
            .load(std::sync::atomic::Ordering::SeqCst)
    }
}

fn index_zero_dispatch(event: &RoomEvent) -> bool {
    matches!(
        event,
        RoomEvent::RemoteEnvelopeDispatched {
            action: Some(reference),
            ..
        } if reference.index == 0
    )
}

fn index_one_dispatch(event: &RoomEvent) -> bool {
    matches!(
        event,
        RoomEvent::RemoteEnvelopeDispatched {
            action: Some(reference),
            ..
        } if reference.index == 1
    )
}

fn index_zero_settlement(event: &RoomEvent) -> bool {
    matches!(event, RoomEvent::TeamSendRecipientSettled { index: 0, .. })
}

async fn wait_for_send_settlement(sender: &mut Sender, index: usize) {
    tokio::time::timeout(std::time::Duration::from_secs(45), async {
        loop {
            match sender.domain_rx.recv().await {
                Some(rustain::domain::events::AppEvent::TeamSendSettled {
                    index: settled_index,
                    ..
                }) if settled_index == index => return,
                Some(_) => {}
                None => panic!("send settlement event stream closed"),
            }
        }
    })
    .await
    .expect("durable send settlement event");
}

pub(crate) async fn controlled_peer(
    alias: &str,
    rejected: bool,
    list_state: Option<&'static str>,
) -> ControlledPeer {
    controlled_peer_inner(alias, rejected, list_state, None).await
}

pub(crate) async fn controlled_pinned_peer(
    alias: &str,
    rejected: bool,
    list_state: Option<&'static str>,
    signing_key: ed25519_dalek::SigningKey,
) -> ControlledPeer {
    controlled_peer_inner(alias, rejected, list_state, Some(signing_key)).await
}

async fn controlled_peer_inner(
    alias: &str,
    rejected: bool,
    list_state: Option<&'static str>,
    signing_key: Option<ed25519_dalek::SigningKey>,
) -> ControlledPeer {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let mut card = serde_json::json!({
        "name": alias,
        "skills": [],
        "url": format!("{}/jsonrpc", server.uri()),
        "preferredTransport": "JSONRPC"
    });
    let pinned_key = signing_key.as_ref().map(|key| {
        use base64::Engine as _;
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use ed25519_dalek::Signer as _;
        let protected = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&serde_json::json!({ "alg": "EdDSA", "kid": "control" }))
                .expect("protected header"),
        );
        let payload = serde_jcs::to_vec(&card).expect("canonical card");
        let input = format!("{protected}.{}", URL_SAFE_NO_PAD.encode(payload));
        let signature = key.sign(input.as_bytes());
        card["signatures"] = serde_json::json!([{
            "protected": protected,
            "signature": URL_SAFE_NO_PAD.encode(signature.to_bytes())
        }]);
        rustain::domain::models::PinnedKey::new(
            rustain::domain::models::PinnedKeyAlgorithm::EdDsa,
            URL_SAFE_NO_PAD.encode(key.verifying_key().to_bytes()),
            Some("control".to_owned()),
        )
    });
    Mock::given(method("GET"))
        .and(path("/.well-known/agent-card.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(card))
        .mount(&server)
        .await;

    let dispatched_task = std::sync::Arc::new(parking_lot::Mutex::new(String::new()));
    let task_for_response = dispatched_task.clone();
    Mock::given(method("POST"))
        .and(path("/jsonrpc"))
        .respond_with(move |request: &wiremock::Request| {
            let input: serde_json::Value =
                serde_json::from_slice(&request.body).expect("JSON-RPC request");
            let request_id = input["id"].clone();
            let result = if input["method"] == "message/send" {
                let task = input["params"]["message"]["messageId"]
                    .as_str()
                    .expect("sent message id");
                *task_for_response.lock() = task.to_owned();
                serde_json::json!({
                    "id": "controlled-task",
                    "status": { "state": if rejected { "rejected" } else { "completed" } },
                    "metadata": { "x-rustain-item-id": "ri_controlled" }
                })
            } else {
                let task = task_for_response.lock().clone();
                let items = list_state.map_or_else(Vec::new, |state| {
                    vec![serde_json::json!({
                        "itemId": "ri_controlled",
                        "task": task,
                        "state": state,
                        "ordinal": 1
                    })]
                });
                serde_json::json!({ "items": items })
            };
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": request_id,
                "result": result
            }))
        })
        .mount(&server)
        .await;

    let mut spec = rustain::domain::models::A2aPeerSpec::new(
        alias,
        rustain::domain::models::RedactedUrl::from(server.uri()),
        rustain::domain::models::A2aPeerSource::Workspace,
    );
    if let Some(pin) = pinned_key {
        spec = spec.with_pinned_key(Some(pin));
    }
    ControlledPeer {
        spec,
        server,
        dispatched_task,
    }
}

pub(crate) async fn list_request_count(server: &wiremock::MockServer) -> usize {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|request| {
            request
                .body
                .windows(b"x-rustain-items/list".len())
                .any(|window| window == b"x-rustain-items/list")
        })
        .count()
}
async fn board(sender: &mut Sender, state: &mut TuiState, selector: TeamBoardSelector) -> String {
    sender.command(state, "/team board --last").await;
    team_board_message(sender.runtime(), &selector).await
}

/// **K02 settlement leg. Mutants M12 (ignore settlements), M13 (swap
/// Declined/Unreachable), M18 (omit live unsettled row), and M19 (drop the
/// ratified refusal sentence) → RED** at their per-row assertions. Positive
/// control: acknowledged, declined, awaiting-approval, delivered, live, and
/// preflight recipients each retain their own board representation.
#[tokio::test]
async fn k02_action_board_applies_settlement_and_remote_rules() {
    let delivered = Host::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let declined = Host::start(A2aAdmissionPolicy::Deny, ResponseMode::NotifyAndWait).await;
    let refused = Host::start(A2aAdmissionPolicy::Ask, ResponseMode::NotifyAndWait).await;
    let gate_journal = crate::harness::Journal::open().await;
    let gate = std::sync::Arc::new(GateRoomJournal::new(gate_journal.room.clone(), |event| {
        matches!(event, RoomEvent::RecipientItemReceived { .. })
    }));
    let held = Host::start_with_journal(
        gate_journal,
        gate.clone(),
        A2aAdmissionPolicy::Allow,
        ResponseMode::NotifyAndWait,
    )
    .await;
    let mut preflight_peer = delivered.peer("preflight");
    preflight_peer.auth = Some("RUSTAIN_19_21_INTENTIONALLY_UNSET_CREDENTIAL".to_owned());
    let peers = vec![
        delivered.peer("delivered"),
        declined.peer("declined"),
        refused.peer("refused"),
        held.peer("pending"),
        preflight_peer,
    ];
    let mut sender = Sender::start(peers.clone()).await;
    let mut state = TuiState::new(100, 40);
    sender
        .command(
            &mut state,
            "/team send delivered,declined,refused,pending,preflight payload",
        )
        .await;
    gate.wait_started().await;
    let actions = folded_actions(&sender.journal).await;
    let action = actions
        .last()
        .expect("action recorded")
        .id
        .as_str()
        .to_owned();
    let during = board(&mut sender, &mut state, TeamBoardSelector::Last).await;
    assert!(
        during.contains("pending") && during.contains("sending…"),
        "{during}"
    );
    assert!(
        during.contains("● delivered") && during.contains("delivered"),
        "{during}"
    );
    gate.release();
    tokio::time::timeout(std::time::Duration::from_secs(45), async {
        loop {
            let settlements = read_room_events(&sender.journal)
                .await
                .iter()
                .filter(|(_, event)| matches!(event, RoomEvent::TeamSendRecipientSettled { .. }))
                .count();
            if settlements == 5 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("all four recipient settlements are durable");
    let delivered_task = read_room_events(&delivered.journal)
        .await
        .into_iter()
        .find_map(|(_, event)| match event {
            RoomEvent::RecipientItemReceived { task, .. } => Some(task),
            _ => None,
        })
        .expect("delivered host's incoming task");
    acknowledge_received_item(&delivered.journal, &delivered_task).await;
    let acknowledged = read_room_events(&delivered.journal).await;
    assert!(
        acknowledged
            .iter()
            .any(|(_, event)| matches!(event, RoomEvent::RecipientItemAcknowledged { .. }))
    );
    let journal = sender.journal.clone();
    drop(sender);
    let reopened = journal.reopen().await;
    let mut refreshed = Sender::with_journal(peers, reopened.room.clone(), reopened).await;
    let rendered = board(
        &mut refreshed,
        &mut TuiState::new(100, 40),
        TeamBoardSelector::Action(action),
    )
    .await;
    assert!(
        rendered.contains("✓ delivered") && rendered.contains("acknowledged"),
        "{rendered}"
    );
    assert!(rendered.contains("✗ declined"), "{rendered}");
    assert!(
        rendered.contains("refused") && rendered.contains("awaiting their approval"),
        "{rendered}"
    );
    assert!(
        rendered.contains("● pending") && rendered.contains("delivered"),
        "{rendered}"
    );
    assert!(
        read_room_events(&refreshed.journal)
            .await
            .iter()
            .any(|(_, e)| matches!(e, RoomEvent::TeamSendRecipientSettled { .. }))
    );
    assert!(
        rendered.contains("⚠ preflight") && rendered.contains("no credential for preflight"),
        "{rendered}"
    );
    delivered.stop().await;
    declined.stop().await;
    refused.stop().await;
    held.stop().await;
}

/// **K02 correlation leg. Mutant M15 (join the newest item by peer instead of
/// this action's dispatch task) → RED:** the second action must stay `sending…`
/// despite an older acknowledged item at the same host. Positive control: the
/// first send's real host item was acknowledged and durably journaled.
#[tokio::test]
async fn k02_new_action_does_not_inherit_older_peer_acknowledgement() {
    let journal = crate::harness::Journal::open().await;
    let gate = std::sync::Arc::new(GateRoomJournal::new(journal.room.clone(), |event| {
        matches!(
            event,
            RoomEvent::RecipientItemReceived { content, .. } if content == "second payload"
        )
    }));
    let host = Host::start_with_journal(
        journal,
        gate.clone(),
        A2aAdmissionPolicy::Allow,
        ResponseMode::NotifyAndWait,
    )
    .await;
    let peers = vec![host.peer("same-peer")];
    let mut sender = Sender::start(peers).await;
    let mut state = TuiState::new(100, 40);
    sender
        .command(&mut state, "/team send same-peer first payload")
        .await;
    sender.settle(&mut state, 1).await;
    let first_task = read_room_events(&host.journal)
        .await
        .into_iter()
        .find_map(|(_, event)| match event {
            RoomEvent::RecipientItemReceived { content, task, .. }
                if content == "first payload" =>
            {
                Some(task)
            }
            _ => None,
        })
        .expect("first real item");
    acknowledge_received_item(&host.journal, &first_task).await;

    sender
        .command(&mut state, "/team send same-peer second payload")
        .await;
    gate.wait_started().await;
    sender.command(&mut state, "/team board --last").await;

    let board = team_board_message(sender.runtime(), &TeamBoardSelector::Last).await;
    assert!(
        board.contains("same-peer") && board.contains("sending…"),
        "{board}"
    );
    assert!(!board.contains("acknowledged"), "{board}");
    host.stop().await;
}

/// **K02 failed-read leg. Mutant M14 (turn a failed read after a landed send
/// into `⚠ unreachable`) → RED** at the explicit token/suffix checks. Positive
/// control: the POST was accepted and durably settled `Delivered` before the
/// real recipient host is stopped, so the read failure cannot rewrite it.
#[tokio::test]
async fn k02_failed_read_does_not_rewrite_landed_delivery() {
    let host = Host::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let peer = host.peer("offline-after-send");
    let mut sender = Sender::start(vec![peer.clone()]).await;
    let mut state = TuiState::new(100, 40);
    sender
        .command(&mut state, "/team send offline-after-send landed")
        .await;
    sender.settle(&mut state, 1).await;
    host.stop().await;

    let journal = sender.journal.clone();
    drop(sender);
    let reopened = journal.reopen().await;
    let mut reader = Sender::with_journal(vec![peer], reopened.room.clone(), reopened).await;
    let rendered = board(
        &mut reader,
        &mut TuiState::new(100, 40),
        TeamBoardSelector::Last,
    )
    .await;
    assert!(rendered.contains("● offline-after-send"), "{rendered}");
    assert!(rendered.contains("not readable now"), "{rendered}");
    assert!(!rendered.contains("⚠ offline-after-send"), "{rendered}");
}

/// **M16 (read the whole roster) → RED:** the action addresses only `target`;
/// the exact list-request counts must be 1 for it and 0 for `not-addressed`.
/// Positive control: the addressed controlled peer returns its correlated item.
#[tokio::test]
async fn k02_action_board_reads_only_addressed_roster_peers() {
    let target = controlled_peer("target", false, Some("received")).await;
    let not_addressed = controlled_peer("not-addressed", false, None).await;
    let mut sender = Sender::start(vec![target.spec.clone(), not_addressed.spec.clone()]).await;
    let mut state = TuiState::new(100, 40);
    sender
        .command(&mut state, "/team send target addressed-only")
        .await;
    sender.settle(&mut state, 1).await;
    sender.command(&mut state, "/team board --last").await;
    let rendered = team_board_message(sender.runtime(), &TeamBoardSelector::Last).await;
    assert!(
        rendered.contains("● target") && rendered.contains("delivered"),
        "{rendered}"
    );
    assert_eq!(list_request_count(&target.server).await, 1);
    assert_eq!(list_request_count(&not_addressed.server).await, 0);
}

/// **M17 (remote item overrides a recorded Declined settlement) → RED:** a
/// real action records `Declined` while its configured list reply would report
/// an acknowledged item. Positive control: a delivered sibling is actually
/// read and refined.
#[tokio::test]
async fn k02_declined_settlement_wins_over_correlated_acknowledgement() {
    let declined = controlled_peer("declined-with-item", true, Some("acknowledged")).await;
    let control = controlled_peer("read-control", false, Some("received")).await;
    let mut sender = Sender::start(vec![declined.spec.clone(), control.spec.clone()]).await;
    let mut state = TuiState::new(100, 40);
    sender
        .command(
            &mut state,
            "/team send declined-with-item,read-control declined-body",
        )
        .await;
    sender.settle(&mut state, 2).await;
    sender.command(&mut state, "/team board --last").await;
    let rendered = team_board_message(sender.runtime(), &TeamBoardSelector::Last).await;
    assert!(rendered.contains("✗ declined-with-item"), "{rendered}");
    assert!(!rendered.contains("✓ declined-with-item"), "{rendered}");
    assert!(
        rendered.contains("● read-control") && rendered.contains("delivered"),
        "{rendered}"
    );
    assert_eq!(list_request_count(&declined.server).await, 0);
    assert_eq!(list_request_count(&control.server).await, 1);
}
/// **M20 (S2 on a successful empty read) → RED:** both sends land, but one
/// list answers with an empty `items` array and the other host is stopped after
/// send. Positive control: only the failed read gets S2.
#[tokio::test]
async fn k02_successful_empty_read_and_failed_read_remain_distinct() {
    let empty = controlled_peer("empty", false, None).await;
    let offline = Host::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let offline_peer = offline.peer("offline");
    let peers = vec![empty.spec.clone(), offline_peer.clone()];
    let mut sender = Sender::start(peers.clone()).await;
    let mut state = TuiState::new(100, 40);
    sender
        .command(&mut state, "/team send empty,offline two-landed-sends")
        .await;
    sender.settle(&mut state, 2).await;
    offline.stop().await;

    let journal = sender.journal.clone();
    drop(sender);
    let reopened = journal.reopen().await;
    let mut reader = Sender::with_journal(peers, reopened.room.clone(), reopened).await;
    let rendered = board(
        &mut reader,
        &mut TuiState::new(100, 40),
        TeamBoardSelector::Last,
    )
    .await;
    assert!(
        rendered.contains("● empty") && rendered.contains("delivered"),
        "{rendered}"
    );
    // Per-ROW checks (the alias column is padded to the widest alias, so a
    // whole-text substring with a fixed gap would be blind to the row):
    // `S2` appears on the recipient whose read FAILED and only there.
    // **M20 (S2 on a successful empty read) → RED** on the first assertion.
    let empty_row = rendered
        .lines()
        .find(|line| line.contains("● empty"))
        .unwrap_or_else(|| panic!("the empty-read peer renders a delivered row: {rendered}"));
    assert!(
        !empty_row.contains("not readable now"),
        "a successful empty read is not `not readable now`: {rendered}"
    );
    let offline_row = rendered
        .lines()
        .find(|line| line.contains("● offline"))
        .unwrap_or_else(|| panic!("the failed-read peer renders a delivered row: {rendered}"));
    assert!(
        offline_row.contains("not readable now"),
        "a failed read after a landed send is `not readable now`: {rendered}"
    );
    assert_eq!(list_request_count(&empty.server).await, 1);
}

/// **M21 (render an unnameable remote state as `●`) → RED:** the controlled
/// peer returns the newer-build state `escalated`; that recipient is withheld
/// while the action header still counts it. Positive control: a real host's
/// nameable correlated item remains rendered as delivered in the same board.
#[tokio::test]
async fn k02_unnameable_correlated_item_is_withheld_and_counted() {
    let future = controlled_peer("future", false, Some("escalated")).await;
    let real = Host::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let peers = vec![future.spec.clone(), real.peer("real")];
    let mut sender = Sender::start(peers).await;
    let mut state = TuiState::new(100, 40);
    sender
        .command(&mut state, "/team send future,real mixed-states")
        .await;
    sender.settle(&mut state, 2).await;
    sender.command(&mut state, "/team board --last").await;
    let rendered = team_board_message(sender.runtime(), &TeamBoardSelector::Last).await;
    assert!(rendered.contains("2 addressed"), "{rendered}");
    assert!(
        rendered.contains("1 roster peer reported an outcome this build cannot name"),
        "{rendered}"
    );
    assert!(!rendered.contains("● future"), "{rendered}");
    assert!(
        rendered.contains("● real") && rendered.contains("delivered"),
        "{rendered}"
    );
    real.stop().await;
}

/// **M22 (identity check after read or alias-only binding) → RED:** a same-pin
/// rename still refines using the recorded alias, while a changed pin renders
/// S5 and performs zero list calls to the replacement. Positive control: the
/// renamed same-pin binding is actually read exactly once.
#[tokio::test]
async fn k02_identity_precheck_uses_pin_and_preserves_send_time_alias() {
    let key = ed25519_dalek::SigningKey::from_bytes(&[31; 32]);
    let original =
        controlled_pinned_peer("send-time", false, Some("acknowledged"), key.clone()).await;
    let mut sender = Sender::start(vec![original.spec.clone()]).await;
    let mut state = TuiState::new(100, 40);
    sender
        .command(&mut state, "/team send send-time pinned-send")
        .await;
    sender.settle(&mut state, 1).await;
    let dispatched_task = original.dispatched_task.lock().clone();
    let journal = sender.journal.clone();
    drop(sender);

    let renamed = controlled_pinned_peer("renamed", false, Some("acknowledged"), key).await;
    *renamed.dispatched_task.lock() = dispatched_task;
    let reopened = journal.reopen().await;
    let mut reader =
        Sender::with_journal(vec![renamed.spec.clone()], reopened.room.clone(), reopened).await;
    let rendered = board(
        &mut reader,
        &mut TuiState::new(100, 40),
        TeamBoardSelector::Last,
    )
    .await;
    assert_eq!(list_request_count(&renamed.server).await, 1);
    assert!(
        rendered.contains("✓ send-time") && rendered.contains("acknowledged"),
        "{rendered}"
    );
    assert!(!rendered.contains("renamed"), "{rendered}");

    let old_key = ed25519_dalek::SigningKey::from_bytes(&[32; 32]);
    let old = controlled_pinned_peer("old-alias", false, Some("received"), old_key).await;
    let mut sender = Sender::start(vec![old.spec.clone()]).await;
    let mut old_state = TuiState::new(100, 40);
    sender
        .command(&mut old_state, "/team send old-alias changed-pin")
        .await;
    sender.settle(&mut old_state, 1).await;
    let journal = sender.journal.clone();
    drop(sender);

    let new_key = ed25519_dalek::SigningKey::from_bytes(&[33; 32]);
    let replacement =
        controlled_pinned_peer("old-alias", false, Some("acknowledged"), new_key).await;
    let reopened = journal.reopen().await;
    let mut reader = Sender::with_journal(
        vec![replacement.spec.clone()],
        reopened.room.clone(),
        reopened,
    )
    .await;
    let rendered = board(
        &mut reader,
        &mut TuiState::new(100, 40),
        TeamBoardSelector::Last,
    )
    .await;
    assert!(
        rendered.contains("old-alias") && rendered.contains("delivered"),
        "{rendered}"
    );
    assert!(
        rendered.contains("not read — no eligible binding for the recorded peer"),
        "{rendered}"
    );
    assert_eq!(list_request_count(&replacement.server).await, 0);
}

/// **M23 (deserialize `Unknown` as absent) → RED:** a newer-build settlement
/// token is appended to the real journal for the interrupted recipient beside
/// a known delivered settlement; the board must say S8, not sending/delivered.
/// Positive control: the other real recipient still renders its delivered row.
#[tokio::test]
async fn k02_unknown_settlement_from_newer_journal_is_stated() {
    let landed = Host::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let interrupted_journal = crate::harness::Journal::open().await;
    let gate = std::sync::Arc::new(GateRoomJournal::new(
        interrupted_journal.room.clone(),
        |event| matches!(event, RoomEvent::RecipientItemReceived { content, .. } if content == "held unknown"),
    ));
    let interrupted = Host::start_with_journal(
        interrupted_journal,
        gate.clone(),
        A2aAdmissionPolicy::Allow,
        ResponseMode::NotifyAndWait,
    )
    .await;
    let peers = vec![landed.peer("landed"), interrupted.peer("interrupted")];
    let mut sender = Sender::start(peers.clone()).await;
    let journal = sender.journal.clone();
    let mut state = TuiState::new(100, 40);
    sender
        .command(&mut state, "/team send landed,interrupted held unknown")
        .await;
    gate.wait_started().await;
    tokio::time::timeout(std::time::Duration::from_secs(45), async {
        loop {
            if read_room_events(&journal).await.iter().any(|(_, event)| {
                matches!(event, RoomEvent::TeamSendRecipientSettled { index: 0, .. })
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the unheld recipient settles before the sender is killed");
    drop(sender);

    let room_file = journal
        .workspace
        .path()
        .join(".rustain")
        .join("rooms")
        .join(format!(
            "room-{}.jsonl",
            rustain::infrastructure::paths::workspace_hash(journal.workspace.path())
        ));
    let contents = std::fs::read_to_string(&room_file).expect("real sender journal file");
    let mut values: Vec<serde_json::Value> = contents
        .lines()
        .map(|line| serde_json::from_str(line).expect("journal entry JSON"))
        .collect();
    let latest_seq = values
        .iter()
        .map(|value| value["seq"].as_u64().expect("journal sequence"))
        .max()
        .expect("non-empty sender journal");
    let mut unknown_line = None;
    for mut value in values.drain(..) {
        if value["record"]["kind"] != "room"
            || value["record"]["payload"]["event"] != "team_send_recipient_settled"
            || value["record"]["payload"]["index"] != 0
        {
            continue;
        }
        value["record"]["payload"]["index"] = serde_json::json!(1);
        value["record"]["payload"]["settlement"]["outcome"] = serde_json::json!("future_outcome");
        value["seq"] = serde_json::json!(latest_seq + 1);
        unknown_line = Some(serde_json::to_vec(&value).expect("newer-build journal line"));
        break;
    }
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&room_file)
        .expect("open real sender journal for fixture append");
    file.write_all(&unknown_line.expect("real settled line copied for forward fixture"))
        .expect("append unknown settlement line");
    file.write_all(b"\n")
        .expect("terminate fixture journal line");
    file.sync_all().expect("durably append fixture line");

    let reopened = journal.reopen().await;
    let mut reader = Sender::with_journal(peers, reopened.room.clone(), reopened).await;
    let rendered = board(
        &mut reader,
        &mut TuiState::new(100, 40),
        TeamBoardSelector::Last,
    )
    .await;
    assert!(
        rendered.contains("interrupted")
            && rendered.contains("recorded outcome not recognised by this build"),
        "{rendered}"
    );
    assert!(!rendered.contains("interrupted  sending…"), "{rendered}");
    assert!(
        rendered.contains("● landed") && rendered.contains("delivered"),
        "{rendered}"
    );
    gate.release();
    landed.stop().await;
    interrupted.stop().await;
}

/// **R08 stale-facts leg.** Hold both action reads, then land the originally
/// selected action's settlement and task dispatches while its sibling read is
/// still held. A newer `--last` action is also dispatched before release.
/// The board must reload the original action, correlate its raw peer reply to
/// that action's latest tasks, and keep its authoritative decline over a
/// conflicting remote acknowledgement. Each addressed peer is read once.
#[tokio::test]
async fn k02_action_board_reconciles_latest_facts_after_held_sibling_read() {
    let declined = interleaving_peer("declined", true).await;
    let correlated = interleaving_peer("correlated", false).await;
    let journal = crate::harness::Journal::open().await;
    let settlement_gate = std::sync::Arc::new(GateRoomJournal::new(
        journal.room.clone(),
        index_zero_settlement,
    ));
    let index_one_gate = std::sync::Arc::new(GateRoomJournal::new(
        settlement_gate.clone(),
        index_one_dispatch,
    ));
    let index_zero_gate = std::sync::Arc::new(GateRoomJournal::new(
        index_one_gate.clone(),
        index_zero_dispatch,
    ));
    let sink: std::sync::Arc<dyn rustain::domain::ports::RoomJournal> = index_zero_gate.clone();
    let mut sender = Sender::with_journal(
        vec![declined.spec.clone(), correlated.spec.clone()],
        sink,
        journal,
    )
    .await;
    let mut state = TuiState::new(100, 40);
    sender
        .command(&mut state, "/team send declined,correlated r08-original")
        .await;
    index_zero_gate.wait_started().await;
    index_one_gate.wait_started().await;
    let original_action = folded_actions(&sender.journal)
        .await
        .last()
        .expect("original action recorded")
        .id
        .as_str()
        .to_owned();

    let runtime = sender.runtime().clone();
    let board_task = tokio::spawn(async move {
        let selector = rustain::adapters::a2a::board::ActionSelector::Last;
        rustain::adapters::a2a::board::collect_action_board(
            &runtime,
            &selector,
            std::time::Instant::now(),
        )
        .await
    });
    declined.wait_for_list().await;
    correlated.wait_for_list().await;

    // Recipient zero lands a real Declined settlement while the remote reply
    // that would otherwise acknowledge it is still withheld.
    index_zero_gate.release();
    settlement_gate.wait_started().await;
    settlement_gate.release();
    wait_for_send_settlement(&mut sender, 0).await;

    // Recipient one obtains its first dispatch and settlement only after its
    // board read has already started.
    index_one_gate.release();
    wait_for_send_settlement(&mut sender, 1).await;

    // This later action changes what a fresh `Last` lookup would select and
    // adds another task to the same peer before the original raw read returns.
    sender
        .command(&mut state, "/team send correlated r08-newer")
        .await;
    index_zero_gate.wait_started().await;
    let newer_action = folded_actions(&sender.journal)
        .await
        .last()
        .expect("newer action recorded")
        .id
        .as_str()
        .to_owned();
    assert_ne!(original_action, newer_action);
    index_zero_gate.release();
    correlated.wait_for_tasks(2).await;
    settlement_gate.wait_started().await;

    // Return the raw read for the selected action's peer while its sibling's
    // read remains held. The reply now contains both action task ids.
    correlated.release_list();
    assert!(
        !board_task.is_finished(),
        "the held sibling read keeps this board collection in flight"
    );
    declined.release_list();
    let board = board_task
        .await
        .expect("action-board task")
        .expect("coherent action board");

    assert_eq!(board.action.as_str(), original_action.as_str());
    assert_ne!(board.action.as_str(), newer_action.as_str());
    assert_eq!(board.rows.len(), 2);
    assert_eq!(board.rows[0].alias, "declined");
    assert_eq!(
        board.rows[0].state,
        rustain::adapters::a2a::board::ActionRowState::Declined
    );
    assert!(
        board.rows[0].items.is_empty(),
        "a latest definitive decline is never refined by the held remote acknowledgement"
    );
    assert_eq!(board.rows[1].alias, "correlated");
    assert_eq!(
        board.rows[1].state,
        rustain::adapters::a2a::board::ActionRowState::Delivered
    );
    let tasks = correlated.tasks().await;
    assert_eq!(tasks.len(), 2);
    assert_eq!(board.rows[1].items.len(), 1);
    assert_eq!(
        board.rows[1].items[0].task.as_deref(),
        Some(tasks[0].as_str())
    );
    assert_ne!(
        board.rows[1].items[0].task.as_deref(),
        Some(tasks[1].as_str()),
        "the newer action's acknowledgement must not leak into the original action"
    );
    assert_eq!(declined.list_request_count(), 1);
    assert_eq!(correlated.list_request_count(), 1);

    settlement_gate.release();
    wait_for_send_settlement(&mut sender, 0).await;
    declined.server.abort();
    correlated.server.abort();
}
