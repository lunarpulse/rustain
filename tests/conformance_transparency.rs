//! Story 18.2 — the transparency projection, the `transparency.jsonl` export,
//! and forward-compatible replay of pre-18.2 room journals.
//!
//! Feature-independent by design: the fold lives in `domain/` and the export
//! shell in `infrastructure/`, so these keystones run in the **default** lane
//! where an `a2a`-gated file would not. (`tests/a2a_*.rs` files must be named
//! in the CI a2a lane — see `every_a2a_integration_test_is_wired_into_the_ci_a2a_lane`.)
//!
//! The fixtures under `tests/fixtures/transparency/` are hash-pinned. The
//! historical fixture proves pre-18.2 fields still replay as explicit unknowns;
//! the separate current-shape fixture is P-7's populated export evidence for
//! persisted inbound and outbound directions.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustain::domain::models::node_journal::{JournalEntry, JournalRecord};
use rustain::domain::models::{Direction, RoomEvent};
use rustain::domain::ports::{RoomJournalError, RoomJournalReader};
use rustain::domain::services::transparency::{
    MAX_SUMMARY_BYTES, STRUCTURAL_REPLAY_CLAIM, StructuralDivergence, TransparencyFilter,
    TransparencyKind, fold_transparency, render_export, sanitize_disclosable, transparency_row,
    validate_replay_structure,
};
use rustain::infrastructure::transparency::TransparencyService;
use sha2::{Digest, Sha256};

const FIXTURE: &str = "FIXTURE_room_journal_pre_18_2.jsonl";
const CURRENT_FIXTURE: &str = "FIXTURE_room_journal_current_18_2.jsonl";

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/transparency")
}

/// Load the vendored journal, hash-checked against the manifest.
fn fixture_bytes(fixture: &str) -> Vec<u8> {
    let bytes = std::fs::read(fixture_dir().join(fixture)).expect("read pinned journal fixture");
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture_dir().join("manifest.json")).unwrap())
            .expect("valid fixture manifest");
    assert_eq!(
        hex::encode(Sha256::digest(&bytes)),
        manifest[fixture]["sha256"]
            .as_str()
            .expect("manifest entry"),
        "fixture {fixture} hash drifted"
    );
    bytes
}

/// Parse a pinned fixture the way the product does: one `JournalEntry` per
/// line.
fn parse_fixture_entries(fixture: &str) -> Vec<JournalEntry> {
    let bytes = fixture_bytes(fixture);
    String::from_utf8(bytes)
        .expect("fixture is utf-8")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str::<JournalEntry>(line)
                .unwrap_or_else(|error| panic!("fixture line must still parse: {error}\n{line}"))
        })
        .collect()
}

fn fixture_entries() -> Vec<JournalEntry> {
    parse_fixture_entries(FIXTURE)
}

fn current_fixture_entries() -> Vec<JournalEntry> {
    parse_fixture_entries(CURRENT_FIXTURE)
}

struct FixtureReader(Vec<JournalEntry>);

#[async_trait::async_trait]
impl RoomJournalReader for FixtureReader {
    async fn load_entries(&self) -> Result<Vec<JournalEntry>, RoomJournalError> {
        Ok(self.0.clone())
    }
}

// ── AC2 — additive, replay-safe enrichment ──────────────────────────────────

