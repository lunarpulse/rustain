use std::sync::Arc;

use rustain::domain::models::{
    AgentId, AgentLaunchSpec, AutoApprovePolicy, ModelTier, SandboxPolicy, ToolCall, ToolPolicy,
};
use rustain::domain::ports::{SecurityPort, StreamingProvider, SubagentRunner};
use rustain::infrastructure::subagent::{NodeTree, SubagentSpool};
use tokio_util::sync::CancellationToken;

use super::k02_matrix::{approve_once, assert_pending_without_egress};
use super::observed_host::ObservedHost;
use super::proposal_harness::ProposalFixture;
use super::tool_provider::tool_provider;

#[tokio::test]
async fn allow_subagent_write_does_not_auto_send_parent_proposal() {
    let host = ObservedHost::start(
        rustain::adapters::a2a::admission::A2aAdmissionPolicy::Allow,
        rustain::domain::models::ResponseMode::NotifyAndWait,
    )
    .await;
    let peers = vec![host.peer("lena-po")];
    let mut fixture = ProposalFixture::start_with_policy(peers, AutoApprovePolicy::Allow).await;
    host.reset_counts();
    let mut approvals = fixture.approval.subscribe();
    assert_eq!(
        fixture.security.current_mode(),
        rustain::domain::models::PermissionMode::Normal
    );

    assert_eq!(
        run_child(
            &fixture,
            tool_provider(
                "Write",
                serde_json::json!({"file_path":"k02-child.txt","content":"written by real child"}),
            ),
            "write the requested file",
        )
        .await,
        rustain::domain::models::NodeState::Completed
    );
    assert_eq!(
        std::fs::read_to_string(
            fixture
                .sender
                .journal
                .workspace
                .path()
                .join("k02-child.txt")
        )
        .expect("child Write output"),
        "written by real child"
    );
    assert!(
        matches!(
            approvals.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ),
        "auto-approved child Write must not request human approval"
    );

    let proposal = serde_json::json!({
        "recipients": ["lena-po"],
        "type": "architecture_update",
        "body": "Proposal remains human-controlled."
    });
    assert_eq!(
        fixture.submit(proposal).await,
        "Proposal shown to the operator. Nothing has been sent; the operator decides whether to send it to lena-po."
    );
    assert!(
        fixture
            .state
            .pending_team_proposals
            .contains_key(&fixture.conversation.id),
        "proposal remains pending until a human Send key"
    );
    assert_pending_without_egress(&fixture, &[&host]).await;
    approve_once(&mut fixture, &[&host]).await;
    host.stop().await;
}

#[tokio::test]
async fn k01_subagent_proposal_is_denied_by_real_runner() {
    for mode in [
        rustain::domain::models::PermissionMode::Normal,
        rustain::domain::models::PermissionMode::Yolo,
        rustain::domain::models::PermissionMode::AutoEdit,
    ] {
        let host = ObservedHost::start(
            rustain::adapters::a2a::admission::A2aAdmissionPolicy::Allow,
            rustain::domain::models::ResponseMode::NotifyAndWait,
        )
        .await;
        let mut fixture = ProposalFixture::start_with_policy(
            vec![host.peer("lena-po")],
            AutoApprovePolicy::Allow,
        )
        .await;
        fixture.set_mode(mode);
        host.reset_counts();
        let input = serde_json::json!({
            "recipients":["lena-po"],
            "type":"architecture_update",
            "body":"Only the foreground owner may propose this message."
        });
        let mut transitions = fixture.scheduler.subscribe();
        let mut approvals = fixture.approval.subscribe();
        assert_eq!(
            run_child(
                &fixture,
                tool_provider("propose_team_message", input.clone()),
                "propose the requested team message",
            )
            .await,
            rustain::domain::models::NodeState::Completed
        );
        let calls: Vec<_> = std::iter::from_fn(|| transitions.try_recv().ok()).collect();
        assert!(
            calls.iter().any(|transition| matches!(
                &transition.call,
                ToolCall::Error { request, .. } if request.tool_name == "propose_team_message"
            )),
            "subagent proposal must fail at the permission boundary in {mode:?}: {calls:?}"
        );
        assert!(
            !calls.iter().any(|transition| matches!(
                &transition.call,
                ToolCall::Executing { request, .. } if request.tool_name == "propose_team_message"
            )),
            "a denied subagent proposal must never execute"
        );
        assert!(
            fixture.state.pending_team_proposals.is_empty(),
            "a child turn cannot install a proposal on its parent surface"
        );
        assert!(
            matches!(
                approvals.try_recv(),
                Err(tokio::sync::broadcast::error::TryRecvError::Empty)
            ),
            "a denied child proposal must not request generic approval"
        );
        assert_eq!(
            fixture.submit(input).await,
            "Proposal shown to the operator. Nothing has been sent; the operator decides whether to send it to lena-po.",
            "the identical valid proposal must remain available to its foreground owner"
        );
        fixture.paint(60, 16);
        fixture
            .raw_key(crossterm::event::Event::Key(
                crossterm::event::KeyEvent::new(
                    crossterm::event::KeyCode::Char('n'),
                    crossterm::event::KeyModifiers::NONE,
                ),
            ))
            .await;
        assert!(fixture.state.pending_team_proposals.is_empty());
        assert!(fixture.state.team_send_blocks.is_empty());
        assert!(fixture.sender.rows().await.is_empty());
        assert!(host.journal().entries().await.is_empty());
        assert_eq!(
            host.counts(),
            super::observed_host::HttpCounts { get: 0, post: 0 }
        );
        host.stop().await;
    }
}

