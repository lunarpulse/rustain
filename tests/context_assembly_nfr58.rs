//! Story 11.4 NFR58 — memory flush MUST complete before context compaction
//! begins (prd.md:2312). This is a barrier-ordering CONTRACT test (Panel-Review
//! Amendment 5, Murat): the real failure mode is a missing `await` at the seam
//! (fire-and-forget), not a missing flush. We drive `flush_then_compact` with a
//! `MemoryPort` stub whose `flush()` blocks on a test-controlled barrier, and a
//! provider that records when compaction starts; then assert compaction could
//! not observe a pre-flush state. Fire-and-forget → red; await → green.
//! Deterministic, no real I/O race, no sleeps.

use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::{self, BoxStream};
use tokio::sync::{Mutex, Notify};

use rustain::adapters::tui::handlers::CompactionPayload;
use rustain::adapters::tui::handlers::compaction::flush_then_compact;
use rustain::domain::errors::{MemoryError, ProviderError};
use rustain::domain::events::{AppEvent, CompactionPurpose};
use rustain::domain::models::{CompletionOptions, Message, ModelDescriptor, StreamChunk};
use rustain::domain::ports::{MemoryPort, StreamingProvider};

type EventLog = Arc<Mutex<Vec<&'static str>>>;

/// `MemoryPort` whose `flush()` blocks on a test-controlled barrier.
/// `reached` signals the test that flush has hit the barrier;
/// `barrier` is unblocked by the test when it's ready.
struct BarrierFlushMemory {
    barrier: Arc<Notify>,
    reached: Arc<Notify>,
    log: EventLog,
}

#[async_trait]
impl MemoryPort for BarrierFlushMemory {
    async fn flush(&self) -> Result<(), MemoryError> {
        // Signal that we've reached the barrier point.
        self.reached.notify_one();
        // Block until the test unblocks us.
        self.barrier.notified().await;
        self.log.lock().await.push("flush");
        Ok(())
    }
}

/// Provider that records `"compaction"` the moment `stream_completion` (the
/// compaction work) is invoked, then errors out (run_compaction degrades).
struct RecordingProvider {
    log: EventLog,
}

#[async_trait]
impl StreamingProvider for RecordingProvider {
    async fn stream_completion(
        &self,
        _messages: Vec<Message>,
        _options: CompletionOptions,
    ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
        self.log.lock().await.push("compaction");
        // Empty stream is enough — run_compaction only needs to have started.
        Ok(Box::pin(stream::iter(Vec::<StreamChunk>::new())))
    }

    async fn abort(&self) -> Result<(), ProviderError> {
        Ok(())
    }

    fn provider_id(&self) -> String {
        "recording".to_string()
    }

    fn list_models(&self) -> Vec<ModelDescriptor> {
        vec![]
    }

    async fn health_check(&self) -> Result<(), ProviderError> {
        Ok(())
    }
}

#[tokio::test]
async fn memory_flush_completes_before_compaction_begins() {
    let log: EventLog = Arc::new(Mutex::new(Vec::new()));
    let barrier = Arc::new(Notify::new());
    let reached = Arc::new(Notify::new());
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();

    let memory: Arc<dyn MemoryPort> = Arc::new(BarrierFlushMemory {
        barrier: barrier.clone(),
        reached: reached.clone(),
        log: log.clone(),
    });
    let payload = CompactionPayload {
        provider: Arc::new(RecordingProvider { log: log.clone() }),
        model: "test-model".to_string(),
        history_text: "some conversation history to compact".to_string(),
        conversation_id: "conv-1".to_string(),
        first_kept_message_id: None,
        pre_tokens: 100,
        purpose: CompactionPurpose::Inline,
        domain_tx: tx,
    };

    // Spawn flush_then_compact in the background so we can inspect state
    // while it's blocked at the flush barrier.
    let handle = tokio::spawn(flush_then_compact(memory, payload));

    // Wait until flush() has reached its barrier.
    reached.notified().await;

    // CRITICAL ASSERTION: compaction MUST NOT have started yet. If
    // flush_then_compact were fire-and-forget (did not await flush), the
    // provider's stream_completion would have recorded "compaction" by now.
    let seq_so_far = log.lock().await.clone();
    assert!(
        !seq_so_far.contains(&"compaction"),
        "NFR58 contract violation: compaction observed before flush completed (fire-and-forget?)"
    );

    // Unblock flush — now it completes, and ONLY THEN does compaction begin.
    barrier.notify_one();

    // Wait for the full operation to finish.
    handle.await.expect("flush_then_compact should not panic");

    let seq = log.lock().await.clone();
    assert_eq!(
        seq,
        vec!["flush", "compaction"],
        "NFR58 contract: flush MUST be observed before compaction begins (await, not fire-and-forget)"
    );
}
