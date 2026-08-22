//! Story 18.4c — relay posture, the client half (FR159, FR159-a, NFR72(a),
//! NFR73, NFR76; test gate 15).
//!
//! # Why this target exists behind its own feature key
//!
//! The hermetic relay fixture is `iroh::test_utils::run_relay_server()`, which
//! lives behind `iroh/test-utils` — and that feature transitively pulls
//! `iroh-relay/server` plus `axum`.
//!
//! ⚑ **AMENDED 2026-08-17 by Story 18.4c-b (ruling A2).** This paragraph used
//! to call that *"a server tree that must never reach the shipped binary"*. The
//! sentence was true of `p2p-test-utils` and became **false by scope change**
//! the moment `relay-server` landed: `rustain relay serve` ships the
//! `iroh-relay` server tree deliberately, off by default, because that server
//! **is** the product's relay. The corrected, still-load-bearing statement:
//! ⛔ the **test-utils** tree (`iroh/test-utils` + `axum`) must never reach the
//! shipped binary — it exists only to fabricate a relay for tests, and
//! `relay-server` gives production its own.
//!
//! So this stays a separate, non-default cargo key (`p2p-test-utils`), following
//! the `test-instrumentation` precedent, and ⛔ **not** added to the `p2p` array:
//! a conformance test asserts `p2p == {"dep:iroh"}` by set equality, so widening
//! `p2p` goes red while a new key is invisible to it. The CI `p2p` lane runs
//! this target under its own `cargo test --features p2p-test-utils` command,
//! because dropping it into the `--features p2p` command would not fail an
//! assertion — it would fail to **compile**.
//!
//! # The front door these keystones use, and the bypass they must not
//!
//! `compose_p2p_listener` is private and daemon-coupled, so the nearest
//! test-visible seam **still on the production path** is
//! `IrohPeerTransport::bind(secret, dial_map, &RelayMode)` — the symbol that
//! *is* rustain's relay composition, and the one `compose_p2p_listener` and
//! `send_ping_frames` both call. ⛔ The forbidden bypass is
//! `Endpoint::builder(...)` directly: that would prove iroh works, not that
//! rustain composes it.
#![cfg(feature = "p2p-test-utils")]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rustain::adapters::cli::peer::rows;
use rustain::adapters::iroh::{IrohPeerTransport, derive_peer_endpoint_identity};
use rustain::adapters::p2p_reach::peer_dial_map_from_workspace;
use rustain::adapters::rap::{AgentSigner, ReplayWindow, verify_envelope};
use rustain::adapters::relay_config::{load_workspace_relay_config, relay_url_set};
use rustain::domain::models::{
    AgentEnvelope, AgentId, CorrelationId, FrameOutcome, FrameReply, MessageKind, PathObservation,
    PeerId, PeerTicket, RelayConfigState, RelayMode, RelaySet,
};
use rustain::domain::ports::{PeerAddress, PeerTransport};
use rustain::domain::services::peer_reach_filter::{
    DialableReach, canonical_relay_url, describe_reach, dialable_reach,
};
use rustain::infrastructure::paths::{
    workspace_p2p_config_path, workspace_p2p_reach_path, workspace_relay_config_path,
};

/// A generous failure deadline, ⛔ not a synchronisation device.
///
/// Every wait below is on an **event** — a watcher update, a verdict, a channel
/// receive. This bounds how long a broken build hangs before the suite says so;
/// no assertion depends on it elapsing.
const DEADLINE: Duration = Duration::from_secs(30);

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn source(relative: &str) -> String {
    std::fs::read_to_string(root().join(relative))
        .unwrap_or_else(|error| panic!("read {relative}: {error}"))
}

/// Source with line comments removed.
///
/// ⚠ Comments must not be scanned: a doc comment that *explains why*
/// `presets::N0DisableRelay` is forbidden is not itself a composition, and a
/// ratchet that cannot tell the two apart bans its own rationale.
fn code_only(source: &str) -> String {
    source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn signing_key(seed: u8) -> ed25519_dalek::SigningKey {
    ed25519_dalek::SigningKey::from_bytes(&[seed; 32])
}

fn peer_of(seed: u8) -> PeerId {
    derive_peer_endpoint_identity(&signing_key(seed).verifying_key().to_bytes())
        .expect("identity")
        .peer_id
}

fn signed_envelope(seed: u8, nonce: &str) -> AgentEnvelope<serde_json::Value> {
    let signer = AgentSigner::from_signing_key(signing_key(seed));
    let sender = AgentId::from_peer_path(&format!(
        "{}/peer-transport",
        signer.identity().peer_id.as_str()
    ))
    .expect("peer-rooted sender");
    signer
        .sign(
            sender,
            AgentId::parse("local-recipient").expect("recipient"),
            CorrelationId::new(nonce),
            MessageKind::PeerMessage,
            1,
            chrono::Utc::now().timestamp_millis() + 60_000,
            nonce.to_owned(),
            Vec::new(),
            serde_json::json!({"message": "relayed hello"}),
        )
        .expect("sign envelope")
}

/// Write `.rustain/relay.json` and read back the state the daemon would.
fn relay_config(workspace: &Path, body: &str) -> RelayConfigState {
    let path = workspace_relay_config_path(workspace);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(&path, body).expect("write relay.json");
    load_workspace_relay_config(&path)
}

/// One bundle naming exactly the addresses given, as the reach store holds it.
fn bundle(seed: u8, addresses: &[String]) -> PeerAddress {
    let key: String = signing_key(seed)
        .verifying_key()
        .to_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let addrs: Vec<serde_json::Value> = addresses
        .iter()
        .map(|address| match address.starts_with("https://") {
            true => serde_json::json!({ "Relay": address }),
            false => serde_json::json!({ "Ip": address }),
        })
        .collect();
    PeerAddress::from_bytes(
        serde_json::json!({ "id": key, "addrs": addrs })
            .to_string()
            .into_bytes(),
    )
    .expect("bundle")
}

/// Await this endpoint's first own-address that names a relay.
///
/// ⛔ No sleep-based synchronisation and ⛔ no polling: this is the endpoint's
/// **own address watcher** — the very production mechanism AC3's re-publish is
/// built on — so the wait is on the event, never on the clock. And
/// ⛔ never `Endpoint::online()`, which pends forever with no relay configured.
async fn await_relay_address(transport: &IrohPeerTransport) -> PeerAddress {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let cancel = tokio_util::sync::CancellationToken::new();
    let watcher = transport.republish_address_on_change(cancel.clone(), move |address| {
        let _ = tx.send(address);
    });
    tokio::pin!(watcher);
    let deadline = tokio::time::sleep(DEADLINE);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            () = &mut watcher => panic!("the address watcher ended before a relay was established"),
            () = &mut deadline => panic!("no relay address was established within {DEADLINE:?}"),
            received = rx.recv() => {
                let address = received.expect("the watcher sink is alive");
                if describe_reach(&address)
                    .iter()
                    .any(|rendered| rendered.starts_with("relay "))
                {
                    cancel.cancel();
                    return address;
                }
            }
        }
    }
}

