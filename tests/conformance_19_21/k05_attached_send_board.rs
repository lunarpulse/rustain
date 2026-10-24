//! Story 19.21 K05 — signed-socket send/board parity and authority keystones.
use super::attached_harness::{
    AttachedClient, AttachedDaemon, RecipientHost, standalone_attached_parity,
};
use super::harness::read_room_events;
use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::daemon::protocol::{
    AttachMode, ClientFrame, ConnectionTier, DaemonFrame, PROTOCOL_VERSION, ProtocolError,
    RecipientAction, RecipientActionOutcome, RecipientActionTarget, TeamBoardRequest, read_frame,
    write_frame,
};
use rustain::adapters::rap::IdentityKeyStore;
use rustain::domain::models::{RoomEvent, SemanticMessageType};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::{TcpSocket, UnixStream};

const BODY: &str = "first line\n  indented second line\n\nlast line";

fn request_refusal(frame: &DaemonFrame, request: &str, action: RecipientAction) -> bool {
    matches!(frame,
        DaemonFrame::RecipientActionResult {
            action: found,
            target: RecipientActionTarget::Request { request: key },
            outcome: RecipientActionOutcome::Refused { .. },
        } if *found == action && key == request
    )
}

fn endpoint_values(facts: &[String]) -> Vec<String> {
    let marker = "endpoint: Some(\"";
    facts
        .iter()
        .flat_map(|fact| {
            let mut values = Vec::new();
            let mut remaining = fact.as_str();
            while let Some(start) = remaining.find(marker) {
                let value_start = start + marker.len();
                let Some(end) = remaining[value_start..].find('"') else {
                    break;
                };
                values.push(remaining[value_start..value_start + end].to_owned());
                remaining = &remaining[value_start + end + 1..];
            }
            values
        })
        .collect()
}

fn assert_endpoint_consistency(facts: &[String]) {
    let endpoints = endpoint_values(facts);
    assert_eq!(
        endpoints.len(),
        4,
        "two recipients have action and dispatch endpoints"
    );
    assert_eq!(
        endpoints[0], endpoints[2],
        "first recipient endpoint is consistent"
    );
    assert_eq!(
        endpoints[1], endpoints[3],
        "second recipient endpoint is consistent"
    );
    assert_ne!(
        endpoints[0], endpoints[1],
        "distinct hosts have distinct endpoints"
    );
}

fn normalize_generated_fact_ids(facts: &[String]) -> Vec<String> {
    let endpoints = endpoint_values(facts);
    let mut unique_endpoints = Vec::<String>::new();
    facts
        .iter()
        .map(|fact| {
            let mut normalized = fact.clone();
            for endpoint in &endpoints {
                let index = unique_endpoints
                    .iter()
                    .position(|seen| seen == endpoint)
                    .unwrap_or_else(|| {
                        unique_endpoints.push(endpoint.clone());
                        unique_endpoints.len() - 1
                    });
                normalized = normalized.replace(
                    &format!("endpoint: Some(\"{endpoint}\")"),
                    &format!("endpoint: Some(\"<endpoint:{}>\")", index + 1),
                );
            }
            for prefix in ["task: Some(\"", "item: Some(\""] {
                if let Some(start) = normalized.find(prefix) {
                    let value_start = start + prefix.len();
                    if let Some(end) = normalized[value_start..].find('"') {
                        normalized.replace_range(value_start..value_start + end, "<generated-id>");
                    }
                }
            }
            normalized
        })
        .collect()
}

