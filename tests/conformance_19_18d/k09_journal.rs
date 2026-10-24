//! `K09` (Story 19.18d AC9, `A4`, `P14`, `F10`) — the durable journal.
//!
//! A bound address (`ItemAddress::from_a2a_bound` / `TurnAddress::answering`
//! of one) round-trips through every `RoomEvent` that carries an address and
//! through the production reader (`WorkspaceJournalReader` → `parse_entries`);
//! `Rap` / `A2aPseudonym` addresses stay byte-identical to the pre-19.18d form;
//! a pre-19.18d reader copy reads a bound line as `Unknown` and keeps the
//! journal intact (`P14`); a malformed / missing `boundPeer` reads `Unknown`
//! through the new reader instead of corrupting the journal (`F10`).
//!
//! Mutants: `M21`, `M51`, `M67`.
//!
//! This module needs no harness and is also mounted by
//! `conformance_19_16_recipient_item.rs` (Project Structure Notes).

use std::path::Path;

use rustain::domain::models::{
    AgentId, Authorship, ItemAddress, ItemId, ItemPrincipal, JournalEntry, JournalRecord,
    OrchestrationRoomId, PeerId, ReplyKind, ReplyTurnId, RoomEvent, TurnAddress,
};
use rustain::domain::ports::RoomJournalReader;
use rustain::infrastructure::subagent::WorkspaceJournalReader;
use serde_json::{Value, json};

const ROOM: &str = "room-k09";
const FIXTURE: &str =
    include_str!("../fixtures/transparency/FIXTURE_room_journal_current_18_2.jsonl");
/// `tests_tui/test_story_19_16g_log_reminder.py:45` (`PEER`) and `:116-121`
/// (`_retract`), the row that suite appends.
const TUI_PEER: &str = "12204bb06f8e4e3a7715d201d573d0aa423762e55dabd61a2c02278fa56cc6d294e0";

fn pin() -> PeerId {
    PeerId::from_public_key(&[0x9b; 32]).expect("fixed key")
}

fn other_peer() -> PeerId {
    PeerId::from_public_key(&[0x2c; 32]).expect("fixed key")
}

fn item(id: &str) -> ItemId {
    ItemId::from_replay(id)
}

fn bound_item_address() -> ItemAddress {
    ItemAddress::from_a2a_bound(pin(), item("ri_bound"))
}

fn bound_turn_address() -> TurnAddress {
    TurnAddress::answering(&bound_item_address(), ReplyTurnId::from_replay("rr_bound"))
}

/// A line the production writer would append for a harmless event; the
/// "known-good" follower every defective line is placed ahead of so
/// torn-tail tolerance (`Err(_) if is_last => break`) cannot hide a failure.
fn known_good(seq: u64) -> String {
    let entry = JournalEntry::new(
        seq,
        JournalRecord::Room(RoomEvent::ConsentRevoked {
            sender: Some(other_peer()),
            revoked_at: 7,
        }),
        1_700_000_000_000 + seq as i64,
    );
    serde_json::to_string(&entry).expect("JournalEntry serializes")
}

fn raw_line(seq: u64, payload: &impl std::fmt::Display) -> String {
    format!(
        r#"{{"schema_version":1,"seq":{seq},"recorded_at_ms":{},"record":{{"kind":"room","payload":{payload}}}}}"#,
        1_700_000_000_000_i64 + seq as i64
    )
}

fn bytes_of(lines: &[String]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for line in lines {
        bytes.extend_from_slice(line.as_bytes());
        bytes.push(b'\n');
    }
    bytes
}

/// Write `bytes` as the workspace room journal and load them through the
/// production reader.
async fn load(bytes: &[u8]) -> Vec<JournalEntry> {
    let workspace = tempfile::tempdir().expect("workspace");
    let rooms = workspace.path().join(".rustain").join("rooms");
    std::fs::create_dir_all(&rooms).expect("rooms dir");
    std::fs::write(rooms.join(format!("{ROOM}.jsonl")), bytes).expect("write journal");
    read_back(workspace.path()).await
}

async fn read_back(workspace: &Path) -> Vec<JournalEntry> {
    WorkspaceJournalReader::open(
        workspace,
        OrchestrationRoomId::parse(ROOM).expect("room id"),
    )
    .load_entries()
    .await
    .expect("the production reader loads the journal")
}

