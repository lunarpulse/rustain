use crate::attached_harness::{AttachedClient, AttachedDaemon, FIXTURE_TIMEOUT};
use crate::g_harness::*;
use crate::observed_host::{HttpCounts, ObservedHost};
use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::daemon::protocol::{
    AttachMode, ClientFrame, ConnectionTier, DaemonFrame, ProtocolError, TeamProposalCloseOutcome,
    read_frame,
};
use rustain::adapters::daemon::server::{
    MAX_IN_FLIGHT_TEAM_SENDS, TeamSendCompletionKind, TeamSendFifoEventKind, TeamSendTestHooks,
};
use rustain::domain::models::{ResponseMode, RoomEvent, SemanticMessageType, TeamProposalDecision};
use tokio::io::AsyncWriteExt;

async fn private_observer(tier: ConnectionTier, diagnostic: &str) {
    let (daemon, mut writer, host) = fixture().await;
    let mut observer = AttachedClient::attach(&daemon.path, tier, true).await;
    assert_eq!(observer.granted, AttachMode::ReadOnly);
    submit(&daemon, &mut writer, input()).await;
    let id = proposal(&mut writer).await;
    fence(&mut observer, "privacy-fence").await;
    assert!(
        !observer
            .received
            .iter()
            .any(|f| matches!(f, DaemonFrame::TeamProposal { .. })),
        "{diagnostic}"
    );
    zero(&daemon, &[&host], diagnostic).await;
    decide(&mut writer, &id, TeamProposalDecision::Send).await;
    daemon
        .team_send_hooks
        .next_completion()
        .await
        .expect("proposal send task completion");
    fence(&mut observer, "decider-fence").await;
    assert!(
        observer.frames_for_request(&id).is_empty(),
        "M31: proposal send results must target only its decider"
    );
    writer.wait_send_started(&id).await;
    writer.wait_send_settled(&id, 0).await;
    assert_eq!(
        host.counts(),
        HttpCounts { get: 0, post: 1 },
        "human decider positive control sends once"
    );
    daemon.stop().await;
    host.stop().await;
}
#[tokio::test]
async fn readonly_observer_never_receives_proposal() {
    private_observer(
        ConnectionTier::TrustedLocal,
        "M23: read-only observer received private proposal",
    )
    .await;
}
#[tokio::test]
async fn peer_observer_never_receives_proposal() {
    private_observer(
        ConnectionTier::Peer,
        "M23: Peer observer received private proposal",
    )
    .await;
}
async fn unauthorized(tier: ConnectionTier, diagnostic: &str) {
    let (daemon, mut writer, host) = fixture().await;
    let mut observer = AttachedClient::attach(&daemon.path, tier, true).await;
    submit(&daemon, &mut writer, input()).await;
    let id = proposal(&mut writer).await;
    decide(&mut observer, &id, TeamProposalDecision::Send).await;
    fence(&mut observer, "unauthorized-decision-fence").await;
    assert!(
        observer.received.iter().any(|error| matches!(
            error,
            DaemonFrame::Error(ProtocolError::ReadOnly)
        ) && tier == ConnectionTier::TrustedLocal
            || matches!(
                error,
                DaemonFrame::Error(ProtocolError::PeerVerification(_))
            ) && tier == ConnectionTier::Peer),
        "{diagnostic}"
    );
    assert_eq!(daemon.proposals.pending().await.len(), 1, "{diagnostic}");
    zero(&daemon, &[&host], diagnostic).await;
    send_once(&daemon, &mut writer, &host, &id).await;
    daemon.stop().await;
    host.stop().await;
}
#[tokio::test]
async fn readonly_decision_is_refused() {
    unauthorized(
        ConnectionTier::TrustedLocal,
        "M24: read-only decision was not refused",
    )
    .await;
}
#[tokio::test]
async fn peer_decision_is_refused() {
    unauthorized(ConnectionTier::Peer, "M24: Peer decision was not refused").await;
}
async fn unauthorized_opt_in(tier: ConnectionTier) {
    let (daemon, mut writer, host) = fixture().await;
    submit(&daemon, &mut writer, input()).await;
    let id = proposal(&mut writer).await;
    let mut observer = AttachedClient::attach_without_proposals(&daemon.path, tier, true).await;
    observer.send_frame(ClientFrame::EnableTeamProposals).await;
    let error = observer
        .wait_for(|frame| matches!(frame, DaemonFrame::Error(_)))
        .await;
    assert!(
        matches!(&error, DaemonFrame::Error(ProtocolError::ReadOnly))
            && tier == ConnectionTier::TrustedLocal
            || matches!(
                &error,
                DaemonFrame::Error(ProtocolError::PeerVerification(_))
            ) && tier == ConnectionTier::Peer,
        "unauthorized capability opt-in must use the standard refusal: {error:?}"
    );
    observer
        .send_frame(ClientFrame::HistoryRequest {
            before_index: Some(0),
            count: 1,
        })
        .await;
    observer
        .wait_for(|frame| matches!(frame, DaemonFrame::History { .. }))
        .await;
    assert!(
        !observer
            .received
            .iter()
            .any(|frame| matches!(frame, DaemonFrame::TeamProposal { .. })),
        "unauthorized opt-in must neither grant nor reemit a private snapshot"
    );
    assert_eq!(daemon.proposals.pending().await[0].id, id);
    zero(
        &daemon,
        &[&host],
        "unauthorized opt-in must have zero effects",
    )
    .await;
    send_once(&daemon, &mut writer, &host, &id).await;
    daemon.stop().await;
    host.stop().await;
}

