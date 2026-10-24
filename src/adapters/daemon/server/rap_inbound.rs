//! RAP is a one-way message rail: admission is shared, replies are not.
use super::*;
use std::sync::Weak;

pub(super) struct DaemonPeerConsumer {
    server: Weak<AttachServer>,
}

impl DaemonPeerConsumer {
    pub(super) fn new(server: Weak<AttachServer>) -> Self {
        Self { server }
    }
}

#[async_trait::async_trait]
impl crate::adapters::rap::VerifiedPeerConsumer for DaemonPeerConsumer {
    async fn consent(
        &self,
        _recipient: &AgentId,
        content: &AgentMessage,
        peer_id: &PeerId,
    ) -> Result<crate::adapters::rap::VerifiedPeerConsent, String> {
        let server = self
            .server
            .upgrade()
            .ok_or_else(|| "daemon peer consumer is shutting down".to_string())?;
        if !crate::domain::ports::InboundPeerRuntime::enforces_sender_consent(server.as_ref()) {
            return Ok(crate::adapters::rap::VerifiedPeerConsent::Accept);
        }
        let ticket = crate::domain::ports::InboundPeerRuntime::request_admission_approval(
            server.as_ref(),
            &PolicyPrincipal::rap(peer_id.clone()),
            &content.content,
        )
        .await
        .map_err(|error| error.to_string())?;
        match ticket.decision.await {
            Ok(crate::domain::ports::InboundApprovalDecision::AllowOnce)
            | Ok(crate::domain::ports::InboundApprovalDecision::AllowAlways) => {
                Ok(crate::adapters::rap::VerifiedPeerConsent::Accept)
            }
            Ok(crate::domain::ports::InboundApprovalDecision::Decline) | Err(_) => {
                Ok(crate::adapters::rap::VerifiedPeerConsent::Decline)
            }
            Ok(crate::domain::ports::InboundApprovalDecision::RuntimeShutdown) => {
                Err("daemon peer consumer is shutting down".to_owned())
            }
        }
    }

    async fn ingest(
        &self,
        recipient: &AgentId,
        content: AgentMessage,
        peer_id: &PeerId,
    ) -> Result<(), String> {
        self.ingest_with_policy(
            recipient,
            content,
            peer_id,
            crate::domain::ports::PeerResponsePolicy::default(),
        )
        .await
    }

    async fn ingest_with_policy(
        &self,
        recipient: &AgentId,
        content: AgentMessage,
        peer_id: &PeerId,
        response_policy: crate::domain::ports::PeerResponsePolicy,
    ) -> Result<(), String> {
        let server = self
            .server
            .upgrade()
            .ok_or_else(|| "daemon peer consumer is shutting down".to_string())?;
        server
            .notify_rap_message(
                recipient.clone(),
                content.content,
                peer_id.clone(),
                response_policy,
            )
            .await
    }
}

impl AttachServer {
    async fn notify_rap_message(
        &self,
        recipient: AgentId,
        text: String,
        peer_id: PeerId,
        response_policy: crate::domain::ports::PeerResponsePolicy,
    ) -> Result<(), String> {
        commit_peer_rows(
            &self.peer_row_serial,
            &self.conversation,
            &self.registry,
            self.core.storage.as_ref(),
            vec![
                PeerRowMutation::Upsert {
                    message: peer_transcript_message(&text, &peer_id, &response_policy.provenance),
                    control: None,
                },
                PeerRowMutation::Upsert {
                    message: ChatMessage {
                        id: generate_message_id(),
                        role: MessageRole::Assistant,
                        content: crate::adapters::daemon::response_modes::rap_one_way_notice(
                            response_policy.mode,
                        ),
                        created_at: crate::domain::models::session_meta::now_unix(),
                        synthetic: true,
                        ..Default::default()
                    },
                    control: Some(peer_row_control(
                        &recipient,
                        None,
                        &response_policy.provenance,
                        peer_id.as_str(),
                        PeerRowStage::Settled,
                        false,
                    )),
                },
            ],
        )
        .await
    }
}