async fn run_child(
    fixture: &ProposalFixture,
    provider: Arc<dyn StreamingProvider>,
    prompt: &str,
) -> rustain::domain::models::NodeState {
    let workspace = fixture.sender.journal.workspace.path().to_path_buf();
    let storage: Arc<dyn rustain::domain::ports::StoragePort> = Arc::new(
        rustain::adapters::filesystem::FileSystemStorage::with_workspace_root(
            rustain::infrastructure::paths::sessions_dir(&workspace),
            workspace.clone(),
        ),
    );
    let security = fixture.security.clone() as Arc<dyn SecurityPort>;
    let tools = fixture.tools.clone() as Arc<dyn rustain::domain::ports::ToolSetPort>;
    let root_authority = rustain::domain::models::CapabilityToken::r1_root(AgentId::root());
    let authority_ledger = Arc::new(
        rustain::domain::services::authority_ledger::AuthorityLedger::new(
            root_authority.clone(),
            Arc::new(rustain::domain::clock::SystemClock::default()),
        ),
    );
    let runner = rustain::adapters::subagent::InProcessSubagentRunner::new(
        provider,
        storage,
        security,
        tools,
        fixture.approval.clone(),
        fixture.scheduler.clone(),
        fixture.sender.app_state.event_bus.clone(),
        Arc::new(NodeTree::new()),
        Arc::new(tokio::sync::RwLock::new(SandboxPolicy::Permissive)),
        Arc::new(
            SubagentSpool::new(workspace.join(".rustain/subagent-spool"))
                .await
                .expect("spool"),
        ),
        Arc::new(rustain::adapters::authority::InProcessAuthorityProvider::new(authority_ledger)),
        root_authority,
    );
    let child = runner
        .launch(
            AgentLaunchSpec {
                prompt: prompt.into(),
                effective_model: "scripted".into(),
                tier: ModelTier::CheapAgentic,
                tools_allow: ToolPolicy::InheritFromParent,
                parent_ctx_tokens: 0,
                sandbox_override: None,
                parent_trace: None,
                isolated: false,
                delegation: rustain::domain::models::launch_spec::DelegationProfile::Child,
            },
            CancellationToken::new(),
            None,
            AgentId::new(),
        )
        .await
        .expect("launch child");
    let mut status_rx = child.status_rx;
    tokio::time::timeout(std::time::Duration::from_secs(45), async {
        while let Some(status) = status_rx.recv().await {
            if status.is_terminal() {
                return status;
            }
        }
        panic!("child status channel closed before terminal state");
    })
    .await
    .expect("child reaches terminal state")
}