#[test]
fn a_pre_18_2_journal_still_parses_and_renders_missing_fields_as_explicit_unknowns() {
    let entries = fixture_entries();
    assert!(
        entries.len() >= 5,
        "the fixture must be populated — an empty journal makes every assertion below vacuous"
    );

    for entry in &entries {
        assert_eq!(
            entry.schema_version, 1,
            "NODE_JOURNAL_SCHEMA_VERSION must not have been bumped: a bump makes every \
             existing room file unreadable"
        );
        assert!(
            !entry.has_timestamp(),
            "this fixture predates `recorded_at_ms`; a stamped line means the fixture drifted"
        );
    }

    let rows = fold_transparency(&entries);
    assert!(!rows.is_empty(), "a pre-18.2 journal still projects rows");
    for row in &rows {
        assert_eq!(
            row.recorded_at_ms, None,
            "a missing timestamp is an explicit unknown, never epoch zero"
        );
        assert_eq!(row.timestamp_label(), "—");
    }

    // The two `RemoteEnvelope*` rows are the ones whose direction was a
    // *persisted* field, absent here. They must read `unknown` — never a
    // direction reconstructed from the node-id prefix, which is exactly the
    // derivation that is impossible for a rejection (it carries no node).
    let envelope_rows: Vec<_> = rows
        .iter()
        .filter(|row| {
            matches!(
                row.kind,
                TransparencyKind::Accepted | TransparencyKind::Rejected
            )
        })
        .collect();
    assert_eq!(envelope_rows.len(), 4, "two accepts and two rejections");
    for row in envelope_rows {
        assert_eq!(
            row.direction,
            Direction::Unknown,
            "a missing direction is `unknown`, never a fabricated inbound/outbound: {row:?}"
        );
    }

    // Host-local admission records are inbound by construction — the gate only
    // exists because a peer asked. That is derivation from the record's own
    // meaning, not from a field that was never written.
    for row in rows.iter().filter(|row| {
        matches!(
            row.kind,
            TransparencyKind::AwaitingApproval | TransparencyKind::StatusQueried
        )
    }) {
        assert_eq!(row.direction, Direction::Inbound);
    }

    // Story 18.3 additivity: the current-shape fixture remains schema-v1 and
    // folds beside the historical one; adding `PeerDisclosure` must not turn a
    // fixture replay into a migration requirement.
    let current_entries = current_fixture_entries();
    assert!(
        !current_entries.is_empty(),
        "the current-shape fixture must stay populated"
    );
    assert!(
        current_entries
            .iter()
            .all(|entry| entry.schema_version == 1),
        "NODE_JOURNAL_SCHEMA_VERSION must remain 1 for current records too"
    );
    assert!(
        validate_replay_structure(&current_entries).is_empty(),
        "the current-shape fixture must remain structurally replayable"
    );
    assert!(
        !fold_transparency(&current_entries).is_empty(),
        "the current-shape fixture must continue to fold"
    );
    assert!(
        entries
            .iter()
            .any(|entry| matches!(&entry.record, JournalRecord::Room(RoomEvent::Unrecognized))),
        "the historical fixture's unknown event tag must still land on Unrecognized"
    );

    // The new variant's fields are additive too. This is the smallest durable
    // shape a future writer may emit; removing the required numeric `default`
    // must make this replay fail rather than silently invent data.
    let peer_disclosure: JournalEntry = serde_json::from_value(serde_json::json!({
        "schema_version": 1,
        "seq": 99,
        "record": {
            "kind": "room",
            "payload": {
                "event": "peer_disclosure",
                "node": "a2a-in/p-peer/t-task"
            }
        }
    }))
    .expect("PeerDisclosure missing additive fields must replay");
    assert!(matches!(
        peer_disclosure.record,
        JournalRecord::Room(RoomEvent::PeerDisclosure {
            task: None,
            disclosed_bytes: 0,
            ..
        })
    ));
}

#[test]
fn a_pre_18_2_journal_never_renders_epoch_zero_as_a_real_time() {
    // The mutant this kills: replacing `Option<i64>` with a bare `i64` and
    // formatting `0`. That renders "1970-01-01" — a fabricated fact, which is
    // strictly worse than an honest dash in an audit log.
    let rows = fold_transparency(&fixture_entries());
    for row in &rows {
        assert!(
            !row.timestamp_label().contains("1970"),
            "epoch zero must never surface as a timestamp: {row:?}"
        );
    }
}

// ── AC5 / UX-DR-ROOM-01 — unknown variants render, never vanish ─────────────

#[test]
fn an_unrecognised_event_tag_renders_an_explicit_unknown_row() {
    let entries = fixture_entries();
    let unknown: Vec<_> = entries
        .iter()
        .filter(|entry| matches!(&entry.record, JournalRecord::Room(RoomEvent::Unrecognized)))
        .collect();
    assert_eq!(
        unknown.len(),
        1,
        "the fixture must carry exactly one deliberately-unmapped event tag, or this \
         assertion cannot fail"
    );

    let rows = fold_transparency(&entries);
    let unknown_rows: Vec<_> = rows
        .iter()
        .filter(|row| row.kind == TransparencyKind::Unknown)
        .collect();
    assert_eq!(
        unknown_rows.len(),
        1,
        "an unrecognised record renders as an unknown row — dropping it silently is the \
         failure UX-DR-ROOM-01 names: {rows:?}"
    );
    assert!(unknown_rows[0].summary.contains("unrecognised"));
}

