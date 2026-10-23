use super::attached_harness::{AttachedClient, AttachedDaemon};
use super::g_harness::*;
use rustain::adapters::daemon::protocol::{AttachMode, ClientFrame, ConnectionTier, DaemonFrame};
use rustain::adapters::daemon::server::TeamSendTestHooks;
use rustain::domain::models::{AppConfig, PermissionMode};
use serde_json::json;

async fn refused(input_value: serde_json::Value, diagnostic: &str) {
    let (daemon, mut writer, host) = fixture().await;
    let result = submit(&daemon, &mut writer, input_value).await;
    assert!(
        !result.contains("Proposal shown to the operator."),
        "{diagnostic}"
    );
    assert!(daemon.proposals.pending().await.is_empty(), "{diagnostic}");
    assert!(
        !writer
            .received
            .iter()
            .any(|f| matches!(f, DaemonFrame::TeamProposal { .. })),
        "{diagnostic}"
    );
    zero(&daemon, &[&host], diagnostic).await;
    assert!(
        submit(&daemon, &mut writer, input())
            .await
            .contains("Proposal shown to the operator."),
        "valid attached proposal positive control"
    );
    let id = proposal(&mut writer).await;
    send_once(&daemon, &mut writer, &host, &id).await;
    daemon.stop().await;
    host.stop().await;
}
#[tokio::test]
async fn plain_writer_refuses_at_raise_then_same_connection_can_opt_in() {
    let (daemon, writer, host) = fixture().await;
    writer.detach().await;
    let mut plain =
        AttachedClient::attach_without_proposals(&daemon.path, ConnectionTier::TrustedLocal, false)
            .await;
    assert_eq!(
        plain.granted,
        AttachMode::ReadWrite,
        "plain attach remains an authenticated ordinary writer"
    );
    let result = submit(&daemon, &mut plain, input()).await;
    assert!(
        result.contains(
            "Not proposed: no operator surface is attached to approve it. Nothing was sent."
        ),
        "plain writer must receive R2 at raise, not a wedged pending approval: {result}"
    );
    plain
        .send_frame(ClientFrame::HistoryRequest {
            before_index: None,
            count: 1,
        })
        .await;
    plain
        .wait_for(|frame| matches!(frame, DaemonFrame::History { .. }))
        .await;
    assert!(daemon.proposals.pending().await.is_empty());
    assert!(
        !plain.received.iter().any(|frame| matches!(
            frame,
            DaemonFrame::TeamProposal { .. } | DaemonFrame::TeamProposalClosed { .. }
        )),
        "plain writer must receive zero proposal lifecycle frames"
    );
    zero(
        &daemon,
        &[&host],
        "plain refusal must have zero send effects",
    )
    .await;

    let before = daemon.provider_request_count();
    plain
        .send_frame(ClientFrame::UserMessage {
            text: "Continue an ordinary stdin turn without proposals.".into(),
            images: vec![],
        })
        .await;
    completed(&mut plain).await;
    assert!(
        daemon.provider_request_count() > before,
        "R2 must not stop subsequent ordinary stdin turns"
    );
    plain.enable_team_proposals().await;
    assert!(
        submit(&daemon, &mut plain, input())
            .await
            .contains("Proposal shown to the operator."),
        "same authenticated socket must become capable only after explicit opt-in"
    );
    let id = proposal(&mut plain).await;
    send_once(&daemon, &mut plain, &host, &id).await;
    daemon.stop().await;
    host.stop().await;
}
#[tokio::test]
async fn attached_unknown_alias_never_raises() {
    let mut value = input();
    value["recipients"] = json!(["unknown"]);
    refused(value, "M05 attached: unknown alias raised a proposal").await;
}
#[tokio::test]
async fn attached_missing_type_never_raises() {
    let mut value = input();
    value.as_object_mut().unwrap().remove("type");
    refused(value, "M06 attached: untyped proposal raised a frame").await;
}
#[tokio::test]
async fn attached_duplicate_alias_never_raises() {
    let mut value = input();
    value["recipients"] = json!(["lena-po", "lena-po"]);
    refused(
        value,
        "duplicate aliases must be refused before publication",
    )
    .await;
}
#[tokio::test]
async fn attached_body_limit_never_raises() {
    let mut value = input();
    value["body"] = json!("x".repeat(131_073));
    refused(value, "M07 attached: oversized body raised a proposal").await;
}
#[tokio::test]
async fn attached_bidi_control_never_raises() {
    let mut value = input();
    value["body"] = json!("hidden\u{202e}operator");
    refused(value, "M07 attached: bidi control raised a proposal").await;
}
#[tokio::test]
async fn attached_plan_mode_never_raises() {
    let (daemon, mut writer, host) = fixture().await;
    daemon.runtime.security.set_mode(PermissionMode::Plan);
    let result = submit(&daemon, &mut writer, input()).await;
    assert!(
        result.contains("propose_team_message is only available") || result.contains("Plan"),
        "M04 attached: Plan proposal was admitted: {result}"
    );
    assert!(
        daemon.proposals.pending().await.is_empty(),
        "M04 attached: Plan proposal was admitted"
    );
    zero(
        &daemon,
        &[&host],
        "M04 attached: Plan proposal was admitted",
    )
    .await;
    daemon.runtime.security.set_mode(PermissionMode::Normal);
    assert!(
        submit(&daemon, &mut writer, input())
            .await
            .contains("Proposal shown")
    );
    let id = proposal(&mut writer).await;
    send_once(&daemon, &mut writer, &host, &id).await;
    daemon.stop().await;
    host.stop().await;
}
#[tokio::test]
async fn attached_second_raise_preserves_first_snapshot() {
    let (daemon, mut writer, host) = fixture().await;
    submit(&daemon, &mut writer, input()).await;
    let id = proposal(&mut writer).await;
    let original = daemon.proposals.pending().await;
    let mut value = input();
    value["body"] = json!("replacement must never win");
    let result = submit(&daemon, &mut writer, value).await;
    assert!(
        result.contains("an earlier proposal is still awaiting"),
        "M08 attached: second raise superseded pending proposal"
    );
    let pending = daemon.proposals.pending().await;
    assert_eq!(
        pending.len(),
        1,
        "M08 attached: second raise superseded pending proposal"
    );
    assert_eq!(
        pending[0].id, original[0].id,
        "M08 attached: second raise superseded pending proposal"
    );
    assert_eq!(
        pending[0].body, BODY,
        "M08 attached: second raise superseded pending proposal"
    );
    zero(&daemon, &[&host], "pending proposals must not send").await;
    send_once(&daemon, &mut writer, &host, &id).await;
    daemon.stop().await;
    host.stop().await;
}
#[tokio::test]
async fn attached_catalogue_requires_egress() {
    for install in [false, true] {
        let daemon = AttachedDaemon::start_scripted(
            vec![],
            AppConfig::default(),
            install,
            TeamSendTestHooks::new(None, None, false),
        )
        .await;
        let tools = daemon.runtime.tools.available_tools();
        assert_eq!(
            tools.iter().any(|tool| tool.name == "propose_team_message"),
            install,
            "M39 attached: proposal catalogue must match composed egress"
        );
        assert!(
            !daemon
                .runtime
                .tools
                .describe()
                .iter()
                .any(|tool| tool.name == "propose_team_message"),
            "proposal must remain absent from discovery"
        );
        daemon.stop().await;
    }
}
async fn no_surface(tier: ConnectionTier, diagnostic: &str) {
    let (daemon, writer, host) = fixture().await;
    writer.detach().await;
    let mut observer = AttachedClient::attach(&daemon.path, tier, true).await;
    fence(&mut observer, "peer-no-surface-fence").await;
    daemon.script.tool("propose_team_message", input());
    let mut conversation = daemon.conversation.lock().await;
    let task = daemon
        .runtime
        .drive_turn(
            "operator request without an attached approval surface".into(),
            rustain::domain::models::ChannelKind::Terminal,
            &mut conversation,
            &daemon.domain_tx,
            rustain::domain::models::TurnOrigin::Interactive,
            tokio_util::sync::CancellationToken::new(),
        )
        .await;
    drop(conversation);
    task.await.expect("unattended production turn completes");
    assert!(
        daemon
            .requests
            .lock()
            .iter()
            .flat_map(|(messages, _)| messages.iter())
            .flat_map(|message| message.tool_results.iter())
            .any(|result| result.content.contains("no operator surface is attached")),
        "{diagnostic}"
    );
    assert!(daemon.proposals.pending().await.is_empty(), "{diagnostic}");
    zero(&daemon, &[&host], diagnostic).await;
    let mut writer =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    assert!(
        submit(&daemon, &mut writer, input())
            .await
            .contains("Proposal shown")
    );
    let id = proposal(&mut writer).await;
    send_once(&daemon, &mut writer, &host, &id).await;
    daemon.stop().await;
    host.stop().await;
}

