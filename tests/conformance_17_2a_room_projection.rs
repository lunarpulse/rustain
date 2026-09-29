use rustain::domain::models::{
    AgentId, HostBinding, NodeOrigin, OrchestrationRoom, OrchestrationRoomId, RoomEvent, WaveId,
    WaveOutcome,
};

fn agent(id: &str) -> AgentId {
    AgentId::parse(id).expect("valid fixture agent id")
}

#[test]
fn room_projection_roundtrips_through_production_serde_and_is_idempotent() {
    let room_id = OrchestrationRoomId::parse("room-contract").expect("valid room id");
    let wave = WaveId::parse("wave-contract").expect("valid wave id");
    let events = [
        RoomEvent::NodeRegistered {
            node: agent("coordinator"),
            origin: NodeOrigin::Interactive,
            host: HostBinding::new("host-a", "workspace-a"),
        },
        RoomEvent::WaveStarted {
            wave: wave.clone(),
            coordinator: agent("coordinator"),
            spokes: vec![agent("spoke-a"), agent("spoke-b")],
        },
        RoomEvent::WaveCompleted {
            wave: wave.clone(),
            outcome: WaveOutcome::Completed,
        },
    ];

    let encoded = events
        .iter()
        .map(|event| serde_json::to_string(event).expect("serialize production event"))
        .collect::<Vec<_>>();
    let decoded = encoded
        .iter()
        .map(|line| serde_json::from_str::<RoomEvent>(line).expect("deserialize production event"))
        .collect::<Vec<_>>();

    let first = OrchestrationRoom::project(room_id.clone(), decoded.clone());
    let second = OrchestrationRoom::project(room_id, decoded);

    assert_eq!(first, second, "the ordered fold must be idempotent");
    assert_eq!(first.waves().len(), 1, "positive control: wave event bites");
    assert_eq!(first.waves()[0].id, wave);
    assert_eq!(first.waves()[0].outcome, Some(WaveOutcome::Completed));
}

#[test]
fn dropped_wave_event_diverges_projection() {
    let room_id = OrchestrationRoomId::parse("room-mutant").expect("valid room id");
    let events = [
        RoomEvent::WaveStarted {
            wave: WaveId::parse("wave-mutant").expect("valid wave id"),
            coordinator: agent("coordinator"),
            spokes: vec![agent("spoke-a")],
        },
        RoomEvent::WaveCompleted {
            wave: WaveId::parse("wave-mutant").expect("valid wave id"),
            outcome: WaveOutcome::Failed,
        },
    ];

    let complete = OrchestrationRoom::project(room_id.clone(), events.clone());
    let dropped = OrchestrationRoom::project(room_id, events.into_iter().take(1));

    assert_ne!(
        complete, dropped,
        "mutant: dropping a durable event must bite"
    );
}

/// P9: a node registered on its home host, then marked unavailable during a
/// foreign-host replay, must render available again when projected for its home
/// host — the stale `HostBoundUnavailable` marker never sticks after it returns
/// home. A genuinely foreign host still renders unavailable (ADR-17-CC-03).
#[test]
fn host_bound_marker_clears_when_node_returns_home() {
    let room_id = OrchestrationRoomId::parse("room-home").expect("valid room id");
    let node = agent("node-a");
    let host = HostBinding::new("host-a", "workspace-a");
    let events = vec![
        RoomEvent::NodeRegistered {
            node: node.clone(),
            origin: NodeOrigin::Subagent,
            host: host.clone(),
        },
        RoomEvent::HostBoundUnavailable {
            node: node.clone(),
            host,
        },
    ];

    let home = OrchestrationRoom::project_for_host(room_id.clone(), events.clone(), "host-a");
    assert!(
        !home.nodes()[&node].host_bound_unavailable,
        "returning to the home host clears the stale unavailable marker"
    );

    let foreign = OrchestrationRoom::project_for_host(room_id, events, "host-b");
    assert!(
        foreign.nodes()[&node].host_bound_unavailable,
        "a genuinely foreign host stays unavailable"
    );
}

/// Story 17.4b (Ruling 6): a refused remote envelope must be *inspectable* in
/// the room, not durable-but-invisible. Before 17.4b the apply arm was a literal
/// no-op (`=> {}`), so a rejection projected to nothing.
#[test]
fn remote_envelope_rejection_is_observable_and_idempotent() {
    use rustain::domain::models::{Direction, PeerId, RejectReason};

    let room_id = OrchestrationRoomId::parse("room-reject").expect("valid room id");
    let peer = PeerId::from_public_key(&[9u8; 32]).expect("peer id");
    let events = [
        RoomEvent::RemoteEnvelopeRejected {
            peer: peer.clone(),
            reason: RejectReason::Policy {
                detail: "multi-turn not supported".to_owned(),
            },
            direction: Direction::Outbound,
            task: Some("task-out".to_owned()),
        },
        RoomEvent::RemoteEnvelopeRejected {
            peer: peer.clone(),
            reason: RejectReason::Malformed,
            direction: Direction::Inbound,
            task: Some("task-in".to_owned()),
        },
    ];
    let encoded = events
        .iter()
        .map(|event| serde_json::to_string(event).expect("serialize"))
        .collect::<Vec<_>>();
    let decoded = encoded
        .iter()
        .map(|line| serde_json::from_str::<RoomEvent>(line).expect("deserialize"))
        .collect::<Vec<_>>();

    let first = OrchestrationRoom::project(room_id.clone(), decoded.clone());
    let second = OrchestrationRoom::project(room_id, decoded);
    assert_eq!(first, second, "rejection projection must be idempotent");
    assert_eq!(
        first.remote_rejections().len(),
        2,
        "both rejections must be observable"
    );
    assert_eq!(first.remote_rejections()[0].peer, peer);
    assert!(matches!(
        first.remote_rejections()[1].reason,
        RejectReason::Malformed
    ));
    // Story 18.2 (AC2, P-3): the read model carries `direction` too — a
    // projection that drops a field it was just told to record is the drift
    // this story exists to prevent. Direction is UNRECOVERABLE here: this
    // variant carries no node to derive it from.
    assert_eq!(first.remote_rejections()[0].direction, Direction::Outbound);
    assert_eq!(first.remote_rejections()[1].direction, Direction::Inbound);
}