fn room_event(entry: &JournalEntry) -> &RoomEvent {
    match &entry.record {
        JournalRecord::Room(event) => event,
        other => panic!("expected a room record, got {other:?}"),
    }
}

fn principal_of(event: &RoomEvent) -> &ItemPrincipal {
    match event {
        RoomEvent::RecipientItemReceived { address, .. }
        | RoomEvent::RecipientItemAcknowledged { address, .. }
        | RoomEvent::RecipientItemRemoved { address }
        | RoomEvent::RecipientItemRetracted { address, .. } => address.principal(),
        RoomEvent::RecipientItemReplied { address, .. } => address.principal(),
        other => panic!("not an address-carrying event: {other:?}"),
    }
}

fn received(address: ItemAddress) -> RoomEvent {
    RoomEvent::RecipientItemReceived {
        stories: None,
        message_type: rustain::domain::models::SemanticMessageType::Unknown,
        address,
        task: "sender-message-1".to_owned(),
        alias: Some("ops".to_owned()),
        content: "review the patch".to_owned(),
        in_reply_to: None,
        capability_hash: None,
        reply_capability_matched: false,
        authorship: rustain::domain::models::Authorship::Unknown,
        send_authorization: rustain::domain::models::SendAuthorization::Unknown,
    }
}

fn replied(address: TurnAddress) -> RoomEvent {
    replied_with_kind(address, ReplyKind::Answer)
}

fn replied_with_kind(address: TurnAddress, kind: ReplyKind) -> RoomEvent {
    RoomEvent::RecipientItemReplied {
        address,
        answers: item("ri_bound"),
        content: "done".to_owned(),
        authorship: Authorship::Unknown,
        send_authorization: rustain::domain::models::SendAuthorization::Unknown,
        kind,
        via_node: Some(AgentId::root()),
    }
}

/// Every `RoomEvent` that carries an address, for one item address (and the
/// turn answering it).
fn address_events(address: &ItemAddress) -> Vec<RoomEvent> {
    vec![
        received(address.clone()),
        RoomEvent::RecipientItemAcknowledged {
            address: address.clone(),
            alias: Some("ops".to_owned()),
        },
        RoomEvent::RecipientItemRemoved {
            address: address.clone(),
        },
        RoomEvent::RecipientItemRetracted {
            address: address.clone(),
            retracted_at_ms: 1_700_000_000_000,
            principal_collapsed: false,
        },
        replied(TurnAddress::answering(
            address,
            ReplyTurnId::from_replay("rr_bound"),
        )),
    ]
}

#[tokio::test]
async fn k09_bound_addresses_round_trip_through_every_event_and_the_production_reader() {
    let address = bound_item_address();
    let events = address_events(&address);
    assert_eq!(events.len(), 5, "every address-carrying RoomEvent");

    // Wire shape (`A4`): the unit principal plus the sibling `boundPeer`.
    assert_eq!(
        serde_json::to_value(&address).expect("address"),
        json!({"principal": {"kind": "a2a_bound"}, "item": "ri_bound", "boundPeer": pin().as_str()})
    );
    assert_eq!(
        serde_json::to_value(bound_turn_address()).expect("turn"),
        json!({"principal": {"kind": "a2a_bound"}, "turn": "rr_bound", "boundPeer": pin().as_str()})
    );

    // Value round trip per event.
    for event in &events {
        let decoded: RoomEvent =
            serde_json::from_value(serde_json::to_value(event).expect("encode")).expect("decode");
        assert_eq!(&decoded, event);
        assert_eq!(principal_of(&decoded), &ItemPrincipal::A2aBound(pin()));
    }

    // Production byte reader: the writer's lines, in order, then a Rap and a
    // pseudonym control so the reader is not just echoing one shape.
    let mut rows: Vec<RoomEvent> = events.clone();
    rows.push(received(ItemAddress::from_rap_ingress(
        pin(),
        item("ri_rap"),
    )));
    rows.push(received(ItemAddress::from_a2a_ingress(
        pin(),
        item("ri_pseudo"),
    )));
    let lines: Vec<String> = rows
        .iter()
        .enumerate()
        .map(|(index, event)| {
            serde_json::to_string(&JournalEntry::new(
                index as u64 + 1,
                JournalRecord::Room(event.clone()),
                1_700_000_000_000 + index as i64,
            ))
            .expect("entry")
        })
        .collect();
    let loaded = load(&bytes_of(&lines)).await;
    assert_eq!(loaded.len(), rows.len(), "no line dropped");
    for (entry, expected) in loaded.iter().zip(&rows) {
        assert_eq!(room_event(entry), expected);
    }
    assert_eq!(
        principal_of(room_event(&loaded[5])),
        &ItemPrincipal::Rap(pin())
    );
    assert_eq!(
        principal_of(room_event(&loaded[6])),
        &ItemPrincipal::A2aPseudonym(pin())
    );
    for entry in &loaded[..5] {
        assert_eq!(
            principal_of(room_event(entry)),
            &ItemPrincipal::A2aBound(pin()),
            "a bound address keeps its pin through the production reader"
        );
    }
}

