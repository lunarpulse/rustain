//! Commit scheduler completions to the daemon conversation.
use super::*;

impl AttachServer {
    #[cfg(feature = "cron")]
    pub(super) async fn commit_cron_completion(&self, completion: CronCompletion) {
        let text = format!("[cron: {}] {}", completion.job_name, completion.result_text);
        let mut conv = self.conversation.lock().await;
        conv.messages.push(ChatMessage {
            id: generate_message_id(),
            role: MessageRole::Assistant,
            content: text,
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: crate::domain::models::session_meta::now_unix(),
            token_count: None,
            stop_reason: Some(StopReason::EndTurn),
            synthetic: false,
            images: vec![],
            origin: ChannelKind::Cron,
            authorship: crate::domain::models::Authorship::HumanWritten,
            send_authorization: crate::domain::models::SendAuthorization::Unknown,
            retracted_at_ms: None,
            history_source: Default::default(),
        });
        let now = crate::domain::models::session_meta::now_unix();
        conv.updated_at = now;
        conv.last_response_at = Some(now);
        if let Err(e) = self.core.storage.save_conversation(&conv).await {
            tracing::warn!(error = %e, "daemon: persisting cron completion failed");
        }
    }

    #[cfg(not(feature = "cron"))]
    pub(super) async fn commit_cron_completion(&self, _completion: CronCompletion) {}
}