/// Settlements are journaled as each recipient answers, so their relative
/// order is the order the hosts replied in — not a property of either rail
/// (reproduced: 1 run in 30 on two CPUs swaps `index: 0` and `index: 1`).
/// Sort each contiguous run of `TeamSendRecipientSettled` facts; everything
/// else, including the action record and dispatch order, stays exact.
fn settlements_in_index_order(mut facts: Vec<String>) -> Vec<String> {
    let mut start = 0;
    while start < facts.len() {
        let is_settlement = |fact: &String| fact.starts_with("TeamSendRecipientSettled");
        if !is_settlement(&facts[start]) {
            start += 1;
            continue;
        }
        let end = facts[start..]
            .iter()
            .position(|fact| !is_settlement(fact))
            .map_or(facts.len(), |offset| start + offset);
        facts[start..end].sort();
        start = end;
    }
    facts
}
fn normalize_rendered_ids(rows: &str) -> String {
    let mut normalized = String::with_capacity(rows.len());
    for line in rows.lines() {
        let mut previous = "";
        for word in line.split_whitespace() {
            if previous == "task" {
                normalized.push_str("<generated-task>");
            } else if previous == "item" && word.starts_with("ri_") {
                normalized.push_str("<generated-item>");
            } else {
                normalized.push_str(word);
            }
            normalized.push(' ');
            previous = word;
        }
        normalized.push('\n');
    }
    normalized
}
#[tokio::test]
async fn k05_attached_typed_labelled_send_matches_standalone_and_preserves_multiline_body() {
    let aliases = vec!["jun".to_owned(), "tom".to_owned()];
    let parity = standalone_attached_parity(aliases, BODY, "sprint-19").await;
    // Only generated action/task/item ids and opaque endpoint fingerprints are
    // normalized. Endpoint slots are independently checked per rail; subjects,
    // recipient order, body hash/bytes, message type, and settlement facts remain exact.
    assert_endpoint_consistency(&parity.standalone_facts);
    assert_endpoint_consistency(&parity.attached_facts);
    let standalone_facts =
        settlements_in_index_order(normalize_generated_fact_ids(&parity.standalone_facts));
    let attached_facts =
        settlements_in_index_order(normalize_generated_fact_ids(&parity.attached_facts));
    assert_eq!(
        standalone_facts, attached_facts,
        "attached typed send must journal the same action and settlement facts"
    );
    let deliberately_changed: Vec<_> = parity
        .attached_facts
        .iter()
        .map(|fact| fact.replace("sprint-19", "different-subject"))
        .collect();
    assert_ne!(
        standalone_facts,
        normalize_generated_fact_ids(&deliberately_changed),
        "normalization must not hide an actual subject difference"
    );
    assert_eq!(
        normalize_rendered_ids(&parity.standalone_rows),
        normalize_rendered_ids(&parity.attached_rows),
        "attached typed send must render through the shared action-board renderer"
    );
    for recipient in parity
        .standalone_hosts
        .iter()
        .chain(parity.attached_hosts.iter())
    {
        let events = read_room_events(&recipient.journal).await;
        let received: Vec<_> = events
            .iter()
            .filter_map(|(_, event)| match event {
                RoomEvent::RecipientItemReceived {
                    content,
                    message_type,
                    ..
                } => Some((content, message_type)),
                _ => None,
            })
            .collect();
        assert_eq!(
            received.len(),
            1,
            "each equivalent real host receives one send from its own rail"
        );
        for (content, message_type) in received {
            assert_eq!(content, BODY, "multiline body arrives byte-for-byte intact");
            assert_eq!(*message_type, SemanticMessageType::StoryAssignment);
            assert!(
                !content.contains("sprint-19"),
                "sender-local label must not reach recipient"
            );
        }
    }
    for recipient in parity
        .standalone_hosts
        .into_iter()
        .chain(parity.attached_hosts)
    {
        recipient.stop().await;
    }
}

