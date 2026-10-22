use crate::attached_harness::{AttachedClient, AttachedDaemon};
use crate::g_harness::*;
use crate::observed_host::ObservedHost;
use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::daemon::protocol::{ClientFrame, ConnectionTier};
use rustain::adapters::daemon::server::TeamSendTestHooks;
use rustain::domain::models::{
    AppConfig, ApprovalOutcome, ApprovalScope, ApprovalSource, AutoApprovePolicy, PermissionMode,
    ResponseMode, ToolRisk,
};
use serde_json::json;
use std::sync::Arc;

async fn pending(
    daemon: &AttachedDaemon,
    writer: &mut AttachedClient,
    host: &ObservedHost,
    diagnostic: &str,
) -> String {
    assert!(
        submit(daemon, writer, input())
            .await
            .contains("Proposal shown")
    );
    let id = proposal(writer).await;
    assert_eq!(daemon.proposals.pending().await.len(), 1, "{diagnostic}");
    zero(daemon, &[host], diagnostic).await;
    id
}
async fn mode(mode: PermissionMode) {
    let (daemon, mut writer, host) = fixture().await;
    daemon.runtime.security.set_mode(mode);
    let path = daemon.journal.workspace.path().join("live-mode-write.txt");
    let mut approvals = daemon.runtime.approval.subscribe();
    daemon.script.tool(
        "Write",
        json!({"file_path":path,"content":"live mode control"}),
    );
    writer
        .send_frame(ClientFrame::UserMessage {
            text: "write the requested file".into(),
            images: vec![],
        })
        .await;
    completed(&mut writer).await;
    assert_eq!(
        std::fs::read_to_string(path).expect("live mode Write"),
        "live mode control"
    );
    assert!(
        matches!(
            approvals.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ),
        "live mode positive control must Write without prompting"
    );
    let id = pending(
        &daemon,
        &mut writer,
        &host,
        "M10 attached: permission mode must not send a proposal",
    )
    .await;
    send_once(&daemon, &mut writer, &host, &id).await;
    daemon.stop().await;
    host.stop().await;
}
#[tokio::test]
async fn yolo_never_sends_without_human_decision() {
    mode(PermissionMode::Yolo).await;
}
#[tokio::test]
async fn autoedit_never_sends_without_human_decision() {
    mode(PermissionMode::AutoEdit).await;
}

async fn grant(persisted: bool) {
    let (daemon, mut writer, host) = fixture().await;
    let source = ApprovalSource::ForegroundTurn {
        conversation_id: "attached-fixture".into(),
    };
    let approval = if persisted {
        let persistence = Arc::new(
            rustain::adapters::approval_persistence_toml::ApprovalPersistenceToml::new(
                daemon
                    .journal
                    .workspace
                    .path()
                    .join(".rustain-test-home/config.toml"),
                daemon
                    .journal
                    .workspace
                    .path()
                    .join(".rustain/permissions.toml"),
            ),
        );
        rustain::domain::services::approval_runtime::ApprovalRuntime::new(32, persistence)
    } else {
        daemon.runtime.approval.clone()
    };
    let (id, response) = approval
        .request(
            source.clone(),
            "propose_team_message".into(),
            json!({}),
            ToolRisk::Elevated,
            None,
            None,
        )
        .await;
    approval
        .resolve(
            &id.expect("grant seeder must request approval"),
            if persisted {
                ApprovalOutcome::AlwaysAndSave {
                    scope: ApprovalScope::Tool("propose_team_message".into()),
                }
            } else {
                ApprovalOutcome::AlwaysTool {
                    tool_name: "propose_team_message".into(),
                }
            },
            rustain::domain::models::PermissionResolution::Operator,
        )
        .await;
    response.await.expect("real grant resolution");
    if persisted {
        daemon.runtime.approval.load_session().await;
    }
    let (id, response) = daemon
        .runtime
        .approval
        .request(
            source,
            "propose_team_message".into(),
            json!({}),
            ToolRisk::Elevated,
            None,
            None,
        )
        .await;
    assert!(
        id.is_none(),
        "live grant must short-circuit the real approval runtime"
    );
    assert_eq!(
        response.await.expect("grant fast path").outcome,
        ApprovalOutcome::Once
    );
    let id = pending(
        &daemon,
        &mut writer,
        &host,
        "M11 attached: session or persisted grant must never send proposal",
    )
    .await;
    send_once(&daemon, &mut writer, &host, &id).await;
    daemon.stop().await;
    host.stop().await;
}
#[tokio::test]
async fn session_grant_never_sends_without_human_decision() {
    grant(false).await;
}
#[tokio::test]
async fn persisted_grant_never_sends_without_human_decision() {
    grant(true).await;
}

