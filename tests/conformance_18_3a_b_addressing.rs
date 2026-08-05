//! Story 18.3a-b conformance — operator address & ticket addressing.
//!
//! Wired into CI's Check job (`.github/workflows/ci.yml`); a conformance file
//! that is green locally and unenforced is the 18.3d shipped defect.
//!
//! Deliberately **not** named `*a2a*`: `every_a2a_integration_test_is_wired_into
//! _the_ci_a2a_lane` (`src/domain/ports/capability_provider.rs`) fails the
//! default lane for an unlisted `tests/*a2a*.rs`.

use std::collections::BTreeMap;
use std::sync::Arc;

use rustain::domain::models::{
    AgentId, ArtifactId, CapabilityToken, ContentHash, HostBinding, JournalEntry, JournalRecord,
    NODE_JOURNAL_SCHEMA_VERSION, NodeOrigin, NodeState, NodeView, OrchestrationRoom,
    OrchestrationRoomId, RoomEvent, TicketAddressee,
};
use rustain::domain::ports::{ArtifactSink, ArtifactStore};
use rustain::infrastructure::subagent::{JournalArtifactSink, NodeJournal, NodeRoomJournal};

/// The task node every ticket below is filed from.
fn task_node() -> AgentId {
    AgentId::from_peer_path("mcp/s-srv").expect("valid peer path")
}

fn sample_artifact() -> ArtifactId {
    ArtifactId::from(ContentHash::from_bytes([0x11; 32]))
}

/// A `TicketAssigned` line exactly as 17.5b wrote it: before Story 18.2 added
/// `recorded_at_ms`, and before this story added `to`. This byte literal tests
/// read/recovery compatibility; the append-only journal never rewrites it.
const TICKET_LINE_17_5B: &str = r#"{"schema_version":1,"seq":7,"record":{"kind":"room","payload":{"event":"ticket_assigned","node":"mcp/s-srv","artifact":"1111111111111111111111111111111111111111111111111111111111111111"}}}"#;

/// The room a 17.5b ticket folds into, spelled out field by field. This is the
/// **pre-change fold output**: `open_tickets` is still `Vec<ArtifactId>` and
/// no addressee reaches the read model.
fn expected_view_after_one_ticket() -> NodeView {
    NodeView {
        id: task_node(),
        origin: NodeOrigin::Remote,
        state: NodeState::Created,
        host: HostBinding::new("local", "h"),
        host_bound_unavailable: false,
        last_remote_content: None,
        mcp_task: None,
        open_tickets: vec![sample_artifact()],
        resolved_tickets: BTreeMap::new(),
    }
}

fn registered() -> RoomEvent {
    RoomEvent::NodeRegistered {
        node: task_node(),
        origin: NodeOrigin::Remote,
        host: HostBinding::new("local", "h"),
    }
}

fn project(events: Vec<RoomEvent>) -> OrchestrationRoom {
    OrchestrationRoom::project(OrchestrationRoomId::new(), events)
}

