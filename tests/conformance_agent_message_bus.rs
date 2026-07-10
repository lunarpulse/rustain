use rustain::domain::models::{
    AgentId, AgentMessage, CorrelationId, DeliveryMode, Envelope, MessageHeader, MessageKind,
    NodeState, delivery_decision,
};

#[test]
fn ac1_status_routing_positive_controls() {
    assert_eq!(delivery_decision(NodeState::Running), DeliveryMode::Aside);
    assert_eq!(delivery_decision(NodeState::Waiting), DeliveryMode::Wake);
    assert_eq!(delivery_decision(NodeState::Created), DeliveryMode::Wake);
    assert_eq!(delivery_decision(NodeState::Suspended), DeliveryMode::Queue);
}

#[test]
fn ac2_run_child_production_has_no_try_recv_consumption() {
    let source = include_str!("../src/adapters/subagent/in_process_runner.rs");
    let production = source
        .split("#[cfg(test)]")
        .next()
        .expect("production section exists");
    // Story 14-4a: try_recv is allowed ONLY inside drain_mailbox (the
    // terminal drain after close()). The inbound spine must use recv().
    let drain_section = production.split("command_rx.close()").nth(1).unwrap_or("");
    let non_drain = &production[..production.len() - drain_section.len()];
    assert_eq!(
        non_drain.matches("try_recv(").count(),
        0,
        "run_child inbound spine must use the select-able recv(), not try_recv polling"
    );
    assert!(
        production.contains("maybe_op = command_rx.recv()"),
        "streaming select must include the command receiver arm"
    );
}

#[test]
fn ac5_header_shape_round_trip_no_sequence_by_default() {
    let env = Envelope {
        header: MessageHeader {
            sender: AgentId::parse("parent").unwrap(),
            recipient: AgentId::parse("child").unwrap(),
            correlation_id: CorrelationId::new("corr-42"),
            kind: MessageKind::PeerMessage,
            sequence: None,
        },
        body: AgentMessage::new("status?"),
    };
    let json = serde_json::to_string(&env).expect("serializes");
    assert!(!json.contains("sequence"));
    let parsed: Envelope<AgentMessage> = serde_json::from_str(&json).expect("deserializes");
    assert_eq!(parsed.header.correlation_id, CorrelationId::new("corr-42"));
}

#[test]
fn ac6_approval_source_has_no_remote_peer_and_stays_non_exhaustive() {
    let source = include_str!("../src/domain/models/tool_call.rs");
    let before_enum = source
        .split("pub enum ApprovalSource")
        .next()
        .expect("source loaded");
    let enum_body = source
        .split("pub enum ApprovalSource")
        .nth(1)
        .and_then(|tail| tail.split("impl ApprovalSource").next())
        .expect("ApprovalSource enum body exists");
    assert!(
        before_enum.contains("#[non_exhaustive]"),
        "ApprovalSource must remain non_exhaustive for R2 additive RemotePeer"
    );
    assert!(
        !enum_body.contains("RemotePeer"),
        "R1 must not add ApprovalSource::RemotePeer variant"
    );
}
