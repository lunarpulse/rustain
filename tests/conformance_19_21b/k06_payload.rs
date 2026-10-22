use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::tui::app::InputAction;
use rustain::domain::events::DomainInputEvent;
use rustain::domain::models::{JournalRecord, ResponseMode, RoomEvent};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::observed_host::{HttpCounts, ObservedHost};
use crate::proposal_harness::ProposalFixture;

const BODY: &str = " \tArchitecture note — café 🛰️\n\tsecond row\nlast row \t ";
const CONVERSATION_MARKER: &str = "CONVERSATION_CONTEXT_MUST_NOT_LEAK_7ef4";
const SELECTED_PEER_MARKER: &str = "SELECTED_PEER_ITEM_MUST_NOT_LEAK_91ac";

#[tokio::test]
async fn k06_approved_proposal_preserves_boundary_payload_bytes() {
    let lena = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let jun = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut fixture = ProposalFixture::start(vec![lena.peer("lena-po"), jun.peer("jun-dev")]).await;
    let mut context_turn = rustain::domain::models::Turn::user(
        format!("{CONVERSATION_MARKER} {SELECTED_PEER_MARKER}"),
        1_800_000_000_000,
    );
    context_turn.history_source = rustain::domain::models::HistorySource::PeerDisplay;
    fixture.conversation.turns.push(context_turn);
    fixture
        .state
        .recipient_context_ids
        .insert(SELECTED_PEER_MARKER.to_owned());
    lena.reset_counts();
    jun.reset_counts();

    let result = fixture
        .submit(json!({
            "recipients": ["lena-po", "jun-dev"],
            "type": "architecture_update",
            "body": BODY
        }))
        .await;
    assert!(
        result.starts_with("Proposal shown to the operator."),
        "{result}"
    );
    let card = fixture.paint(100, 32);
    assert!(
        card.iter()
            .any(|line| line.contains("To: lena-po, jun-dev")),
        "{card:#?}"
    );
    assert!(
        card.iter()
            .any(|line| line.contains("Type: architecture_update")),
        "{card:#?}"
    );
    assert_eq!(lena.counts(), HttpCounts { get: 0, post: 0 });
    assert_eq!(jun.counts(), HttpCounts { get: 0, post: 0 });

    let decision = fixture.key(DomainInputEvent::KeyPress('y')).await;
    assert!(
        matches!(decision, InputAction::TeamProposal { .. }),
        "{decision:?}"
    );
    let proposal_block = fixture
        .state
        .team_send_blocks
        .keys()
        .next()
        .expect("pending proposal send")
        .clone();
    fixture.settle(2).await;
    let proposal_rows = rendered_send_rows(&fixture, &proposal_block);
    assert_eq!(
        proposal_rows,
        vec![
            ("lena-po".to_owned(), "Delivered"),
            ("jun-dev".to_owned(), "Delivered")
        ]
    );

    let proposal_posts_lena = lena.posts();
    let proposal_posts_jun = jun.posts();
    assert_eq!(
        proposal_posts_lena.len(),
        1,
        "lena-po actual forwarded POST"
    );
    assert_eq!(proposal_posts_jun.len(), 1, "jun-dev actual forwarded POST");
    assert_eq!(lena.counts(), HttpCounts { get: 0, post: 1 });
    assert_eq!(jun.counts(), HttpCounts { get: 0, post: 1 });
    for post in proposal_posts_lena.iter().chain(proposal_posts_jun.iter()) {
        let wire: Value = serde_json::from_slice(post.body()).expect("captured POST is JSON");
        assert_wire_payload(&wire, BODY);
        assert_no_forbidden_wire_fields(&wire);
        assert_no_forbidden_headers(post.headers());
        assert!(
            post.headers().get("content-type").is_some(),
            "captured POST content type"
        );
    }
    let proposal_facts = stable_send_facts(&fixture).await;
    assert_eq!(event_count(&proposal_facts, "team_send_action_recorded"), 1);
    assert_eq!(
        dispatch_authorship(&proposal_facts),
        vec!["agent-composed", "agent-composed"]
    );
    let proposal_action_id = action_id(&proposal_facts, 0);
    let proposal_action_facts = facts_for_action(&proposal_facts, &proposal_action_id);
    assert_action_facts(&proposal_action_facts, BODY);
    let proposal_received_lena = received_facts(&lena).await;
    let proposal_received_jun = received_facts(&jun).await;
    assert_recipient_received(&proposal_received_lena, BODY);
    assert_recipient_received(&proposal_received_jun, BODY);

    fixture.typed("/team board --last").await;
    fixture.typed("/team log").await;
    let proposal_board = &fixture.state.feedback_blocks
        [rustain::adapters::tui::handlers::team_command::TEAM_BOARD_BLOCK_ID]
        .message;
    assert!(
        proposal_board.contains("lena-po") && proposal_board.contains("jun-dev"),
        "board --last projection: {proposal_board}"
    );
    let proposal_log = &fixture.state.feedback_blocks
        [rustain::adapters::tui::handlers::team_command::TEAM_LOG_BLOCK_ID]
        .message;
    assert_eq!(
        proposal_log.matches("[agent-composed]").count(),
        2,
        "approved proposal log tag: {proposal_log}"
    );

    lena.stop().await;
    jun.stop().await;
}
const PARITY_BODY: &str = "Architecture note — café 🛰️\n\tsecond row\nlast row";

