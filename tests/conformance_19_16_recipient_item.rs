use rustain::adapters::policy::recipient_item::{JournalRecipientItemProjection, ThreadTurn};
use rustain::domain::models::{
    AgentId, Authorship, ContentHash, DispatchAct, ItemAddress, ItemId, ItemPrincipal,
    JournalEntry, PeerId, ReplyKind, ReplyTurnId, RoomEvent, TurnAddress,
};

fn peer(seed: u8) -> PeerId {
    PeerId::from_public_key(&[seed; 32]).expect("fixed key")
}

#[test]
fn recipient_item_events_round_trip_without_falling_into_unrecognized() {
    let address = ItemAddress::from_a2a_ingress(peer(7), ItemId::from_replay("item-1"));
    let events = [
        RoomEvent::RecipientItemReceived {
            message_type: rustain::domain::models::SemanticMessageType::Unknown,
            address: address.clone(),
            task: "sender-message-1".to_owned(),
            alias: None,
            content: "review the patch".to_owned(),
            in_reply_to: None,
            capability_hash: None,
            reply_capability_matched: false,
        },
        RoomEvent::RecipientItemAcknowledged {
            address: address.clone(),
            alias: None,
        },
        RoomEvent::RecipientItemRemoved {
            address: address.clone(),
        },
        // Story 19.16d AC3(a) — the retract fact class joins the family.
        RoomEvent::RecipientItemRetracted {
            address,
            retracted_at_ms: 1_700_000_000_000,
            principal_collapsed: true,
        },
    ];

    for event in events {
        let encoded = serde_json::to_value(&event).expect("event serializes");
        if matches!(event, RoomEvent::RecipientItemRetracted { .. }) {
            // The durable tag other builds and the pinned transparency fixture
            // name. ⛔ A respelling would round-trip in THIS build and fall
            // into `Unrecognized` in every journal written before it.
            assert_eq!(encoded["event"], "recipient_item_retracted", "{encoded}");
        }
        let decoded: RoomEvent = serde_json::from_value(encoded).expect("event replays");
        assert!(
            matches!(
                decoded,
                RoomEvent::RecipientItemReceived { .. }
                    | RoomEvent::RecipientItemAcknowledged { .. }
                    | RoomEvent::RecipientItemRemoved { .. }
                    | RoomEvent::RecipientItemRetracted { .. }
            ),
            "the new event family must not be swallowed by RoomEvent::Unrecognized"
        );
        assert_eq!(decoded, event, "every field survives the round trip");
    }
}

