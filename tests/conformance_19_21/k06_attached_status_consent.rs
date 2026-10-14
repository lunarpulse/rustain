//! Story 19.21 K06 — attached status and consent boundaries.

use crate::attached_harness::{
    AppendGateJournal, AttachedClient, AttachedDaemon, FIXTURE_TIMEOUT, InboundSenderIdentity,
    is_consent_event, mint_request_key,
};
use crate::harness::Journal;
use rustain::adapters::daemon::protocol::{
    AttachMode, ClientFrame, ConnectionTier, DaemonFrame, ProtocolError, RecipientAction,
    RecipientActionOutcome, RecipientActionTarget,
};
use rustain::domain::models::{PolicyPrincipal, PrincipalKind, RoomEvent};
use rustain::domain::ports::{ConsentProjectionQuery, ConsentState};
use std::sync::Arc;

#[tokio::test]
async fn k06_toml_implied_consent_admits_the_pinned_sender_without_a_journal_grant() {
    let journal = Journal::open().await;
    let identity = InboundSenderIdentity::new("toml-consented");
    let peer = identity.identity.peer_id.clone();
    let config_dir = journal.workspace.path().join(".rustain");
    std::fs::create_dir_all(&config_dir).expect("create workspace policy directory");
    std::fs::write(
        config_dir.join(rustain::domain::models::INDIVIDUAL_POLICY_FILE),
        format!(
            "[interaction.overrides.\"toml-consented\"]\npeer_id = \"{}\"\nresponse_mode = \"notify-and-auto\"\nauto_response = \"TOML consent response\"\n",
            peer.as_str()
        ),
    )
    .expect("write TOML sender consent");
    let peers = vec![crate::attached_harness::pinned_peer_for_inbound_sender(
        &identity,
    )];
    let daemon = AttachedDaemon::start_with_journal(journal, peers, None).await;
    let host = daemon.serve_http(vec![identity]).await;
    let mut writer =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    let first_task = "toml-consented-task";
    let result = host
        .sender("toml-consented")
        .message_send(first_task, "k06 TOML-only consent first task")
        .await;
    assert!(result.json["error"].is_null(), "{result:?}");
    daemon.wait_for_received_task(first_task).await;
    assert!(
        daemon.sender_consent_prompts_for(&peer).await.is_empty(),
        "a matching pinned TOML override must admit without a sender-consent card"
    );
    let revoke = mint_request_key();
    writer
        .revoke_sender_consent(revoke.clone(), peer.as_str())
        .await;
    assert!(matches!(
        writer.wait_revoked(&revoke).await,
        DaemonFrame::SenderConsentRevoked { request, .. } if request == revoke
    ));
    let after_revoke_task = "toml-consented-after-revoke-task";
    let after_revoke = host
        .sender("toml-consented")
        .message_send(after_revoke_task, "k06 TOML-only consent after revoke")
        .await;
    assert!(after_revoke.json["error"].is_null(), "{after_revoke:?}");
    daemon.wait_for_received_task(after_revoke_task).await;
    assert!(
        daemon.sender_consent_prompts_for(&peer).await.is_empty(),
        "revoke cannot override sender consent implied by TOML"
    );
    assert!(
        daemon.journal.entries().await.iter().all(|entry| !matches!(
            &entry.record,
            rustain::domain::models::JournalRecord::Room(RoomEvent::ConsentGranted { .. })
        )),
        "the TOML-only positive control must not be mistaken for a standing journal grant"
    );
    host.stop().await;
    daemon.stop().await;
}
#[tokio::test]
async fn k06_saved_consent_and_attached_untrust_follow_the_production_admission_path() {
    let journal = Journal::open().await;
    let gate = Arc::new(AppendGateJournal::new(
        journal.room.clone(),
        is_consent_event,
        false,
    ));
    let daemon = AttachedDaemon::start_with_journal(journal, Vec::new(), Some(gate.clone())).await;
    let identity = InboundSenderIdentity::new("saved-consent");
    let peer = identity.identity.peer_id.clone();
    let host = daemon.serve_http(vec![identity]).await;
    let sender = host.sender("saved-consent");
    let mut writer =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;

    let first_text = "k06-saved-first-prompt-marker";
    let first = sender.message_send("saved-first", first_text).await;
    assert_eq!(
        first.task_state.as_deref(),
        Some("auth-required"),
        "{first:?}"
    );

    let first_prompts = daemon.sender_consent_prompts_for(&peer).await;
    assert_eq!(
        first_prompts.len(),
        1,
        "the first admission needs a consent card"
    );
    let (grant_request, _) = writer.wait_sender_consent_request().await;
    assert_eq!(first_prompts[0], grant_request);
    writer
        .answer_sender_consent_always_and_save(grant_request.clone())
        .await;
    gate.wait_durable_for(1).await;
    assert!(
        gate.durable_seen().iter().any(|event| matches!(
            event,
            RoomEvent::ConsentGranted {
                sender: Some(saved),
                principal: Some(PrincipalKind::Pinned),
                ..
            } if saved == &peer
        )),
        "production approval must durably grant the authenticated sender: {:?}",
        gate.durable_seen()
    );
    daemon.wait_for_pinned_sender_consent(&peer).await;

    // Refused writers must not change the next sender-consent decision.
    let mut read_only =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, true).await;
    let read_request = mint_request_key();
    read_only
        .revoke_sender_consent(read_request.clone(), peer.as_str())
        .await;
    assert_request_refusal(
        read_only.wait_revoked(&read_request).await,
        &read_request,
        RecipientAction::RevokeSenderConsent,
        |error| matches!(error, ProtocolError::ReadOnly),
    );
    let mut peer_client = AttachedClient::attach(&daemon.path, ConnectionTier::Peer, false).await;
    let peer_request = mint_request_key();
    peer_client
        .revoke_sender_consent(peer_request.clone(), peer.as_str())
        .await;
    assert_request_refusal(
        peer_client.wait_revoked(&peer_request).await,
        &peer_request,
        RecipientAction::RevokeSenderConsent,
        |error| matches!(error, ProtocolError::PeerVerification(_)),
    );
    let unchanged_text = "k06-saved-still-admitted-marker";
    let unchanged = sender
        .message_send("saved-still-admitted", unchanged_text)
        .await;
    assert_eq!(
        unchanged.task_state.as_deref(),
        Some("auth-required"),
        "{unchanged:?}"
    );
    daemon.wait_for_received_task("saved-still-admitted").await;
    assert!(
        daemon.sender_consent_prompts_for(&peer).await.is_empty(),
        "read-only and Peer revoke refusals must leave this sender admitted"
    );

    let revoke_request = mint_request_key();
    writer
        .revoke_sender_consent(revoke_request.clone(), peer.as_str())
        .await;
    gate.wait_durable_for(2).await;
    assert!(
        gate.durable_seen().iter().any(|event| matches!(
            event,
            RoomEvent::ConsentRevoked { sender: Some(saved), .. } if saved == &peer
        )),
        "ConsentRevoked must be durable before the attached answer"
    );
    let revoked = writer.wait_revoked(&revoke_request).await;
    let DaemonFrame::SenderConsentRevoked {
        request, message, ..
    } = revoked
    else {
        panic!("the trusted-local writer must revoke standing consent: {revoked:?}");
    };
    assert_eq!(request, revoke_request);
    assert_eq!(
        message,
        format!("Revoked trust for {peer} ({peer}). Future tasks require consent."),
        "attached untrust must preserve the standalone revocation wording"
    );

    let re_prompt_text = "k06-saved-after-revoke-prompt-marker";
    let prompted = sender
        .message_send("saved-after-revoke", re_prompt_text)
        .await;
    assert_eq!(
        prompted.task_state.as_deref(),
        Some("auth-required"),
        "{prompted:?}"
    );

    let prompts = daemon.sender_consent_prompts_for(&peer).await;
    assert_eq!(prompts.len(), 1, "revoked consent must prompt again");
    let (new_request, _) = writer.wait_sender_consent_request().await;
    assert_eq!(prompts[0], new_request);
    assert_ne!(
        new_request, grant_request,
        "revocation must clear the old pending group"
    );
    writer
        .answer_sender_consent_reject(new_request, "deterministic pending-group cleanup")
        .await;
    host.stop().await;
    daemon.stop().await;
}