#[tokio::test]
async fn k05_every_attached_board_selector_matches_shared_standalone_renderer() {
    // The action gives Last/Action/Actions a real journal row; Peer and Inventory
    // exercise the same real roster and recipient journal. `team_board_message_at`
    // is the shared renderer with the test-only injected monotonic Instant.
    let recipient = RecipientHost::start(A2aAdmissionPolicy::Allow).await;
    let board_times = Arc::new(parking_lot::Mutex::new(Vec::<Instant>::new()));
    let recorded_times = board_times.clone();
    let start = Instant::now();
    let board_now: Arc<dyn Fn() -> Instant + Send + Sync> = Arc::new(move || {
        let mut times = recorded_times.lock();
        let now = start + Duration::from_secs(3 * (times.len() as u64 + 1));
        times.push(now);
        now
    });
    let daemon =
        AttachedDaemon::start_with_board_now(vec![recipient.peer("peer-a")], board_now).await;
    let mut writer =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    writer
        .send_team(
            "seed-board",
            vec!["peer-a".into()],
            BODY,
            Some(SemanticMessageType::StoryAssignment),
            Some("board-label".into()),
            None,
        )
        .await;
    let started = writer.wait_send_started("seed-board").await;
    let action_id = match started {
        DaemonFrame::TeamSendStarted { action, .. } => action.as_str().to_owned(),
        other => panic!("board seed send refused: {other:?}"),
    };
    assert!(matches!(
        writer.wait_send_settled("seed-board", 0).await,
        DaemonFrame::TeamSendSettled { .. }
    ));

    let selectors = [
        TeamBoardRequest::Inventory,
        TeamBoardRequest::Peer("peer-a".into()),
        TeamBoardRequest::Last,
        TeamBoardRequest::Action(action_id.clone()),
        TeamBoardRequest::Actions,
    ];
    for (index, selector) in selectors.into_iter().enumerate() {
        let request = format!("board-{index}");
        writer.team_board(request.clone(), selector.clone()).await;
        let result = writer.wait_board_result(&request).await;
        let attached =
            result.unwrap_or_else(|other| panic!("trusted board selector refused: {other:?}"));
        let now = *board_times
            .lock()
            .last()
            .expect("attached TeamBoard uses injected clock");
        let oracle = rustain::infrastructure::runtime::transparency_bridge::team_board_message_at(
            daemon.egress.runtime(),
            &rustain::adapters::tui::handlers::team_command::TeamBoardSelector::from(selector),
            now + Duration::from_secs(1),
        )
        .await;
        assert_eq!(
            attached, oracle,
            "attached selector {index} differs from shared renderer"
        );
    }
    daemon.stop().await;
    recipient.stop().await;
}

