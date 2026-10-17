use std::sync::Arc;
use std::time::Duration;

use futures::stream::BoxStream;
use rustain::domain::errors::ProviderError;
use rustain::domain::models::{CompletionOptions, ModelDescriptor, StopReason, StreamChunk};
use rustain::domain::ports::{ProbeOutcome, StreamingProvider};

struct ProposalProvider {
    tool_name: &'static str,
    input: serde_json::Value,
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl StreamingProvider for ProposalProvider {
    async fn stream_completion(
        &self,
        _messages: Vec<rustain::domain::models::Message>,
        _options: CompletionOptions,
    ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
        let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let chunks = if call == 0 {
            vec![
                StreamChunk::ToolUse {
                    id: format!("proposal-{call}"),
                    name: self.tool_name.into(),
                    input: self.input.clone(),
                },
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::ToolUse,
                },
            ]
        } else {
            vec![StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            }]
        };
        Ok(Box::pin(futures::stream::iter(chunks)))
    }
    async fn abort(&self) -> Result<(), ProviderError> {
        Ok(())
    }
    fn provider_id(&self) -> String {
        "proposal-conformance".into()
    }
    fn list_models(&self) -> Vec<ModelDescriptor> {
        vec![]
    }
    async fn health_check(&self) -> Result<(), ProviderError> {
        Ok(())
    }
    async fn connectivity_probe(&self) -> Result<ProbeOutcome, ProviderError> {
        Ok(ProbeOutcome {
            latency: Duration::ZERO,
        })
    }
}

pub(crate) fn tool_provider(
    tool_name: &'static str,
    input: serde_json::Value,
) -> Arc<dyn StreamingProvider> {
    Arc::new(ProposalProvider {
        tool_name,
        input,
        calls: std::sync::atomic::AtomicUsize::new(0),
    })
}
