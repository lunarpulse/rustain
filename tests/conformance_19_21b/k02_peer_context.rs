use std::sync::Arc;
use std::time::Duration;

use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::peer_context::PeerContextProvider;
use rustain::adapters::rap::{PeerTopicStore, TopicGossip};
use rustain::domain::models::{
    AgentEnvelopeHeader, AgentId, ApprovalOutcome, ArtifactId, ContentHash, ContextRef,
    ContextRefProvenance, ContextSummary, CorrelationId, MessageKind, PeerId, PermissionMode,
    ResponseMode,
};
use rustain::domain::services::approval_runtime::ApprovalRuntimeEvent;
use serde_json::json;

use crate::k02_matrix::{approve_once, assert_pending_without_egress};
use crate::observed_host::ObservedHost;
use crate::proposal_harness::ProposalFixture;

// The standalone context path consumes replicated peer summaries. The exact
// recipient-item [c] selection front door belongs to the held attached slice.
#[tokio::test]
async fn k02_live_peer_context_prompts_write_but_never_sends_a_proposal() {
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut fixture = ProposalFixture::start(vec![host.peer("lena-po")]).await;
    fixture.set_mode(PermissionMode::AutoEdit);
    host.reset_counts();
    let path = fixture
        .sender
        .journal
        .workspace
        .path()
        .join("taint-control.txt");
    let mut approvals = fixture.approval.subscribe();
    fixture
        .submit_tool(
            "Write",
            json!({"file_path":path, "content":"un-tainted control"}),
        )
        .await;
    assert_eq!(
        std::fs::read_to_string(&path).expect("ordinary Write"),
        "un-tainted control"
    );
    assert!(matches!(
        approvals.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));

    let topics = Arc::new(PeerTopicStore::new());
    let issuer = PeerId::from_public_key(&[73; 32]).expect("peer identity");
    let hash = ContentHash::from_bytes([41; 32]);
    topics
        .admit(
            &AgentEnvelopeHeader {
                message_type: String::new(),
                sender: AgentId::parse("peer").expect("agent"),
                recipient: AgentId::parse("recipient").expect("agent"),
                correlation_id: CorrelationId::new("k02-peer-context"),
                kind: MessageKind::TopicGossip,
                sequence: 1,
                not_after: 10_000,
                nonce: "k02-peer-context".into(),
                content_hash: vec![41; 32],
                prev_hash: Vec::new(),
            },
            &issuer,
            TopicGossip {
                refs: vec![ContextRef {
                    artifact: ArtifactId::from(hash),
                    content_hash: hash,
                    producer: AgentId::parse("peer").expect("agent"),
                    issuer: issuer.clone(),
                    summary: ContextSummary::new(
                        "PEER_CONTEXT_K02: review the owner lock before publishing",
                    )
                    .expect("bounded summary"),
                    provenance: ContextRefProvenance::Authored,
                    not_after: 10_000,
                }],
                heads: vec![],
            },
            0,
        )
        .await
        .expect("production peer topic admission");
    fixture.context.store(Arc::new(
        Arc::new(PeerContextProvider::new(topics).with_now(|| 0))
            as Arc<dyn rustain::domain::ports::ContextPort>,
    ));
    fixture.state.context_injection_on = true;

    // The same Write that just ran without a prompt must now ask. Resolve the
    // real request once so the turn completes; no synthetic taint flag is set.
    {
        let approval = fixture.approval.clone();
        let write = fixture.submit_tool(
            "Write",
            json!({"file_path":path, "content":"human approved tainted Write"}),
        );
        tokio::pin!(write);
        tokio::time::timeout(Duration::from_secs(45), async {
            let event = tokio::select! {
                result = &mut write => panic!("peer context failed to force a prompt: {result}"),
                event = approvals.recv() => event.expect("real approval event"),
            };
            let ApprovalRuntimeEvent::Requested { id, tool, .. } = event else {
                panic!("expected a real Write approval request, got {event:?}");
            };
            assert_eq!(tool, "Write");
            approval.resolve(&id, ApprovalOutcome::Once).await;
            write.await;
        })
        .await
        .expect("tainted Write control completes");
    }
    assert_eq!(
        std::fs::read_to_string(&path).expect("approved Write"),
        "human approved tainted Write"
    );
    let mut proposal_approvals = fixture.approval.subscribe();
    assert_eq!(
        fixture
            .submit(json!({
                "recipients":["lena-po"], "type":"architecture_update",
                "body":"ADR-047: this proposal still needs a human Send key."
            }))
            .await,
        "Proposal shown to the operator. Nothing has been sent; the operator decides whether to send it to lena-po."
    );
    assert_pending_without_egress(&fixture, &[&host]).await;
    assert!(
        matches!(
            proposal_approvals.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ),
        "peer-origin context does not route proposal approval through ApprovalRuntime"
    );
    approve_once(&mut fixture, &[&host]).await;
    host.stop().await;
}