#[tokio::test]
async fn k06_proposal_matches_typed_send_for_representable_body() {
    let lena = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let jun = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut fixture = ProposalFixture::start(vec![lena.peer("lena-po"), jun.peer("jun-dev")]).await;
    let mut context_turn = rustain::domain::models::Turn::user(
        format!("{CONVERSATION_MARKER} {SELECTED_PEER_MARKER}"),
        1_800_000_000_000,
    );
    context_turn.history_source = rustain::domain::models::HistorySource::PeerDisplay;
    fixture.conversation.turns.push(context_turn);
    fixture
        .state
        .recipient_context_ids
        .insert(SELECTED_PEER_MARKER.to_owned());
    lena.reset_counts();
    jun.reset_counts();

    let result = fixture
        .submit(json!({
            "recipients": ["lena-po", "jun-dev"],
            "type": "architecture_update",
            "body": PARITY_BODY
        }))
        .await;
    assert!(
        result.starts_with("Proposal shown to the operator."),
        "{result}"
    );
    let card = fixture.paint(60, 16);
    assert!(
        card.iter()
            .any(|line| line.contains("To: lena-po, jun-dev")),
        "proposal aliases must remain complete and ordered at 60 columns: {card:#?}"
    );
    assert!(
        card.iter()
            .any(|line| line.contains("Type: architecture_update")),
        "proposal type must remain complete at 60 columns: {card:#?}"
    );
    assert_eq!(lena.counts(), HttpCounts { get: 0, post: 0 });
    assert_eq!(jun.counts(), HttpCounts { get: 0, post: 0 });

    let decision = fixture.key(DomainInputEvent::KeyPress('y')).await;
    assert!(
        matches!(decision, InputAction::TeamProposal { .. }),
        "{decision:?}"
    );
    let proposal_block = fixture
        .state
        .team_send_blocks
        .keys()
        .next()
        .expect("pending proposal send")
        .clone();
    fixture.settle(2).await;
    let proposal_rows = rendered_send_rows(&fixture, &proposal_block);
    assert_eq!(
        proposal_rows,
        vec![
            ("lena-po".to_owned(), "Delivered"),
            ("jun-dev".to_owned(), "Delivered")
        ]
    );
    for host in [&lena, &jun] {
        assert_eq!(host.counts(), HttpCounts { get: 0, post: 1 });
        let posts = host.posts();
        assert_eq!(posts.len(), 1);
        let wire: Value =
            serde_json::from_slice(posts[0].body()).expect("actual forwarded proposal POST");
        assert_wire_payload(&wire, PARITY_BODY);
        assert_no_forbidden_wire_fields(&wire);
        assert_no_forbidden_headers(posts[0].headers());
        assert!(posts[0].headers().get("content-type").is_some());
    }
    let proposal_facts = stable_send_facts(&fixture).await;
    assert_eq!(
        event_count(&proposal_facts, "team_send_action_recorded"),
        1,
        "approved proposal must record one action through the operator coordinator"
    );
    assert_eq!(
        dispatch_authorship(&proposal_facts),
        vec!["agent-composed", "agent-composed"],
        "approved proposal must remain agent-composed"
    );
    let proposal_action_facts = facts_for_action(&proposal_facts, &action_id(&proposal_facts, 0));
    assert_action_facts(&proposal_action_facts, PARITY_BODY);
    let proposal_received_lena = received_facts(&lena).await;
    let proposal_received_jun = received_facts(&jun).await;
    assert_recipient_received(&proposal_received_lena, PARITY_BODY);
    assert_recipient_received(&proposal_received_jun, PARITY_BODY);

    fixture.typed("/team board --last").await;
    let board = &fixture.state.feedback_blocks
        [rustain::adapters::tui::handlers::team_command::TEAM_BOARD_BLOCK_ID]
        .message;
    assert!(
        board.contains("lena-po") && board.contains("jun-dev"),
        "board --last projection: {board}"
    );
    let before_typed = [lena.counts(), jun.counts()];
    for host in [&lena, &jun] {
        for post in host.posts().iter().skip(1) {
            let read: Value = serde_json::from_slice(post.body()).expect("board RPC");
            assert_eq!(
                read["method"], "x-rustain-items/list",
                "the intervening board read must not be another message send"
            );
        }
    }

    fixture
        .typed(&format!(
            "/team send --type architecture_update lena-po,jun-dev {PARITY_BODY}"
        ))
        .await;
    let typed_block = fixture
        .state
        .team_send_blocks
        .keys()
        .next()
        .expect("pending typed send")
        .clone();
    fixture.settle(2).await;
    let typed_rows = rendered_send_rows(&fixture, &typed_block);
    assert_eq!(
        typed_rows, proposal_rows,
        "the typed action's completed rows match the approved proposal"
    );
    for (host, before) in [&lena, &jun].into_iter().zip(before_typed) {
        assert_eq!(
            host.counts(),
            HttpCounts {
                get: before.get,
                post: before.post + 1
            }
        );
        let posts = host.posts();
        assert_eq!(
            posts.len(),
            before.post + 1,
            "exactly one new POST for the typed send"
        );
        let post = &posts[before.post];
        let wire: Value = serde_json::from_slice(post.body()).expect("actual forwarded typed POST");
        assert_eq!(wire["method"], "message/send");
        assert_wire_payload(&wire, PARITY_BODY);
        assert_no_forbidden_wire_fields(&wire);
        assert_no_forbidden_headers(post.headers());
    }
    let typed_facts = stable_send_facts(&fixture).await;
    assert_eq!(event_count(&typed_facts, "team_send_action_recorded"), 2);
    assert_eq!(
        dispatch_authorship(&typed_facts),
        vec![
            "agent-composed",
            "agent-composed",
            "human-written",
            "human-written"
        ]
    );
    let typed_action_facts = facts_for_action(&typed_facts, &action_id(&typed_facts, 1));
    assert_action_facts(&typed_action_facts, PARITY_BODY);
    assert_eq!(
        normalize_for_equivalence(&proposal_action_facts),
        normalize_for_equivalence(&typed_action_facts)
    );

    let typed_received_lena = received_facts(&lena).await;
    let typed_received_jun = received_facts(&jun).await;
    assert_recipient_received(&typed_received_lena[1..], PARITY_BODY);
    assert_recipient_received(&typed_received_jun[1..], PARITY_BODY);
    assert_eq!(
        normalize_recipient_facts(&proposal_received_lena),
        normalize_recipient_facts(&typed_received_lena[1..])
    );
    assert_eq!(
        normalize_recipient_facts(&proposal_received_jun),
        normalize_recipient_facts(&typed_received_jun[1..])
    );

    fixture.typed("/team log").await;
    let log = &fixture.state.feedback_blocks
        [rustain::adapters::tui::handlers::team_command::TEAM_LOG_BLOCK_ID]
        .message;
    assert_eq!(
        log.matches("[agent-composed]").count(),
        2,
        "only proposal dispatches are tagged: {log}"
    );

    lena.stop().await;
    jun.stop().await;
}