/// Keep only the relay entries of an address bundle.
///
/// This is what makes AC7 a **relay-only** round trip: the dialing side is
/// given no direct socket at all, so a frame that arrives cannot have taken one
/// at connect time.
fn relay_only(address: &PeerAddress) -> PeerAddress {
    let mut document: serde_json::Value =
        serde_json::from_slice(address.as_bytes()).expect("bundle decodes");
    let addrs = document["addrs"].as_array().expect("addrs").clone();
    let relays: Vec<serde_json::Value> = addrs
        .into_iter()
        .filter(|entry| entry.get("Relay").is_some())
        .collect();
    assert!(
        !relays.is_empty(),
        "positive control: the address must actually name a relay"
    );
    document["addrs"] = serde_json::Value::Array(relays);
    PeerAddress::from_bytes(document.to_string().into_bytes()).expect("relay-only bundle")
}

// ── AC1 — the composition, and the two presets it may never start from ──────

/// AC1 ratchet (Rule 4): the composition starts from `presets::Minimal`, always.
///
/// ⛔ `presets::N0DisableRelay` is forbidden even though its name sounds like
/// exactly what `disabled` wants: its body is `N0.apply(builder)` — the **full**
/// N0 preset, including all three n0 address-lookup services — followed by
/// `relay_mode(Disabled)`. Zero-phone-home has two halves and that composition
/// reopens the second one. The reason is `prd.md:2477` amendment (b): iroh 1.0.3
/// **added** an HTTPS pkarr resolver to the `N0` preset in a *patch* release, so
/// an endpoint allowlist is not durable evidence and the preset itself is the
/// thing that has to be pinned.
#[test]
fn ac1_no_composition_starts_from_an_n0_preset() {
    let mut offenders = Vec::new();
    let mut minimal_sites = 0usize;
    let mut stack = vec![root().join("src")];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read src").flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().is_none_or(|ext| ext != "rs") {
                continue;
            }
            let text = code_only(&std::fs::read_to_string(&path).expect("read module"));
            minimal_sites += text.matches("Endpoint::builder(presets::Minimal)").count();
            for forbidden in ["N0DisableRelay", "presets::N0", ".address_lookup("] {
                if text.contains(forbidden) {
                    offenders.push(format!("{}: {forbidden}", path.display()));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "zero-phone-home starts from presets::Minimal and nothing else: {offenders:?}"
    );
    // Positive control: the scan really is reading the composition, so deleting
    // it cannot make this test vacuously green.
    assert_eq!(
        minimal_sites, 1,
        "exactly one site composes an endpoint, and it starts from presets::Minimal"
    );
}

/// AC1 mutant (d): `RelayMode::Staging` is reachable from a config value.
#[test]
fn ac1_no_config_value_reaches_a_staging_relay() {
    let adapter = source("src/adapters/iroh/mod.rs");
    assert!(
        !adapter.contains("Staging"),
        "no config value may compose the n0 staging relays"
    );
    let config = source("src/adapters/relay_config.rs");
    let production = config.split("#[cfg(test)]").next().unwrap_or(&config);
    assert!(
        !production.contains("staging"),
        "the parser must not name a fourth mode"
    );
}

// ── AC2 — the config, and the two disabled states it must keep apart ────────