#[tokio::test]
async fn readonly_proposal_opt_in_is_refused() {
    unauthorized_opt_in(ConnectionTier::TrustedLocal).await;
}

#[tokio::test]
async fn peer_proposal_opt_in_is_refused() {
    unauthorized_opt_in(ConnectionTier::Peer).await;
}

#[tokio::test]
async fn dismiss_and_unknown_decision_have_zero_effect() {
    let (daemon, mut writer, host) = fixture().await;
    submit(&daemon, &mut writer, input()).await;
    let id = proposal(&mut writer).await;
    decide(&mut writer, &id, TeamProposalDecision::Dismiss).await;
    writer
        .send_frame(ClientFrame::HistoryRequest {
            before_index: None,
            count: 1,
        })
        .await;
    writer
        .wait_for(|f| matches!(f, DaemonFrame::History { .. }))
        .await;
    assert!(writer.received.iter().any(|f|matches!(f,
        DaemonFrame::TeamProposalClosed {proposal,outcome:TeamProposalCloseOutcome::Dismissed} if proposal==&id)),
        "M18 attached: Dismiss must close without sending");
    decide(&mut writer, &id, TeamProposalDecision::Send).await;
    writer
        .wait_for(|f| {
            matches!(f,DaemonFrame::TeamProposalClosed {
        proposal,outcome:TeamProposalCloseOutcome::Refused {..}} if proposal==&id)
        })
        .await;
    zero(
        &daemon,
        &[&host],
        "M18 attached: Dismiss must close without sending",
    )
    .await;
    submit(&daemon, &mut writer, input()).await;
    let next = proposal(&mut writer).await;
    send_once(&daemon, &mut writer, &host, &next).await;
    daemon.stop().await;
    host.stop().await;
}
#[tokio::test]
async fn replacement_reemits_same_snapshot_and_sends_once() {
    let (daemon, mut writer, host) = fixture().await;
    submit(&daemon, &mut writer, input()).await;
    let id = proposal(&mut writer).await;
    writer.detach().await;
    let mut replacement =
        AttachedClient::attach_without_proposals(&daemon.path, ConnectionTier::TrustedLocal, false)
            .await;
    replacement
        .send_frame(ClientFrame::HistoryRequest {
            before_index: Some(0),
            count: 1,
        })
        .await;
    replacement
        .wait_for(|frame| matches!(frame, DaemonFrame::History { .. }))
        .await;
    assert!(
        !replacement
            .received
            .iter()
            .any(|frame| matches!(frame, DaemonFrame::TeamProposal { .. })),
        "replacement writer must not receive the retained snapshot before opt-in"
    );
    replacement.enable_team_proposals().await;
    assert!(
        replacement
            .received
            .iter()
            .any(|f| matches!(f,DaemonFrame::TeamProposal {proposal,..} if proposal==&id)),
        "M28: replacement must reemit same pending proposal"
    );
    let reemitted = replacement
        .received
        .iter()
        .find(
            |frame| matches!(frame, DaemonFrame::TeamProposal { proposal, .. } if proposal == &id),
        )
        .expect("opt-in reemits retained proposal");
    let pending = daemon.proposals.pending().await;
    assert!(
        matches!(reemitted, DaemonFrame::TeamProposal { recipients, message_type, body, .. }
        if recipients == &pending[0].recipients
            && message_type == &pending[0].message_type
            && body == &pending[0].body),
        "opt-in must reemit the exact retained body, type and recipient order"
    );
    zero(
        &daemon,
        &[&host],
        "M28: pending proposal must survive detach without sending",
    )
    .await;
    send_once(&daemon, &mut replacement, &host, &id).await;
    daemon.stop().await;
    host.stop().await;
}
#[tokio::test]
async fn duplicated_send_consumes_snapshot_under_one_lock() {
    let (daemon, mut writer, host) = fixture().await;
    submit(&daemon, &mut writer, input()).await;
    let id = proposal(&mut writer).await;
    let before = daemon.proposals.lock_acquisitions();
    decide(&mut writer, &id, TeamProposalDecision::Send).await;
    writer.wait_send_settled(&id, 0).await;
    // No pending() read is included in this structural measurement.
    assert_eq!(
        daemon.proposals.lock_acquisitions() - before,
        1,
        "M26: decision take must acquire registry lock exactly once"
    );
    decide(&mut writer, &id, TeamProposalDecision::Send).await;
    writer.wait_for(|f|matches!(f,DaemonFrame::TeamProposalClosed {proposal,outcome:TeamProposalCloseOutcome::Refused {..}} if proposal==&id)).await;
    assert_eq!(
        host.counts(),
        HttpCounts { get: 0, post: 1 },
        "M26: duplicate Send must produce exactly one POST"
    );
    assert_eq!(
        crate::harness::read_room_events(&daemon.journal)
            .await
            .iter()
            .filter(|(_, e)| matches!(e, RoomEvent::TeamSendActionRecorded { .. }))
            .count(),
        1,
        "M26: duplicate Send must produce exactly one action"
    );
    daemon.stop().await;
    host.stop().await;
}