/// **AC1 keystone.** A genuine `TicketAssigned` byte literal written by 17.5b
/// replays through the current build: its absent timestamp becomes the
/// explicit unknown sentinel, `to` becomes `None`, and it folds identically to
/// the pre-change room state.
///
/// Mutants this must turn RED (all observed 2026-08-04):
///   1. Drop `skip_serializing_if` from `to` → the event-shape assertion below
///      fires on `"to":null`, as does the pre-armed 17.5b gate
///      `ticket_assigned_serializes_without_a_to_field`.
///   3. Bump `NODE_JOURNAL_SCHEMA_VERSION` to 2 → every existing journal is
///      rejected.
///   4. Make the fold depend on the addressee (the read-model widening this
///      cut forbids) → the field-by-field fold assertion fires. This is
///      preflight ruling P1b made executable.
///
/// ⚠ **AC1's second mutant does not exist, and that is a finding, not a gap.**
/// The story predicted *"drop `#[serde(default)]` from `to` → the 17.5b byte
/// literal fails to deserialize."* It does not: serde resolves a missing
/// `Option<T>` field to `None` through its `missing_field` helper, with or
/// without the attribute. The attribute is kept as explicit intent and becomes
/// load-bearing if `to` ever stops being an `Option`; replay is pinned here by
/// parsing and folding the real legacy envelope, not by reserializing it.
#[test]
fn a_17_5b_ticket_line_replays_and_folds_identically() {
    assert_eq!(
        NODE_JOURNAL_SCHEMA_VERSION, 1,
        "a schema bump makes every existing room file unreadable: parse_entries rejects \
         mismatches, and this story is additive"
    );

    let entry: JournalEntry =
        serde_json::from_str(TICKET_LINE_17_5B).expect("a 17.5b ticket line must still parse");
    assert_eq!(entry.schema_version, 1);
    assert_eq!(entry.seq, 7);
    assert_eq!(entry.recorded_at_ms, 0);
    assert!(
        !entry.has_timestamp(),
        "a pre-18.2 line must retain an explicit unknown timestamp"
    );

    let JournalRecord::Room(event) = entry.record.clone() else {
        panic!("expected a room record, got {:?}", entry.record);
    };
    let RoomEvent::TicketAssigned { node, artifact, to } = event.clone() else {
        panic!("expected TicketAssigned, got {event:?}");
    };
    assert_eq!(node, task_node());
    assert_eq!(artifact, sample_artifact());
    assert_eq!(
        to, None,
        "a 17.5b ticket carries no addressee and must never gain a fabricated one"
    );

    let unaddressed_json = serde_json::to_string(&event).expect("serialize event");
    assert!(
        !unaddressed_json.contains("\"to\""),
        "the legacy event shape must not gain a `to:null` field"
    );

    // The fold is unchanged: same node view, field for field.
    let room = project(vec![registered(), event.clone()]);
    assert_eq!(
        room.nodes().get(&task_node()),
        Some(&expected_view_after_one_ticket()),
        "the addressee is durable-only in this cut; the read model must be identical to the \
         pre-change fold"
    );
}

/// **AC1 positive control.** A ticket written by *this* build carries its
/// addressee through serde intact, folds into `open_tickets` exactly once, and
/// a duplicate replay is a no-op (17.5b shipped the opposite defect).
#[test]
fn an_addressed_ticket_round_trips_and_replays_idempotently() {
    let addressed = RoomEvent::TicketAssigned {
        node: task_node(),
        artifact: sample_artifact(),
        to: Some(TicketAddressee::Operator {
            id: AgentId::local_operator(),
        }),
    };

    let json = serde_json::to_string(&addressed).expect("serialize");
    assert!(
        json.contains(r#""to":{"kind":"operator","id":"operator"}"#),
        "the addressee must reach the wire as a tagged variant: {json}"
    );
    let back: RoomEvent = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, addressed, "the addressee must survive a round-trip");

    // Folded twice — the artifact appears once, and the read model is the same
    // one an unaddressed ticket produces.
    let room = project(vec![registered(), addressed.clone(), addressed]);
    assert_eq!(
        room.nodes().get(&task_node()),
        Some(&expected_view_after_one_ticket()),
        "duplicate replay must not duplicate the ticket, and `to` must not reach the view"
    );
}