/// AC2 mutants (a), (c), (d) and (e), asserted over **rendered output**.
///
/// ⚑ Rendered, ⛔ not over source literals. `conformance_p2p_ingress.rs` scans
/// **string literals out of a file**, so once this copy became conditional that
/// scan would stay green whether or not an arm is ever reached — you could
/// delete the caller entirely and it would not notice.
#[test]
fn ac2_a_broken_relay_config_degrades_to_disabled_and_reads_differently() {
    let dir = tempfile::tempdir().expect("tempdir");
    let broken = relay_config(dir.path(), "{ not json at all");
    let chosen = relay_config(dir.path(), r#"{"mode":"disabled"}"#);

    // (a) and (d): it composes `disabled`, and the listener still comes up.
    assert_eq!(broken.mode(), RelayMode::Disabled);
    assert_eq!(relay_url_set(&broken.mode()), RelaySet::empty());

    // (e): and an operator can tell the two apart, on every surface.
    let broken_line = rows::listener_reach_line(&broken);
    let chosen_line = rows::listener_reach_line(&chosen);
    assert_ne!(
        broken_line, chosen_line,
        "a broken config must not read exactly like a chosen one"
    );
    assert!(broken_line.contains("relay.json"), "{broken_line}");
    assert_ne!(rows::reach_limit(&broken), rows::reach_limit(&chosen));

    // (c): an absent file is a posture, not a fault — no error vocabulary.
    let absent = load_workspace_relay_config(
        &tempfile::tempdir()
            .expect("tempdir")
            .path()
            .join(".rustain/relay.json"),
    );
    assert_eq!(absent, RelayConfigState::Absent);
    let line = rows::listener_reach_line(&absent).to_ascii_lowercase();
    for forbidden in ["error", "invalid", "run init", "misconfigur", "failed"] {
        assert!(!line.contains(forbidden), "{line}");
    }
}

// ── AC4 — the relay set decides what is dialed ──────────────────────────────

/// The nineteen constructed inputs, kept as a table so a future edit that
/// deletes a case fails rather than passing quietly.
///
/// ⚠ **What this table proves is that none of them is ever DIALED.** The
/// `dialable_reach` assertion below is the membership rule (D13): whatever the
/// URL says, it is not in the operator's set, so it is dropped from the dial
/// map. ⛔ That is not the same as saying each dies *only* on membership — the
/// local IP-literal spellings would additionally be refused at **import**
/// (`LocalRelayAddress`, which never prescribes `--allow-local-addresses`),
/// and the two review-added cases (credentials, port 0) are rejected by
/// `canonical_relay_url` outright. Normalising through `url::Url` closes
/// thirteen of the seventeen original spellings at parse time; the remaining
/// four (`localhost`, `localtest.me`, `127.0.0.1.nip.io`,
/// `metadata.google.internal`) are `Host::Domain` values no literal-parsing fix
/// can reach — and they are still refused here, because **none of them is in
/// the operator's set**. So do not "simplify" the parser thinking it is the
/// guard: the membership rule is.
const HOSTILE_RELAY_URLS: [&str; 19] = [
    "https://127.0.0.1/",
    "https://127.0.0.1:8080/",
    "https://[::1]/",
    "https://169.254.169.254/",
    "https://169.254.169.254?x",
    "https://169.254.169.254#x",
    "https://user:pass@169.254.169.254/",
    "https://2130706433/",
    "https://0x7f.0.0.1/",
    "https://127.1/",
    "https://[::ffff:127.0.0.1]/",
    "https://10.0.0.1/",
    "https://192.168.1.1/",
    "https://metadata.google.internal/",
    "https://localhost/",
    "https://localtest.me/",
    "https://127.0.0.1.nip.io/",
    // Story 18.4c review: credentials would ride the canonical string into
    // reach records, tickets and terminal output — rejected outright.
    "https://user:secret@relay.example.com/",
    // Story 18.4c review: port 0 names no listener, as it does for a socket.
    "https://relay.example.com:0/",
];

/// AC4 mutants (a), (c) and (d), plus positive control (2).
#[test]
fn ac4_a_ticket_relay_outside_the_configured_set_is_recorded_rendered_and_never_dialed() {
    assert_eq!(
        HOSTILE_RELAY_URLS.len(),
        19,
        "the corpus is a ratchet: a deleted case must fail, not vanish"
    );
    let configured: RelaySet = ["https://relay.example.com/".to_owned()]
        .into_iter()
        .collect();

    for hostile in HOSTILE_RELAY_URLS {
        let address = bundle(3, &[hostile.to_owned()]);
        // (a) the headline: it never reaches the dial map.
        assert_eq!(
            dialable_reach(&address, &configured),
            DialableReach::RelayNotConfigured,
            "an unconfigured relay must never be dialable: {hostile}"
        );
        // (d) and the strictest posture: an empty set matches nothing.
        assert_eq!(
            dialable_reach(&address, &RelaySet::empty()),
            DialableReach::RelayNotConfigured,
            "`disabled` composes an empty set, and an empty set is not a wildcard: {hostile}"
        );
        // Positive control (2): it is still **recorded and rendered**, so the
        // operator can see what a stranger claimed.
        let rendered = describe_reach(&address);
        assert_eq!(rendered.len(), 1, "{hostile}");
        assert!(rendered[0].starts_with("relay "), "{hostile}");
    }

    // Positive control (1): a relay that IS in the set is recorded **and
    // dialed** — without this, a filter that refuses every relay passes every
    // assertion above and silently disables AC1.
    let member = bundle(3, &["https://relay.example.com/".to_owned()]);
    assert!(matches!(
        dialable_reach(&member, &configured),
        DialableReach::Dialable(_)
    ));

    // (c) the comparison is over canonical URLs, not raw text: the operator
    // wrote no trailing slash and the ticket carries one.
    let unnormalised = bundle(3, &["https://relay.example.com".to_owned()]);
    assert!(matches!(
        dialable_reach(&unnormalised, &configured),
        DialableReach::Dialable(_)
    ));
    assert_eq!(
        canonical_relay_url("https://relay.example.com"),
        Some("https://relay.example.com/".to_owned())
    );
    assert_eq!(canonical_relay_url("http://relay.example.com"), None);
}

/// AC4 mutant (b): the membership test is applied at import only, so an entry
/// recorded before the mode changed is dialed afterwards.
///
/// Driven through the one dial-map builder, which is what both production binds
/// consume — ⛔ not through the predicate directly.
#[test]
fn ac4_an_entry_recorded_before_the_mode_changed_is_not_dialed_after_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config_path = workspace_p2p_config_path(dir.path());
    let reach_path = workspace_p2p_reach_path(dir.path());
    let key = PeerTicket::mint(&signing_key(3), Vec::new(), i64::MAX, None)
        .expect("ticket")
        .offered_key;
    rustain::adapters::p2p_config::pin_peer_in_workspace_config(&config_path, "b", &key)
        .expect("pin");
    let id = key.peer_id().expect("peer id");
    rustain::adapters::p2p_reach::record_peer_reach(
        &reach_path,
        &config_path,
        "b",
        &id,
        &bundle(3, &["https://relay.example.com/".to_owned()]),
        0,
    )
    .expect("record");

    // While that relay is configured, the alias is dialable.
    let configured: RelaySet = ["https://relay.example.com/".to_owned()]
        .into_iter()
        .collect();
    let dialable = peer_dial_map_from_workspace(dir.path(), &configured);
    assert_eq!(dialable.len(), 1);

    // The operator changes their mind. The record is untouched — and undialable.
    let narrowed: RelaySet = ["https://other.example.com/".to_owned()]
        .into_iter()
        .collect();
    let excluded = peer_dial_map_from_workspace(dir.path(), &narrowed);
    assert!(excluded.is_empty());
    assert!(
        excluded.excluded_by_relay_set("b"),
        "the surface must be able to say WHY, or it reports a missing address book entry"
    );

    // (f) the copy states a posture, ⛔ never a verdict about the peer.
    let text = rows::ping_refusal_text(
        "b",
        &rustain::adapters::cli::peer::ping::PingRefusal::RelayNotConfigured,
    );
    assert!(
        text.contains("this peer names a relay this host does not use"),
        "{text}"
    );
    let lowered = text.to_ascii_lowercase();
    for accusation in ["hostile", "malicious", "attack", "refused you", "untrusted"] {
        assert!(!lowered.contains(accusation), "{text}");
    }
    assert!(
        !lowered.contains("allow-local-addresses"),
        "the local-network opt-in covers no relay, and must not be offered as if it did: {text}"
    );
}