#[tokio::test]
async fn k09_rap_and_pseudonym_addresses_stay_byte_identical_to_the_pre_19_18d_form() {
    let rap = ItemAddress::from_rap_ingress(pin(), item("ri_a"));
    let pseudonym = ItemAddress::from_a2a_ingress(pin(), item("ri_b"));
    let rap_json = serde_json::to_string(&rap).expect("rap");
    let pseudonym_json = serde_json::to_string(&pseudonym).expect("pseudonym");
    assert_eq!(
        rap_json,
        format!(
            r#"{{"principal":{{"kind":"rap","peer":"{}"}},"item":"ri_a"}}"#,
            pin().as_str()
        )
    );
    assert_eq!(
        pseudonym_json,
        format!(
            r#"{{"principal":{{"kind":"a2a_pseudonym","peer":"{}"}},"item":"ri_b"}}"#,
            pin().as_str()
        )
    );
    for text in [&rap_json, &pseudonym_json] {
        assert!(!text.contains("boundPeer"), "{text}");
    }
    let turn = serde_json::to_string(&TurnAddress::answering(
        &pseudonym,
        ReplyTurnId::from_replay("rr_b"),
    ))
    .expect("turn");
    assert_eq!(
        turn,
        format!(
            r#"{{"principal":{{"kind":"a2a_pseudonym","peer":"{}"}},"turn":"rr_b"}}"#,
            pin().as_str()
        )
    );
    // Positive control: the bound form DOES carry the key.
    assert!(
        serde_json::to_string(&bound_item_address())
            .expect("bound")
            .contains("boundPeer")
    );

    // The pinned fixture reads unchanged and re-serializes to the same bytes
    // (line 8 is a pseudonym retract).
    let fixture_lines: Vec<&str> = FIXTURE.lines().collect();
    let loaded = load(FIXTURE.as_bytes()).await;
    assert_eq!(loaded.len(), fixture_lines.len(), "no fixture line dropped");
    for (entry, line) in loaded.iter().zip(&fixture_lines) {
        // The fixture's one forward-written event (an unmapped `event` tag)
        // reads as `Unrecognized` by design and has no byte-faithful echo.
        if matches!(room_event(entry), RoomEvent::Unrecognized) {
            continue;
        }
        assert_eq!(
            serde_json::to_string(entry).expect("entry"),
            *line,
            "fixture line re-serializes byte-identically"
        );
    }
    assert!(matches!(
        principal_of(room_event(&loaded[7])),
        ItemPrincipal::A2aPseudonym(_)
    ));

    // The row `test_story_19_16g_log_reminder.py::_retract` appends.
    let tui_payload = format!(
        r#"{{"event":"recipient_item_retracted","address":{{"principal":{{"kind":"a2a_pseudonym","peer":"{TUI_PEER}"}},"item":"ri_tui"}},"retracted_at_ms":1700000000000,"principal_collapsed":false}}"#
    );
    let tui_line = raw_line(1, &tui_payload);
    let loaded = load(&bytes_of(std::slice::from_ref(&tui_line))).await;
    assert_eq!(serde_json::to_string(&loaded[0]).expect("entry"), tui_line);
    assert_eq!(
        principal_of(room_event(&loaded[0])),
        &ItemPrincipal::A2aPseudonym(PeerId::parse(TUI_PEER).expect("peer"))
    );
}

