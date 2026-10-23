use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::domain::models::ResponseMode;
use serde_json::json;

use super::k02_matrix::{approve_once, assert_pending_without_egress};
use super::observed_host::ObservedHost;
use super::proposal_harness::ProposalFixture;

#[tokio::test]
async fn private_ready_reaches_local_card_but_never_raw_observers() {
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut fixture = ProposalFixture::start(vec![host.peer("lena-po")]).await;
    let mut raw = fixture.sender.app_state.event_bus.subscribe_raw();
    host.reset_counts();
    fixture
        .submit(json!({
            "recipients": ["lena-po"], "type": "architecture_update",
            "body": "PRIVATE_PROPOSAL_47: only this conversation may see the pending draft"
        }))
        .await;
    assert_pending_without_egress(&fixture, &[&host]).await;
    assert!(
        matches!(
            raw.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ),
        "private proposal-ready event must not reach raw observers"
    );
    // The local production consumer really retained a usable proposal: this is
    // not a tool/sink that silently discarded it to satisfy the privacy check.
    approve_once(&mut fixture, &[&host]).await;
    host.stop().await;
}
