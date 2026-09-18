use rustain::domain::models::{ItemAddress, ItemId, ItemPrincipal, PeerId, RoomEvent};

fn peer(seed: u8) -> PeerId {
    PeerId::from_public_key(&[seed; 32]).expect("fixed key")
}

#[test]
fn recipient_item_events_round_trip_without_falling_into_unrecognized() {
    let address = ItemAddress::from_a2a_ingress(peer(7), ItemId::from_replay("item-1"));
    let events = [
        RoomEvent::RecipientItemReceived {
            address: address.clone(),
            task: "sender-message-1".to_owned(),
            alias: None,
            content: "review the patch".to_owned(),
        },
        RoomEvent::RecipientItemAcknowledged {
            address,
            alias: None,
        },
    ];

    for event in events {
        let encoded = serde_json::to_value(&event).expect("event serializes");
        let decoded: RoomEvent = serde_json::from_value(encoded).expect("event replays");
        assert!(
            matches!(
                decoded,
                RoomEvent::RecipientItemReceived { .. }
                    | RoomEvent::RecipientItemAcknowledged { .. }
            ),
            "the new event family must not be swallowed by RoomEvent::Unrecognized"
        );
    }
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
    use rustain::domain::models::RecipientItemAllocator;

    let allocator = RecipientItemAllocator::from_addresses([]);
    let principal = peer(9);
    let first = allocator
        .allocate_a2a_ingress(principal.clone())
        .await
        .expect("first allocation");
    let second = allocator
        .allocate_a2a_ingress(principal)
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
        address: address.clone(),
        task: "sender-task".to_owned(),
        alias: None,
        content: "durable content".to_owned(),
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
    assert_eq!(item.state, RecipientItemState::Acknowledged);
    assert_eq!(item.content, "durable content");

    let replayed = JournalRecipientItemProjection::from_entries(&entries);
    assert_eq!(projection.snapshot(), replayed.snapshot());
}

#[test]
fn replacing_from_the_journal_discards_an_in_memory_divergence() {
    use rustain::adapters::policy::recipient_item::JournalRecipientItemProjection;
    use rustain::domain::models::RecipientItemState;

    let address = ItemAddress::from_a2a_ingress(peer(11), ItemId::from_replay("ri_refold"));
    let received = RoomEvent::RecipientItemReceived {
        address: address.clone(),
        task: "sender-task".to_owned(),
        alias: None,
        content: "durable content".to_owned(),
    };
    let entries = vec![entry(1, received.clone())];
    let projection = JournalRecipientItemProjection::from_entries(&entries);
    projection.apply(&RoomEvent::RecipientItemAcknowledged {
        address: address.clone(),
        alias: None,
    });
    assert_eq!(
        projection.get(&address).unwrap().state,
        RecipientItemState::Acknowledged
    );

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
        address,
        task: "resent-message-id".to_owned(),
        alias: None,
        content: "durable content".to_owned(),
    };
    let entries = vec![entry(1, received(first)), entry(2, received(second.clone()))];

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