/// `P14` — the exact serde topology of the pre-19.18d types
/// (`git show 7f631a9:src/domain/models/recipient_item.rs`: `ItemPrincipal`
/// :34-43, `ItemAddress` :53, `TurnAddress` :333) in the same
/// `JournalEntry → JournalRecord → RoomEvent` shape production parses, fed raw
/// bytes directly (no permissive buffering wrapper). Shape of the proof copied
/// from `conformance_19_18d0_a2a_typed_policy_principal.rs` `k05_durable::old_reader`.
mod old_reader {
    use rustain::domain::models::{ItemId, PeerId, ReplyTurnId};
    use serde::{Deserialize, Serialize};

    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case", tag = "kind", content = "peer")]
    pub enum ItemPrincipal {
        Rap(PeerId),
        A2aPseudonym(PeerId),
        #[serde(other)]
        Unknown,
    }

    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct ItemAddress {
        pub principal: ItemPrincipal,
        pub item: ItemId,
    }

    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct TurnAddress {
        pub principal: ItemPrincipal,
        pub turn: ReplyTurnId,
    }

    /// Only the event shapes the proof reads; the pre-19.18d `RoomEvent` is
    /// `#[serde(rename_all = "snake_case", tag = "event")]` (`orchestration_room.rs`).
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case", tag = "event")]
    pub enum RoomEvent {
        RecipientItemReceived {
            address: ItemAddress,
            task: String,
            #[serde(default)]
            alias: Option<String>,
            content: String,
        },
        RecipientItemReplied {
            address: TurnAddress,
            answers: ItemId,
            content: String,
        },
        ConsentRevoked {
            #[serde(default)]
            sender: Option<PeerId>,
            #[serde(default)]
            revoked_at: i64,
        },
        #[serde(other)]
        Unrecognized,
    }

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct JournalEntry {
        pub schema_version: u32,
        pub seq: u64,
        #[serde(default)]
        pub recorded_at_ms: i64,
        pub record: JournalRecord,
    }

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case", tag = "kind", content = "payload")]
    pub enum JournalRecord {
        Room(RoomEvent),
    }

    #[derive(Debug)]
    pub enum OldJournalError {
        CorruptRecord(usize),
        InvalidSequence,
    }

    /// `node_journal.rs` `parse_entries` (infra) at `7f631a9`, error type reduced.
    pub fn parse_entries(bytes: &[u8]) -> Result<Vec<JournalEntry>, OldJournalError> {
        let mut entries = Vec::new();
        let mut expected_seq = 1u64;
        let mut lines = bytes.split_inclusive(|byte| *byte == b'\n').peekable();
        while let Some(raw_line) = lines.next() {
            let is_last = lines.peek().is_none();
            if is_last && raw_line.last() != Some(&b'\n') {
                break;
            }
            let line = raw_line.strip_suffix(b"\n").unwrap_or(raw_line);
            let entry = match serde_json::from_slice::<JournalEntry>(line) {
                Ok(entry) => entry,
                Err(_) if is_last => break,
                Err(_) => return Err(OldJournalError::CorruptRecord(entries.len() + 1)),
            };
            if entry.seq != expected_seq {
                return Err(OldJournalError::InvalidSequence);
            }
            expected_seq += 1;
            entries.push(entry);
        }
        Ok(entries)
    }
}

fn old_known_good() -> old_reader::JournalRecord {
    old_reader::JournalRecord::Room(old_reader::RoomEvent::ConsentRevoked {
        sender: Some(other_peer()),
        revoked_at: 7,
    })
}

/// `[lead-in good, target, known-good follower]` as production-written bytes.
fn bytes_around(target: &RoomEvent) -> Vec<u8> {
    let target_line = serde_json::to_string(&JournalEntry::new(
        2,
        JournalRecord::Room(target.clone()),
        1_700_000_000_002,
    ))
    .expect("target");
    bytes_of(&[known_good(1), target_line, known_good(3)])
}

