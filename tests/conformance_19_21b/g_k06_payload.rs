use crate::attached_harness::{AttachedClient, AttachedDaemon};
use crate::g_harness::*;
use crate::observed_host::{HttpCounts, ObservedHost};
use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::daemon::protocol::{ClientFrame, ConnectionTier, DaemonFrame};
use rustain::domain::models::{
    JournalRecord, ResponseMode, RoomEvent, SemanticMessageType, TeamProposalDecision,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

fn hash(body: &str) -> String {
    let digest: [u8; 32] = Sha256::digest(serde_json::to_vec(body).unwrap()).into();
    rustain::domain::models::ContentHash::from_bytes(digest).to_string()
}
async fn facts(daemon: &AttachedDaemon) -> Vec<Value> {
    daemon
        .journal
        .entries()
        .await
        .into_iter()
        .filter_map(|entry| {
            let JournalRecord::Room(event) = entry.record else {
                return None;
            };
            matches!(
                event,
                RoomEvent::TeamSendActionRecorded { .. }
                    | RoomEvent::RemoteEnvelopeDispatched { .. }
                    | RoomEvent::TeamSendRecipientSettled { .. }
            )
            .then(|| serde_json::to_value(event).unwrap())
        })
        .collect()
}
fn normalize(mut facts: Vec<Value>) -> Value {
    for event in &mut facts {
        let object = event.as_object_mut().unwrap();
        match object["event"].as_str().unwrap() {
            "team_send_action_recorded" => {
                object.remove("action");
            }
            "remote_envelope_dispatched" => {
                object.remove("task");
                object.remove("authorship");
                object
                    .get_mut("action")
                    .unwrap()
                    .as_object_mut()
                    .unwrap()
                    .remove("action");
            }
            "team_send_recipient_settled" => {
                object.remove("action");
                let settlement = object
                    .get_mut("settlement")
                    .unwrap()
                    .as_object_mut()
                    .unwrap();
                settlement.remove("item");
                settlement.remove("task_id");
            }
            tag => panic!("unexpected stable send fact {tag}"),
        }
    }
    facts.sort_by_key(|event| match event["event"].as_str().unwrap() {
        "team_send_action_recorded" => (0, 0),
        "remote_envelope_dispatched" => (1, event["action"]["index"].as_u64().unwrap()),
        "team_send_recipient_settled" => (2, event["index"].as_u64().unwrap()),
        _ => unreachable!(),
    });
    json!(facts)
}
fn forbidden(value: &Value) {
    match value {
        Value::Object(object) => {
            for (key, child) in object {
                let normalized = key.to_ascii_lowercase();
                assert!(
                    !normalized.contains("proposal")
                        && !normalized.contains("authorship")
                        && normalized != "subject"
                        && normalized != "context",
                    "M36 attached: proposal wire must have no subject, context or authorship"
                );
                forbidden(child);
            }
        }
        Value::Array(values) => {
            for child in values {
                forbidden(child);
            }
        }
        _ => {}
    }
}
#[cfg(feature = "cron")]
#[tokio::test]
async fn interleaved_cron_keeps_attached_proposal_on_invoking_conversation() {
    use crate::attached_harness::FIXTURE_TIMEOUT;
    use rustain::adapters::noop::NoOpChannel;
    use rustain::adapters::scheduler::cron::CronSchedulerAdapter;
    use rustain::domain::models::{ApprovalOutcome, StopReason, StreamChunk};
    use rustain::domain::ports::SchedulerPort;
    use std::sync::Arc;

    let (daemon, mut writer, host) = fixture().await;
    let invoking_conversation = daemon.conversation.lock().await.id.clone();
    let path = daemon.journal.workspace.path().join("approval-barrier.txt");
    daemon.script.exchange(vec![
        StreamChunk::ToolUse {
            id: "attached-write-barrier".into(),
            name: "Write".into(),
            input: json!({"file_path":path,"content":"not written: operator rejects"}),
        },
        StreamChunk::ToolUse {
            id: "attached-proposal-after-cron".into(),
            name: "propose_team_message".into(),
            input: input(),
        },
        StreamChunk::TurnComplete {
            stop_reason: StopReason::ToolUse,
        },
    ]);
    writer
        .send_frame(ClientFrame::UserMessage {
            text: "Write the supplied file, then propose the supplied team message.".into(),
            images: vec![],
        })
        .await;
    // The batch has already set execution context to the attached conversation.
    // Write is not parallel-safe, so its genuine permission wait holds the
    // following proposal until the operator answers over the signed socket.
    let request_id = match writer
        .wait_for(
            |frame| matches!(frame, DaemonFrame::ApprovalRequest { tool, .. } if tool == "Write"),
        )
        .await
    {
        DaemonFrame::ApprovalRequest { request_id, .. } => request_id,
        _ => unreachable!(),
    };
    assert!(daemon.proposals.pending().await.is_empty());

    // The production cron caller reuses the daemon core but owns a different
    // conversation and does not take AttachServer's turn_serial mutex.
    let before_cron = daemon.provider_request_count();
    daemon.script.tool("propose_team_message", input());
    let cron_path = daemon.journal.workspace.path().join("cron.toml");
    tokio::fs::write(
        &cron_path,
        "[[jobs]]\nname = \"conversation-binding\"\nschedule = \"* * * * * *\"\nprompt = \"Propose the supplied team message.\"\nforward = false\n",
    )
    .await
    .expect("write actual cron configuration");
    let (completion_tx, mut completion_rx) = tokio::sync::mpsc::unbounded_channel();
    let scheduler = CronSchedulerAdapter::load(
        cron_path,
        daemon.core.clone(),
        completion_tx,
        Arc::new(NoOpChannel),
        daemon.runtime.storage.clone(),
    )
    .await
    .expect("load production cron scheduler");
    scheduler
        .start_loop()
        .await
        .expect("start actual cron loop");
    let completion = tokio::time::timeout(FIXTURE_TIMEOUT, completion_rx.recv())
        .await
        .expect("cron completion deadline")
        .expect("cron job completes while attached tool awaits approval");
    scheduler
        .shutdown_loop()
        .await
        .expect("join actual cron jobs before resuming attached batch");
    assert_eq!(completion.job_name, "conversation-binding");
    assert!(
        daemon.requests.lock()[before_cron..]
            .iter()
            .flat_map(|(messages, _)| messages)
            .flat_map(|message| &message.tool_results)
            .any(|result| result
                .content
                .contains("propose_team_message is only available to the operator's own agent")),
        "cron must execute its tool batch, overwriting shared execution context"
    );
    assert!(daemon.proposals.pending().await.is_empty());

    writer
        .send_frame(ClientFrame::ApprovalResponse {
            request_id,
            outcome: ApprovalOutcome::Reject { feedback: None },
        })
        .await;
    completed(&mut writer).await;
    let _ = proposal(&mut writer).await;
    let snapshot = daemon.proposals.pending().await;
    assert_eq!(snapshot.len(), 1, "attached turn must raise one proposal");
    assert_eq!(
        snapshot[0].conversation_id, invoking_conversation,
        "M42 attached: snapshot must bind invoking UserMessage conversation"
    );
    zero(&daemon, &[&host], "interleaving must not send the proposal").await;
    daemon.stop().await;
    host.stop().await;
}

#[tokio::test]
async fn approved_snapshot_matches_attached_typed_send_exactly() {
    let lena = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let jun = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let daemon = AttachedDaemon::start(vec![lena.peer("lena-po"), jun.peer("jun-dev")]).await;
    let mut writer =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    writer
        .send_frame(ClientFrame::UserMessage {
            text: "CONVERSATION_MARKER_MUST_NOT_LEAK_73bb".into(),
            images: vec![],
        })
        .await;
    completed(&mut writer).await;
    lena.reset_counts();
    jun.reset_counts();
    let mut value = input();
    value["recipients"] = json!(["jun-dev", "lena-po"]);
    submit(&daemon, &mut writer, value).await;
    let id = proposal(&mut writer).await;
    let snapshot = daemon.proposals.pending().await;
    assert_eq!(
        snapshot[0].conversation_id,
        daemon.conversation.lock().await.id,
        "M42 attached: snapshot must bind invoking UserMessage conversation"
    );
    assert_eq!(
        snapshot[0].recipients,
        ["jun-dev", "lena-po"],
        "M35 attached: immutable proposal preserves operator recipient order"
    );
    zero(
        &daemon,
        &[&lena, &jun],
        "undecided snapshot must cause zero requests",
    )
    .await;
    decide(&mut writer, &id, TeamProposalDecision::Send).await;
    daemon
        .team_send_hooks
        .next_completion()
        .await
        .expect("daemon-owned proposed send completes");
    let proposal_facts = facts(&daemon).await;
    assert!(
        proposal_facts
            .iter()
            .any(|e| e["event"] == "team_send_action_recorded"),
        "M20 attached: approved proposal must use canonical typed send coordinator"
    );
    let proposal_log =
        rustain::domain::services::transparency::fold_transparency(&daemon.journal.entries().await)
            .iter()
            .map(|row| row.one_line())
            .collect::<Vec<_>>()
            .join("\n");
    assert_eq!(
        proposal_log.matches("[agent-composed]").count(),
        2,
        "M44 attached: approved dispatch shared log must carry agent-composed tag"
    );
    let started = writer.wait_send_started(&id).await;
    writer.wait_send_settled(&id, 0).await;
    writer.wait_send_settled(&id, 1).await;
    let action = proposal_facts
        .iter()
        .find(|e| e["event"] == "team_send_action_recorded")
        .unwrap();
    assert_eq!(
        action["body_hash"],
        hash(BODY),
        "M33 attached: action hash must bind exact body bytes"
    );
    assert_eq!(
        action["body_bytes"],
        BODY.len(),
        "M33 attached: action bytes must bind exact body bytes"
    );
    assert!(
        action.get("subject").is_none() || action["subject"].is_null(),
        "M36 attached: approved proposal subject must be None"
    );
    assert_eq!(
        action["recipients"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["alias"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["jun-dev", "lena-po"],
        "M35 attached: decision must use snapshot recipient order"
    );
    for event in proposal_facts
        .iter()
        .filter(|e| e["event"] == "remote_envelope_dispatched")
    {
        assert_eq!(
            event["authorship"], "agent-composed",
            "M44 attached: approved dispatch must be agent-composed"
        );
    }
    for host in [&lena, &jun] {
        assert_eq!(host.counts(), HttpCounts { get: 0, post: 1 });
        let posts = host.posts();
        let wire: Value = serde_json::from_slice(posts[0].body()).unwrap();
        assert_eq!(
            wire.pointer("/params/message/parts/0/text")
                .and_then(Value::as_str)
                .unwrap()
                .as_bytes(),
            BODY.as_bytes(),
            "M33 attached: recipient must receive byte-identical body"
        );
        assert!(
            !wire
                .to_string()
                .contains("CONVERSATION_MARKER_MUST_NOT_LEAK_73bb"),
            "M34 attached: conversation context must not reach recipient"
        );
        assert_eq!(
            wire.pointer("/params/message/metadata/x-rustain-message-type")
                .and_then(Value::as_str),
            Some("architecture_update")
        );
        forbidden(&wire);
        let received = host.journal().entries().await;
        let contents: Vec<_> = received
            .iter()
            .filter_map(|entry| match &entry.record {
                JournalRecord::Room(event @ RoomEvent::RecipientItemReceived { .. }) => {
                    Some(serde_json::to_value(event).unwrap())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            contents.len(),
            1,
            "recipient must durably receive exactly one item"
        );
        let received_body = contents[0]["content"]
            .as_str()
            .expect("recipient journal body");
        assert_eq!(
            received_body.as_bytes(),
            BODY.as_bytes(),
            "M33 attached: recipient journal must preserve exact bytes"
        );
        assert_eq!(
            hash(received_body),
            hash(BODY),
            "recipient journal hash must equal approved snapshot"
        );
    }
    writer
        .send_team(
            "typed-control",
            vec!["jun-dev".into(), "lena-po".into()],
            BODY,
            Some(SemanticMessageType::ArchitectureUpdate),
            None,
        )
        .await;
    let typed_started = writer.wait_send_started("typed-control").await;
    writer.wait_send_settled("typed-control", 0).await;
    writer.wait_send_settled("typed-control", 1).await;
    let all = facts(&daemon).await;
    let combined_log =
        rustain::domain::services::transparency::fold_transparency(&daemon.journal.entries().await)
            .iter()
            .map(|row| row.one_line())
            .collect::<Vec<_>>()
            .join("\n");
    assert_eq!(
        combined_log.matches("[agent-composed]").count(),
        2,
        "typed attached send log must not acquire agent-composed tags"
    );
    let typed = all[proposal_facts.len()..].to_vec();
    assert_eq!(
        normalize(proposal_facts),
        normalize(typed.clone()),
        "M20 attached: approved proposal must use canonical typed send coordinator"
    );
    for event in typed
        .iter()
        .filter(|e| e["event"] == "remote_envelope_dispatched")
    {
        assert_eq!(
            event["authorship"], "human-written",
            "typed control must remain human-written"
        );
    }
    let normalize_started = |frame: DaemonFrame| {
        let mut value = serde_json::to_value(frame).unwrap();
        let object = value
            .get_mut("teamSendStarted")
            .expect("started frame")
            .as_object_mut()
            .unwrap();
        object.remove("request");
        object.remove("action");
        value
    };
    assert_eq!(
        normalize_started(started),
        normalize_started(typed_started),
        "M20 attached: canonical started rows must match typed control"
    );
    daemon.stop().await;
    lena.stop().await;
    jun.stop().await;
}