// ── AC3 — the fold and the export ───────────────────────────────────────────

#[test]
fn the_legacy_fixture_projects_every_disclosable_variant_and_nothing_else() {
    let rows = fold_transparency(&fixture_entries());

    assert_eq!(
        rows.len(),
        7,
        "fixture shape drifted; rows = {:?}",
        rows.iter()
            .map(|row| (row.seq, row.kind))
            .collect::<Vec<_>>()
    );

    let kinds: Vec<_> = rows.iter().map(|row| row.kind).collect();
    for want in [
        TransparencyKind::Accepted,
        TransparencyKind::Rejected,
        TransparencyKind::AwaitingApproval,
        TransparencyKind::StatusQueried,
        TransparencyKind::Unknown,
    ] {
        assert!(kinds.contains(&want), "no {want:?} row in {kinds:?}");
    }

    // Every row carries at least peer and summary; a row that renders blank is
    // a row that discloses nothing.
    for row in &rows {
        assert!(!row.peer.is_empty(), "empty peer on {row:?}");
        assert!(!row.summary.is_empty(), "empty summary on {row:?}");
        assert!(!row.one_line().is_empty());
    }

    // Non-disclosable records must NOT project. The fixture carries a node
    // registration and a state change; if they leaked in, the count above
    // would already be wrong, but say so explicitly.
    assert!(
        rows.iter().all(
            |row| row.kind != TransparencyKind::Unknown || row.summary.contains("unrecognised")
        ),
        "a recognised-but-non-disclosable record must be skipped, not folded to unknown"
    );
}

/// P-7: populated current-shape evidence, not direction inferred from legacy
/// node-id prefixes. Every remote envelope persists its direction and original
/// task correlation before the export keystone relies on the projection.
#[test]
fn current_fixture_pins_every_projected_variant_and_both_persisted_directions() {
    let entries = current_fixture_entries();
    assert_eq!(entries.len(), 7, "current fixture must be populated");
    assert!(
        entries.iter().all(JournalEntry::has_timestamp),
        "current fixture must carry nonlegacy timestamps"
    );
    assert!(validate_replay_structure(&entries).is_empty());

    let persisted_envelopes: Vec<_> = entries
        .iter()
        .filter_map(|entry| match &entry.record {
            JournalRecord::Room(
                RoomEvent::RemoteEnvelopeAccepted {
                    direction, task, ..
                }
                | RoomEvent::RemoteEnvelopeRejected {
                    direction, task, ..
                },
            ) => Some((*direction, task.as_deref())),
            _ => None,
        })
        .collect();
    assert_eq!(
        persisted_envelopes,
        vec![
            (Direction::Inbound, Some("inbound-accepted-task")),
            (Direction::Inbound, Some("inbound-rejected-task")),
            (Direction::Outbound, Some("outbound-accepted-task")),
            (Direction::Outbound, Some("outbound-rejected-task")),
        ],
        "P-7 requires persisted inbound and outbound directions for accepted and rejected events"
    );

    let rows = fold_transparency(&entries);
    assert_eq!(
        rows.iter()
            .map(|row| (row.seq, row.kind, row.direction, row.task.as_deref()))
            .collect::<Vec<_>>(),
        vec![
            (
                1,
                TransparencyKind::Accepted,
                Direction::Inbound,
                Some("inbound-accepted-task"),
            ),
            (
                2,
                TransparencyKind::Rejected,
                Direction::Inbound,
                Some("inbound-rejected-task"),
            ),
            (
                3,
                TransparencyKind::Accepted,
                Direction::Outbound,
                Some("outbound-accepted-task"),
            ),
            (
                4,
                TransparencyKind::Rejected,
                Direction::Outbound,
                Some("outbound-rejected-task"),
            ),
            (
                5,
                TransparencyKind::AwaitingApproval,
                Direction::Inbound,
                Some("awaiting-task"),
            ),
            (
                6,
                TransparencyKind::StatusQueried,
                Direction::Inbound,
                Some("queried-task"),
            ),
            (7, TransparencyKind::Unknown, Direction::Unknown, None),
        ]
    );
}