#[tokio::test]
async fn k05_read_only_and_peer_gates_refuse_mutation_but_trusted_frames_work() {
    let recipient = RecipientHost::start(A2aAdmissionPolicy::Allow).await;
    let daemon = AttachedDaemon::start(vec![recipient.peer("peer-a")]).await;

    let mut read_only =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, true).await;
    assert_eq!(read_only.granted, AttachMode::ReadOnly);
    read_only
        .send_team(
            "ro-send",
            vec!["peer-a".into()],
            "must not dispatch",
            None,
            None,
            None,
        )
        .await;
    let refused = read_only.wait_send_started("ro-send").await;
    assert!(
        request_refusal(&refused, "ro-send", RecipientAction::TeamSend),
        "read-only send must return request-keyed refusal: {refused:?}"
    );
    read_only
        .team_board("ro-board", TeamBoardRequest::Inventory)
        .await;
    let board = read_only.wait_board_result("ro-board").await;
    assert!(
        board.is_ok(),
        "read-only TeamBoard must be served: {board:?}"
    );
    read_only.team_status("ro-status").await;
    let status = read_only.wait_status_result("ro-status").await;
    assert!(
        matches!(status, DaemonFrame::TeamStatusResult { .. }),
        "read-only TeamStatus must be served: {status:?}"
    );
    assert!(
        read_room_events(&daemon.journal)
            .await
            .iter()
            .all(|(_, event)| {
                !matches!(
                    event,
                    RoomEvent::TeamSendActionRecorded { .. }
                        | RoomEvent::RemoteEnvelopeDispatched { .. }
                )
            }),
        "read-only refusal has no sender journal or HTTP dispatch fact"
    );
    assert!(
        read_room_events(&recipient.journal)
            .await
            .iter()
            .all(|(_, event)| !matches!(event, RoomEvent::RecipientItemReceived { .. }))
    );

    let mut peer = AttachedClient::attach(&daemon.path, ConnectionTier::Peer, false).await;
    assert_eq!(peer.granted, AttachMode::ReadOnly);
    peer.send_team(
        "peer-send",
        vec!["peer-a".into()],
        "peer is not a writer",
        None,
        None,
        None,
    )
    .await;
    let refused = peer.wait_send_started("peer-send").await;
    assert!(
        request_refusal(&refused, "peer-send", RecipientAction::TeamSend),
        "Peer send must be refused: {refused:?}"
    );
    peer.team_board("peer-board", TeamBoardRequest::Inventory)
        .await;
    let refused = peer
        .wait_board_result("peer-board")
        .await
        .expect_err("Peer board must be refused");
    assert!(
        request_refusal(&refused, "peer-board", RecipientAction::TeamBoard),
        "Peer board refusal must retain its request and action: {refused:?}"
    );
    peer.team_status("peer-status").await;
    let refused = peer.wait_status_result("peer-status").await;
    assert!(
        request_refusal(&refused, "peer-status", RecipientAction::TeamStatus),
        "Peer status must be refused: {refused:?}"
    );
    assert!(
        read_room_events(&daemon.journal)
            .await
            .iter()
            .all(|(_, event)| {
                !matches!(
                    event,
                    RoomEvent::TeamSendActionRecorded { .. }
                        | RoomEvent::RemoteEnvelopeDispatched { .. }
                )
            }),
        "Peer refusal has no sender journal or HTTP dispatch fact"
    );

    let mut trusted =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    assert_eq!(trusted.granted, AttachMode::ReadWrite);
    trusted
        .send_team(
            "trusted-send",
            vec!["peer-a".into()],
            "trusted positive control",
            None,
            None,
            None,
        )
        .await;
    assert!(matches!(
        trusted.wait_send_started("trusted-send").await,
        DaemonFrame::TeamSendStarted { .. }
    ));
    assert!(matches!(
        trusted.wait_send_settled("trusted-send", 0).await,
        DaemonFrame::TeamSendSettled { .. }
    ));
    trusted
        .team_board("trusted-board", TeamBoardRequest::Inventory)
        .await;
    assert!(trusted.wait_board_result("trusted-board").await.is_ok());
    trusted.team_status("trusted-status").await;
    assert!(matches!(
        trusted.wait_status_result("trusted-status").await,
        DaemonFrame::TeamStatusResult { .. }
    ));

    daemon.stop().await;
    recipient.stop().await;
}

