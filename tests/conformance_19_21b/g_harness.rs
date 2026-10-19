use crate::attached_harness::{AttachedClient, AttachedDaemon};
use crate::observed_host::{HttpCounts, ObservedHost};
use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::daemon::protocol::{ClientFrame, ConnectionTier, DaemonFrame};
use rustain::domain::models::{
    ResponseMode, RoomEvent, StopReason, StreamChunk, TeamProposalDecision,
};
use rustain::infrastructure::runtime::event_bus::RawEventKind;
use serde_json::{Value, json};

pub const BODY: &str = "Architecture note — café 🛰️\n\tsecond row\nlast row \t ";
pub fn input() -> Value {
    json!({"recipients":["lena-po"],"type":"architecture_update","body":BODY})
}
pub async fn fixture() -> (AttachedDaemon, AttachedClient, ObservedHost) {
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let daemon = AttachedDaemon::start(vec![host.peer("lena-po")]).await;
    let writer = AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    assert!(
        host.counts().get > 0,
        "real recipient card warm-up must complete"
    );
    host.reset_counts();
    (daemon, writer, host)
}
pub async fn completed(client: &mut AttachedClient) {
    client.wait_for(|frame| matches!(frame,
        DaemonFrame::Event(event) if matches!(&event.kind,
            RawEventKind::Provider(StreamChunk::TurnComplete { stop_reason }) if *stop_reason != StopReason::ToolUse)
    )).await;
}
pub async fn submit(daemon: &AttachedDaemon, writer: &mut AttachedClient, input: Value) -> String {
    let before = daemon.provider_request_count();
    let tool_use_id = daemon.script.tool("propose_team_message", input);
    writer
        .send_frame(ClientFrame::UserMessage {
            text: "Please propose the exact supplied message; never send it.".into(),
            images: vec![],
        })
        .await;
    // A direct/unattended predecessor can leave an already-emitted completion
    // in the forwarder. Fence the newly queued tool exchange, not that old event.
    loop {
        completed(writer).await;
        if daemon.provider_request_count() >= before + 2 {
            break;
        }
    }
    daemon.requests.lock()[before..]
        .iter()
        .flat_map(|(messages, _)| messages.iter())
        .flat_map(|message| message.tool_results.iter())
        .find(|result| result.tool_use_id == tool_use_id)
        .expect("the current scripted proposal must reach its model continuation")
        .content
        .clone()
}
pub async fn proposal(writer: &mut AttachedClient) -> String {
    match writer
        .wait_for(|f| matches!(f, DaemonFrame::TeamProposal { .. } | DaemonFrame::Error(_)))
        .await
    {
        DaemonFrame::TeamProposal { proposal, .. } => proposal,
        DaemonFrame::Error(error) => {
            panic!("waiting proposal was replaced by a protocol error: {error:?}")
        }
        _ => unreachable!(),
    }
}
pub async fn decide(writer: &mut AttachedClient, id: &str, decision: TeamProposalDecision) {
    writer
        .send_frame(ClientFrame::TeamProposalDecision {
            proposal: id.into(),
            decision,
        })
        .await;
}
pub async fn fence(writer: &mut AttachedClient, key: &str) {
    writer.team_status(key).await;
    let _ = writer.wait_status_result(key).await;
}
pub async fn zero(daemon: &AttachedDaemon, hosts: &[&ObservedHost], diagnostic: &str) {
    for host in hosts {
        assert_eq!(
            host.counts(),
            HttpCounts { get: 0, post: 0 },
            "{diagnostic}"
        );
    }
    let events = crate::harness::read_room_events(&daemon.journal).await;
    assert!(
        !events.iter().any(|(_, event)| matches!(
            event,
            RoomEvent::TeamSendActionRecorded { .. } | RoomEvent::RemoteEnvelopeDispatched { .. }
        )),
        "{diagnostic}"
    );
}
pub async fn send_once(
    daemon: &AttachedDaemon,
    writer: &mut AttachedClient,
    host: &ObservedHost,
    id: &str,
) {
    decide(writer, id, TeamProposalDecision::Send).await;
    assert!(
        matches!(
            writer.wait_send_started(id).await,
            DaemonFrame::TeamSendStarted { .. }
        ),
        "human Send must enter canonical send front door"
    );
    assert!(
        matches!(
            writer.wait_send_settled(id, 0).await,
            DaemonFrame::TeamSendSettled { .. }
        ),
        "human Send positive control must settle"
    );
    assert_eq!(
        host.counts(),
        HttpCounts { get: 0, post: 1 },
        "human Send must POST exactly once"
    );
    let events = crate::harness::read_room_events(&daemon.journal).await;
    assert_eq!(
        events
            .iter()
            .filter(|(_, e)| matches!(e, RoomEvent::TeamSendActionRecorded { .. }))
            .count(),
        1,
        "human Send must record one action"
    );
}

pub async fn child(daemon: &AttachedDaemon, name: &'static str, value: Value) {
    use rustain::domain::models::{AgentId, AgentLaunchSpec, ModelTier, SandboxPolicy, ToolPolicy};
    use rustain::domain::ports::SubagentRunner;
    use std::sync::Arc;
    let workspace = daemon.journal.workspace.path().to_path_buf();
    let authority = rustain::domain::models::CapabilityToken::r1_root(AgentId::root());
    let ledger = Arc::new(
        rustain::domain::services::authority_ledger::AuthorityLedger::new(
            authority.clone(),
            Arc::new(rustain::domain::clock::SystemClock::default()),
        ),
    );
    let (raw_tx, _) = tokio::sync::broadcast::channel(128);
    let bus = Arc::new(rustain::infrastructure::runtime::event_bus::EventBus {
        domain_tx: daemon.domain_tx.clone(),
        raw_tx,
    });
    let runner = rustain::adapters::subagent::InProcessSubagentRunner::new(
        crate::tool_provider::tool_provider(name, value),
        daemon.runtime.storage.clone(),
        daemon.runtime.security.clone(),
        daemon.runtime.tools.clone(),
        daemon.runtime.approval.clone(),
        daemon.runtime.tool_scheduler.clone(),
        bus,
        Arc::new(rustain::infrastructure::subagent::NodeTree::new()),
        Arc::new(tokio::sync::RwLock::new(SandboxPolicy::Permissive)),
        Arc::new(
            rustain::infrastructure::subagent::SubagentSpool::new(
                workspace.join(".rustain/subagent-spool"),
            )
            .await
            .expect("child spool"),
        ),
        Arc::new(rustain::adapters::authority::InProcessAuthorityProvider::new(ledger)),
        authority,
    );
    let child = runner
        .launch(
            AgentLaunchSpec {
                prompt: "execute the scripted tool".into(),
                effective_model: "scripted".into(),
                tier: ModelTier::CheapAgentic,
                tools_allow: ToolPolicy::InheritFromParent,
                parent_ctx_tokens: 0,
                sandbox_override: None,
                parent_trace: None,
                isolated: false,
                delegation: rustain::domain::models::launch_spec::DelegationProfile::Child,
            },
            tokio_util::sync::CancellationToken::new(),
            None,
            AgentId::new(),
        )
        .await
        .expect("real child launch");
    let mut status = child.status_rx;
    tokio::time::timeout(crate::attached_harness::FIXTURE_TIMEOUT, async {
        while let Some(state) = status.recv().await {
            if state.is_terminal() {
                assert_eq!(
                    state,
                    rustain::domain::models::NodeState::Completed,
                    "real subagent turn completes"
                );
                return;
            }
        }
        panic!("child status closed without terminal completion");
    })
    .await
    .expect("real child completion");
}