#[test]
fn reply_thread_journal_fields_round_trip_and_legacy_omissions_default_to_none() {
    let address = ItemAddress::from_a2a_ingress(peer(18), ItemId::from_replay("ri_question"));
    let content_hash = ContentHash::from_bytes([0x18; 32]);

    let received = RoomEvent::RecipientItemReceived {
        message_type: rustain::domain::models::SemanticMessageType::Unknown,
        address: address.clone(),
        task: "follow-up-task".to_owned(),
        alias: Some("peer eighteen".to_owned()),
        content: "follow-up question".to_owned(),
        in_reply_to: Some("rr_previous".to_owned()),
        capability_hash: None,
        reply_capability_matched: false,
    };
    let mut legacy_received = serde_json::to_value(&received).expect("serialize received item");
    assert_eq!(legacy_received["in_reply_to"], "rr_previous");
    legacy_received
        .as_object_mut()
        .expect("event object")
        .remove("in_reply_to");
    assert!(matches!(
        serde_json::from_value::<RoomEvent>(legacy_received).expect("pre-reply journal replays"),
        RoomEvent::RecipientItemReceived {
            in_reply_to: None,
            ..
        }
    ));
    assert!(matches!(
        serde_json::from_value::<RoomEvent>(
            serde_json::to_value(received).expect("serialize received item")
        )
        .expect("received item replays"),
        RoomEvent::RecipientItemReceived {
            in_reply_to: Some(in_reply_to),
            ..
        } if in_reply_to == "rr_previous"
    ));

    let dispatched = RoomEvent::RemoteEnvelopeDispatched {
        peer: peer(19),
        task: Some("outbound-task".to_owned()),
        bytes: 17,
        act: DispatchAct::Task,
        content_hash: Some(content_hash),
        endpoint: None,
        action: None,
        authorship: Authorship::Unknown,
        send_authorization: rustain::domain::models::SendAuthorization::Unknown,
    };
    let mut legacy_dispatched =
        serde_json::to_value(&dispatched).expect("serialize outbound dispatch");
    assert_eq!(
        legacy_dispatched["content_hash"],
        serde_json::to_value(content_hash).expect("serialize content hash")
    );
    legacy_dispatched
        .as_object_mut()
        .expect("event object")
        .remove("content_hash");
    assert!(matches!(
        serde_json::from_value::<RoomEvent>(legacy_dispatched)
            .expect("pre-content-hash dispatch replays"),
        RoomEvent::RemoteEnvelopeDispatched {
            content_hash: None,
            ..
        }
    ));
    assert!(matches!(
        serde_json::from_value::<RoomEvent>(
            serde_json::to_value(dispatched).expect("serialize outbound dispatch")
        )
        .expect("outbound dispatch replays"),
        RoomEvent::RemoteEnvelopeDispatched {
            content_hash: Some(persisted_hash),
            ..
        } if persisted_hash == content_hash
    ));

    let replied = RoomEvent::RecipientItemReplied {
        address: TurnAddress::answering(&address, ReplyTurnId::from_replay("rr_answer")),
        answers: address.item().clone(),
        content: "durable answer".to_owned(),
        authorship: Authorship::AgentComposed,
        send_authorization: rustain::domain::models::SendAuthorization::HumanDecided,
        kind: ReplyKind::Answer,
        via_node: Some(AgentId::parse("reply-node").expect("valid reply node")),
    };
    let mut legacy_replied = serde_json::to_value(&replied).expect("serialize reply turn");
    assert_eq!(legacy_replied["via_node"], "reply-node");
    legacy_replied
        .as_object_mut()
        .expect("event object")
        .remove("via_node");
    assert!(matches!(
        serde_json::from_value::<RoomEvent>(legacy_replied).expect("pre-via-node reply replays"),
        RoomEvent::RecipientItemReplied { via_node: None, .. }
    ));
    assert!(matches!(
        serde_json::from_value::<RoomEvent>(
            serde_json::to_value(replied).expect("serialize reply turn")
        )
        .expect("reply turn replays"),
        RoomEvent::RecipientItemReplied {
            answers,
            content,
            authorship: Authorship::AgentComposed,
            via_node: Some(via_node),
            ..
        } if answers.as_str() == "ri_question"
            && content == "durable answer"
            && via_node.as_str() == "reply-node"
    ));

    let observed = RoomEvent::PeerReplyObserved {
        peer: peer(20),
        endpoint: None,
        turn: "rr_remote".to_owned(),
        answers: Some("ri_question".to_owned()),
        content: Some("peer answer".to_owned()),
        content_hash: Some(content_hash),
    };
    let mut legacy_observed = serde_json::to_value(&observed).expect("serialize observed reply");
    assert_eq!(
        legacy_observed["content_hash"],
        serde_json::to_value(content_hash).expect("serialize content hash")
    );
    for field in ["answers", "content", "content_hash"] {
        legacy_observed
            .as_object_mut()
            .expect("event object")
            .remove(field);
    }
    assert!(matches!(
        serde_json::from_value::<RoomEvent>(legacy_observed)
            .expect("pre-observed-reply journal replays"),
        RoomEvent::PeerReplyObserved {
            answers: None,
            content: None,
            content_hash: None,
            ..
        }
    ));
    assert!(matches!(
        serde_json::from_value::<RoomEvent>(
            serde_json::to_value(observed).expect("serialize observed reply")
        )
        .expect("observed reply replays"),
        RoomEvent::PeerReplyObserved {
            turn,
            answers: Some(answers),
            content: Some(content),
            content_hash: Some(persisted_hash),
            ..
        } if turn == "rr_remote"
            && answers == "ri_question"
            && content == "peer answer"
            && persisted_hash == content_hash
    ));
}