/// OPEN-DR-3's standing obligation: do not add a variant without exercising the
/// `Unrecognized` fallback.
///
/// ⚠ **Note the asymmetry, deliberately pinned here.** An unknown *event tag*
/// degrades to [`RoomEvent::Unrecognized`] and folds as a no-op, so one line
/// from a newer build cannot fail the whole load. An unknown *record kind* is
/// fatal — `JournalRecord` has no `#[serde(other)]`.
#[test]
fn an_unknown_event_tag_folds_as_a_no_op_while_an_unknown_record_kind_is_fatal() {
    let unknown_event = r#"{"schema_version":1,"seq":8,"recorded_at_ms":1,"record":{"kind":"room","payload":{"event":"ticket_forwarded","node":"mcp/s-srv","to":{"kind":"operator","id":"operator"}}}}"#;
    let entry: JournalEntry =
        serde_json::from_str(unknown_event).expect("an unknown event tag must not fail the load");
    let JournalRecord::Room(event) = entry.record else {
        panic!("expected a room record");
    };
    assert_eq!(event, RoomEvent::Unrecognized);

    // Folding it changes nothing.
    let room = project(vec![registered(), event]);
    let mut untouched = expected_view_after_one_ticket();
    untouched.open_tickets.clear();
    assert_eq!(room.nodes().get(&task_node()), Some(&untouched));

    let unknown_kind = r#"{"schema_version":1,"seq":9,"recorded_at_ms":1,"record":{"kind":"telemetry","payload":{}}}"#;
    assert!(
        serde_json::from_str::<JournalEntry>(unknown_kind).is_err(),
        "JournalRecord has no #[serde(other)]: an unknown record kind is fatal, and that \
         asymmetry is load-bearing"
    );
}

