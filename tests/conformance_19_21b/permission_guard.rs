use rustain::adapters::{noop::NoOpToolSet, security_adapter::SecurityAdapter};
use rustain::domain::{
    models::{PermissionMode, tool_call::ApprovalSource},
    ports::SecurityPort,
    services::permission_chain::{self, PermissionDecision},
};

/// The legacy API carries source but no ingress route. A foreground-looking
/// source must not grant proposal authority to an unbound caller.
#[tokio::test]
async fn legacy_permission_entry_never_infers_interactive_from_foreground_source() {
    let workspace = tempfile::tempdir().expect("workspace");
    let security = SecurityAdapter::new(workspace.path().to_path_buf());
    security.set_mode(PermissionMode::Yolo);
    let source = ApprovalSource::ForegroundTurn {
        conversation_id: "legacy-route".into(),
    };
    let result = permission_chain::check_with_source(
        &security,
        "propose_team_message",
        &serde_json::json!({"recipients":["lena-po"],"type":"architecture_update","body":"memo"}),
        None,
        None,
        &NoOpToolSet,
        Some(&source),
    )
    .await;
    assert!(
        matches!(result, PermissionDecision::Deny(_)),
        "unrouted legacy calls must remain denied: {result:?}"
    );
}

#[tokio::test]
async fn proposal_guard_requires_both_interactive_route_and_foreground_source() {
    use rustain::domain::models::{ProvenanceTag, TurnRoute};
    let workspace = tempfile::tempdir().expect("workspace");
    let security = SecurityAdapter::new(workspace.path().to_path_buf());
    let foreground = ApprovalSource::ForegroundTurn {
        conversation_id: "guard".into(),
    };
    let other_sources = [
        None,
        Some(ApprovalSource::ForegroundSubagent {
            conversation_id: "guard".into(),
            parent_tool_call_id: "parent".into(),
            subagent_type: "worker".into(),
        }),
        Some(ApprovalSource::BackgroundAgent {
            conversation_id: "guard".into(),
            task_id: "background".into(),
            subagent_type: "worker".into(),
        }),
        Some(ApprovalSource::AcpSession {
            conversation_id: "guard".into(),
            session_id: "acp".into(),
        }),
        Some(ApprovalSource::RemotePeer {
            conversation_id: "guard".into(),
            peer_id: rustain::domain::models::peer_identity::PeerId::from_public_key(&[7; 32])
                .expect("peer identity"),
            principal: None,
        }),
    ];
    let input =
        serde_json::json!({"recipients":["lena-po"],"type":"architecture_update","body":"memo"});
    for mode in [
        PermissionMode::Normal,
        PermissionMode::Yolo,
        PermissionMode::AutoEdit,
        PermissionMode::Plan,
    ] {
        security.set_mode(mode);
        for route in [
            Some(TurnRoute::Cron),
            Some(TurnRoute::Acp),
            Some(TurnRoute::RemotePeer),
            Some(TurnRoute::Subagent),
            None,
            Some(TurnRoute::Channel),
        ] {
            let decision = permission_chain::check_with_source_and_provenance_and_restriction(
                &security,
                "propose_team_message",
                &input,
                None,
                None,
                None,
                &NoOpToolSet,
                Some(&foreground),
                ProvenanceTag::UserOriginated,
                route,
            )
            .await;
            assert!(
                matches!(decision, PermissionDecision::Deny(_)),
                "noninteractive ingress must be denied in {mode:?}: {route:?}"
            );
        }
        for source in &other_sources {
            let decision = permission_chain::check_with_source_and_provenance_and_restriction(
                &security,
                "propose_team_message",
                &input,
                None,
                None,
                None,
                &NoOpToolSet,
                source.as_ref(),
                ProvenanceTag::UserOriginated,
                Some(TurnRoute::Interactive),
            )
            .await;
            assert!(
                matches!(decision, PermissionDecision::Deny(_)),
                "nonforeground source must be denied in {mode:?}: {source:?}"
            );
        }
        let admitted = permission_chain::check_with_source_and_provenance_and_restriction(
            &security,
            "propose_team_message",
            &input,
            None,
            None,
            None,
            &NoOpToolSet,
            Some(&foreground),
            ProvenanceTag::UserOriginated,
            Some(TurnRoute::Interactive),
        )
        .await;
        if mode == PermissionMode::Plan {
            assert!(
                matches!(admitted, PermissionDecision::Deny(_)),
                "Plan proposal must be denied: {admitted:?}"
            );
        } else {
            assert_eq!(
                admitted,
                PermissionDecision::Allow,
                "interactive foreground proposal must be eligible in {mode:?}"
            );
        }
    }
}
