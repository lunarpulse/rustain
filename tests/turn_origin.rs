use rustain::domain::models::{NodeOrigin, PeerId, TurnOrigin};

fn peer_id() -> PeerId {
    PeerId::from_public_key(&[7; 32]).expect("valid test peer id")
}

#[test]
fn turn_origin_maps_every_variant_to_its_node_birth_origin() {
    let cases = [
        (TurnOrigin::Interactive, NodeOrigin::Interactive),
        (
            TurnOrigin::Acp {
                session_id: "acp-1".into(),
            },
            NodeOrigin::Interactive,
        ),
        (
            TurnOrigin::RemotePeer { peer_id: peer_id() },
            NodeOrigin::Remote,
        ),
        (TurnOrigin::Subagent, NodeOrigin::Subagent),
        (TurnOrigin::Cron, NodeOrigin::Cron),
        (TurnOrigin::Channel, NodeOrigin::Channel),
    ];

    for (turn_origin, node_origin) in cases {
        assert_eq!(turn_origin.node_origin(), node_origin);
    }
}

#[test]
fn remote_peer_origin_preserves_peer_identity_in_approval_scope() {
    let peer_id = peer_id();
    let source = TurnOrigin::RemotePeer {
        peer_id: peer_id.clone(),
    }
    .approval_source("conversation-1");

    assert_eq!(
        source,
        rustain::domain::models::ApprovalSource::RemotePeer {
            conversation_id: "conversation-1".into(),
            peer_id,
        }
    );
}

#[test]
fn runtime_tool_authorization_never_reconstructs_origin_from_acp_wire_prefix() {
    for (path, source) in [
        (
            "runtime/turn.rs",
            include_str!("../src/infrastructure/runtime/turn.rs"),
        ),
        ("acp/run.rs", include_str!("../src/adapters/acp/run.rs")),
        ("acp/agent.rs", include_str!("../src/adapters/acp/agent.rs")),
    ] {
        assert!(
            !source.contains("is_acp_session_id"),
            "{path} must consume typed/session identity state, not branch on ACP's prefix"
        );
    }
}

#[test]
fn acp_wire_session_format_still_round_trips() {
    let formatted = rustain::adapters::acp::format_acp_session_id("conversation-42");
    assert_eq!(
        rustain::adapters::acp::conversation_id_from_acp_session_id(&formatted),
        Some("conversation-42")
    );
}