#[tokio::test]
async fn k05_peer_send_gate_refuses_while_trusted_send_succeeds() {
    let recipient = RecipientHost::start(A2aAdmissionPolicy::Allow).await;
    let daemon = AttachedDaemon::start(vec![recipient.peer("peer-a")]).await;
    let mut peer = AttachedClient::attach(&daemon.path, ConnectionTier::Peer, false).await;
    peer.send_team(
        "peer-send",
        vec!["peer-a".into()],
        "peer is not a writer",
        None,
        None,
        None,
    )
    .await;
    let refused = peer.wait_send_started("peer-send").await;
    assert!(
        request_refusal(&refused, "peer-send", RecipientAction::TeamSend),
        "Peer send must be refused: {refused:?}"
    );
    assert!(
        read_room_events(&daemon.journal)
            .await
            .iter()
            .all(|(_, event)| !matches!(
                event,
                RoomEvent::TeamSendActionRecorded { .. }
                    | RoomEvent::RemoteEnvelopeDispatched { .. }
            )),
        "Peer send refusal has no journal or HTTP effect"
    );

    let mut trusted =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    trusted
        .send_team(
            "trusted-send",
            vec!["peer-a".into()],
            "trusted positive control",
            None,
            None,
            None,
        )
        .await;
    let action = match trusted.wait_send_started("trusted-send").await {
        DaemonFrame::TeamSendStarted { action, .. } => action,
        other => panic!("TrustedLocal positive control did not start: {other:?}"),
    };
    assert!(matches!(
        trusted.wait_send_settled("trusted-send", 0).await,
        DaemonFrame::TeamSendSettled {
            outcome: rustain::adapters::daemon::protocol::TeamSendOutcome::Delivered,
            ..
        }
    ));
    assert!(
        read_room_events(&recipient.journal)
            .await
            .iter()
            .any(|(_, event)| matches!(
                event,
                RoomEvent::RecipientItemReceived { content, .. }
                    if content == "trusted positive control"
            )),
        "TrustedLocal send positive control reaches the real host"
    );
    let entries = daemon.journal.entries().await;
    let dispatches: Vec<_> = entries
        .iter()
        .filter(|entry| {
            matches!(
                &entry.record,
                rustain::domain::models::JournalRecord::Room(RoomEvent::RemoteEnvelopeDispatched {
                    action: Some(reference),
                    ..
                }) if reference.action == action && reference.index == 0
            )
        })
        .collect();
    assert_eq!(
        dispatches.len(),
        1,
        "typed send action has exactly one dispatch"
    );
    let dispatch = dispatches[0];
    assert!(
        matches!(
            &dispatch.record,
            rustain::domain::models::JournalRecord::Room(RoomEvent::RemoteEnvelopeDispatched {
                authorship: rustain::domain::models::Authorship::HumanWritten,
                send_authorization: rustain::domain::models::SendAuthorization::HumanDecided,
                ..
            })
        ),
        "R6 typed TeamSend dispatch retains HumanWritten + HumanDecided"
    );
    let row = rustain::domain::services::transparency::transparency_row(dispatch).unwrap();
    assert!(
        !row.summary.contains("[agent-composed]") && !row.summary.contains("[auto-sent]"),
        "R6 typed TeamSend summary remains untagged: {}",
        row.summary
    );
    daemon.stop().await;
    recipient.stop().await;
}

#[tokio::test]
async fn k05_interleaved_send_results_and_refusal_stay_with_their_request() {
    let (recipient, gate) = RecipientHost::start_with_gate(A2aAdmissionPolicy::Allow, |event| {
        matches!(event, RoomEvent::RecipientItemReceived { .. })
    })
    .await;
    let daemon = AttachedDaemon::start(vec![recipient.peer("good")]).await;
    let mut client =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    client
        .send_team(
            "good-request",
            vec!["good".into()],
            "valid body",
            None,
            Some("good-label".into()),
            None,
        )
        .await;
    let good_started = client
        .wait_for(|frame| matches!(frame, DaemonFrame::TeamSendStarted { .. }))
        .await;
    assert!(
        matches!(good_started, DaemonFrame::TeamSendStarted { ref request, .. } if request == "good-request"),
        "started frame crossed request keys: {good_started:?}"
    );
    gate.wait_for(1).await;

    client
        .send_team(
            "bad-request",
            vec!["unknown-alias".into()],
            "invalid body",
            None,
            Some("bad-label".into()),
            None,
        )
        .await;
    let bad = client.wait_send_started("bad-request").await;
    assert!(
        request_refusal(&bad, "bad-request", RecipientAction::TeamSend),
        "unknown alias refusal crossed request keys: {bad:?}"
    );
    gate.release_append();
    let good_settled = client.wait_send_settled("good-request", 0).await;
    assert!(
        matches!(good_settled, DaemonFrame::TeamSendSettled { ref request, .. } if request == "good-request"),
        "settlement crossed request keys: {good_settled:?}"
    );
    let grouped = client.frames_by_request_and_kind();
    assert!(
        grouped["good-request"].contains_key("started")
            && grouped["good-request"].contains_key("settled")
    );
    assert!(grouped["bad-request"].contains_key("send-refusal"));
    assert!(!grouped["good-request"].contains_key("send-refusal"));
    assert!(
        !grouped["bad-request"].contains_key("started")
            && !grouped["bad-request"].contains_key("settled")
    );
    daemon.stop().await;
    recipient.stop().await;
}

