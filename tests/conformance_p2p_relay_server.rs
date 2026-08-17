//! Story 18.4c-b — `rustain relay serve`, the relay SERVER half (FR159).
//!
//! # What this target proves that Story 18.4c's did not
//!
//! 18.4c round-tripped a signed envelope over `iroh::test_utils::run_relay_server()`
//! — a relay the **test framework** built. This target runs the same exchange
//! against a relay **rustain's own production composition** built, from
//! operator-shaped PEM files, through the same flags an operator types.
//!
//! ⛔ **Test gate 15 is spent and this target does not claim it**, nor NFR72(a),
//! nor NFR76 — 18.4c discharged all three. Claiming a spent gate is how a story
//! manufactures credit it did not earn. What is new here is the *producer* of the
//! relay, and that is the difference the story exists for.
//!
//! # Front doors, and the bypasses these keystones must not take
//!
//! * **Server:** `ServePlan::resolve` → `relay_server::spawn_relay`, the exact
//!   pair `relay serve` itself calls. ⛔ Forbidden bypass: building an
//!   `iroh_relay::server::ServerConfig` / `RelayConfig` / `TlsConfig` inline —
//!   that would prove `iroh-relay` works, not that rustain composes it. (The one
//!   exception is named and justified at
//!   [`a_relay_that_terminates_on_its_own_exits_non_zero`].)
//! * **Client:** `IrohPeerTransport::bind_without_direct_paths`. ⛔ Forbidden
//!   bypass: `Endpoint::builder(...)`. ⚠ Be precise about what that is: 18.4c
//!   names `IrohPeerTransport::bind` as the front door, and
//!   `bind_without_direct_paths` is its test-gated sibling — one builder call
//!   apart, entering the same `compose`. It is the nearest test-visible seam
//!   still on the production path, ⛔ not the production entry itself.
//!
//! # TLS here is production TLS
//!
//! The certificates are real `rcgen` PEM files fed to the production
//! `--cert`/`--key` path, so the **server side executes zero test-only code**:
//! `server::testing::self_signed_tls_certs_and_config()` is gated by
//! `test-utils`, not `server`, and a production `relay-server` build has no
//! access to it. Only the *client* needs an override —
//! `CaTlsConfig::insecure_skip_verify()`, already wired inside the production
//! `compose` under `#[cfg(all(feature = "p2p-test-utils", debug_assertions))]`.
//! ⇒ this target runs under `p2p-test-utils,relay-server` in a **debug** profile.
//!
//! # ⚠ The metrics dependency this target rests on
//!
//! `Counter::get()` returns a hardcoded `0` when `iroh-metrics/metrics` is off,
//! which would make the ask-the-relay leg silently vacuous. It is safe here only
//! because `iroh-relay`'s `server = ["metrics", …]` re-implies it even under
//! `default-features = false`. ⛔ A later "optimisation" of that feature list
//! breaks this target's central assertion without failing to compile.

#![cfg(all(feature = "relay-server", feature = "p2p-test-utils"))]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rustain::adapters::cli::relay::serve::{ServeArgs, ServePlan};
use rustain::adapters::iroh::{IrohPeerTransport, derive_peer_endpoint_identity};
use rustain::adapters::rap::{AgentSigner, ReplayWindow, verify_envelope};
use rustain::adapters::relay_config::{load_workspace_relay_config, relay_url_set};
use rustain::adapters::relay_server::{RelayExit, serve, spawn_relay};
use rustain::domain::models::{
    AgentEnvelope, AgentId, CorrelationId, FrameOutcome, FrameReply, MessageKind,
    PathObservation, PeerId, RelayConfigState,
};
use rustain::domain::ports::{PeerAddress, PeerTransport};
use rustain::domain::services::peer_reach_filter::{canonical_relay_url, describe_reach};
use rustain::infrastructure::paths::workspace_relay_config_path;

/// A generous failure deadline, ⛔ not a synchronisation device. Every wait below
/// is on an event — a watcher update, a verdict, a channel receive.
const DEADLINE: Duration = Duration::from_secs(30);

// ── fixtures ────────────────────────────────────────────────────────────────