/// Story 19.18c `AC3(c)` — the capability fields round-trip, and their
/// absence (`None`/`false`) serializes to exactly a 19.18 row: the key set
/// is unchanged and every existing key keeps its value, so the pinned
/// transparency fixtures and every older journal line stay byte-identical.
#[test]
fn item_capability_fields_round_trip_and_absence_serializes_as_a_19_18_row() {
    let address = ItemAddress::from_a2a_ingress(peer(21), ItemId::from_replay("ri_capable"));
    let hash = "ab".repeat(32);
    let received = RoomEvent::RecipientItemReceived {
        message_type: rustain::domain::models::SemanticMessageType::Unknown,
        address: address.clone(),
        task: "rustain-1-00000000000000aa".to_owned(),
        alias: None,
        content: "question".to_owned(),
        in_reply_to: Some("ri_root".to_owned()),
        capability_hash: Some(hash.clone()),
        reply_capability_matched: true,
    };
    let encoded = serde_json::to_value(&received).expect("serialize");
    assert_eq!(encoded["capability_hash"], hash.as_str());
    assert_eq!(encoded["reply_capability_matched"], true);
    assert_eq!(
        serde_json::from_value::<RoomEvent>(encoded).expect("replays"),
        received
    );

    let legacy = RoomEvent::RecipientItemReceived {
        message_type: rustain::domain::models::SemanticMessageType::Unknown,
        address: address.clone(),
        task: "sender-task".to_owned(),
        alias: None,
        content: "question".to_owned(),
        in_reply_to: None,
        capability_hash: None,
        reply_capability_matched: false,
    };
    let row = serde_json::to_string(&legacy).expect("serialize legacy");
    let value: serde_json::Value = serde_json::from_str(&row).expect("json");
    let keys: Vec<&str> = value
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        ["address", "alias", "content", "event", "task"],
        "a 19.18 row's key set: {row}"
    );
    assert!(!row.contains("capability"), "{row}");
    let mut older = value.clone();
    older.as_object_mut().expect("object").remove("alias");
    assert_eq!(
        serde_json::from_value::<RoomEvent>(value).expect("19.18 row replays"),
        legacy,
        "a row without the fields replays as legacy"
    );
    assert!(matches!(
        serde_json::from_value::<RoomEvent>(older).expect("older row replays"),
        RoomEvent::RecipientItemReceived {
            capability_hash: None,
            reply_capability_matched: false,
            ..
        }
    ));

    let unpinned = RoomEvent::RemoteEnvelopeDispatched {
        peer: peer(22),
        task: Some("rustain-1-00000000000000aa".to_owned()),
        bytes: 8,
        act: DispatchAct::Task,
        content_hash: None,
        endpoint: Some("cd".repeat(32)),
        action: None,
        authorship: Authorship::Unknown,
        send_authorization: rustain::domain::models::SendAuthorization::Unknown,
    };
    let encoded = serde_json::to_value(&unpinned).expect("serialize dispatch");
    assert_eq!(encoded["endpoint"], "cd".repeat(32));
    assert_eq!(
        serde_json::from_value::<RoomEvent>(encoded).expect("replays"),
        unpinned
    );
    let pinned = RoomEvent::RemoteEnvelopeDispatched {
        peer: peer(22),
        task: Some("t".to_owned()),
        bytes: 8,
        act: DispatchAct::Task,
        content_hash: None,
        endpoint: None,
        action: None,
        authorship: Authorship::Unknown,
        send_authorization: rustain::domain::models::SendAuthorization::Unknown,
    };
    let row = serde_json::to_value(&pinned).expect("serialize dispatch");
    assert_eq!(
        row.as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["bytes", "event", "peer", "task"],
        "a pinned send's row is a 19.18 row"
    );
}

#[test]
fn persisted_principal_has_a_forward_unknown_arm() {
    let unknown: ItemPrincipal = serde_json::from_value(serde_json::json!({
        "kind": "future_transport"
    }))
    .expect("a newer provenance remains replayable");
    assert!(matches!(unknown, ItemPrincipal::Unknown));
}

#[test]
fn item_address_preserves_principal_provenance_without_a_bare_peer_accessor() {
    let source = include_str!("../src/domain/models/recipient_item.rs");
    assert!(source.contains("pub enum ItemPrincipal"));
    assert!(!source.contains("type ItemPrincipal = PeerId"));
    assert!(!source.contains("-> PeerId"));
    assert!(!source.contains("impl From<PeerId>"));
}

#[tokio::test]
async fn recipient_allocator_mints_distinct_opaque_ids_per_principal() {
    use rustain::domain::models::{A2aIngressPrincipal, RecipientItemAllocator};

    let allocator = RecipientItemAllocator::from_addresses([]);
    let principal = peer(9);
    let first = allocator
        .allocate_a2a_ingress(A2aIngressPrincipal::Pseudonym(principal.clone()))
        .await
        .expect("first allocation");
    let second = allocator
        .allocate_a2a_ingress(A2aIngressPrincipal::Pseudonym(principal))
        .await
        .expect("second allocation");

    assert_ne!(first, second);
    assert!(first.item().as_str().starts_with("ri_"));
    assert!(first.item().as_str().len() >= 32);
    assert_ne!(first.item().as_str(), "sender-message-1");
}

