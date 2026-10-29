//! Local TUI producer ownership and source-captured stream admission.
//!
//! The registry is deliberately runtime-local: neither daemon/ACP protocols nor
//! persisted turns carry a generation. Retire admission before cancelling tasks.

use std::collections::HashMap;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::domain::clock::Clock;
use crate::domain::events::AppEvent;
use crate::domain::models::{Conversation, StopReason, StreamChunk, StreamingState};
use crate::domain::services::reducer::ReducerState;

struct Producer {
    identity: u64,
    cancel: CancellationToken,
    model: String,
    task: Option<JoinHandle<()>>,
    task_abort: Option<tokio::task::AbortHandle>,
    forwarder: Option<JoinHandle<()>>,
}

#[derive(Default)]
pub struct TuiTurnLifecycle {
    next_identity: u64,
    producers: HashMap<String, Producer>,
}

impl TuiTurnLifecycle {
    pub fn is_live(&self, conversation_id: &str) -> bool {
        self.producers.contains_key(conversation_id)
    }

    /// Register before spawning `run_turn`. Every chunk is attributed at its
    /// originating sender, never by looking up a turn when a queued event lands.
    pub fn register(
        &mut self,
        conversation_id: String,
        cancel: CancellationToken,
        model: String,
        domain_tx: mpsc::UnboundedSender<AppEvent>,
    ) -> (u64, mpsc::UnboundedSender<AppEvent>) {
        assert!(
            !self.is_live(&conversation_id),
            "previous conversation producer must be stopped before replacement"
        );
        self.next_identity = self
            .next_identity
            .checked_add(1)
            .expect("producer identity exhausted");
        let identity = self.next_identity;
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let owner = conversation_id.clone();
        let forwarding_cancel = cancel.clone();
        let forwarder = tokio::spawn(async move {
            let mut closing = false;
            let mut terminal_seen = false;
            let mut buffered_success = None;
            loop {
                let event = tokio::select! {
                    biased;
                    _ = forwarding_cancel.cancelled(), if !closing => {
                        // Stop new ingress but drain events already emitted, in
                        // order. Retired chunks are fenced; unrelated events are
                        // not lost by aborting this forwarding task.
                        receiver.close();
                        closing = true;
                        continue;
                    }
                    event = receiver.recv() => event,
                };
                let Some(event) = event else {
                    // Natural producer EOF is the accounting boundary. Some
                    // adapters emit Usage after a normal terminal; completion
                    // effects must observe that usage before saving/retitling.
                    if !closing
                        && !forwarding_cancel.is_cancelled()
                        && let Some(stop_reason) = buffered_success
                    {
                        let _ = domain_tx.send(AppEvent::LocalProviderChunk {
                            conversation_id: owner.clone(),
                            producer_id: identity,
                            chunk: StreamChunk::TurnComplete { stop_reason },
                        });
                    }
                    break;
                };
                let event = match event {
                    AppEvent::ProviderChunk {
                        conversation_id,
                        chunk,
                    } => {
                        // A submission cannot admit chunks for another conversation.
                        if conversation_id != owner {
                            continue;
                        }
                        match &chunk {
                            StreamChunk::TurnComplete {
                                stop_reason: StopReason::EndTurn | StopReason::MaxTokens,
                            } => {
                                if !terminal_seen {
                                    terminal_seen = true;
                                    if let StreamChunk::TurnComplete { stop_reason } = chunk {
                                        buffered_success = Some(stop_reason);
                                    }
                                }
                                continue;
                            }
                            StreamChunk::TurnComplete {
                                stop_reason: StopReason::Cancelled,
                            } => {
                                // A cancelled producer never publishes a prior
                                // buffered success, even if its sender closes.
                                if terminal_seen && buffered_success.is_none() {
                                    continue;
                                }
                                terminal_seen = true;
                                buffered_success = None;
                            }
                            StreamChunk::Usage { .. } => {}
                            _ if terminal_seen => continue,
                            _ => {}
                        }
                        AppEvent::LocalProviderChunk {
                            conversation_id,
                            producer_id: identity,
                            chunk,
                        }
                    }
                    other => other,
                };
                if domain_tx.send(event).is_err() {
                    break;
                }
            }
        });
        self.producers.insert(
            conversation_id,
            Producer {
                identity,
                cancel,
                model,
                task: None,
                task_abort: None,
                forwarder: Some(forwarder),
            },
        );
        (identity, sender)
    }

