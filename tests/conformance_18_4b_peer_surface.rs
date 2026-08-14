//! Story 18.4b — Peer Admission Surface conformance.
//!
//! Default-feature target. It is named WITHOUT `p2p` in its stem on purpose:
//! `capability_provider.rs::every_p2p_integration_test_is_wired_into_the_ci_p2p_lane`
//! filters `tests/` by `stem.contains("p2p")` and would demand this target be
//! run in the `--features p2p` lane, which it does not need. It is wired into
//! the `check` job's explicit `--test` enumeration in `.github/workflows/ci.yml`
//! instead — a target in no lane is a silent false green (AC7).
//!
//! The `p2p`-gated half of this story's evidence (AC6's positive control, which
//! needs a live transport) lives in `tests/conformance_p2p_peer_admission.rs`.

use ed25519_dalek::SigningKey;

use rustain::domain::models::{
    PEER_FINGERPRINT_COLUMNS, PEER_TICKET_PREFIX, PeerTicket, PeerTicketError, PinnedKey,
    short_fingerprint,
};
use rustain::domain::services::peer_admission::{
    PeerImportVerdict, PeerRevokeVerdict, PeerRoster, peer_import_verdict, peer_revoke_verdict,
    peer_roster, resolve_configured_p2p_target,
};

const NOW: i64 = 1_760_000_000;
const HOUR: i64 = 3_600;