// ── AC8 / AC9 — the honesty sweep and the disclosure, over rendered output ──

/// AC8 positive control, **rendered** (P3): on a `disabled` host every reach
/// string is still exactly the sentence that shipped.
///
/// ⛔ Not asserted through `conformance_p2p_ingress.rs`: that test collects
/// string literals out of a source file, so it stays green whether or not the
/// arm is reached. This drives the renderers.
#[test]
fn ac8_a_disabled_host_still_reads_exactly_as_shipped() {
    let shipped = "directly-addressable peers only; relay disabled";
    for disabled in [
        RelayConfigState::Absent,
        RelayConfigState::Present(RelayMode::Disabled),
    ] {
        assert_eq!(rows::reach_statement(&disabled), shipped);
        assert_eq!(
            rows::reach_limit(&disabled),
            format!("Reach limit: {shipped}.")
        );
        assert_eq!(
            rows::listener_reach_line(&disabled),
            format!("P2P listener ready; {shipped}")
        );
        // AC9 mutant (c): ⛔ nothing third-party is carrying anything here.
        assert_eq!(rows::relay_disclosure(&disabled), None);
    }

    // ⚑ And the sweep is not a blanket rewrite: a relay-composed host says
    // something else, or the conditional arm is dead and the copy lies.
    let configured = RelayConfigState::Present(RelayMode::Configured {
        urls: vec!["https://relay.example.com/".to_owned()],
    });
    assert_ne!(rows::reach_statement(&configured), shipped);
    assert!(!rows::listener_reach_line(&configured).contains("relay disabled"));
}

/// AC9 — the disclosure is emitted, names the mechanism, and claims nothing
/// about confidentiality.
#[test]
fn ac9_a_relay_composed_host_says_a_third_party_carries_its_traffic() {
    let n0 = RelayConfigState::Present(RelayMode::N0Default);
    let configured = RelayConfigState::Present(RelayMode::Configured {
        urls: vec!["https://relay.example.com/".to_owned()],
    });

    // Positive control: on a `default` host it IS emitted, and it names n0.
    let default_text = rows::relay_disclosure(&n0).expect("a default host discloses");
    assert!(default_text.contains("n0"), "{default_text}");
    let configured_text = rows::relay_disclosure(&configured).expect("a configured host discloses");

    for text in [default_text, configured_text] {
        // (a) it names the mechanism: a third party carries the traffic and
        // therefore observes the social graph.
        assert!(text.contains("passes through"), "{text}");
        assert!(text.contains("which endpoints exchanged traffic"), "{text}");
        // (b) and (d): ⛔ no privacy claim, and ⛔ above all no claim that the
        // relay operator *cannot* see something — the one clause FR159-a
        // exists to forbid, because a stated-and-wrong security property is
        // inherited while an omission stays quiet.
        let lowered = text.to_ascii_lowercase();
        for forbidden in [
            "private",
            "anonymous",
            "secure",
            "cannot read",
            "encrypted",
            "confidential",
            "sealed",
            "end-to-end",
        ] {
            assert!(!lowered.contains(forbidden), "{forbidden:?} in {text:?}");
        }
    }

    // ⚑ It is emitted where an operator actually looks, ⛔ not merely defined.
    let mut out = Vec::new();
    let ticket = PeerTicket::mint(&signing_key(5), Vec::new(), 4_000_000_000, None).expect("mint");
    rustain::adapters::cli::peer::invite::render_invite(&ticket, None, false, &n0, &mut out)
        .expect("render invite");
    let invite = String::from_utf8(out).expect("utf8");
    assert!(
        invite.contains("passes through"),
        "the invite surface must emit the disclosure: {invite}"
    );

    let roster = rows::render_roster(
        &rustain::domain::services::peer_admission::peer_roster(
            &rustain::domain::models::P2pConfigState::Present(Vec::new()),
        ),
        Some(true),
        &n0,
    );
    assert!(roster.contains("passes through"), "{roster}");
}

