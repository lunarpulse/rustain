use super::inherited::Host;
use super::proposal_harness::ProposalFixture;
use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::tui::app::InputAction;
use rustain::domain::events::DomainInputEvent;
use rustain::domain::models::{
    Authorship, JournalRecord, ResponseMode, RoomEvent, SendAuthorization,
};
use serde_json::json;

#[tokio::test]
async fn k20a_approved_proposal_is_agent_composed_human_decided() {
    let host = Host::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut f = ProposalFixture::start(vec![host.peer("lena-po")]).await;
    let result = f.submit(json!({"recipients":["lena-po"],"type":"architecture_update","body":"proposal authorization control"})).await;
    assert!(
        result.starts_with("Proposal shown to the operator."),
        "{result}"
    );
    f.paint(100, 32);
    assert!(
        f.state
            .pending_team_proposals
            .contains_key(&f.conversation.id)
    );
    let decision = f.key(DomainInputEvent::KeyPress('y')).await;
    assert!(
        matches!(decision, InputAction::TeamProposal { .. }),
        "{decision:?}"
    );
    assert!(
        !f.state
            .pending_team_proposals
            .contains_key(&f.conversation.id),
        "M38: real y must close the raised card"
    );
    assert_eq!(
        f.state.team_send_blocks.len(),
        1,
        "M38: real y must create ordinary send handoff rows"
    );
    let block_id = f.state.team_send_blocks.keys().next().unwrap().clone();
    f.settle(1).await;
    assert!(
        f.state.feedback_blocks[&block_id]
            .message
            .lines()
            .any(|line| line.split_whitespace().take(2).eq(["●", "lena-po"])),
        "ordinary send row must settle Delivered"
    );
    let entries = f.sender.journal.entries().await;
    let dispatches: Vec<_> = entries
        .iter()
        .filter(|e| {
            matches!(
                e.record,
                JournalRecord::Room(RoomEvent::RemoteEnvelopeDispatched { .. })
            )
        })
        .collect();
    assert_eq!(dispatches.len(), 1, "approved card dispatches exactly once");
    let entry = dispatches[0];
    let JournalRecord::Room(RoomEvent::RemoteEnvelopeDispatched {
        authorship,
        send_authorization,
        ..
    }) = &entry.record
    else {
        unreachable!()
    };
    assert_eq!(*authorship, Authorship::AgentComposed);
    assert_eq!(
        *send_authorization,
        SendAuthorization::HumanDecided,
        "M27: proposal y is a human decision, not automatic"
    );
    let row = rustain::domain::services::transparency::transparency_row(entry).unwrap();
    assert!(row.summary.contains("[agent-composed]"), "{}", row.summary);
    assert!(
        !row.summary.contains("[auto-sent]"),
        "M26: approved proposal is not auto-sent: {}",
        row.summary
    );
    assert_eq!(
        super::inherited::read_room_events(&host.journal)
            .await
            .iter()
            .filter(|(_, e)| matches!(e, RoomEvent::RecipientItemReceived { .. }))
            .count(),
        1
    );
    host.stop().await;
}