#[tokio::test]
async fn k06_failed_consent_revoke_append_preserves_admission() {
    let journal = Journal::open().await;
    let gate = Arc::new(AppendGateJournal::new(
        journal.room.clone(),
        is_consent_event,
        false,
    ));
    gate.reject_matching_occurrence(1, "reject ConsentRevoked");
    let daemon = AttachedDaemon::start_with_journal(journal, Vec::new(), Some(gate.clone())).await;
    let identity = InboundSenderIdentity::new("append-failure");
    let peer = identity.identity.peer_id.clone();
    let host = daemon.serve_http(vec![identity]).await;
    let sender = host.sender("append-failure");
    let mut writer =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;

    let first_text = "k06-append-first-prompt-marker";
    let first = sender.message_send("append-first", first_text).await;
    assert_eq!(
        first.task_state.as_deref(),
        Some("auth-required"),
        "{first:?}"
    );

    let prompts = daemon.sender_consent_prompts_for(&peer).await;
    assert_eq!(prompts.len(), 1, "initial consent must be requested");
    let (grant_request, _) = writer.wait_sender_consent_request().await;
    assert_eq!(prompts[0], grant_request);
    writer
        .answer_sender_consent_always_and_save(grant_request)
        .await;
    gate.wait_durable_for(1).await;
    daemon.wait_for_pinned_sender_consent(&peer).await;
    let trusted_before_revoke = sender
        .message_send("append-trusted-before-revoke", "append precondition")
        .await;
    assert_eq!(
        trusted_before_revoke.task_state.as_deref(),
        Some("auth-required"),
        "{trusted_before_revoke:?}"
    );
    daemon
        .wait_for_received_task("append-trusted-before-revoke")
        .await;
    assert!(
        daemon.sender_consent_prompts_for(&peer).await.is_empty(),
        "a saved grant must admit before testing revocation failure"
    );

    let request = mint_request_key();
    writer
        .revoke_sender_consent(request.clone(), peer.as_str())
        .await;
    gate.wait_for(2).await;
    let refused = writer.wait_revoked(&request).await;
    assert_request_refusal(
        refused,
        &request,
        RecipientAction::RevokeSenderConsent,
        |error| matches!(error, ProtocolError::Internal(_)),
    );
    assert_eq!(
        gate.durable_seen()
            .iter()
            .filter(|event| matches!(event, RoomEvent::ConsentRevoked { .. }))
            .count(),
        0,
        "the failed ConsentRevoked append must leave sender consent admitted without a prompt"
    );
    assert_eq!(
        daemon
            .consent_projection
            .consent_for(&PolicyPrincipal::bound_a2a(peer.clone())),
        ConsentState::Trusted,
        "the failed ConsentRevoked append must leave sender consent admitted without a prompt"
    );
    let still_admitted_text = "k06-append-still-admitted-marker";
    let still_admitted = sender
        .message_send("append-still-admitted", still_admitted_text)
        .await;
    assert_eq!(
        still_admitted.task_state.as_deref(),
        Some("auth-required"),
        "the failed ConsentRevoked append must leave sender consent admitted without a prompt: {still_admitted:?}"
    );
    daemon.wait_for_received_task("append-still-admitted").await;
    assert!(
        daemon.sender_consent_prompts_for(&peer).await.is_empty(),
        "failed ConsentRevoked append must not prompt the still-trusted sender"
    );

    let append_enabled_request = mint_request_key();
    writer
        .revoke_sender_consent(append_enabled_request.clone(), peer.as_str())
        .await;
    gate.wait_durable_for(2).await;
    assert!(matches!(
        writer.wait_revoked(&append_enabled_request).await,
        DaemonFrame::SenderConsentRevoked { request, .. } if request == append_enabled_request
    ));
    let after_revoke_text = "k06-append-after-revoke-prompt-marker";
    let prompted = sender
        .message_send("append-after-revoke", after_revoke_text)
        .await;
    assert_eq!(
        prompted.task_state.as_deref(),
        Some("auth-required"),
        "{prompted:?}"
    );
    let new_prompts = daemon.sender_consent_prompts_for(&peer).await;
    assert_eq!(
        new_prompts.len(),
        1,
        "a durable revoke must restore prompting"
    );
    let (new_request, _) = writer.wait_sender_consent_request().await;
    assert_eq!(new_prompts[0], new_request);
    writer
        .answer_sender_consent_reject(new_request, "deterministic pending-group cleanup")
        .await;
    host.stop().await;
    daemon.stop().await;
}