#[tokio::test]
async fn the_export_regenerates_byte_identically_after_deletion_and_corruption() {
    let workspace = tempfile::tempdir().unwrap();
    let service = TransparencyService::new(
        Arc::new(FixtureReader(current_fixture_entries())),
        workspace.path().to_path_buf(),
    );

    let report = service.report().await.expect("snapshot");
    assert!(report.is_structurally_consistent());
    let first = service.export_report(&report).await.expect("export");
    assert_eq!(
        first.rows,
        report.rows.len(),
        "the export must report the snapshot's unfiltered row count"
    );
    assert_eq!(
        first.rows, 7,
        "the export must carry the current fixture's rows"
    );
    let good = std::fs::read(&first.path).expect("export exists");
    assert_eq!(
        good.iter().filter(|byte| **byte == b'\n').count(),
        7,
        "one JSON line per row"
    );
    for line in String::from_utf8(good.clone()).unwrap().lines() {
        serde_json::from_str::<serde_json::Value>(line).expect("each line is a JSON object");
    }

    std::fs::write(&first.path, b"{\"corrupt\":true}\n").unwrap();
    let report = service.report().await.expect("fresh snapshot");
    service
        .export_report(&report)
        .await
        .expect("re-export over corruption");
    assert_eq!(std::fs::read(&first.path).unwrap(), good);

    std::fs::remove_file(&first.path).unwrap();
    let report = service.report().await.expect("fresh snapshot");
    service
        .export_report(&report)
        .await
        .expect("re-export after deletion");
    assert_eq!(
        std::fs::read(&first.path).unwrap(),
        good,
        "the export holds no fact that does not live in the journal"
    );
}

#[tokio::test]
async fn the_export_lands_under_the_paths_module_and_is_owner_only() {
    let workspace = tempfile::tempdir().unwrap();
    let service = TransparencyService::new(
        Arc::new(FixtureReader(current_fixture_entries())),
        workspace.path().to_path_buf(),
    );
    let report = service.report().await.expect("snapshot");
    let export = service.export_report(&report).await.expect("export");
    assert_eq!(
        export.path,
        workspace.path().join(".rustain").join("transparency.jsonl"),
        "the path is owned by infrastructure::paths, never inlined"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&export.path)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "the export is operator-only");
    }
}

// ── AC7 — structural replay scope and self-detecting divergence ─────────────

#[test]
fn an_unmodified_journal_is_structurally_replayable() {
    assert!(
        validate_replay_structure(&fixture_entries()).is_empty(),
        "the pinned legacy fixture is structurally replayable"
    );
}

#[tokio::test]
async fn a_sequence_gap_is_reported_as_a_structural_divergence() {
    let mut entries = fixture_entries();
    // Remove a line from the middle — the shape an in-place edit leaves.
    entries.remove(2);

    let findings = validate_replay_structure(&entries);
    assert!(
        findings
            .iter()
            .any(|finding| matches!(finding, StructuralDivergence::SequenceGap { .. })),
        "a removed line breaks `seq` contiguity and must be reported: {findings:?}"
    );

    let workspace = tempfile::tempdir().unwrap();
    let service = TransparencyService::new(
        Arc::new(FixtureReader(entries)),
        workspace.path().to_path_buf(),
    );
    let report = service.report().await.unwrap();
    assert!(!report.is_structurally_consistent());
    let divergence = report
        .structural_divergence_report()
        .expect("divergence is reported");
    assert!(
        divergence.contains("structural divergences"),
        "{divergence}"
    );
    assert!(
        divergence.contains("not structurally replayable"),
        "{divergence}"
    );
}

#[test]
fn structural_replay_claim_excludes_payload_and_provenance_authentication() {
    let claim = STRUCTURAL_REPLAY_CLAIM;
    assert!(claim.contains("append-only"));
    assert!(claim.contains("crash-safe"));
    assert!(claim.contains("structurally replayable"));
    assert!(
        claim.contains("not payload- or provenance-authenticated"),
        "the claim must scope itself: {claim}"
    );
}

// ── AC8 — peer-controlled text is neutralized on the read path too ──────────