/// Real PEM files, for the **production** `--cert`/`--key` path.
fn self_signed_pems() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().expect("tls dir");
    let cert = rcgen::generate_simple_self_signed(vec![
        "localhost".to_owned(),
        "127.0.0.1".to_owned(),
    ])
    .expect("self-signed certificate");
    let cert_path = dir.path().join("relay.crt");
    let key_path = dir.path().join("relay.key");
    std::fs::write(&cert_path, cert.cert.pem()).expect("write cert");
    std::fs::write(&key_path, cert.signing_key.serialize_pem()).expect("write key");
    (dir, cert_path, key_path)
}

/// The flags an operator would type for a loopback relay on assigned ports.
fn loopback_tls_args(cert: &Path, key: &Path) -> ServeArgs {
    ServeArgs {
        http_addr: Some("127.0.0.1:0".to_owned()),
        https_addr: Some("127.0.0.1:0".to_owned()),
        quic_addr: Some("127.0.0.1:0".to_owned()),
        hostname: Some("127.0.0.1".to_owned()),
        cert: Some(cert.to_path_buf()),
        key: Some(key.to_path_buf()),
        dev: false,
        print_service_unit: false,
    }
}

/// A relay this product built, plus the URL its own client will accept.
///
/// ⚑ The URL is **not** `format!`ed here: the observed listener port is fed back
/// through `ServePlan`, and `ServePlan::shareable_url` is the production
/// `canonical_relay_url` caller. Reading the assigned port back is what a
/// hermetic fixture must do; the operator-facing verb never invents one.
async fn our_relay(cert: &Path, key: &Path) -> (iroh_relay::server::Server, String) {
    let plan = ServePlan::resolve(&loopback_tls_args(cert, key)).expect("a valid plan");
    let server = spawn_relay(&plan).await.expect("our own relay starts");
    let https = server.https_addr().expect("the relay listener is bound");
    let observed = ServePlan::resolve(&ServeArgs {
        https_addr: Some(https.to_string()),
        ..loopback_tls_args(cert, key)
    })
    .expect("a plan naming the bound port");
    let url = observed
        .shareable_url()
        .expect("the bound relay must have a URL its own client would keep");
    (server, url)
}

/// Write `.rustain/relay.json` and read back the state the daemon would.
fn relay_config(workspace: &Path, urls: &[&str]) -> RelayConfigState {
    let path = workspace_relay_config_path(workspace);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &path,
        serde_json::json!({ "mode": "configured", "relays": urls }).to_string(),
    )
    .expect("write relay.json");
    load_workspace_relay_config(&path)
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
            serde_json::json!({"message": "relayed through our own relay"}),
        )
        .expect("sign envelope")
}

/// Await this endpoint's first own-address that names a relay. ⛔ No sleep and no
/// polling: this is the endpoint's own address watcher, so the wait is on the
/// event. ⛔ Never `Endpoint::online()`.
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

/// Keep only the relay entries of an address bundle: the dialing side is given no
/// direct socket at all, so a frame that arrives cannot have taken one.
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

// ── AC2 — three addresses, and the collision rustain refuses itself ──────────

/// AC2 positive control: distinct addresses spawn, and **all three** sockets
/// report bound on the TLS path.
///
/// ⚑ The `quic_addr().is_some()` assertion is the load-bearing one. The mutant is
/// `config.quic = None` on the TLS path: relaying still works and the suite stays
/// green, but every client built from this relay's URL probes UDP 7842 for
/// address discovery and finds it dead — so public-address discovery degrades,
/// hole-punching degrades, and peers that could have gone direct stay relayed
/// with nothing anywhere explaining why.
#[tokio::test]
async fn the_tls_path_binds_two_tcp_sockets_and_one_udp_socket() {
    let (_dir, cert, key) = self_signed_pems();
    let plan = ServePlan::resolve(&loopback_tls_args(&cert, &key)).expect("plan");
    let server = spawn_relay(&plan).await.expect("our relay starts");

    // ⚠ Read every address BEFORE shutdown: it consumes `self`.
    let http = server.http_addr().expect("the captive-portal probe binds");
    let https = server.https_addr().expect("the relay listener binds");
    let quic = server
        .quic_addr()
        .expect("address discovery must be served whenever TLS is configured");

    assert_ne!(
        http, https,
        "the probe runs in plain text and cannot share the listener's port"
    );
    assert!(http.port() != 0 && https.port() != 0 && quic.port() != 0);

    let exit = serve(server, std::future::ready(())).await;
    assert_eq!(exit, RelayExit::Cancelled);
}