#[tokio::test]
async fn accepted_proposal_send_survives_eof_without_replacement() {
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let hooks = TeamSendTestHooks::new(Some(2), None, false).with_send_action_gate();
    let daemon = AttachedDaemon::start_with_test_hooks(vec![host.peer("lena-po")], hooks).await;
    let mut writer =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    host.reset_counts();

    // Exercise the canonical send and held-task ownership before the regression.
    writer
        .send_team(
            "pre-eof-control",
            vec!["lena-po".into()],
            BODY,
            Some(SemanticMessageType::ArchitectureUpdate),
            None,
        )
        .await;
    daemon.team_send_hooks.wait_for_send_action_count(1).await;
    assert_eq!(daemon.server.team_send_observations().0, 1);
    daemon.team_send_hooks.release_send_actions(1);
    let control = daemon
        .team_send_hooks
        .next_completion()
        .await
        .expect("typed control completion");
    assert_eq!(control.request, "pre-eof-control");
    assert_eq!(control.kind, TeamSendCompletionKind::Finished);
    assert!(matches!(
        writer.wait_send_started("pre-eof-control").await,
        DaemonFrame::TeamSendStarted { .. }
    ));
    assert!(matches!(
        writer.wait_send_settled("pre-eof-control", 0).await,
        DaemonFrame::TeamSendSettled { .. }
    ));
    assert_eq!(
        host.counts(),
        HttpCounts { get: 0, post: 1 },
        "live typed send positive control"
    );
    let control_events = crate::harness::read_room_events(&daemon.journal).await;
    assert_eq!(
        control_events
            .iter()
            .filter(|(_, event)| matches!(event, RoomEvent::TeamSendActionRecorded { .. }))
            .count(),
        1
    );
    host.reset_counts();

    submit(&daemon, &mut writer, input()).await;
    let id = proposal(&mut writer).await;
    decide(&mut writer, &id, TeamProposalDecision::Send).await;
    daemon.team_send_hooks.wait_for_worker_gate_count(1).await;
    assert_eq!(daemon.proposals.pending().await[0].id, id);
    writer.detach().await; // Real EOF fences removal of the deciding connection.
    // No replacement grant occurs before the admitted decision finishes.
    daemon.team_send_hooks.release_worker();
    for expected in [
        TeamSendFifoEventKind::Enqueued,
        TeamSendFifoEventKind::Dequeued,
        TeamSendFifoEventKind::Handled,
    ] {
        loop {
            let event = daemon
                .team_send_hooks
                .next_fifo_event()
                .await
                .expect("accepted Send FIFO");
            if event.request == id {
                assert_eq!(
                    event.kind, expected,
                    "DETACHED-SEND: an accepted decision must execute without a replacement writer"
                );
                break;
            }
        }
    }
    // AC4(c), A5/A7/A8: undecided cards survive detach, but a human Send already
    // accepted by the FIFO is daemon-owned unless a later writer fences it.
    // The coordinator is held, so its owned permit is observable even if its
    // task has not been polled. A missing task fails here, never by a timeout.
    assert_eq!(
        daemon.server.team_send_observations().0,
        1,
        "DETACHED-SEND: accepted pre-EOF Send must remain daemon-owned until its exactly-once dispatch, not silently consume and lose the pending proposal"
    );
    assert!(
        daemon.proposals.pending().await.is_empty(),
        "accepted Send consumes its snapshot once"
    );
    assert_eq!(
        host.counts(),
        HttpCounts { get: 0, post: 0 },
        "held coordinator has no HTTP effects"
    );
    daemon.team_send_hooks.wait_for_send_action_count(2).await;
    daemon.team_send_hooks.release_send_actions(1);
    let completion = daemon
        .team_send_hooks
        .next_completion()
        .await
        .expect("detached Send completion");
    assert_eq!(completion.request, id);
    assert_eq!(completion.kind, TeamSendCompletionKind::Finished);
    assert_eq!(daemon.server.team_send_observations(), (0, 2, 2, 0));
    assert_eq!(
        host.counts(),
        HttpCounts { get: 0, post: 1 },
        "DETACHED-SEND: accepted pre-EOF Send must POST exactly once"
    );
    let events = crate::harness::read_room_events(&daemon.journal).await;
    let sent = &events[control_events.len()..];
    assert_eq!(
        sent.iter()
            .filter(|(_, event)| matches!(event, RoomEvent::TeamSendActionRecorded { .. }))
            .count(),
        1,
        "DETACHED-SEND: accepted pre-EOF Send must durably record exactly one canonical action"
    );
    assert_eq!(
        sent.iter()
            .filter(|(_, event)| matches!(event, RoomEvent::RemoteEnvelopeDispatched { .. }))
            .count(),
        1
    );
    assert_eq!(
        sent.iter()
            .filter(|(_, event)| matches!(event, RoomEvent::TeamSendRecipientSettled { .. }))
            .count(),
        1
    );
    let dispatch = sent
        .iter()
        .find_map(|(_, event)| match event {
            RoomEvent::RemoteEnvelopeDispatched { .. } => {
                Some(serde_json::to_value(event).unwrap())
            }
            _ => None,
        })
        .expect("canonical detached dispatch");
    assert_eq!(dispatch["authorship"], "agent-composed");
    let posts = host.posts();
    let wire: serde_json::Value = serde_json::from_slice(posts[0].body()).unwrap();
    assert_eq!(
        wire.pointer("/params/message/parts/0/text")
            .and_then(serde_json::Value::as_str),
        Some(BODY),
        "detached send must deliver the approved snapshot, not a replacement body"
    );
    assert_eq!(
        wire.pointer("/params/message/metadata/x-rustain-message-type")
            .and_then(serde_json::Value::as_str),
        Some("architecture_update")
    );

    let mut replacement =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    fence(&mut replacement, "completed-detached-send-fence").await;
    assert!(
        !replacement.received.iter().any(|frame| matches!(frame,
        DaemonFrame::TeamProposal { proposal, .. } if proposal == &id)),
        "completed detached Send must not reappear as an unsent card"
    );
    assert!(
        replacement.frames_for_request(&id).is_empty(),
        "detached send receipts remain private to the deciding connection"
    );
    decide(&mut replacement, &id, TeamProposalDecision::Send).await;
    let refused = replacement
        .wait_for(|frame| {
            matches!(frame,
        DaemonFrame::TeamProposalClosed { proposal, .. } if proposal == &id)
        })
        .await;
    assert!(
        matches!(refused, DaemonFrame::TeamProposalClosed {
        outcome: TeamProposalCloseOutcome::Refused { reason }, ..
    } if reason == "That proposal is no longer waiting. Nothing was sent."),
        "fresh attachment must honestly refuse a duplicate of the completed detached Send"
    );
    assert_eq!(
        host.counts(),
        HttpCounts { get: 0, post: 1 },
        "reattachment must not resend"
    );
    assert_eq!(
        crate::harness::read_room_events(&daemon.journal)
            .await
            .iter()
            .filter(|(_, event)| matches!(event, RoomEvent::TeamSendActionRecorded { .. }))
            .count(),
        2,
        "typed control plus detached Send must remain exactly two actions after the duplicate"
    );
    daemon.stop().await;
    host.stop().await;
}