#[test]
fn a_hostile_record_already_on_disk_is_neutralized_on_read() {
    // 18.1b journaled records without sanitizing them, so a write-only fix
    // would leave those bytes hot. This entry models one: an escape sequence
    // already durable, projected by today's build.
    let hostile = JournalEntry::new(
        1,
        JournalRecord::Room(RoomEvent::RemoteEnvelopeRejected {
            peer: rustain::domain::models::PeerId::from_public_key(&[9u8; 32]).unwrap(),
            reason: rustain::domain::models::RejectReason::Policy {
                detail: "remote agent requested input (task \u{1b}[2K\rEVIL\u{7}, context \
                         \u{9b}31m); multi-turn not supported"
                    .to_owned(),
            },
            direction: Direction::Outbound,
            task: None,
        }),
        1_700_000_000_000,
    );

    let row = transparency_row(&hostile).expect("a rejection projects");
    for field in [row.summary.as_str(), row.peer.as_str(), &row.one_line()] {
        assert!(
            !field
                .chars()
                .any(|ch| ch.is_control() || ('\u{80}'..='\u{9f}').contains(&ch)),
            "no C0/C1 byte may survive the read path: {field:?}"
        );
    }
    assert!(
        row.summary.contains("EVIL"),
        "the text itself is preserved — only the control bytes are removed: {}",
        row.summary
    );
}

#[tokio::test]
async fn the_export_bytes_carry_no_control_sequences() {
    let workspace = tempfile::tempdir().unwrap();
    let hostile = JournalEntry::new(
        1,
        JournalRecord::Room(RoomEvent::RemoteEnvelopeRejected {
            peer: rustain::domain::models::PeerId::from_public_key(&[9u8; 32]).unwrap(),
            reason: rustain::domain::models::RejectReason::Policy {
                detail: "\u{1b}]0;pwned\u{7}".to_owned(),
            },
            direction: Direction::Outbound,
            task: None,
        }),
        1,
    );
    let service = TransparencyService::new(
        Arc::new(FixtureReader(vec![hostile])),
        workspace.path().to_path_buf(),
    );
    let report = service.report().await.unwrap();
    let export = service.export_report(&report).await.unwrap();
    let bytes = std::fs::read(&export.path).unwrap();

    // Assert on BYTES, not on a rendered screen: pyte interprets escapes, so a
    // `tests_tui/` assertion would pass no matter what we wrote.
    let body = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
    assert!(
        !body.iter().any(|byte| *byte < 0x20 || *byte == 0x7f),
        "the export must contain no C0/DEL bytes: {body:?}"
    );
    assert!(
        !String::from_utf8(body.to_vec())
            .unwrap()
            .chars()
            .any(|ch| ('\u{80}'..='\u{9f}').contains(&ch)),
        "the export must contain no C1 characters"
    );
}

#[test]
fn sanitizing_is_idempotent_and_bounded() {
    let hostile = format!("{}\u{1b}[31m", "x".repeat(MAX_SUMMARY_BYTES * 2));
    let once = sanitize_disclosable(&hostile, MAX_SUMMARY_BYTES);
    let twice = sanitize_disclosable(&once, MAX_SUMMARY_BYTES);
    assert_eq!(once, twice, "the strip must reach a fixed point");
    assert!(once.len() <= MAX_SUMMARY_BYTES + "…[truncated]".len());
    assert!(once.ends_with("…[truncated]"), "truncation is never silent");
}

// ── AC6 support — the filter grammar both faces share ───────────────────────

#[test]
fn both_faces_filter_the_same_rows_from_the_same_fold() {
    let rows = fold_transparency(&fixture_entries());
    let filter = TransparencyFilter::parse("kind=refused").expect("valid filter");
    let filtered: Vec<_> = rows.iter().filter(|row| filter.matches(row)).collect();
    assert!(
        !filtered.is_empty(),
        "the fixture carries refusals, so this filter must not be vacuous"
    );
    assert!(
        filtered
            .iter()
            .all(|row| row.kind == TransparencyKind::Rejected)
    );
    assert!(
        filtered.len() < rows.len(),
        "a filter that keeps everything proves nothing"
    );
}

#[test]
fn the_export_render_is_a_pure_function_of_the_rows() {
    let rows = fold_transparency(&fixture_entries());
    assert_eq!(render_export(&rows), render_export(&rows));
    assert_eq!(
        render_export(&[]),
        "",
        "an empty journal renders an empty file"
    );
}
