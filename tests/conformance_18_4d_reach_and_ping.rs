//! Story 18.4d — peer reach, the acknowledged first frame, and the honesty
//! guardrails around both.
//!
//! Default-feature target. Its stem deliberately omits `p2p`, exactly as
//! `conformance_18_4b_peer_surface.rs` does: the guard
//! `capability_provider.rs::every_p2p_integration_test_is_wired_into_the_ci_p2p_lane`
//! filters `tests/` by `stem.contains("p2p")` and would otherwise demand a
//! `--features p2p` lane this target does not need. It is wired into the `check`
//! job's explicit `--test` enumeration in `.github/workflows/ci.yml`; the
//! live-endpoint half lives in `tests/conformance_p2p_reach.rs`.

use std::path::{Path, PathBuf};

use rustain::adapters::cli::peer::ping::{
    PING_BODY, PING_TTL_MS, PingRefusal, parse_interval, ping_recipient_path, ping_sender_path,
    validate_count,
};
use rustain::adapters::cli::peer::rows;
use rustain::adapters::p2p_config::pin_peer_in_workspace_config;
use rustain::adapters::p2p_reach::{
    load_workspace_p2p_reach, peer_dial_map_from_workspace, publish_self_reach, record_peer_reach,
};
use rustain::domain::models::{
    Direction, FrameOutcome, FrameRefusal, FrameVerdict, JournalEntry, JournalRecord,
    PeerFrameAttemptOutcome, PeerId, PeerReachState, PeerTicket, PinnedKey, RejectReason,
    RoomEvent, peer_fingerprint,
};
use rustain::domain::ports::PeerAddress;
use rustain::domain::services::peer_reach_filter::{
    MAX_TRANSPORT_ADDRESSES, ReachRefusal, describe_ticket_reach, imported_reach,
    transport_address_count,
};
use rustain::domain::services::transparency::{TransparencyKind, fold_transparency};
use rustain::infrastructure::paths::{workspace_p2p_config_path, workspace_p2p_reach_path};

const NOW: i64 = 1_800_000_000;
const HOUR: i64 = 3_600;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn source(relative: &str) -> String {
    std::fs::read_to_string(root().join(relative))
        .unwrap_or_else(|error| panic!("read {relative}: {error}"))
}

fn signer(seed: u8) -> ed25519_dalek::SigningKey {
    ed25519_dalek::SigningKey::from_bytes(&[seed; 32])
}

fn public_key(seed: u8) -> [u8; 32] {
    signer(seed).verifying_key().to_bytes()
}

fn peer(seed: u8) -> PeerId {
    PeerId::from_public_key(&public_key(seed)).expect("peer id")
}