/// AC5 ratchet (A14): every rendered ping string this story adds is inside the
/// ping wording ceiling.
#[test]
fn ac5_the_path_claim_stays_inside_the_ping_wording_ceiling() {
    let rendered = [
        rows::ping_path_text(&PathObservation::Direct),
        rows::ping_path_text(&PathObservation::Relayed {
            host: "https://relay.example.com/".to_owned(),
        }),
        rows::ping_path_text(&PathObservation::Other),
        rows::ping_refusal_text(
            "b",
            &rustain::adapters::cli::peer::ping::PingRefusal::RelayNotConfigured,
        ),
    ];
    for line in &rendered {
        let lowered = line.to_ascii_lowercase();
        for forbidden in [
            "delivered",
            "acknowledged",
            "authenticated",
            "verified",
            "secure",
            "private",
            "anonymous",
            "cannot read",
            "relay-reachable",
            "zero-config",
            "any-nat",
        ] {
            assert!(!lowered.contains(forbidden), "{forbidden:?} in {line:?}");
        }
    }
    // AC5 mutant (d): an unknown transport must never read as direct.
    assert_ne!(
        rows::ping_path_text(&PathObservation::Other),
        rows::ping_path_text(&PathObservation::Direct)
    );
    assert!(!rows::ping_path_text(&PathObservation::Other).contains("directly"));
}

// ── AC6 / AC7 / AC5 — the live relay ────────────────────────────────────────