fn signer(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn fresh_ticket(seed: u8) -> PeerTicket {
    PeerTicket::mint(
        &signer(seed),
        vec![
            b"opaque-address-one".to_vec(),
            b"opaque-address-two".to_vec(),
        ],
        NOW + HOUR,
        Some("their-suggested-name".to_owned()),
    )
    .expect("minting a ticket for a well-formed key must succeed")
}

fn pinned_key_of(ticket: &PeerTicket) -> PinnedKey {
    ticket.offered_key.clone()
}

fn spec(alias: &str, key: Option<PinnedKey>) -> rustain::domain::models::P2pPeerSpec {
    rustain::domain::models::P2pPeerSpec::new(alias.to_owned(), key)
}

// ── AC1 — the ticket: expiring, issuer-signed, one identity derivation ──────

/// Positive control. Without it every refusal assertion below could pass
/// against a decoder that refuses everything.
#[test]
fn a_fresh_correctly_signed_ticket_round_trips() {
    let ticket = fresh_ticket(7);
    let blob = ticket.encode().expect("encode");
    assert!(
        blob.starts_with(PEER_TICKET_PREFIX),
        "the blob must be self-identifying: {blob}"
    );
    let decoded = PeerTicket::decode(&blob, NOW).expect("a fresh ticket must import");
    assert_eq!(decoded, ticket, "the ticket must survive the round trip");
    assert_eq!(
        decoded.addresses,
        vec![
            b"opaque-address-one".to_vec(),
            b"opaque-address-two".to_vec()
        ],
        "opaque reachability bytes must survive verbatim"
    );
    assert_eq!(
        decoded.peer_id().expect("peer id"),
        ticket
            .offered_key
            .peer_id()
            .expect("the one derivation must agree"),
        "the ticket must derive its identity through PinnedKey::peer_id and nowhere else"
    );
}

/// Mutant (a): expiry ignored → an expired ticket imports.
#[test]
fn an_expired_ticket_refuses_with_its_own_reason() {
    let ticket = PeerTicket::mint(&signer(9), Vec::new(), NOW - 1, None).expect("mint");
    let blob = ticket.encode().expect("encode");
    match PeerTicket::decode(&blob, NOW) {
        Err(PeerTicketError::Expired { not_after }) => assert_eq!(not_after, NOW - 1),
        other => panic!("an expired ticket must refuse as Expired, got {other:?}"),
    }
    // The same ticket one second earlier is fine: the boundary is `now > not_after`.
    assert!(
        PeerTicket::decode(&blob, NOW - 1).is_ok(),
        "a ticket must be usable up to and including its expiry instant"
    );
}

/// Mutant (b): issuer signature unchecked → an altered ticket imports.
#[test]
fn an_altered_ticket_refuses_as_a_signature_mismatch() {
    let mut ticket = fresh_ticket(11);
    // Alter a signed field without re-signing: the addresses.
    ticket.addresses.push(b"an-address-nobody-signed".to_vec());
    let blob = ticket.encode().expect("encode");
    assert!(
        matches!(
            PeerTicket::decode(&blob, NOW),
            Err(PeerTicketError::BadIssuerSignature)
        ),
        "an altered ticket must refuse as BadIssuerSignature"
    );

    // Expiry is also covered by the signature.
    let mut moved_expiry = fresh_ticket(11);
    moved_expiry.not_after = NOW + 10 * HOUR;
    let blob = moved_expiry.encode().expect("encode");
    assert!(
        matches!(
            PeerTicket::decode(&blob, NOW),
            Err(PeerTicketError::BadIssuerSignature)
        ),
        "extending an expiry must not survive the signature check"
    );

    // So is the suggested name — a remote-editable field that reaches a
    // terminal must not be alterable in flight.
    let mut renamed = fresh_ticket(11);
    renamed.suggested_name = Some("a-name-nobody-signed".to_owned());
    let blob = renamed.encode().expect("encode");
    assert!(
        matches!(
            PeerTicket::decode(&blob, NOW),
            Err(PeerTicketError::BadIssuerSignature)
        ),
        "rewriting the suggested name must not survive the signature check"
    );
}

/// Mutant (c): expired and altered collapse to one reason.
#[test]
fn malformed_expired_and_altered_are_three_distinct_reasons() {
    let malformed = PeerTicket::decode("not-a-ticket-at-all", NOW);
    assert!(
        matches!(malformed, Err(PeerTicketError::Malformed { .. })),
        "a paste that is not a ticket must refuse as Malformed, got {malformed:?}"
    );
    let truncated = PeerTicket::decode(&format!("{PEER_TICKET_PREFIX}!!!not-base64!!!"), NOW);
    assert!(
        matches!(truncated, Err(PeerTicketError::Malformed { .. })),
        "a corrupt body must refuse as Malformed, got {truncated:?}"
    );

    // A ticket that is BOTH altered and expired reports the signature failure:
    // an unchecked field is never used to refuse. The two reasons therefore
    // never collapse into one another.
    let mut both = PeerTicket::mint(&signer(13), Vec::new(), NOW - 1, None).expect("mint");
    both.addresses.push(b"unsigned".to_vec());
    let blob = both.encode().expect("encode");
    assert!(
        matches!(
            PeerTicket::decode(&blob, NOW),
            Err(PeerTicketError::BadIssuerSignature)
        ),
        "signature is checked before expiry, so the two reasons stay distinct"
    );

    // And each reason renders differently for the operator.
    let reasons: Vec<String> = vec![
        PeerTicketError::Malformed {
            reason: "x".to_owned(),
        }
        .to_string(),
        PeerTicketError::Expired { not_after: 0 }.to_string(),
        PeerTicketError::BadIssuerSignature.to_string(),
    ];
    let unique: std::collections::BTreeSet<&String> = reasons.iter().collect();
    assert_eq!(unique.len(), 3, "each refusal must read differently");
}

/// A ticket signed by one key but offering another is not importable: the
/// signature is checked against the key the ticket itself offers, so swapping
/// the key invalidates it.
#[test]
fn a_ticket_whose_key_was_swapped_refuses() {
    let mut ticket = fresh_ticket(17);
    ticket.offered_key = pinned_key_of(&fresh_ticket(18));
    let blob = ticket.encode().expect("encode");
    assert!(
        matches!(
            PeerTicket::decode(&blob, NOW),
            Err(PeerTicketError::BadIssuerSignature)
        ),
        "swapping the offered key must invalidate the issuer signature"
    );
}

/// Fingerprints render head…tail so both ends stay checkable. Head-only
/// truncation is forgeable at the tail (AC4 mutant (c) rests on this).
#[test]
fn fingerprints_truncate_head_and_tail() {
    let long = "0123456789abcdef0123456789abcdef";
    let short = short_fingerprint(long, PEER_FINGERPRINT_COLUMNS);
    assert_eq!(short.chars().count(), PEER_FINGERPRINT_COLUMNS);
    assert!(short.contains('…'), "must be middle-elided: {short}");
    assert!(
        short.starts_with("01234") && short.ends_with("bcdef"),
        "both ends must survive: {short}"
    );
    // Two keys that share a long prefix and differ only at the tail must render
    // differently — the property head-only truncation destroys.
    let a = format!("{}{}", "a".repeat(60), "1111");
    let b = format!("{}{}", "a".repeat(60), "2222");
    assert_ne!(
        short_fingerprint(&a, PEER_FINGERPRINT_COLUMNS),
        short_fingerprint(&b, PEER_FINGERPRINT_COLUMNS),
        "tail-differing keys must not render identically"
    );
    assert_eq!(short_fingerprint("short", 12), "short");
}

// ── AC3/AC4 — the import decision core ─────────────────────────────────────

#[test]
fn a_new_alias_pins_and_an_identical_reimport_changes_nothing() {
    let ticket = fresh_ticket(21);
    let key = pinned_key_of(&ticket);
    assert_eq!(
        peer_import_verdict("alice", &key, &P2P_ABSENT),
        PeerImportVerdict::Pin,
        "an absent allowlist is created by the first import"
    );
    let roster =
        rustain::domain::models::P2pConfigState::Present(vec![spec("alice", Some(key.clone()))]);
    assert_eq!(
        peer_import_verdict("alice", &key, &roster),
        PeerImportVerdict::AlreadyPinned,
        "positive control for AC4: a MATCHING re-import must not alarm"
    );
    assert_eq!(
        peer_import_verdict("bob", &key, &roster),
        PeerImportVerdict::AlreadyPinnedAs {
            alias: "alice".to_owned()
        },
        "one cryptographic peer identity must not be admitted under two aliases"
    );
}

#[test]
fn import_rejects_blank_aliases_and_compares_identity_not_key_metadata() {
    let key = pinned_key_of(&fresh_ticket(22));
    assert_eq!(
        peer_import_verdict(" \t", &key, &P2P_ABSENT),
        PeerImportVerdict::InvalidAlias
    );

    let mut metadata_variant = key.clone();
    metadata_variant.kid = Some("display-metadata-only".to_owned());
    let roster = rustain::domain::models::P2pConfigState::Present(vec![spec("alice", Some(key))]);
    assert_eq!(
        peer_import_verdict("alice", &metadata_variant, &roster),
        PeerImportVerdict::AlreadyPinned,
        "kid metadata must not turn one Ed25519 identity into a key mismatch"
    );
}

/// AC4 mutant (a): import overwrites the pin when the alias matches and the key
/// differs. The verdict core must name the collision instead.
#[test]
fn a_different_key_for_a_pinned_alias_is_a_mismatch_not_an_overwrite() {
    let on_file = pinned_key_of(&fresh_ticket(31));
    let offered = pinned_key_of(&fresh_ticket(32));
    assert_ne!(on_file, offered, "fixture must offer two distinct keys");
    let roster = rustain::domain::models::P2pConfigState::Present(vec![spec(
        "alice",
        Some(on_file.clone()),
    )]);
    match peer_import_verdict("alice", &offered, &roster) {
        PeerImportVerdict::KeyMismatch { on_file: found } => assert_eq!(found, on_file),
        other => panic!("a rotated key must alarm, got {other:?}"),
    }
}

/// An alias the operator wrote but never pinned is *unanswerable*, not
/// *mismatched*: the first import is its first key, not a replacement.
#[test]
fn an_unpinned_alias_accepts_a_first_key_without_alarming() {
    let offered = pinned_key_of(&fresh_ticket(41));
    let roster = rustain::domain::models::P2pConfigState::Present(vec![spec("carol", None)]);
    assert_eq!(
        peer_import_verdict("carol", &offered, &roster),
        PeerImportVerdict::Pin
    );
}

/// A file whose contents could not be parsed is not a file to overwrite.
#[test]
fn an_unreadable_allowlist_refuses_both_import_and_revocation() {
    let broken = rustain::domain::models::P2pConfigState::Malformed {
        reason: "invalid JSON".to_owned(),
    };
    let offered = pinned_key_of(&fresh_ticket(51));
    assert!(matches!(
        peer_import_verdict("alice", &offered, &broken),
        PeerImportVerdict::RefuseUnreadable { .. }
    ));
    assert!(matches!(
        peer_revoke_verdict("alice", &broken),
        PeerRevokeVerdict::RefuseUnreadable { .. }
    ));
}

// ── AC5 — four states, no invented columns ─────────────────────────────────

const P2P_ABSENT: rustain::domain::models::P2pConfigState =
    rustain::domain::models::P2pConfigState::Absent;

/// Mutants (a) empty renders as an error, (b) absent and empty render
/// identically, (c) a peer with no pinned key renders as admitted — all rest on
/// the four states staying distinct in the projection.
#[test]
fn the_four_allowlist_states_stay_distinct() {
    assert_eq!(peer_roster(&P2P_ABSENT), PeerRoster::Absent);
    assert_eq!(
        peer_roster(&rustain::domain::models::P2pConfigState::Present(Vec::new())),
        PeerRoster::Empty,
        "an allowlist present and empty is a well-formed 'admit nobody'"
    );
    assert!(matches!(
        peer_roster(&rustain::domain::models::P2pConfigState::Malformed {
            reason: "invalid JSON".to_owned()
        }),
        PeerRoster::Unreadable { .. }
    ));
    let key = pinned_key_of(&fresh_ticket(61));
    let populated = peer_roster(&rustain::domain::models::P2pConfigState::Present(vec![
        spec("alice", Some(key)),
        spec("carol", None),
    ]));
    match populated {
        PeerRoster::Populated(rows) => {
            assert_eq!(rows.len(), 2);
            assert!(rows[0].is_pinned(), "alice carries a pin");
            assert!(
                !rows[1].is_pinned(),
                "carol has no pinned key and admits nobody"
            );
        }
        other => panic!("expected a populated roster, got {other:?}"),
    }
}

/// AC5 positive control: a well-formed two-peer allowlist renders two pinned
/// rows. Without it, a projection that emitted "admits nobody" for every state
/// would pass every four-state mutant.
#[test]
fn a_two_peer_allowlist_projects_two_pinned_rows() {
    let rows = match peer_roster(&rustain::domain::models::P2pConfigState::Present(vec![
        spec("alice", Some(pinned_key_of(&fresh_ticket(71)))),
        spec("bob", Some(pinned_key_of(&fresh_ticket(72)))),
    ])) {
        PeerRoster::Populated(rows) => rows,
        other => panic!("expected populated, got {other:?}"),
    };
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|row| row.is_pinned()));
    assert_ne!(
        rows[0].peer_id, rows[1].peer_id,
        "two distinct keys must derive two distinct identities"
    );
}

