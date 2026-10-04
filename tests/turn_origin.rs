use rustain::domain::models::{ApprovalSource, NodeOrigin, PeerId, PolicyPrincipal, TurnOrigin};

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
            TurnOrigin::RemotePeer {
                principal: PolicyPrincipal::rap(peer_id()),
            },
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
    let principal = PolicyPrincipal::rap(peer_id.clone());
    let source = TurnOrigin::RemotePeer {
        principal: principal.clone(),
    }
    .approval_source("conversation-1");

    match &source {
        ApprovalSource::RemotePeer {
            conversation_id,
            peer_id: carried,
            ..
        } => {
            assert_eq!(conversation_id, "conversation-1");
            assert_eq!(carried, &peer_id);
        }
        other => panic!("expected RemotePeer source, got {other:?}"),
    }
    assert_eq!(source.policy_principal(), Some(&principal));
}

/// K06a: the turn origin carries the ingress kind into the approval source
/// unchanged — a pseudonymous caller never becomes pinned (or the reverse).
#[test]
fn k06a_turn_origin_carries_principal_kind_into_approval_source() {
    let peer = peer_id();
    let pinned = PolicyPrincipal::rap(peer.clone());
    let pseudonymous = PolicyPrincipal::Pseudonymous(peer.clone());

    let from_pinned = TurnOrigin::RemotePeer {
        principal: pinned.clone(),
    }
    .approval_source("conversation-1");
    let from_pseudonymous = TurnOrigin::RemotePeer {
        principal: pseudonymous.clone(),
    }
    .approval_source("conversation-1");

    assert_eq!(from_pinned.policy_principal(), Some(&pinned));
    assert_eq!(from_pseudonymous.policy_principal(), Some(&pseudonymous));
    assert_ne!(
        from_pinned.policy_principal(),
        from_pseudonymous.policy_principal()
    );
    assert_eq!(from_pinned.conversation_id(), "conversation-1");
    assert_eq!(from_pseudonymous.conversation_id(), "conversation-1");
    // Non-remote origins carry no policy principal.
    assert_eq!(
        TurnOrigin::Interactive
            .approval_source("conversation-1")
            .policy_principal(),
        None
    );
}

/// K06c: the approval scope string and the serialized bytes are independent of
/// the principal kind, and a decoded source never recovers provenance.
#[test]
fn k06c_scope_and_wire_bytes_are_kind_independent_and_decode_without_provenance() {
    let peer =
        PeerId::parse("1220fd4baad8bbc21b3d13fcc22ab21561129ab39d9c335fd4f7509cf26ba15f7569")
            .unwrap();
    let expected_scope =
        "15:consent-gate-r168:1220fd4baad8bbc21b3d13fcc22ab21561129ab39d9c335fd4f7509cf26ba15f7569";
    // Exact source bytes from Task 0 G4; no production ApprovalResolved writer.
    let expected_wire = r#"{"RemotePeer":{"conversation_id":"consent-gate-r1","peer_id":"1220fd4baad8bbc21b3d13fcc22ab21561129ab39d9c335fd4f7509cf26ba15f7569"}}"#;

    for principal in [
        PolicyPrincipal::rap(peer.clone()),
        PolicyPrincipal::Pseudonymous(peer.clone()),
    ] {
        let source = ApprovalSource::remote_peer("consent-gate-r1".to_owned(), &principal);
        assert_eq!(source.scope_agent_id().as_str(), expected_scope);

        let wire = serde_json::to_string(&source).expect("serialize source");
        assert_eq!(wire, expected_wire);
        assert!(!wire.contains("principal"));

        let decoded: ApprovalSource = serde_json::from_str(&wire).expect("decode source");
        assert_eq!(decoded.policy_principal(), None);
        assert_eq!(decoded.scope_agent_id().as_str(), expected_scope);
        match decoded {
            ApprovalSource::RemotePeer {
                conversation_id,
                peer_id,
                ..
            } => {
                assert_eq!(conversation_id, "consent-gate-r1");
                assert_eq!(peer_id, peer);
            }
            other => panic!("expected RemotePeer, got {other:?}"),
        }
    }
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
