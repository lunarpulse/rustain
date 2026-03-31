//! Simple pass-through turn orchestrator.
//! Sprint 1 (Story 1.5) wraps this in a tool loop with permission chain.

use std::sync::Arc;

use futures::StreamExt;
use tokio::sync::mpsc;

use crate::domain::events::AppEvent;
use crate::domain::models::{CompletionOptions, Message, NoticeLevel, StopReason, StreamChunk};
use crate::domain::ports::ProviderPort;

/// Execute a single turn: stream completion from the provider and forward
/// chunks as `AppEvent::ProviderChunk` through the event channel.
///
/// This is the Sprint 0 simple pass-through — no tool loop, no retry.
pub async fn run_turn(
    provider: Arc<dyn ProviderPort>,
    messages: Vec<Message>,
    options: CompletionOptions,
    event_tx: mpsc::UnboundedSender<AppEvent>,
) {
    match provider.stream_completion(messages, options).await {
        Ok(stream) => {
            futures::pin_mut!(stream);
            let mut received_turn_complete = false;
            while let Some(chunk) = stream.next().await {
                if matches!(chunk, StreamChunk::TurnComplete { .. }) {
                    received_turn_complete = true;
                }
                let _ = event_tx.send(AppEvent::ProviderChunk(chunk));
            }
            // Safety: if stream ended without TurnComplete, synthesize one so the
            // event loop doesn't stay stuck in is_streaming=true forever.
            if !received_turn_complete {
                tracing::warn!("Provider stream ended without TurnComplete — synthesizing end");
                let _ = event_tx.send(AppEvent::ProviderChunk(StreamChunk::Error {
                    content: "Stream disconnected unexpectedly".to_string(),
                }));
                let _ = event_tx.send(AppEvent::ProviderChunk(StreamChunk::TurnComplete {
                    stop_reason: StopReason::Cancelled,
                }));
            }
        }
        Err(e) => {
            let _ = event_tx.send(AppEvent::SystemNotice(NoticeLevel::Error, format!("{e}")));
        }
    }
}