#[tokio::test]
async fn detach_safe_gate_does_not_authorize_pending_proposal() {
    let (daemon, mut writer, host) = fixture().await;
    let id = pending(
        &daemon,
        &mut writer,
        &host,
        "M13 attached: generic approval must not authorize proposal",
    )
    .await;
    writer.detach().await;
    let mut reader = AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, true).await;
    fence(&mut reader, "detached-fence").await;
    let (request, response) = daemon
        .runtime
        .approval
        .request(
            ApprovalSource::ForegroundTurn {
                conversation_id: "attached-fixture".into(),
            },
            "detached-safe-control".into(),
            json!({}),
            ToolRisk::Safe,
            None,
            None,
        )
        .await;
    let response = tokio::time::timeout(crate::attached_harness::FIXTURE_TIMEOUT, response)
        .await
        .expect("daemon no-writer Safe gate resolves")
        .expect("Safe response");
    assert_eq!(
        response.outcome,
        ApprovalOutcome::Once,
        "live no-writer gate must auto-allow Safe"
    );
    assert!(
        request.is_some(),
        "Safe control must enter actual daemon approval gate"
    );
    assert_eq!(
        daemon.proposals.pending().await[0].id,
        id,
        "M13 attached: generic approval must not authorize proposal"
    );
    zero(
        &daemon,
        &[&host],
        "M13 attached: generic approval must not authorize proposal",
    )
    .await;
    let mut writer =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    assert_eq!(proposal(&mut writer).await, id);
    send_once(&daemon, &mut writer, &host, &id).await;
    daemon.stop().await;
    host.stop().await;
}

#[tokio::test]
async fn subagent_allow_policy_never_authorizes_parent_proposal() {
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut config = AppConfig::default();
    config.subagents.auto_approve = AutoApprovePolicy::Allow;
    let daemon = AttachedDaemon::start_scripted(
        vec![host.peer("lena-po")],
        config,
        true,
        TeamSendTestHooks::new(None, None, false),
    )
    .await;
    let mut writer =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    host.reset_counts();
    let path = daemon
        .journal
        .workspace
        .path()
        .join("subagent-policy-control.txt");
    let mut approvals = daemon.runtime.approval.subscribe();
    child(
        &daemon,
        "Write",
        json!({"file_path":path,"content":"real auto-approved child"}),
    )
    .await;
    assert_eq!(
        std::fs::read_to_string(path).expect("child Write"),
        "real auto-approved child"
    );
    assert!(
        matches!(
            approvals.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ),
        "live subagent allow policy must permit Write without a prompt"
    );
    let id = pending(
        &daemon,
        &mut writer,
        &host,
        "M12 attached: subagent policy must not send parent proposal",
    )
    .await;
    send_once(&daemon, &mut writer, &host, &id).await;
    daemon.stop().await;
    host.stop().await;
}