#[test]
fn k09_pre_19_18d_reader_reads_a_bound_line_as_unknown_and_keeps_the_journal_intact() {
    let item_target = received(bound_item_address());
    let turn_target = replied(bound_turn_address());
    for (label, target) in [("item", item_target), ("turn", turn_target)] {
        let bytes = bytes_around(&target);
        let entries = old_reader::parse_entries(&bytes)
            .unwrap_or_else(|error| panic!("{label}: old reader rejected the journal: {error:?}"));
        assert_eq!(entries.len(), 3, "{label}: old reader dropped a row");
        assert_eq!(entries[0].record, old_known_good());
        assert_eq!(entries[2].record, old_known_good(), "{label}: follower");
        let unknown = match (&entries[1].record, label) {
            (
                old_reader::JournalRecord::Room(old_reader::RoomEvent::RecipientItemReceived {
                    address,
                    content,
                    ..
                }),
                "item",
            ) => {
                assert_eq!(address.item, item("ri_bound"));
                assert_eq!(content, "review the patch");
                &address.principal
            }
            (
                old_reader::JournalRecord::Room(old_reader::RoomEvent::RecipientItemReplied {
                    address,
                    answers,
                    ..
                }),
                "turn",
            ) => {
                assert_eq!(address.turn, ReplyTurnId::from_replay("rr_bound"));
                assert_eq!(answers, &item("ri_bound"));
                &address.principal
            }
            other => panic!("{label}: old reader decoded the line as {other:?}"),
        };
        assert_eq!(unknown, &old_reader::ItemPrincipal::Unknown, "{label}");
    }

    // Positive control: a Rap line of the same shape decodes to its real
    // principal, so `Unknown` above is the bound arm's doing, not the copy's.
    let rap = bytes_around(&received(ItemAddress::from_rap_ingress(
        pin(),
        item("ri_r"),
    )));
    let entries = old_reader::parse_entries(&rap).expect("rap journal");
    assert!(matches!(
        &entries[1].record,
        old_reader::JournalRecord::Room(old_reader::RoomEvent::RecipientItemReceived {
            address: old_reader::ItemAddress {
                principal: old_reader::ItemPrincipal::Rap(_),
                ..
            },
            ..
        })
    ));

    // Positive control (what `M21` would write): the natural content-bearing
    // form `{"kind":"a2a_bound","peer":"…"}` makes the old reader ERROR, for
    // both address types, ahead of a known-good line.
    for (label, key, id) in [("item", "item", "ri_bound"), ("turn", "turn", "rr_bound")] {
        let address = json!({
            "principal": {"kind": "a2a_bound", "peer": pin().as_str()},
            key: id,
        });
        let bytes = bytes_of(&[
            known_good(1),
            raw_line(2, &payload_for(label, address)),
            known_good(3),
        ]);
        assert!(
            matches!(
                old_reader::parse_entries(&bytes),
                Err(old_reader::OldJournalError::CorruptRecord(2))
            ),
            "{label}: the content-bearing form must break the old reader"
        );
    }
}

fn address_json(key: &str, id: &str, principal: Value, bound_peer: Option<&str>) -> Value {
    let mut address = json!({"principal": principal, key: id});
    if let Some(bound_peer) = bound_peer {
        address["boundPeer"] = json!(bound_peer);
    }
    address
}

fn payload_for(label: &str, address: Value) -> Value {
    match label {
        "item" => json!({
            "event": "recipient_item_received", "address": address,
            "task": "t", "alias": null, "content": "c",
        }),
        _ => json!({
            "event": "recipient_item_replied", "address": address,
            "answers": "ri_bound", "content": "c",
        }),
    }
}

/// Load `[target, known-good]` through the production reader and return the
/// target's principal; the journal must be intact (both rows, follower last).
async fn principal_of_target(label: &str, address: Value) -> ItemPrincipal {
    let bytes = bytes_of(&[raw_line(1, &payload_for(label, address)), known_good(2)]);
    let loaded = load(&bytes).await;
    assert_eq!(loaded.len(), 2, "{label}: the journal stays intact");
    assert!(
        matches!(room_event(&loaded[1]), RoomEvent::ConsentRevoked { .. }),
        "{label}: the known-good follower is read"
    );
    principal_of(room_event(&loaded[0])).clone()
}

#[tokio::test]
async fn k09_malformed_or_missing_bound_peer_reads_unknown_never_a_corrupt_record() {
    let bound = || json!({"kind": "a2a_bound"});
    for (label, key, id) in [("item", "item", "ri_bound"), ("turn", "turn", "rr_bound")] {
        // Positive control: a well-formed pin reads `A2aBound`.
        assert_eq!(
            principal_of_target(label, address_json(key, id, bound(), Some(pin().as_str()))).await,
            ItemPrincipal::A2aBound(pin()),
            "{label}: control"
        );
        // F10: malformed pin.
        assert_eq!(
            principal_of_target(label, address_json(key, id, bound(), Some("zz"))).await,
            ItemPrincipal::Unknown,
            "{label}: malformed boundPeer"
        );
        // Non-text pins (numeric, null, object) fold to `Unknown` too (A4).
        for bad in [json!(7), json!(null), json!({})] {
            assert_eq!(
                principal_of_target(
                    label,
                    json!({"principal": bound(), key: id, "boundPeer": bad})
                )
                .await,
                ItemPrincipal::Unknown,
                "{label}: non-text boundPeer {bad}"
            );
        }
        // Missing pin.
        assert_eq!(
            principal_of_target(label, address_json(key, id, bound(), None)).await,
            ItemPrincipal::Unknown,
            "{label}: missing boundPeer"
        );
        // A stray boundPeer on a pseudonym address is ignored.
        assert_eq!(
            principal_of_target(
                label,
                address_json(
                    key,
                    id,
                    json!({"kind": "a2a_pseudonym", "peer": other_peer().as_str()}),
                    Some(pin().as_str()),
                ),
            )
            .await,
            ItemPrincipal::A2aPseudonym(other_peer()),
            "{label}: stray boundPeer ignored"
        );
    }
}