/// **AC3 keystone.** Table-driven over `{addressed-to-operator, unaddressed,
/// unknown}`, driven through the real types rather than re-deriving the
/// predicate in the test body: the serde shape holds and the fold is
/// indifferent to all three.
#[test]
fn every_addressee_shape_serializes_and_leaves_the_fold_indifferent() {
    let cases: [(Option<TicketAddressee>, Option<&str>); 3] = [
        (
            Some(TicketAddressee::Operator {
                id: AgentId::local_operator(),
            }),
            Some(r#""to":{"kind":"operator","id":"operator"}"#),
        ),
        (None, None),
        (
            Some(TicketAddressee::Unknown),
            Some(r#""to":{"kind":"unknown"}"#),
        ),
    ];

    for (to, expected_fragment) in cases {
        let event = RoomEvent::TicketAssigned {
            node: task_node(),
            artifact: sample_artifact(),
            to: to.clone(),
        };
        let json = serde_json::to_string(&event).expect("serialize");
        match expected_fragment {
            Some(fragment) => assert!(json.contains(fragment), "{to:?} → {json}"),
            None => assert!(
                !json.contains(r#""to""#),
                "an unaddressed ticket must emit no `to` key at all: {json}"
            ),
        }

        let room = project(vec![registered(), event]);
        assert_eq!(
            room.nodes().get(&task_node()),
            Some(&expected_view_after_one_ticket()),
            "the fold must be indifferent to the addressee ({to:?})"
        );
    }
}

/// **AC1 end-to-end keystone.** A ticket written by *this* build, through the
/// production front door, lands on disk with its addressee.
///
/// **Front door:** `ArtifactSink::write_input_request` — the trait method
/// (`domain/ports/artifact_sink.rs`), driven as `&dyn ArtifactSink` against the
/// real `FileSystemArtifactStore` and a real on-disk `NodeJournal`. The
/// production impl behind it is `JournalArtifactSink`, constructed exactly as
/// the four composition roots construct it, and its production trigger is the
/// MCP task driver's `Waiting` transition (asserted end-to-end in
/// `tests/integration_mcp_tasks.rs`).
///
/// **Forbidden bypass (a listed mutant):** constructing
/// `RoomEvent::TicketAssigned` in the test and calling `RoomJournal::record_event`
/// directly. That proves the enum compiles, not that the producer populates the
/// addressee — the whole capability this AC ships.
///
/// The assertion reads the **persisted journal line back off disk**, not the
/// value that was passed in.
#[tokio::test]
async fn the_production_producer_journals_the_operator_as_the_addressee() {
    let dir = tempfile::tempdir().expect("tempdir");
    let journal = Arc::new(
        NodeJournal::open_workspace(dir.path())
            .await
            .expect("journal opens"),
    );
    let room = Arc::new(NodeRoomJournal::new(journal.clone(), None));
    let store: Arc<dyn ArtifactStore> = Arc::new(
        rustain::adapters::artifact::FileSystemArtifactStore::new(dir.path()),
    );
    let authority = CapabilityToken::r1_root(AgentId::root());
    let sink: Arc<dyn ArtifactSink> = Arc::new(JournalArtifactSink::new(
        store,
        room,
        authority.id,
        HostBinding::new("local", "h"),
        AgentId::local_operator(),
    ));

    let node = task_node();
    let artifact = sink
        .write_input_request(
            &node,
            &node,
            serde_json::json!({"method": "elicitation/create", "params": {"key": "confirm"}}),
        )
        .await
        .expect("the elicitation files a durable ticket");

    // Re-open the journal and read the line that was actually written.
    let persisted = NodeJournal::open_workspace(dir.path())
        .await
        .expect("reopen")
        .load()
        .await
        .expect("read the durable journal");
    let ticket = persisted
        .iter()
        .find_map(|entry| match &entry.record {
            JournalRecord::Room(event @ RoomEvent::TicketAssigned { .. }) => Some(event.clone()),
            _ => None,
        })
        .expect("write_input_request must journal a TicketAssigned");

    let RoomEvent::TicketAssigned {
        node: journaled_node,
        artifact: journaled_artifact,
        to,
    } = ticket
    else {
        unreachable!("filtered above")
    };
    assert_eq!(journaled_node, node);
    assert_eq!(journaled_artifact, artifact);
    assert_eq!(
        to,
        Some(TicketAddressee::Operator {
            id: AgentId::local_operator()
        }),
        "the sole production producer must record who must act, sourced from the composition \
         root — not left `None` and not fabricated from a parse"
    );

    // Durable-first ordering is unchanged: the artifact exists before the
    // ticket that points at it.
    let artifact_pos = persisted
        .iter()
        .position(|e| {
            matches!(
                &e.record,
                JournalRecord::Room(RoomEvent::ArtifactCreated { .. })
            )
        })
        .expect("ArtifactCreated journaled");
    let ticket_pos = persisted
        .iter()
        .position(|e| {
            matches!(
                &e.record,
                JournalRecord::Room(RoomEvent::TicketAssigned { .. })
            )
        })
        .expect("TicketAssigned journaled");
    assert!(
        artifact_pos < ticket_pos,
        "durable-first: the artifact must be journaled before the ticket that addresses it"
    );
}

/// AC2 third mutant, structural (Rule 4) — **the placeholder is gone.**
///
/// 18.3a shipped `const LOCAL_OPERATOR_ROLE: RoomRole = RoomRole::Owner` with
/// this story named in its doc comment as the trigger. Leaving it in place
/// beside the new named principal is an absence no behavioural test proves —
/// both would answer `Owner`, and every existing test would stay green while
/// the placeholder quietly re-accumulated callers.
///
/// Mutant this must turn RED: reinstate the constant.
#[test]
fn the_local_operator_role_placeholder_is_retired() {
    let path = format!(
        "{}/src/infrastructure/runtime/room_bridge.rs",
        env!("CARGO_MANIFEST_DIR")
    );
    let source = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    assert!(
        !source.contains("LOCAL_OPERATOR_ROLE"),
        "18.3a's placeholder constant must be gone: the acting principal is now \
         `AgentId::local_operator()` and its role is derived by `local_room_role`"
    );
    assert!(
        source.contains("local_room_role"),
        "positive control: the replacement derivation must actually be wired into the \
         `/room role` gate, or this ratchet would pass on a deletion"
    );
}

/// AC3 ratchet #1 (Rule 4) — **no bare addressee slot.**
///
/// FR152 forbids *"a bare `assignee` field (which would launder execution
/// authority)"*. The source scan holds that named absence across `src/`.
/// The exact serialized `TicketAssigned` field set separately catches a bare
/// convenience field with a different name.
///
/// ⚠ **The source scan is scoped to `src/` on purpose.**
/// `tests/fixtures/skill_eval_corpus/` carries 13 hits in a vendored
/// third-party corpus that this project does not own and must not rewrite. A
/// repo-wide scan would be RED on day one.
///
/// Mutant this must turn RED: add a `to_name: String` convenience field beside
/// `to` on `RoomEvent::TicketAssigned` and update the compile sites. The wire
/// field-count assertion still rejects the laundering shape.
#[test]
fn no_bare_addressee_slot_exists_in_source_or_wire_shape() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders: Vec<String> = Vec::new();
    for path in rs_files(&root) {
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        if source.contains("assignee") {
            offenders.push(path.display().to_string());
        }
    }
    assert!(
        offenders.is_empty(),
        "FR152: an addressee must be a `TicketAddressee` variant carrying its authority \
         consequence, never a bare `assignee` name. Offending files: {offenders:?}"
    );

    let event = RoomEvent::TicketAssigned {
        node: task_node(),
        artifact: sample_artifact(),
        to: Some(TicketAddressee::Operator {
            id: AgentId::local_operator(),
        }),
    };
    let value = serde_json::to_value(event).expect("serialize TicketAssigned");
    let object = value.as_object().expect("internally tagged event object");
    for key in ["event", "node", "artifact", "to"] {
        assert!(
            object.contains_key(key),
            "positive control: TicketAssigned must retain `{key}`"
        );
    }
    assert_eq!(
        object.len(),
        4,
        "TicketAssigned must have exactly event/node/artifact/to; a bare convenience field \
         launders the addressee around TicketAddressee: {object:?}"
    );
}

/// AC2 keystone — the operator has exactly one durable, reserved, unforgeable
/// address, and it survives the journal it is written to.
///
/// Mutants this must turn RED:
///   1. Omit the exact-path rejection → `AgentId::parse("operator")` succeeds
///      and any agent can forge the operator's address.
///   2. Omit the `Deserialize` sentinel special-case → the operator's own
///      address fails to deserialize from a journal it just wrote.
///
/// ⛔ Identity is **equality**, never a parse: `AgentId` segment 0 is a route
/// discriminator (`ADR-18-3b-01` D1), so a `segments().next() == "operator"`
/// check would be right for one of three production path shapes and silently
/// wrong for two. This test asserts equality and inspects no segment.
#[test]
fn the_local_operator_address_is_reserved_and_round_trips() {
    let operator = AgentId::local_operator();

    // Reserved: no other constructor produces the exact sentinel.
    assert!(
        AgentId::parse("operator").is_err(),
        "the exact sentinel must be unforgeable by the public fallible constructor"
    );

    // Round-trips through serde as the bare sentinel string.
    let json = serde_json::to_string(&operator).expect("serialize");
    assert_eq!(json, r#""operator""#);
    let back: AgentId = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, operator);
    assert_eq!(
        serde_json::from_str::<AgentId>(r#""operator""#).expect("sentinel parses"),
        AgentId::local_operator(),
        "the sentinel special-case must yield the same value the constructor does"
    );

    // Positive controls: the reservation is exact. Previously-valid nested
    // paths remain constructable and replayable.
    assert!(AgentId::parse("operators").is_ok());
    let nested = AgentId::from_peer_path("peer/operator").expect("legacy nested path");
    let nested_json = serde_json::to_string(&nested).expect("serialize nested path");
    let nested_back: AgentId = serde_json::from_str(&nested_json).expect("deserialize nested path");
    assert_eq!(nested_back, nested);
    assert!(AgentId::new().is_local());
}

fn rs_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(&current)
            .unwrap_or_else(|e| panic!("read dir {}: {e}", current.display()))
        {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }
    assert!(
        !out.is_empty(),
        "no .rs files found under {}",
        dir.display()
    );
    out
}