/// AC2's `--dev` arm: no TLS, so no listener and no discovery socket, and the
/// plain-text port **is** the relay.
#[tokio::test]
async fn the_dev_path_binds_one_loopback_socket_and_nothing_else() {
    let plan = ServePlan::resolve(&ServeArgs {
        dev: true,
        http_addr: Some("127.0.0.1:0".to_owned()),
        ..ServeArgs::default()
    })
    .expect("plan");
    let server = spawn_relay(&plan).await.expect("a dev relay starts");

    let http = server.http_addr().expect("the relay itself");
    assert!(http.ip().is_loopback(), "⛔ deliberately not [::]:3340");
    assert_eq!(
        server.https_addr(),
        None,
        "no TLS means no https listener at all"
    );
    assert_eq!(
        server.quic_addr(),
        None,
        "address discovery inherits the TLS config, so without one it cannot spawn"
    );

    assert_eq!(serve(server, std::future::ready(())).await, RelayExit::Cancelled);
}

/// AC2 keystone: rustain refuses the collision **before** `Server::spawn`.
///
/// ⚑ Mutant: delete the equality check in `ServePlan::resolve`. `iroh-relay`
/// performs no such validation, binds the plain-HTTP listener *first*, and the
/// relay listener then fails with a bare `AddrInUse` naming the port the operator
/// did not type — which is why this refusal has to be rustain's.
#[tokio::test]
async fn an_equal_fixed_tcp_pair_is_refused_before_a_socket_is_touched() {
    let (_dir, cert, key) = self_signed_pems();
    // A fixed port both flags name. Bind it first so that, if the refusal were
    // deleted, the run would demonstrably reach iroh's own failure instead.
    let squatter = std::net::TcpListener::bind("127.0.0.1:0").expect("a fixed port");
    let taken = squatter.local_addr().expect("addr");
    drop(squatter);

    let args = ServeArgs {
        http_addr: Some(taken.to_string()),
        https_addr: Some(taken.to_string()),
        ..loopback_tls_args(&cert, &key)
    };
    let refusal = ServePlan::resolve(&args).expect_err("rustain must refuse this itself");
    let statement = refusal.statement();
    assert!(statement.contains("--http-addr"), "{statement}");
    assert!(statement.contains("--https-addr"), "{statement}");
    assert!(
        !statement.to_ascii_lowercase().contains("address in use"),
        "the refusal must not be iroh's AddrInUse from the other port: {statement}"
    );
}

// ── AC3 — the printed URL is the client's own judgement ─────────────────────

/// AC3 keystone: the URL `relay serve` prints is one this build's **client**
/// accepts verbatim.
///
/// ⚑ Mutant: print `format!("https://{hostname}")`. A bare host loses the
/// canonical trailing slash `Url::as_str()` appends, and `RelaySet` membership is
/// an exact string match — so a peer pasting the printed value would be dropped
/// before dial by the very check that makes the relay set configuration.
#[tokio::test]
async fn the_printed_url_is_one_this_builds_client_keeps() {
    let (_dir, cert, key) = self_signed_pems();
    let (server, url) = our_relay(&cert, &key).await;

    assert!(url.ends_with('/'), "the canonical form is what is printed: {url}");
    assert_eq!(
        canonical_relay_url(&url),
        Some(url.clone()),
        "idempotence: the judge returns the printed value unchanged"
    );

    // And the client's own set-membership accepts it, through the config file an
    // operator would actually edit.
    let dir = tempfile::tempdir().expect("workspace");
    let state = relay_config(dir.path(), &[url.as_str()]);
    let set = relay_url_set(&state.mode());
    assert_eq!(set.len(), 1);
    assert!(
        set.contains(&url),
        "the printed URL must survive the peer-side membership check verbatim"
    );

    assert_eq!(serve(server, std::future::ready(())).await, RelayExit::Cancelled);
}

// ── AC4 — the round trip through a relay this product built ─────────────────

