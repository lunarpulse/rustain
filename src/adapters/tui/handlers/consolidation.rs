//! Background memory-consolidation sub-turn — Story 11.2a (completes 11.2 AC4).
//!
//! Mirrors `handlers/compaction.rs::run_compaction` (the established background
//! model-call precedent): the event loop builds a [`ConsolidationPayload`] and
//! `tokio::spawn(run_consolidation(payload))`s it at the dispatch site (the
//! spawn-stays invariant — ADR-08-01 §D2). The task runs ONE structured model
//! sub-turn with `tools: vec![]` so the model must emit JSON text rather than
//! call the risk-Safe `remember_fact` tool directly (which would auto-approve,
//! bypassing the user confirmation AC4 requires — the exact reason 11.2 split
//! AC4). On success it emits `MemoryConsolidationProposed`; on empty / error /
//! timeout it emits `MemoryConsolidationFailed` (AC6 — never panics, never
//! blocks the event loop).
//!
//! `generate_proposals` and `CONSOLIDATION_TIMEOUT` have been extracted to
//! `domain::services::consolidation` (Story 12.2d AC8) so the daemon attach path
//! can reuse them without adapter→adapter coupling.

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::domain::events::AppEvent;
use crate::domain::models::tab::ConversationId;
use crate::domain::ports::StreamingProvider;

/// Re-export the shared timeout from the domain service (Story 12.2d AC8).
pub use crate::domain::services::consolidation::CONSOLIDATION_TIMEOUT;

/// Data payload for the consolidation spawn (mirrors `CompactionPayload`).
/// Carries DATA only — the `tokio::spawn(...)` future is constructed at the
/// dispatch site so the event-loop spawn topology stays grep-able.
pub struct ConsolidationPayload {
    pub provider: Arc<dyn StreamingProvider>,
    pub model: String,
    pub prompt_body: String,
    pub conversation_id: ConversationId,
    pub domain_tx: mpsc::UnboundedSender<AppEvent>,
}