fn entry(seq: u64, event: RoomEvent) -> rustain::domain::models::JournalEntry {
    rustain::domain::models::JournalEntry::new(
        seq,
        rustain::domain::models::JournalRecord::Room(event),
        seq as i64,
    )
}

#[test]
fn recipient_item_fold_replays_acknowledgement_and_is_idempotent() {
    use rustain::adapters::policy::recipient_item::JournalRecipientItemProjection;
    use rustain::domain::models::RecipientItemState;

    let address = ItemAddress::from_a2a_ingress(peer(10), ItemId::from_replay("ri_replay"));
    let received = RoomEvent::RecipientItemReceived {
        message_type: rustain::domain::models::SemanticMessageType::Unknown,
        address: address.clone(),
        task: "sender-task".to_owned(),
        alias: None,
        content: "durable content".to_owned(),
        in_reply_to: None,
        capability_hash: None,
        reply_capability_matched: false,
    };
    let acknowledged = RoomEvent::RecipientItemAcknowledged {
        address: address.clone(),
        alias: Some("local operator".to_owned()),
    };
    let entries = vec![
        entry(1, received.clone()),
        entry(2, received),
        entry(3, acknowledged.clone()),
        entry(4, acknowledged),
    ];
    let rows: Vec<_> = entries
        .iter()
        .filter_map(rustain::domain::services::transparency::transparency_row)
        .collect();
    assert!(rows.iter().any(|row| {
        row.kind == rustain::domain::services::transparency::TransparencyKind::RecipientItemReceived
            && row.task.as_deref() == Some("ri_replay")
    }));
    assert!(rows.iter().any(|row| {
        row.kind
            == rustain::domain::services::transparency::TransparencyKind::RecipientItemAcknowledged
            && row.task.as_deref() == Some("ri_replay")
    }));

    let projection = JournalRecipientItemProjection::from_entries(&entries);
    let item = projection.get(&address).expect("received item folds");
    assert!(matches!(
        item.state,
        RecipientItemState::Acknowledged {
            content,
            ..
        } if content == "durable content"
    ));

    let replayed = JournalRecipientItemProjection::from_entries(&entries);
    assert_eq!(projection.snapshot(), replayed.snapshot());
}

#[test]
fn replacing_from_the_journal_discards_an_in_memory_divergence() {
    use rustain::adapters::policy::recipient_item::JournalRecipientItemProjection;
    use rustain::domain::models::RecipientItemState;

    let address = ItemAddress::from_a2a_ingress(peer(11), ItemId::from_replay("ri_refold"));
    let received = RoomEvent::RecipientItemReceived {
        message_type: rustain::domain::models::SemanticMessageType::Unknown,
        address: address.clone(),
        task: "sender-task".to_owned(),
        alias: None,
        content: "durable content".to_owned(),
        in_reply_to: None,
        capability_hash: None,
        reply_capability_matched: false,
    };
    let entries = vec![entry(1, received.clone())];
    let projection = JournalRecipientItemProjection::from_entries(&entries);
    projection.apply(&RoomEvent::RecipientItemAcknowledged {
        address: address.clone(),
        alias: None,
    });
    assert!(matches!(
        projection.get(&address).unwrap().state,
        RecipientItemState::Acknowledged { .. }
    ));

    projection.replace_from(&entries);
    assert_eq!(
        projection,
        JournalRecipientItemProjection::from_entries(&entries),
        "the journal fold, not an in-memory mutation, is the source of truth"
    );
}

#[test]
fn find_by_task_binds_the_newest_item_when_a_message_id_is_resent() {
    // A2A clients retry with the same `messageId`; each retry mints a fresh
    // item. Restart recovery must bind the task to the LATEST execution's item,
    // never an arbitrary `HashMap` match.
    use rustain::adapters::policy::recipient_item::JournalRecipientItemProjection;

    let principal_peer = peer(12);
    let first =
        ItemAddress::from_a2a_ingress(principal_peer.clone(), ItemId::from_replay("ri_first"));
    let second =
        ItemAddress::from_a2a_ingress(principal_peer.clone(), ItemId::from_replay("ri_second"));
    let received = |address: ItemAddress| RoomEvent::RecipientItemReceived {
        message_type: rustain::domain::models::SemanticMessageType::Unknown,
        address,
        task: "resent-message-id".to_owned(),
        alias: None,
        content: "durable content".to_owned(),
        in_reply_to: None,
        capability_hash: None,
        reply_capability_matched: false,
    };
    let entries = vec![
        entry(1, received(first)),
        entry(2, received(second.clone())),
    ];

    let projection = JournalRecipientItemProjection::from_entries(&entries);
    let principal = ItemPrincipal::A2aPseudonym(principal_peer);
    assert_eq!(
        projection
            .find_by_task(&principal, "resent-message-id")
            .map(|item| item.address),
        Some(second),
        "a restarted task belongs to the newest execution's durable item"
    );
}