#[tokio::test]
async fn real_subagent_cannot_raise_on_attached_parent_surface() {
    let (daemon, mut writer, host) = fixture().await;
    let mut transitions = daemon.runtime.tool_scheduler.subscribe();
    child(&daemon, "propose_team_message", input()).await;
    let calls: Vec<_> = std::iter::from_fn(|| transitions.try_recv().ok()).collect();
    assert!(calls.iter().any(|event|matches!(&event.call,rustain::domain::models::ToolCall::Error {request,error,..}
        if request.tool_name=="propose_team_message" && error.contains("only available to the operator"))),
        "M02 attached: real subagent proposal must be denied");
    assert!(
        daemon.proposals.pending().await.is_empty(),
        "M02 attached: real subagent proposal must be denied"
    );
    zero(
        &daemon,
        &[&host],
        "M02 attached: real subagent proposal must be denied",
    )
    .await;
    let id = pending(
        &daemon,
        &mut writer,
        &host,
        "parent foreground positive control",
    )
    .await;
    send_once(&daemon, &mut writer, &host, &id).await;
    daemon.stop().await;
    host.stop().await;
}

#[tokio::test]
async fn selected_peer_context_never_authorizes_proposal() {
    use rustain::adapters::daemon::protocol::DaemonFrame;
    use rustain::domain::models::{JournalRecord, RoomEvent};
    let (daemon, mut writer, host, inbound) = crate::g_k05_liveness::fixture_with_remote().await;
    const MARKER: &str = "SELECTED_PEER_CONTEXT_MUST_NOT_LEAK_f3d1";
    let received = inbound
        .sender("remote-drafter")
        .message_send("selected-peer-context", MARKER)
        .await;
    assert!(received.json["error"].is_null());
    daemon.wait_for_received_task("selected-peer-context").await;
    crate::g_k05_liveness::peer_ready(&daemon).await;
    let item = daemon
        .journal
        .entries()
        .await
        .into_iter()
        .find_map(|entry| match entry.record {
            JournalRecord::Room(RoomEvent::RecipientItemReceived {
                address, content, ..
            }) if content == MARKER => Some(address.item().as_str().to_owned()),
            _ => None,
        })
        .expect("real received item address");
    writer
        .send_frame(ClientFrame::SetRecipientContext {
            item_id: item.clone(),
            selected: true,
        })
        .await;
    writer.wait_for(|frame|matches!(frame,DaemonFrame::RecipientContextState {selected_item_ids,..} if selected_item_ids.contains(&item))).await;
    daemon.runtime.security.set_mode(PermissionMode::AutoEdit);
    let path = daemon
        .journal
        .workspace
        .path()
        .join("tainted-write-control.txt");
    daemon.script.tool(
        "Write",
        json!({"file_path":path,"content":"explicit approved tainted Write"}),
    );
    writer
        .send_frame(ClientFrame::UserMessage {
            text: "write the requested file after using selected context".into(),
            images: vec![],
        })
        .await;
    let request = match writer
        .wait_for(|f| matches!(f,DaemonFrame::ApprovalRequest {tool,..} if tool=="Write"))
        .await
    {
        DaemonFrame::ApprovalRequest { request_id, .. } => request_id,
        _ => unreachable!(),
    };
    writer
        .send_frame(ClientFrame::ApprovalResponse {
            request_id: request,
            outcome: ApprovalOutcome::Once,
        })
        .await;
    completed(&mut writer).await;
    assert_eq!(
        std::fs::read_to_string(path).expect("tainted Write control"),
        "explicit approved tainted Write"
    );
    let id = pending(
        &daemon,
        &mut writer,
        &host,
        "M15 attached: selected peer context must not authorize proposal",
    )
    .await;
    assert!(
        daemon
            .requests
            .lock()
            .iter()
            .any(|(messages, _)| messages.iter().any(|message| message
                .context_prefix
                .as_ref()
                .is_some_and(|prefix| prefix.contains(MARKER)))),
        "selected peer item must actually reach the real model context"
    );
    send_once(&daemon, &mut writer, &host, &id).await;
    assert!(
        !String::from_utf8_lossy(host.posts()[0].body()).contains(MARKER),
        "M34 attached: selected peer context must never reach recipient"
    );
    inbound.stop().await;
    daemon.stop().await;
    host.stop().await;
}
