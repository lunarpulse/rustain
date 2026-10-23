use std::sync::Arc;

use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::domain::models::{
    ApprovalOutcome, ApprovalScope, ApprovalSource, ResponseMode, ToolRisk,
};
use rustain::domain::ports::ApprovalPersistencePort;
use rustain::domain::services::approval_runtime::ApprovalRuntime;
use serde_json::json;

use super::k02_matrix::{approve_once, assert_pending_without_egress};
use super::observed_host::{HttpCounts, ObservedHost};
use super::proposal_harness::ProposalFixture;

const TOOL: &str = "propose_team_message";
const R1: &str = "Proposal shown to the operator. Nothing has been sent; the operator decides whether to send it to lena-po.";
const PROPOSAL: &str = "ADR-047: preserve the explicit human send boundary.";

async fn fixture() -> (ProposalFixture, ObservedHost) {
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let fixture = ProposalFixture::start(vec![host.peer("lena-po")]).await;
    assert!(
        host.counts().get > 0,
        "warm-up card GET must reach the real observer"
    );
    host.reset_counts();
    (fixture, host)
}

async fn request_tool(
    approval: &ApprovalRuntime,
    conversation_id: &str,
) -> (
    Option<rustain::domain::models::RequestId>,
    tokio::sync::oneshot::Receiver<rustain::domain::services::approval_runtime::ResolvedApproval>,
) {
    approval
        .request(
            ApprovalSource::ForegroundTurn {
                conversation_id: conversation_id.to_owned(),
            },
            TOOL.to_owned(),
            json!({"probe":"approval grant"}),
            ToolRisk::Elevated,
            None,
            None,
        )
        .await
}

async fn prove_proposal_stays_pending(fixture: &mut ProposalFixture, host: &ObservedHost) {
    let mut events = fixture.approval.subscribe();
    assert_eq!(
        fixture
            .submit(json!({
                "recipients": ["lena-po"],
                "type": "architecture_update",
                "body": PROPOSAL,
            }))
            .await,
        R1,
        "production proposal invocation returns R1"
    );
    assert_pending_without_egress(fixture, &[host]).await;
    assert!(
        matches!(
            events.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ),
        "the Safe-risk proposal itself must not request tool approval"
    );
}

#[tokio::test]
async fn session_always_tool_grant_only_leaves_proposal_pending() {
    let (mut fixture, host) = fixture().await;
    let (id, pending) = request_tool(&fixture.approval, &fixture.conversation.id).await;
    let id = id.expect("first foreground probe must create a pending approval");
    fixture
        .approval
        .resolve(
            &id,
            ApprovalOutcome::AlwaysTool {
                tool_name: TOOL.to_owned(),
            },
            rustain::domain::models::PermissionResolution::Operator,
        )
        .await;
    assert_eq!(
        pending.await.expect("resolved pending approval").outcome,
        ApprovalOutcome::AlwaysTool {
            tool_name: TOOL.to_owned(),
        }
    );

    let (id, fast_path) = request_tool(&fixture.approval, &fixture.conversation.id).await;
    assert!(
        id.is_none(),
        "same-tool session grant must short-circuit without a RequestId"
    );
    assert_eq!(
        fast_path.await.expect("fast-path resolution").outcome,
        ApprovalOutcome::Once
    );

    prove_proposal_stays_pending(&mut fixture, &host).await;
    approve_once(&mut fixture, &[&host]).await;
    host.stop().await;
}

#[tokio::test]
async fn persisted_always_tool_load_only_leaves_proposal_pending() {
    let (mut fixture, host) = fixture().await;
    let persistence = fixture.approval_persistence.clone() as Arc<dyn ApprovalPersistencePort>;
    let seeder = ApprovalRuntime::new(32, persistence);
    let (id, pending) = request_tool(&seeder, &fixture.conversation.id).await;
    let id = id.expect("seeder must create a pending approval before saving");
    seeder
        .resolve(
            &id,
            ApprovalOutcome::AlwaysAndSave {
                scope: ApprovalScope::Tool(TOOL.to_owned()),
            },
            rustain::domain::models::PermissionResolution::Operator,
        )
        .await;
    assert_eq!(
        pending.await.expect("saved pending approval").outcome,
        ApprovalOutcome::AlwaysAndSave {
            scope: ApprovalScope::Tool(TOOL.to_owned()),
        }
    );
    drop(seeder);

    let (id, before_load) = request_tool(&fixture.approval, &fixture.conversation.id).await;
    let id = id.expect("fixture runtime must have no grant before loading persisted state");
    fixture
        .approval
        .resolve(
            &id,
            ApprovalOutcome::Cancel,
            rustain::domain::models::PermissionResolution::Operator,
        )
        .await;
    assert_eq!(
        before_load.await.expect("cancelled pre-load probe").outcome,
        ApprovalOutcome::Cancel
    );

    fixture.approval.load_session().await;
    let (id, after_load) = request_tool(&fixture.approval, &fixture.conversation.id).await;
    assert!(
        id.is_none(),
        "loaded same-name persisted grant must short-circuit without a RequestId"
    );
    assert_eq!(
        after_load
            .await
            .expect("loaded fast-path resolution")
            .outcome,
        ApprovalOutcome::Once
    );

    prove_proposal_stays_pending(&mut fixture, &host).await;
    approve_once(&mut fixture, &[&host]).await;
    assert_eq!(host.counts(), HttpCounts { get: 0, post: 1 });
    host.stop().await;
}