/// The story's reason to exist: a signed envelope crosses **rustain's own
/// relay**, relay-only, and the relay's own counters say it carried it.
///
/// # Three legs, the third of which is an observation rather than an inference
///
/// 1. **Structural:** the composed relay set holds exactly one URL and it is
///    ours, so "bytes to a relay" can only mean bytes to *this* relay.
/// 2. **Relay-only, structurally:** both endpoints enter `compose` with
///    `clear_ip_transports()` (via `bind_without_direct_paths`) and the dialer is
///    handed a relay-only bundle. ⛔ `AddrFilter::relay_only()` is the wrong tool
///    and would be vacuously green — it governs which addresses get *published*
///    to pkarr/DNS and does not prevent a direct path forming. On one host,
///    loopback hole-punching wins inside the first round trip, so this has to be
///    structural, ⛔ never a race.
/// 3. ⚑ **Ask our relay.** `Server::metrics()` on the server *this product
///    composed*. 18.4c asked the fixture; asking ours is the whole difference.
#[tokio::test]
async fn a_signed_envelope_round_trips_through_our_own_relay_and_it_says_so() {
    let (_dir, cert, key) = self_signed_pems();
    let (relay, url) = our_relay(&cert, &key).await;

    let workspace = tempfile::tempdir().expect("workspace");
    let mode = relay_config(workspace.path(), &[url.as_str()]).mode();

    // Leg 1, structural.
    let set = relay_url_set(&mode);
    assert_eq!(set.len(), 1, "exactly one relay, and it is ours");
    assert!(set.contains(&url));

    let receiver = IrohPeerTransport::bind_without_direct_paths(
        signing_key(71).to_bytes(),
        HashMap::new(),
        &mode,
    )
    .await
    .expect("bind the receiving endpoint");
    let receiver_peer = peer_of(71);
    let mut inbound = receiver.inbound().expect("inbound");

    let receiver_address = tokio::time::timeout(DEADLINE, await_relay_address(&receiver))
        .await
        .expect("the receiver registers with our relay");

    let sender = IrohPeerTransport::bind_without_direct_paths(
        signing_key(72).to_bytes(),
        HashMap::from([(receiver_peer.clone(), relay_only(&receiver_address))]),
        &mode,
    )
    .await
    .expect("bind the sending endpoint");

    // Read from the ENDPOINTS, ⛔ not from the pre-composition domain set: a
    // mutant adding a relay beside the configured one still satisfies the latter.
    for (name, endpoint) in [("sender", &sender), ("receiver", &receiver)] {
        let held = tokio::time::timeout(DEADLINE, endpoint.await_connected_home_relay())
            .await
            .expect("the endpoint establishes a session with our relay");
        assert_eq!(
            canonical_relay_url(&held),
            Some(url.clone()),
            "{name} must hold a session with exactly the relay we started: {held}"
        );
    }

    let before_relay_bytes = sender.relay_bytes_sent();

    let envelope = signed_envelope(72, "our-own-relay-round-trip");
    let expected = envelope.clone();
    let receive = tokio::spawn(async move {
        let mut frame = inbound.recv().await.expect("a frame arrives");
        let responder = frame.responder.take().expect("answer channel");
        // The far side checks the signature BEFORE answering. ⛔ Answering first
        // would make the round trip prove transport only.
        //
        // ⚑ POSITIVE CONTROL, and it is what stops this line being decoration:
        // the receiver IS the test here, so deleting the check below cannot turn
        // anything red on its own. So the check is first shown to FIRE — the very
        // envelope that crossed, with its body altered, must be rejected — and
        // only then applied to the real one. A `verify_envelope` that accepted
        // everything would fail here, before it ever got the chance to accept.
        let mut tampered = frame.envelope.clone();
        tampered.body = serde_json::json!({"message": "altered after signing"});
        assert!(
            verify_envelope(&tampered, chrono::Utc::now().timestamp_millis(), None).is_err(),
            "the check must be able to reject: an altered body breaks the content hash"
        );
        let mut replay = ReplayWindow::default();
        verify_envelope(
            &frame.envelope,
            chrono::Utc::now().timestamp_millis(),
            Some(&mut replay),
        )
        .expect("the relayed envelope must check out at the far end");
        responder.answer(FrameReply::accepted());
        frame
    });

    let verdict = tokio::time::timeout(DEADLINE, sender.send_to(&receiver_peer, envelope))
        .await
        .expect("the relayed frame is answered")
        .expect("send over our relay");
    let frame = receive.await.expect("receiver task");

    assert_eq!(frame.envelope, expected, "the envelope crossed unaltered");
    assert_eq!(verdict.outcome(), FrameOutcome::Accepted);
    match verdict.path() {
        Some(PathObservation::Relayed { host }) => assert_eq!(
            canonical_relay_url(host),
            Some(url.clone()),
            "the path claim must name the relay this host is running"
        ),
        other => panic!("a relay-only round trip must observe a relayed path, got {other:?}"),
    }

    // Positive control for the metrics feature itself: with
    // `iroh-metrics/metrics` off every counter reads a hardcoded 0, so this is
    // what turns a silently-disabled feature RED instead of green.
    assert!(
        sender.relay_bytes_sent() > before_relay_bytes,
        "positive control: the run must actually have sent relay bytes"
    );

    // ⚑ Leg 3 — ask OUR relay. Read before shutdown.
    let metrics = relay.metrics();
    assert!(
        metrics.server.accepts.get() > 0,
        "positive control: our own server must be the one that was contacted, \
         ⛔ not a fixture silently substituted for it"
    );
    assert!(
        metrics.server.accepts.get() >= 2,
        "our relay must have accepted both endpoints"
    );
    assert!(
        metrics.server.unique_client_keys.get() >= 2,
        "both endpoints must have registered with the relay we started"
    );

    sender.shutdown().await.expect("shutdown sender");
    receiver.shutdown().await.expect("shutdown receiver");
    assert_eq!(serve(relay, std::future::ready(())).await, RelayExit::Cancelled);
}

