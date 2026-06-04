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

// ── Interleaving variant (R-002, test-design-epic-11.md) ─────────────────────
// The single-cycle test above proves the barrier for one flush/compaction. This
// proves it HOLDS UNDER CONCURRENCY: N cycles run at once, their flushes are
// released in REVERSE order to force interleaving, yet every cycle's flush is
// still observed before that cycle's own compaction. A per-cycle fire-and-forget
// regression (dropping the `.await` on flush) → red.
//
// (The complementary "replayed-history prefix byte-unchanged" concern is already
// covered by the frozen wire golden in `tests/passthrough_unchanged.rs`.)

type OrderLog = Arc<Mutex<Vec<String>>>;

/// Per-cycle barrier-flushing memory: records `flush-{id}` only after the test
/// releases its barrier.
struct OrderedFlush {
    id: usize,
    barrier: Arc<Notify>,
    reached: Arc<Notify>,
    log: OrderLog,
}

#[async_trait]
impl MemoryPort for OrderedFlush {
    async fn flush(&self) -> Result<(), MemoryError> {
        self.reached.notify_one();
        self.barrier.notified().await;
        self.log.lock().await.push(format!("flush-{}", self.id));
        Ok(())
    }
}

/// Per-cycle provider: records `compaction-{id}` the moment compaction starts.
struct OrderedProvider {
    id: usize,
    log: OrderLog,
}

#[async_trait]
impl StreamingProvider for OrderedProvider {
    async fn stream_completion(
        &self,
        _messages: Vec<Message>,
        _options: CompletionOptions,
    ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
        self.log
            .lock()
            .await
            .push(format!("compaction-{}", self.id));
        Ok(Box::pin(stream::iter(Vec::<StreamChunk>::new())))
    }

    async fn abort(&self) -> Result<(), ProviderError> {
        Ok(())
    }

    fn provider_id(&self) -> String {
        format!("ordered-{}", self.id)
    }

    fn list_models(&self) -> Vec<ModelDescriptor> {
        vec![]
    }

    async fn health_check(&self) -> Result<(), ProviderError> {
        Ok(())
    }
}

#[tokio::test]
async fn interleaved_cycles_keep_flush_before_compaction_per_cycle() {
    const CYCLES: usize = 3;
    let log: OrderLog = Arc::new(Mutex::new(Vec::new()));
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();

    let mut barriers = Vec::new();
    let mut reached_signals = Vec::new();
    let mut handles = Vec::new();

    for id in 0..CYCLES {
        let barrier = Arc::new(Notify::new());
        let reached = Arc::new(Notify::new());
        let memory: Arc<dyn MemoryPort> = Arc::new(OrderedFlush {
            id,
            barrier: barrier.clone(),
            reached: reached.clone(),
            log: log.clone(),
        });
        let payload = CompactionPayload {
            provider: Arc::new(OrderedProvider {
                id,
                log: log.clone(),
            }),
            model: format!("model-{id}"),
            history_text: format!("history {id}"),
            conversation_id: format!("conv-{id}"),
            first_kept_message_id: None,
            pre_tokens: 100,
            purpose: CompactionPurpose::Inline,
            domain_tx: tx.clone(),
        };
        handles.push(tokio::spawn(flush_then_compact(memory, payload)));
        barriers.push(barrier);
        reached_signals.push(reached);
    }

    // Precondition (enforced, not assumed): this loop blocks until EVERY cycle's
    // flush has signalled `reached`, i.e. all flushes are parked at their barriers
    // and none has been released. Only then is the emptiness assertion meaningful —
    // an empty log here proves no compaction ran ahead of its flush, rather than
    // merely racing the first flush.
    for reached in &reached_signals {
        reached.notified().await;
    }
    assert!(
        log.lock().await.is_empty(),
        "nothing recorded while all flushes are parked at their barriers (no early compaction)"
    );

    // Release in REVERSE order to force cross-cycle interleaving.
    for barrier in barriers.iter().rev() {
        barrier.notify_one();
    }

    for handle in handles {
        handle.await.expect("flush_then_compact should not panic");
    }

    // Per cycle, the flush MUST appear before that cycle's compaction.
    let seq = log.lock().await.clone();
    for id in 0..CYCLES {
        let f = seq.iter().position(|s| s == &format!("flush-{id}"));
        let c = seq.iter().position(|s| s == &format!("compaction-{id}"));
        match (f, c) {
            (Some(f), Some(c)) => assert!(
                f < c,
                "cycle {id}: flush (idx {f}) must precede compaction (idx {c}); seq={seq:?}"
            ),
            other => {
                panic!("cycle {id}: missing flush/compaction marker: {other:?}; seq={seq:?}")
            }
        }
    }
}