fn rendered_send_rows(fixture: &ProposalFixture, block_id: &str) -> Vec<(String, &'static str)> {
    fixture.state.feedback_blocks[block_id]
        .message
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let icon = fields.next()?;
            if !matches!(icon, "●" | "✗" | "⚠") {
                return None;
            }
            let alias = fields.next()?.to_owned();
            let status = match icon {
                "●" => "Delivered",
                "✗" => "Declined",
                "⚠" => "Unreachable",
                _ => unreachable!(),
            };
            Some((alias, status))
        })
        .collect()
}

async fn stable_send_facts(fixture: &ProposalFixture) -> Vec<Value> {
    fixture
        .sender
        .rows()
        .await
        .into_iter()
        .filter_map(|entry| {
            let JournalRecord::Room(event) = entry.record else {
                return None;
            };
            matches!(
                event,
                RoomEvent::TeamSendActionRecorded { .. }
                    | RoomEvent::RemoteEnvelopeDispatched { .. }
                    | RoomEvent::TeamSendRecipientSettled { .. }
            )
            .then(|| {
                json!({
                    "recorded_at_ms": entry.recorded_at_ms,
                    "event": serde_json::to_value(event).expect("room event JSON")
                })
            })
        })
        .collect()
}

fn event_count(facts: &[Value], tag: &str) -> usize {
    facts
        .iter()
        .filter(|row| row.pointer("/event/event").and_then(Value::as_str) == Some(tag))
        .count()
}