/// Spawn body — runs the structured sub-turn and emits the terminal event via
/// the carried `domain_tx` (title/compaction precedent). Called from the
/// dispatch arm via `tokio::spawn(run_consolidation(payload))`.
pub async fn run_consolidation(payload: ConsolidationPayload) {
    let ConsolidationPayload {
        provider,
        model,
        prompt_body,
        conversation_id,
        domain_tx,
    } = payload;

    let result = tokio::time::timeout(
        CONSOLIDATION_TIMEOUT,
        crate::domain::services::consolidation::generate_proposals(
            &*provider,
            &model,
            &prompt_body,
        ),
    )
    .await;

    // Pre-build the result event, then emit on a single line — this mirrors
    // `run_compaction`/`generate_title` exactly (the sanctioned spawned-task
    // `domain_tx` send) so the eventbus-bypass conformance tag lands on the
    // `.send(...)` line itself, not on a trailing `});`.
    let event = match result {
        Ok(Ok(proposals)) => {
            if proposals.is_empty() {
                AppEvent::MemoryConsolidationFailed {
                    conversation_id,
                    reason: "nothing worth promoting from recent activity".to_string(),
                }
            } else {
                AppEvent::MemoryConsolidationProposed {
                    conversation_id,
                    proposals,
                }
            }
        }
        Ok(Err(e)) => AppEvent::MemoryConsolidationFailed {
            conversation_id,
            reason: format!("{e}"),
        },
        Err(_) => AppEvent::MemoryConsolidationFailed {
            conversation_id,
            reason: "consolidation timed out".to_string(),
        },
    };
    let _ = domain_tx.send(event); // CONFORMANCE_EXCEPTION_EVENTBUS_BYPASS: 11-2a — background sub-turn result (mirrors TitleGenerated/CompactionComplete)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::errors::ProviderError;
    use crate::domain::models::{
        CompletionOptions, Message, ModelDescriptor, StopReason, StreamChunk,
        generate_conversation_id,
    };
    use async_trait::async_trait;
    use futures::stream::{self, BoxStream};

    /// Stub provider returning a fixed reply as streaming text (mirrors
    /// `tests/title_generation.rs::MockTitleProvider`).
    struct CannedProvider {
        text: String,
    }

    #[async_trait]
    impl StreamingProvider for CannedProvider {
        async fn stream_completion(
            &self,
            _messages: Vec<Message>,
            _options: CompletionOptions,
        ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
            let chunks = vec![
                StreamChunk::Text {
                    content: self.text.clone(),
                    parent_tool_use_id: None,
                },
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::EndTurn,
                },
            ];
            Ok(Box::pin(stream::iter(chunks)))
        }
        async fn abort(&self) -> Result<(), ProviderError> {
            Ok(())
        }
        fn provider_id(&self) -> String {
            "canned".to_string()
        }
        fn list_models(&self) -> Vec<ModelDescriptor> {
            vec![]
        }
        async fn health_check(&self) -> Result<(), ProviderError> {
            Ok(())
        }
        async fn connectivity_probe(
            &self,
        ) -> Result<crate::domain::ports::ProbeOutcome, crate::domain::errors::ProviderError>
        {
            Ok(crate::domain::ports::ProbeOutcome {
                latency: std::time::Duration::ZERO,
            })
        }
    }

    struct FailingProvider;

    #[async_trait]
    impl StreamingProvider for FailingProvider {
        async fn stream_completion(
            &self,
            _messages: Vec<Message>,
            _options: CompletionOptions,
        ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
            Err(ProviderError::Other("network down".into()))
        }
        async fn abort(&self) -> Result<(), ProviderError> {
            Ok(())
        }
        fn provider_id(&self) -> String {
            "failing".to_string()
        }
        fn list_models(&self) -> Vec<ModelDescriptor> {
            vec![]
        }
        async fn health_check(&self) -> Result<(), ProviderError> {
            Ok(())
        }
        async fn connectivity_probe(
            &self,
        ) -> Result<crate::domain::ports::ProbeOutcome, crate::domain::errors::ProviderError>
        {
            Ok(crate::domain::ports::ProbeOutcome {
                latency: std::time::Duration::ZERO,
            })
        }
    }

    fn payload(
        provider: Arc<dyn StreamingProvider>,
        tx: mpsc::UnboundedSender<AppEvent>,
    ) -> ConsolidationPayload {
        ConsolidationPayload {
            provider,
            model: "test-model".to_string(),
            prompt_body: "recent activity".to_string(),
            conversation_id: generate_conversation_id(),
            domain_tx: tx,
        }
    }

    #[tokio::test]
    async fn emits_proposed_on_valid_json() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let provider = Arc::new(CannedProvider {
            text: r#"[{"category":"Prefs","fact":"snake_case"}]"#.to_string(),
        });
        run_consolidation(payload(provider, tx)).await;
        match rx.try_recv().expect("an event was emitted") {
            AppEvent::MemoryConsolidationProposed { proposals, .. } => {
                assert_eq!(proposals.len(), 1);
                assert_eq!(proposals[0].category, "Prefs");
                assert_eq!(proposals[0].fact, "snake_case");
            }
            other => panic!("expected Proposed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn filters_secret_bearing_proposal() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        // Build the AWS key at runtime so the literal never appears in source.
        let akia = format!("AKIA{}", "A".repeat(16));
        let json = format!(
            r#"[{{"category":"Prefs","fact":"snake_case"}},{{"category":"Creds","fact":"key is {akia}"}}]"#
        );
        let provider = Arc::new(CannedProvider { text: json });
        run_consolidation(payload(provider, tx)).await;
        match rx.try_recv().expect("an event was emitted") {
            AppEvent::MemoryConsolidationProposed { proposals, .. } => {
                assert_eq!(
                    proposals.len(),
                    1,
                    "secret-bearing proposal must be dropped"
                );
                assert_eq!(proposals[0].category, "Prefs");
            }
            other => panic!("expected Proposed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn emits_failed_on_provider_error() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        run_consolidation(payload(Arc::new(FailingProvider), tx)).await;
        assert!(matches!(
            rx.try_recv().expect("an event was emitted"),
            AppEvent::MemoryConsolidationFailed { .. }
        ));
    }

    #[tokio::test]
    async fn emits_failed_on_empty_proposals() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let provider = Arc::new(CannedProvider {
            text: "[]".to_string(),
        });
        run_consolidation(payload(provider, tx)).await;
        assert!(matches!(
            rx.try_recv().expect("an event was emitted"),
            AppEvent::MemoryConsolidationFailed { .. }
        ));
    }
}