#[test]
fn received_semantic_type_survives_replay_without_trusting_body_claims() {
    use rustain::adapters::policy::recipient_item::JournalRecipientItemProjection;
    use rustain::domain::models::{RecipientItemState, SemanticMessageType};

    let principal_peer = peer(19);
    let legacy_address = ItemAddress::from_a2a_ingress(
        principal_peer.clone(),
        ItemId::from_replay("ri_untyped_legacy"),
    );
    let typed_address =
        ItemAddress::from_a2a_ingress(principal_peer.clone(), ItemId::from_replay("ri_typed"));
    let legacy_json = serde_json::json!({
        "event": "recipient_item_received",
        "address": legacy_address,
        "task": "legacy-task",
        "alias": null,
        "content": "consultation: this is only a body claim"
    });
    let legacy: RoomEvent =
        serde_json::from_value(legacy_json.clone()).expect("pre-type journal line replays");
    assert_eq!(
        serde_json::to_value(&legacy).expect("legacy serialization"),
        legacy_json,
        "default type must not change a pre-type record's serialized fields"
    );
    let typed: RoomEvent = serde_json::from_value(serde_json::json!({
        "event": "recipient_item_received",
        "address": typed_address,
        "task": "typed-task",
        "alias": null,
        "message_type": "consultation",
        "content": "bug_report: this contradicts the trusted type"
    }))
    .expect("typed journal line replays");
    let persisted = serde_json::to_vec(&vec![entry(1, legacy), entry(2, typed)])
        .expect("persist journal records");
    let replayed: Vec<rustain::domain::models::JournalEntry> =
        serde_json::from_slice(&persisted).expect("reload journal records");
    let projection = JournalRecipientItemProjection::from_entries(&replayed);
    let principal = ItemPrincipal::A2aPseudonym(principal_peer);
    let legacy = projection
        .find_by_task(&principal, "legacy-task")
        .expect("legacy item survives projection");
    let typed = projection
        .find_by_task(&principal, "typed-task")
        .expect("typed item survives projection");
    assert_eq!(legacy.message_type, SemanticMessageType::Unknown);
    assert_eq!(typed.message_type, SemanticMessageType::Consultation);
    assert!(matches!(
        typed.state,
        RecipientItemState::Received { content }
            if content == "bug_report: this contradicts the trusted type"
    ));
}