// ── AC6 — target resolution never manufactures identity ────────────────────

#[test]
fn a_revocation_target_resolves_alias_or_configured_peer_id_but_never_a_phantom() {
    let key = pinned_key_of(&fresh_ticket(81));
    let configured_id = key.peer_id().expect("peer id");
    let peers = vec![spec("alice", Some(key)), spec("carol", None)];

    assert_eq!(
        resolve_configured_p2p_target("alice", &peers).map(|spec| spec.id.as_str()),
        Some("alice")
    );
    assert_eq!(
        resolve_configured_p2p_target(configured_id.as_str(), &peers).map(|spec| spec.id.as_str()),
        Some("alice"),
        "a configured PeerId must resolve to its alias"
    );
    assert!(
        resolve_configured_p2p_target("nobody", &peers).is_none(),
        "an unknown alias must not resolve"
    );
    // A syntactically valid PeerId that no configured pin derives is a phantom.
    let phantom = pinned_key_of(&fresh_ticket(82)).peer_id().expect("peer id");
    assert!(
        resolve_configured_p2p_target(phantom.as_str(), &peers).is_none(),
        "a well-formed id no entry derives must not resolve"
    );

    let roster = rustain::domain::models::P2pConfigState::Present(peers);
    match peer_revoke_verdict("alice", &roster) {
        PeerRevokeVerdict::Remove { alias, peer_id } => {
            assert_eq!(alias, "alice");
            assert_eq!(peer_id, Some(configured_id));
        }
        other => panic!("expected a removal, got {other:?}"),
    }
    // Mutant (b): revoking an unknown alias appends an event. The verdict must
    // be "nothing recorded" so no caller has anything to journal.
    assert_eq!(
        peer_revoke_verdict("nobody", &roster),
        PeerRevokeVerdict::NothingRecorded
    );
    // A revocation never manufactures identity: an unpinned entry is removable
    // but carries no PeerId to name.
    match peer_revoke_verdict("carol", &roster) {
        PeerRevokeVerdict::Remove { alias, peer_id } => {
            assert_eq!(alias, "carol");
            assert_eq!(peer_id, None);
        }
        other => panic!("expected a removal, got {other:?}"),
    }
    assert_eq!(
        peer_revoke_verdict("alice", &P2P_ABSENT),
        PeerRevokeVerdict::NothingRecorded,
        "an absent allowlist records nothing to revoke"
    );
}

#[test]
fn an_exact_alias_wins_over_a_peer_id_shaped_alias() {
    let shadowed_key = pinned_key_of(&fresh_ticket(83));
    let target = shadowed_key.peer_id().expect("peer id").to_string();
    let exact_alias_key = pinned_key_of(&fresh_ticket(84));
    let exact_alias_id = exact_alias_key.peer_id().expect("peer id");
    let peers = vec![
        spec(&target, Some(exact_alias_key)),
        spec("alice", Some(shadowed_key)),
    ];

    let resolved = resolve_configured_p2p_target(&target, &peers).expect("exact alias");
    assert_eq!(resolved.id, target);
    assert_eq!(
        resolved.pinned_identity(),
        Some(exact_alias_id),
        "a PeerId-shaped alias must not revoke a different entry by derived identity"
    );
}