fn action_id(facts: &[Value], index: usize) -> String {
    facts
        .iter()
        .filter(|row| {
            row.pointer("/event/event").and_then(Value::as_str) == Some("team_send_action_recorded")
        })
        .nth(index)
        .expect("action fact")
        .pointer("/event/action")
        .and_then(Value::as_str)
        .expect("action id")
        .to_owned()
}

fn facts_for_action(facts: &[Value], id: &str) -> Vec<Value> {
    facts
        .iter()
        .filter(|row| {
            let event = &row["event"];
            match event["event"].as_str() {
                Some("team_send_action_recorded") => event["action"] == id,
                Some("remote_envelope_dispatched") => {
                    event.pointer("/action/action").and_then(Value::as_str) == Some(id)
                }
                Some("team_send_recipient_settled") => event["action"] == id,
                _ => false,
            }
        })
        .cloned()
        .collect()
}

fn normalize_for_equivalence(facts: &[Value]) -> Value {
    let mut normalized = Value::Array(facts.to_vec());
    for row in normalized.as_array_mut().expect("facts array") {
        let row = row.as_object_mut().expect("journal fact wrapper");
        row.remove("recorded_at_ms");
        let event = row.get_mut("event").unwrap().as_object_mut().unwrap();
        match event.get("event").and_then(Value::as_str) {
            Some("team_send_action_recorded") => {
                event.remove("action");
            }
            Some("remote_envelope_dispatched") => {
                event.remove("task");
                event.remove("authorship");
                if let Some(action) = event.get_mut("action").and_then(Value::as_object_mut) {
                    action.remove("action");
                }
            }
            Some("team_send_recipient_settled") => {
                event.remove("action");
                if let Some(settlement) = event.get_mut("settlement").and_then(Value::as_object_mut)
                {
                    match settlement.get("outcome").and_then(Value::as_str) {
                        Some("delivered") => {
                            settlement.remove("item");
                        }
                        Some("asked_question") => {
                            settlement.remove("task_id");
                        }
                        _ => {}
                    }
                }
            }
            other => panic!("unexpected send fact {other:?}"),
        }
    }
    normalized
        .as_array_mut()
        .expect("facts array")
        .sort_by_key(|row| {
            let event = &row["event"];
            match event["event"].as_str() {
                Some("team_send_action_recorded") => (0u8, 0u64),
                Some("remote_envelope_dispatched") => (
                    1,
                    event
                        .pointer("/action/index")
                        .and_then(Value::as_u64)
                        .expect("dispatch recipient index"),
                ),
                Some("team_send_recipient_settled") => (
                    2,
                    event["index"].as_u64().expect("settlement recipient index"),
                ),
                _ => unreachable!("send fact tag checked before sorting"),
            }
        });
    normalized
}