/// AC4 mutant (a), as a live discriminator: the counters name a HOST.
///
/// Two real rustain relays. The endpoints are configured with the second one
/// only, so the first must stay at zero accepts while the second carries the
/// exchange. ⚠ Deliberately **not** a non-existent relay: the client would never
/// establish a home relay at all and `await_relay_address` would panic on the
/// deadline — a timeout is a weaker signal than a counter that stayed zero while
/// its twin moved.
#[tokio::test]
async fn a_relay_nobody_configured_stays_at_zero_while_its_twin_carries_the_traffic() {
    let (_dir_a, cert_a, key_a) = self_signed_pems();
    let (_dir_b, cert_b, key_b) = self_signed_pems();
    let (unused, _unused_url) = our_relay(&cert_a, &key_a).await;
    let (chosen, chosen_url) = our_relay(&cert_b, &key_b).await;

    let workspace = tempfile::tempdir().expect("workspace");
    let mode = relay_config(workspace.path(), &[chosen_url.as_str()]).mode();

    let receiver = IrohPeerTransport::bind_without_direct_paths(
        signing_key(73).to_bytes(),
        HashMap::new(),
        &mode,
    )
    .await
    .expect("bind receiver");
    let receiver_peer = peer_of(73);
    let mut inbound = receiver.inbound().expect("inbound");
    let receiver_address = tokio::time::timeout(DEADLINE, await_relay_address(&receiver))
        .await
        .expect("the receiver registers with the configured relay");

    let sender = IrohPeerTransport::bind_without_direct_paths(
        signing_key(74).to_bytes(),
        HashMap::from([(receiver_peer.clone(), relay_only(&receiver_address))]),
        &mode,
    )
    .await
    .expect("bind sender");

    let receive = tokio::spawn(async move {
        let mut frame = inbound.recv().await.expect("a frame arrives");
        let responder = frame.responder.take().expect("answer channel");
        responder.answer(FrameReply::accepted());
    });
    tokio::time::timeout(
        DEADLINE,
        sender.send_to(&receiver_peer, signed_envelope(74, "twin-relay-discrimination")),
    )
    .await
    .expect("answered")
    .expect("sent");
    receive.await.expect("receiver task");

    assert!(
        chosen.metrics().server.accepts.get() >= 2,
        "the configured relay carried both endpoints"
    );
    assert_eq!(
        unused.metrics().server.accepts.get(),
        0,
        "a relay nobody configured must have accepted nothing — this is what \
         makes the counter a HOST claim rather than a traffic claim"
    );

    sender.shutdown().await.expect("shutdown sender");
    receiver.shutdown().await.expect("shutdown receiver");
    assert_eq!(serve(chosen, std::future::ready(())).await, RelayExit::Cancelled);
    assert_eq!(serve(unused, std::future::ready(())).await, RelayExit::Cancelled);
}