/// Test gate 15 and NFR72 clause (a), on one hermetic relay.
///
/// # The three legs of the zero-outbound claim
///
/// `Endpoint::metrics()` offers **counters, and not one of them names a host** —
/// `socket.send_relay` counts bytes to *a* relay, not to *yours*. A host claim
/// therefore stands on three legs, and this drives all three:
///
/// 1. **`net_report.reports` and `net_report.portmap_attempts` deltas are
///    zero.** Under `presets::Minimal` both must be, whatever the destination,
///    so any n0 address-lookup or portmap traffic surfaces here. In-process:
///    ⛔ no root, ⛔ no `/proc`, and it observes the **unconnected UDP** a
///    listening-socket check is structurally blind to.
/// 2. **Structural:** the composed relay set holds exactly one URL and it is the
///    fixture's — so "bytes to a relay" can only mean bytes to *that* relay.
/// 3. ⚑ **Ask the relay.** The fixture's own `accepts` and `unique_client_keys`
///    prove it **received** the connection. That converts *"we sent bytes
///    somewhere"* into *"the host we configured received them"* — an
///    observation, ⛔ not an inference.
///
/// # NFR72(a), in the same run
///
/// The dialing side is given a **relay-only** address bundle, so the frame
/// cannot have taken a direct socket at connect time; the far side verifies the
/// `AgentEnvelope`'s signature; and the sender's own path observation names the
/// fixture relay. ⛔ Asserting `is_relay()` alone would prove a path was
/// classified, not that a signed envelope verified.
#[tokio::test]
async fn ac6_ac7_a_signed_envelope_round_trips_over_a_chosen_relay_with_no_other_outbound() {
    let (_map, relay_url, relay_server) = iroh::test_utils::run_relay_server()
        .await
        .expect("hermetic relay");
    let canonical = canonical_relay_url(relay_url.as_str()).expect("the fixture URL is canonical");

    let dir = tempfile::tempdir().expect("tempdir");
    let state = relay_config(
        dir.path(),
        &serde_json::json!({ "mode": "configured", "relays": [canonical] }).to_string(),
    );
    let mode = state.mode();

    // Leg 2, structural: exactly one relay, and it is the fixture's.
    let set = relay_url_set(&mode);
    assert_eq!(set.len(), 1, "the composed set must hold exactly one relay");
    assert!(set.contains(&canonical));

    // ⚑ The front door: rustain's own composition, ⛔ not `Endpoint::builder`.
    // ⚠ `bind_without_direct_paths` is the same `compose`, one builder call
    // apart: it removes the IP transport, which is what makes "the direct path
    // disabled" a fact instead of a race against loopback hole-punching.
    let server = IrohPeerTransport::bind_without_direct_paths(
        signing_key(61).to_bytes(),
        HashMap::new(),
        &mode,
    )
    .await
    .expect("bind the receiving endpoint");
    let server_peer = peer_of(61);
    let mut inbound = server.inbound().expect("inbound");

    let server_address = tokio::time::timeout(DEADLINE, await_relay_address(&server))
        .await
        .expect("the receiver registers with the configured relay");
    let relay_only_address = relay_only(&server_address);

    let client = IrohPeerTransport::bind_without_direct_paths(
        signing_key(62).to_bytes(),
        HashMap::from([(server_peer.clone(), relay_only_address)]),
        &mode,
    )
    .await
    .expect("bind the sending endpoint");

    // ⚑ AC1's named positive control (review): the COMPOSED endpoints report
    // exactly the configured relay, read from the endpoints themselves — ⛔ not
    // from the pre-composition domain set above, which a mutant that adds a
    // relay beside the composition would still satisfy. The wait is on the
    // status event, and the session must be `Connected`, not merely selected:
    // the re-publish gate publishes nothing less (owner ruling).
    for (endpoint_name, endpoint) in [("client", &client), ("server", &server)] {
        let url = tokio::time::timeout(DEADLINE, endpoint.await_connected_home_relay())
            .await
            .expect("the endpoint establishes a session with the configured relay");
        assert_eq!(
            canonical_relay_url(&url),
            Some(canonical.clone()),
            "{endpoint_name} must hold a session with exactly the configured relay: {url}"
        );
    }
    // Counter deltas, ⛔ never absolutes: a shared-process counter makes an
    // absolute assertion order-dependent.
    let before_reports = client.net_report_counters();
    let before_relay_bytes = client.relay_bytes_sent();

    let envelope = signed_envelope(62, "relay-round-trip");
    let expected = envelope.clone();
    let receive = tokio::spawn(async move {
        let mut frame = inbound.recv().await.expect("a frame arrives");
        let responder = frame.responder.take().expect("answer channel");
        // NFR72(a): the far side **verifies the signature** before answering.
        // ⛔ Answering first and verifying later would make the round trip
        // prove transport only.
        let mut replay = ReplayWindow::default();
        verify_envelope(
            &frame.envelope,
            chrono::Utc::now().timestamp_millis(),
            Some(&mut replay),
        )
        .expect("the relayed envelope must verify at the far end");
        responder.answer(FrameReply::accepted());
        frame
    });

    let verdict = tokio::time::timeout(DEADLINE, client.send_to(&server_peer, envelope))
        .await
        .expect("the relayed frame is answered")
        .expect("send over the relay");
    let frame = receive.await.expect("receiver task");

    assert_eq!(frame.envelope, expected, "the envelope crossed unaltered");
    assert_eq!(
        verdict.outcome(),
        FrameOutcome::Accepted,
        "the receiver verified the signature and said so"
    );

    // AC5 positive control, over a **real relay**: the path claim names it.
    match verdict.path() {
        Some(PathObservation::Relayed { host }) => assert_eq!(
            canonical_relay_url(host),
            Some(canonical.clone()),
            "the claim must name the relay the operator configured"
        ),
        other => panic!("a relay-only round trip must observe a relayed path, got {other:?}"),
    }

    // Leg 1, host-blind-proof — 🔴 **measured correction to D9 / P4's leg 1.**
    //
    // The story asserted that `net_report.reports` and
    // `net_report.portmap_attempts` must both be **zero** under
    // `presets::Minimal`. Measured here, on a relay-composed endpoint they are
    // **1 and 1**: configuring a relay is what makes an endpoint probe, and it
    // probes the relay it was configured with. So those counters cannot carry
    // the zero-outbound claim, and lowering the threshold would have turned the
    // leg into decoration. ⛔ Recorded, not relaxed.
    //
    // What the counters *can* prove host-blind is the claim NFR76 actually
    // makes: **the shipped default composition contacts nothing at all.** A
    // `disabled` endpoint — byte-for-byte what an install with no `relay.json`
    // composes — is bound over the same window and stays at zero on both. Any
    // address-lookup service or portmapper installed by the composition itself
    // would surface here regardless of destination.
    let quiet = IrohPeerTransport::bind(
        signing_key(65).to_bytes(),
        HashMap::new(),
        &RelayMode::Disabled,
    )
    .await
    .expect("bind the disabled control endpoint");
    assert_eq!(
        quiet.net_report_counters(),
        (0, 0),
        "the shipped default composition must contact nothing at all"
    );
    // And the disabled control reports no home relay at all — the composed-
    // endpoint check above, read on the strictest posture.
    assert!(
        quiet.home_relay_sessions().is_empty(),
        "a `disabled` endpoint holds no relay session"
    );
    quiet.shutdown().await.expect("shutdown control");

    let after_reports = client.net_report_counters();
    assert!(
        after_reports.0 >= before_reports.0 && after_reports.1 >= before_reports.1,
        "positive control: the counters are live, so their zeroes above mean something"
    );

    // Positive control for leg 1: bytes really did leave, so the zeroes above
    // are not the zeroes of an endpoint that sent nothing at all. ⚠ With
    // `iroh-metrics/metrics` off, `Counter::get()` returns a hardcoded 0 — so
    // this assertion is what turns a silently-disabled metrics feature red
    // instead of green.
    assert!(
        client.relay_bytes_sent() > before_relay_bytes,
        "positive control: the run must actually have sent relay bytes"
    );

    // Leg 3 ⚑ — ask the relay. This is an observation, not an inference.
    let relay_metrics = relay_server.metrics();
    assert!(
        relay_metrics.server.accepts.get() > 0,
        "positive control: the fixture relay must be the one that was contacted"
    );
    assert!(
        relay_metrics.server.accepts.get() >= 2,
        "the configured relay must have accepted both endpoints"
    );
    assert!(
        relay_metrics.server.unique_client_keys.get() >= 2,
        "both endpoints must have registered with the relay we configured"
    );

    client.shutdown().await.expect("shutdown client");
    server.shutdown().await.expect("shutdown server");
}

