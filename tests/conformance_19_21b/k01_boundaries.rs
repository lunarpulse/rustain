use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::domain::models::{PermissionMode, ResponseMode, SemanticMessageType};
use serde_json::json;

use super::observed_host::{HttpCounts, ObservedHost};
use super::proposal_harness::ProposalFixture;

const R2: &str = "Not proposed: no operator surface is attached to approve it. Nothing was sent.";

async fn host() -> ObservedHost {
    ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await
}

#[tokio::test]
async fn no_surface_returns_r2_instead_of_parking_or_creating_a_card() {
    let peer = host().await;
    let mut fixture = ProposalFixture::start_without_surface(vec![peer.peer("lena-po")]).await;
    fixture.set_mode(PermissionMode::Yolo);
    assert!(
        peer.counts().get > 0,
        "boot card GET must reach the real observer"
    );
    peer.reset_counts();
    let result = fixture
        .submit(
            json!({"recipients":["lena-po"],"type":"architecture_update","body":"private draft"}),
        )
        .await;
    assert_eq!(result, R2);
    assert!(fixture.last_turn_completed);
    assert!(fixture.state.pending_team_proposals.is_empty());
    assert_eq!(peer.counts(), HttpCounts { get: 0, post: 0 });
    assert!(peer.journal().entries().await.is_empty());
    assert!(fixture.sender.journal.entries().await.is_empty());
    peer.stop().await;
}

#[tokio::test]
async fn no_egress_does_not_offer_a_model_proposal_capability() {
    let peer = host().await;
    let fixture = ProposalFixture::start_without_egress(vec![peer.peer("lena-po")]).await;
    assert!(
        !fixture
            .available_tools()
            .iter()
            .any(|name| name == "propose_team_message"),
        "without egress the model must not be offered a proposal capability"
    );
    assert!(
        !fixture
            .describe()
            .iter()
            .any(|tool| tool.name == "propose_team_message")
    );
    peer.stop().await;
}

#[tokio::test]
async fn exact_utf8_byte_limit_and_allowed_tabs_newlines_preserve_the_snapshot() {
    let peer = host().await;
    let mut fixture = ProposalFixture::start(vec![peer.peer("lena-po")]).await;
    fixture.set_mode(PermissionMode::Yolo);
    peer.reset_counts();
    let body = format!("{}\n\t", "é".repeat(65_535));
    assert_eq!(body.len(), 131_072);
    let result = fixture
        .submit(json!({"recipients":["lena-po"],"type":"architecture_update","body":body}))
        .await;
    assert_eq!(
        result,
        "Proposal shown to the operator. Nothing has been sent; the operator decides whether to send it to lena-po."
    );
    let snapshot = fixture
        .state
        .pending_team_proposals
        .get(&fixture.conversation.id)
        .expect("ready event must retain its real snapshot");
    assert_eq!(snapshot.body.as_bytes(), body.as_bytes());
    assert_eq!(
        snapshot.message_type,
        SemanticMessageType::ArchitectureUpdate
    );
    assert_eq!(snapshot.recipients, ["lena-po"]);
    assert_eq!(snapshot.conversation_id, fixture.conversation.id);
    assert!(snapshot.id.starts_with("tp_"));
    assert_eq!(peer.counts(), HttpCounts { get: 0, post: 0 });
    assert!(peer.journal().entries().await.is_empty());
    peer.stop().await;
}

#[tokio::test]
async fn utf8_limit_refusal_reports_bytes_not_character_count() {
    let peer = host().await;
    let mut fixture = ProposalFixture::start(vec![peer.peer("lena-po")]).await;
    fixture.set_mode(PermissionMode::Yolo);
    peer.reset_counts();
    let result = fixture.submit(json!({"recipients":["lena-po"],"type":"architecture_update","body":"é".repeat(65_537)})).await;
    assert!(
        result.contains("131074") && result.contains("131072"),
        "R4 must identify actual bytes and the bound: {result}"
    );
    assert!(fixture.state.pending_team_proposals.is_empty());
    assert_eq!(peer.counts(), HttpCounts { get: 0, post: 0 });
    assert!(peer.journal().entries().await.is_empty());
    assert!(fixture.sender.journal.entries().await.is_empty());
    peer.stop().await;
}

#[tokio::test]
async fn forbidden_range_edges_report_utf8_byte_offsets_without_recipient_io() {
    let peer = host().await;
    let mut fixture = ProposalFixture::start(vec![peer.peer("lena-po")]).await;
    fixture.set_mode(PermissionMode::Yolo);
    peer.reset_counts();
    for forbidden in [
        '\u{0000}', '\u{001f}', '\u{0080}', '\u{009f}', '\u{202a}', '\u{2066}', '\u{2069}',
    ] {
        let result = fixture.submit(json!({"recipients":["lena-po"],"type":"architecture_update","body":format!("é\t{forbidden}end")})).await;
        assert!(
            result.contains("body") && result.contains("byte offset 3"),
            "R4 must locate U+{:04X} by bytes, not characters: {result}",
            forbidden as u32
        );
        assert!(fixture.state.pending_team_proposals.is_empty());
    }
    assert_eq!(peer.counts(), HttpCounts { get: 0, post: 0 });
    assert!(peer.journal().entries().await.is_empty());
    assert!(fixture.sender.journal.entries().await.is_empty());
    peer.stop().await;
}