#[tokio::test]
async fn replaced_writer_queued_decision_is_discarded() {
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let hooks = TeamSendTestHooks::new(Some(1), None, false);
    let daemon = AttachedDaemon::start_with_test_hooks(vec![host.peer("lena-po")], hooks).await;
    let mut writer =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    host.reset_counts();
    submit(&daemon, &mut writer, input()).await;
    let id = proposal(&mut writer).await;
    // Only send/decision frames count toward the deterministic worker gate.
    decide(&mut writer, &id, TeamProposalDecision::Send).await;
    daemon.team_send_hooks.wait_for_worker_gate_count(1).await;
    // Capability requests queued behind the held decision obey the same stale
    // writer fence; they cannot reclaim the replacement's proposal surface.
    writer.send_frame(ClientFrame::EnableTeamProposals).await;
    writer.detach().await;
    let mut replacement =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    assert_eq!(proposal(&mut replacement).await, id);
    // The new writer can withdraw the old card and raise a distinct one while
    // the old writer's accepted decision is still held before its stale fence.
    replacement
        .send_frame(ClientFrame::UserMessage {
            text: "/clear".into(),
            images: vec![],
        })
        .await;
    replacement
        .send_frame(ClientFrame::HistoryRequest {
            before_index: None,
            count: 1,
        })
        .await;
    replacement
        .wait_for(|frame| matches!(frame, DaemonFrame::History { .. }))
        .await;
    assert!(replacement.received.iter().any(|frame| matches!(frame,
        DaemonFrame::TeamProposalClosed { proposal, outcome: TeamProposalCloseOutcome::Withdrawn }
            if proposal == &id)));
    let fresh_body = "A fresh writer's proposal must survive every old decision.";
    let mut fresh_input = input();
    fresh_input["body"] = serde_json::json!(fresh_body);
    submit(&daemon, &mut replacement, fresh_input).await;
    let fresh_id = proposal(&mut replacement).await;
    assert_ne!(
        fresh_id, id,
        "replacement must raise a fresh host-minted identity"
    );
    daemon.team_send_hooks.release_worker();
    loop {
        let event = daemon
            .team_send_hooks
            .next_fifo_event()
            .await
            .expect("FIFO observation");
        if event.request == id && event.kind != TeamSendFifoEventKind::Enqueued {
            assert_eq!(
                event.kind,
                TeamSendFifoEventKind::Discarded,
                "M25: replaced writer queued decision must be discarded"
            );
            break;
        }
    }
    zero(
        &daemon,
        &[&host],
        "M25: replaced writer queued decision must be discarded",
    )
    .await;
    let pending = daemon.proposals.pending().await;
    assert_eq!(
        pending.len(),
        1,
        "M25: stale writer must preserve the replacement's fresh card"
    );
    assert_eq!(pending[0].id, fresh_id);
    assert_eq!(pending[0].body, fresh_body);
    // Even from the new authenticated writer, a decision naming the withdrawn
    // identity must not take the current conversation's different proposal.
    decide(&mut replacement, &id, TeamProposalDecision::Send).await;
    let refused = replacement.wait_for(|frame| matches!(frame,
        DaemonFrame::TeamProposalClosed { proposal, outcome: TeamProposalCloseOutcome::Refused { .. } }
            if proposal == &id)).await;
    assert!(matches!(refused, DaemonFrame::TeamProposalClosed {
        outcome: TeamProposalCloseOutcome::Refused { reason }, ..
    } if reason == "That proposal is no longer waiting. Nothing was sent."));
    let pending = daemon.proposals.pending().await;
    assert_eq!(
        pending.len(),
        1,
        "old proposal identity must not consume the fresh card"
    );
    assert_eq!(pending[0].id, fresh_id);
    assert_eq!(pending[0].body, fresh_body);
    zero(
        &daemon,
        &[&host],
        "M25: old decisions must not send or consume the fresh snapshot",
    )
    .await;
    send_once(&daemon, &mut replacement, &host, &fresh_id).await;
    let posts = host.posts();
    let wire: serde_json::Value = serde_json::from_slice(posts[0].body()).unwrap();
    assert_eq!(
        wire.pointer("/params/message/parts/0/text")
            .and_then(serde_json::Value::as_str),
        Some(fresh_body),
        "replacement positive control must send only its fresh snapshot"
    );
    daemon.stop().await;
    host.stop().await;
}
#[tokio::test]
async fn opt_in_waits_for_operator_fifo_and_cannot_outlive_its_writer() {
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let daemon = AttachedDaemon::start_with_test_hooks(
        vec![host.peer("lena-po")],
        TeamSendTestHooks::new(Some(1), None, false),
    )
    .await;
    let mut plain =
        AttachedClient::attach_without_proposals(&daemon.path, ConnectionTier::TrustedLocal, false)
            .await;
    host.reset_counts();
    plain
        .send_team(
            "hold-before-capability",
            vec!["lena-po".into()],
            BODY,
            Some(SemanticMessageType::ArchitectureUpdate),
            None,
        )
        .await;
    daemon.team_send_hooks.wait_for_worker_gate_count(1).await;
    plain.send_frame(ClientFrame::EnableTeamProposals).await;
    plain
        .send_team(
            "after-capability-read-fence",
            vec!["lena-po".into()],
            BODY,
            Some(SemanticMessageType::ArchitectureUpdate),
            None,
        )
        .await;
    loop {
        let event = daemon.team_send_hooks.next_fifo_event().await.unwrap();
        if event.request == "after-capability-read-fence" {
            assert_eq!(event.kind, TeamSendFifoEventKind::Enqueued);
            break;
        }
    }
    // The reader has accepted the opt-in but its FIFO predecessor is held.
    // A real operator-origin turn must still observe no decision surface.
    let before = daemon.provider_request_count();
    daemon.script.tool("propose_team_message", input());
    let mut conversation = daemon.conversation.lock().await;
    let task = daemon
        .runtime
        .drive_turn(
            "Propose while the capability grant is still queued.".into(),
            rustain::domain::models::ChannelKind::Terminal,
            &mut conversation,
            &daemon.domain_tx,
            rustain::domain::models::TurnOrigin::Interactive,
            tokio_util::sync::CancellationToken::new(),
        )
        .await;
    drop(conversation);
    task.await.expect("operator turn completes before grant");
    assert!(
        daemon.requests.lock()[before..]
            .iter()
            .flat_map(|(messages, _)| messages.iter())
            .flat_map(|message| message.tool_results.iter())
            .any(|result| result.content.contains("no operator surface is attached")),
        "capability must not bypass the held operator FIFO"
    );
    assert!(daemon.proposals.pending().await.is_empty());
    zero(
        &daemon,
        &[&host],
        "queued capability must have zero effects",
    )
    .await;
    plain.detach().await;
    let mut replacement =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    daemon.team_send_hooks.release_worker();
    loop {
        let event = daemon.team_send_hooks.next_fifo_event().await.unwrap();
        if event.request == "after-capability-read-fence" {
            assert_eq!(
                event.kind,
                TeamSendFifoEventKind::Discarded,
                "stale writer's capability and following send must be fenced"
            );
            break;
        }
    }
    submit(&daemon, &mut replacement, input()).await;
    let id = proposal(&mut replacement).await;
    zero(
        &daemon,
        &[&host],
        "stale opt-in must not dispatch old sends",
    )
    .await;
    send_once(&daemon, &mut replacement, &host, &id).await;
    daemon.stop().await;
    host.stop().await;
}
#[tokio::test]
async fn saturated_send_preserves_proposal_for_retry() {
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let hooks = TeamSendTestHooks::new(None, None, false).with_send_action_gate();
    let daemon = AttachedDaemon::start_with_test_hooks(vec![host.peer("lena-po")], hooks).await;
    let mut writer =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    host.reset_counts();
    submit(&daemon, &mut writer, input()).await;
    let id = proposal(&mut writer).await;
    for i in 0..MAX_IN_FLIGHT_TEAM_SENDS {
        writer
            .send_team(
                format!("capacity-{i}"),
                vec!["lena-po".into()],
                "held",
                Some(SemanticMessageType::ArchitectureUpdate),
                None,
            )
            .await;
    }
    daemon
        .team_send_hooks
        .wait_for_send_action_count(MAX_IN_FLIGHT_TEAM_SENDS)
        .await;
    decide(&mut writer, &id, TeamProposalDecision::Send).await;
    let refused = writer
        .wait_for(|f| matches!(f,DaemonFrame::TeamProposalClosed {proposal,..} if proposal==&id))
        .await;
    assert!(
        matches!(refused,DaemonFrame::TeamProposalClosed {outcome:TeamProposalCloseOutcome::Refused {reason},..} if reason=="Not sent yet — this daemon is busy sending. The proposal is still waiting."),
        "capacity refusal must retain proposal and state T11"
    );
    assert_eq!(
        daemon.proposals.pending().await[0].id,
        id,
        "capacity must be acquired before destructive take"
    );
    zero(
        &daemon,
        &[&host],
        "saturation must have zero outbound effects before release",
    )
    .await;
    daemon.team_send_hooks.release_send_actions(1);
    daemon
        .team_send_hooks
        .next_completion()
        .await
        .expect("released capacity completion");
    decide(&mut writer, &id, TeamProposalDecision::Send).await;
    daemon
        .team_send_hooks
        .wait_for_send_action_count(MAX_IN_FLIGHT_TEAM_SENDS + 1)
        .await;
    daemon
        .team_send_hooks
        .release_send_actions(MAX_IN_FLIGHT_TEAM_SENDS);
    writer.wait_send_settled(&id, 0).await;
    assert!(daemon.proposals.pending().await.is_empty());
    daemon.stop().await;
    host.stop().await;
}
#[tokio::test]
async fn reset_withdraws_pending_proposal_without_io() {
    let (daemon, mut writer, host) = fixture().await;
    submit(&daemon, &mut writer, input()).await;
    let id = proposal(&mut writer).await;
    writer
        .send_frame(ClientFrame::UserMessage {
            text: "/clear".into(),
            images: vec![],
        })
        .await;
    writer
        .send_frame(ClientFrame::HistoryRequest {
            before_index: None,
            count: 1,
        })
        .await;
    writer
        .wait_for(|f| matches!(f, DaemonFrame::History { .. }))
        .await;
    assert!(writer.received.iter().any(|f|matches!(f,
        DaemonFrame::TeamProposalClosed {proposal,outcome:TeamProposalCloseOutcome::Withdrawn} if proposal==&id)),
        "M43 attached: reset must withdraw rather than retarget proposal");
    assert!(
        daemon.proposals.pending().await.is_empty(),
        "M43 attached: reset must withdraw rather than retarget proposal"
    );
    zero(
        &daemon,
        &[&host],
        "M43 attached: reset must withdraw rather than retarget proposal",
    )
    .await;
    submit(&daemon, &mut writer, input()).await;
    let next = proposal(&mut writer).await;
    assert_ne!(id, next);
    send_once(&daemon, &mut writer, &host, &next).await;
    daemon.stop().await;
    host.stop().await;
}