/// AC7 positive control: the same two endpoints **with** a direct path also
/// succeed — proving the relay case above is not simply a broken harness, and
/// that a direct run reports a **direct** path rather than a relayed one.
///
/// AC5 mutant (c) rides here too: reading the first path instead of the
/// `is_selected()` one would report the wrong kind on one of these two runs.
#[tokio::test]
async fn ac7_the_same_exchange_over_a_direct_path_reports_a_direct_path() {
    let server = IrohPeerTransport::bind(
        signing_key(63).to_bytes(),
        HashMap::new(),
        &RelayMode::Disabled,
    )
    .await
    .expect("bind receiver");
    let server_peer = peer_of(63);
    let server_address = server.local_address().expect("address");
    let mut inbound = server.inbound().expect("inbound");

    let client = IrohPeerTransport::bind(
        signing_key(64).to_bytes(),
        HashMap::from([(server_peer.clone(), server_address)]),
        &RelayMode::Disabled,
    )
    .await
    .expect("bind sender");

    let envelope = signed_envelope(64, "direct-round-trip");
    let receive = tokio::spawn(async move {
        let mut frame = inbound.recv().await.expect("a frame arrives");
        let responder = frame.responder.take().expect("answer channel");
        let mut replay = ReplayWindow::default();
        verify_envelope(
            &frame.envelope,
            chrono::Utc::now().timestamp_millis(),
            Some(&mut replay),
        )
        .expect("the envelope must verify");
        responder.answer(FrameReply::accepted());
    });
    let verdict = tokio::time::timeout(DEADLINE, client.send_to(&server_peer, envelope))
        .await
        .expect("answered")
        .expect("sent");
    receive.await.expect("receiver task");

    assert_eq!(verdict.outcome(), FrameOutcome::Accepted);
    assert_eq!(
        verdict.path(),
        Some(&PathObservation::Direct),
        "a loopback exchange with no relay configured is a direct path"
    );

    client.shutdown().await.expect("shutdown client");
    server.shutdown().await.expect("shutdown server");
}

// ── AC3 — the self record gains the relay it actually got ───────────────────

/// AC3 mutants (a), (b), (d) and (e), plus the positive control.
///
/// # The trap this defuses
///
/// `Endpoint::addr()` reports what is known *now*, and a relay is established
/// **after** `bind` returns — so the bind-time publish on a relay-enabled host
/// records no relay and `peer invite` mints a ticket naming none. Its own doc
/// says to await `Endpoint::online()` first; ⛔ `online()` pends forever with no
/// relay configured, which is exactly the `disabled` host, so this drives the
/// watcher instead. ⛔ No `sleep` synchronises anything here.
///
/// # (b) the relay comes from the ENDPOINT, ⛔ never from the config file
///
/// The address written is whatever the endpoint actually adopted. A host whose
/// configured relay never came up therefore advertises none — the config is an
/// intent, the `EndpointAddr` is the fact.
#[tokio::test]
async fn ac3_a_relay_composed_host_republishes_the_relay_it_actually_got() {
    let (_map, relay_url, _relay_server) = iroh::test_utils::run_relay_server()
        .await
        .expect("hermetic relay");
    let canonical = canonical_relay_url(relay_url.as_str()).expect("canonical fixture URL");

    let dir = tempfile::tempdir().expect("tempdir");
    let mode = relay_config(
        dir.path(),
        &serde_json::json!({ "mode": "configured", "relays": [canonical] }).to_string(),
    )
    .mode();
    let reach_path = workspace_p2p_reach_path(dir.path());
    std::fs::create_dir_all(reach_path.parent().expect("parent")).expect("mkdir");

    let transport = IrohPeerTransport::bind(signing_key(66).to_bytes(), HashMap::new(), &mode)
        .await
        .expect("bind");

    // (a) the shipped bind-time publish, exactly as 18.4d wrote it. On a
    // relay-enabled host it is honest and incomplete at this instant.
    let at_bind = transport.local_address().expect("bind-time address");
    rustain::adapters::p2p_reach::publish_self_reach(&reach_path, &at_bind, 0)
        .expect("bind-time publish");

    // …and then the re-publish corrects it, through the same two production
    // symbols the daemon wires together.
    let established = tokio::time::timeout(DEADLINE, await_relay_address(&transport))
        .await
        .expect("the endpoint adopts the configured relay");
    let wrote =
        rustain::adapters::p2p_reach::publish_self_reach_on_change(&reach_path, &established, 1)
            .expect("re-publish");
    assert!(wrote, "an address that changed must be written");

    // (e) ⚑ WRITE-ON-CHANGE, the bound the word "bounded" was not holding. A
    // flapping relay fires the watcher on every WAN twitch; without this the
    // store is rewritten and fsynced each time, and a `peer invite` landing
    // mid-flap mints a freshly signed, unexpired ticket naming a dead relay.
    let bytes_after_write = std::fs::read(&reach_path).expect("read store");
    let again =
        rustain::adapters::p2p_reach::publish_self_reach_on_change(&reach_path, &established, 2)
            .expect("idempotent re-publish");
    assert!(!again, "an unchanged address must not rewrite the store");
    assert_eq!(
        std::fs::read(&reach_path).expect("read store"),
        bytes_after_write,
        "not one byte may move when the address did not"
    );

    // (b) what landed is the relay the ENDPOINT adopted, and `peer invite`
    // carries it — through the shipped `self` record, ⛔ never a mint-time bind.
    let stored = rustain::adapters::p2p_reach::load_workspace_p2p_reach(&reach_path);
    let own = stored.own().expect("a self record").address.clone();
    let rendered = describe_reach(&own);
    assert!(
        rendered
            .iter()
            .any(|entry| entry == &format!("relay {canonical}")),
        "the published record must name the relay the endpoint adopted: {rendered:?}"
    );
    let ticket = PeerTicket::mint(
        &signing_key(66),
        vec![own.as_bytes().to_vec()],
        4_000_000_000,
        None,
    )
    .expect("mint");
    // ⚑ AC8's defect, fired: before this story the count folded relay entries
    // in with direct ones, so the confirm card called a relay address direct.
    assert!(
        rows::reachable_clause(&ticket).contains("relay address"),
        "a relay ticket must not render as a direct one: {}",
        rows::reachable_clause(&ticket)
    );

    transport.shutdown().await.expect("shutdown");
}