#[tokio::test]
async fn k06_attached_status_uses_daemon_composed_roster_and_never_starts_a_model_turn() {
    let peer = crate::attached_harness::RecipientHost::start(
        rustain::adapters::a2a::admission::A2aAdmissionPolicy::Allow,
    )
    .await;
    let profile_peer = peer.peer("profile-only-peer");
    let peer_target = profile_peer.resolved_identity().as_str().to_owned();
    let daemon = AttachedDaemon::start(vec![profile_peer]).await;
    let mut client =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    assert_eq!(client.granted, AttachMode::ReadWrite);

    for key in ["k06-status", "k06-trust"] {
        client.team_status(key).await;
        let result = client.wait_status_result(key).await;
        let DaemonFrame::TeamStatusResult { request, message } = result else {
            panic!("status request {key} was not answered: {result:?}");
        };
        assert_eq!(request, key);
        assert!(
            message.contains("profile-only-peer"),
            "status must use the daemon's profile-composed roster: {message}"
        );
    }
    client
        .revoke_sender_consent("k06-untrust", peer_target)
        .await;
    let untrust = client.wait_revoked("k06-untrust").await;
    assert!(
        matches!(&untrust, DaemonFrame::SenderConsentRevoked { request, .. } if request == "k06-untrust"),
        "attached untrust must be a correlated daemon result, got {untrust:?}"
    );
    assert_eq!(
        daemon.provider_request_count(),
        0,
        "attached /team status, /team trust, and /team untrust are not provider prompts"
    );
    assert_eq!(daemon.model_turn_count(), 0);

    // Positive control on this exact socket and daemon runtime: ordinary input
    // crosses the model front door once, so the zero above is meaningful.
    client
        .send_frame(ClientFrame::UserMessage {
            text: "K06 positive provider control".into(),
            images: Vec::new(),
        })
        .await;
    tokio::time::timeout(FIXTURE_TIMEOUT, async {
        while daemon.provider_request_count() != 1 || daemon.model_turn_count() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the same attached daemon must observe the positive provider/turn control");
    assert_eq!(daemon.provider_request_count(), 1);
    assert_eq!(daemon.model_turn_count(), 1);
    daemon.stop().await;
    peer.stop().await;
}

#[tokio::test]
async fn k06_attached_read_only_and_peer_untrust_are_request_keyed_refusals() {
    let sender = rustain::domain::models::PeerId::from_public_key(&[7; 32])
        .expect("valid pinned sender peer id");
    let daemon = AttachedDaemon::start(Vec::new()).await;
    let mut peer = AttachedClient::attach(&daemon.path, ConnectionTier::Peer, false).await;
    assert_eq!(peer.granted, AttachMode::ReadOnly);
    let peer_request = mint_request_key();
    peer.revoke_sender_consent(peer_request.clone(), sender.as_str())
        .await;
    assert_request_refusal(
        peer.wait_revoked(&peer_request).await,
        &peer_request,
        RecipientAction::RevokeSenderConsent,
        |error| matches!(error, ProtocolError::PeerVerification(_)),
    );
    let mut read_only =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, true).await;
    assert_eq!(read_only.granted, AttachMode::ReadOnly);
    let read_only_request = mint_request_key();
    read_only
        .revoke_sender_consent(read_only_request.clone(), sender.as_str())
        .await;
    assert_request_refusal(
        read_only.wait_revoked(&read_only_request).await,
        &read_only_request,
        RecipientAction::RevokeSenderConsent,
        |error| matches!(error, ProtocolError::ReadOnly),
    );

    assert_eq!(
        daemon.journal.entries().await.len(),
        0,
        "read-only and Peer untrust refusals must not mutate the journal"
    );
    assert_eq!(daemon.provider_request_count(), 0);
    assert_eq!(daemon.model_turn_count(), 0);
    daemon.stop().await;
}

fn assert_request_refusal(
    frame: DaemonFrame,
    request: &str,
    action: RecipientAction,
    is_expected_error: impl FnOnce(&ProtocolError) -> bool,
) {
    let DaemonFrame::RecipientActionResult {
        action: actual_action,
        target,
        outcome: RecipientActionOutcome::Refused { error },
    } = frame
    else {
        panic!("request {request} must receive a correlated refusal, got {frame:?}");
    };
    assert_eq!(actual_action, action, "refusal action for {request}");
    assert_eq!(
        target,
        RecipientActionTarget::Request {
            request: request.into()
        }
    );
    assert!(
        is_expected_error(&error),
        "request {request} was refused for the wrong reason: {error:?}"
    );
}