#[tokio::test]
async fn full_writer_queue_closes_explicitly_and_reemits_retained_proposal() {
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let daemon = AttachedDaemon::start_with_test_hooks(
        vec![host.peer("lena-po")],
        TeamSendTestHooks::new(None, Some(1), true),
    )
    .await;
    let mut writer =
        AttachedClient::attach_without_proposals(&daemon.path, ConnectionTier::TrustedLocal, false)
            .await;
    // This writer is deliberately held before draining: opt in without a
    // response fence. The following UserMessage shares its grant's FIFO.
    writer.send_frame(ClientFrame::EnableTeamProposals).await;
    host.reset_counts();
    // Fill the bounded socket queue through a real command before the raise.
    writer.team_status("fill-writer-queue").await;
    daemon
        .team_send_hooks
        .next_publication()
        .await
        .expect("queued status publication");
    daemon.script.tool("propose_team_message", input());
    writer
        .send_frame(ClientFrame::UserMessage {
            text: "propose, never send".into(),
            images: vec![],
        })
        .await;
    tokio::time::timeout(FIXTURE_TIMEOUT, async {
        while daemon.provider_request_count() < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("raise tool continuation completes");
    assert_eq!(
        daemon.team_send_hooks.overflow_count(),
        1,
        "M29: full proposal queue must publish terminal error"
    );
    assert_eq!(
        daemon.proposals.pending().await.len(),
        1,
        "M29: overflowing proposal must remain retained"
    );
    let id = daemon.proposals.pending().await[0].id.clone();
    daemon.team_send_hooks.release_writer();
    writer
        .stream
        .shutdown()
        .await
        .expect("half-close failed connection");
    let error = tokio::time::timeout(
        FIXTURE_TIMEOUT,
        read_frame::<_, DaemonFrame>(&mut writer.stream),
    )
    .await
    .expect("terminal Error deadline")
    .expect("terminal Error decode");
    assert!(
        matches!(error, Some(DaemonFrame::Error(ProtocolError::Internal(_)))),
        "M29: full proposal queue must publish terminal error"
    );
    assert!(
        tokio::time::timeout(
            FIXTURE_TIMEOUT,
            read_frame::<_, DaemonFrame>(&mut writer.stream)
        )
        .await
        .expect("terminal EOF deadline")
        .expect("EOF decode")
        .is_none(),
        "M29: terminal error must be followed by EOF"
    );
    zero(
        &daemon,
        &[&host],
        "M29: failed writer publication must not send",
    )
    .await;
    // The positive reconnect must be healthy, not a second deliberately
    // one-slot overloaded connection racing the model's final public events.
    daemon.team_send_hooks.restore_default_queue_capacity();
    let mut replacement =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    assert_eq!(
        proposal(&mut replacement).await,
        id,
        "M29: overflowed proposal must reemit on next grant"
    );
    send_once(&daemon, &mut replacement, &host, &id).await;
    daemon.stop().await;
    host.stop().await;
}