#[test]
fn frozen_reader_ignores_additive_defer_reply_kind() {
    let target = replied_with_kind(bound_turn_address(), ReplyKind::Defer { minutes: 30 });
    let entries = old_reader::parse_entries(&bytes_around(&target))
        .expect("frozen reader keeps a new defer row");
    assert_eq!(entries.len(), 3);
    assert!(matches!(
        &entries[1].record,
        old_reader::JournalRecord::Room(old_reader::RoomEvent::RecipientItemReplied {
            address,
            answers,
            ..
        }) if address.turn == ReplyTurnId::from_replay("rr_bound")
            && answers == &item("ri_bound")
    ));
}

#[tokio::test]
async fn malformed_reply_kind_is_field_local_in_the_production_journal_reader() {
    let root = serde_json::to_value(received(bound_item_address())).unwrap();
    let reply = serde_json::to_value(replied_with_kind(
        bound_turn_address(),
        ReplyKind::Defer { minutes: 30 },
    ))
    .unwrap();
    let mut cases = vec![
        (
            Some(json!({"type":"defer","minutes":1})),
            ReplyKind::Defer { minutes: 1 },
        ),
        (
            Some(json!({"type":"defer","minutes":65535})),
            ReplyKind::Defer { minutes: u16::MAX },
        ),
        (None, ReplyKind::Answer),
    ];
    for malformed in [
        json!(null),
        json!(true),
        json!([]),
        json!("defer"),
        json!({"type":"future","minutes":30}),
        json!({"type":17,"minutes":30}),
        json!({"type":"defer"}),
        json!({"type":"defer","minutes":null}),
        json!({"type":"defer","minutes":"30"}),
        json!({"type":"defer","minutes":0}),
        json!({"type":"defer","minutes":-1}),
        json!({"type":"defer","minutes":65536}),
    ] {
        cases.push((Some(malformed), ReplyKind::Unknown));
    }
    for (kind, expected) in cases {
        let mut payload = reply.clone();
        if let Some(kind) = kind {
            payload["kind"] = kind;
        } else {
            payload.as_object_mut().unwrap().remove("kind");
        }
        let loaded = load(&bytes_of(&[
            raw_line(1, &root),
            raw_line(2, &payload),
            known_good(3),
        ]))
        .await;
        assert_eq!(
            loaded.iter().map(|entry| entry.seq).collect::<Vec<_>>(),
            [1, 2, 3],
            "a bad additive field cannot discard its row or following durable history"
        );
        let projection =
            rustain::adapters::policy::JournalRecipientItemProjection::from_entries(&loaded);
        let (_, turns) = projection
            .thread(
                &rustain::domain::models::ItemScope::exact(ItemPrincipal::A2aBound(pin())),
                "ri_bound",
            )
            .unwrap();
        assert!(
            matches!(&turns[1],
            rustain::adapters::policy::recipient_item::ThreadTurn::Reply(turn)
                if turn.kind == expected && turn.content == "done"
                    && turn.answers == item("ri_bound")),
            "kind fallback must retain the real reply body and answered identity"
        );
    }
    // JSON escapes change neither field identity nor the valid interval.
    let escaped = reply
        .to_string()
        .replace("\"type\":\"defer\"", "\"ty\\u0070e\":\"defer\"");
    let loaded = load(&bytes_of(&[
        raw_line(1, &root),
        raw_line(2, &escaped),
        known_good(3),
    ]))
    .await;
    assert!(matches!(
        room_event(&loaded[1]),
        RoomEvent::RecipientItemReplied {
            kind: ReplyKind::Defer { minutes: 30 },
            ..
        }
    ));
}