/// Doc comments with their `///` markers stripped and whitespace normalized, so
/// a phrase that wraps across physical lines is still one phrase. ⚠ A naive
/// `split_whitespace` over the raw source leaves a `///` token between the words
/// and every wrapped phrase silently fails to match.
fn normalized_docs(source: &str) -> String {
    source
        .lines()
        .map(|line| {
            let trimmed = line.trim_start();
            trimmed
                .strip_prefix("///")
                .or_else(|| trimmed.strip_prefix("//!"))
                .or_else(|| trimmed.strip_prefix("//"))
                .unwrap_or(trimmed)
        })
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Source with comment lines removed, so an assertion about what the *code* does
/// is not defeated by a comment explaining why it does not do the opposite.
fn code_only(source: &str) -> String {
    source
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            !trimmed.starts_with("//")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ── AC2 — the blob is primary, the QR is size-gated ─────────────────────────

/// Mutant (a): the QR renders unconditionally → red at the 60×16 layout floor.
/// Mutant (b): below the threshold the QR is dropped with no explanation.
#[test]
fn the_qr_is_size_gated_and_degrades_to_a_stated_reason() {
    use rustain::adapters::cli::peer::invite::{QrOutcome, qr_for};

    let blob = fresh_ticket(101).encode().expect("encode");

    // Never rendered unless asked for.
    assert_eq!(
        qr_for(&blob, Some((200, 200)), false),
        QrOutcome::NotRequested,
        "the copyable ticket is the primary artifact; the QR is opt-in"
    );

    // At the layout floor it refuses AND names the requirement, so mutant (b)
    // (silent omission) and mutant (a) (render anyway) are both red.
    match qr_for(&blob, Some((60, 16)), true) {
        QrOutcome::TooSmall {
            needed_columns,
            needed_rows,
            have_columns,
            have_rows,
        } => {
            assert!(
                needed_columns > 60 || needed_rows > 16,
                "a code that fits the floor would make this gate vacuous"
            );
            assert_eq!((have_columns, have_rows), (60, 16));
        }
        other => panic!("a ticket-sized QR must not claim to fit 60x16: {other:?}"),
    }

    // An unmeasurable terminal is treated as "does not fit", never as
    // permission to try.
    assert!(
        matches!(qr_for(&blob, None, true), QrOutcome::TooSmall { .. }),
        "an unknown terminal size must not render a code"
    );
}

/// AC2 positive control: at a wide terminal the blob **and** the QR both render,
/// and the QR encodes the same payload — without it, a gate that refuses every
/// size would pass the mutants above vacuously.
#[test]
fn at_a_wide_terminal_the_blob_and_the_qr_both_render_from_one_payload() {
    use rustain::adapters::cli::peer::invite::{QrOutcome, qr_for, render_invite};

    let ticket = fresh_ticket(103);
    let blob = ticket.encode().expect("encode");
    let art = match qr_for(&blob, Some((200, 200)), true) {
        QrOutcome::Rendered { art } => art,
        other => panic!("a wide terminal must render a complete code: {other:?}"),
    };
    assert!(
        !art.trim().is_empty(),
        "the rendered code must have content"
    );

    let mut out = Vec::new();
    render_invite(&ticket, Some((200, 200)), true, &mut out).expect("render");
    let text = String::from_utf8(out).expect("utf8");
    assert!(
        text.contains(&blob),
        "the copyable ticket must be printed even when a code renders"
    );
    assert!(
        text.contains(art.lines().next().expect("a code row")),
        "the same payload must render as a code beside the ticket"
    );

    // ⛔ The QR is built from the printed blob itself, so the two cannot encode
    // different payloads. Prove the coupling: a different ticket renders a
    // different code.
    let other_blob = fresh_ticket(104).encode().expect("encode");
    assert_ne!(other_blob, blob);
    let other_art = match qr_for(&other_blob, Some((200, 200)), true) {
        QrOutcome::Rendered { art } => art,
        other => panic!("expected a code: {other:?}"),
    };
    assert_ne!(art, other_art, "two payloads must not render one code");
}

/// The invite copy states the honest facts and ⛔ never calls a ticket a secret
/// or a credential.
#[test]
fn the_invite_copy_states_reach_expiry_and_the_absence_of_an_address() {
    use rustain::adapters::cli::peer::invite::render_invite;

    let ticket = PeerTicket::mint(&signer(111), Vec::new(), NOW + HOUR, None).expect("mint");
    let mut out = Vec::new();
    render_invite(&ticket, Some((80, 24)), false, &mut out).expect("render");
    let text = String::from_utf8(out).expect("utf8");
    for needle in [
        "carries no network address",
        "grants reach, not authority",
        "one person",
        "not a secret and not a credential",
        "directly-addressable peers only; relay disabled",
    ] {
        assert!(text.contains(needle), "invite copy must state {needle:?}");
    }
    // ⛔ It never claims to carry an address it does not carry.
    assert!(
        !text.contains("direct address"),
        "a ticket with no addresses must not advertise any: {text}"
    );

    // With addresses it says so, and still never calls the ticket a secret.
    let reachable = PeerTicket::mint(
        &signer(112),
        vec![b"a".to_vec(), b"b".to_vec()],
        NOW + HOUR,
        None,
    )
    .expect("mint");
    let mut out = Vec::new();
    render_invite(&reachable, Some((80, 24)), false, &mut out).expect("render");
    let text = String::from_utf8(out).expect("utf8");
    assert!(text.contains("2 direct addresses"), "{text}");
}

// ── AC3 — the writer: atomic, 0o600, and losslessly faithful ───────────────

fn write_config(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
    let path = dir.join(".rustain").join("p2p.json");
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(&path, body).expect("write fixture");
    path
}

/// AC3 mutant (b): a rewrite normalizes `pinned_key` → `pinnedKey`, injects a
/// `listen` key the operator never wrote, or reorders `agents`.
#[test]
fn the_writer_preserves_spelling_defaults_and_order() {
    use rustain::adapters::p2p_config::pin_peer_in_workspace_config;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(
        dir.path(),
        r#"{
  "agents": {
    "zeta": {
      "pinned_key": {
        "x": "OCX0lGCg_qqiATl8qK7m2tG-G9meEbgr2SXLLMtKyeI",
        "alg": "EdDSA"
      }
    },
    "carol": {}
  }
}
"#,
    );
    let key = pinned_key_of(&fresh_ticket(121));
    pin_peer_in_workspace_config(&path, "newcomer", &key).expect("pin");
    let after = std::fs::read_to_string(&path).expect("read");

    assert!(
        after.contains("\"pinned_key\""),
        "the operator's snake_case spelling must survive: {after}"
    );
    assert!(
        !after.contains("\"pinnedKey\": {\n        \"alg\": \"EdDSA\",\n        \"x\": \"OCX0"),
        "zeta's entry must not be renamed or reordered: {after}"
    );
    assert!(
        !after.contains("\"listen\""),
        "a key the operator never wrote must not be injected: {after}"
    );
    let zeta = after.find("\"zeta\"").expect("zeta");
    let carol = after.find("\"carol\"").expect("carol");
    let newcomer = after.find("\"newcomer\"").expect("newcomer");
    assert!(
        zeta < carol && carol < newcomer,
        "hand-authored alias order must survive and new entries append: {after}"
    );
    // The leaf order inside an untouched pinned key survives too.
    let entry = &after[zeta..carol];
    assert!(
        entry.find("\"x\"") < entry.find("\"alg\""),
        "leaf order inside an untouched key must survive: {entry}"
    );
    // And the file still parses through the SHIPPED loader.
    match rustain::adapters::p2p_config::load_workspace_p2p_config(&path) {
        rustain::domain::models::P2pConfigState::Present(peers) => {
            assert_eq!(peers.len(), 3);
        }
        other => panic!("the rewrite must stay readable: {other:?}"),
    }
}

/// An explicit `"listen": true` the operator DID write survives, in place.
#[test]
fn the_writer_preserves_a_listen_flag_the_operator_wrote() {
    use rustain::adapters::p2p_config::pin_peer_in_workspace_config;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(dir.path(), "{\n  \"listen\": true,\n  \"agents\": {}\n}\n");
    pin_peer_in_workspace_config(&path, "alice", &pinned_key_of(&fresh_ticket(131))).expect("pin");
    let after = std::fs::read_to_string(&path).expect("read");
    assert!(after.contains("\"listen\": true"), "{after}");
    assert!(
        after.find("\"listen\"") < after.find("\"agents\""),
        "top-level key order must survive: {after}"
    );
    assert!(
        rustain::adapters::p2p_config::p2p_listener_requested(&path).expect("read listen"),
        "the listener flag must still read as true"
    );
}

/// AC3 mutant (c): a non-atomic write leaves a truncated file on interrupt.
///
/// The writer creates a sibling temp, chmods it, fsyncs it, then renames — so
/// the target is either the old file or the new one, never a partial. Asserted
/// through the two observable consequences: mode `0o600`, and no temp left
/// behind.
#[test]
fn the_writer_is_atomic_and_not_world_readable() {
    use rustain::adapters::p2p_config::pin_peer_in_workspace_config;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(".rustain").join("p2p.json");
    pin_peer_in_workspace_config(&path, "alice", &pinned_key_of(&fresh_ticket(141)))
        .expect("pin into a fresh workspace");
    assert!(path.exists(), "the writer must create the file");
    let temporary_files: Vec<_> = std::fs::read_dir(path.parent().expect("parent"))
        .expect("read config directory")
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.starts_with(".p2p-config-") && name.ends_with(".tmp"))
        .collect();
    assert!(
        temporary_files.is_empty(),
        "no randomized temporary file may survive a successful write: {temporary_files:?}"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "an allowlist must not be world-readable");
    }
}

/// A file that does not parse is not a file to overwrite — the writer refuses
/// and leaves it byte-identical.
#[test]
fn the_writer_refuses_an_unparseable_file_without_touching_it() {
    use rustain::adapters::p2p_config::{
        pin_peer_in_workspace_config, remove_peer_from_workspace_config,
    };

    let dir = tempfile::tempdir().expect("tempdir");
    let original = "{ this is not json";
    let path = write_config(dir.path(), original);
    assert!(
        pin_peer_in_workspace_config(&path, "alice", &pinned_key_of(&fresh_ticket(151))).is_err()
    );
    assert!(remove_peer_from_workspace_config(&path, "alice").is_err());
    assert_eq!(
        std::fs::read_to_string(&path).expect("read"),
        original,
        "a refused write must leave the file untouched"
    );
}

#[test]
fn the_writer_refuses_well_formed_but_invalid_config_without_touching_it() {
    use rustain::adapters::p2p_config::pin_peer_in_workspace_config;

    for (case, original) in [
        (
            "unknown root field",
            r#"{"listen":false,"agents":{},"unexpected":13}"#,
        ),
        (
            "unknown peer field",
            r#"{"agents":{"alice":{"unexpected":13}}}"#,
        ),
        (
            "invalid pinned key",
            r#"{"agents":{"alice":{"pinnedKey":{"alg":"EdDSA","x":"not-base64"}}}}"#,
        ),
        (
            "duplicate alias",
            r#"{"agents":{"alice":{},"alice":{"pinnedKey":null}}}"#,
        ),
    ] {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_config(dir.path(), original);
        let result = pin_peer_in_workspace_config(&path, "bob", &pinned_key_of(&fresh_ticket(152)));
        assert!(result.is_err(), "{case} must refuse, got {result:?}");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            original,
            "{case} must remain byte-identical"
        );
    }
}