    pub fn install_task(&mut self, conversation_id: &str, identity: u64, task: JoinHandle<()>) {
        let producer = self
            .producers
            .get_mut(conversation_id)
            .expect("registered producer");
        assert_eq!(producer.identity, identity);
        assert!(producer.task.is_none(), "producer task already installed");
        producer.task_abort = Some(task.abort_handle());
        producer.task = Some(task);
    }
    /// Join the producer and ordered forwarding tail while retaining admission
    /// and abort ownership. Used by embedders that await a submitted turn.
    pub fn take_task(&mut self, conversation_id: &str) -> Option<JoinHandle<()>> {
        let producer = self.producers.get_mut(conversation_id)?;
        let task = producer.task.take()?;
        let forwarder = producer.forwarder.take()?;
        Some(tokio::spawn(async move {
            task.await.expect("local producer task failed");
            forwarder.await.expect("local producer forwarding failed");
        }))
    }

    pub fn retire(&mut self, conversation_id: &str) {
        if let Some(producer) = self.producers.remove(conversation_id) {
            producer.cancel.cancel();
            if let Some(task) = producer.task_abort {
                task.abort();
            }
        }
    }

    /// The sole ingress fence, before reduction, mirror/usage/model updates or
    /// completion effects. A stale normal terminal cannot close a replacement.
    /// Unrelated bus events are forwarded intact, without draining the bus.
    pub fn admit(&mut self, event: AppEvent) -> Option<(AppEvent, Option<String>)> {
        match event {
            AppEvent::LocalProviderChunk {
                conversation_id,
                producer_id,
                chunk,
            } => {
                if !self
                    .producers
                    .get(&conversation_id)
                    .is_some_and(|producer| producer.identity == producer_id)
                {
                    return None;
                }
                let terminal = matches!(
                    &chunk,
                    StreamChunk::TurnComplete {
                        stop_reason: StopReason::EndTurn | StopReason::MaxTokens
                    }
                );
                let model = if terminal {
                    // Detach finished tasks; do not abort their already-admitted
                    // completion/accounting tail. Old chunks remain fenced.
                    self.producers
                        .remove(&conversation_id)
                        .map(|producer| producer.model)
                } else {
                    None
                };
                Some((
                    AppEvent::ProviderChunk {
                        conversation_id,
                        chunk,
                    },
                    model,
                ))
            }
            other => Some((other, None)),
        }
    }

    /// Shared interruption front door for cancel, quit, NewTab and provider
    /// cancellation. Task retirement precedes the synchronous reducer close.
    pub fn interrupt(
        &mut self,
        streaming: &mut StreamingState,
        conversation: &mut Conversation,
        reducer: &mut ReducerState,
        clock: &dyn Clock,
        pending_model: &mut Option<String>,
    ) {
        self.retire(&conversation.id);
        crate::adapters::tui::handlers::turn_finalize::finalize_streaming_turn(
            streaming,
            conversation,
            reducer,
            clock,
        );
        pending_model.take();
    }

    pub fn interrupt_active(
        &mut self,
        tabs: &mut crate::domain::models::tab::TabManager,
        conversation: &mut Conversation,
        streaming: &mut StreamingState,
        pending_model: &mut Option<String>,
    ) {
        let tab = tabs.active_tab_mut();
        self.interrupt(
            streaming,
            conversation,
            &mut tab.reducer,
            &*tab.clock,
            pending_model,
        );
    }

    /// All owned tabs close before any shutdown save, including background work.
    pub fn interrupt_all(
        &mut self,
        tabs: &mut crate::domain::models::tab::TabManager,
        conversation: &mut Conversation,
        streaming: &mut StreamingState,
        pending_model: &mut Option<String>,
    ) {
        self.interrupt_active(tabs, conversation, streaming, pending_model);
        for tab in tabs.tabs_mut() {
            if tab.conversation.id != conversation.id {
                self.interrupt(
                    &mut tab.streaming,
                    &mut tab.conversation,
                    &mut tab.reducer,
                    &*tab.clock,
                    &mut None,
                );
            }
        }
    }
}

impl Drop for TuiTurnLifecycle {
    fn drop(&mut self) {
        for (_, producer) in self.producers.drain() {
            producer.cancel.cancel();
            if let Some(task) = producer.task_abort {
                task.abort();
            }
        }
    }
}
