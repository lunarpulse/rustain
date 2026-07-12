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