#[test]
fn the_writer_rejects_blank_aliases_and_treats_kid_as_metadata() {
    use rustain::adapters::p2p_config::pin_peer_in_workspace_config;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(".rustain").join("p2p.json");
    let key = pinned_key_of(&fresh_ticket(155));
    assert!(
        pin_peer_in_workspace_config(&path, " \t", &key).is_err(),
        "the public writer must reject an alias the loader cannot represent"
    );
    assert!(
        !path.exists(),
        "a rejected blank alias must create no config"
    );

    pin_peer_in_workspace_config(&path, "alice", &key).expect("first pin");
    let before = std::fs::read_to_string(&path).expect("read");
    let mut metadata_variant = key;
    metadata_variant.kid = Some("display-metadata-only".to_owned());
    pin_peer_in_workspace_config(&path, "alice", &metadata_variant)
        .expect("same identity is idempotent");
    assert_eq!(
        std::fs::read_to_string(&path).expect("read"),
        before,
        "metadata-only differences must not rewrite a pinned identity"
    );
}

#[test]
fn the_loader_and_writer_refuse_one_identity_under_two_aliases() {
    use rustain::adapters::p2p_config::{load_workspace_p2p_config, pin_peer_in_workspace_config};
    use rustain::domain::models::P2pConfigState;

    let key = pinned_key_of(&fresh_ticket(153));
    let body = serde_json::to_string_pretty(&serde_json::json!({
        "agents": {
            "alice": {"pinnedKey": {"alg": "EdDSA", "x": key.x.clone()}},
            "bob": {"pinnedKey": {"alg": "EdDSA", "x": key.x.clone()}},
        }
    }))
    .expect("config");
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(dir.path(), &body);

    assert!(
        matches!(
            load_workspace_p2p_config(&path),
            P2pConfigState::Malformed { .. }
        ),
        "an ambiguous identity-to-alias mapping must fail closed"
    );
    assert!(
        pin_peer_in_workspace_config(&path, "carol", &pinned_key_of(&fresh_ticket(154))).is_err(),
        "the writer must not normalize or extend an ambiguous mapping"
    );
    assert_eq!(std::fs::read_to_string(&path).expect("read"), body);
}

#[cfg(unix)]
#[test]
fn concurrent_config_rewrites_preserve_every_successful_update() {
    use std::sync::{Arc, Barrier};

    use rustain::adapters::p2p_config::{load_workspace_p2p_config, pin_peer_in_workspace_config};
    use rustain::domain::models::P2pConfigState;

    const WRITERS: usize = 8;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = Arc::new(dir.path().join(".rustain").join("p2p.json"));
    let barrier = Arc::new(Barrier::new(WRITERS));
    let mut handles = Vec::new();
    for index in 0..WRITERS {
        let path = Arc::clone(&path);
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            let alias = format!("peer-{index}");
            let key = pinned_key_of(&fresh_ticket(160 + index as u8));
            barrier.wait();
            pin_peer_in_workspace_config(&path, &alias, &key)
        }));
    }
    for handle in handles {
        handle
            .join()
            .expect("writer thread")
            .expect("serialized pin");
    }

    match load_workspace_p2p_config(&path) {
        P2pConfigState::Present(peers) => {
            assert_eq!(peers.len(), WRITERS, "no successful update may be lost");
        }
        other => panic!("serialized rewrites must leave readable config, got {other:?}"),
    }
}

// ── AC3/AC4 — the confirm has no bypass, the alarm binds no key ─────────────

/// AC3 mutant (a): a `--yes` path pins without the confirm.
///
/// There is no such flag on the CLI (clap would reject it) and the `/peer`
/// parser refuses it by name rather than ignoring it, so an operator who
/// believes they skipped the gate is told they did not.
#[test]
fn no_flag_skips_the_confirm() {
    use rustain::adapters::tui::handlers::peer_command::parse_peer_command;

    for attempt in ["add alice TICKET --yes", "add alice TICKET --force"] {
        let error = parse_peer_command(Some(attempt)).expect_err(attempt);
        assert!(
            error.contains("no way to skip the confirm"),
            "a bypass attempt must be named and refused: {error}"
        );
    }
    // The CLI surface carries no such flag either.
    let source = std::fs::read_to_string("src/adapters/cli/peer/mod.rs").expect("read");
    for forbidden in ["yes", "force"] {
        assert!(
            !source.contains(&format!("{forbidden}: bool")),
            "`peer add` must not grow a {forbidden} flag"
        );
    }
    // ⛔ And a non-interactive run refuses rather than defaulting to yes.
    assert!(
        rustain::adapters::cli::peer::add::NO_TERMINAL_REFUSAL.contains("Refusing to pin"),
        "a non-TTY must refuse with a named reason"
    );
}

/// AC4 mutants (b) only the offered fingerprint renders, (c) head-truncation
/// instead of head…tail, (d) any key is bound on this surface.
#[test]
fn the_alarm_names_both_fingerprints_binds_no_key_and_offers_no_accept() {
    use rustain::adapters::cli::peer::rows::key_mismatch_text;

    let on_file = pinned_key_of(&fresh_ticket(161))
        .peer_id()
        .expect("peer id");
    let offered = pinned_key_of(&fresh_ticket(162))
        .peer_id()
        .expect("peer id");
    let block = key_mismatch_text("alice", &on_file, &offered);

    // (b) both fingerprints, labelled in both directions.
    assert!(block.contains("on file"), "{block}");
    assert!(block.contains("offered"), "{block}");
    // (c) head…tail, not head-only — both ends of both keys survive.
    for id in [&on_file, &offered] {
        let short = rustain::domain::models::peer_fingerprint(id);
        assert!(block.contains(&short), "{short} missing from {block}");
        assert!(short.contains('…'), "{short} must be middle-elided");
    }
    // (d) no accept affordance anywhere. Replacing a pin is two verbs.
    assert!(
        !block.contains("[y]") && !block.contains("Confirm"),
        "the alarm must paint no accept key: {block}"
    );
    assert!(
        block.contains("rustain peer revoke alice"),
        "the only accept path is a separate, named verb: {block}"
    );
    assert!(
        block.contains("rustain peer show alice"),
        "comparison routes to the verb that prints both keys whole: {block}"
    );
    // The doctrine is the shipped sentence, single-sourced.
    // Case-insensitive: the alarm opens a sentence with the doctrine, so it
    // capitalizes the first letter of the single-sourced constant.
    assert!(
        block
            .to_ascii_lowercase()
            .contains(rustain::domain::services::peer_admission::ROTATED_KEY_DOCTRINE),
        "the alarm must state the shipped doctrine: {block}"
    );
    assert!(block.contains("Nothing was changed"), "{block}");
    // ⛔ Observation, never cause.
    for forbidden in [
        "attack",
        "compromise",
        "interception",
        "impersonation",
        "intercept",
    ] {
        assert!(
            !block.to_ascii_lowercase().contains(forbidden),
            "the alarm must state the observation, never the cause: {forbidden}"
        );
    }
}