/// Story 19.20c `M30`: old signed clients — including the immediately previous
/// version — are refused with the current daemon version before registration.
#[tokio::test]
async fn k05_old_signed_attach_versions_are_rejected_and_current_control_succeeds() {
    let daemon = AttachedDaemon::start(Vec::new()).await;
    let mut stream = UnixStream::connect(&daemon.path)
        .await
        .expect("connect daemon");
    let challenge = tokio::time::timeout(
        super::attached_harness::FIXTURE_TIMEOUT,
        read_frame::<_, DaemonFrame>(&mut stream),
    )
    .await
    .expect("challenge deadline")
    .expect("read challenge")
    .expect("challenge frame");
    let nonce = match challenge {
        DaemonFrame::AttachChallenge { nonce } => nonce,
        other => panic!("challenge expected: {other:?}"),
    };
    let keys = tempfile::tempdir().expect("old signer directory");
    let signer = IdentityKeyStore::new(keys.path())
        .load_or_generate()
        .expect("old signer");
    let proof = signer.attach_proof(&nonce, 4, ConnectionTier::TrustedLocal.proof_tag(), false);
    write_frame(
        &mut stream,
        &ClientFrame::Attach {
            protocol_version: 4,
            read_only_ok: false,
            tier: ConnectionTier::TrustedLocal,
            challenge_nonce: nonce,
            identity: signer.identity().clone(),
            proof,
        },
    )
    .await
    .expect("write fixed-v4 attach");
    let frame = tokio::time::timeout(
        super::attached_harness::FIXTURE_TIMEOUT,
        read_frame::<_, DaemonFrame>(&mut stream),
    )
    .await
    .expect("version result deadline")
    .expect("read mismatch")
    .expect("version mismatch frame");
    assert!(
        matches!(
            frame,
            DaemonFrame::Error(ProtocolError::VersionMismatch {
                daemon: PROTOCOL_VERSION,
                client: 4
            })
        ),
        "fixed old signed client must receive precise VersionMismatch: {frame:?}"
    );
    let mut older = UnixStream::connect(&daemon.path)
        .await
        .expect("connect v5 client");
    let nonce = match tokio::time::timeout(
        super::attached_harness::FIXTURE_TIMEOUT,
        read_frame::<_, DaemonFrame>(&mut older),
    )
    .await
    .expect("v5 challenge deadline")
    .expect("read v5 challenge")
    .expect("v5 challenge frame")
    {
        DaemonFrame::AttachChallenge { nonce } => nonce,
        other => panic!("challenge expected: {other:?}"),
    };
    let proof = signer.attach_proof(&nonce, 5, ConnectionTier::TrustedLocal.proof_tag(), false);
    write_frame(
        &mut older,
        &ClientFrame::Attach {
            protocol_version: 5,
            read_only_ok: false,
            tier: ConnectionTier::TrustedLocal,
            challenge_nonce: nonce,
            identity: signer.identity().clone(),
            proof,
        },
    )
    .await
    .expect("write fixed-v5 attach");
    assert!(matches!(
        read_frame::<_, DaemonFrame>(&mut older).await.unwrap(),
        Some(DaemonFrame::Error(ProtocolError::VersionMismatch {
            daemon: PROTOCOL_VERSION,
            client: 5
        }))
    ));
    drop(older);
    let previous_version = PROTOCOL_VERSION - 1;
    let mut previous = UnixStream::connect(&daemon.path)
        .await
        .expect("connect previous-version client");
    let nonce = match tokio::time::timeout(
        super::attached_harness::FIXTURE_TIMEOUT,
        read_frame::<_, DaemonFrame>(&mut previous),
    )
    .await
    .expect("v6 challenge deadline")
    .expect("read v6 challenge")
    .expect("v6 challenge frame")
    {
        DaemonFrame::AttachChallenge { nonce } => nonce,
        other => panic!("challenge expected: {other:?}"),
    };
    let proof = signer.attach_proof(
        &nonce,
        previous_version,
        ConnectionTier::TrustedLocal.proof_tag(),
        false,
    );
    write_frame(
        &mut previous,
        &ClientFrame::Attach {
            protocol_version: previous_version,
            read_only_ok: false,
            tier: ConnectionTier::TrustedLocal,
            challenge_nonce: nonce,
            identity: signer.identity().clone(),
            proof,
        },
    )
    .await
    .expect("write previous-version attach");
    let frame = tokio::time::timeout(
        super::attached_harness::FIXTURE_TIMEOUT,
        read_frame::<_, DaemonFrame>(&mut previous),
    )
    .await
    .expect("v6 version result deadline")
    .expect("read v6 mismatch")
    .expect("v6 mismatch frame");
    assert!(
        matches!(
            frame,
            DaemonFrame::Error(ProtocolError::VersionMismatch { daemon, client })
                if daemon == PROTOCOL_VERSION && client == previous_version
        ),
        "signed previous-version client must be refused by the current protocol: {frame:?}"
    );
    drop(previous);
    let control = AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    assert_eq!(
        control.granted,
        AttachMode::ReadWrite,
        "signed current-version control attaches"
    );
    daemon.stop().await;
}

