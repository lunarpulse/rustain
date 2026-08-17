#![cfg(feature = "p2p")]

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rustain::adapters::iroh::{IrohPeerTransport, derive_peer_endpoint_identity};
use rustain::adapters::rap::AgentSigner;
use rustain::domain::models::{
    AgentEnvelope, AgentId, CorrelationId, FrameOutcome, FrameReply, MessageKind, PeerId, RelayMode,
};
use rustain::domain::ports::{PeerTransport, PeerTransportError};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn source(relative: &str) -> String {
    std::fs::read_to_string(root().join(relative))
        .unwrap_or_else(|error| panic!("read {relative}: {error}"))
}

fn signing_key(seed: u8) -> ed25519_dalek::SigningKey {
    ed25519_dalek::SigningKey::from_bytes(&[seed; 32])
}

fn signed_envelope(seed: u8) -> AgentEnvelope<serde_json::Value> {
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
            CorrelationId::new("p2p-round-trip"),
            MessageKind::PeerMessage,
            1,
            9_999,
            "p2p-nonce".to_owned(),
            Vec::new(),
            serde_json::json!({"message": "hello"}),
        )
        .expect("sign envelope")
}

#[test]
fn one_public_key_derives_both_peer_and_endpoint_identity() {
    let public_key = signing_key(7).verifying_key().to_bytes();
    let derived = derive_peer_endpoint_identity(&public_key).expect("valid Ed25519 public key");

    assert_eq!(
        derived.peer_id,
        PeerId::from_public_key(&public_key).expect("peer id")
    );
    assert_eq!(derived.endpoint_id.as_bytes(), &public_key);
    assert_eq!(
        iroh::EndpointId::from_bytes(derived.endpoint_id.as_bytes()).expect("round trip"),
        derived.endpoint_id
    );
}

#[tokio::test]
async fn minimal_i_roh_adapter_round_trips_an_unverified_frame() {
    let server_seed = signing_key(17).to_bytes();
    let server_public = signing_key(17).verifying_key().to_bytes();
    let server_identity =
        derive_peer_endpoint_identity(&server_public).expect("server identity derives");
    let server = IrohPeerTransport::bind(server_seed, HashMap::new(), &RelayMode::Disabled)
        .await
        .expect("bind server");
    let server_address = server.local_address().expect("server address");
    let mut server_inbound = server.inbound().expect("take inbound receiver");

    let client_seed = signing_key(23).to_bytes();
    let client_public = signing_key(23).verifying_key().to_bytes();
    let client_identity =
        derive_peer_endpoint_identity(&client_public).expect("client identity derives");
    let client = IrohPeerTransport::bind(
        client_seed,
        HashMap::from([(server_identity.peer_id.clone(), server_address)]),
        &RelayMode::Disabled,
    )
    .await
    .expect("bind client");

    client
        .dial(&server_identity.peer_id)
        .await
        .expect("dial direct endpoint address");
    let envelope = signed_envelope(23);

    // A frame is a request now, so the receiver has to answer or the sender waits
    // out its verdict timeout. The answering half runs as its own task — which is
    // what a real ingress is — and this is what proves the answer channel end to
    // end rather than by inspection.
    let receive = tokio::spawn(async move {
        let mut frame = server_inbound.recv().await.expect("accepted frame");
        let responder = frame
            .responder
            .take()
            .expect("a frame on a real connection carries an answer channel");
        responder.answer(FrameReply::accepted());
        frame
    });
    let verdict = client
        .send_to(&server_identity.peer_id, envelope.clone())
        .await
        .expect("send signed envelope");
    let frame = receive.await.expect("receiver task");

    assert_eq!(frame.peer_id, client_identity.peer_id);
    assert_eq!(frame.envelope, envelope);
    assert_eq!(
        verdict.outcome(),
        FrameOutcome::Accepted,
        "the sender must learn the outcome from the receiver, never infer it"
    );
    assert!(
        verdict.expected.is_none(),
        "an acceptance carries no feed-position correction"
    );

    client.shutdown().await.expect("shutdown client");
    server.shutdown().await.expect("shutdown server");
}

#[tokio::test]
async fn closed_cached_connection_is_evicted_before_redial() {
    let server_seed = signing_key(31).to_bytes();
    let server_public = signing_key(31).verifying_key().to_bytes();
    let server_identity =
        derive_peer_endpoint_identity(&server_public).expect("server identity derives");
    let server = IrohPeerTransport::bind(server_seed, HashMap::new(), &RelayMode::Disabled)
        .await
        .expect("bind server");
    let server_address = server.local_address().expect("server address");

    let client_seed = signing_key(37).to_bytes();
    let client = IrohPeerTransport::bind(
        client_seed,
        HashMap::from([(server_identity.peer_id.clone(), server_address)]),
        &RelayMode::Disabled,
    )
    .await
    .expect("bind client");

    client
        .dial(&server_identity.peer_id)
        .await
        .expect("establish cached connection");
    assert_eq!(client.active_connection_count().await, 1);

    server.shutdown().await.expect("close remote endpoint");
    let error = client
        .dial(&server_identity.peer_id)
        .await
        .expect_err("closed cached connection must be replaced by a new dial");
    assert!(matches!(error, PeerTransportError::Dial(_)));
    assert_eq!(client.active_connection_count().await, 0);

    client.shutdown().await.expect("shutdown client");
}

#[tokio::test]
async fn shutdown_clears_active_connection_cache() {
    let server_seed = signing_key(41).to_bytes();
    let server_public = signing_key(41).verifying_key().to_bytes();
    let server_identity =
        derive_peer_endpoint_identity(&server_public).expect("server identity derives");
    let server = IrohPeerTransport::bind(server_seed, HashMap::new(), &RelayMode::Disabled)
        .await
        .expect("bind server");
    let server_address = server.local_address().expect("server address");

    let client = IrohPeerTransport::bind(
        signing_key(43).to_bytes(),
        HashMap::from([(server_identity.peer_id.clone(), server_address)]),
        &RelayMode::Disabled,
    )
    .await
    .expect("bind client");

    client
        .dial(&server_identity.peer_id)
        .await
        .expect("establish cached connection");
    assert_eq!(client.active_connection_count().await, 1);

    client.shutdown().await.expect("shutdown client");
    assert_eq!(client.active_connection_count().await, 0);
    server.shutdown().await.expect("shutdown server");
}

#[test]
fn nfr74_transport_types_never_become_room_authority_or_provenance() {
    let scoped = [
        (
            "src/domain/models/orchestration_room.rs",
            "pub enum RoomEvent",
        ),
        (
            "src/domain/models/capability_token.rs",
            "pub struct CapabilityToken",
        ),
        ("src/domain/models/taint.rs", "pub enum ProvenanceTag"),
        (
            "src/domain/models/context_bundle.rs",
            "pub struct ProvenancedEntry",
        ),
    ];

    for (relative, positive_control) in scoped {
        let text = source(relative);
        assert!(
            text.contains(positive_control),
            "positive control missing from {relative}: {positive_control}"
        );
        for forbidden_type in [
            "iroh::",
            "EndpointId",
            "EndpointAddr",
            "PeerAddress",
            "transport_address",
        ] {
            assert!(
                !text.contains(forbidden_type),
                "NFR74 violation in {relative}: {forbidden_type} is a transport type, not authority or provenance"
            );
        }
    }
}