/// AC4 mutant (d), the security-weight branch, asserted structurally: the
/// mismatch surface must not reach the apply card's mode-blind key table, and
/// the confirm card must not either.
#[test]
fn neither_peer_surface_routes_through_the_apply_card_key_table() {
    for relative in [
        "src/adapters/cli/peer/rows.rs",
        "src/adapters/tui/handlers/peer_command.rs",
        "src/adapters/tui/widgets/peer_add_prompt.rs",
        "src/infrastructure/runtime/peer_bridge.rs",
    ] {
        let source = std::fs::read_to_string(relative).expect(relative);
        let production = code_only(source.split("#[cfg(test)]").next().unwrap_or(&source));
        for forbidden in ["choice_for_key", "APPLY_CARD_BINDINGS", "bindings_for"] {
            assert!(
                !production.contains(forbidden),
                "{relative} must not bind a key through {forbidden}: it is single-sourced \
                 and consulted mode-blind, so a card painting no `y` would still accept on `y`"
            );
        }
    }
    // Positive control: the apply card really does own that table, so the
    // assertions above are not vacuously true of a renamed symbol.
    let apply = std::fs::read_to_string("src/adapters/tui/widgets/apply_card.rs").expect("read");
    assert!(apply.contains("pub fn choice_for_key"), "{apply:.0}");

    // The alarm block carries NO actions, so no keystroke resolves it at all.
    let handler =
        std::fs::read_to_string("src/adapters/tui/handlers/peer_command.rs").expect("read");
    let alarm = handler
        .split("pub fn show_peer_alarm")
        .nth(1)
        .expect("show_peer_alarm");
    let alarm = &alarm[..alarm.find("\n}").expect("end")];
    assert!(
        alarm.contains("FeedbackLevel::Error"),
        "the alarm must be a never-truncated Error block: {alarm}"
    );
    assert!(
        alarm.contains("actions: vec![]"),
        "the alarm must carry no actions: {alarm}"
    );
}

/// The confirm card states what a pin governs and what it does not claim.
#[test]
fn the_confirm_card_states_what_a_pin_governs() {
    use rustain::adapters::cli::peer::rows::confirm_card_text;

    let ticket = fresh_ticket(171);
    let peer_id = ticket.peer_id().expect("peer id");
    let card = confirm_card_text("alice", &ticket, &peer_id);
    for needle in [
        "Add peer — alice",
        "fingerprint",
        "who may reach this host",
        "not that",
        "never trust",
        "[y] Confirm and pin",
        "[n] Cancel (Esc)",
    ] {
        assert!(card.contains(needle), "confirm card must state {needle:?}");
    }
    // It names WHICH encoding it shows: three encodings of one key reach
    // operators and they are not interchangeable.
    assert!(card.contains("peer id digest"), "{card}");
    // AC1's advisory-name rule, made visible: the ticket's suggestion is shown
    // as a suggestion, and the operator's alias is what gets written.
    assert!(
        card.contains("their-suggested-name") && card.contains("your alias above"),
        "a ticket-suggested name must render as advisory only: {card}"
    );
}

// ── AC5 — the roster render ────────────────────────────────────────────────

/// Mutants (a) empty renders as an error/warning, (b) absent and empty render
/// identically, (c) a peer with no pinned key renders as admitted.
#[test]
fn the_roster_renders_four_distinct_states_and_empty_is_not_an_error() {
    use rustain::adapters::cli::peer::rows::render_roster;
    use rustain::domain::models::P2pConfigState;

    let absent = render_roster(&peer_roster(&P2P_ABSENT), Some(false));
    let empty = render_roster(
        &peer_roster(&P2pConfigState::Present(Vec::new())),
        Some(false),
    );
    let broken = render_roster(
        &peer_roster(&P2pConfigState::Malformed {
            reason: "invalid JSON".to_owned(),
        }),
        None,
    );
    let populated = render_roster(
        &peer_roster(&P2pConfigState::Present(vec![
            spec("alice", Some(pinned_key_of(&fresh_ticket(181)))),
            spec("carol", None),
        ])),
        Some(true),
    );

    assert!(absent.contains("No peer allowlist. This host admits no peer."));
    assert!(empty.contains("Allowlist present and empty — admits nobody."));
    assert!(broken.contains("Allowlist unreadable: invalid JSON"));
    assert_ne!(
        absent, empty,
        "absent and empty must not render identically"
    );
    // ⛔ Empty is a deliberate posture, not a fault.
    let lowered = empty.to_ascii_lowercase();
    for forbidden in ["error", "warning", "misconfigur", "run init", "invalid"] {
        assert!(
            !lowered.contains(forbidden),
            "an empty allowlist is a valid posture, not {forbidden}: {empty}"
        );
    }
    // (c) an unpinned peer must not read as admitted.
    assert!(
        populated.contains("no pinned key — admits nobody"),
        "{populated}"
    );
    assert!(populated.contains(": pinned"), "{populated}");
    // ⛔ No column the domain holds no fact for.
    for forbidden in ["last_seen", "last seen", "tier", "relayed", "connected"] {
        assert!(
            !populated.to_ascii_lowercase().contains(forbidden),
            "no invented column: {forbidden} in {populated}"
        );
    }
    // The header says what this list is, and is not.
    assert!(
        populated.contains("configuration, not connection status"),
        "{populated}"
    );
    assert!(populated.contains("listener: on"), "{populated}");
    assert!(absent.contains("listener: off"), "{absent}");
    // A failed listener read is unknown, never guessed as off.
    assert!(
        !broken.contains("listener:"),
        "an unreadable listener flag must not be reported as off: {broken}"
    );
}

/// Mutant (d): an alias containing an ANSI escape reaches the terminal
/// unsanitized. A hand-edited config file is untrusted input to a terminal.
#[test]
fn aliases_are_sanitized_on_the_read_path() {
    use rustain::adapters::cli::peer::rows::render_roster;
    use rustain::domain::models::P2pConfigState;

    let hostile = "al\x1b[31mice\x07\x1b]0;pwned\x07";
    let rendered = render_roster(
        &peer_roster(&P2pConfigState::Present(vec![spec(
            hostile,
            Some(pinned_key_of(&fresh_ticket(191))),
        )])),
        Some(false),
    );
    assert!(
        !rendered.contains('\x1b'),
        "no escape may reach the terminal: {rendered:?}"
    );
    assert!(
        !rendered.contains('\x07'),
        "no control character may reach the terminal: {rendered:?}"
    );
    assert!(
        rendered.contains("ice"),
        "the readable text survives: {rendered}"
    );
}

/// `peer show` prints both keys whole, because AC4 sends the operator here.
#[test]
fn peer_show_prints_both_keys_whole() {
    use rustain::adapters::cli::peer::rows::show_text;

    let key = pinned_key_of(&fresh_ticket(201));
    let peer_id = key.peer_id().expect("peer id");
    let text = show_text("alice", Some(&key), Some(&peer_id));
    assert!(
        text.contains(&key.x),
        "the pinned key must print whole: {text}"
    );
    assert!(
        text.contains(peer_id.as_str()),
        "the peer id must print whole: {text}"
    );
    assert!(
        text.contains("recognition aid, not a comparison aid"),
        "{text}"
    );
    // An unpinned entry says it admits nobody rather than showing nothing.
    let none = show_text("carol", None, None);
    assert!(none.contains("admits nobody"), "{none}");
}

// ── AC6 — the journaled variant renders, and concedes what it cannot undo ───