#[test]
fn reply_kind_journal_values_are_bounded_and_bad_tags_do_not_break_the_fold() {
    let principal = peer(41);
    let address = ItemAddress::from_a2a_ingress(principal.clone(), ItemId::from_replay("ri_root"));
    let root = RoomEvent::RecipientItemReceived {
        message_type: rustain::domain::models::SemanticMessageType::Unknown,
        address: address.clone(),
        task: "task".to_owned(),
        alias: None,
        content: "question".to_owned(),
        in_reply_to: None,
        capability_hash: None,
        reply_capability_matched: false,
    };
    let raw_reply = |id: &str| {
        serde_json::to_value(RoomEvent::RecipientItemReplied {
            address: TurnAddress::answering(&address, ReplyTurnId::from_replay(id)),
            answers: address.item().clone(),
            content: format!("body {id}"),
            authorship: Authorship::HumanWritten,
            send_authorization: rustain::domain::models::SendAuthorization::HumanDecided,
            kind: ReplyKind::Answer,
            via_node: None,
        })
        .expect("reply serializes")
    };

    let mut entries = vec![entry(1, root)];
    let cases = [
        (None, ReplyKind::Answer),
        (
            Some(serde_json::json!({"type":"defer","minutes":1})),
            ReplyKind::Defer { minutes: 1 },
        ),
        (
            Some(serde_json::json!({"type":"defer","minutes":65535})),
            ReplyKind::Defer { minutes: u16::MAX },
        ),
        (
            Some(serde_json::json!({"type":"future"})),
            ReplyKind::Unknown,
        ),
        (
            Some(serde_json::json!({"type":"defer"})),
            ReplyKind::Unknown,
        ),
        (
            Some(serde_json::json!({"type":"defer","minutes":null})),
            ReplyKind::Unknown,
        ),
        (
            Some(serde_json::json!({"type":"defer","minutes":"30"})),
            ReplyKind::Unknown,
        ),
        (
            Some(serde_json::json!({"type":"defer","minutes":0})),
            ReplyKind::Unknown,
        ),
        (
            Some(serde_json::json!({"type":"defer","minutes":-1})),
            ReplyKind::Unknown,
        ),
        (
            Some(serde_json::json!({"type":"defer","minutes":65536})),
            ReplyKind::Unknown,
        ),
        (Some(serde_json::json!(null)), ReplyKind::Unknown),
        (Some(serde_json::json!("defer")), ReplyKind::Unknown),
    ];
    let mut expected = Vec::new();
    for (index, (kind, expected_kind)) in cases.into_iter().enumerate() {
        let id = format!("rr_{index}");
        let mut encoded = raw_reply(&id);
        let object = encoded.as_object_mut().expect("event object");
        if let Some(kind) = kind {
            object.insert("kind".to_owned(), kind);
        } else {
            object.remove("kind");
        }
        let event: RoomEvent = serde_json::from_value(encoded).expect("kind cannot poison event");
        entries.push(entry(index as u64 + 2, event));
        expected.push(expected_kind);
    }
    let bytes = entries
        .iter()
        .map(|entry| serde_json::to_string(entry).expect("journal row encodes"))
        .collect::<Vec<_>>()
        .join("\n");
    let replayed: Vec<JournalEntry> = bytes
        .lines()
        .map(|line| serde_json::from_str(line).expect("journal row replays"))
        .collect();
    let projection = JournalRecipientItemProjection::from_entries(&replayed);
    let (_, turns) = projection
        .thread(
            &rustain::domain::models::ItemScope::exact(ItemPrincipal::A2aPseudonym(principal)),
            "ri_root",
        )
        .expect("complete thread remains");
    let actual: Vec<_> = turns
        .iter()
        .filter_map(|turn| match turn {
            ThreadTurn::Reply(reply) => Some(reply.kind),
            ThreadTurn::Item(_) => None,
        })
        .collect();
    assert_eq!(actual, expected);

    let answer = serde_json::to_value(ReplyKind::Answer).expect("answer kind serializes");
    assert_eq!(answer, serde_json::json!({"type":"answer"}));
    assert_eq!(
        serde_json::to_value(ReplyKind::Defer { minutes: 30 }).expect("defer serializes"),
        serde_json::json!({"type":"defer","minutes":30})
    );
    let answer_event = serde_json::to_value(RoomEvent::RecipientItemReplied {
        address: TurnAddress::answering(&address, ReplyTurnId::from_replay("rr_default")),
        answers: address.item().clone(),
        content: "answer".to_owned(),
        authorship: Authorship::HumanWritten,
        send_authorization: rustain::domain::models::SendAuthorization::HumanDecided,
        kind: ReplyKind::Answer,
        via_node: None,
    })
    .expect("event serializes");
    assert!(answer_event.get("kind").is_none(), "Answer remains omitted");

    let deferred_event = serde_json::to_value(RoomEvent::RecipientItemReplied {
        address: TurnAddress::answering(&address, ReplyTurnId::from_replay("rr_defer")),
        answers: address.item().clone(),
        content: "Busy — defer 30m".to_owned(),
        authorship: Authorship::HumanWritten,
        send_authorization: rustain::domain::models::SendAuthorization::HumanDecided,
        kind: ReplyKind::Defer { minutes: 30 },
        via_node: None,
    })
    .expect("defer event serializes");
    assert_eq!(
        deferred_event["kind"],
        serde_json::json!({"type":"defer","minutes":30})
    );

    assert_eq!(
        serde_json::from_str::<ReplyKind>(r#"{"type":"defer","minutes":30,"minutes":31}"#)
            .expect("duplicate field degrades safely"),
        ReplyKind::Unknown
    );
}

// Story 19.18d K09 (AC9, A4, P14, F10): the durable-journal cases, shared with
// the 19.18d CI target.
#[path = "conformance_19_18d/k09_journal.rs"]
mod k09_journal;