#[tokio::test]
async fn k05_boot_down_egress_slot_stays_refused_after_peer_comes_up() {
    // Bound but not listening: warm-up fails without releasing this test's port.
    let reserved = TcpSocket::new_v4().expect("create reserved socket");
    reserved
        .bind(([127, 0, 0, 1], 0).into())
        .expect("reserve endpoint");
    let address = reserved.local_addr().expect("reserved address");
    let boot_down = rustain::domain::models::A2aPeerSpec::new(
        "boot-down",
        rustain::domain::models::RedactedUrl::from(format!("http://{address}/")),
        rustain::domain::models::A2aPeerSource::Workspace,
    );
    let boot_up_host = RecipientHost::start(A2aAdmissionPolicy::Allow).await;
    let boot_up = boot_up_host.peer("boot-up");
    let daemon = AttachedDaemon::start(vec![boot_down, boot_up]).await;
    let listener = reserved
        .listen(1024)
        .expect("bring boot-down peer up on same address");
    let boot_down_host = RecipientHost::start_on(listener, A2aAdmissionPolicy::Allow, None).await;
    let mut client =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    client
        .send_team(
            "fresh-egress",
            vec!["boot-down".into(), "boot-up".into()],
            BODY,
            Some(SemanticMessageType::StoryAssignment),
            Some("fresh-label".into()),
            None,
        )
        .await;
    let started = client.wait_send_started("fresh-egress").await;
    assert!(
        matches!(started, DaemonFrame::TeamSendStarted { .. }),
        "fresh-egress send must start before recipient settlements: {started:?}"
    );
    let down = client.wait_send_settled("fresh-egress", 0).await;
    let up = client.wait_send_settled("fresh-egress", 1).await;
    let down_text = format!("{down:?}");
    assert!(
        down_text.contains("CardNotCached")
            || down_text.to_ascii_lowercase().contains("not cached"),
        "original boot-down egress must keep its ratified refusal: {down:?}"
    );
    assert!(
        matches!(
            up,
            DaemonFrame::TeamSendSettled {
                outcome: rustain::adapters::daemon::protocol::TeamSendOutcome::Delivered,
                ..
            }
        ),
        "boot-up peer is the delivery positive control: {up:?}"
    );
    assert!(
        read_room_events(&boot_down_host.journal)
            .await
            .iter()
            .all(|(_, event)| !matches!(event, RoomEvent::RecipientItemReceived { .. })),
        "boot-down peer receives nothing"
    );
    assert!(read_room_events(&boot_up_host.journal).await.iter().any(|(_, event)|
        matches!(event, RoomEvent::RecipientItemReceived { content, .. } if content == BODY)),
        "boot-up peer receives the exact body");
    daemon.stop().await;
    boot_down_host.stop().await;
    boot_up_host.stop().await;
}
#[tokio::test]
async fn k05_attached_board_withholds_unnameable_row_but_keeps_six_addressed_five_visible() {
    let first = RecipientHost::start(A2aAdmissionPolicy::Allow).await;
    let second = RecipientHost::start(A2aAdmissionPolicy::Allow).await;
    let declined = RecipientHost::start(A2aAdmissionPolicy::Deny).await;
    let preflight_host = RecipientHost::start(A2aAdmissionPolicy::Allow).await;
    let (pending, gate) = RecipientHost::start_with_gate(A2aAdmissionPolicy::Allow, |event| {
        matches!(event, RoomEvent::RecipientItemReceived { .. })
    })
    .await;
    let future = super::k02_board_rules::controlled_peer("future", false, Some("escalated")).await;
    let mut preflight_peer = preflight_host.peer("preflight");
    preflight_peer.auth = Some("RUSTAIN_19_21_INTENTIONALLY_UNSET_CREDENTIAL".into());
    let peers = vec![
        first.peer("acknowledged"),
        second.peer("delivered"),
        declined.peer("declined"),
        preflight_peer,
        pending.peer("pending"),
        future.spec.clone(),
    ];
    let daemon = AttachedDaemon::start(peers).await;
    let mut client =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    client
        .send_team(
            "six-addressed",
            vec![
                "acknowledged".into(),
                "delivered".into(),
                "declined".into(),
                "preflight".into(),
                "pending".into(),
                "future".into(),
            ],
            "six-peer payload",
            Some(SemanticMessageType::StoryAssignment),
            Some("six-label".into()),
            None,
        )
        .await;
    let started = client.wait_send_started("six-addressed").await;
    let action_id = match started {
        DaemonFrame::TeamSendStarted { action, .. } => action.as_str().to_owned(),
        other => panic!("six-addressed send refused: {other:?}"),
    };
    gate.wait_for(1).await;
    for index in [0, 1, 2, 3, 5] {
        let settled = client.wait_send_settled("six-addressed", index).await;
        assert!(
            matches!(settled, DaemonFrame::TeamSendSettled { ref request, index: found, .. }
                if request == "six-addressed" && found == index),
            "wrong six-addressed settlement: {settled:?}"
        );
    }
    let selector = TeamBoardRequest::Action(action_id);
    client.team_board("six-board", selector.clone()).await;
    let result = client.wait_board_result("six-board").await;
    let attached =
        result.unwrap_or_else(|other| panic!("six-addressed action board refused: {other:?}"));
    let now = std::time::Instant::now() + Duration::from_secs(2);
    let oracle = rustain::infrastructure::runtime::transparency_bridge::team_board_message_at(
        daemon.egress.runtime(),
        &rustain::adapters::tui::handlers::team_command::TeamBoardSelector::from(selector),
        now,
    )
    .await;
    assert_eq!(
        attached, oracle,
        "attached action board diverges from shared renderer"
    );
    assert!(attached.contains("6 addressed"), "{attached}");
    let rendered_aliases: Vec<_> = attached
        .lines()
        .filter(|line| {
            line.split_whitespace().any(|word| {
                [
                    "acknowledged",
                    "delivered",
                    "declined",
                    "preflight",
                    "pending",
                ]
                .contains(&word)
            })
        })
        .collect();
    assert_eq!(
        rendered_aliases.len(),
        5,
        "unnameable peer row is withheld, count retained: {attached}"
    );
    assert!(
        attached.contains("1 roster peer reported an outcome this build cannot name"),
        "unknown peer state is withheld with its count: {attached}"
    );
    assert!(
        !attached.contains("future"),
        "unknown peer alias/state is not echoed: {attached}"
    );
    assert!(
        attached.contains("six-label") && attached.contains("sending…"),
        "{attached}"
    );
    gate.release_append();
    assert!(matches!(
        client.wait_send_settled("six-addressed", 4).await,
        DaemonFrame::TeamSendSettled { .. }
    ));
    daemon.stop().await;
    first.stop().await;
    second.stop().await;
    declined.stop().await;
    preflight_host.stop().await;
    pending.stop().await;
}