/// The variant AC4 and AC6 share is ONE variant with three outcomes, and it
/// projects into `/team log` as its own kind.
#[test]
fn the_transport_admission_variant_projects_one_kind_with_three_outcomes() {
    use rustain::domain::models::{JournalEntry, JournalRecord, PeerAdmissionOutcome, RoomEvent};
    use rustain::domain::services::transparency::{TransparencyKind, transparency_row};

    let peer_id = pinned_key_of(&fresh_ticket(211))
        .peer_id()
        .expect("peer id");
    let mut seen = Vec::new();
    for (seq, outcome) in [
        PeerAdmissionOutcome::Pinned,
        PeerAdmissionOutcome::Revoked,
        PeerAdmissionOutcome::ImportRefused,
    ]
    .into_iter()
    .enumerate()
    {
        let entry = JournalEntry {
            schema_version: 1,
            seq: seq as u64 + 1,
            recorded_at_ms: 1_760_000_000_000,
            record: JournalRecord::Room(RoomEvent::PeerAdmissionRecorded {
                alias: "alice".to_owned(),
                peer: Some(peer_id.clone()),
                outcome,
            }),
        };
        let row = transparency_row(&entry).expect("a transport-admission fact must render");
        assert_eq!(
            row.kind,
            TransparencyKind::TransportAdmission,
            "one variant, one kind"
        );
        assert_eq!(row.peer, peer_id.as_str());
        assert!(row.summary.contains("alice"), "{:?}", row.summary);
        if outcome == PeerAdmissionOutcome::Revoked {
            assert!(
                row.summary.contains("open connections stay open"),
                "{:?}",
                row.summary
            );
            assert!(
                row.summary
                    .contains("already-delivered bytes stay delivered"),
                "{:?}",
                row.summary
            );
        }
        seen.push(row.summary);
    }
    let unique: std::collections::BTreeSet<&String> = seen.iter().collect();
    assert_eq!(
        unique.len(),
        3,
        "three outcomes must read differently: {seen:?}"
    );

    // Mutant (c): the variant reuses RoomRoleRevoked. It must be its own kind,
    // distinct from all three sibling withdrawal kinds.
    for sibling in [
        TransparencyKind::ConsentRevoked,
        TransparencyKind::RoomRoleRevoked,
        TransparencyKind::RoomRoleGranted,
    ] {
        assert_ne!(TransparencyKind::TransportAdmission, sibling);
        assert_ne!(
            TransparencyKind::TransportAdmission.glyph(),
            sibling.glyph()
        );
        assert_ne!(
            TransparencyKind::TransportAdmission.label(),
            sibling.label()
        );
    }
    // ⛔ And it carries no transport address of any kind (NFR74).
    let model = std::fs::read_to_string("src/domain/models/orchestration_room.rs").expect("read");
    let variant = model
        .split("PeerAdmissionRecorded {")
        .nth(1)
        .expect("variant");
    let variant = &variant[..variant.find("\n    }").expect("end")];
    for forbidden in ["Endpoint", "Address", "addr", "recorded_at"] {
        assert!(
            !variant.contains(forbidden),
            "the variant must carry no transport address: {forbidden} in {variant}"
        );
    }
}

#[test]
fn unknown_or_missing_admission_outcomes_never_deserialize_as_pinned() {
    use rustain::adapters::cli::peer::rows::outcome_label;
    use rustain::domain::models::{PeerAdmissionOutcome, RoomEvent};

    for json in [
        r#"{"event":"peer_admission_recorded","alias":"alice","outcome":"future_value"}"#,
        r#"{"event":"peer_admission_recorded","alias":"alice"}"#,
    ] {
        let event: RoomEvent = serde_json::from_str(json).expect("forward-compatible event");
        match event {
            RoomEvent::PeerAdmissionRecorded { outcome, .. } => {
                assert_eq!(outcome, PeerAdmissionOutcome::Unknown);
                assert_eq!(outcome_label(outcome), "unknown");
            }
            other => panic!("known event tag must remain typed, got {other:?}"),
        }
    }
}

/// The revocation copy states the one true observable and concedes the rest.
///
/// Mutant (d): success copy claims the connection was closed.
#[test]
fn the_revocation_copy_concedes_what_it_cannot_undo() {
    use rustain::adapters::cli::peer::rows::revocation_text;

    let peer_id = pinned_key_of(&fresh_ticket(221))
        .peer_id()
        .expect("peer id");
    let text = revocation_text("alice", Some(&peer_id));
    assert!(text.contains("Recorded revocation of alice"), "{text}");
    assert!(text.contains("next frame is refused"), "{text}");
    assert!(text.contains("no restart needed"), "{text}");
    assert!(
        text.contains("Bytes already delivered stay delivered"),
        "{text}"
    );
    let lowered = text.to_ascii_lowercase();
    for forbidden in [
        "closed the connection",
        "disconnected",
        "session ended",
        "rekey",
        "can no longer read",
        "revoked their key",
    ] {
        assert!(
            !lowered.contains(forbidden),
            "a revocation must not claim {forbidden}: {text}"
        );
    }
}

/// `--now` is a documented no-op alias, and the copy says so rather than
/// implying it selected a behaviour.
#[test]
fn now_is_documented_as_changing_nothing() {
    let note = rustain::adapters::cli::peer::revoke::NOW_IS_A_NO_OP;
    assert!(note.contains("changes nothing"), "{note}");
    assert!(note.contains("per frame"), "{note}");
}

// ── AC7 — the wiring chain, entered through the production path ─────────────

/// Trap 3: an unlisted verb silently never runs. A handler-only test stays green
/// while the command is dead, so this keystone enters through
/// `submit_message_for_test` — the production input router.
#[test]
fn the_peer_command_routes_as_an_execute_command_not_a_missing_custom_command() {
    use rustain::adapters::tui::app::{InputAction, submit_message_for_test};
    use rustain::adapters::tui::state::TuiState;

    for input in [
        "/peer",
        "/peer list",
        "/peer list --json",
        "/peer show alice",
        "/peer invite --ttl=2h",
        "/peer add alice rustain-peer1.abc",
        "/peer revoke alice",
    ] {
        let mut state = TuiState::new(120, 24);
        state.input_buffer = input.to_owned();
        match submit_message_for_test(&mut state) {
            InputAction::ExecuteCommand { name, args } => {
                assert_eq!(name, "peer", "{input}");
                let tail = input.strip_prefix("/peer").unwrap().trim();
                if tail.is_empty() {
                    assert!(args.is_none(), "{input} → {args:?}");
                } else {
                    assert_eq!(args.as_deref(), Some(tail), "{input}");
                }
            }
            other => panic!("`{input}` must reach the /peer dispatch arm, got {other:?}"),
        }
    }
}

/// Every sub-verb the CLI ships is parseable by the slash face, and the two
/// faces agree on the verb set. ⛔ Two cores is the defect.
#[test]
fn the_slash_face_parses_every_sub_verb_the_cli_ships() {
    use rustain::adapters::tui::handlers::peer_command::{PeerCommandArgs, parse_peer_command};

    assert_eq!(
        parse_peer_command(None).expect("bare"),
        PeerCommandArgs::List { json: false },
        "bare /peer opens the roster"
    );
    assert_eq!(
        parse_peer_command(Some("list --json")).expect("json"),
        PeerCommandArgs::List { json: true }
    );
    assert_eq!(
        parse_peer_command(Some("show alice")).expect("show"),
        PeerCommandArgs::Show {
            alias: "alice".to_owned()
        }
    );
    assert_eq!(
        parse_peer_command(Some("add alice BLOB")).expect("add"),
        PeerCommandArgs::Add {
            alias: "alice".to_owned(),
            ticket: "BLOB".to_owned()
        }
    );
    assert_eq!(
        parse_peer_command(Some("revoke alice --now")).expect("revoke"),
        PeerCommandArgs::Revoke {
            target: "alice".to_owned()
        },
        "--now parses and changes nothing"
    );
    // An alias is mandatory on `add`: without it AC4 cannot fire at all.
    assert!(parse_peer_command(Some("add")).is_err());
    assert!(parse_peer_command(Some("add BLOB")).is_err());
    assert!(parse_peer_command(Some("nonsense")).is_err());
}