/// AC4 mutant (b): restore the direct path and the relay is bypassed.
///
/// The same two endpoints, bound through the **production** entry
/// `IrohPeerTransport::bind` with a direct address, observe `Direct` — proving
/// the relayed observation above is the structural consequence of
/// `clear_ip_transports()` and ⛔ not an artefact of the harness.
#[tokio::test]
async fn the_same_exchange_with_a_direct_path_bypasses_the_relay_entirely() {
    let (_dir, cert, key) = self_signed_pems();
    let (relay, url) = our_relay(&cert, &key).await;
    let workspace = tempfile::tempdir().expect("workspace");
    let mode = relay_config(workspace.path(), &[url.as_str()]).mode();

    let receiver =
        IrohPeerTransport::bind(signing_key(75).to_bytes(), HashMap::new(), &mode)
            .await
            .expect("bind receiver");
    let receiver_peer = peer_of(75);
    let mut inbound = receiver.inbound().expect("inbound");
    let direct = receiver.local_address().expect("a direct address");

    let sender = IrohPeerTransport::bind(
        signing_key(76).to_bytes(),
        HashMap::from([(receiver_peer.clone(), direct)]),
        &mode,
    )
    .await
    .expect("bind sender");

    let receive = tokio::spawn(async move {
        let mut frame = inbound.recv().await.expect("a frame arrives");
        let responder = frame.responder.take().expect("answer channel");
        responder.answer(FrameReply::accepted());
    });
    let verdict = tokio::time::timeout(
        DEADLINE,
        sender.send_to(&receiver_peer, signed_envelope(76, "direct-path-control")),
    )
    .await
    .expect("answered")
    .expect("sent");
    receive.await.expect("receiver task");

    assert!(
        matches!(verdict.path(), Some(PathObservation::Direct)),
        "with the IP transport present, loopback wins: {:?}",
        verdict.path()
    );

    sender.shutdown().await.expect("shutdown sender");
    receiver.shutdown().await.expect("shutdown receiver");
    assert_eq!(serve(relay, std::future::ready(())).await, RelayExit::Cancelled);
}

// ── AC5b — the serve loop's exit discipline ─────────────────────────────────

/// AC5b positive control, through the front door: a cancel returns the success
/// variant, and the sockets are genuinely released before `serve` returns.
///
/// ⚑ The **structural** property is what is asserted — shutdown is awaited to
/// completion before returning, evidenced by the port being rebindable
/// immediately afterwards. NFR24's < 5 s is applied as a generous failure
/// deadline, ⛔ never as the thing being proven.
#[tokio::test]
async fn a_cancelled_serve_loop_returns_success_after_shutting_the_relay_down() {
    let plan = ServePlan::resolve(&ServeArgs {
        dev: true,
        http_addr: Some("127.0.0.1:0".to_owned()),
        ..ServeArgs::default()
    })
    .expect("plan");
    let server = spawn_relay(&plan).await.expect("a dev relay starts");
    let bound = server.http_addr().expect("bound");

    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let loop_task = tokio::spawn(async move {
        serve(server, async move {
            let _ = rx.await;
        })
        .await
    });
    tx.send(()).expect("the serve loop is listening");

    let exit = tokio::time::timeout(Duration::from_secs(5), loop_task)
        .await
        .expect("NFR24: graceful shutdown must complete inside 5 s")
        .expect("the serve task");
    assert_eq!(exit, RelayExit::Cancelled);
    assert_eq!(exit.code(), 0, "an operator stop is a clean stop");

    std::net::TcpListener::bind(bound)
        .expect("shutdown was awaited to completion, so the port is free again");
}

/// The TLS arm of the rebind proof: all THREE sockets — captive-portal TCP,
/// relay TCP and the discovery UDP socket — must be released before `serve`
/// returns. The dev-path test above proves the discipline on one socket; this
/// proves it on the pair a TLS operator actually runs.
#[tokio::test]
async fn a_cancelled_tls_relay_releases_all_three_sockets() {
    let (_dir, cert, key) = self_signed_pems();
    let plan = ServePlan::resolve(&loopback_tls_args(&cert, &key)).expect("plan");
    let server = spawn_relay(&plan).await.expect("a TLS relay starts");
    // ⚠ Every address is read BEFORE shutdown: it consumes `self`.
    let http = server.http_addr().expect("the probe binds");
    let https = server.https_addr().expect("the relay listener binds");
    let quic = server.quic_addr().expect("discovery binds");

    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let loop_task = tokio::spawn(async move {
        serve(server, async move {
            let _ = rx.await;
        })
        .await
    });
    tx.send(()).expect("the serve loop is listening");

    let exit = tokio::time::timeout(Duration::from_secs(5), loop_task)
        .await
        .expect("NFR24: graceful shutdown must complete inside 5 s")
        .expect("the serve task");
    assert_eq!(exit, RelayExit::Cancelled);

    std::net::TcpListener::bind(http)
        .expect("the probe socket was released before serve returned");
    std::net::TcpListener::bind(https)
        .expect("the relay socket was released before serve returned");
    std::net::UdpSocket::bind(quic)
        .expect("the discovery socket was released before serve returned");
}