fn pinned(seed: u8) -> PinnedKey {
    PeerTicket::mint(&signer(seed), Vec::new(), i64::MAX, None)
        .expect("ticket")
        .offered_key
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// One address bundle in the shape the transport adapter encodes.
///
/// ⚠ This is a **hand-built mirror** of the adapter's encoding, and that is a
/// double-divergence risk (class C): a shape change here would silently keep this
/// target green. `conformance_p2p_reach.rs` closes it by decoding a **real bound
/// endpoint's** address through the same filter.
fn bundle(seed: u8, sockets: &[&str]) -> Vec<u8> {
    let addrs = sockets
        .iter()
        .map(|socket| format!(r#"{{"Ip":"{socket}"}}"#))
        .collect::<Vec<_>>()
        .join(",");
    format!(r#"{{"id":"{}","addrs":[{addrs}]}}"#, hex(&public_key(seed))).into_bytes()
}

fn address(seed: u8, sockets: &[&str]) -> PeerAddress {
    PeerAddress::from_bytes(bundle(seed, sockets)).expect("address")
}

// ── AC1 / AC4 — the store, the builder, and the separation ──────────────────

/// AC4 ratchet (Rule 4): the two files stay separate **by construction**.
///
/// `p2p.json`'s loader must carry no reach field, and the reach store must be
/// mapped to a dial map by exactly one symbol. Mutant: a second builder, or a
/// reach key inside the allowlist schema.
#[test]
fn ac4_ratchet_the_allowlist_loader_knows_nothing_about_reach() {
    let allowlist = source("src/adapters/p2p_config.rs");
    for reach_token in ["p2p-reach", "PeerReach", "peer_dial_map", "reach:"] {
        assert!(
            !allowlist.contains(reach_token),
            "the admission loader gained the reach token {reach_token:?}; a reachability fact must \
             not be able to make the allowlist Malformed"
        );
    }
    // Positive control: the file really is the allowlist loader.
    assert!(
        allowlist.contains("deny_unknown_fields"),
        "positive control: p2p_config.rs must still be the fail-closed allowlist loader"
    );

    // AC1 mutant (c): `listen: false` must reach no reach write at all. The
    // disabled arm returns before the composition it guards, so the invariant is
    // structural rather than behavioural — there is no listener to observe.
    let daemon = source("src/adapters/daemon/mod.rs");
    let spawn = daemon
        .split("async fn spawn_p2p_listener(")
        .nth(1)
        .expect("the listener gate")
        .split("\n/// A bound listener")
        .next()
        .expect("end of the gate");
    let disabled = spawn.find("if !enabled {").expect("the disabled arm");
    let returns = spawn[disabled..]
        .find("return Ok(None);")
        .expect("the disabled arm returns");
    assert!(
        !spawn[..disabled + returns].contains("publish_self_reach"),
        "a disabled listener must return before anything publishes reach"
    );
    assert!(
        !spawn.contains("publish_self_reach"),
        "the reach write belongs to the bind path, not the gate"
    );

    let reach = source("src/adapters/p2p_reach.rs");
    assert_eq!(
        reach.matches("pub fn peer_dial_map_from_workspace").count(),
        1,
        "exactly one symbol may build the dial map"
    );
    // Every non-test bind site takes the builder's output, never an inline map.
    let daemon = source("src/adapters/daemon/mod.rs");
    let production = daemon
        .split("#[cfg(all(test, unix, feature = \"p2p\"))]")
        .next()
        .expect("production half");
    assert!(
        production.contains("peer_dial_map_from_workspace(workspace)"),
        "the daemon listener must bind with the builder's output"
    );
    assert!(
        !production.contains("IrohPeerTransport::bind(\n            transport_secret_key,\n            std::collections::HashMap::new(),"),
        "the daemon must no longer bind an empty, immutable dial map"
    );
    let bridge = source("src/infrastructure/runtime/peer_bridge.rs");
    assert!(
        bridge.contains("peer_dial_map_from_workspace(workspace)"),
        "`peer ping` must resolve reach through the same builder"
    );
}

/// AC1 positive control plus mutant (a): with no self record a ticket carries no
/// address and the honest empty copy is what renders.
#[test]
fn ac1_the_self_record_is_what_a_ticket_publishes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let reach_path = workspace_p2p_reach_path(dir.path());
    assert_eq!(
        load_workspace_p2p_reach(&reach_path),
        PeerReachState::Absent
    );

    publish_self_reach(&reach_path, &address(1, &["203.0.113.5:4433"]), NOW).expect("publish");
    let state = load_workspace_p2p_reach(&reach_path);
    let own = state.own().expect("self record");
    assert_eq!(own.captured_at, NOW);
    // Positive control: the recorded bundle is the one a ticket would carry, and
    // it decodes through the filter that guards an import.
    assert_eq!(
        imported_reach(
            &[own.address.as_bytes().to_vec()],
            &peer(1),
            /* allow_local */ false
        ),
        Ok(Some(own.address.clone()))
    );
}

// ── AC2 — the ticket copy and the count ─────────────────────────────────────

/// AC2 mutants (c) and (d) plus (e): the copy follows the ticket, and N counts
/// transport addresses **inside** the bundle.
#[test]
fn ac2_the_invite_copy_counts_inside_the_bundle() {
    use rustain::adapters::cli::peer::invite::render_invite;

    let empty = PeerTicket::mint(&signer(2), Vec::new(), NOW + HOUR, None).expect("mint");
    let mut out = Vec::new();
    render_invite(&empty, Some((80, 24)), false, &mut out).expect("render");
    let text = String::from_utf8(out).expect("utf8");
    assert!(text.contains("carries no network address"));
    assert!(
        !text.contains("direct address"),
        "a ticket with no addresses must advertise none: {text}"
    );

    // ONE bundle holding THREE transports. Mutant (e) — counting vector elements
    // renders "1 direct address" here.
    let three = PeerTicket::mint(
        &signer(2),
        vec![bundle(
            2,
            &["203.0.113.5:4433", "203.0.113.6:4433", "203.0.113.7:4433"],
        )],
        NOW + HOUR,
        None,
    )
    .expect("mint");
    assert_eq!(transport_address_count(&three.addresses), 3);
    let mut out = Vec::new();
    render_invite(&three, Some((80, 24)), false, &mut out).expect("render");
    let text = String::from_utf8(out).expect("utf8");
    assert!(text.contains("3 direct addresses"), "{text}");
    assert!(
        text.contains("grants reach, not authority"),
        "the shipped honesty clause must survive: {text}"
    );

    // The singular form, which is the ordinary case for a host with one
    // interface. Mutants (c)/(d): collapsing 1 into the empty sentence, or the
    // reverse, both land here.
    let one = PeerTicket::mint(
        &signer(2),
        vec![bundle(2, &["203.0.113.5:4433"])],
        NOW + HOUR,
        None,
    )
    .expect("mint");
    let mut out = Vec::new();
    render_invite(&one, Some((80, 24)), false, &mut out).expect("render");
    let text = String::from_utf8(out).expect("utf8");
    assert!(text.contains("1 direct address"), "{text}");
    assert!(
        !text.contains("carries no network address"),
        "one address is not no address: {text}"
    );
}

/// AC2 — reach bytes enlarge the blob, so the QR gate is re-checked rather than
/// assumed. A code either renders whole or names the requirement; ⛔ never a
/// scaled or partial one.
#[test]
fn ac2_the_qr_gate_still_holds_once_reach_enlarges_the_blob() {
    use rustain::adapters::cli::peer::invite::{QrOutcome, qr_for};

    let bare = PeerTicket::mint(&signer(3), Vec::new(), NOW + HOUR, None).expect("mint");
    let reachable = PeerTicket::mint(
        &signer(3),
        vec![bundle(3, &["203.0.113.9:4433", "[2001:db8::1]:4433"])],
        NOW + HOUR,
        None,
    )
    .expect("mint");
    let bare_blob = bare.encode().expect("encode");
    let blob = reachable.encode().expect("encode");
    assert!(
        blob.len() > bare_blob.len(),
        "positive control: reach must actually enlarge the blob, or this gate proves nothing"
    );

    match qr_for(&blob, Some((60, 16)), true) {
        QrOutcome::TooSmall { needed_columns, .. } => assert!(needed_columns > 60),
        other => panic!("a reach-bearing ticket must not claim to fit 60x16: {other:?}"),
    }
    match qr_for(&blob, Some((250, 250)), true) {
        QrOutcome::Rendered { art } => assert!(!art.trim().is_empty()),
        other => panic!("a wide terminal must render a complete code: {other:?}"),
    }
}

/// AC2 ruling P3 — the confirm card renders the **actual** addresses, because it
/// is the only human checkpoint before this host dials what a stranger named.
/// Mutant: count-only on the card.
#[test]
fn ac2_the_confirm_card_names_the_addresses_it_would_dial() {
    let ticket = PeerTicket::mint(
        &signer(4),
        vec![bundle(4, &["203.0.113.7:4433"])],
        NOW + HOUR,
        None,
    )
    .expect("mint");
    let card = rows::confirm_card_text("b", &ticket, &peer(4));
    assert!(card.contains("203.0.113.7:4433"), "{card}");
    assert!(
        card.contains("claimed"),
        "the card must say the addresses are claimed, not established: {card}"
    );
    // A count alone cannot tell these two apart, which is the whole reason the
    // card shows them.
    let hostile = PeerTicket::mint(
        &signer(4),
        vec![bundle(4, &["169.254.169.254:80"])],
        NOW + HOUR,
        None,
    )
    .expect("mint");
    let hostile_card = rows::confirm_card_text("b", &hostile, &peer(4));
    assert_eq!(
        rows::reachable_clause(&ticket),
        rows::reachable_clause(&hostile),
        "positive control: the counts really are identical"
    );
    assert_ne!(card, hostile_card);
    assert!(hostile_card.contains("169.254.169.254:80"));
}

// ── AC3 — the import filter and the write ordering ──────────────────────────

/// AC3 mutant (d) and D4: a ticket naming two endpoints is refused, not silently
/// halved. Zero is the honest empty case.
#[test]
fn ac3_bundle_cardinality_is_a_refusal_not_a_choice() {
    assert_eq!(imported_reach(&[], &peer(5), false), Ok(None));
    assert_eq!(
        imported_reach(
            &[
                bundle(5, &["203.0.113.1:4433"]),
                bundle(5, &["203.0.113.2:4433"])
            ],
            &peer(5),
            false
        ),
        Err(ReachRefusal::TooManyBundles { count: 2 })
    );
    // Reuse of the shipped bind check, not a second one: a bundle naming another
    // endpoint is refused.
    assert_eq!(
        imported_reach(&[bundle(6, &["203.0.113.1:4433"])], &peer(5), false),
        Err(ReachRefusal::KeyMismatch)
    );
}

/// AC3 / D15 — the dimension that actually gets dialed is bounded, and every
/// refusal message says nothing was written.
#[test]
fn ac3_the_import_filter_bounds_the_inner_set_and_names_its_reason() {
    let sockets: Vec<String> = (1..=(MAX_TRANSPORT_ADDRESSES + 1))
        .map(|n| format!("203.0.113.{n}:4433"))
        .collect();
    let refs: Vec<&str> = sockets.iter().map(String::as_str).collect();
    let refusal = imported_reach(&[bundle(7, &refs)], &peer(7), false).expect_err("bounded");
    assert!(
        refusal.message().contains("Nothing was written"),
        "{refusal:?}"
    );

    // The metadata endpoint every SSRF write-up names, refused without the
    // per-import opt-in and importable with it.
    let local = [bundle(7, &["169.254.169.254:80"])];
    assert!(matches!(
        imported_reach(&local, &peer(7), false),
        Err(ReachRefusal::LocalNetworkAddress { .. })
    ));
    assert!(imported_reach(&local, &peer(7), true).is_ok());
}

/// AC3 ordering (mutants a/b/c): in the real add flow the reach write is the
/// **last** step, after the confirm, the expiry re-check and the pin.
///
/// Structural, because the alternative is driving a TTY: the four steps have a
/// fixed order in one function and their positions are asserted.
#[test]
fn ac3_ratchet_reach_is_written_after_the_confirm_and_the_pin() {
    let bridge = source("src/infrastructure/runtime/peer_bridge.rs");
    let add = bridge
        .split("async fn run_cli_add(")
        .nth(1)
        .expect("the real add flow")
        .split("\n/// Persist an already-filtered bundle")
        .next()
        .expect("end of the add flow");

    let filter = add.find("imported_reach(").expect("the D15 filter runs");
    let confirm = add
        .find("confirm_on_terminal(")
        .expect("the confirm is taken");
    let expiry = add.find("check_expiry(").expect("expiry is re-checked");
    let pin = add.find("record_pin(").expect("the pin is recorded");
    let reach = add
        .find("write_imported_reach(")
        .expect("reach is recorded");

    assert!(
        filter < confirm,
        "a hostile bundle must be refused before a human is asked anything"
    );
    assert!(confirm < expiry, "expiry is re-checked after the confirm");
    assert!(expiry < pin, "the pin follows the expiry re-check");
    assert!(
        pin < reach,
        "reach is written only after the pin it belongs to"
    );
    // Mutant (a): writing reach before the confirm would leave reach behind on a
    // declined import.
    assert!(
        !add[..confirm].contains("write_imported_reach("),
        "reach must not be written before the confirm"
    );
}

// ── AC5 — the ping copy and its conditional vocabulary ──────────────────────

/// AC5 — every shipped line, and the rule that makes them honest: *accepted* and
/// *refused* appear only when a verdict said so.
#[test]
fn ac5_the_ping_copy_says_only_what_the_verdict_said() {
    let accepted = rows::ping_single_text("b", &peer(8), FrameOutcome::Accepted);
    assert_eq!(
        accepted,
        format!(
            "Sent 1 frame to b ({}) — accepted.",
            peer_fingerprint(&peer(8))
        )
    );

    let unknown = rows::ping_single_text("b", &peer(8), FrameOutcome::Unanswered);
    assert!(
        unknown.contains("outcome unknown; the peer did not answer"),
        "{unknown}"
    );
    // ⛔ Mutant (g4): inferring acceptance from a successful write.
    assert!(
        !unknown.contains("accepted"),
        "a written frame with no answer must never be reported as accepted: {unknown}"
    );

    let refused = rows::ping_single_text(
        "b",
        &peer(8),
        FrameOutcome::Refused(FrameRefusal::NotAdmitted),
    );
    assert!(refused.starts_with("Sent 1 frame to b ("), "{refused}");
    assert!(refused.contains("— refused: "), "{refused}");
    assert!(
        refused.contains("not in the peer's allowlist"),
        "the class must be named in this host's own words: {refused}"
    );

    assert_eq!(
        rows::ping_multi_text("b", &peer(8), 3),
        format!(
            "Sent 3 frames to b ({}) on one connection.",
            peer_fingerprint(&peer(8))
        )
    );
    // A guided retry transmits more envelopes than the frame count names, and
    // the final line must say so; no retry, no clause.
    assert_eq!(rows::ping_retry_clause(1, 1), "");
    assert_eq!(
        rows::ping_retry_clause(1, 2),
        " 2 envelopes were transmitted; the peer guided one position retry."
    );
    let partial = rows::ping_partial_text("b", &peer(8), 1, 3, 2, "a reason");
    assert!(partial.contains("Sent 1 of 3 frames"), "{partial}");
    assert!(
        partial.contains("Nothing further was attempted."),
        "{partial}"
    );

    for (refusal, needle) in [
        (PingRefusal::UnknownAlias, "unknown alias"),
        (PingRefusal::Unpinned, "no pinned key"),
        (PingRefusal::NoReach, "no network address on file"),
        (
            PingRefusal::DialFailed {
                reason: "timed out".to_owned(),
            },
            "dial failed",
        ),
    ] {
        let text = rows::ping_refusal_text("b", &refusal);
        assert!(text.starts_with("Cannot ping b: "), "{text}");
        assert!(text.contains(needle), "{text}");
        assert!(text.ends_with("Nothing was sent."), "{text}");
    }
}

/// AC9 conditional needle rule, scoped to the surface it governs.
///
/// ⚠ Scoped deliberately. `delivered` cannot join the repo-wide needle set: the
/// shipped revocation copy says *"Bytes already delivered stay delivered"* and
/// AC9's own regression floor requires that block byte-identical. So the ban is
/// enforced where it means something — the ping copy — instead of being widened
/// until it has to be weakened.
#[test]
fn ac9_the_ping_copy_stays_inside_the_wording_ceiling() {
    let mut rendered = vec![
        rows::ping_single_text("b", &peer(9), FrameOutcome::Accepted),
        rows::ping_single_text("b", &peer(9), FrameOutcome::Unanswered),
        rows::ping_multi_text("b", &peer(9), 2),
        rows::ping_partial_text("b", &peer(9), 0, 2, 1, "a reason"),
        rows::reach_refreshed_text("b", &peer(9)),
        rows::ping_retry_clause(1, 2),
    ];
    for refusal in [
        PingRefusal::UnknownAlias,
        PingRefusal::Unpinned,
        PingRefusal::NoReach,
        PingRefusal::FeatureDisabled,
        PingRefusal::DialFailed {
            reason: "no route".to_owned(),
        },
        PingRefusal::LocalFault {
            reason: "no key".to_owned(),
        },
    ] {
        rendered.push(rows::ping_refusal_text("b", &refusal));
    }
    for class in [
        FrameRefusal::NotAdmitted,
        FrameRefusal::SignatureInvalid,
        FrameRefusal::FeedPositionMismatch,
        FrameRefusal::Expired,
        FrameRefusal::Malformed,
        FrameRefusal::Declined,
        FrameRefusal::Unavailable,
        FrameRefusal::Unclassified,
    ] {
        rendered.push(rows::ping_single_text(
            "b",
            &peer(9),
            FrameOutcome::Refused(class),
        ));
    }
    assert!(
        rendered.len() >= 20,
        "positive control: the scan found suspiciously few ping strings"
    );
    for line in &rendered {
        let lowered = line.to_ascii_lowercase();
        for forbidden in [
            "delivered",
            "acknowledged",
            "authenticated",
            "verified",
            "secure",
            "tamper-evident",
            "audit trail",
            "evidence",
            "proof",
            "private",
            "anonymous",
            "zero-config",
            "any-nat",
            "relay-reachable",
        ] {
            assert!(
                !lowered.contains(forbidden),
                "forbidden ping wording {forbidden:?} in {line:?}"
            );
        }
    }
    // Positive control for the conditional rule: the two words DO appear, and
    // only in the two forms a verdict produced.
    assert!(rendered.iter().any(|line| line.contains("accepted")));
    assert!(rendered.iter().any(|line| line.contains("refused")));
}

/// AC5 — `--count 0` is refused at parse rather than silently succeeding, and
/// the wire identities are pinned rather than invented per call site.
#[test]
fn ac5_the_verb_refuses_a_zero_count_and_pins_its_wire_identities() {
    assert!(validate_count(0).is_err());
    assert_eq!(validate_count(2), Ok(2));
    assert!(parse_interval("1s").is_ok());
    assert!(parse_interval("later").is_err());

    let local = peer(10);
    assert_eq!(
        ping_sender_path(&local),
        format!("{}/peer-ping", local.as_str())
    );
    assert_eq!(
        ping_recipient_path(&local),
        format!("{}/peer-ping-recipient", local.as_str())
    );
    // The body is a JSON string, which is the shape the delivery front door
    // accepts; `{text: …}` is rejected there, so a fixed shape is one fewer thing
    // to get wrong.
    assert_eq!(PING_BODY, "ping");
    // ⚠ The window is short on purpose: a liveness probe that stays replayable
    // for hours is one an observer can hold and re-present.
    const { assert!(PING_TTL_MS > 0 && PING_TTL_MS <= 300_000) };
}

// ── AC5 / AC6 — the learned feed position ───────────────────────────────────

/// D9 mutants (g2) and (g3): a peer-supplied position is validated before it can
/// reach the local signing path, and there is exactly one guided retry.
#[test]
fn ac5_guided_retry_validates_the_position_and_happens_once() {
    use rustain::domain::models::{FeedPosition, MAX_GUIDED_SEQUENCE_JUMP};

    let start = FeedPosition::start();
    let sane = FeedPosition {
        next_sequence: 7,
        prev_hash: vec![3u8; 32],
    };
    let verdict =
        FrameVerdict::refused(FrameRefusal::FeedPositionMismatch).with_expected(sane.clone());
    assert_eq!(verdict.guided_retry(&start), Some(sane));

    // Mutant (g2): acting on a malformed position.
    for hostile in [
        FeedPosition {
            next_sequence: 7,
            prev_hash: vec![3u8; 31],
        },
        FeedPosition {
            next_sequence: 0,
            prev_hash: Vec::new(),
        },
        FeedPosition {
            next_sequence: u64::MAX,
            prev_hash: Vec::new(),
        },
        FeedPosition {
            next_sequence: MAX_GUIDED_SEQUENCE_JUMP + 2,
            prev_hash: vec![3u8; 32],
        },
    ] {
        assert!(
            FrameVerdict::refused(FrameRefusal::FeedPositionMismatch)
                .with_expected(hostile.clone())
                .guided_retry(&start)
                .is_none(),
            "a hostile position must not reach the signing path: {hostile:?}"
        );
    }

    // Mutant (g3): the retry is spent once. The production loop bounds its
    // attempts structurally, so the bound is asserted where it lives.
    let bridge = source("src/infrastructure/runtime/peer_bridge.rs");
    assert!(
        bridge.contains("for attempt in 1..=2u32"),
        "the guided retry must be bounded by construction, not by hope"
    );
    assert!(
        bridge.contains("guided_retry_spent"),
        "one guided retry per destination per process"
    );
}

// ── AC6 / D10 — the durable rows ────────────────────────────────────────────

fn entry(seq: u64, event: RoomEvent) -> JournalEntry {
    JournalEntry::new(
        seq,
        JournalRecord::Room(event),
        1_700_000_000_000 + seq as i64,
    )
}

/// AC5/AC6 — the two hosts' rows are two independent records of one event, and
/// each renders in the operator's log with the right direction.
#[test]
fn the_two_halves_of_one_frame_render_as_two_rows() {
    let rows = fold_transparency(&[
        entry(
            1,
            RoomEvent::PeerFrameAttempted {
                peer: peer(11),
                correlation: "corr-1".to_owned(),
                bytes: 512,
                outcome: PeerFrameAttemptOutcome::Accepted,
                refusal: None,
            },
        ),
        entry(
            2,
            RoomEvent::RemoteEnvelopeRejected {
                peer: peer(11),
                reason: RejectReason::Policy {
                    detail: "transport admission refused a frame from the connected key: this key \
                             is not in the peer allowlist"
                        .to_owned(),
                },
                direction: Direction::Inbound,
                task: Some("corr-2".to_owned()),
            },
        ),
    ]);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].kind, TransparencyKind::PeerFrameAttempted);
    assert_eq!(rows[0].direction, Direction::Outbound);
    assert_eq!(rows[0].task.as_deref(), Some("corr-1"));
    assert!(rows[0].summary.contains("peer accepted this host's frame"));
    assert_eq!(rows[1].kind, TransparencyKind::Rejected);
    assert_eq!(rows[1].direction, Direction::Inbound);
    assert!(
        rows[1].summary.contains("not in the peer allowlist"),
        "the receiver's row must name the typed refusal: {}",
        rows[1].summary
    );
    // ⛔ Truthfulness: the refusal precedes signature checking, so the row must
    // not imply the envelope was checked.
    for banned in ["signature", "authenticated", "tamper"] {
        assert!(
            !rows[1].summary.to_ascii_lowercase().contains(banned),
            "an admission refusal must claim nothing about the envelope: {}",
            rows[1].summary
        );
    }
}

/// AC5 — an outcome the verdict did not establish never renders as acceptance.
#[test]
fn an_unanswered_frame_never_renders_as_accepted() {
    for (outcome, needle) in [
        (
            PeerFrameAttemptOutcome::OutcomeUnknown,
            "the peer did not answer",
        ),
        (PeerFrameAttemptOutcome::SendFailed, "could not send"),
        (PeerFrameAttemptOutcome::Unknown, "is unknown"),
    ] {
        let rows = fold_transparency(&[entry(
            1,
            RoomEvent::PeerFrameAttempted {
                peer: peer(12),
                correlation: "c".to_owned(),
                bytes: 1,
                outcome,
                refusal: None,
            },
        )]);
        assert!(rows[0].summary.contains(needle), "{}", rows[0].summary);
        assert!(
            !rows[0].summary.contains("accepted"),
            "{outcome:?} must not render as acceptance: {}",
            rows[0].summary
        );
    }
}

/// Task 0 / D10 — the new event degrades on an older build.
///
/// Two levels: an unknown `event` tag lands on `RoomEvent::Unrecognized`, and an
/// outcome value below a known tag lands on the nested sentinel instead of
/// failing the whole journal line.
#[test]
fn the_outbound_event_degrades_rather_than_failing_a_journal_line() {
    let unknown_tag: RoomEvent =
        serde_json::from_str(r#"{"event":"peer_frame_teleported","peer":"x"}"#)
            .expect("an unknown event tag must not fail the load");
    assert_eq!(unknown_tag, RoomEvent::Unrecognized);

    let unknown_outcome: RoomEvent = serde_json::from_value(serde_json::json!({
        "event": "peer_frame_attempted",
        "peer": peer(13).as_str(),
        "correlation": "c",
        "bytes": 8,
        "outcome": "invented_by_a_newer_build",
    }))
    .expect("an unknown outcome value must not fail the load");
    assert!(matches!(
        unknown_outcome,
        RoomEvent::PeerFrameAttempted {
            outcome: PeerFrameAttemptOutcome::Unknown,
            ..
        }
    ));
    // A future refusal class degrades the same way, and stays a refusal.
    let unknown_class: RoomEvent = serde_json::from_value(serde_json::json!({
        "event": "peer_frame_attempted",
        "peer": peer(13).as_str(),
        "correlation": "c",
        "bytes": 8,
        "outcome": "refused",
        "refusal": "invented_by_a_newer_build",
    }))
    .expect("an unknown refusal class must not fail the load");
    assert!(matches!(
        unknown_class,
        RoomEvent::PeerFrameAttempted {
            outcome: PeerFrameAttemptOutcome::Refused,
            refusal: Some(FrameRefusal::Unclassified),
            ..
        }
    ));
}

// ── AC9 — NFR74 over the types this story added ─────────────────────────────

/// AC9 / NFR74 — the new domain values carry no transport type and no address.
///
/// ⛔ Asserted on the **types that must not appear**: a grep for `NodeId` passes
/// vacuously because that name does not exist in iroh 1.0.3 at all.
#[test]
fn ac9_nfr74_the_new_domain_values_carry_no_transport_identity() {
    for (relative, positive_control) in [
        ("src/domain/models/peer_frame.rs", "pub struct FrameVerdict"),
        ("src/domain/models/peer_frame.rs", "pub struct FeedPosition"),
        (
            "src/domain/services/refusal_quota.rs",
            "pub struct RefusalJournalQuota",
        ),
    ] {
        let text = source(relative);
        assert!(
            text.contains(positive_control),
            "positive control missing from {relative}: {positive_control}"
        );
        for forbidden in [
            "iroh::",
            "EndpointId",
            "EndpointAddr",
            "PeerAddress",
            "SocketAddr",
            "transport_address",
        ] {
            assert!(
                !text.contains(forbidden),
                "NFR74 violation in {relative}: {forbidden} is transport data, not a domain value"
            );
        }
    }

    // The outbound room event is the one that reaches the durable journal, so it
    // is held to the same rule as the rest of `RoomEvent`.
    let room = source("src/domain/models/orchestration_room.rs");
    let variant = room
        .split("PeerFrameAttempted {")
        .nth(1)
        .expect("the outbound variant")
        .split("},")
        .next()
        .expect("end of the variant");
    for forbidden in ["PeerAddress", "EndpointAddr", "address", "socket"] {
        assert!(
            !variant.contains(forbidden),
            "the outbound event must carry no address: {forbidden}"
        );
    }
    assert!(
        variant.contains("peer: PeerId"),
        "positive control: it does carry the domain identity"
    );
}

/// AC9 — every module this story added is inside the wording ceiling, and the
/// banned words really are absent from them.
#[test]
fn ac9_the_wording_ceiling_covers_every_module_this_story_added() {
    let ceiling = source("tests/conformance_p2p_ingress.rs");
    let owned = ceiling
        .split("let owned_modules = [")
        .nth(1)
        .expect("owned_modules")
        .split("];")
        .next()
        .expect("end of array");
    let added = [
        "src/domain/models/peer_frame.rs",
        "src/domain/models/peer_reach.rs",
        "src/domain/services/peer_reach_filter.rs",
        "src/domain/services/refusal_quota.rs",
        "src/adapters/p2p_reach.rs",
        "src/adapters/cli/peer/ping.rs",
    ];
    for module in added {
        assert!(
            owned.contains(module),
            "{module} renders operator copy and is not inside the wording ceiling"
        );
        assert!(
            root().join(module).exists(),
            "{module} is named in the ceiling but does not exist"
        );
        let text = source(module);
        let production = text.split("#[cfg(test)]").next().unwrap_or(&text);
        for banned in ["\"verified", " verified", "cannot read", "\"private"] {
            assert!(
                !production.to_ascii_lowercase().contains(banned),
                "{module} carries the banned wording {banned:?}"
            );
        }
    }
}

/// Task 8 — both new targets are named by a lane that runs them. A target wired
/// into nothing compiles, passes locally, and is never run by CI.
#[test]
fn both_new_test_targets_are_named_by_a_lane_that_runs_them() {
    let ci = source(".github/workflows/ci.yml");
    assert!(
        ci.contains("--test conformance_18_4d_reach_and_ping"),
        "this default-lane target must be named in the check job"
    );
    assert!(
        ci.contains("--test conformance_p2p_reach"),
        "the p2p-lane target must be named in the p2p job"
    );
}

/// AC4 — `Unreachable` keeps its one meaning: an address-book miss, never a
/// network verdict. Mutant (b): returning it for a live-address dial failure.
#[test]
fn ac4_unreachable_stays_an_address_book_miss() {
    let adapter = source("src/adapters/iroh/mod.rs");
    let dial = adapter
        .split("async fn dial(")
        .nth(1)
        .expect("the dial path")
        .split("\n    async fn send_to(")
        .next()
        .expect("end of dial");
    assert_eq!(
        dial.matches("PeerTransportError::Unreachable").count(),
        1,
        "exactly one place may produce Unreachable, and it is the address-book miss"
    );
    let miss = dial
        .find("self\n            .peer_addresses")
        .expect("the address-book lookup");
    let unreachable = dial
        .find("PeerTransportError::Unreachable")
        .expect("the miss arm");
    assert!(
        miss < unreachable,
        "Unreachable must be the consequence of the lookup, not of a connect failure"
    );
    assert!(
        dial.contains("PeerTransportError::Dial(error.to_string())"),
        "a dial that reached the network and failed must surface Dial"
    );
}

/// AC3 refresh (ruling P7) — the same-alias reach refresh exists and cannot
/// become a re-pin.
#[test]
fn ac3_the_reach_refresh_changes_only_the_address() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config_path = workspace_p2p_config_path(dir.path());
    let reach_path = workspace_p2p_reach_path(dir.path());
    let key = pinned(14);
    pin_peer_in_workspace_config(&config_path, "b", &key).expect("pin");
    let id = key.peer_id().expect("peer id");
    let before = std::fs::read(&config_path).expect("read allowlist");

    record_peer_reach(
        &reach_path,
        &config_path,
        "b",
        &id,
        &address(14, &["203.0.113.20:4433"]),
        NOW,
    )
    .expect("first record");
    // A restart changes the port; the refresh records the new one.
    record_peer_reach(
        &reach_path,
        &config_path,
        "b",
        &id,
        &address(14, &["203.0.113.20:55001"]),
        NOW + 10,
    )
    .expect("refresh");

    let map = peer_dial_map_from_workspace(dir.path());
    assert_eq!(map.len(), 1);
    assert_eq!(
        describe_ticket_reach(&[map[&id].as_bytes().to_vec()]),
        ["ip 203.0.113.20:55001"]
    );
    assert_eq!(
        std::fs::read(&config_path).expect("read allowlist"),
        before,
        "a reach refresh must leave the allowlist byte-identical"
    );

    // The refresh card says the key is not up for decision.
    let ticket = PeerTicket::mint(
        &signer(14),
        vec![bundle(14, &["203.0.113.20:55001"])],
        NOW + HOUR,
        None,
    )
    .expect("mint");
    let card = rows::reach_refresh_card_text("b", &ticket, &id);
    assert!(card.contains("this key is unchanged"), "{card}");
    assert!(card.contains("203.0.113.20:55001"), "{card}");
    assert!(
        !card.contains("Confirm and pin"),
        "a refresh must not present itself as a pin: {card}"
    );
}