/// The registry entry advertises every sub-verb — a tested convention.
#[test]
fn the_palette_entry_names_every_sub_verb() {
    use rustain::adapters::command_registry::CommandRegistry;

    let registry = CommandRegistry::new();
    let entry = registry.find("peer").expect("/peer must be registered");
    for verb in ["list", "show", "invite", "add", "revoke"] {
        assert!(
            entry.description.contains(verb),
            "the /peer description must advertise `{verb}`: {}",
            entry.description
        );
    }
    // ⛔ No tier, plan, edition or posture anywhere in the surface's copy.
    let lowered = entry.description.to_ascii_lowercase();
    for forbidden in ["tier", "enterprise", "plan", "edition"] {
        assert!(!lowered.contains(forbidden), "{forbidden} in {lowered}");
    }
}

/// AC7 mutant (a): the `owned_modules` extension is omitted, so the wording
/// ceiling stays vacuous for this story's copy.
///
/// Asserted from the other side: every module this story adds is named in the
/// ceiling's array, and the banned words really are absent from those modules —
/// so inserting one goes red.
#[test]
fn the_wording_ceiling_covers_every_module_this_story_added() {
    let ceiling = std::fs::read_to_string("tests/conformance_p2p_ingress.rs").expect("read");
    let owned = ceiling
        .split("let owned_modules = [")
        .nth(1)
        .expect("owned_modules")
        .split("];")
        .next()
        .expect("end of array");
    let added = [
        "src/domain/models/peer_ticket.rs",
        "src/domain/services/peer_admission.rs",
        "src/adapters/cli/peer/mod.rs",
        "src/adapters/cli/peer/rows.rs",
        "src/adapters/cli/peer/invite.rs",
        "src/adapters/cli/peer/add.rs",
        "src/adapters/cli/peer/list.rs",
        "src/adapters/cli/peer/show.rs",
        "src/adapters/cli/peer/revoke.rs",
        "src/adapters/tui/handlers/peer_command.rs",
        "src/adapters/tui/widgets/peer_add_prompt.rs",
        "src/infrastructure/runtime/peer_bridge.rs",
    ];
    for module in added {
        assert!(
            owned.contains(module),
            "{module} renders operator copy and is not inside the wording ceiling"
        );
        assert!(
            std::path::Path::new(module).exists(),
            "{module} is named in the ceiling but does not exist"
        );
    }
    // The banned word most likely to reach this copy is the shipped word for a
    // pinned peer. It must be absent, so a mutant inserting it goes red.
    for module in added {
        let source = std::fs::read_to_string(module).expect(module);
        let production = source.split("#[cfg(test)]").next().unwrap_or(&source);
        for banned in ["\"verified", " verified", "cannot read", "\"private"] {
            assert!(
                !production.to_ascii_lowercase().contains(banned),
                "{module} carries the banned wording {banned:?}"
            );
        }
    }
}

/// AC7 mutant (b): a new test target is added but wired into no lane, so it
/// compiles, passes locally, and CI never runs it.
#[test]
fn both_new_test_targets_are_named_by_a_lane_that_runs_them() {
    let ci = std::fs::read_to_string(".github/workflows/ci.yml").expect("read");

    // The default-feature target belongs to the `check` job's enumeration.
    let check: String = ci
        .lines()
        .skip_while(|line| !line.contains("name: Test"))
        .take_while(|line| !line.starts_with("  skills-validation:"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        check.contains("--test conformance_18_4b_peer_surface"),
        "the default-feature target must be in the check job's enumeration"
    );
    // Its stem must NOT contain `p2p`, or the p2p wiring guard would demand it
    // be in the p2p lane instead.
    assert!(
        !"conformance_18_4b_peer_surface".contains("p2p"),
        "this target is default-feature and must not be matched by the p2p guard"
    );

    // The p2p-gated target belongs to the `p2p` lane, and its stem contains
    // `p2p` so the wiring guard enforces exactly that.
    let lane: String = ci
        .lines()
        .skip_while(|line| *line != "  p2p:")
        .skip(1)
        .take_while(|line| line.is_empty() || !line.starts_with("  ") || line.starts_with("    "))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        lane.contains("--test conformance_p2p_peer_admission"),
        "the p2p-gated target must be in the p2p lane"
    );
    assert!("conformance_p2p_peer_admission".contains("p2p"));
    assert!(
        std::path::Path::new("tests/conformance_p2p_peer_admission.rs").exists(),
        "positive control: the p2p-gated target must exist"
    );
}

/// The cut-1 scope fence is retired **by name**, with a record of why.
#[test]
fn the_cut_one_scope_fence_is_retired_by_name() {
    let source = std::fs::read_to_string("tests/conformance_p2p_architecture.rs").expect("read");
    assert!(
        !source.contains("fn p2p_has_no_event_loop_palette_or_slash_command_surface"),
        "the scope fence must not still assert a surface this story ships"
    );
    assert!(
        source.contains("p2p_has_no_event_loop_palette_or_slash_command_surface"),
        "it must be retired by name, not silently deleted"
    );
    assert!(
        source.contains("scope fence for cut 1"),
        "the retirement must record that it was a fence, not a bug"
    );
}

/// The two now-false doc comments are amended in this commit.
#[test]
fn the_two_stale_doc_comments_are_amended() {
    let room = std::fs::read_to_string("src/domain/models/orchestration_room.rs").expect("read");
    let normalized = normalized_docs(&room);
    assert!(
        normalized.contains("Story 18.4b authored [`RoomEvent::PeerAdmissionRecorded`] for that"),
        "the block must name the variant that now exists"
    );
    // The false sentence survives only as a quoted correction, never as a claim
    // — the amendment records what it used to say and that it was false.
    assert!(
        normalized.contains("was false at HEAD"),
        "the amendment must record the correction rather than quietly repairing it"
    );
    assert!(
        !normalized.contains("**18.4 authors its own variant for that**"),
        "the emphasised claim must be gone, not merely re-worded"
    );

    let transparency =
        std::fs::read_to_string("src/domain/services/transparency.rs").expect("read");
    let normalized = normalized_docs(&transparency);
    assert!(
        normalized.contains("[`Self::TransportAdmission`]"),
        "it must point at the kind that now exists"
    );
    assert!(
        normalized.contains("shipped the transport substrate and authored no variant"),
        "the amendment must record why the prior sentence was false"
    );
}

/// ⛔ No tier, in any sense, anywhere in this story's surface (ruling A1).
#[test]
fn the_surface_names_no_tier() {
    for relative in [
        "src/domain/models/peer_ticket.rs",
        "src/domain/services/peer_admission.rs",
        "src/adapters/cli/peer/mod.rs",
        "src/adapters/cli/peer/rows.rs",
        "src/adapters/cli/peer/invite.rs",
        "src/adapters/cli/peer/add.rs",
        "src/adapters/cli/peer/list.rs",
        "src/adapters/cli/peer/show.rs",
        "src/adapters/cli/peer/revoke.rs",
        "src/adapters/tui/handlers/peer_command.rs",
        "src/adapters/tui/widgets/peer_add_prompt.rs",
        "src/infrastructure/runtime/peer_bridge.rs",
    ] {
        let source = std::fs::read_to_string(relative).expect(relative);
        // Code only: a comment may explain WHY the shipped `TrustTier` spelling
        // is banned from copy without itself being a tier surface.
        let lowered = code_only(&source).to_ascii_lowercase();
        for forbidden in ["enterprise", "trusttier", "free tier", "paid"] {
            assert!(
                !lowered.contains(forbidden),
                "{relative} names {forbidden}, but no tier mechanism exists in this tree"
            );
        }
    }
}