/// AC5b keystone: a relay that terminates on its own exits **non-zero**.
///
/// ⚑ **The mutant this exists for:** return `Ok`/zero on the `join()` arm. A dead
/// relay then looks like a clean stop, systemd's `Restart=on-failure` never
/// fires, and the host silently stops carrying traffic. This matters *precisely
/// because* rustain-side supervision defers (`DF-18-4c-b-SUPERVISION`) — the init
/// system is the only thing watching, and it watches the exit code.
///
/// ⚠ **Named exception to this file's no-inline-`ServerConfig` rule, and why.**
/// `spawn_relay` cannot produce a self-terminating relay: every plan it accepts
/// enables the relay service, and iroh's supervisor only finishes on its own when
/// none is enabled. The property under test here is the **serve loop's exit
/// discipline**, not rustain's composition — so a services-disabled config is the
/// only producer of the state, and using it proves nothing about composition
/// either way. ⛔ It must not spread to the round-trip keystones above, whose
/// entire claim is *whose* relay carried the traffic.
#[tokio::test]
async fn a_relay_that_terminates_on_its_own_exits_non_zero() {
    let config = iroh_relay::server::ServerConfig::default();
    let server = iroh_relay::server::Server::spawn(config)
        .await
        .expect("a services-disabled server still spawns");

    // Never resolves: a relay that dies must not hang waiting for a Ctrl-C that
    // will never come.
    let exit = tokio::time::timeout(
        Duration::from_secs(5),
        serve(server, std::future::pending::<()>()),
    )
    .await
    .expect("the join arm must fire, ⛔ not hang on the signal");

    assert_eq!(exit, RelayExit::RelayStopped);
    assert_eq!(
        exit.code(),
        1,
        "non-zero is what makes Restart=on-failure fire"
    );
}

// ── AC6 — the copy, checked on RENDERED output ───────────────────────────────