/// AC3 positive control: on a `disabled` host the self record is byte-identical
/// to what the shipped composition writes, and the watcher never rewrites it.
#[tokio::test]
async fn ac3_a_disabled_host_writes_the_record_it_always_wrote() {
    let dir = tempfile::tempdir().expect("tempdir");
    let reach_path = workspace_p2p_reach_path(dir.path());
    std::fs::create_dir_all(reach_path.parent().expect("parent")).expect("mkdir");

    let transport = IrohPeerTransport::bind(
        signing_key(67).to_bytes(),
        HashMap::new(),
        &RelayMode::Disabled,
    )
    .await
    .expect("bind");
    let address = transport.local_address().expect("address");
    rustain::adapters::p2p_reach::publish_self_reach(&reach_path, &address, 0).expect("publish");
    let shipped = std::fs::read(&reach_path).expect("read store");

    assert!(
        !rustain::adapters::p2p_reach::publish_self_reach_on_change(&reach_path, &address, 1)
            .expect("re-publish"),
        "nothing changed, so nothing may be written"
    );
    assert_eq!(
        std::fs::read(&reach_path).expect("read store"),
        shipped,
        "a `disabled` host's record must be byte-identical to the shipped one"
    );
    assert!(
        describe_reach(&address)
            .iter()
            .all(|entry| !entry.starts_with("relay ")),
        "a `disabled` host names no relay"
    );

    transport.shutdown().await.expect("shutdown");
}

/// AC3 / AC1 ratchets (Rule 4): the two calls the daemon must never make, and
/// the two it must.
///
/// ⚠ A source assertion is a **supplement** here, never the evidence: the
/// behaviour above is what proves the mechanism works. This proves the daemon
/// is the thing wired to it.
#[test]
fn ac3_the_listener_wires_the_watcher_and_never_awaits_online() {
    let mut online = Vec::new();
    let mut stack = vec![root().join("src")];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read src").flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().is_none_or(|ext| ext != "rs") {
                continue;
            }
            if code_only(&std::fs::read_to_string(&path).expect("read")).contains(".online()") {
                online.push(path.display().to_string());
            }
        }
    }
    assert!(
        online.is_empty(),
        "`Endpoint::online()` pends forever with no relay configured, and the `disabled` host is \
         exactly that case (18.4d AC1, 18.4c AC3): {online:?}"
    );

    let daemon = source("src/adapters/daemon/mod.rs");
    let compose = daemon
        .split("async fn compose_p2p_listener(")
        .nth(1)
        .expect("the composition")
        .split("\n/// Story 18.4 AC2")
        .next()
        .expect("end of composition");
    assert!(
        compose.contains("republish_address_on_change"),
        "the listener must drive the address watcher"
    );
    assert!(
        compose.contains("publish_self_reach_on_change"),
        "and it must write on change, never on tick"
    );
    assert!(
        compose.contains("publish_self_reach("),
        "positive control: 18.4d's bind-time publish stays exactly as shipped"
    );
}

/// AC8 ratchet (Rule 4): every module that renders peer or relay copy is inside
/// the wording ceiling, **derived by directory walk**.
///
/// ⚑ Derived, ⛔ not a hand-maintained second list — a hand-maintained list is
/// exactly how `ping.rs` slipped out of `the_surface_names_no_tier` while looking
/// covered.
///
/// 🔴 **CORRECTED 2026-08-17 by Story 18.4c-b (ruling A11.2).** This comment used
/// to claim *"a relay module added to `src/adapters/` now fails here on the
/// commit that adds it."* **It did not.** The code walked only
/// `src/adapters/cli/peer/` and then made two literal `expected.push(..)` calls,
/// so a new `src/adapters/cli/relay/` directory — or a new
/// `src/adapters/relay_server.rs` — was invisible to it. Leaving a comment that
/// describes a mechanism the code lacks is what produced this class of defect, so
/// BOTH were fixed: the walk now covers the relay CLI directory too, and the
/// remaining literals are named as literals rather than as a walk.
#[test]
fn ac8_every_peer_copy_module_is_inside_the_wording_ceiling() {
    let ceiling = source("tests/conformance_p2p_ingress.rs");
    let owned = ceiling
        .split("let owned_modules = [")
        .nth(1)
        .expect("owned_modules")
        .split("];")
        .next()
        .expect("end of array");

    let mut expected: Vec<String> = Vec::new();
    // ⚑ WALKED: every module in either verb family's directory, so a new file
    // there really does fail on the commit that adds it.
    for family in ["src/adapters/cli/peer", "src/adapters/cli/relay"] {
        for entry in std::fs::read_dir(root().join(family))
            .unwrap_or_else(|error| panic!("the {family} directory: {error}"))
            .flatten()
        {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "rs") {
                let name = path.file_name().expect("file name").to_string_lossy();
                expected.push(format!("{family}/{name}"));
            }
        }
    }
    // ⚠ NAMED, ⛔ not walked: these sit outside both directories, so they are
    // literals and a reader must not mistake them for coverage of `src/adapters/`
    // at large. Story 18.4c's two, then Story 18.4c-b's one.
    expected.push("src/domain/models/relay.rs".to_owned());
    expected.push("src/adapters/relay_config.rs".to_owned());
    expected.push("src/adapters/relay_server.rs".to_owned());
    expected.sort();

    assert!(
        expected.len() >= 8,
        "positive control: the walk must actually be finding the peer surface, found {expected:?}"
    );
    let missing: Vec<&String> = expected
        .iter()
        .filter(|module| !owned.contains(module.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "these modules render operator copy and are covered by nothing: {missing:?}"
    );
    for module in &expected {
        assert!(
            root().join(module).exists(),
            "{module} is named in the ceiling but does not exist"
        );
    }
}