fn assert_action_facts(facts: &[Value], body: &str) {
    let events = facts
        .iter()
        .filter_map(|row| row.get("event"))
        .collect::<Vec<_>>();
    let action = events
        .iter()
        .find(|event| event["event"] == "team_send_action_recorded")
        .expect("send action record");
    let expected_hash = text_hash(body);
    assert_eq!(
        action["body_hash"].as_str(),
        Some(expected_hash.as_str()),
        "action hash is the exact body hash"
    );
    assert_eq!(action["message_type"], "architecture_update");
    assert!(
        action.get("subject").is_none() || action["subject"].is_null(),
        "subject is sender-local and absent for this action"
    );
    assert_eq!(action["body_bytes"], body.len());
    assert_eq!(
        action["recipients"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["alias"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["lena-po", "jun-dev"]
    );
    let mut dispatches = events
        .iter()
        .filter(|event| event["event"] == "remote_envelope_dispatched")
        .collect::<Vec<_>>();
    assert_eq!(dispatches.len(), 2);
    dispatches.sort_by_key(|event| {
        event
            .pointer("/action/index")
            .and_then(Value::as_u64)
            .expect("dispatch recipient index")
    });
    assert_eq!(
        dispatches
            .iter()
            .map(|dispatch| dispatch.pointer("/action/index").and_then(Value::as_u64))
            .collect::<Vec<_>>(),
        [Some(0), Some(1)]
    );
    for (dispatch, recipient) in dispatches
        .into_iter()
        .zip(action["recipients"].as_array().unwrap())
    {
        assert_eq!(
            dispatch["peer"], recipient["peer"],
            "dispatch follows frozen roster order"
        );
        assert_eq!(dispatch["bytes"], body.len());
        assert_eq!(dispatch["content_hash"], action["body_hash"]);
    }
}
fn text_hash(text: &str) -> String {
    // Matches driver::text_content_hash: serialize the text as JSON, then SHA-256.
    let digest: [u8; 32] = Sha256::digest(serde_json::to_vec(text).expect("hash text JSON")).into();
    rustain::domain::models::ContentHash::from_bytes(digest).to_string()
}

fn dispatch_authorship(facts: &[Value]) -> Vec<&str> {
    facts
        .iter()
        .filter(|row| {
            row.pointer("/event/event").and_then(Value::as_str)
                == Some("remote_envelope_dispatched")
        })
        .map(|row| {
            row.pointer("/event/authorship")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
        })
        .collect()
}

fn assert_wire_payload(wire: &Value, body: &str) {
    let encoded = wire.to_string();
    assert!(
        !encoded.contains(CONVERSATION_MARKER),
        "conversation context leaked onto wire: {encoded}"
    );
    assert!(
        !encoded.contains(SELECTED_PEER_MARKER),
        "selected peer context leaked onto wire: {encoded}"
    );
    assert_eq!(
        wire.pointer("/params/message/parts/0/text")
            .and_then(Value::as_str)
            .expect("A2A text part")
            .as_bytes(),
        body.as_bytes(),
        "approved proposal must preserve exact body bytes"
    );
    assert_eq!(
        wire.pointer("/params/message/metadata/x-rustain-message-type")
            .and_then(Value::as_str),
        Some("architecture_update")
    );
}

fn assert_no_forbidden_wire_fields(value: &Value) {
    match value {
        Value::Object(object) => {
            for key in object.keys() {
                let normalized = key.to_ascii_lowercase().replace('-', "_");
                assert!(
                    !["subject", "context"].contains(&normalized.as_str())
                        && !normalized.contains("authorship")
                        && !normalized.contains("send_authorization")
                        && !normalized.contains("sendauthorization")
                        && !normalized.contains("proposal"),
                    "forbidden wire field {key}"
                );
            }
            for child in object.values() {
                assert_no_forbidden_wire_fields(child);
            }
        }
        Value::Array(values) => {
            for child in values {
                assert_no_forbidden_wire_fields(child);
            }
        }
        _ => {}
    }
}
fn assert_no_forbidden_headers(headers: &axum::http::HeaderMap) {
    for name in headers.keys() {
        let normalized = name.as_str().to_ascii_lowercase().replace('-', "_");
        assert!(
            !["subject", "context"].contains(&normalized.as_str())
                && !normalized.contains("authorship")
                && !normalized.contains("send_authorization")
                && !normalized.contains("sendauthorization")
                && !normalized.contains("proposal"),
            "forbidden wire header {name}"
        );
    }
}

async fn received_facts(host: &ObservedHost) -> Vec<Value> {
    host.journal()
        .entries()
        .await
        .into_iter()
        .filter_map(|entry| {
            let JournalRecord::Room(event @ RoomEvent::RecipientItemReceived { .. }) = entry.record
            else {
                return None;
            };
            Some(json!({
                "recorded_at_ms": entry.recorded_at_ms,
                "event": serde_json::to_value(event).expect("recipient journal event JSON")
            }))
        })
        .collect()
}

fn assert_recipient_received(received: &[Value], body: &str) {
    assert_eq!(received.len(), 1, "one real recipient journal item");
    let content = received[0]
        .pointer("/event/content")
        .and_then(Value::as_str)
        .expect("received content");
    assert_eq!(content.as_bytes(), body.as_bytes());
    assert_eq!(
        text_hash(content),
        text_hash(body),
        "recipient-journal content hash matches sent body hash"
    );
    assert_eq!(
        received[0]
            .pointer("/event/message_type")
            .and_then(Value::as_str),
        Some("architecture_update")
    );
}
fn normalize_recipient_facts(received: &[Value]) -> Value {
    let mut normalized = Value::Array(received.to_vec());
    for fact in normalized.as_array_mut().expect("recipient facts array") {
        let fact = fact.as_object_mut().unwrap();
        fact.remove("recorded_at_ms");
        let event = fact.get_mut("event").unwrap().as_object_mut().unwrap();
        event.remove("task");
        // The recipient persists only a hash of the x-rustain-item-capability,
        // itself derived from the generated outbound message id. Preserve every
        // other received-event field (including the address principal).
        event.remove("capability_hash");
        if let Some(address) = event.get_mut("address").and_then(Value::as_object_mut) {
            address.remove("item");
        }
    }
    normalized
}