/// AC6 keystone: every line `relay serve` prints is inside the wording ceiling,
/// asserted over **rendered output**.
///
/// 🔴 A source-literal scan is not enough — 18.4c's preflight P3 caught exactly
/// that false green: `rust_string_literals` cannot see the result of a `format!`,
/// and this surface formats every socket row. The ceiling in
/// `conformance_p2p_ingress.rs::every_p2p_operator_string_stays_within_the_wording_ceiling`
/// covers the literals; this covers what an operator actually sees.
///
/// ⚑ It lives in this target, not in `cli/relay/serve.rs`'s own test module,
/// because the needle array below is itself a source occurrence of the words it
/// forbids, and `the_surface_names_no_tier` scans that module over `code_only()`
/// — which strips comments but not `#[cfg(test)]`. Measured: keeping it there
/// turned that ratchet RED on its own prohibition.
///
/// **Mutant → RED:** add a line containing any needle to `ready_lines` or
/// `disclosure_lines`. **Then** delete `src/adapters/cli/relay/serve.rs` from
/// `owned_modules` — the source scan goes GREEN while this stays RED, which is
/// what proves the two mechanisms are not the same mechanism.
#[test]
fn every_rendered_relay_line_stays_inside_the_wording_ceiling() {
    use rustain::adapters::cli::relay::serve::{
        NoUrl, ServeRefusal, disclosure_lines, ready_lines,
    };

    let (_dir, cert, key) = self_signed_pems();
    let mut rendered: Vec<String> = Vec::new();
    // Both TLS and `--dev` arms: the dev arm renders a different socket row and a
    // different URL line, and a scan of one arm covers neither.
    for args in [
        loopback_tls_args(&cert, &key),
        ServeArgs {
            dev: true,
            ..ServeArgs::default()
        },
    ] {
        let plan = ServePlan::resolve(&args).expect("plan");
        rendered.extend(ready_lines(&plan));
    }
    rendered.extend(disclosure_lines());
    rendered.extend(
        [NoUrl::NoTls, NoUrl::NoHostname, NoUrl::Refused]
            .into_iter()
            .map(|reason| reason.statement().to_owned()),
    );
    rendered.extend(
        [
            ServeRefusal::DevWithTls,
            ServeRefusal::IncompleteCertPair,
            ServeRefusal::TlsFlagsWithoutCert,
            ServeRefusal::UnitNeedsTls,
            ServeRefusal::UnitNeedsFixedPorts,
            ServeRefusal::SameTcpAddress {
                addr: "0.0.0.0:443".parse().unwrap(),
            },
            ServeRefusal::OverlappingTcpBinds {
                http: "0.0.0.0:443".parse().unwrap(),
                https: "127.0.0.1:443".parse().unwrap(),
            },
            ServeRefusal::QuicPortUndiscoverable {
                addr: "127.0.0.1:9999".parse().unwrap(),
            },
            ServeRefusal::UnparsableAddress {
                flag: "--http-addr",
                value: "nope".to_owned(),
            },
            ServeRefusal::BadHostname {
                hostname: "relay.example?x".to_owned(),
            },
            ServeRefusal::UnsafeUnitValue {
                field: "--cert",
                value: "/etc/my certs/relay.crt".to_owned(),
            },
        ]
        .into_iter()
        .map(|refusal| refusal.statement()),
    );

    assert_eq!(
        rendered.len(),
        33,
        "positive control, EXACT: 7 TLS ready rows + 5 dev ready rows + 7 \
         disclosure rows + 3 no-URL reasons + 11 refusals. A row added to the \
         surface without being added here is a line this ceiling never read — \
         {rendered:#?}"
    );

    for line in &rendered {
        let lowered = line.to_ascii_lowercase();
        for forbidden in [
            // `UX-DR-PT-13` and the shipped ceiling, on rendered text.
            "authenticated",
            "tamper-evident",
            "verified",
            "audit trail",
            "evidence",
            "proof",
            "private",
            "anonymous",
            "end-to-end secure",
            "cannot read",
            "ciphertext only",
            "zero-config",
            "any-nat",
            "relay-reachable",
            "secure",
            "enterprise",
            "trusted",
            "free tier",
            // ⛔ Binding a wildcard address proves a socket, ⛔ not that a peer
            // on the internet can reach it. `UX-DR-PT-13` forbids the claim.
            "reachable from the internet",
            "publicly reachable",
            "anyone on the internet",
            // ⛔ The story key says self-hosting; the copy may not. The client's
            // label is `configured`, because a host that points at a relay cannot
            // claim to run it — and `relay serve` is how an operator becomes the
            // host without the label encoding custody. `posture` is a
            // specification term and ships in no string.
            "self-hosted",
            "self-hosting",
            "posture",
            // ⛔ No class of customer, in any spelling.
            "trusttier",
            "paid",
            "edition",
        ] {
            assert!(
                !lowered.contains(forbidden),
                "forbidden relay wording {forbidden:?} in rendered line {line:?}"
            );
        }
    }
}

/// AC6's three required statements, asserted on the block an operator reads.
///
/// ⚑ Each names a MECHANISM rather than denying a property (constraint 13).
#[test]
fn the_disclosure_states_membership_reach_and_the_restart_effect() {
    let block = rustain::adapters::cli::relay::serve::disclosure_lines().join(" ");

    // 1 — D13 membership. Without this an operator stands up a relay, hands out
    // the URL and watches nothing connect: the relay set is configuration on the
    // DIALING side, and `dialable_reach` silently drops a non-member relay.
    assert!(block.contains(".rustain/relay.json"), "{block}");
    assert!(block.contains("dropped before dial"), "{block}");

    // 2 — this cut ships `AllowAll`. Silence here would let an operator assume
    // the relay is restricted to their own peers, which it is not.
    assert!(
        block.contains("carries traffic for anyone holding its URL"),
        "{block}"
    );

    // 3 — forward-only conduit: a restart does not preserve relayed traffic, and
    // per `DF-18-4-CROSSHOST-RETRACT` no recall or unsend may be implied.
    assert!(block.contains("restart ends every connection"), "{block}");
    assert!(block.contains("kept, queued or recoverable"), "{block}");
}


