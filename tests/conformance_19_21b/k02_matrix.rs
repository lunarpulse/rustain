use rustain::domain::events::DomainInputEvent;
use rustain::domain::models::{JournalRecord, RoomEvent};

use super::observed_host::{HttpCounts, ObservedHost};
use super::proposal_harness::ProposalFixture;

pub(crate) async fn assert_pending_without_egress(
    fixture: &ProposalFixture,
    hosts: &[&ObservedHost],
) {
    assert!(
        fixture
            .state
            .pending_team_proposals
            .contains_key(&fixture.conversation.id),
        "proposal remains pending until a human Send key"
    );
    for host in hosts {
        assert_eq!(
            host.counts(),
            HttpCounts { get: 0, post: 0 },
            "automatic policy never contacts a recipient"
        );
    }
    assert!(
        !fixture.sender.rows().await.iter().any(|row| matches!(
            row.record,
            JournalRecord::Room(
                RoomEvent::TeamSendActionRecorded { .. }
                    | RoomEvent::RemoteEnvelopeDispatched { .. }
            )
        )),
        "pending proposal writes no send action or dispatch"
    );
}

pub(crate) async fn approve_once(fixture: &mut ProposalFixture, hosts: &[&ObservedHost]) {
    fixture.paint(60, 16);
    fixture.key(DomainInputEvent::KeyPress('y')).await;
    fixture.settle(hosts.len()).await;
    assert!(
        !fixture
            .state
            .pending_team_proposals
            .contains_key(&fixture.conversation.id),
        "human Send consumes the pending proposal"
    );
    for host in hosts {
        assert_eq!(
            host.counts(),
            HttpCounts { get: 0, post: 1 },
            "human Send performs exactly one POST per recipient"
        );
    }
    let rows = fixture.sender.rows().await;
    assert_eq!(
        rows.iter()
            .filter(|row| matches!(
                row.record,
                JournalRecord::Room(RoomEvent::TeamSendActionRecorded { .. })
            ))
            .count(),
        1,
        "one human Send records exactly one action"
    );
    assert_eq!(
        rows.iter()
            .filter(|row| matches!(
                row.record,
                JournalRecord::Room(RoomEvent::RemoteEnvelopeDispatched { .. })
            ))
            .count(),
        hosts.len(),
        "one human Send dispatches once to each recipient"
    );
}

async fn mode_requires_human_send(mode: rustain::domain::models::PermissionMode) {
    use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
    use rustain::domain::models::{ResponseMode, ToolCall};
    use serde_json::json;

    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut fixture = ProposalFixture::start(vec![host.peer("lena-po")]).await;
    fixture.set_mode(mode);
    host.reset_counts();
    let mut approvals = fixture.approval.subscribe();
    let path = fixture
        .sender
        .journal
        .workspace
        .path()
        .join("mode-control.txt");
    let contents = format!("{mode:?} permits the ordinary Write, not a team send");
    fixture
        .submit_tool("Write", json!({"file_path": path, "content": contents}))
        .await;
    assert_eq!(
        std::fs::read_to_string(&path).expect("real Write completed"),
        contents
    );
    assert_eq!(fixture.last_tool_errors, [false]);
    assert!(
        !fixture
            .last_transitions
            .iter()
            .any(|transition| matches!(&transition.call, ToolCall::AwaitingApproval { .. })),
        "the ordinary Write proves this permission mode is live"
    );
    assert!(
        matches!(
            approvals.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ),
        "the mode control must run without an approval prompt"
    );
    assert_eq!(
        fixture
            .submit(json!({
                "recipients": ["lena-po"],
                "type": "architecture_update",
                "body": "ADR-047: require a human decision even when tools are auto-approved."
            }))
            .await,
        "Proposal shown to the operator. Nothing has been sent; the operator decides whether to send it to lena-po."
    );
    assert_pending_without_egress(&fixture, &[&host]).await;
    assert!(
        matches!(
            approvals.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ),
        "a proposal must not enter the tool approval runtime"
    );
    approve_once(&mut fixture, &[&host]).await;
    host.stop().await;
}

#[tokio::test]
async fn k02_yolo_allows_write_but_only_human_key_sends_proposal() {
    mode_requires_human_send(rustain::domain::models::PermissionMode::Yolo).await;
}

#[tokio::test]
async fn k02_auto_edit_allows_write_but_only_human_key_sends_proposal() {
    mode_requires_human_send(rustain::domain::models::PermissionMode::AutoEdit).await;
}