#[tokio::test]
async fn attached_no_writer_raise_returns_r2_without_retention() {
    no_surface(
        ConnectionTier::Peer,
        "M27 attached: no-writer raise must return R2",
    )
    .await;
}
#[tokio::test]
async fn readonly_only_is_not_an_operator_surface() {
    no_surface(
        ConnectionTier::TrustedLocal,
        "M46: read-only-only daemon must refuse raise with R2",
    )
    .await;
}

#[tokio::test]
async fn legacy_scheduler_wrapper_cannot_infer_interactive_authority() {
    use rustain::domain::models::{ApprovalSource, ToolCall, ToolCallRequest};
    let (daemon, mut writer, host) = fixture().await;
    let result = daemon
        .runtime
        .tool_scheduler
        .clone()
        .schedule(
            ApprovalSource::ForegroundTurn {
                conversation_id: "attached-fixture".into(),
            },
            vec![ToolCallRequest {
                id: "legacy-wrapper-proposal".into(),
                tool_name: "propose_team_message".into(),
                input: input(),
            }],
            tokio_util::sync::CancellationToken::new(),
            None,
        )
        .await;
    assert!(
        matches!(&result[0],ToolCall::Error {error,..} if error.contains("only available to the operator")),
        "M41 attached: wrapper route None must never gain interactive authority"
    );
    assert!(
        daemon.proposals.pending().await.is_empty(),
        "M41 attached: wrapper route None must never gain interactive authority"
    );
    zero(
        &daemon,
        &[&host],
        "M41 attached: wrapper route None must never gain interactive authority",
    )
    .await;
    submit(&daemon, &mut writer, input()).await;
    let id = proposal(&mut writer).await;
    send_once(&daemon, &mut writer, &host, &id).await;
    daemon.stop().await;
    host.stop().await;
}
